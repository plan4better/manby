The changes in each version of manby, from the titles of the pull requests that made them.

## [0.1.9](https://github.com/birkskyum/manby/compare/v0.1.8...v0.1.9) - 2026-11-08

### Added

- Count an attribute's values in clusters with --cluster-categories ([#25](https://github.com/birkskyum/manby/pull/25))

## [0.1.8](https://github.com/birkskyum/manby/compare/v0.1.7...v0.1.8) - 2026-11-08

### Added

- Bin points into H3 hexagons below a base zoom with --h3 ([#24](https://github.com/birkskyum/manby/pull/24))
- Read coordinates in any EPSG or ESRI CRS, from -s or GeoParquet metadata ([#22](https://github.com/birkskyum/manby/pull/22))

## [0.1.7](https://github.com/birkskyum/manby/compare/v0.1.6...v0.1.7) - 2026-11-08

### Added

- Read GeoParquet from object storage and the web, and pick rows with --bbox ([#21](https://github.com/birkskyum/manby/pull/21))
- Report a tileset's tile sizes and features at each zoom with manby stats ([#19](https://github.com/birkskyum/manby/pull/19))

## [0.1.6](https://github.com/birkskyum/manby/compare/v0.1.5...v0.1.6) - 2026-11-07

### Added

- Serve tilesets with a map of them, from the command line, R and Python ([#18](https://github.com/birkskyum/manby/pull/18))

## [0.1.5](https://github.com/birkskyum/manby/compare/v0.1.4...v0.1.5) - 2026-11-07

### Added

- Tile sf data frames and DuckDB tables from R ([#15](https://github.com/birkskyum/manby/pull/15))
- Read GeoParquet and Arrow, and tile DuckDB queries and data frames from Python ([#13](https://github.com/birkskyum/manby/pull/13))

### Fixed

- Read GeoJSON with a third coordinate in linear time ([#11](https://github.com/birkskyum/manby/pull/11))

### Dependencies

- Bump rusqlite from 0.40.2 to 0.41.1 ([#6](https://github.com/birkskyum/manby/pull/6))
- Bump the cargo group with 2 updates ([#5](https://github.com/birkskyum/manby/pull/5))
- Bump tikv-jemallocator from 0.6.1 to 0.7.0 ([#7](https://github.com/birkskyum/manby/pull/7))

## [0.1.4](https://github.com/birkskyum/manby/compare/v0.1.3...v0.1.4) - 2026-11-07

### Added

- `pip install manby` and `uvx manby`, from wheels on PyPI for macOS and Linux
- Binaries for macOS and Linux on the GitHub releases, which write MapLibre Tiles too

## [0.1.3](https://github.com/birkskyum/manby/compare/v0.1.2...v0.1.3) - 2026-11-07

### Fixed

- `--no-simplification-of-shared-nodes` in a third of the memory, and no panic on lines that collapse to one point ([#3](https://github.com/birkskyum/manby/pull/3))

## [0.1.2](https://github.com/birkskyum/manby/compare/v0.1.1...v0.1.2) - 2026-11-06

### Added

- `--as-needed-per-tile`, which fits each tile on its own rather than its whole zoom level

## [0.1.1](https://github.com/birkskyum/manby/compare/v0.1.0...v0.1.1) - 2026-11-06

### Faster or smaller

- The union of a simple polygon ring without wagyu's sweep, 5 to 9% less CPU on the census tracts

## [0.1.0](https://github.com/birkskyum/manby/releases/tag/v0.1.0) - 2026-11-05

The first release.
