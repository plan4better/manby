# manby design

manby builds vector tilesets from geographic features, like tippecanoe, but several times faster, ideally ten times.
It is named after the Aaron Manby, the first iron steamship.

## Baseline

tippecanoe and its faithful Rust port, [tippecanoe-rs](crates/tippecanoe-rs), run at about the same speed.
On an Apple M5 Max (18 cores, 128 GB):

| Benchmark | Command (after `tippecanoe -f -o out.pmtiles`) | tippecanoe | Target |
| --- | --- | --- | --- |
| points | `-zg --drop-densest-as-needed -P points4m.geojsonl` | 57.5 s | 6 s |
| tracts | `-zg --coalesce-densest-as-needed -P cb_2023_us_tract_500k.geojsonl` | 28.8 s | 3 s |
| tracts-z12 | `-z12 -Z0 --coalesce-densest-as-needed -P cb_2023_us_tract_500k.geojsonl` | 54.6 s | 5.5 s |
| roads-z14 | `-z14 -Z0 --drop-densest-as-needed -P tl_2024_us_primaryroads.geojsonl` | 14.9 s | 1.5 s |

`bench/` has the scripts and describes the data.

## Where tippecanoe spends its time

Measured from tippecanoe's own progress output:

- **Low zooms on few cores.** For the 4 million points, zoom levels 0 to 2 take 39 of the 57 seconds. Those levels hold 21 tiles, and tippecanoe works on each tile with a single thread.
- **Size limit retries.** Tiles over 500 KB are rebuilt with more features dropped, about 85 times for the points and 5 times for the tracts' single zoom 0 tile. Each retry reads all of the tile's features back from a compressed temporary file.
- **Idle cores.** Averaged over a run, tippecanoe keeps 2 to 3 of the 18 cores busy.
- **Zoom by zoom.** Every zoom level finishes before the next starts, and all features pass through zlib-compressed temporary files once per zoom.
- **PMTiles by way of SQLite.** PMTiles output is written as a complete MBTiles database first, checked with `PRAGMA integrity_check`, copied into a temporary file, and copied once more into the archive. That adds 22 to 31% on runs with hundreds of thousands of tiles.
- **Reading is not the problem.** Parsing 556 MB of GeoJSON in parallel takes about 4 seconds, and sorting about 4 more.

## Principles

1. **The same tiles, first.** In its default mode manby makes the tiles tippecanoe makes, for every option it supports, so the decoded output matches tippecanoe-rs exactly. The speed comes from how the work is done, not from changing what is computed. This keeps tippecanoe's 279 expected outputs and tippecanoe-rs as the referee while the engine underneath is replaced. Speedups that change the output are opt-in (see Fast mode) and are measured separately.
2. **A library first.** manby's own code has no global state and never exits the process. An `Options` value goes in and a `Result` comes out, so manby can run inside servers, other tools and, later, WebAssembly. The `manby` command is a thin layer that accepts tippecanoe's options.
3. **Deterministic in parallel.** The output does not depend on the number of threads or on scheduling. Tiles are keyed by z/x/y, the features of a tile are always in the same order, and parallel work inside a tile is collected in order.
4. **Memory first.** Features live in memory in compact fixed-size records. Inputs larger than memory can later move those arrays into memory-mapped files without changing the code that reads them.
5. **Same results on every platform.** No result depends on unstable sorts, fused multiply-add or other platform differences, and manby makes the same tiles on macOS and Linux. Where tippecanoe's own output depends on the platform, manby follows its x86-64 build. Its arm64 builds differ: clang and GCC both fuse multiplications and additions there by default (`-ffp-contract`), which changes the size of the tracts' zoom 0 tile by 6 bytes and so which features the retry keeps. Built with `-ffp-contract=off`, or for x86-64, tippecanoe makes manby's tiles on every benchmark.

## Architecture

```
input files ──► ingest ──► FeatureStore ──► order ──► tiler ──► TileSink ──► .pmtiles / .mbtiles / directory
               (parallel)                  (sort, minzoom)   (quadtree, parallel)  (streaming)
```

### Ingest (`src/ingest/`)

The input is split into byte ranges at line or feature boundaries, and each thread parses its range into its own part of the store, with its own interned keys and values. The parts are then joined and the interned ids remapped. Parsing follows tippecanoe's rules for what counts as a feature and how attribute values are typed and written, by reusing tippecanoe-rs's `jsonpull`, `read_json` and `serial` code where that is fast enough, or a faster parser where it is not.

