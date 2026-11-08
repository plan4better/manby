//! The tilesets that `manby serve` reads: PMTiles archives, MBTiles databases
//! and directories of tiles. A tileset is opened again when its file changes,
//! so one made again while it is served is served as it is now.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use tippecanoe::jsonpull::{JsonPull, JsonRef, JsonType};
use tippecanoe::pmtiles::{self, EntryV3, HeaderV3};

/// PMTiles' tile type for MapLibre Tiles.
const TILETYPE_MLT: u8 = 0x6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Pmtiles,
    Mbtiles,
    Dir,
}

/// A version of a file: its size, modification time and inode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ino: u64,
}

impl Stamp {
    pub fn of(path: &Path) -> std::io::Result<Stamp> {
        let m = std::fs::metadata(path)?;
        Ok(Stamp {
            len: m.len(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
            ino: m.ino(),
        })
    }

    /// The version as text, for ETags and URLs.
    pub fn tag(&self) -> String {
        format!(
            "{:x}-{:x}.{:x}-{:x}",
            self.len, self.mtime, self.mtime_nsec, self.ino
        )
    }
}

/// What a tileset says about itself, for its TileJSON.
#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub name: Option<String>,
    pub description: Option<String>,
    pub attribution: Option<String>,
    pub minzoom: u8,
    pub maxzoom: u8,
    pub bounds: Option<[f64; 4]>,
    pub center: Option<[f64; 3]>,
    /// The `vector_layers` array, as JSON.
    pub vector_layers: String,
    pub mlt: bool,
}

pub struct Tileset {
    /// The name in URLs.
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
    open: Mutex<Option<Arc<Opened>>>,
}

/// A tileset as it was when it was opened.
pub struct Opened {
    pub stamp: Stamp,
    pub meta: Meta,
    reader: Reader,
}

enum Reader {
    Pmtiles {
        file: File,
        header: HeaderV3,
        root: Arc<Vec<EntryV3>>,
        /// Leaf directories read so far, by offset.
        leaves: Mutex<HashMap<u64, Arc<Vec<EntryV3>>>>,
    },
    Mbtiles(Mutex<Connection>),
    Dir(PathBuf),
}

