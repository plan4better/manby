---
title: Getting started
description: Installing manby, and making a first tileset with tippecanoe's options.
---

manby runs on macOS and Linux, and on Windows in WSL. pip installs it.

```sh
pip install manby
```

uv runs it without installing.

```sh
uvx manby -zg --drop-densest-as-needed -o places.pmtiles places.geojson
```

Homebrew and mise install the program alone, from the releases.

```sh
brew install birkskyum/tap/manby
mise use -g github:birkskyum/manby
```

The [releases](https://github.com/birkskyum/manby/releases) have the program alone, for macOS on Apple silicon and Intel, and for Linux on x86-64 and arm64, built for glibc 2.28 or later, or for musl, which runs on any Linux.

cargo builds it, with Rust 1.89 or later, but without [MapLibre Tiles](../guides/mlt/).

```sh
cargo install manby
```

From a clone, the program ends up in `target/release/manby`.

```sh
git clone https://github.com/birkskyum/manby
cd manby
cargo build --release
```

The repository also has [tippecanoe-rs](https://github.com/birkskyum/manby/tree/main/crates/tippecanoe-rs), the faithful Rust port of tippecanoe that manby uses as a library.

## A first tileset

manby takes [tippecanoe's options](https://github.com/felt/tippecanoe#readme), and a tippecanoe command works with `manby` in its place.

```sh
manby -zg --drop-densest-as-needed -o places.pmtiles places.geojson
```

The input can be GeoJSON, newline-delimited GeoJSON (read in parallel with `-P`), FlatGeobuf, CSV, Geobuf, [GeoParquet or Arrow](../guides/python/), and GeoParquet in object storage or on the web, by URL. GeoPackage, Shapefile and OpenStreetMap data can be [converted to FlatGeobuf](../guides/other-formats/) with GDAL first. The output is a PMTiles archive, an MBTiles database, or a directory of tiles with `-e`.

Points can be clustered as tippecanoe clusters them, with their attributes accumulated.

```sh
manby -zg -r1 --cluster-distance=10 --cluster-densest-as-needed \
  --accumulate-attribute=value:sum -o clusters.pmtiles points.geojsonl
```

## From Python

`pip install manby` also installs a Python module, which tiles files, DuckDB queries and data frames. [GeoParquet, DuckDB and Python](../guides/python/) has more.

```python
import manby

manby.tile("places.geojson", "places.pmtiles", maximum_zoom="g", drop_densest_as_needed=True)
```

## From R

The R package tiles files, sf data frames and DuckDB tables. [From R](../guides/r/) has more.

```r
install.packages("manby", repos = c("https://birkskyum.r-universe.dev", "https://cloud.r-project.org"))

manby::tile(sf::st_read("places.gpkg"), "places.pmtiles", maximum_zoom = "g", drop_densest_as_needed = TRUE)
```

## Testing

```sh
make test
```

`make test` runs the unit tests and tippecanoe's own test suite with manby in place of tippecanoe, from GeoJSON input and again from Geobuf input. All 188 of tippecanoe's expected outputs match, and the tests of tippecanoe's other programs pass with manby as each of them.

## As a library

manby is a library first. `manby::run(&Options)` reads, tiles and writes a tileset and returns an error instead of exiting, `manby::join(&JoinOptions)` joins tilesets, and the commands are thin layers over them.
