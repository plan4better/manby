//! What one thread reads from one range of the input: features, with their
//! keys, values and layers interned in tables of the chunk's own, which are
//! merged into the store afterwards.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use tippecanoe::geometry::{Draw, VT_MOVETO};
use tippecanoe::mbtiles::{LayermapEntry, Tilestat};
use tippecanoe::mvt::{MVT_DOUBLE, MVT_NULL};
use tippecanoe::serial::{SerialVal, Vertex};

use crate::buf::Buf;
use crate::store::{Attr, Feature};

/// Number of shards of the global value table, which are built in parallel.
pub const SHARDS: usize = 64;

pub fn hash_bytes(type_: i32, s: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64_with_seed(s, type_ as u64)
}

/// The vertices of `--no-simplification-of-shared-nodes`, tippecanoe's vertex
/// file. A `Vertex` takes 72 bytes, one for every vertex of every line and
/// polygon. Where all three of its points are on the world grid (x and y in
/// 0..2^32), it is kept as three `u64`s instead, mid, p1 and p2, each with y in
/// the high half: they sort as `Vertex` does, by mid, p1 and p2, each by y and
/// then x, so the nodes found in the sorted vertices are the same.
#[derive(Default)]
pub struct SharedVertices {
    pub packed: Vec<[u64; 3]>,
    /// Vertices with a point off the grid.
    pub wide: Vec<Vertex>,
}

impl SharedVertices {
    pub fn push(&mut self, v: Vertex) {
        match (pack(v.mid), pack(v.p1), pack(v.p2)) {
            (Some(mid), Some(p1), Some(p2)) => self.packed.push([mid, p1, p2]),
            _ => self.wide.push(v),
        }
    }
}

/// A point on the world grid as a `u64` that sorts as the point does.
pub fn pack(d: Draw) -> Option<u64> {
    let grid = 0..1i64 << 32;
    if grid.contains(&d.x) && grid.contains(&d.y) {
        Some(((d.y as u64) << 32) | d.x as u64)
    } else {
        None
    }
}

pub fn unpack(p: u64) -> Draw {
    Draw::new(VT_MOVETO, (p & 0xFFFF_FFFF) as i64, (p >> 32) as i64)
}

/// A packed vertex as a `Vertex` again.
pub fn unpack_vertex(&[mid, p1, p2]: &[u64; 3]) -> Vertex {
    Vertex {
        p1: unpack(p1),
        mid: unpack(mid),
        p2: unpack(p2),
    }
}

/// An interned string or typed value: its type and the bytes of its string.
#[derive(Default)]
pub struct Interner {
    pub types: Buf<i8>,
    pub hashes: Buf<u64>,
    /// End offsets in `bytes`; entry `i` is `bytes[ends[i-1]..ends[i]]`.
    pub ends: Buf<usize>,
    pub bytes: Buf<u8>,
    table: Buf<u32>,
}

const EMPTY: u32 = u32::MAX;

