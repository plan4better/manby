//! The in-memory feature store that ingest fills, order sorts and the tiler reads.
//!
//! A `Feature` holds what tippecanoe serializes for a feature in its temporary
//! files. Geometry and attributes live in shared arrays that the records point
//! into, so sorting moves only the fixed-size records.

use std::collections::BTreeMap;

use rayon::prelude::*;

use crate::buf::Buf;

use tippecanoe::geometry::{Draw, DrawVec, VT_LINETO, VT_MOVETO};
use tippecanoe::mbtiles::LayermapEntry;
use tippecanoe::serial::SerialVal;

/// One input feature, in 56 bytes. Its id, extent and label point, which
/// many features do not have, are in `FeatureStore`'s columns.
#[derive(Clone, Copy, Debug, Default)]
pub struct Feature {
    /// The position on the space-filling curve that features are sorted by,
    /// tippecanoe's `struct index` `ix`.
    pub ix: u64,
    /// Input sequence number, tippecanoe's `seq`, which keeps the order of
    /// features with the same index deterministic.
    pub seq: u64,
    /// The feature's vertices are `geometry[geom_start..geom_start + geom_len]`.
    pub geom_start: u64,
    /// The feature's attributes are `attrs[attr_start..attr_start + attr_len]`.
    pub attr_start: u64,
    pub geom_len: u32,
    pub attr_len: u32,
    /// Index into `FeatureStore::layers`: the layer that tippecanoe's tiling
    /// puts the feature in, which is the one it names except where layer
    /// numbers collide (see `segment_layers`).
    pub layer: u32,
    /// The tippecanoe reader that read the feature (`sf.segment`), whose
    /// string pool, layer numbers and `initial_x`/`initial_y` its serialized
    /// form refers to. The same for any number of manby threads.
    pub segment: u16,
    /// `VT_POINT`, `VT_LINE` or `VT_POLYGON`.
    pub t: i8,
    pub has_id: bool,
    /// The minimum zoom that the drop rate assigns (`calc_feature_minzoom`).
    pub feature_minzoom: i8,
    /// `"tippecanoe": {"minzoom": …, "maxzoom": …}` in the input, or -1.
    pub tippecanoe_minzoom: i8,
    pub tippecanoe_maxzoom: i8,
    /// Whether tippecanoe's serialized `index` keeps `ix`, which it does if an
    /// option needs it (dropping or coalescing the densest, gamma,
    /// clustering, ...).
    pub keep_index: bool,
}

impl Feature {
    /// tippecanoe's serialized `index`, as the tiling reads it: `ix`, or 0.
    pub fn index(&self) -> u64 {
        if self.keep_index { self.ix } else { 0 }
    }
}

/// An attribute of a feature: interned key and value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    pub key: u32,
    pub value: u32,
}

/// Where a feature's attribute is in its segment's string pool, as tippecanoe
/// serializes it: the offsets of the key and of the value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolOffsets {
    pub key: i64,
    pub value: i64,
}

/// A pointer that parallel jobs write through at places that do not
/// overlap.
struct Shared<T>(*mut T);

// SAFETY: the jobs write disjoint places, and the owner outlives them.
unsafe impl<T> Sync for Shared<T> {}

/// A sequence kept as the vectors it was built in, so that joining the
/// pieces that parallel jobs made does not copy them.
#[derive(Clone)]
pub struct Chunked<T> {
    parts: Vec<Buf<T>>,
    /// The end of each part in the whole sequence.
    ends: Vec<usize>,
}

impl<T> Default for Chunked<T> {
    fn default() -> Self {
        Chunked {
            parts: Vec::new(),
            ends: Vec::new(),
        }
    }
}

