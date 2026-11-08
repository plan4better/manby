---
title: GeoParquet, DuckDB and Python
description: Tiling GeoParquet and Arrow files on the command line, and DuckDB queries and data frames from Python, with tippecanoe's tiles.
---

manby reads GeoParquet and Arrow files directly, and its Python package tiles DuckDB queries, GeoDataFrames, and Polars and PyArrow tables. The tiles are the ones tippecanoe makes from the same features as FlatGeobuf.

## GeoParquet

```sh
manby -zg --drop-densest-as-needed -o buildings.pmtiles buildings.parquet
```

Arrow IPC files and streams, `.arrow`, `.arrows` and `.feather`, read the same way.

- **Geometry.** The column that the GeoParquet metadata names, or the first with a GeoArrow type, or a column named `geometry` in well-known binary. Rows without a geometry are left out.
- **Coordinates.** In the CRS that the GeoParquet or GeoArrow metadata names: longitude and latitude, Web Mercator, or any of the 9,508 EPSG and ESRI codes of [crs-defs](https://github.com/birkskyum/crs-defs), which are made longitude and latitude within centimeters of PROJ. Without a CRS, `-s` names it.
- **Attributes.** Each field of a struct becomes an attribute of its own, such as `names.primary`, and lists and maps become JSON text, as GDAL makes them. Dates and times become ISO 8601 text, with a `Z` for UTC where they have a zone. Binary columns are left out.

### In object storage and on the web

A URL reads GeoParquet in place: one file, a prefix ending in `/`, or a glob, whose files are read in the order of their names as one table. Only the row groups that `--bbox` may overlap are fetched, by byte range, so a city out of a planet takes seconds.

```sh
manby -zg --drop-densest-as-needed --bbox=13.08,52.33,13.76,52.68 -o berlin.pmtiles \
  's3://overturemaps-us-west-2/release/2026-10-21.0/theme=places/type=place/*'
```

That reads 29 of the 6,144 row groups of Overture's places, 12 GB in 16 files, in 6 to 25 seconds from Europe. The layer is named after the last part of the path, here `place`.

- **URLs.** `s3://`, `gs://`, `az://` and `https://`. Credentials come from the environment, as `AWS_ACCESS_KEY_ID` or `GOOGLE_APPLICATION_CREDENTIALS`. Without them, requests go unsigned, as public buckets take them, and an S3 bucket's region is looked up unless `AWS_REGION` gives it.
- **`--bbox=minlon,minlat,maxlon,maxlat`** keeps the rows whose geometries' bounding boxes overlap it, from local files as well. The row groups come from the statistics of GeoParquet 1.1 bounding box columns, or of Parquet's own geospatial statistics. The tiles are those of the table of the rows kept.
- **Builds.** The releases, pip, Homebrew and R read URLs. Built with cargo, it needs `--features remote`.

## From Python

`pip install manby` also installs the `manby` module. `manby.tile` takes the input, the tileset to write, and tippecanoe's options.

```python
import manby

manby.tile("buildings.parquet", "buildings.pmtiles", "-zg", "--drop-densest-as-needed")
```

The options can be keywords too, named as tippecanoe's long options, with `True` for a flag and a list to repeat one.

```python
manby.tile(
    "buildings.parquet",
    "buildings.pmtiles",
    maximum_zoom="g",
    drop_densest_as_needed=True,
    include=["height", "class"],
)
```

An existing tileset is overwritten, unless `force=False`. If manby fails, `manby.ManbyError` has tippecanoe's exit status.

### DuckDB

A DuckDB query goes in as it is. Its result streams to manby in Arrow batches, so it need not fit in memory.

```python
import duckdb, manby

con = duckdb.connect()
con.sql("LOAD spatial")
roads = con.sql("""
    SELECT name, class, geometry
    FROM 'roads.parquet'
    WHERE class IN ('motorway', 'trunk', 'primary')
""")
manby.tile(roads, "roads.pmtiles", layer="roads", maximum_zoom=14)
```

### Data frames

A GeoDataFrame in another CRS is reprojected to WGS 84 first.

```python
import geopandas, manby

counties = geopandas.read_file("counties.gpkg")
manby.tile(
    counties,
    "counties.pmtiles",
    layer="counties",
    maximum_zoom="g",
    coalesce_densest_as_needed=True,
)
```

Polars and PyArrow tables go in the same way, as does anything else with an Arrow stream, with the geometry in a GeoArrow column or in a column named `geometry` in well-known binary.

### Layers

A dict names the layers, each a table or a file.

```python
manby.tile({"roads": roads, "places": "places.parquet"}, "map.pmtiles", maximum_zoom=14)
```

### Viewing

`manby.serve` serves tilesets with a map of them, in a browser or in a notebook, and `manby.stats` reports their tiles' sizes and features. [Looking at tilesets](../serve/) has more.

```python
server = manby.serve("roads.pmtiles")
server.stop()
```

### Other commands

`manby.run` runs manby with arguments as on the command line, such as `manby join`.

```python
manby.run("join", "-o", "map.pmtiles", "roads.pmtiles", "places.pmtiles")
```