GeoParquet and Arrow, which tippecanoe does not read, are read as tippecanoe reads the FlatGeobuf file that GDAL would make of the same table (`src/ingest/arrow.rs`). The rows are numbered in tippecanoe's FlatGeobuf batches of `CPUS * 500 + 1`, each parsed in `CPUS` ranges, and the attributes are typed as tippecanoe types FlatGeobuf's columns, so the tiles are the same as from that file. Record batches stream in, and are parsed in parallel a window of whole batches at a time. Each window is read into one chunk per reader and run of batches, which numbers each range's features as tippecanoe does, rather than into a chunk per range of about 500 rows: those columns are too small for the memory budget to move to a file, and all chunks wait for the merge, which took about 250 bytes of heap per point for a table with one text column. The chunks grow on the heap and are settled into the file, if the budget says so, once read, since growing many vectors in the file at once is many times slower. FlatGeobuf files are read in the same windows. GeoParquet at a URL (`src/ingest/remote.rs`, the `remote` feature) is read through object_store on a tokio runtime of its own, a few row groups at a time, handed over in order through a channel. Coordinates in a CRS other than tippecanoe's two, from `-s` or GeoParquet metadata, are made longitude and latitude with proj4rs and the PROJ strings of crs-defs compiled in (`src/crs/`), point by point as they are read. `--h3` (`src/ingest/h3.rs`) counts each accepted point into the H3 cells of the finest resolution it needs, per chunk, and once every input is read adds the chunks' cells up in chunk order, the coarser resolutions from the finer, and serializes the hexagons as one more chunk with a zoom range each, so the tiling treats them as any polygons. `--cluster-categories` gives each feature a `kind:<value>` of 1 as it is read, and the tiler sums keys with that prefix as `--accumulate-attribute=...:sum` would. `--bbox` picks row groups by the statistics of GeoParquet 1.1 bounding box columns or Parquet's geospatial statistics, and rows by their geometries' bounding boxes in world coordinates, before the rows are numbered.

### FeatureStore (`src/store.rs`)

All features as fixed-size `Feature` records, with their geometry and attributes in shared arrays. Keys and values are interned once. A record holds what tippecanoe serializes for a feature (index, sequence, bbox, extent, label point, id, layer, type, minimum and maximum zoom).

### Order (`src/order.rs`)

Sorts the records by tippecanoe's sort order with a parallel radix sort, then makes the single ordered pass tippecanoe makes to assign each feature its minimum zoom from the drop rate and base zoom. It also collects what `-zg` and `-Bg` need to guess the zoom levels.

### Tiler (`src/tiler/`)

Walks the tile quadtree from the top. Each tile is a task. Its input is the features its parent passed down (clipped to the parent tile, as in tippecanoe), and its outputs are the finished tile and the clipped features for its children. Children start as soon as their parent is done, so all cores stay busy without waiting for whole zoom levels. A tile with many features splits its per-feature work (clipping, simplification, polygon cleaning, encoding) across threads and collects the results in order. When a tile is over the size or feature limit, the retry repeats only the selection and encoding of data already in memory, choosing the next threshold exactly as tippecanoe does, so the same tiles come out. The geometry algorithms come from tippecanoe-rs (`clip`, `geometry`, `visvalingam`, `shared_borders`, `mvt`), apart from polygon cleaning, which has a copy of its own (below).

A coordinator on the calling thread hands ready tiles to the pool and applies tippecanoe's zoom-level rules to the results in order. While small tiles are made, results come every few microseconds, so the coordinator waits for the next one awake for up to 200 µs before it sleeps. When it slept between results, waking it cost more than the tiles took, and the pool ran out of tasks. On Linux in a virtual machine, where a wakeup wakes a virtual processor, that nearly tripled the time of runs with many small tiles (roads at `-z14`: 2.7 s against 1.0 s on 16 processors).

### Polygon cleaning (`src/clean.rs`, `crates/wagyu`)

Polygons are cleaned with wagyu, as in tippecanoe, by manby's own copy of tippecanoe-rs's port of it, so that it can be made faster while tippecanoe-rs stays faithful to the C++. `src/clean.rs` has manby's copies of `clean_or_clip_poly` and `coalesce_polygon`, which call it. On the Nova Scotia water polygons at `-zg --coalesce-densest-as-needed`, where cleaning was most of the time, these changes took the run from 69 s to 24 s, against tippecanoe's 185 s, each with the same polygons.