impl<T> Chunked<T> {
    pub fn len(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends a part.
    pub fn append(&mut self, part: impl Into<Buf<T>>) {
        let part = part.into();
        if part.is_empty() {
            return;
        }
        self.ends.push(self.len() + part.len());
        self.parts.push(part);
    }

    pub fn push(&mut self, v: T) {
        if self.parts.is_empty() {
            self.parts.push(Buf::new());
            self.ends.push(0);
        }
        self.parts.last_mut().unwrap().push(v);
        *self.ends.last_mut().unwrap() += 1;
    }

    fn locate(&self, i: usize) -> (usize, usize) {
        let n = self.parts.len();
        let p = if n == 1 {
            0
        } else {
            // the parts are often about the same size, so first try the
            // part that the position would be in if they all were
            let guess = (i as u64)
                .checked_mul(n as u64)
                .map_or(0, |x| x / self.len().max(1) as u64) as usize;
            let guess = guess.min(n - 1);
            let start = if guess == 0 { 0 } else { self.ends[guess - 1] };
            if start <= i && i < self.ends[guess] {
                guess
            } else {
                self.ends.partition_point(|&e| e <= i)
            }
        };
        let start = if p == 0 { 0 } else { self.ends[p - 1] };
        (p, i - start)
    }

    /// `len` items from `start`, which lie in one part.
    pub fn slice(&self, start: usize, len: usize) -> &[T] {
        if len == 0 {
            return &[];
        }
        let (p, off) = self.locate(start);
        &self.parts[p][off..off + len]
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.parts.iter().flatten()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.parts.iter_mut().flatten()
    }
}

impl<T> std::ops::Index<usize> for Chunked<T> {
    type Output = T;

    fn index(&self, i: usize) -> &T {
        let (p, off) = self.locate(i);
        &self.parts[p][off]
    }
}

impl<T: Clone> Chunked<T> {
    pub fn extend_from_slice(&mut self, s: &[T]) {
        for v in s {
            self.push(v.clone());
        }
    }
}

impl<'a, T> IntoIterator for &'a Chunked<T> {
    type Item = &'a T;
    type IntoIter = std::iter::Flatten<std::slice::Iter<'a, Buf<T>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.parts.iter().flatten()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Chunked<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// Feature geometry, packed as tippecanoe's temporary files keep it: for
/// each vertex its op and the differences of its coordinates from the
/// vertex before, in units of the geometry scale, as varints. The vertex
/// codes are `zigzag(dx) << 2 | k`, where `k` is 0 for a move, 1 for a
/// line, and 2 for any other op, which follows as a byte without
/// coordinates; `zigzag(dy)` follows a move or line.
#[derive(Clone, Debug, Default)]
pub struct Geometry {
    bytes: Chunked<u8>,
    vertices: u64,
}

/// Where bytes can be appended.
pub trait Bytes {
    fn put(&mut self, b: u8);
}

impl Bytes for Vec<u8> {
    #[inline]
    fn put(&mut self, b: u8) {
        self.push(b);
    }
}

impl Bytes for Buf<u8> {
    #[inline]
    fn put(&mut self, b: u8) {
        self.push(b);
    }
}

#[inline]
pub(crate) fn put_varint(out: &mut impl Bytes, mut v: u64) {
    while v >= 0x80 {
        out.put(v as u8 | 0x80);
        v >>= 7;
    }
    out.put(v as u8);
}

#[inline(always)]
pub(crate) fn get_varint(data: &[u8], pos: &mut usize) -> u64 {
    let b = data[*pos];
    *pos += 1;
    if b < 0x80 {
        return b as u64;
    }
    let mut v = (b & 0x7F) as u64;
    let mut shift = 7;
    loop {
        let b = data[*pos];
        *pos += 1;
        v |= ((b & 0x7F) as u64) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
    }
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

impl Geometry {
    /// The bytes of the `n` vertices from byte `start`.
    fn raw(&self, start: u64, n: u32) -> &[u8] {
        if n == 0 {
            return &[];
        }
        let (p, off) = self.bytes.locate(start as usize);
        let data = &self.bytes.parts[p][off..];
        let mut pos = 0;
        for _ in 0..n {
            if get_varint(data, &mut pos) & 3 == 2 {
                pos += 1;
            } else {
                get_varint(data, &mut pos);
            }
        }
        &data[..pos]
    }

    /// Appends a feature's geometry, its coordinates already divided by the
    /// geometry scale, to `out`.
    pub fn encode(out: &mut impl Bytes, g: &[Draw]) {
        let (mut x, mut y) = (0i64, 0i64);
        for d in g {
            if d.op == VT_MOVETO || d.op == VT_LINETO {
                let k = (d.op == VT_LINETO) as u64;
                put_varint(out, zigzag(d.x - x) << 2 | k);
                put_varint(out, zigzag(d.y - y));
                x = d.x;
                y = d.y;
            } else {
                put_varint(out, 2);
                out.put(d.op as u8);
            }
        }
    }

    /// Appends the geometry that one reader wrote, with `vertices` vertices.
    pub fn append(&mut self, part: impl Into<Buf<u8>>, vertices: u64) {
        self.bytes.append(part);
        self.vertices += vertices;
    }

    /// The bytes so far, where the next feature's geometry starts.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The number of vertices.
    pub fn vertices(&self) -> u64 {
        self.vertices
    }

    /// Appends `n` vertices from byte `start` to `out`, in world coordinates.
    pub fn decode(&self, start: u64, n: u32, geometry_scale: i32, out: &mut DrawVec) {
        if n == 0 {
            return;
        }
        let (p, off) = self.bytes.locate(start as usize);
        Geometry::decode_from(&self.bytes.parts[p][off..], n, geometry_scale, out);
    }

    /// Appends the `n` vertices that `encode` wrote at the start of `data` to
    /// `out`, the coordinates multiplied by `2^shift`.
    pub fn decode_from(data: &[u8], n: u32, shift: i32, out: &mut DrawVec) {
        let mut d = Decoder::default();
        out.reserve(n as usize);
        for _ in 0..n {
            out.push(d.next(data, shift));
        }
    }

    /// Fills `out` with the vertices that `encode` wrote at the start of
    /// `data`, as `decode_from` would.
    pub fn decode_slice(data: &[u8], shift: i32, out: &mut [Draw]) {
        let mut d = Decoder::default();
        for o in out {
            *o = d.next(data, shift);
        }
    }
}

#[derive(Default)]
struct Decoder {
    pos: usize,
    x: i64,
    y: i64,
}

impl Decoder {
    #[inline(always)]
    fn next(&mut self, data: &[u8], shift: i32) -> Draw {
        let c = get_varint(data, &mut self.pos);
        if c & 3 == 2 {
            let op = data[self.pos] as i8;
            self.pos += 1;
            return Draw::new(op, 0, 0);
        }
        self.x += unzigzag(c >> 2);
        self.y += unzigzag(get_varint(data, &mut self.pos));
        let op = if c & 3 == 0 { VT_MOVETO } else { VT_LINETO };
        Draw::new(op, self.x << shift, self.y << shift)
    }
}

/// A value: its type and its string, as tippecanoe's string pool keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Value<'a> {
    pub type_: i32,
    pub s: &'a str,
}

impl Value<'_> {
    pub fn to_serial(self) -> SerialVal {
        SerialVal::new(self.type_, self.s)
    }
}

/// Increasing offsets in 4 bytes each, and where they pass each multiple
/// of 2^32.
#[derive(Clone, Default, PartialEq, Eq)]
struct Ends {
    low: Buf<u32>,
    /// For each multiple of 2^32 passed, the first offset past it.
    wraps: Vec<u32>,
}

impl Ends {
    fn get(&self, i: usize) -> u64 {
        let hi = if self.wraps.is_empty() {
            0
        } else {
            self.wraps.partition_point(|&w| w as usize <= i)
        };
        (hi as u64) << 32 | self.low[i] as u64
    }

