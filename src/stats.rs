//! `manby stats`: how big a tileset's tiles are at each zoom, as stored, and
//! with `--layers` how many features of each layer the tiles at each zoom
//! hold, which shows where tippecanoe's dropping and coalescing took effect.
//! The sizes of a PMTiles archive come from its directories alone. A run of
//! identical tiles counts as that many tiles of the run's size, which is what
//! a map fetches rather than what is on disk.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Read;

use rayon::prelude::*;
use tippecanoe::errors::{EXIT_ARGS, EXIT_OPEN, EXIT_SUCCESS};

use crate::serve::tileset::{Kind, Listed, Opened, Tileset};
use crate::{Error, Result};

const USAGE: &str = "Usage: manby stats [--layers] [--largest=10] [--json] tileset ...
Reports the sizes of a PMTiles archive's, an MBTiles database's or a
directory's tiles at each zoom, as stored.
  --layers       also count the features of each layer at each zoom
  --largest=N    list the N largest tiles, 10 unless given
  --json         write the report as JSON";

#[derive(Clone, Debug, PartialEq)]
pub struct StatsOptions {
    pub layers: bool,
    pub largest: usize,
    pub json: bool,
    pub tilesets: Vec<String>,
}

pub fn parse_args(argv: &[String]) -> Result<StatsOptions> {
    let mut o = StatsOptions {
        layers: false,
        largest: 10,
        json: false,
        tilesets: Vec::new(),
    };
    let usage = |status: i32| Error::new(status, USAGE);
    let mut args = argv.iter().skip(1);
    while let Some(a) = args.next() {
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        match key {
            "-h" | "--help" => return Err(usage(EXIT_SUCCESS)),
            "--layers" => o.layers = true,
            "--json" => o.json = true,
            "--largest" => {
                o.largest = inline
                    .or_else(|| args.next().cloned())
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| usage(EXIT_ARGS))?
            }
            _ if key.starts_with('-') && key.len() > 1 => return Err(usage(EXIT_ARGS)),
            _ => o.tilesets.push(a.clone()),
        }
    }
    if o.tilesets.is_empty() {
        return Err(usage(EXIT_ARGS));
    }
    Ok(o)
}