- **Bounds from the front of a deque.** Building a ring's bounds took them off the front of a vector of edges, moving all the edges after them each time, which was quadratic in the length of the ring. The C++ does the same.
- **Joining rings.** When the sweep joins two rings, only the absorbed ring's points need to learn their new ring, rather than every point of the joined one.
- **Indexed rings in the tree.** Putting the rings in their tree tests each ring against larger ones with a point-in-polygon test, which walks the larger ring's edges. A ring tested more than once gets its edges in buckets by y, and the test reads the bucket that holds the point. The test decides on the edges whose y range holds the point, in any order, so the answer is the same.
- **Hot pixel rows.** A segment of a path only gets hot pixels between its two x values, so rows without one are passed over before working out where the edge crosses them.
- **The bottom point.** Joining two rings looks for the bottom point of each, by walking the ring. A ring now carries its bottom point while no other point of it is at the same place, as then the walk would find that point from wherever it started, and keeps it up as points are added and rings joined.
- **Simple rings without the sweep.** Most cleanings are of a single ring that is already simple, a census tract clipped and simplified, which wagyu gives back unchanged, in the same direction, starting at its point with the smallest y, or of two such points the one with the larger x. `wagyu::union_of_simple_ring` makes that result directly when a conservative test, quadratic and so only for rings of up to 64 points, finds no crossings, no repeated or collinear points and no edge within a pixel of another point. Otherwise the ring goes through the sweep. On the tracts at `-zg`, 1,113,328 of 1,199,465 single rings took it, each the same as the sweep's, and the test took 0.38 s where sweeping them took 2.12 s. Debug builds sweep every ring that takes it too, and check the two agree. The idea comes from tylertoo, which proves a ring simple once and then skips the general clipping for it.

`crates/wagyu/examples/wagyu_replay.rs` replays recorded cleanings through both tippecanoe-rs's wagyu and manby's, checks that every polygon is the same, and times them. On the water polygons' 270,000 recorded cleanings, manby's takes 125 s of processor time against 283 s. Some other changes measured no faster and were left out: binary search for the hot pixel rows, and other sorts where the comparison allows them. So was keeping recently cleaned big polygons, which a retried tile cleans again: it saved a tenth of the time on the water polygons for 50 MB more memory.

### Sinks (`src/sink/`)

`TileSink` receives finished tiles from any thread.
- **PMTiles.** Writes directly, without SQLite. Tiles are deduplicated by content hash, kept in memory or appended to a spill file, laid out in tile id order at the end so the archive is clustered, and the directories are built with tippecanoe-rs's `pmtiles` module. The data is written once.
- **MBTiles.** One writer thread inserting in large transactions.
- **Directory.** Parallel file writes.

### Compression (`src/compress.rs`)

The size limit is checked on the compressed size, so the size zlib makes at tippecanoe's settings decides which features a tile keeps. manby runs zlib only where that size can change a decision. zlib's `deflateBound()` is the most bytes zlib can make of a tile. When that bound, scaled up for skipped features as tippecanoe scales the size, is within the limit (or `-pk` lifts the limit), the tile is kept whatever zlib makes of it, so manby compresses it with libdeflate instead and skips the check. A tile near or over the limit goes through zlib, and when it is kept, that zlib stream is stored as is. Tiles over the feature count limit always go through zlib, since their size picks the next threshold.

libdeflate at its default level 6 makes smaller tiles than zlib -6 on all four benchmarks, by 0.1% (roads-z14), 1.1% (tracts-z12), 3.3% (tracts) and 4.1% (points), and is 1.8 to 4.5 times faster (`examples/compress_levels.rs` compares every level). So the tiles decode to exactly tippecanoe's, and the archives are smaller, but not byte for byte tippecanoe's. `--compression=zlib` stores every tile as zlib makes it, which makes tippecanoe's archive byte for byte, apart from the flag itself in the `generator_options` metadata.

## Fast mode

