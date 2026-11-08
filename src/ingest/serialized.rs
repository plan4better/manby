//! What tippecanoe's temporary files hold beyond the features themselves: the
//! string pool offsets of the attributes, the layer numbers of each segment,
//! and the size of the serialized features. `-at` estimates tile sizes from
//! the bytes of those files, so it needs them. Nothing else does, so this
//! runs only with `-at`.

use std::ops::Range;

use rayon::prelude::*;
use tippecanoe::geometry::{VT_LINETO, VT_MOVETO};
use tippecanoe::mvt::MVT_STRING;
use tippecanoe::text::fnv1a_cstr;

use crate::buf::Buf;
use crate::store::{Feature, FeatureStore, PoolOffsets};

/// The size of each of a reader's dedup tables (sic, not 65536).
const DEDUP: usize = 655536;

/// Which of a reader's two dedup tables a string goes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    Key = 0,
    Value = 1,
}

#[derive(Clone, Copy, Default)]
struct Node {
    left: usize,
    right: usize,
    off: usize,
    hash: u64,
}

/// One reader's string pool as tippecanoe's `addpool` fills it: the offsets
/// only, with the dedup tables, the search tree and its depth limit, and the
/// switch to appending without deduplication once the pool would take more
/// than `limit` bytes of memory. The limit is tippecanoe's, from the
/// machine's memory, so the pool and the tree are kept where the memory
/// budget says.
pub struct Pool {
    /// The part of the pool that is kept in memory.
    bytes: Buf<u8>,
    off: usize,
    full: bool,
    limit: usize,
    /// The search tree. Node 0 is the placeholder that tippecanoe writes
    /// first, so that 0 can mean no node.
    nodes: Buf<Node>,
    root: usize,
    dedup: [Vec<i64>; 2],
}

fn cstr_at(map: &[u8], off: usize) -> &[u8] {
    let s = &map[off..];
    &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]
}

fn swizzlecmp(a: &[u8], atype: i32, ahash: u64, b: &[u8], btype: i32, bhash: u64) -> i64 {
    if ahash == bhash {
        if atype == btype {
            a.cmp(b) as i64
        } else {
            (atype - btype) as i64
        }
    } else {
        ahash.wrapping_sub(bhash) as i64
    }
}

impl Pool {
    /// A pool that stops deduplicating at `limit` bytes, tippecanoe's
    /// `memsize / CPUS / 10`.
    pub fn new(limit: usize) -> Pool {
        let mut nodes = Buf::new();
        nodes.push(Node::default());
        Pool {
            bytes: Buf::new(),
            off: 0,
            full: false,
            limit,
            nodes,
            root: 0,
            dedup: [vec![-1; DEDUP], vec![-1; DEDUP]],
        }
    }

    /// Appends the entry, returning whether it went to memory.
    fn write(&mut self, s: &[u8], type_: i8) -> bool {
        self.off += s.len() + 2;
        if self.full {
            return false;
        }
        self.bytes.push(type_ as u8);
        self.bytes.extend_from_slice(s);
        self.bytes.push(0);
        true
    }

    /// `addpool`: the offset of the C string `s` of type `type_`, which is
    /// added if the pool does not find it.
    pub fn add(&mut self, s: &[u8], type_: i8, table: Table) -> i64 {
        let s = cstr_at(s, 0);
        let t = table as usize;
        let hash = fnv1a_cstr(s, type_ as u8);
        let h = (hash % DEDUP as u64) as usize;

        let d = self.dedup[t][h];
        if d >= 0
            && self.bytes[d as usize] as i8 == type_
            && cstr_at(&self.bytes, d as usize + 1) == s
        {
            return d;
        }

        let max = ((3.0 * (self.nodes.len() as f64).ln() / 2f64.ln()) as usize).max(30);
        let mut parent: Option<(usize, bool)> = None;
        let mut cur = self.root;
        let mut depth = 0;
        while cur != 0 {
            let n = self.nodes[cur];
            let cmp = swizzlecmp(
                s,
                type_ as i32,
                hash,
                cstr_at(&self.bytes, n.off + 1),
                self.bytes[n.off] as i8 as i32,
                n.hash,
            );
            if cmp < 0 {
                parent = Some((cur, false));
                cur = n.left;
            } else if cmp > 0 {
                parent = Some((cur, true));
                cur = n.right;
            } else {
                self.dedup[t][h] = n.off as i64;
                return n.off as i64;
            }

            depth += 1;
            if depth > max {
                // Probably unique: added without going into the tree.
                let off = self.off as i64;
                if self.write(s, type_) {
                    self.dedup[t][h] = off;
                }
                return off;
            }
        }

        if self.off + self.nodes.len() * 32 > self.limit {
            self.full = true;
        }
        let off = self.off;
        if !self.write(s, type_) {
            return off as i64;
        }
        self.dedup[t][h] = off as i64;

        let i = self.nodes.len();
        self.nodes.push(Node {
            off,
            hash,
            ..Default::default()
        });
        match parent {
            None => self.root = i,
            Some((p, false)) => self.nodes[p].left = i,
            Some((p, true)) => self.nodes[p].right = i,
        }
        off as i64
    }
}

/// A chunk of features as one segment read them.
pub(super) struct Span {
    pub segment: usize,
    pub features: Range<usize>,
    pub attrs: Range<usize>,
}

fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

fn varint_len(v: u64) -> u64 {
    (64 - (v | 1).leading_zeros() as u64).div_ceil(7)
}