    fn push(&mut self, e: u64) {
        while self.wraps.len() < (e >> 32) as usize {
            self.wraps.push(self.low.len() as u32);
        }
        self.low.push(e as u32);
    }

    fn reserve(&mut self, n: usize) {
        self.low.reserve(n);
    }
}

/// The interned values, packed: a type and the end of its string per value.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Values {
    types: Buf<i8>,
    ends: Ends,
    bytes: Buf<u8>,
}

impl Values {
    /// Room for `n` values with `bytes` of strings.
    pub fn with_capacity(n: usize, bytes: usize) -> Values {
        let mut v = Values {
            types: Buf::with_capacity(n),
            ends: Ends::default(),
            bytes: Buf::with_capacity(bytes),
        };
        v.ends.reserve(n);
        v
    }

    /// Appends values from their types, the ends of their strings within
    /// `bytes`, and the strings, which must be UTF-8.
    pub fn append(&mut self, types: &[i8], ends: &[usize], bytes: &[u8]) {
        debug_assert!(std::str::from_utf8(bytes).is_ok());
        let base = self.bytes.len() as u64;
        self.types.extend_from_slice(types);
        for &e in ends {
            self.ends.push(base + e as u64);
        }
        self.bytes.extend_from_slice(bytes);
    }

    pub fn len(&self) -> usize {
        self.types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    pub fn get(&self, i: u32) -> Value<'_> {
        let i = i as usize;
        let start = if i == 0 {
            0
        } else {
            self.ends.get(i - 1) as usize
        };
        let bytes = &self.bytes[start..self.ends.get(i) as usize];
        Value {
            type_: self.types[i] as i32,
            // SAFETY: only strings are pushed.
            s: unsafe { std::str::from_utf8_unchecked(bytes) },
        }
    }