impl Interner {
    pub fn len(&self) -> usize {
        self.types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    pub fn get(&self, i: u32) -> (i32, &[u8]) {
        let i = i as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        (self.types[i] as i32, &self.bytes[start..self.ends[i]])
    }

    pub fn get_str(&self, i: u32) -> &str {
        // SAFETY: only valid UTF-8 is interned.
        unsafe { std::str::from_utf8_unchecked(self.get(i).1) }
    }

    fn grow(&mut self) {
        let n = if self.table.is_empty() {
            1024
        } else {
            self.table.len() * 2
        };
        self.table = Buf::new();
        self.table.resize(n, EMPTY);
        let mask = n - 1;
        for (i, &h) in self.hashes.iter().enumerate() {
            let mut slot = (h as usize) & mask;
            while self.table[slot] != EMPTY {
                slot = (slot + 1) & mask;
            }
            self.table[slot] = i as u32;
        }
    }

    /// The id of `s` with `type_`, adding it if it is new. `s` must be UTF-8.
    pub fn intern(&mut self, type_: i32, s: &[u8]) -> u32 {
        if self.hashes.len() * 2 >= self.table.len() {
            self.grow();
        }
        let h = hash_bytes(type_, s);
        let mask = self.table.len() - 1;
        let mut slot = (h as usize) & mask;
        loop {
            let e = self.table[slot];
            if e == EMPTY {
                break;
            }
            if self.hashes[e as usize] == h {
                let (t, b) = self.get(e);
                if t == type_ && b == s {
                    return e;
                }
            }
            slot = (slot + 1) & mask;
        }
        let id = self.types.len() as u32;
        self.table[slot] = id;
        self.types.push(type_ as i8);
        self.hashes.push(h);
        self.bytes.extend_from_slice(s);
        self.ends.push(self.bytes.len());
        id
    }

    /// Frees the hash table, which only interning needs, and the room that
    /// growing left, and settles the rest where the memory budget says. The
    /// hashes stay, for merging.
    pub fn finish(&mut self) {
        self.table = Buf::new();
        self.types.settle();
        self.hashes.settle();
        self.ends.settle();
        self.bytes.settle();
    }

    /// Frees what only interning needs: the hash table and the hashes.
    pub fn stop_interning(&mut self) {
        self.table = Buf::new();
        self.hashes = Buf::new();
    }

    /// The entries grouped by the shard they hash into, in order within
    /// each shard, and where each shard's start in them.
    pub fn by_shard(&self) -> (Buf<u32>, [usize; SHARDS + 1]) {
        let shard = |h: u64| (h >> 58) as usize % SHARDS;
        let mut starts = [0usize; SHARDS + 1];
        for &h in self.hashes.iter() {
            starts[shard(h) + 1] += 1;
        }
        for s in 0..SHARDS {
            starts[s + 1] += starts[s];
        }
        let mut at = starts;
        let mut ids = Buf::with_capacity(self.len());
        ids.resize(self.len(), 0u32);
        for (i, &h) in self.hashes.iter().enumerate() {
            let s = shard(h);
            ids[at[s]] = i as u32;
            at[s] += 1;
        }
        (ids, starts)
    }
}

/// tippecanoe's `add_to_tilestats`, with the sample values kept in a set
/// capped at `max_samples`: it keeps the same smallest distinct values.
#[derive(Clone, Debug)]
pub struct Stat {
    pub samples: BTreeSet<SerialValKey>,
    pub min: f64,
    pub max: f64,
    pub type_: i32,
}

impl Default for Stat {
    fn default() -> Self {
        Stat {
            samples: BTreeSet::new(),
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            type_: 0,
        }
    }
}

/// A `SerialVal` ordered as `serial_val::operator<` orders it: by string, then type.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SerialValKey {
    pub s: String,
    pub type_: i32,
}

/// libc `atof` on a value tippecanoe stringified: Rust's parse where it
/// accepts the text, which gives the same correctly rounded result.
pub fn atof_fast(s: &str) -> f64 {
    s.parse::<f64>()
        .unwrap_or_else(|_| tippecanoe::cstd::atof(s))
}

impl Stat {
    pub fn add(&mut self, type_: i32, s: &str, max_samples: usize) {
        if type_ == MVT_NULL {
            return;
        }

        if type_ == MVT_DOUBLE {
            let d = atof_fast(s);
            if d < self.min {
                self.min = d;
            }
            if d > self.max {
                self.max = d;
            }
        }

        let full = self.samples.len() >= max_samples;
        let beyond_last = match self.samples.last() {
            Some(last) => (s, type_) > (last.s.as_str(), last.type_),
            None => true,
        };
        if !(full && beyond_last) {
            let key = SerialValKey {
                s: s.to_string(),
                type_,
            };
            if self.samples.insert(key) && self.samples.len() > max_samples {
                self.samples.pop_last();
            }
        }

        self.type_ |= 1 << type_;
    }

    /// Merges another chunk's statistics into these, as merge_layermaps does.
    pub fn merge(&mut self, other: Stat, max_samples: usize) {
        for v in other.samples {
            if self.samples.insert(v) && self.samples.len() > max_samples {
                self.samples.pop_last();
            }
        }
        self.type_ |= other.type_;
        if other.min < self.min {
            self.min = other.min;
        }
        if other.max > self.max {
            self.max = other.max;
        }
    }

    pub fn to_tilestat(&self) -> Tilestat {
        Tilestat {
            sample_values: self
                .samples
                .iter()
                .map(|k| SerialVal {
                    type_: k.type_,
                    s: k.s.clone(),
                })
                .collect(),
            min: self.min,
            max: self.max,
            type_: self.type_,
        }
    }
}

/// A layer as one chunk sees it.
#[derive(Clone, Debug, Default)]
pub struct ChunkLayer {
    pub name: String,
    pub points: usize,
    pub lines: usize,
    pub polygons: usize,
    pub stats: BTreeMap<String, Stat>,
}

/// A message to print once the chunks are in order. Each part is a line of
/// the message; a part with a line number (relative to the chunk's first line)
/// starts with "fname:line: " when it is printed.
#[derive(Clone, Debug)]
pub struct Message {
    pub kind: MessageKind,
    pub parts: Vec<(Option<i32>, Vec<u8>)>,
}