Opt-in changes that make different tiles, measured against the default mode for speed and for how close the tiles stay (tile sizes under the limits, features kept per zoom):
- libdeflate's compressed size for the size limit, instead of zlib's.
- Predicting each feature's share of the tile size and choosing the drop threshold in one pass, instead of encode-and-retry.
- Reading the features of a low zoom tile in drop-priority order, so a zoom 0 tile touches only the features it keeps.

## Thresholds per tile

`--as-needed-per-tile` keeps the thresholds each tile raised for itself, rather than folding the highest into its level's and making the level again. It is tippecanoe's `--force-feature-limit` for every `-as-needed` strategy, and it is how tylertoo fits its tiles. It is 1.1 to 1.2 times faster on the tracts and the points, and no faster on the roads. Most of what remains is the attempts at the tiles that are too big, each of which makes the tile again (99 attempts at the 52 tiles of z5 on the tracts at z7). Letting more zoom levels run ahead of the finished ones (`LOOKAHEAD` 64 instead of 3) saved 0.1 s of 1.8 there, as the low zoom tiles then share the threads, and was left out.

## MapLibre Tiles (MLT)

`--tile-format=mlt` writes MapLibre Tiles instead of Mapbox Vector Tiles. It is manby's first output beyond tippecanoe, so it is opt-in, and the default output does not change.
- **Encoding.** The tiler makes each tile's MVT bytes as usual, and `src/mlt.rs` turns them into MLT version 1 layers with mlt-core's `LayerWriter`, before the tile is compressed and its size checked. An MLT tile holds the layers, features (in the same order), ids, geometry and attribute values of the MVT tile. manby reads the MVT bytes itself, because mlt-core's reader refuses tiles that decode to more than 32 MiB and tippecanoe allows 200,000 features in a tile. `examples/mlt_reader_check.rs` checks that both make the same bytes.
- **Attribute types.** MLT stores each attribute of a layer as one typed column, so attributes are typed as mlt-core's own MVT conversion types them. Integers of different MVT types become one integer column, integers mixed with floats become 64-bit floats, and any other mix (strings with numbers, booleans with numbers) becomes strings, in mlt-core's text form. The order of attributes within a feature is the column order, not the feature's own.
- **Encoder profile.** `--mlt-profile` picks the encodings. The default, `web`, writes tiles that every MapLibre GL JS with MLT support reads (5.12 and later), with attribute values of the same JavaScript types as from MVT. It leaves out polygon triangles (GL JS ignores them), FastPFOR, FSST and shared dictionaries (GL JS before 6.0, whose decoder is `@maplibre/mlt` 1.1, does not read them all), and it stores integer columns whose values need more than 32 bits as doubles where that is exact (up to 2^53), because GL JS reads 64-bit integers as BigInt, which its style expressions do not compare as numbers. (mlt-core already stores integer columns that fit in 32 bits.) `native`, `min-size` and `fast` are mlt-core's own profiles and types, for MapLibre Native, the fewest bytes (which may reorder the features of a layer, and so their drawing order) and the quickest encode. GL JS reads them from 6.0.
- **Compression.** As for MVT, tiles are gzipped unless `-pC`, which is tippecanoe's rule and Planetiler's default for MLT too. On the four benchmarks gzipped MLT is 2 to 45% smaller than gzipped MVT. FSST and FastPFOR, which only `native` and `min-size` use, make tiles smaller without gzip and larger with it. MapLibre's `mlt` CLI writes MLT without gzip by default, and its `ls` reads only uncompressed files.
- **Outputs.** PMTiles archives get tile type 6 (MapLibre Vector Tile in the PMTiles spec) and `"format": "mlt"` in their metadata, MBTiles databases the `format` "mlt", and directories `z/x/y.mlt` files and `"format": "mlt"` in metadata.json, the values Martin and the `mlt` CLI use. MapLibre GL JS (5.12 and later) reads the tiles from a vector source with `encoding: "mlt"`.
- **Size limit.** The tile size limit (`-M`, 500 KB by default) is measured on the stored MLT bytes, as tippecanoe measures the stored MVT bytes. MLT tiles are smaller, so a tile that reaches the limit can keep more features than the MVT tile would, and the as-needed strategies drop and coalesce less. Tiles under the limits hold the same features as the MVT tiles.
- **Rust version.** mlt-core needs Rust 1.98, so MLT output is behind the `mlt` cargo feature (`cargo +1.98.1 build --release --features mlt`). Without it, `--tile-format=mlt` fails with an error that says so.
- **Verification.** `examples/mlt_verify.rs` decodes the MLT archive of a run with mlt-core and the MVT archive of the same run with tippecanoe-rs, and compares every tile, layer, feature, id, geometry part and attribute value under the typing rules above. `make test-golden-mlt` runs it on the golden tests.