    pub fn push(&mut self, type_: i32, s: &str) {
        self.types.push(type_ as i8);
        self.bytes.extend_from_slice(s.as_bytes());
        self.ends.push(self.bytes.len() as u64);
    }

    pub fn iter(&self) -> impl Iterator<Item = Value<'_>> {
        (0..self.len() as u32).map(|i| self.get(i))
    }

    /// Values from their types, the ends of their strings, and the strings,
    /// which must be UTF-8.
    pub fn from_parts(
        types: impl Into<Buf<i8>>,
        ends: impl Into<Buf<u64>>,
        bytes: impl Into<Buf<u8>>,
    ) -> Values {
        let (types, ends, bytes): (Buf<i8>, Buf<u64>, Buf<u8>) =
            (types.into(), ends.into(), bytes.into());
        assert_eq!(types.len(), ends.len());
        assert!(std::str::from_utf8(&bytes).is_ok());
        let mut e = Ends::default();
        e.reserve(ends.len());
        for &x in ends.iter() {
            e.push(x);
        }
        Values {
            types,
            ends: e,
            bytes,
        }
    }
}

impl std::fmt::Debug for Values {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// A layer: its name and the statistics tippecanoe keeps for the metadata.
#[derive(Clone, Debug)]
pub struct Layer {
    pub name: String,
    pub entry: LayermapEntry,
}

#[derive(Default)]
pub struct FeatureStore {
    pub features: Buf<Feature>,
    /// Each feature's `extent`, or empty when they are all 0; features
    /// past its end have 0.
    pub extents: Buf<i64>,
    /// Each feature's `label_point`, in the same way.
    pub label_points: Buf<u64>,
    /// Each feature's id, in the same way; 0 for one without `has_id`.
    pub ids: Buf<u64>,
    /// World coordinates (2^32 per side at zoom 0), with tippecanoe's move and
    /// line ops, as `decode_geometry` returns them at zoom 0.
    pub geometry: Geometry,
    /// The geometry's coordinates are multiples of `2^geometry_scale`.
    pub geometry_scale: i32,
    pub attrs: Chunked<Attr>,
    /// Interned attribute keys.
    pub keys: Vec<String>,
    /// Interned attribute values, typed as tippecanoe's string pool types them.
    pub values: Values,
    pub layers: Vec<Layer>,
    /// Bounds of the input in world coordinates, as tippecanoe's `file_bbox`.
    pub bbox: [i64; 4],
    /// tippecanoe's `file_bbox1` and `file_bbox2`: the bounds on the -180 to
    /// 180 and the 0 to 360 world planes, for the antimeridian-aware bounds.
    pub bbox1: [i64; 4],
    pub bbox2: [i64; 4],
    /// What `-zg` needs from reading: tippecanoe's `dist_sum`, `dist_count`
    /// and `area_sum`.
    pub dist_sum: f64,
    pub dist_count: usize,
    pub area_sum: f64,
    /// `--no-simplification-of-shared-nodes`: the points shared between
    /// features, sorted, and tippecanoe's bloom filter of them. Both are
    /// empty without that option.
    pub shared_nodes: Vec<tippecanoe::serial::Node>,
    pub shared_nodes_bloom: Vec<u8>,
    /// tippecanoe's `initial_x` and `initial_y`, by segment: the point that
    /// the first geometry delta of each serialized feature is relative to,
    /// after `SHIFT_RIGHT`. 0 for a segment that read no point.
    pub initial_x: Vec<u32>,
    pub initial_y: Vec<u32>,
    /// `segment_layers[segment][layer]`: the layer number that tippecanoe
    /// serializes for a feature of store layer `layer` read by `segment`,
    /// or -1 if that segment has no such layer.
    pub segment_layers: Vec<Vec<i64>>,