/// A message that ends the run, as its exit status and the parts of a [`Message`].
pub type Fatal = (i32, Vec<(Option<i32>, Vec<u8>)>);

/// Messages that tippecanoe prints only once per run, and the ones it prints every time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageKind {
    Always,
    Once(&'static str),
}

/// What a chunk read.
#[derive(Default)]
pub struct Chunk {
    /// `seq` is the feature's rank within the chunk, `layer` an index into
    /// `layers`, and the attributes point into `keys` and `values`.
    pub features: Buf<Feature>,
    /// The features' ids, extents and label points, as `FeatureStore` keeps
    /// them.
    pub ids: Buf<u64>,
    pub extents: Buf<i64>,
    pub label_points: Buf<u64>,
    /// The features' geometry, packed as `store::Geometry` keeps it.
    pub geometry: Buf<u8>,
    pub geometry_vertices: u64,
    pub attrs: Buf<Attr>,
    pub keys: Interner,
    pub values: Interner,
    pub layers: Vec<ChunkLayer>,
    pub layer_index: HashMap<String, u32>,
    /// `--no-simplification-of-shared-nodes`: the vertices with their
    /// neighbors, and the nodes that must be kept, as tippecanoe writes them
    /// to its vertex and node files.
    pub vertices: SharedVertices,
    pub nodes: Vec<u64>,
    /// Features that `--generate-ids` gives the id seq + 1.
    pub generate_ids: Vec<u32>,

    pub file_bbox: [i64; 4],
    pub file_bbox1: [i64; 4],
    pub file_bbox2: [i64; 4],
    /// The `initial_x` and `initial_y` that the chunk's first feature with a
    /// point would give its segment if it were the segment's first.
    pub initial: Option<(u32, u32)>,

    /// The terms tippecanoe adds to `dist_sum` and `area_sum`, in order, for `-zg`.
    pub dist_terms: Buf<f64>,
    pub dist_count: usize,
    pub area_terms: Buf<f64>,

    pub messages: Vec<Message>,
    /// The once-only messages the chunk has made. Only a chunk's first can be
    /// printed, and making one can stringify a whole feature.
    pub once: Vec<&'static str>,
    /// A problem that stops tippecanoe: its exit status and message.
    pub fatal: Option<Fatal>,
    /// Whether this chunk took a bare geometry before it found any feature,
    /// which it would not have done if an earlier chunk had found one.
    pub bare_before_feature: bool,
    pub found_features: i64,
    pub found_geometries: i64,
    pub found_hashes: i64,
    /// The JSON syntax error that ended the chunk, if any, as a message.
    pub error: Option<Message>,
    /// Whether the chunk ended inside a value, so that it would have been
    /// read on into the next chunk.
    pub cut_short: bool,
    /// Whether the error was found at the very end of the chunk, where the
    /// next chunk's first byte might have changed it.
    pub error_at_end: bool,
    /// `--h3`: the points counted into hexagons.
    pub h3: Option<Box<super::h3::Bins>>,
}

impl Chunk {
    /// Frees what only parsing needs, and the room that growing left, and
    /// settles the rest where the memory budget says (`Buf::settle`): a read
    /// chunk waits for the others before they are merged.
    pub fn finish(&mut self) {
        self.keys.finish();
        self.values.finish();
        self.features.settle();
        self.ids.settle();
        self.extents.settle();
        self.label_points.settle();
        self.geometry.settle();
        self.attrs.settle();
        self.dist_terms.settle();
        self.area_terms.settle();
    }

    pub fn new(layers: &[String]) -> Chunk {
        let mut c = Chunk {
            file_bbox: [u32::MAX as i64, u32::MAX as i64, 0, 0],
            file_bbox1: [0xFFFFFFFF, 0xFFFFFFFF, 0, 0],
            file_bbox2: [0x1FFFFFFFF, 0xFFFFFFFF, 0x100000000, 0],
            ..Default::default()
        };
        for l in layers {
            c.layer(l);
        }
        c
    }

    /// The chunk's index for layer `name`, adding it if it is new.
    pub fn layer(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.layer_index.get(name) {
            return i;
        }
        let i = self.layers.len() as u32;
        self.layers.push(ChunkLayer {
            name: name.to_string(),
            ..Default::default()
        });
        self.layer_index.insert(name.to_string(), i);
        i
    }

    /// A message of one line.
    /// Whether the chunk has yet to make the once-only message `key`, which it
    /// then makes.
    pub fn first(&mut self, key: &'static str) -> bool {
        if self.once.contains(&key) {
            return false;
        }
        self.once.push(key);
        true
    }