## tile-join (`src/join/`)

`manby join` is tippecanoe's tile-join, with tile-join's options, messages and exit statuses. It makes tile-join's tiles, with their features in the same order, and tile-join's metadata. Under the name `tile-join` (a link to manby, say) it is tile-join, so it can replace it.

### Where tile-join spends its time

Copying the tracts at `-z12` (312,073 tiles, 168 MB) from PMTiles to PMTiles, tippecanoe-rs's tile-join takes 16 s, with 48 s of user time and 26 to 30 s of system time on 18 cores.
- **A megabyte for every layer of every tile.** A layer keeps two hash tables of 65,536 slots for its keys and values, which every decoded and every written layer allocates and fills. Most of the system time is that memory coming and going.
- **Attributes as strings.** Each feature's attributes go through a map keyed by their names, and into tile-stats as text, which is parsed back for the minimum and maximum of numbers.
- **zlib for every tile.**
- **PMTiles by way of SQLite.** The output is a complete MBTiles database first, copied into the archive at the end (1.7 of the 16 s).
- **Overzooming one tile at a time,** with four more tiles made for each, to find out which of its children will have features, each with its hash tables.

### Design

- **The readers' order** (`merge.rs`). Which input tiles make each output tile, and in what order, comes from tile-join's queue of readers, followed step for step, overzooming and its end included, and so does the order in which the inputs' metadata is read at the end. Nothing in it waits for tile data except where two inputs have a tile at the same z/x/y (see below).
- **Batches.** The queue hands over the input tiles of 2,048 output tiles at a time, never splitting a z/x/y. The tiles of a batch go biggest first, by the size of their input data, each thread taking the next as it finishes.
- **Tiles** (`tile.rs`). Decoding follows tippecanoe-rs's decoder call for call on its protozero reader, so that malformed tiles fail where they fail there, but borrows the strings, and keeps the geometry as encoded commands. Appending follows tile-join's `append_tile`, with the hash slots of each layer's keys and values kept in small maps rather than tables of 65,536, which gives the same keys and values in the same order, collisions included. Geometry is decoded into points only where an extent changes. Encoding makes tippecanoe-rs's bytes.
- **Tile-stats** (`stats.rs`). Each thread keeps its own, merged at the end as tile-join merges its threads', which gives the same result in any order. A full list of sample values leaves out larger values after one comparison, and numbers are taken as they are rather than parsed back from their text, which gives the same bits.
- **Compression.** tile-join leaves out tiles over 500,000 compressed bytes unless `-pk`. As for tiling, a tile whose zlib bound is within that is compressed with libdeflate, and only the others with zlib. A tile that comes out as its one input tile, byte for byte, keeps the input's gzip stream when zlib or libdeflate made it (their headers say so), which spares compressing it again, so a copy compresses nothing. `--compression=zlib` compresses every tile with zlib, which makes tile-join's archive byte for byte, apart from the command line in `generator_options`.
- **Reading** (`source.rs`). PMTiles directories are read at the start, and the tiles of each batch from a mapping of the archive made for that batch, whose pages leave memory with it. Reading each tile with `pread` took longer, since reads of one file do not overlap on macOS (152,000 random reads take 0.1 s on one thread and 0.15 s on eighteen). MBTiles rows come from tile-join's query on a thread of their own, a chunk at a time. Directories are listed at the start and their files read by the threads that need them.
- **Overzooming** (`overzoom.rs`). tippecanoe-rs's `overzoom_source_tiles` with tile-join's arguments, making the same bytes with the output layers of the joining code and tippecanoe-rs's own clipping and cleaning. The children it tries only need to know whether a feature survives, so they stop at the first. A reader makes its overzoomed tiles 256 at a time on all threads, in the order the queue takes them, from the same parent tiles.
- **Output.** manby's sinks, PMTiles written directly.

### Tiles at the same z/x/y (`key.rs`)

