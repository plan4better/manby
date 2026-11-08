---
title: Looking at tilesets
description: Serving tilesets with a map of them while they are made, and reporting their tiles' sizes and features, from the command line, R or Python.
---

`manby serve` serves PMTiles archives, MBTiles databases and directories of tiles, with a map of them.

```sh
manby serve --open roads.pmtiles places.mbtiles
```

The map, at `http://127.0.0.1:8080/`, shows each layer of a tileset on a basemap, and the attributes of the features clicked. A tileset made again while it is served shows when the map reloads, so `manby -f` and the map make a quick loop. The map loads MapLibre GL JS and the basemap from the internet.

- **`--port`** is 8080 unless given, or 0 for any free port.
- **`--host`** is 127.0.0.1, so only this computer can reach the server. `--host=0.0.0.0` serves the local network too.
- **`--open`** opens the map in a browser.

It is a server for looking at tilesets, not for hosting them. A PMTiles archive on a bucket or a CDN needs no server.

## In a map of one's own

Each tileset's TileJSON is at `/{name}.json`, its tiles at `/{name}/{z}/{x}/{y}.mvt` (`.mlt` for [MapLibre Tiles](../mlt/)), and a PMTiles archive is also served whole, at its file name, with range requests. Every response allows any origin.

```js
map.addSource('roads', { type: 'vector', url: 'http://127.0.0.1:8080/roads.json' });
```

## From R

`serve()` shows the map in the viewer pane, or a browser, and gives the addresses.

```r
server <- manby::serve("roads.pmtiles")
server$tilejson[["roads"]]
server$stop()
```

With mapgl, the archive goes in as PMTiles.

```r
mapgl::maplibre() |>
  mapgl::add_pmtiles_source(id = "roads", url = "http://127.0.0.1:8080/roads.pmtiles")
```

## From Python

`manby.serve()` opens the map in a browser, or in a notebook shows it there.

```python
server = manby.serve("roads.pmtiles")
server.tilejson["roads"]
server.stop()
```

## Tile sizes

`manby stats` reports how big a tileset's tiles are at each zoom, as stored, and lists the largest. `--layers` also counts each layer's features at each zoom, from every tile, which shows where dropping and coalescing took effect.

```sh
manby stats --layers --largest=3 states.pmtiles
```

```
states.pmtiles: PMTiles of MVT tiles, 76 tiles, 32,803 bytes
z  tiles   total   mean    p50    p99    max
0      1   1,365  1,365  1,365  1,365  1,365
1      1   1,734  1,734  1,734  1,734  1,734
2      2   2,529  1,264  1,221  1,308  1,308
3      4   3,511    877    866  1,158  1,158
4      6   4,037    672    588  1,233  1,233
5     17   6,783    399    363  1,082  1,082
6     45  12,844    285    256    596    596

z  layer   tiles  features  points  lines  polygons
0  states      1       110       0    110         0
1  states      1       110       0    110         0
2  states      2       129       0    129         0
3  states      4       148       0    148         0
4  states      6       138       0    138         0
5  states     17       160       0    160         0
6  states     45       206       0    206         0

The 3 largest tiles:
z  x  y  bytes
1  0  0  1,734
0  0  0  1,365
2  1  1  1,308
```

It reads PMTiles archives, MBTiles databases and directories of tiles, an archive's sizes from its directories alone. `--json` writes the report as JSON, `--largest=N` lists N tiles, and a run of identical tiles counts as that many tiles, as a map fetches them.

From R, `tile_stats()` gives the report as data frames, and from Python `manby.stats()` as a dict.

```r
manby::tile_stats("roads.pmtiles", layers = TRUE)$layers
```

```python
manby.stats("roads.pmtiles", layers=True)["zooms"]
```
