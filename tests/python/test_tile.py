"""The Python module: the same features from a GeoParquet file, DuckDB,
GeoPandas, Polars and PyArrow make the same tileset.

Run with the wheel installed: python tests/python/test_tile.py
"""

import gzip
import json
import os
import struct
import sys
import tempfile
import urllib.request

import duckdb
import geopandas
import polars
import pyarrow as pa
import pyarrow.parquet as pq
import shapely

import manby


def tiles(path):
    """A PMTiles archive's tile data and layers."""
    with open(path, "rb") as f:
        h = f.read(127)
        md_o, md_l, _, _, td_o, td_l = struct.unpack("<6Q", h[24:72])
        f.seek(md_o)
        layers = json.loads(gzip.decompress(f.read(md_l)))["vector_layers"]
        f.seek(td_o)
        return f.read(td_l), [l["id"] for l in layers]


def main():
    d = tempfile.mkdtemp(prefix="manby-test-")
    n = 20000
    points = shapely.points(
        [(-10 + 40 * (i * 7919 % n) / n, 35 + 25 * (i * 104729 % n) / n) for i in range(n)]
    )
    gdf = geopandas.GeoDataFrame(
        {
            "n": range(n),
            "kind": ["k%d" % (i % 13) for i in range(n)],
            "score": [i / 7 for i in range(n)],
        },
        geometry=points,
        crs="EPSG:4326",
    )
    path = os.path.join(d, "points.parquet")
    gdf.to_parquet(path)
    wkb = pa.table(
        {
            "n": pa.array(range(n), pa.int64()),
            "kind": gdf["kind"].tolist(),
            "score": gdf["score"].tolist(),
            "geometry": pa.array(shapely.to_wkb(points), pa.binary()),
        }
    )
    con = duckdb.connect()
    forms = {
        "file": path,
        "geopandas": gdf,
        "pyarrow": wkb,
        "duckdb": con.sql("SELECT * FROM wkb"),
        "polars": polars.from_arrow(wkb),
        "projected": gdf.to_crs(3035),
    }

    reference = None
    for name, data in forms.items():
        out = os.path.join(d, name, "points.pmtiles")
        os.makedirs(os.path.dirname(out))
        assert manby.tile(data, out, maximum_zoom="g", drop_densest_as_needed=True, quiet=True) == out
        t, layers = tiles(out)
        assert layers == ["points"], (name, layers)
        if name == "projected":
            # reprojected there and back, so near but not the same
            assert len(t) > 0
            continue
        if reference is None:
            reference = t
        assert t == reference, "%s makes different tiles" % name
        print("%s: the same tiles" % name)

    out = os.path.join(d, "layers.pmtiles")
    manby.tile({"a": gdf, "b": path}, out, maximum_zoom=4, quiet=True)
    assert tiles(out)[1] == ["a", "b"]
    print("two layers")

    try:
        manby.tile(gdf, os.path.join(d, "bad.pmtiles"), maximum_zoom="x", quiet=True)
        raise AssertionError("no error")
    except manby.ManbyError as e:
        assert e.returncode != 0
    print("an error from manby raises ManbyError")

    with manby.serve(path_out := os.path.join(d, "geopandas", "points.pmtiles"), port=0, open=False) as server:
        tilejson = json.load(urllib.request.urlopen(server.tilejson["points"]))
        assert tilejson["vector_layers"][0]["id"] == "points", tilejson
        assert urllib.request.urlopen(server.url).status == 200
    assert server.process.poll() is not None
    print("serves %s" % path_out)

    report = manby.stats(path_out, layers=True, largest=3)
    assert sum(z["tiles"] for z in report["zooms"]) > 0
    assert all(z["layers"]["points"]["features"] > 0 for z in report["zooms"]), report
    assert len(report["largest"]) == 3
    print("stats")

    manby.run("--version")
    return 0


if __name__ == "__main__":
    sys.exit(main())