/// The length of the feature as tippecanoe's `serialize_feature` writes it
/// into a reader's geometry file, before the feature's own length.
fn serialized_len(
    store: &FeatureStore,
    fi: usize,
    f: &Feature,
    layer: i64,
    pool: &[PoolOffsets],
    initial: (i64, i64),
    geometry_scale: i32,
) -> u64 {
    let mut flags = layer.wrapping_shl(7);
    let (extent, label_point) = (store.extent(fi), store.label_point(fi));
    flags |= ((label_point != 0) as i64) << 6;
    flags |= ((f.index() != 0) as i64) << 4;
    flags |= ((extent != 0) as i64) << 3;
    flags |= (f.has_id as i64) << 2;
    flags |= ((f.tippecanoe_minzoom != -1) as i64) << 1;
    flags |= (f.tippecanoe_maxzoom != -1) as i64;

    let mut n = 1 + varint_len(zigzag(flags)) + varint_len(zigzag(f.seq as i64));
    if f.tippecanoe_minzoom != -1 {
        n += varint_len(zigzag(f.tippecanoe_minzoom as i64));
    }
    if f.tippecanoe_maxzoom != -1 {
        n += varint_len(zigzag(f.tippecanoe_maxzoom as i64));
    }
    if f.has_id {
        n += varint_len(store.id(fi));
    }
    n += varint_len(zigzag(f.segment as i64));

    let (mut wx, mut wy) = initial;
    for d in store.feature_geometry(f) {
        n += 1;
        if d.op == VT_MOVETO || d.op == VT_LINETO {
            let (x, y) = (d.x >> geometry_scale, d.y >> geometry_scale);
            n += varint_len(zigzag(x - wx)) + varint_len(zigzag(y - wy));
            wx = x;
            wy = y;
        }
    }
    n += 1;

    if f.index() != 0 {
        // and the gap, which is 0 when reading
        n += varint_len(f.index()) + 1;
    }
    if label_point != 0 {
        n += varint_len(label_point);
    }
    if extent != 0 {
        n += varint_len(zigzag(extent));
    }
    n += varint_len(zigzag(pool.len() as i64));
    for p in pool {
        n += varint_len(zigzag(p.key)) + varint_len(zigzag(p.value));
    }
    n + 1
}

/// Fills `pool_offsets` and `serialized_bytes`, replaying each segment's
/// reading in order.
pub(super) fn layout(
    store: &mut FeatureStore,
    spans: &[Span],
    segments: usize,
    pool_limit: usize,
    geometry_scale: i32,
) {
    let mut offsets: Buf<PoolOffsets> = Buf::new();
    offsets.resize(store.attrs.len(), PoolOffsets::default());
    let mut by_segment: Vec<Vec<(&Span, &mut [PoolOffsets])>> =
        (0..segments).map(|_| Vec::new()).collect();
    let mut rest: &mut [PoolOffsets] = &mut offsets;
    for span in spans {
        let (mine, after) = std::mem::take(&mut rest).split_at_mut(span.attrs.len());
        rest = after;
        by_segment[span.segment].push((span, mine));
    }

    let s: &FeatureStore = store;
    let shift_right = |a: u32| ((a as f64) / ((1i64 << geometry_scale) as f64)).round() as i64;
    let bytes: u64 = by_segment
        .into_par_iter()
        .enumerate()
        .map(|(segment, spans)| {
            let layers = &s.segment_layers[segment];
            let initial = (
                shift_right(s.initial_x[segment]),
                shift_right(s.initial_y[segment]),
            );
            let mut pool = Pool::new(pool_limit);
            let mut bytes = 0u64;
            for (span, out) in spans {
                for fi in span.features.clone() {
                    let f = &s.features[fi];
                    let at = f.attr_start as usize - span.attrs.start;
                    let mine = &mut out[at..at + f.attr_len as usize];
                    for (o, a) in mine.iter_mut().zip(s.feature_attrs(f)) {
                        let v = s.values.get(a.value);
                        o.key = pool.add(
                            s.keys[a.key as usize].as_bytes(),
                            MVT_STRING as i8,
                            Table::Key,
                        );
                        o.value = pool.add(v.s.as_bytes(), v.type_ as i8, Table::Value);
                    }
                    let layer = layers[f.layer as usize];
                    let len = serialized_len(s, fi, f, layer, mine, initial, geometry_scale);
                    bytes += varint_len(zigzag(len as i64)) + len;
                }
            }
            bytes
        })
        .sum();

    store.serialized_bytes = bytes;
    store.pool_offsets = offsets;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints() {
        for v in [0u64, 1, 127, 128, 16383, 16384, u64::MAX] {
            let mut out = Vec::new();
            tippecanoe::serial::serialize_ulong_long(&mut out, v);
            assert_eq!(varint_len(v), out.len() as u64, "{}", v);
        }
        for n in [0i64, -1, 1, -64, 64, i64::MIN, i64::MAX] {
            assert_eq!(zigzag(n), tippecanoe::serial::encode_zigzag64(n));
        }
    }

    #[test]
    fn pool_dedups() {
        let mut p = Pool::new(usize::MAX);
        let a = p.add(b"name", MVT_STRING as i8, Table::Key);
        let b = p.add(b"name", 1, Table::Value);
        let c = p.add(b"name", MVT_STRING as i8, Table::Value);
        assert_eq!((a, b), (0, 6));
        // the same string and type through the other table is found in the tree
        assert_eq!(c, a);
        assert_eq!(p.add(b"name\0x", 1, Table::Value), b);
    }
}
