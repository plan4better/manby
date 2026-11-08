---
title: Why manby?
description: What manby is for, how it compares with tippecanoe and the other ways of making vector tiles, and when to use something else.
---

## The short answer

- **tippecanoe's tiles.** manby takes tippecanoe's options and makes the tiles tippecanoe makes. All 188 of tippecanoe's expected test outputs match, so a tippecanoe command works with `manby` in its place.
- **Much faster.** Eight to eighteen times faster on the [benchmarks](../reference/benchmarks/), and up to forty times where tippecanoe's disk slows it down.
- **Less memory.** manby's peak memory is below tippecanoe's on every benchmark, on macOS and on Linux.
- **Larger than memory.** With [`--memory-budget`](../guides/memory/), 40 million points take 79 seconds in 1.1 GB, where tippecanoe takes 652 seconds and 10 GB.
- **tile-join too.** [`manby join`](../guides/join/) makes tile-join's tiles, two to thirty-five times faster.
- **MapLibre Tiles.** [`--tile-format=mlt`](../guides/mlt/) writes MLT as well as Mapbox Vector Tiles.

## Compared with the alternatives

| Tool | Written in | Reads | Keeping tiles small | MapLibre Tiles |
| --- | --- | --- | --- | --- |
| tippecanoe | C++ | GeoJSON, FlatGeobuf, CSV, Geobuf | Drops, coalesces or clusters features evenly across each zoom level until its tiles fit | In review |
| <span class="whitespace-nowrap">tippecanoe-rs</span> | Rust | As tippecanoe | As tippecanoe, with its tiles | No |
| manby | Rust, also from Python and R | As tippecanoe, GeoParquet (on disk or in object storage), Arrow, DuckDB queries and data frames, and [other formats by way of GDAL](../guides/other-formats/) | As tippecanoe, with its tiles, or [each tile on its own](../guides/per-tile/), or [H3 hexagons](../guides/h3/) below a base zoom | Yes |
| tylertoo | Rust, also from Python | GeoParquet | A size limit on each tile on its own, with its own thinning and simplification, so not tippecanoe's tiles | No |
| planetiler | Java | OpenStreetMap, Shapefile, GeoPackage, GeoJSON, GeoParquet | A profile, in Java or YAML, decides what each zoom level keeps | Yes |
| GDAL (ogr2ogr) | C++ | Almost any vector format | Each tile on its own, at a lower resolution past 500 kB, then only its largest features | No |
| freestiler | Rust, from R and Python | Data frames, GeoParquet, DuckDB queries | A drop rate below a base zoom, clustering and coalescing, not fitted to a size limit | Experimental |
| geojson-vt | JavaScript | GeoJSON | Simplification, with tiles made in the browser as the map asks for them | No |

### tippecanoe

[tippecanoe](https://github.com/felt/tippecanoe) is the reference. [Erica Fischer](https://github.com/e-n-f) started it in 2014 and wrote nearly all of it, at Mapbox and then at Felt. Its way of thinning features evenly across a zoom level until every tile fits is what manby keeps, and owes to her. If tippecanoe is fast enough for you, there is no need to switch.

### tippecanoe-rs

A line-by-line Rust port of tippecanoe, in [manby's repository](https://github.com/birkskyum/manby/tree/main/crates/tippecanoe-rs). It makes the same tiles at the speed of the C++, so a port alone is no faster. manby takes its geometry from the port, which is why the tiles are the same, and its speed from doing the work differently.

### tylertoo

[tylertoo](https://github.com/geoparquet-io/tylertoo) tiles GeoParquet into PMTiles, from the command line or from Python, with remote reads and builds split across machines. It fits each tile on its own rather than a zoom level at a time, so its tiles are not tippecanoe's. manby does that too with [`--as-needed-per-tile`](../guides/per-tile/). On [manby's benchmarks](../reference/benchmarks/#against-tylertoo) it is faster on the census tracts and slower on points and roads, with three to eight times manby's memory.

### planetiler

[planetiler](https://github.com/onthegomap/planetiler) builds basemaps such as OpenMapTiles from OpenStreetMap, the whole planet in hours on one machine, with a profile deciding what each zoom level keeps. manby tiles your own features as they are.

### GDAL

`ogr2ogr` writes vector tiles from anything GDAL reads, each tile on its own, cutting tiles over 500 kB down to their largest features. It is also the usual way into manby for the formats manby does not read.

### freestiler

[freestiler](https://github.com/walkerke/freestiler) tiles data frames and DuckDB queries from R and Python, with tippecanoe's drop rate, clustering and coalescing, but without fitting tiles to a size limit.

### geojson-vt

[geojson-vt](https://github.com/maplibre/geojson-vt) slices GeoJSON into tiles in the browser, as MapLibre GL JS does for GeoJSON sources. For data small enough to send whole, no tileset is needed.

## When to use something else

- **A basemap from OpenStreetMap.** planetiler, or tilemaker, with a profile such as OpenMapTiles. manby tiles the features it is given.
- **Tiles that follow a live database.** PostGIS's `ST_AsMVT`, served by [Martin](https://maplibre.org/martin/), makes each tile when it is asked for.
- **Small GeoJSON in a web map.** MapLibre's GeoJSON source, with geojson-vt.
- **Small inputs.** They take seconds with tippecanoe too, and tippecanoe is the reference.

## When manby fits

- **tippecanoe's tiles, in a fraction of the time.** Tilesets rebuilt often, in CI or every night, or many of them.
- **Large inputs on a small machine.** `--memory-budget` keeps memory near a budget, at a fraction of tippecanoe's time.
- **PMTiles for hosting.** manby writes PMTiles directly, ready for a bucket or a CDN.
- **GeoParquet, DuckDB, Python and R.** manby reads GeoParquet and Arrow directly, from disk or object storage, and tiles DuckDB queries and data frames [from Python](../guides/python/) and [from R](../guides/r/).
- **GeoPackage, Shapefile or OpenStreetMap.** GDAL's `ogr2ogr`, or any converter like it, makes FlatGeobuf of them, which manby reads in parallel. [Other formats](../guides/other-formats/) shows how.