/// The features of one layer in the tiles of one zoom.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerCount {
    /// Tiles with the layer.
    pub tiles: u64,
    pub features: u64,
    pub points: u64,
    pub lines: u64,
    pub polygons: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZoomStats {
    pub z: u8,
    pub tiles: u64,
    /// Bytes as stored, summed over the tiles.
    pub total: u64,
    pub mean: u64,
    /// Nearest-rank percentiles: the smallest size that this share of the
    /// tiles are no larger than.
    pub p50: u64,
    pub p99: u64,
    pub max: u64,
    pub layers: BTreeMap<String, LayerCount>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Largest {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub tileset: String,
    pub kind: &'static str,
    pub format: &'static str,
    pub zooms: Vec<ZoomStats>,
    pub largest: Vec<Largest>,
    pub layers: bool,
}

/// The report on one tileset.
pub fn report(path: &str, layers: bool, largest: usize) -> Result<Report> {
    let open = |e: String| Error::new(EXIT_OPEN, e);
    let t = Tileset::new(path, String::new()).map_err(open)?;
    let o = t.opened().map_err(open)?;
    let listed = o.list().map_err(|e| open(format!("{}: {}", path, e)))?;

    let mut by_zoom: BTreeMap<u8, Vec<&Listed>> = BTreeMap::new();
    for l in &listed {
        by_zoom.entry(l.z).or_default().push(l);
    }
    let counts = if layers {
        count_features(&o, &listed).map_err(|e| open(format!("{}: {}", path, e)))?
    } else {
        HashMap::new()
    };

    let mut zooms = Vec::new();
    for (z, tiles) in by_zoom {
        let mut sizes: Vec<(u64, u64)> = tiles.iter().map(|l| (l.len, l.count)).collect();
        sizes.sort_unstable();
        let n: u64 = sizes.iter().map(|s| s.1).sum();
        let total: u64 = sizes.iter().map(|s| s.0.saturating_mul(s.1)).sum();
        let mut zs = ZoomStats {
            z,
            tiles: n,
            total,
            mean: total / n.max(1),
            p50: percentile(&sizes, n, 50),
            p99: percentile(&sizes, n, 99),
            max: sizes.last().map(|s| s.0).unwrap_or(0),
            layers: BTreeMap::new(),
        };
        for l in &tiles {
            if let Some(per_layer) = counts.get(&key(l)) {
                for (name, c) in per_layer {
                    let into = zs.layers.entry(name.clone()).or_default();
                    into.tiles += l.count;
                    into.features += c.features * l.count;
                    into.points += c.points * l.count;
                    into.lines += c.lines * l.count;
                    into.polygons += c.polygons * l.count;
                }
            }
        }
        zooms.push(zs);
    }

    Ok(Report {
        tileset: path.to_string(),
        kind: match t.kind {
            Kind::Pmtiles => "PMTiles",
            Kind::Mbtiles => "MBTiles",
            Kind::Dir => "directory",
        },
        format: if o.is_mlt() { "MLT" } else { "MVT" },
        zooms,
        largest: largest_tiles(&listed, largest),
        layers,
    })
}

/// The smallest size that at least `p` percent of the `n` tiles are no
/// larger than, from sizes and counts sorted by size.
fn percentile(sizes: &[(u64, u64)], n: u64, p: u64) -> u64 {
    let rank = (n * p).div_ceil(100).max(1);
    let mut seen = 0;
    for &(size, count) in sizes {
        seen += count;
        if seen >= rank {
            return size;
        }
    }
    0
}

fn largest_tiles(listed: &[Listed], n: usize) -> Vec<Largest> {
    let mut order: Vec<&Listed> = listed.iter().collect();
    order.sort_by(|a, b| {
        b.len
            .cmp(&a.len)
            .then(a.z.cmp(&b.z))
            .then(a.x.cmp(&b.x))
            .then(a.y.cmp(&b.y))
    });
    let mut out = Vec::new();
    for l in order {
        for i in 0..l.count.min((n - out.len()) as u64) {
            let (z, x, y) = if i == 0 {
                (l.z, l.x, l.y)
            } else {
                let t = tippecanoe::pmtiles::tileid_to_zxy(l.id + i);
                (t.z as u8, t.x as u32, t.y as u32)
            };
            out.push(Largest {
                z,
                x,
                y,
                bytes: l.len,
            });
        }
        if out.len() >= n {
            break;
        }
    }
    out
}

/// Where a tile's data is, so tiles that share it are decoded once.
fn key(l: &Listed) -> Key {
    if l.offset > 0 {
        (l.offset, 0, 0, 0)
    } else {
        (0, l.z, l.x, l.y)
    }
}

type Counts = Vec<(String, LayerCount)>;

/// Where a tile's data is: see `key`.
type Key = (u64, u8, u32, u32);

fn count_features(
    o: &Opened,
    listed: &[Listed],
) -> std::result::Result<HashMap<Key, Counts>, String> {
    let mut unique: HashMap<Key, &Listed> = HashMap::new();
    for l in listed {
        unique.entry(key(l)).or_insert(l);
    }
    let mlt = o.is_mlt();
    unique
        .into_par_iter()
        .map(|(k, l)| {
            let data = gunzip(o.data(l)?)?;
            let counts = if mlt {
                mlt_counts(&data)?
            } else {
                mvt_counts(&data)?
            };
            Ok((k, counts))
        })
        .collect()
}

fn gunzip(data: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
    if !data.starts_with(&[0x1f, 0x8b]) {
        return Ok(data);
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&data[..])
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

fn mvt_counts(data: &[u8]) -> std::result::Result<Counts, String> {
    use tippecanoe::mvt::{MVT_LINESTRING, MVT_POINT, MVT_POLYGON, MvtTile};
    let mut tile = MvtTile::default();
    let mut compressed = false;
    match tile.try_decode(data, &mut compressed) {
        Ok(true) => {}
        Ok(false) => return Err("a tile is not a vector tile".to_string()),
        Err(e) => return Err(format!("a tile can't be decoded: {:?}", e)),
    }
    Ok(tile
        .layers
        .iter()
        .map(|layer| {
            let mut c = LayerCount {
                tiles: 1,
                features: layer.features.len() as u64,
                ..Default::default()
            };
            for f in &layer.features {
                match f.type_ {
                    MVT_POINT => c.points += 1,
                    MVT_LINESTRING => c.lines += 1,
                    MVT_POLYGON => c.polygons += 1,
                    _ => {}
                }
            }
            (layer.name.clone(), c)
        })
        .collect())
}

#[cfg(feature = "mlt")]
fn mlt_counts(data: &[u8]) -> std::result::Result<Counts, String> {
    use mlt_core::geo_types::Geometry;
    Ok(crate::mlt::decode_tile(data)?
        .iter()
        .map(|layer| {
            let mut c = LayerCount {
                tiles: 1,
                features: layer.features().len() as u64,
                ..Default::default()
            };
            for f in layer.features() {
                match f.geometry() {
                    Geometry::Point(_) | Geometry::MultiPoint(_) => c.points += 1,
                    Geometry::LineString(_) | Geometry::MultiLineString(_) | Geometry::Line(_) => {
                        c.lines += 1
                    }
                    Geometry::Polygon(_) | Geometry::MultiPolygon(_) => c.polygons += 1,
                    _ => {}
                }
            }
            (layer.name().to_string(), c)
        })
        .collect())
}

#[cfg(not(feature = "mlt"))]
fn mlt_counts(_: &[u8]) -> std::result::Result<Counts, String> {
    Err(crate::mlt::NOT_BUILT.to_string())
}

/// Prints the reports, as tables or as JSON.
pub fn stats(o: &StatsOptions) -> Result<()> {
    let reports: Vec<Report> = o
        .tilesets
        .iter()
        .map(|t| report(t, o.layers, o.largest))
        .collect::<Result<_>>()?;
    let text = if o.json {
        let items: Vec<String> = reports.iter().map(json).collect();
        if items.len() == 1 {
            items[0].clone() + "\n"
        } else {
            format!("[{}]\n", items.join(","))
        }
    } else {
        reports.iter().map(table).collect::<Vec<_>>().join("\n")
    };
    print!("{}", text);
    Ok(())
}

/// Numbers with thousands separators.
fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Columns aligned to their widest cell, to the right except those in `left`.
fn columns(rows: &[Vec<String>], left: &[usize]) -> String {
    let mut widths = vec![0; rows.first().map(|r| r.len()).unwrap_or(0)];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let mut out = String::new();
    for r in rows {
        let cells: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if left.contains(&i) {
                    format!("{:<w$}", c, w = widths[i])
                } else {
                    format!("{:>w$}", c, w = widths[i])
                }
            })
            .collect();
        out += cells.join("  ").trim_end();
        out.push('\n');
    }
    out
}