    // The rest is what tippecanoe's temporary files hold beyond the features
    // themselves, which `-at` needs to estimate tile sizes from their bytes.
    // Without `-at` it is left empty.
    /// For each entry of `attrs`, its offsets in its feature's segment's
    /// string pool.
    pub pool_offsets: Buf<PoolOffsets>,
    /// The bytes the features take in tippecanoe's geometry files: for each,
    /// its length as a varint and its serialized form.
    pub serialized_bytes: u64,
}

impl FeatureStore {
    /// Feature `i`'s extent.
    pub fn extent(&self, i: usize) -> i64 {
        self.extents.get(i).copied().unwrap_or(0)
    }

    /// Feature `i`'s id.
    pub fn id(&self, i: usize) -> u64 {
        self.ids.get(i).copied().unwrap_or(0)
    }

    /// Feature `i`'s label point.
    pub fn label_point(&self, i: usize) -> u64 {
        self.label_points.get(i).copied().unwrap_or(0)
    }

    /// Sets feature `i`'s id, extent and label point.
    pub fn set_extras(&mut self, i: usize, id: u64, extent: i64, label_point: u64) {
        set_sparse(&mut self.ids, i, id);
        set_sparse(&mut self.extents, i, extent);
        set_sparse(&mut self.label_points, i, label_point);
    }

    /// Sorts the features by index and sequence, which makes every key
    /// distinct, and their extents and label points with them.
    pub fn sort_features(&mut self) {
        if self.ids.is_empty() && self.extents.is_empty() && self.label_points.is_empty() {
            self.features.par_sort_unstable_by_key(|f| (f.ix, f.seq));
            return;
        }
        let n = self.features.len();
        let mut order: Buf<u32> = (0..n as u32).collect();
        let features = &self.features;
        order.par_sort_unstable_by_key(|&i| (features[i as usize].ix, features[i as usize].seq));
        self.features = order.iter().map(|&i| features[i as usize]).collect();
        permute(&mut self.ids, &order);
        permute(&mut self.extents, &order);
        permute(&mut self.label_points, &order);
    }

    /// The feature's geometry, in world coordinates, as `decode_geometry`
    /// reads it back at zoom 0.
    pub fn feature_geometry(&self, f: &Feature) -> DrawVec {
        let mut out = DrawVec::new();
        self.geometry_into(f, &mut out);
        out
    }

    /// Appends the feature's geometry to `out`.
    pub fn geometry_into(&self, f: &Feature, out: &mut DrawVec) {
        self.geometry
            .decode(f.geom_start, f.geom_len, self.geometry_scale, out);
    }

    /// Appends world geometry, whose coordinates must be multiples of the
    /// geometry scale, returning where it starts.
    pub fn push_geometry(&mut self, g: &[Draw]) -> u64 {
        let gs = self.geometry_scale;
        let scaled: DrawVec = g
            .iter()
            .map(|d| {
                debug_assert!(d.x % (1i64 << gs) == 0 && d.y % (1i64 << gs) == 0);
                Draw::new(d.op, d.x >> gs, d.y >> gs)
            })
            .collect();
        let start = self.geometry.len() as u64;
        let mut part = Vec::new();
        Geometry::encode(&mut part, &scaled);
        self.geometry.append(part, g.len() as u64);
        start
    }

    /// The bounds of the feature's points, min x, min y, max x, max y, as
    /// `decode_geometry` gives them at zoom 0.
    pub fn feature_bbox(&self, f: &Feature) -> [i64; 4] {
        let mut b = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
        for d in &self.feature_geometry(f) {
            if d.op == VT_MOVETO || d.op == VT_LINETO {
                b[0] = b[0].min(d.x);
                b[1] = b[1].min(d.y);
                b[2] = b[2].max(d.x);
                b[3] = b[3].max(d.y);
            }
        }
        b
    }