Where several inputs have a tile at the same z/x/y, tile-join appends their features in the order of the stored bytes of the tiles. manby compresses most tiles with libdeflate, so its tiles' bytes are not tippecanoe's and sort differently, and tippecanoe and then tile-join, and manby and then tile-join, give the features of shared tiles in different orders. So manby join compares a tile by the bytes zlib makes of it, with tippecanoe's settings, which are the bytes tippecanoe would have stored, when
- its gzip header says libdeflate made it (OS byte 255, where zlib writes its own system's code), and
- its tileset's `generator` says tippecanoe or tile-join made it, which manby's tilesets say.

tippecanoe and tile-join store only zlib's bytes, so this changes nothing for their tilesets, and other tools' tilesets keep the comparison of stored bytes. Nothing changes for manby's tilesets made with `--compression=zlib` either, since all their tiles are zlib's. tile-join's own output says tile-join made it, and manby join stores only zlib's or libdeflate's streams, so joins of joins compare the same way.

So manby and then manby join make what tippecanoe and then tile-join make, which `bench/join.sh` checks for its merges, and tile-join's tests pass with manby making their inputs. The price is that tile-join itself, run on manby's tilesets, can order the features of shared tiles differently from manby join on the same files. `TieBreak::Bytes` in `JoinOptions` compares stored bytes for every tile, as tile-join does.

Compressing with zlib to compare costs processor time where shared tiles are many and large. The queue works these keys out ahead on all threads. When a tile ties with another reader's, it looks at each reader's next tiles (up to 16 MB or 16,384 tiles), finds those that share a z/x/y, and works out their keys together. Merging the points and the tracts that manby made takes 0.46 s comparing zlib's bytes against 0.42 s comparing stored bytes (medians of five), with 0.5 s more processor time. Comparing as the queue came to the tiles, one at a time, it took 0.93 s.

### Differences from tile-join

- **Messages.** The progress line names the first tile of each batch, and tiles over the size limit are reported as threads finish them, so in another order.
- **Duplicate tiles.** An MBTiles database without a unique index can have two rows for one z/x/y. tile-join can write such a tile twice; manby join joins the rows into one tile.
- **Errors inside tippecanoe-rs.** A feature filter that tippecanoe-rs's evaluator does not understand still ends the process from inside it, as in tile-join, rather than coming back as an error.

## Library and command

`manby::run(&Options) -> Result<Summary>` is the entry point for tiling, and `manby::join(&JoinOptions) -> Result<JoinSummary>` for joining. Transitionally, the algorithm functions borrowed from tippecanoe-rs read tippecanoe-rs's globals. manby sets those once from `Options` and replaces each borrowed function with its own parameterized copy when it needs to change it.

`manby serve` (`src/serve/`) serves tilesets for looking at them: tiles, TileJSON and PMTiles archives with range requests, over tiny_http, and a page with a MapLibre map (`viewer.html`, compiled in). It reads a tileset with positioned reads rather than a mapping, so an archive rewritten while it is served can't fault the server, and opens it again when its size, time or inode changes. `manby stats` (`src/stats.rs`) lists the same readers' tiles, a PMTiles archive's from its directories alone, and with `--layers` decodes each distinct tile once, in parallel.

## Verification

- **Expected outputs.** `make test-golden` runs tippecanoe's `.json.check` recipes with `manby` for the options it supports, and tile-join's recipes with `manby join` as `./tile-join`. A skip list names the rest and shrinks over time.
- **Against tippecanoe.** The benchmark scripts compare manby's decoded output with tippecanoe-rs's, or with the C++ tippecanoe's (`TIPPECANOE`), for every benchmark, every time. Unit tests check the decoding, joining and overzooming of `manby join` against tippecanoe-rs's and tile-join's own code on random tiles.
- **Unit tests** for each module, and determinism tests that run the same input with 1, 2 and 18 threads.

## Milestones

1. **Points.** GeoJSON points to PMTiles in the default mode, identical to tippecanoe on the points benchmark, at least 5 times faster.
2. **Lines and polygons.** The tracts, tracts-z12 and roads-z14 benchmarks identical and at least 5 times faster, including coalescing and polygon cleaning.
3. **Options.** Most of tippecanoe's options, MBTiles and directory output, FlatGeobuf and CSV input, most golden tests passing.
4. **Beyond tippecanoe.** Fast mode, MapLibre Tiles (MLT) output, input through terrio (GeoPackage, GeoParquet, Shapefile), WebAssembly.