    pub fn message(&mut self, kind: MessageKind, line: Option<i32>, text: impl Into<Vec<u8>>) {
        self.messages.push(Message {
            kind,
            parts: vec![(line, text.into())],
        });
    }
}

/// The merged layer statistics, in the form tippecanoe-rs's metadata takes.
pub fn layermap_entry(id: usize, l: &ChunkLayer) -> LayermapEntry {
    LayermapEntry {
        id,
        tilestats: l
            .stats
            .iter()
            .map(|(k, v)| (k.clone(), v.to_tilestat()))
            .collect(),
        points: l.points,
        lines: l.lines,
        polygons: l.polygons,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tippecanoe::mvt::MVT_STRING;

    /// Packed vertices sort as `Vertex` does, and points off the grid stay
    /// whole.
    #[test]
    fn packed_vertices_sort_as_vertices() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // few distinct coordinates, so that points and mids repeat
        let mut coord = || match next() % 4 {
            0 => 0,
            1 => (1i64 << 32) - 1,
            _ => (next() % 5) as i64 * 0x3FFF_FFFF,
        };
        let mut point = || Draw::new(VT_MOVETO, coord(), coord());

        let mut vertices = SharedVertices::default();
        let mut whole = Vec::new();
        for _ in 0..5000 {
            let v = Vertex::new(point(), point(), point());
            vertices.push(v);
            whole.push(v);
        }
        assert!(vertices.wide.is_empty());
        vertices.packed.sort_unstable();
        whole.sort_unstable();
        for (p, w) in vertices.packed.iter().zip(&whole) {
            assert_eq!(unpack(p[0]), w.mid);
            assert_eq!(unpack(p[1]), w.p1);
            assert_eq!(unpack(p[2]), w.p2);
        }

        let off = Draw::new(VT_MOVETO, -1, 5);
        let far = Draw::new(VT_MOVETO, 3, 1i64 << 32);
        assert_eq!(pack(off), None);
        assert_eq!(pack(far), None);
        vertices.push(Vertex::new(off, point(), point()));
        vertices.push(Vertex::new(point(), far, point()));
        assert_eq!(vertices.wide.len(), 2);
    }

    #[test]
    fn interning() {
        let mut i = Interner::default();
        let a = i.intern(MVT_STRING, b"abc");
        let b = i.intern(MVT_DOUBLE, b"abc");
        let c = i.intern(MVT_STRING, b"abc");
        assert_eq!(a, c);
        assert_ne!(a, b);
        for n in 0..5000 {
            i.intern(MVT_DOUBLE, n.to_string().as_bytes());
        }
        for n in 0..5000 {
            let id = i.intern(MVT_DOUBLE, n.to_string().as_bytes());
            assert_eq!(i.get(id), (MVT_DOUBLE, n.to_string().as_bytes()));
        }
        assert_eq!(i.len(), 5002);
        let (ids, starts) = i.by_shard();
        assert_eq!((ids.len(), starts[SHARDS]), (5002, 5002));
        for s in 0..SHARDS {
            assert!(
                ids[starts[s]..starts[s + 1]]
                    .windows(2)
                    .all(|w| w[0] < w[1])
            );
        }
    }

    #[test]
    fn stats_match_add_to_tilestats() {
        use std::sync::atomic::Ordering;
        // add_to_tilestats reads the limit from tippecanoe-rs's globals,
        // which other tests set
        let _g = crate::tiler::oracle::GLOBALS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        tippecanoe::globals::MAX_TILESTATS_SAMPLE_VALUES.store(7, Ordering::Relaxed);
        let mut ts = BTreeMap::new();
        let mut st = Stat::default();
        let mut a = Stat::default();
        let mut b = Stat::default();
        let mut x: u64 = 12345;
        for i in 0..400 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let v = SerialVal {
                type_: if x % 3 == 0 { MVT_STRING } else { MVT_DOUBLE },
                s: ((x >> 40) % 50).to_string(),
            };
            tippecanoe::mbtiles::add_to_tilestats(&mut ts, "k", &v);
            st.add(v.type_, &v.s, 7);
            if i < 150 {
                a.add(v.type_, &v.s, 7);
            } else {
                b.add(v.type_, &v.s, 7);
            }
        }
        a.merge(b, 7);
        let want = &ts["k"];
        for s in [&st, &a] {
            let got = s.to_tilestat();
            assert_eq!(got.sample_values, want.sample_values);
            assert_eq!(got.min, want.min);
            assert_eq!(got.max, want.max);
            assert_eq!(got.type_, want.type_);
        }
    }
}