fn table(r: &Report) -> String {
    let tiles: u64 = r.zooms.iter().map(|z| z.tiles).sum();
    let bytes: u64 = r.zooms.iter().map(|z| z.total).sum();
    let mut out = format!(
        "{}: {} of {} tiles, {} tiles, {} bytes\n",
        r.tileset,
        r.kind,
        r.format,
        grouped(tiles),
        grouped(bytes)
    );
    if r.zooms.is_empty() {
        return out + "no tiles\n";
    }
    let mut rows = vec![
        ["z", "tiles", "total", "mean", "p50", "p99", "max"]
            .map(String::from)
            .to_vec(),
    ];
    for z in &r.zooms {
        rows.push(vec![
            z.z.to_string(),
            grouped(z.tiles),
            grouped(z.total),
            grouped(z.mean),
            grouped(z.p50),
            grouped(z.p99),
            grouped(z.max),
        ]);
    }
    out += &columns(&rows, &[]);
    if r.layers {
        let mut rows = vec![
            [
                "z", "layer", "tiles", "features", "points", "lines", "polygons",
            ]
            .map(String::from)
            .to_vec(),
        ];
        for z in &r.zooms {
            for (name, c) in &z.layers {
                rows.push(vec![
                    z.z.to_string(),
                    name.clone(),
                    grouped(c.tiles),
                    grouped(c.features),
                    grouped(c.points),
                    grouped(c.lines),
                    grouped(c.polygons),
                ]);
            }
        }
        out.push('\n');
        out += &columns(&rows, &[1]);
    }
    if !r.largest.is_empty() {
        let _ = writeln!(out, "\nThe {} largest tiles:", r.largest.len());
        let mut rows = vec![["z", "x", "y", "bytes"].map(String::from).to_vec()];
        for t in &r.largest {
            rows.push(vec![
                t.z.to_string(),
                t.x.to_string(),
                t.y.to_string(),
                grouped(t.bytes),
            ]);
        }
        out += &columns(&rows, &[]);
    }
    out
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json(r: &Report) -> String {
    let zooms: Vec<String> = r
        .zooms
        .iter()
        .map(|z| {
            let mut s = format!(
                "{{\"z\":{},\"tiles\":{},\"total_bytes\":{},\"mean_bytes\":{},\"p50_bytes\":{},\"p99_bytes\":{},\"max_bytes\":{}",
                z.z, z.tiles, z.total, z.mean, z.p50, z.p99, z.max
            );
            if r.layers {
                let layers: Vec<String> = z
                    .layers
                    .iter()
                    .map(|(name, c)| {
                        format!(
                            "{}:{{\"tiles\":{},\"features\":{},\"points\":{},\"lines\":{},\"polygons\":{}}}",
                            json_string(name),
                            c.tiles,
                            c.features,
                            c.points,
                            c.lines,
                            c.polygons
                        )
                    })
                    .collect();
                let _ = write!(s, ",\"layers\":{{{}}}", layers.join(","));
            }
            s + "}"
        })
        .collect();
    let largest: Vec<String> = r
        .largest
        .iter()
        .map(|t| {
            format!(
                "{{\"z\":{},\"x\":{},\"y\":{},\"bytes\":{}}}",
                t.z, t.x, t.y, t.bytes
            )
        })
        .collect();
    format!(
        "{{\"tileset\":{},\"kind\":\"{}\",\"format\":\"{}\",\"zooms\":[{}],\"largest\":[{}]}}",
        json_string(&r.tileset),
        r.kind,
        r.format,
        zooms.join(","),
        largest.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        // 1 tile of 10 bytes, 98 of 20, 1 of 1,000
        let sizes = [(10, 1), (20, 98), (1000, 1)];
        assert_eq!(percentile(&sizes, 100, 50), 20);
        assert_eq!(percentile(&sizes, 100, 99), 20);
        assert_eq!(percentile(&[(5, 1)], 1, 99), 5);
        assert_eq!(percentile(&sizes, 100, 100), 1000);
    }

    #[test]
    fn numbers() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1234567), "1,234,567");
    }
}