    /// Lays the geometry and attributes out in the order of the features,
    /// so that going through the features in order goes through them in
    /// order too, rather than in the order they were read. The copying goes
    /// forward through the old layout once per bucket of features, so that
    /// what it touches at random is one bucket's part of the new layout,
    /// which `bucket_bytes` bounds.
    pub fn lay_out_in_order(&mut self, bucket_bytes: u64) {
        let n = self.features.len();
        if n == 0 {
            return;
        }
        // where each feature was, in the order it was read
        let mut order: Buf<(u64, u64, u32, u32)> = Buf::with_capacity(n);
        for (i, f) in self.features.iter().enumerate() {
            order.push((f.geom_start, f.attr_start, f.geom_len, i as u32));
            if i % (1 << 16) == 0 {
                crate::buf::relieve();
            }
        }
        order.par_sort_unstable();

        let geometry = &self.geometry;
        let mut lens: Buf<u32> = Buf::with_capacity(n);
        lens.resize(n, 0);
        let out = Shared(lens.as_mut_ptr());
        order.par_chunks(1 << 12).for_each(|block| {
            let out = &out;
            for &(g, _, vertices, i) in block {
                let len = geometry.raw(g, vertices).len() as u32;
                // SAFETY: each index is in order once.
                unsafe { *out.0.add(i as usize) = len };
            }
            crate::buf::relieve();
        });

        let (mut g, mut a, mut vertices) = (0u64, 0u64, 0u64);
        for (i, (f, &len)) in self.features.iter_mut().zip(lens.iter()).enumerate() {
            f.geom_start = g;
            f.attr_start = a;
            g += len as u64;
            a += f.attr_len as u64;
            vertices += f.geom_len as u64;
            if i % (1 << 16) == 0 {
                crate::buf::relieve();
            }
        }
        drop(lens);

        let pooled = !self.pool_offsets.is_empty();
        let mut new_geometry: Buf<u8> = Buf::with_capacity(g as usize);
        let mut new_attrs: Buf<Attr> = Buf::with_capacity(a as usize);
        let mut new_pool: Buf<PoolOffsets> =
            Buf::with_capacity(if pooled { a as usize } else { 0 });
        let to = (
            Shared(new_geometry.as_mut_ptr()),
            Shared(new_attrs.as_mut_ptr()),
            Shared(new_pool.as_mut_ptr()),
        );
        let (features, attrs, pool) = (&self.features, &self.attrs, &self.pool_offsets);
        let per_feature = (g + a * 8) / n as u64 + 80;
        let buckets = (n as u64 * per_feature)
            .div_ceil(bucket_bytes.max(1))
            .max(1) as usize;
        for b in 0..buckets {
            let (lo, hi) = ((n * b / buckets) as u32, (n * (b + 1) / buckets) as u32);
            order.par_chunks(1 << 12).for_each(|block| {
                let to = &to;
                for &(og, oa, vertices, i) in block {
                    if i < lo || i >= hi {
                        continue;
                    }
                    let f = &features[i as usize];
                    let bytes = geometry.raw(og, vertices);
                    let (oa, len) = (oa as usize, f.attr_len as usize);
                    // SAFETY: the features' new places are disjoint and lie
                    // within the capacities, and each feature is copied once.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            bytes.as_ptr(),
                            to.0.0.add(f.geom_start as usize),
                            bytes.len(),
                        );
                        std::ptr::copy_nonoverlapping(
                            attrs.slice(oa, len).as_ptr(),
                            to.1.0.add(f.attr_start as usize),
                            len,
                        );
                        if pooled {
                            std::ptr::copy_nonoverlapping(
                                pool[oa..oa + len].as_ptr(),
                                to.2.0.add(f.attr_start as usize),
                                len,
                            );
                        }
                    }
                }
                crate::buf::relieve();
            });
        }
        // SAFETY: every byte and attribute was written above.
        unsafe {
            new_geometry.set_len(g as usize);
            new_attrs.set_len(a as usize);
            if pooled {
                new_pool.set_len(a as usize);
            }
        }
        let mut geometry = Geometry::default();
        geometry.append(new_geometry, vertices);
        self.geometry = geometry;
        self.attrs = Chunked::default();
        self.attrs.append(new_attrs);
        self.pool_offsets = new_pool;
    }

    /// Numbers the values in the order the attributes, in turn, first use
    /// them, and lays them out in that order, so that going through the
    /// features in order goes through their values about in order too.
    pub fn number_values_in_order(&mut self, bucket_bytes: u64) {
        let nv = self.values.len();
        if nv == 0 {
            return;
        }
        let mut new_id: Buf<u32> = Buf::with_capacity(nv);
        new_id.resize(nv, u32::MAX);
        let mut next = 0u32;
        for (k, a) in self.attrs.iter_mut().enumerate() {
            let v = &mut new_id[a.value as usize];
            if *v == u32::MAX {
                *v = next;
                next += 1;
            }
            a.value = *v;
            if k % (1 << 16) == 0 {
                crate::buf::relieve();
            }
        }
        for v in new_id.iter_mut() {
            if *v == u32::MAX {
                *v = next;
                next += 1;
            }
        }

        let old = &self.values;
        let span = |o: usize| {
            let start = if o == 0 { 0 } else { old.ends.get(o - 1) };
            (start as usize, old.ends.get(o) as usize)
        };
        let mut types: Buf<i8> = Buf::with_capacity(nv);
        types.resize(nv, 0);
        let mut ends: Buf<u64> = Buf::with_capacity(nv);
        ends.resize(nv, 0);
        for (o, &n) in new_id.iter().enumerate() {
            let (start, end) = span(o);
            types[n as usize] = old.types[o];
            ends[n as usize] = (end - start) as u64;
            if o % (1 << 16) == 0 {
                crate::buf::relieve();
            }
        }
        let mut total = 0;
        for e in ends.iter_mut() {
            total += *e;
            *e = total;
        }

        let mut bytes: Buf<u8> = Buf::with_capacity(total as usize);
        let to = Shared(bytes.as_mut_ptr());
        let buckets = total.div_ceil(bucket_bytes.max(1)).max(1) as usize;
        for b in 0..buckets {
            let (lo, hi) = ((nv * b / buckets) as u32, (nv * (b + 1) / buckets) as u32);
            new_id
                .par_chunks(1 << 14)
                .enumerate()
                .for_each(|(c, block)| {
                    let to = &to;
                    for (k, &n) in block.iter().enumerate() {
                        if n < lo || n >= hi {
                            continue;
                        }
                        let (start, end) = span((c << 14) + k);
                        let at = if n == 0 {
                            0
                        } else {
                            ends[n as usize - 1] as usize
                        };
                        // SAFETY: the values' new places are disjoint and lie
                        // within the capacity, and each value is copied once.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                old.bytes.as_ptr().add(start),
                                to.0.add(at),
                                end - start,
                            )
                        };
                    }
                    crate::buf::relieve();
                });
        }
        // SAFETY: every byte was written above.
        unsafe { bytes.set_len(total as usize) };
        let mut packed = Ends::default();
        packed.reserve(nv);
        for &e in ends.iter() {
            packed.push(e);
        }
        drop(ends);
        self.values = Values {
            types,
            ends: packed,
            bytes,
        };
    }

    pub fn feature_attrs(&self, f: &Feature) -> &[Attr] {
        self.attrs.slice(f.attr_start as usize, f.attr_len as usize)
    }

    /// The layer statistics in the form tippecanoe-rs's metadata code takes.
    pub fn layermap(&self) -> BTreeMap<String, LayermapEntry> {
        self.layers
            .iter()
            .map(|l| (l.name.clone(), l.entry.clone()))
            .collect()
    }
}

/// Sets item `i` of a column that is empty while all its items are 0.
pub fn set_sparse<T: Copy + Default + PartialEq>(column: &mut Buf<T>, i: usize, v: T) {
    if v == T::default() && column.len() <= i {
        return;
    }
    if column.len() <= i {
        column.resize(i + 1, T::default());
    }
    column[i] = v;
}

/// Puts a column that is empty while all its items are 0 in `order`.
fn permute<T: Copy + Default>(column: &mut Buf<T>, order: &[u32]) {
    if column.is_empty() {
        return;
    }
    column.resize(order.len(), T::default());
    *column = order.iter().map(|&i| column[i as usize]).collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ends_past_4_gb() {
        let all = [
            0,
            5,
            (1 << 32) - 1,
            1 << 32,
            (1 << 32) + 7,
            (3 << 32) + 1,
            3 << 33,
        ];
        let mut e = Ends::default();
        for &x in &all {
            e.push(x);
        }
        for (i, &x) in all.iter().enumerate() {
            assert_eq!(e.get(i), x);
        }
    }
}
