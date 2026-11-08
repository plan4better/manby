# manby

[![crates.io](https://img.shields.io/crates/v/manby)](https://crates.io/crates/manby)
[![PyPI](https://img.shields.io/pypi/v/manby)](https://pypi.org/project/manby/)
[![R-universe](https://birkskyum.r-universe.dev/manby/badges/version)](https://birkskyum.r-universe.dev/manby)
[![Homebrew](https://img.shields.io/badge/homebrew-birkskyum%2Ftap-orange)](https://github.com/birkskyum/homebrew-tap)
[![release](https://img.shields.io/github/v/release/birkskyum/manby)](https://github.com/birkskyum/manby/releases)

manby builds vector tilesets from GeoJSON, FlatGeobuf, CSV, Geobuf, GeoParquet or Arrow features, GeoParquet in object storage, and from DuckDB queries and data frames in Python and R. It makes the same tiles as [tippecanoe](https://github.com/felt/tippecanoe), in less memory, ten to eighteen times faster on a Mac and eight to fifteen times faster on Linux, more where tippecanoe's disk slows it down. `manby join` is tippecanoe's tile-join, two to thirty-five times faster.

The `manby` command takes tippecanoe's options:

```sh
manby -zg --drop-densest-as-needed -o places.pmtiles places.geojson
```

The [site](https://birkskyum.github.io/manby/) has the guides.

## Install

manby runs on macOS and Linux, and on Windows in WSL.

| | |
| --- | --- |
| Homebrew | `brew install birkskyum/tap/manby` |
| pip, with the Python module | `pip install manby` |
| uv, without installing | `uvx manby` |
| mise | `mise use -g github:birkskyum/manby` |
| cargo, with Rust 1.89 or later | `cargo install manby` |
| R, from [R-universe](https://birkskyum.r-universe.dev/manby) | `install.packages("manby", repos = c("https://birkskyum.r-universe.dev", "https://cloud.r-project.org"))` |
| The program alone | the [releases](https://github.com/birkskyum/manby/releases), for macOS and Linux on arm64 and x86-64 |

cargo builds it without MapLibre Tiles and URLs, which `--features mlt,remote` adds, with Rust 1.98.

## From Python and R

From Python, a DuckDB query streams to manby in Arrow batches.

```python
import duckdb, manby

places = duckdb.sql("SELECT name, geometry FROM 'places.parquet' WHERE country = 'DE'")
manby.tile(places, "places.pmtiles", maximum_zoom="g", drop_densest_as_needed=True)
```

The R package tiles sf data frames and DuckDB tables the same way. `manby serve` serves tilesets with a map of them, and `manby stats` reports their tiles' sizes, from the command line, R or Python.

```r
manby::tile(sf::st_read("places.gpkg"), "places.pmtiles", maximum_zoom = "g", drop_densest_as_needed = TRUE)
```

## Speed

Against tippecanoe 2.82.0 itself, the C++ built with its own Makefile, writing PMTiles. Each figure is the median of three runs, and memory is peak resident memory. tippecanoe's runs and manby's come from separate sessions on the same machine, since tippecanoe's do not change with manby's version.

On macOS, on an Apple M5 Max with 18 cores and 128 GB:

| Benchmark | tippecanoe | manby | |
| --- | --- | --- | --- |
| 4 million points, `-zg --drop-densest-as-needed` | 61.4 s, 2,276 MB | 3.44 s, 919 MB | 17.8x |
| The same points, `-zg -r1 --cluster-distance=10 --cluster-densest-as-needed --accumulate-attribute=value:sum` | 54.4 s, 1,318 MB | 3.88 s, 891 MB | 14.0x |
| The same points, `-zg --cluster-densest-as-needed` | 59.8 s, 2,330 MB | 4.07 s, 916 MB | 14.7x |
| The same points, `-zg -r1 --cluster-densest-as-needed --accumulate-attribute=value:sum` | 87.7 s, 3,443 MB | 7.82 s, 1,115 MB | 11.2x |
| 17,522 roads at `-z14`, 122,725 tiles | 11.8 s, 327 MB | 0.81 s, 177 MB | 14.5x |
| 85,186 census tracts at `-z12`, 312,073 tiles | 46.6 s, 566 MB | 3.78 s, 338 MB | 12.3x |
| 85,186 census tracts, `-zg --coalesce-densest-as-needed` | 24.8 s, 554 MB | 2.27 s, 366 MB | 10.9x |

On Linux, on a Lenovo Yoga Slim 7x with a Snapdragon X Elite (12 cores, 32 GB), running Arch Linux ARM:

| Benchmark | tippecanoe | manby | |
| --- | --- | --- | --- |
| 4 million points, `-zg --drop-densest-as-needed` | 88.5 s, 1,088 MB | 5.86 s, 994 MB | 15.1x |
| The same points, clustered within 10 pixels, summing `value` | 82.5 s, 1,028 MB | 6.02 s, 974 MB | 13.7x |
| The same points, `-zg --cluster-densest-as-needed` | 89.6 s, 1,165 MB | 6.37 s, 1,000 MB | 14.1x |
| The same points, `-zg -r1 --cluster-densest-as-needed`, summing `value` | 133.6 s, 1,498 MB | 13.9 s, 1,152 MB | 9.6x |
| 17,522 roads at `-z14`, 122,725 tiles | 59.0 s, 240 MB | 1.51 s, 236 MB | 39.1x |
| 85,186 census tracts at `-z12`, 312,073 tiles | 196.8 s, 456 MB | 7.77 s, 360 MB | 25.3x |
| 85,186 census tracts, `-zg --coalesce-densest-as-needed` | 36.1 s, 451 MB | 4.46 s, 379 MB | 8.1x |

On this machine tippecanoe uses 8 of the 12 cores, since it rounds its thread count down to a power of 2, which keeps its memory low, while manby uses all 12. tippecanoe is slow on the runs with many tiles here because it writes PMTiles by way of a SQLite database next to the output, on a btrfs filesystem that compresses with zstd. With the output in memory instead, roads take it 27 s rather than 59. The kernel also backs memory with 2 MB pages wherever it can (transparent huge pages set to `always`, as Arch sets them), which adds about a tenth to manby's figures, and leaves its roads only just below tippecanoe's. `bench/` has the data, the scripts and how the Linux numbers were made.

For every run the tiles decode to exactly what tippecanoe makes, with one exception. tippecanoe's arm64 builds, from Apple's clang on macOS and from GCC on Linux, fuse multiplications and additions where its x86-64 build does not, and that changes a few tiles near the size limit. On these benchmarks it changes only the census tracts at `-zg`, here and 20 times over below. In the tracts, the zoom 0 tile comes out 6 bytes smaller compressed, so that tippecanoe's retry keeps other features. manby makes the same tiles on macOS and Linux alike, the tiles of tippecanoe's x86-64 build. Built for x86-64, or on arm64 with `-ffp-contract=off`, tippecanoe makes manby's tiles on the tracts too.

[tippecanoe-rs](crates/tippecanoe-rs), the faithful Rust port that manby takes tippecanoe's algorithms from, runs at about the speed of the C++. So porting tippecanoe to Rust is not what makes manby fast. Doing the work differently is.

The archives are not tippecanoe's byte for byte, and a little smaller. zlib's exact output decides which features a tile keeps only when the tile could be near the size limit. Every other tile is compressed with libdeflate, which is faster and compresses tiles better. `--compression=zlib` compresses every tile with zlib, as tippecanoe does, for the same archive bytes.

## How

tippecanoe spends most of its time waiting on itself rather than computing. It works through the low zoom levels one tile per thread, rebuilds oversized tiles by reading their features back from compressed temporary files, finishes each zoom level before starting the next, and writes PMTiles by way of a SQLite database. manby keeps the features in memory, sorted once, walks the tile pyramid in parallel on every core, splits big tiles across threads, retries oversized tiles on data already in memory, and writes PMTiles directly. The geometry algorithms are tippecanoe's own, by way of tippecanoe-rs, which is why the tiles come out the same. [DESIGN.md](DESIGN.md) has the measurements and the architecture.

## Memory

manby's peak memory is below tippecanoe's on the benchmarks. For inputs larger than memory, `--memory-budget` bounds it (default: half the physical memory, as tippecanoe sizes its sorting). Past the budget, the features and the tile data live in an unlinked temporary file in the `-t` directory, mapped into memory, laid out in the order the tiling reads them, and manby keeps its resident memory near the budget. `bench/large.sh` tiles inputs several times larger than a 1 GB budget, once each:

| Input | System | tippecanoe | manby, `--memory-budget=1G` |
| --- | --- | --- | --- |
| 40 million points (5.6 GB), `-zg --drop-densest-as-needed` | macOS | 652 s, 10,206 MB | 79 s, 1,139 MB |
| | Linux | 1,025 s, 6,379 MB | 121 s, 1,188 MB |
| The census tracts 20 times over (3 GB), `-zg --coalesce-densest-as-needed` | macOS | 564 s, 4,810 MB | 137 s, 1,022 MB |
| | Linux | 854 s, 3,730 MB | 269 s, 1,206 MB |

## Joining tilesets

`manby join` takes tile-join's options, prints its messages, exits as it does, and makes the same tiles, with their features in the same order. It merges tilesets, joins attributes from a CSV file, filters and renames layers, filters features, overzooms, and converts between MBTiles, PMTiles and directories:

```sh
manby join -o merged.pmtiles roads.pmtiles places.mbtiles
manby join -c tracts.csv -o joined.pmtiles tracts.pmtiles
```

`manby tile-join` works too. Under the name `tile-join`, a link to it for example, manby is tile-join, so it can take its place.

On the same machines, median of three runs, against tile-join 2.82.0, the C++. For every run the tiles decode to exactly what tile-join makes.

| Benchmark | System | tile-join | manby join | |
| --- | --- | --- | --- | --- |
| The tracts at `-z12` (312,073 tiles) copied to PMTiles | macOS | 21.9 s, 589 MB | 0.63 s, 161 MB | 34.8x |
| | Linux | 24.2 s, 461 MB | 1.05 s, 185 MB | 23.1x |
| The same, to MBTiles | macOS | 16.6 s, 560 MB | 1.19 s, 380 MB | 13.9x |
| | Linux | 20.4 s, 463 MB | 1.28 s, 371 MB | 16.0x |
| The same tracts joined to a CSV keyed on `GEOID` | macOS | 22.6 s, 652 MB | 1.09 s, 296 MB | 20.7x |
| | Linux | 25.9 s, 556 MB | 1.92 s, 325 MB | 13.5x |
| The points and the tracts, both `-zg`, merged to PMTiles | macOS | 1.20 s, 701 MB | 0.38 s, 269 MB | 3.2x |
| | Linux | 1.62 s, 573 MB | 0.68 s, 249 MB | 2.4x |
| The same, to MBTiles | macOS | 1.15 s, 707 MB | 0.40 s, 273 MB | 2.9x |
| | Linux | 1.51 s, 567 MB | 0.69 s, 258 MB | 2.2x |
| Counties at `-z5` and tracts at `-z9`, with `--overzoom` | macOS | 3.87 s, 592 MB | 0.39 s, 182 MB | 9.9x |
| | Linux | 5.99 s, 440 MB | 0.72 s, 196 MB | 8.3x |

The merges spend most of their time on a hundred tiles near the 500 KB limit, which manby join, like tile-join, has to compress with zlib to check their size. `bench/join.sh` has the benchmarks.

tile-join decodes each tile into layers that each allocate a megabyte of hash tables, does its overzooming one tile at a time, compresses every tile with zlib, and writes PMTiles by way of a SQLite database. manby join keeps tile-join's order of input tiles and its metadata, does the work on the tiles on every core without those tables, keeps the compression of tiles it does not change, and writes PMTiles directly. [DESIGN.md](DESIGN.md) has the details.

Where several inputs have a tile at the same z/x/y, tile-join appends their features in the order of the tiles' stored bytes. manby compresses most tiles with libdeflate, whose bytes sort differently from zlib's, so for tilesets that manby made, manby join compares tiles by the bytes zlib makes of them, which are tippecanoe's. So manby and then manby join make the tiles that tippecanoe and then tile-join make, with the tiles of tippecanoe's x86-64 build as above. tile-join itself, run on manby's tilesets, can order the features of those shared tiles differently. Tilesets that manby made with `--compression=zlib` are joined the same way by both.

## tippecanoe's other programs

manby also runs tippecanoe's smaller programs, as tippecanoe-rs has them, so one program does all of tippecanoe's work:

| tippecanoe | manby |
| --- | --- |
| `tippecanoe` | `manby` |
| `tile-join` | `manby join` |
| `tippecanoe-decode` | `manby decode` |
| `tippecanoe-enumerate` | `manby enumerate` |
| `tippecanoe-overzoom` | `manby overzoom` |
| `tippecanoe-json-tool` | `manby json-tool` |

Each program's own name works as the subcommand too (`manby tippecanoe-decode`), and under that name, through a link, manby is that program, so links named after all six make manby a drop-in replacement for tippecanoe. Only the tiling and joining are manby's own, since that is where the time goes. The other four are tippecanoe-rs's code and run at its speed.

To tile an input file named like a subcommand, such as `decode`, write it as `./decode`.

## Building and testing

manby uses tippecanoe-rs, in `crates/tippecanoe-rs`, as a library. The port has tippecanoe's six programs, built with `cargo build --release -p tippecanoe-rs`, and runs tippecanoe's test suite against them with `make -C crates/tippecanoe-rs test`.

```sh
cargo build --release
make test
```

`make test` runs the unit tests and tippecanoe's own test suite with manby in place of tippecanoe, from GeoJSON input and again from Geobuf input. All 188 of tippecanoe's expected outputs match, and the tests of tippecanoe's other programs pass with manby as each of them.

manby is a library first. `manby::run(&Options)` reads, tiles and writes a tileset and returns an error instead of exiting, `manby::join(&JoinOptions)` joins tilesets, and the commands are thin layers over them.

## MapLibre Tiles

`--tile-format=mlt` writes [MapLibre Tiles](https://maplibre.org/maplibre-tile-spec/) instead of Mapbox Vector Tiles, with the same features, into PMTiles, MBTiles or a directory. The tiles are 2 to 45% smaller than MVT on the benchmarks, and MapLibre GL JS 5.12 and later reads them from a vector source with `encoding: "mlt"`.

```sh
manby -zg --drop-densest-as-needed --tile-format=mlt -o places.pmtiles places.geojson
```

The wheels and the release binaries write MLT. The encoder, mlt-core, needs Rust 1.98, so from source MLT output is a cargo feature, `cargo +1.98.1 build --release --features mlt`.

DESIGN.md describes the choices behind the tiles, and `make test-golden-mlt` checks them against the MVT tiles of the same runs.

## Fitting each tile on its own

When a tile is too big, tippecanoe raises its threshold for dropping, coalescing or clustering until it fits, then makes the whole zoom level again with the highest threshold any tile needed. `--as-needed-per-tile` keeps each tile's own threshold instead. Only the tiles that are too big are thinned, so the others keep more, but neighboring tiles can be thinned differently, which can show at tile edges. On the census tracts, the tiles of z5 keep 67,163 tract polygons rather than 17,094, and the run is 1.2 times faster. The tiles are not tippecanoe's.

## Thanks

manby is built on tippecanoe. [Erica Fischer](https://github.com/e-n-f) started it in 2014 and, over twelve years at Mapbox and then at Felt, wrote nearly all of it. The way it thins features evenly across a zoom level until every tile fits, and the test suite that pins all of that down, are what manby keeps, and they are her work. Thank you, Erica.

## License

BSD 2-Clause, see [LICENSE.md](LICENSE.md).
