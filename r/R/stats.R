#' The sizes of a tileset's tiles
#'
#' How big a PMTiles archive's, an MBTiles database's or a directory's tiles
#' are at each zoom, as stored, and the largest tiles. With `layers`, also
#' how many features of each layer the tiles at each zoom hold, which shows
#' where dropping and coalescing took effect.
#'
#' @param tileset The path of the tileset.
#' @param layers Count the features of each layer, from every tile.
#' @param largest How many of the largest tiles to list.
#' @return A list of data frames: `zooms`, with the tiles at each zoom and
#'   their total, mean, median, 99th percentile and largest size in bytes;
#'   `largest`, the largest tiles; and with `layers`, `layers`, the tiles and
#'   features of each layer at each zoom.
#' @examples
#' \dontrun{
#' tile_stats("roads.pmtiles", layers = TRUE)
#' }
#' @export
tile_stats <- function(tileset, layers = FALSE, largest = 10) {
  args <- c(
    "stats", "--json", paste0("--largest=", format(largest, scientific = FALSE)),
    if (isTRUE(layers)) "--layers", path.expand(tileset)
  )
  done <- processx::run(manby_binary(), args, error_on_status = FALSE)
  if (done$status != 0) {
    stop(manby_error(done$status, args, done$stderr))
  }
  r <- jsonlite::fromJSON(done$stdout, simplifyVector = FALSE)
  rows <- function(items, fields) {
    as.data.frame(
      stats::setNames(lapply(fields, function(f) vapply(items, function(i) as.numeric(i[[f]]), 0)), fields)
    )
  }
  out <- list(
    zooms = rows(r$zooms, c("z", "tiles", "total_bytes", "mean_bytes", "p50_bytes", "p99_bytes", "max_bytes")),
    largest = rows(r$largest, c("z", "x", "y", "bytes"))
  )
  if (isTRUE(layers)) {
    per <- unlist(lapply(r$zooms, function(z) {
      lapply(names(z$layers), function(name) c(list(z = z$z, layer = name), z$layers[[name]]))
    }), recursive = FALSE)
    out$layers <- data.frame(
      z = vapply(per, function(l) as.numeric(l$z), 0),
      layer = vapply(per, function(l) l$layer, ""),
      rows(per, c("tiles", "features", "points", "lines", "polygons"))
    )
  }
  out
}