impl Tileset {
    pub fn new(path: &str, name: String) -> Result<Tileset, String> {
        let path = PathBuf::from(path);
        let m = std::fs::metadata(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
        let kind = if m.is_dir() {
            Kind::Dir
        } else {
            let mut magic = [0u8; 7];
            let pmtiles = File::open(&path)
                .and_then(|f| f.read_exact_at(&mut magic, 0))
                .is_ok()
                && &magic == b"PMTiles";
            if pmtiles {
                Kind::Pmtiles
            } else if path.extension().is_some_and(|e| e == "mbtiles") {
                Kind::Mbtiles
            } else {
                return Err(format!(
                    "{}: not a PMTiles archive, an MBTiles database or a directory of tiles",
                    path.display()
                ));
            }
        };
        Ok(Tileset {
            name,
            path,
            kind,
            open: Mutex::new(None),
        })
    }

    /// The file whose changes mean the tileset changed.
    fn watched(&self) -> PathBuf {
        match self.kind {
            Kind::Dir => self.path.join("metadata.json"),
            _ => self.path.clone(),
        }
    }

    /// The tileset as it is now, opened again if it changed.
    pub fn opened(&self) -> Result<Arc<Opened>, String> {
        let watched = self.watched();
        let stamp = Stamp::of(&watched)
            .or_else(|e| match self.kind {
                // a directory without metadata.json
                Kind::Dir => Stamp::of(&self.path),
                _ => Err(e),
            })
            .map_err(|e| format!("{}: {}", self.path.display(), e))?;
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(o) = open.as_ref()
            && o.stamp == stamp
        {
            return Ok(o.clone());
        }
        let o = Arc::new(
            Opened::open(self.kind, &self.path, stamp)
                .map_err(|e| format!("{}: {}", self.path.display(), e))?,
        );
        *open = Some(o.clone());
        Ok(o)
    }
}

impl Opened {
    fn open(kind: Kind, path: &Path, stamp: Stamp) -> Result<Opened, String> {
        match kind {
            Kind::Pmtiles => {
                let file = File::open(path).map_err(|e| e.to_string())?;
                let h = read(&file, 0, 127)?;
                if &h[..7] != b"PMTiles" || h[7] != 3 {
                    return Err("not a version 3 PMTiles archive".to_string());
                }
                let header = pmtiles::deserialize_header(&h);
                let c = header.internal_compression;
                let root = Arc::new(pmtiles::deserialize_directory(&decompress(
                    &read(&file, header.root_dir_offset, header.root_dir_bytes)?,
                    c,
                )?));
                let json = decompress(
                    &read(
                        &file,
                        header.json_metadata_offset,
                        header.json_metadata_bytes,
                    )?,
                    c,
                )?;
                let meta = pmtiles_meta(&header, &json);
                Ok(Opened {
                    stamp,
                    meta,
                    reader: Reader::Pmtiles {
                        file,
                        header,
                        root,
                        leaves: Mutex::new(HashMap::new()),
                    },
                })
            }
            Kind::Mbtiles => {
                let db = Connection::open_with_flags(
                    path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(|e| e.to_string())?;
                let mut pairs = HashMap::new();
                {
                    let mut q = db
                        .prepare("SELECT name, value FROM metadata")
                        .map_err(|e| e.to_string())?;
                    let rows = q
                        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                        .map_err(|e| e.to_string())?;
                    for (k, v) in rows.flatten() {
                        pairs.insert(k, v);
                    }
                }
                Ok(Opened {
                    stamp,
                    meta: pairs_meta(&pairs),
                    reader: Reader::Mbtiles(Mutex::new(db)),
                })
            }
            Kind::Dir => {
                let mut pairs = HashMap::new();
                if let Ok(text) = std::fs::read(path.join("metadata.json"))
                    && let Some(tree) = JsonPull::begin_string(&text).read_tree()
                    && tree.root().type_() == JsonType::Hash
                {
                    for (k, v) in tree.root().entries() {
                        let value = match v.type_() {
                            JsonType::String => v.string().to_string(),
                            _ => v.stringify(),
                        };
                        pairs.insert(k.string().to_string(), value);
                    }
                }
                Ok(Opened {
                    stamp,
                    meta: pairs_meta(&pairs),
                    reader: Reader::Dir(path.to_path_buf()),
                })
            }
        }
    }

    /// The tile's data as stored, often gzipped, or None if there is none.
    pub fn tile(&self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>, String> {
        if z > 31 || x >= 1u32 << z || y >= 1u32 << z {
            return Ok(None);
        }
        match &self.reader {
            Reader::Pmtiles {
                file,
                header,
                root,
                leaves,
            } => {
                let id = pmtiles::zxy_to_tileid(z, x, y);
                let mut dir = root.clone();
                for _ in 0..4 {
                    let Some(e) = find(&dir, id) else {
                        return Ok(None);
                    };
                    if e.run_length > 0 {
                        return read(file, header.tile_data_offset + e.offset, e.length as u64)
                            .map(Some);
                    }
                    let offset = header.leaf_dirs_offset + e.offset;
                    let cached = leaves
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&offset)
                        .cloned();
                    dir = match cached {
                        Some(d) => d,
                        None => {
                            let d = Arc::new(pmtiles::deserialize_directory(&decompress(
                                &read(file, offset, e.length as u64)?,
                                header.internal_compression,
                            )?));
                            let mut l = leaves.lock().unwrap_or_else(|e| e.into_inner());
                            if l.len() >= 256 {
                                l.clear();
                            }
                            l.insert(offset, d.clone());
                            d
                        }
                    };
                }
                Ok(None)
            }
            Reader::Mbtiles(db) => {
                let db = db.lock().unwrap_or_else(|e| e.into_inner());
                // TMS rows count up from the south
                let row = (1u64 << z) - 1 - y as u64;
                db.query_row(
                    "SELECT tile_data FROM tiles WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3",
                    (z as i64, x as i64, row as i64),
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(|e| e.to_string())
            }
            Reader::Dir(dir) => {
                for ext in ["pbf", "mvt", "mlt"] {
                    match std::fs::read(dir.join(format!("{}/{}/{}.{}", z, x, y, ext))) {
                        Ok(data) => return Ok(Some(data)),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Ok(None)
            }
        }
    }
}

/// A tile, or a run of identical tiles in a PMTiles archive, which the
/// first tile names.
#[derive(Clone, Debug)]
pub struct Listed {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    /// The tiles in the run, all at zoom `z`.
    pub count: u64,
    /// The size of the tile's data as stored.
    pub len: u64,
    /// The PMTiles tile id of the first tile, and where its data is.
    pub id: u64,
    pub offset: u64,
}

impl Opened {
    pub fn is_mlt(&self) -> bool {
        self.meta.mlt
    }

    /// Every tile, from the directories of a PMTiles archive without reading
    /// the tiles, with runs split where the zoom changes.
    pub fn list(&self) -> Result<Vec<Listed>, String> {
        let mut out = Vec::new();
        match &self.reader {
            Reader::Pmtiles {
                file, header, root, ..
            } => {
                let mut dirs = vec![(root.clone(), 0usize)];
                while let Some((dir, depth)) = dirs.pop() {
                    for e in dir.iter().rev() {
                        if e.run_length == 0 {
                            if depth >= 3 {
                                return Err("leaf directories nested too deep".to_string());
                            }
                            let d = pmtiles::deserialize_directory(&decompress(
                                &read(file, header.leaf_dirs_offset + e.offset, e.length as u64)?,
                                header.internal_compression,
                            )?);
                            dirs.push((Arc::new(d), depth + 1));
                            continue;
                        }
                        let mut id = e.tile_id;
                        let end = e.tile_id + e.run_length as u64;
                        while id < end {
                            let first = pmtiles::tileid_to_zxy(id);
                            let next_zoom = (4u64.pow(first.z as u32 + 1) - 1) / 3;
                            let stop = end.min(next_zoom);
                            out.push(Listed {
                                z: first.z as u8,
                                x: first.x as u32,
                                y: first.y as u32,
                                count: stop - id,
                                len: e.length as u64,
                                id,
                                offset: header.tile_data_offset + e.offset,
                            });
                            id = stop;
                        }
                    }
                }
            }
            Reader::Mbtiles(db) => {
                let db = db.lock().unwrap_or_else(|e| e.into_inner());
                let mut q = db
                    .prepare(
                        "SELECT zoom_level, tile_column, tile_row, length(tile_data) FROM tiles",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = q
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    })
                    .map_err(|e| e.to_string())?;
                for row in rows {
                    let (z, x, row, len) = row.map_err(|e| e.to_string())?;
                    if !(0..=31).contains(&z) {
                        continue;
                    }
                    out.push(Listed {
                        z: z as u8,
                        x: x as u32,
                        y: ((1i64 << z) - 1 - row) as u32,
                        count: 1,
                        len: len as u64,
                        id: 0,
                        offset: 0,
                    });
                }
            }
            Reader::Dir(dir) => {
                let number = |p: &Path| -> Option<u32> { p.file_stem()?.to_str()?.parse().ok() };
                for z in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
                    let Some(zn) = number(&z.path()).filter(|&n| n <= 31) else {
                        continue;
                    };
                    let Ok(xs) = std::fs::read_dir(z.path()) else {
                        continue;
                    };
                    for x in xs.flatten() {
                        let Some(xn) = number(&x.path()) else {
                            continue;
                        };
                        let Ok(ys) = std::fs::read_dir(x.path()) else {
                            continue;
                        };
                        for y in ys.flatten() {
                            let p = y.path();
                            let tile = p
                                .extension()
                                .is_some_and(|e| e == "pbf" || e == "mvt" || e == "mlt");
                            if let (true, Some(yn), Ok(m)) = (tile, number(&p), y.metadata()) {
                                out.push(Listed {
                                    z: zn as u8,
                                    x: xn,
                                    y: yn,
                                    count: 1,
                                    len: m.len(),
                                    id: 0,
                                    offset: 0,
                                });
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// The data of a listed tile, as stored.
    pub fn data(&self, t: &Listed) -> Result<Vec<u8>, String> {
        match &self.reader {
            Reader::Pmtiles { file, .. } => read(file, t.offset, t.len),
            _ => self
                .tile(t.z, t.x, t.y)?
                .ok_or_else(|| format!("tile {}/{}/{} is gone", t.z, t.x, t.y)),
        }
    }
}

/// The entry for a tile, or for the leaf directory that has it.
fn find(entries: &[EntryV3], id: u64) -> Option<EntryV3> {
    let i = entries.partition_point(|e| e.tile_id <= id);
    let e = *entries.get(i.checked_sub(1)?)?;
    if e.length == 0 {
        return None;
    }
    if e.run_length == 0 || id - e.tile_id < e.run_length as u64 {
        Some(e)
    } else {
        None
    }
}

fn read(file: &File, offset: u64, len: u64) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; len as usize];
    file.read_exact_at(&mut buf, offset)
        .map_err(|e| format!("reading {} bytes at {}: {}", len, offset, e))?;
    Ok(buf)
}

fn decompress(data: &[u8], compression: u8) -> Result<Vec<u8>, String> {
    match compression {
        pmtiles::COMPRESSION_NONE | pmtiles::COMPRESSION_UNKNOWN => Ok(data.to_vec()),
        pmtiles::COMPRESSION_GZIP => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(data)
                .read_to_end(&mut out)
                .map_err(|e| e.to_string())?;
            Ok(out)
        }
        c => Err(format!("compression {} is not supported", c)),
    }
}

fn string(j: Option<JsonRef<'_>>) -> Option<String> {
    j.filter(|j| j.type_() == JsonType::String)
        .map(|j| j.string().to_string())
}

fn vector_layers(json: &[u8]) -> String {
    JsonPull::begin_string(json)
        .read_tree()
        .and_then(|t| {
            t.root()
                .hash_get("vector_layers")
                .filter(|v| v.type_() == JsonType::Array)
                .map(|v| v.stringify())
        })
        .unwrap_or_else(|| "[]".to_string())
}

fn pmtiles_meta(h: &HeaderV3, json: &[u8]) -> Meta {
    let tree = JsonPull::begin_string(json).read_tree();
    let root = tree.as_ref().map(|t| t.root());
    let get = |k: &str| root.and_then(|r| r.hash_get(k));
    let e7 = |v: i32| v as f64 / 1e7;
    Meta {
        name: string(get("name")),
        description: string(get("description")),
        attribution: string(get("attribution")),
        minzoom: h.min_zoom,
        maxzoom: h.max_zoom,
        bounds: (h.min_lon_e7 < h.max_lon_e7 && h.min_lat_e7 < h.max_lat_e7).then(|| {
            [
                e7(h.min_lon_e7),
                e7(h.min_lat_e7),
                e7(h.max_lon_e7),
                e7(h.max_lat_e7),
            ]
        }),
        center: Some([
            e7(h.center_lon_e7),
            e7(h.center_lat_e7),
            h.center_zoom as f64,
        ]),
        vector_layers: vector_layers(json),
        mlt: h.tile_type == TILETYPE_MLT,
    }
}

/// The metadata of an MBTiles database or of a directory's metadata.json.
fn pairs_meta(pairs: &HashMap<String, String>) -> Meta {
    let numbers = |k: &str| -> Option<Vec<f64>> {
        pairs.get(k).and_then(|v| {
            v.split(',')
                .map(|n| n.trim().parse::<f64>().ok())
                .collect::<Option<Vec<f64>>>()
        })
    };
    let zoom = |k: &str, default: u8| {
        pairs
            .get(k)
            .and_then(|v| v.trim().parse::<u8>().ok())
            .unwrap_or(default)
    };
    Meta {
        name: pairs.get("name").cloned(),
        description: pairs.get("description").cloned(),
        attribution: pairs.get("attribution").cloned(),
        minzoom: zoom("minzoom", 0),
        maxzoom: zoom("maxzoom", 14),
        bounds: numbers("bounds")
            .filter(|b| b.len() == 4 && b[0] < b[2] && b[1] < b[3])
            .map(|b| [b[0], b[1], b[2], b[3]]),
        center: numbers("center")
            .filter(|c| c.len() == 3)
            .map(|c| [c[0], c[1], c[2]]),
        vector_layers: pairs
            .get("json")
            .map(|j| vector_layers(j.as_bytes()))
            .unwrap_or_else(|| "[]".to_string()),
        mlt: pairs.get("format").is_some_and(|f| f == "mlt"),
    }
}
