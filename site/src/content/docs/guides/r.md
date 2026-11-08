---
title: From R
description: Tiling sf data frames, DuckDB tables and Arrow tables from R, with tippecanoe's tiles.
---

The manby R package tiles sf data frames, DuckDB tables, Arrow tables and files. An sf data frame makes the tiles that tippecanoe makes from GDAL's FlatGeobuf of it. The package runs on macOS and Linux.

```r
install.packages(
  "manby",
  repos = c("https://birkskyum.r-universe.dev", "https://cloud.r-project.org")
)
```

It runs the manby program, which it downloads once from the GitHub release of its own version and keeps in its cache. To run a manby installed another way, set the `MANBY_BINARY` environment variable to its path.

## Tiling

`tile()` takes the input, the tileset to write, and tippecanoe's options.

```r
library(manby)

nc <- sf::st_read(system.file("shape/nc.shp", package = "sf"))
tile(nc, "nc.pmtiles", layer = "counties", maximum_zoom = "g", coalesce_densest_as_needed = TRUE)
```

The options are tippecanoe's long options, with `_` for `-`, `TRUE` for a flag and a vector to repeat one. Unnamed arguments are passed as they are, such as `"-zg"`. An existing tileset is overwritten, unless `force = FALSE`. If manby fails, the error has the class `manby_error` and tippecanoe's exit status in `status`.

An sf data frame in a CRS other than WGS 84 or Web Mercator is reprojected to WGS 84 first. Factors become text and quantities with units become numbers, as GDAL writes them.

### DuckDB

A DuckDB table from dplyr goes in as it is. DuckDB writes it to a temporary Parquet file, so it need not fit in memory.

```r
con <- DBI::dbConnect(duckdb::duckdb())
DBI::dbExecute(con, "LOAD spatial")
roads <- dplyr::tbl(con, dplyr::sql("
  SELECT name, class, geometry
  FROM 'roads.parquet'
  WHERE class IN ('motorway', 'trunk', 'primary')
"))
tile(roads, "roads.pmtiles", layer = "roads", maximum_zoom = 14)
```

### Other tables

Arrow tables and streams, nanoarrow streams and data frames go in the same way, with the geometry in a GeoArrow column or in a column named `geometry` in well-known binary. They are written to a temporary Arrow file for manby to read.

### Layers

A named list makes a layer of each table or file.

```r
tile(list(roads = roads, places = "places.parquet"), "map.pmtiles", maximum_zoom = 14)
```

### Viewing

`serve()` serves tilesets with a map of them, in the viewer pane, and `tile_stats()` reports their tiles' sizes and features. [Looking at tilesets](../serve/) has more.

```r
server <- serve("roads.pmtiles")
server$stop()
```

### Other commands

`manby()` runs manby with arguments as on the command line, such as `manby join`.

```r
manby("join", "-o", "map.pmtiles", "roads.pmtiles", "places.pmtiles")
```
