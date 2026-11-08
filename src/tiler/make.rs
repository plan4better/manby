//! Making a tile from its candidates: tippecanoe's `write_tile`, from choosing
//! which features to keep to encoding and checking the size, with its retries.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::clean::coalesce_polygon;
use tippecanoe::clip::{close_poly, get_mp_area as get_mp_area_vec, remove_noop, to_tile_scale};
use tippecanoe::geometry::{
    DrawVec, VT_LINE, VT_POINT, VT_POLYGON, check_polygon, checkerboard_anchors, reorder_lines,
    simplify_lines, stairstep,
};
use tippecanoe::globals::{ORDER_BY_SIZE, OrderField};
use tippecanoe::mbtiles::{LayermapEntry, add_to_tilestats};
use tippecanoe::mvt::{
    MVT_BOOL, MVT_DOUBLE, MVT_FLOAT, MVT_INT, MVT_NULL, MVT_SINT, MVT_STRING, MVT_UINT, MvtFeature,
    MvtGeometry, MvtLayer, MvtTile, MvtValue, stringified_to_mvt_value,
};
use tippecanoe::options::*;
use tippecanoe::serial::{
    FEATURE_ADDED_FOR_MULTIPLIER_DENSITY, FEATURE_DROPPED, FEATURE_KEPT, KeyPool, SerialFeature,
    SerialVal,
};
use tippecanoe::shared_borders::find_common_edges;
use tippecanoe::tile::{Strategy, manage_gap};

use super::accum;
use super::encode::LayerEnc;
use super::engine::Ctx;
use super::filter::prefilter_tile;
use super::geom::{get_mp_area, line_is_too_small, reduce_tiny_poly};
use super::read::{NO_LIST, ReadTile};
use super::util::*;
use crate::store::Attr;
use tippecanoe::plugin::filter_layers;

/// The thresholds for dropping features that a zoom level is using.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    pub gamma: f64,
    pub mingap: u64,
    pub minextent: i64,
    pub mindrop_sequence: u64,
    pub minattribute: f64,
}

/// Zoom-level values that making a tile needs.
pub struct MakeParams {
    pub maxzoom: i32,
    pub simplification: f64,
    pub th: Thresholds,
}

/// What making a tile produced: write_tile's return value and the values it
/// reports back through its `write_tile_args`.
pub struct MakeResult {
    /// write_tile's return: the number of vertices written, or -1 on failure.
    pub len: i64,
    /// The compressed tile, if one is written.
    pub tile: Option<Vec<u8>>,
    /// The strategy counts, if the tile succeeded (`strategy_out->add_from`).
    pub strategy: Option<Strategy>,
    pub th_out: Thresholds,
    pub tile_size_out: usize,
    pub feature_count_out: usize,
    pub still_dropping: bool,
    /// Attribute statistics added while making the tile.
    pub tilestats: BTreeMap<String, LayermapEntry>,
    /// Set where tippecanoe would give up on the whole run.
    pub error: Option<String>,
    /// The thresholds of the attempt that succeeded, if it was at full
    /// detail with no detail reduced before it, which is when the same
    /// attempt would come first in another pass.
    pub reusable: Option<Thresholds>,
    /// For `reusable`: the smallest gap of a kept candidate that is not
    /// below the gap threshold, and the smallest drop sequence above the
    /// drop sequence threshold. A higher threshold that stays at or below
    /// them leaves the tile as it is.
    pub reuse_gap: u64,
    pub reuse_drop_sequence: u64,
    /// With `-at`: the tile fit at the detail meant for overzooming, so its
    /// children are left out (tippecanoe's `skip_children_out`).
    pub truncated: bool,
    /// Whether the children may not try to stop early: the first attempt met
    /// a feature that first appears beyond the next zoom, or a feature waits
    /// for a deeper zoom.
    pub children_must_go_on: bool,
    /// With `-at`: the tile was skipped, not made.
    pub skipped: bool,
}

/// Whether a zoom level's thresholds drop features.
pub fn dropping_features(th: &Thresholds, drop_by_attribute_descending: bool) -> bool {
    let dropping_by_attribute = if drop_by_attribute_descending {
        th.minattribute != f64::INFINITY
    } else {
        th.minattribute != f64::NEG_INFINITY
    };
    th.mingap != 0 || th.minextent != 0 || th.mindrop_sequence != 0 || dropping_by_attribute
}

/// What only some kept features have, boxed so that the others spend 8
/// bytes on it.
#[derive(Clone, Debug, Default)]
struct Extra {
    /// The feature's attributes once something has changed them.
    attrs: Option<Vec<Attr>>,
    full_keys: Vec<Arc<String>>,
    full_values: Vec<SerialVal>,
    /// Accumulated numbers that `full_values` does not show yet.
    accs: Vec<accum::Acc>,
}

static NO_EXTRA: Extra = Extra {
    attrs: None,
    full_keys: Vec::new(),
    full_values: Vec::new(),
    accs: Vec::new(),
};

impl Extra {
    /// The extra part of a feature that arrives with full keys and values.
    fn from_full(
        (full_keys, full_values): (Vec<Arc<String>>, Vec<SerialVal>),
    ) -> Option<Box<Extra>> {
        (!full_keys.is_empty()).then(|| {
            Box::new(Extra {
                attrs: None,
                full_keys,
                full_values,
                accs: Vec::new(),
            })
        })
    }
}

/// A feature that went into the tile: tippecanoe's `serial_feature` during tiling.
#[derive(Clone, Debug)]
struct TF {
    cand: u32,
    fi: u32,
    t: i8,
    geometry: DrawVec,
    extra: Option<Box<Extra>>,
    /// Where among `full_keys` the cluster attributes go, or
    /// `NO_CLUSTER_ATTRS`. They follow from `clustered`, so they are made
    /// when they are read rather than kept for every feature.
    cluster_at: u32,
    extent: i64,
    index: u64,
    reduced: bool,
    coalesced: bool,
    clustered: u32,
    line_detail: i8,
    extra_detail: i8,
    spacing: f64,
    simplification: f64,
}

// A tile being made holds up to hundreds of thousands of these.
const _: () = assert!(std::mem::size_of::<TF>() <= 88);

const NO_CLUSTER_ATTRS: u32 = u32::MAX;

/// The attributes tippecanoe gives a feature that others were clustered
/// onto, in its order.
const CLUSTER_KEYS: [&str; 4] = [
    "clustered",
    "point_count",
    "sqrt_point_count",
    "point_count_abbreviated",
];

/// Cluster attribute `k` of a feature that `clustered` others joined.
fn cluster_value(clustered: i64, k: usize) -> SerialVal {
    let point_count = clustered + 1;
    match k {
        0 => SerialVal::new(MVT_BOOL, "true"),
        1 => SerialVal::new(MVT_DOUBLE, &point_count.to_string()),
        2 => SerialVal::new(
            MVT_DOUBLE,
            &format!(
                "{:.6}",
                (100.0 * (point_count as f64).sqrt()).round() / 100.0
            ),
        ),
        _ => {
            let abbrev = if point_count >= 10000 {
                format!("{:.0}k", point_count as f64 / 1000.0)
            } else if point_count >= 1000 {
                format!("{:.1}k", point_count as f64 / 1000.0)
            } else {
                format!("{}", point_count)
            };
            SerialVal::new(MVT_STRING, &abbrev)
        }
    }
}

impl TF {
    fn extra(&self) -> &Extra {
        self.extra.as_deref().unwrap_or(&NO_EXTRA)
    }

    fn extra_mut(&mut self) -> &mut Extra {
        self.extra.get_or_insert_with(Default::default)
    }

    /// How many attributes `full_keys` and the cluster attributes make.
    fn full_len(&self) -> usize {
        self.extra().full_keys.len()
            + if self.cluster_at == NO_CLUSTER_ATTRS {
                0
            } else {
                4
            }
    }

    /// Attribute `i` of those, with the cluster attributes where they go.
    fn full_at(&self, i: usize) -> (&str, Cow<'_, SerialVal>) {
        let at = self.cluster_at as usize;
        let x = self.extra();
        if self.cluster_at == NO_CLUSTER_ATTRS || i < at {
            (&x.full_keys[i], Cow::Borrowed(&x.full_values[i]))
        } else if i < at + 4 {
            (
                CLUSTER_KEYS[i - at],
                Cow::Owned(cluster_value(self.clustered as i64, i - at)),
            )
        } else {
            (&x.full_keys[i - 4], Cow::Borrowed(&x.full_values[i - 4]))
        }
    }
}

#[derive(Default)]
struct LayerFeatures {
    features: Vec<TF>,
    multiplier_cluster_size: usize,
}

/// Per-tile view of the attributes of features.
struct Attrs<'a> {
    ctx: &'a Ctx<'a>,
    rt: &'a ReadTile,
}

impl<'a> Attrs<'a> {
    /// The attributes of candidate `i`.
    fn cand(&self, i: usize) -> &'a [Attr] {
        let attrs = self.rt.extra(i).attrs;
        if attrs != NO_LIST {
            &self.rt.attr_lists[attrs as usize]
        } else {
            let f = &self.ctx.store.features[self.rt.cands[i].fi as usize];
            self.ctx.store.feature_attrs(f)
        }
    }

    /// The attributes of a kept feature, nulls included (callers skip them).
    fn of<'b>(&self, tf: &'b TF) -> &'b [Attr]
    where
        'a: 'b,
    {
        match &tf.extra().attrs {
            Some(a) => a,
            None => self.cand(tf.cand as usize),
        }
    }

    fn live(&self, a: &Attr) -> bool {
        !self.ctx.value_null[a.value as usize]
    }

    fn key(&self, a: &Attr) -> &'a str {
        &self.ctx.store.keys[a.key as usize]
    }

    fn sval(&self, a: &Attr) -> SerialVal {
        self.ctx.store.values.get(a.value).to_serial()
    }

    /// The attributes without nulls, materialized so they can be changed.
    fn materialize(&self, tf: &mut TF) {
        if tf.extra().attrs.is_none() {
            let list: Vec<Attr> = self
                .of(tf)
                .iter()
                .filter(|a| self.live(a))
                .copied()
                .collect();
            tf.extra_mut().attrs = Some(list);
        }
    }
}

/// `promote_attribute`: if the feature being merged into has this key as an
/// attribute, makes it a full key so that it can be changed.
/// `key_id` is the store's id of `key`, if it has one.
fn promote_attribute(
    ax: &Attrs<'_>,
    key: &str,
    key_id: Option<u32>,
    p: &mut TF,
    key_pool: &mut KeyPool,
) {
    let found = ax
        .of(p)
        .iter()
        .filter(|a| ax.live(a))
        .position(|a| match key_id {
            Some(k) => a.key == k,
            None => ax.key(a) == key,
        });
    if let Some(i) = found {
        ax.materialize(p);
        let x = p.extra_mut();
        let a = x.attrs.as_mut().unwrap().remove(i);
        accum::push_one(&mut x.full_keys, key_pool.pool(key));
        accum::push_one(&mut x.full_values, ax.sval(&a));
    }
}

/// `preserve_attributes`: accumulates the attributes of the dropped candidate onto `p`.
fn preserve_attributes(ax: &Attrs<'_>, c: usize, p: &mut TF, key_pool: &mut KeyPool) {
    if !ax.ctx.o.accumulates() {
        return;
    }
    for a in ax.cand(c) {
        let Some(op) = &ax.ctx.key_accum[a.key as usize] else {
            continue;
        };
        if !ax.live(a) {
            continue;
        }
        let key = ax.key(a);
        let v = ax.ctx.store.values.get(a.value);
        promote_attribute(ax, key, Some(a.key), p, key_pool);
        let x = p.extra_mut();
        accum::preserve(
            op,
            key,
            v.type_,
            v.s,
            ax.ctx.value_num[a.value as usize],
            &mut x.full_keys,
            &mut x.full_values,
            &mut x.accs,
            key_pool,
        );
    }
    let full = ax.rt.extra(c).full;
    if full != NO_LIST {
        let (keys, values) = &ax.rt.full[full as usize];
        for (key, sv) in keys.iter().zip(values.iter()) {
            if let Some(op) = ax.ctx.o.accumulate_op(key.as_str()) {
                promote_attribute(ax, key, None, p, key_pool);
                let x = p.extra_mut();
                accum::preserve(
                    &op,
                    key,
                    sv.type_,
                    &sv.s,
                    accum::to_double(&sv.s),
                    &mut x.full_keys,
                    &mut x.full_values,
                    &mut x.accs,
                    key_pool,
                );
            }
        }
    }
}

/// The full keys and values a candidate arrives with, from the prefilter.
fn cand_full(rt: &ReadTile, c: usize) -> (Vec<Arc<String>>, Vec<SerialVal>) {
    let full = rt.extra(c).full;
    if full == NO_LIST {
        (Vec::new(), Vec::new())
    } else {
        rt.full[full as usize].clone()
    }
}

/// `find_feature_to_accumulate_onto`. The features of a layer all have the
/// layer's name, so only the type and extent decide.
fn find_feature_to_accumulate_onto(features: &[TF], t: i8, maxextent: i64) -> Option<usize> {
    (0..features.len())
        .rev()
        .find(|&i| features[i].t == t && features[i].extent <= maxextent)
}

fn sign(c: std::cmp::Ordering) -> i32 {
    match c {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `metacmp`: compares the attributes of two features.
fn metacmp(ax: &Attrs<'_>, one: &TF, two: &TF) -> i32 {
    let a1: Vec<&Attr> = ax.of(one).iter().filter(|a| ax.live(a)).collect();
    let a2: Vec<&Attr> = ax.of(two).iter().filter(|a| ax.live(a)).collect();
    if a1.len() < a2.len() {
        return -1;
    } else if a1.len() > a2.len() {
        return 1;
    }

    for (x, y) in a1.iter().zip(a2.iter()) {
        let cmp = sign(ax.key(x).as_bytes().cmp(ax.key(y).as_bytes()));
        if cmp != 0 {
            return cmp;
        }

        let v1 = ax.sval(x);
        let v2 = ax.sval(y);
        if v1.type_ != v2.type_ {
            return v1.type_ - v2.type_;
        }
        let cmp = sign(cstr(&v1.s).cmp(cstr(&v2.s)));
        if cmp != 0 {
            return cmp;
        }
    }

    0
}

/// The bytes of a string up to its first NUL, as `strcmp` sees it.
fn cstr(s: &str) -> &[u8] {
    let b = s.as_bytes();
    match b.iter().position(|&c| c == 0) {
        Some(n) => &b[..n],
        None => b,
    }
}

/// `coalcmp`: whether two features can be coalesced.
fn coalcmp(ax: &Attrs<'_>, c1: &TF, c2: &TF) -> i32 {
    let mut cmp = c1.t as i32 - c2.t as i32;
    if cmp != 0 {
        return cmp;
    }

    let store = &ax.ctx.store;
    let f1 = &store.features[c1.fi as usize];
    let f2 = &store.features[c2.fi as usize];
    if f1.has_id != f2.has_id {
        return f1.has_id as i32 - f2.has_id as i32;
    }

    if f1.has_id && f2.has_id {
        let (id1, id2) = (store.id(c1.fi as usize), store.id(c2.fi as usize));
        if id1 < id2 {
            return -1;
        }
        if id1 > id2 {
            return 1;
        }
    }

    cmp = metacmp(ax, c1, c2);
    if cmp != 0 {
        return cmp;
    }

    if c1.full_len() < c2.full_len() {
        return -1;
    } else if c1.full_len() > c2.full_len() {
        return 1;
    }

    for i in 0..c1.full_len() {
        let ((k1, v1), (k2, v2)) = (c1.full_at(i), c2.full_at(i));
        if k1 < k2 {
            return -1;
        } else if k1 > k2 {
            return 1;
        }

        if v1.type_ < v2.type_ {
            return -1;
        } else if v1.type_ > v2.type_ {
            return 1;
        }

        if v1.s < v2.s {
            return -1;
        } else if v1.s > v2.s {
            return 1;
        }
    }

    0
}

fn coalindexcmp(ax: &Attrs<'_>, c1: &TF, c2: &TF) -> i32 {
    let cmp = coalcmp(ax, c1, c2);

    if cmp == 0 {
        if c1.index < c2.index {
            return -1;
        } else if c1.index > c2.index {
            return 1;
        }

        if c1.geometry < c2.geometry {
            return -1;
        } else if c1.geometry > c2.geometry {
            return 1;
        }
    }

    cmp
}

/// `find_attribute_value`, for `--order-by` and `--drop-by-attribute-as-needed`.
fn find_attribute_value(ax: &Attrs<'_>, tf: &TF, key: &str) -> MvtValue {
    if key == ORDER_BY_SIZE {
        let mut v = MvtValue {
            type_: MVT_DOUBLE,
            ..Default::default()
        };
        v.numeric_value.set_double_value(tf.extent as f64);
        return v;
    }

    for a in ax.of(tf) {
        if ax.live(a) && ax.key(a) == key {
            return ax.ctx.value(a.value);
        }
    }

    for i in 0..tf.full_len() {
        let (k, v) = tf.full_at(i);
        if k == key {
            return stringified_to_mvt_value(v.type_, &v.s);
        }
    }

    let mut v = MvtValue {
        type_: MVT_NULL,
        ..Default::default()
    };
    v.numeric_value.set_null_value(0);
    v
}

fn ordercmp(ax: &Attrs<'_>, order_by: &[OrderField], a: &TF, b: &TF) -> bool {
    for field in order_by {
        let v1 = coerce_double(find_attribute_value(ax, a, &field.name));
        let v2 = coerce_double(find_attribute_value(ax, b, &field.name));

        if field.descending {
            if v2 < v1 {
                return true;
            } else if v1 < v2 {
                return false;
            }
        } else if v1 < v2 {
            return true;
        } else if v2 < v1 {
            return false;
        }
    }

    a.index < b.index
}

const MULTIPLIER_FIRST: &str = "tippecanoe:retain_points_multiplier_first";

fn assemble_multiplier_clusters(rpm: i32, features: Vec<TF>) -> Vec<Vec<TF>> {
    let mut clusters: Vec<Vec<TF>> = Vec::new();

    if rpm == 1 {
        for feature in features {
            clusters.push(vec![feature]);
        }
    } else {
        for feature in features {
            let is_cluster_start = feature
                .extra()
                .full_keys
                .iter()
                .any(|k| **k == MULTIPLIER_FIRST);

            if is_cluster_start || clusters.is_empty() {
                clusters.push(Vec::new());
            }

            clusters.last_mut().unwrap().push(feature);
        }
    }

    clusters
}

fn disassemble_multiplier_clusters(ctx: &Ctx<'_>, clusters: Vec<Vec<TF>>) -> Vec<TF> {
    let mut out = Vec::new();
    let drop_sequence = |tf: &TF| {
        calculate_drop_sequence(ctx.store.features[tf.fi as usize].feature_minzoom, tf.index)
    };

    for mut cluster in clusters {
        'outer: for i in 0..cluster.len() {
            for j in 0..cluster[i].extra().full_keys.len() {
                if *cluster[i].extra().full_keys[j] == MULTIPLIER_FIRST {
                    let x = cluster[i].extra_mut();
                    let k = x.full_keys.remove(j);
                    let v = x.full_values.remove(j);
                    let at = &mut cluster[i].cluster_at;
                    if *at != NO_CLUSTER_ATTRS && (j as u32) < *at {
                        *at -= 1;
                    }

                    let x = cluster[0].extra_mut();
                    x.full_keys.push(k);
                    x.full_values.push(v);

                    break 'outer;
                }
            }
        }

        if cluster.len() > 1 {
            cluster[1..].sort_by(by_less(|a: &TF, b: &TF| {
                drop_sequence(a) > drop_sequence(b)
            }));
        }

        out.extend(cluster);
    }

    out
}

/// `simplify_feature`. Returns the area of a polygon before simplification.
#[allow(clippy::too_many_arguments)]
fn simplify_feature(
    ctx: &Ctx<'_>,
    p: &mut TF,
    z: i32,
    tx: u32,
    ty: u32,
    maxzoom: i32,
    shared_nodes: &DrawVec,
) -> f64 {
    let o = ctx.o;
    let mut geom = std::mem::take(&mut p.geometry);
    let t = p.t as i32;
    let line_detail = p.line_detail as i32;

    if o.a(A_GRID_LOW_ZOOMS) && z < maxzoom {
        geom = stairstep(&mut geom, z, line_detail);
    }

    let mut area = 0.0;
    if t == VT_POLYGON {
        area = get_mp_area_vec(&geom);
    }

    if (t == VT_LINE || t == VT_POLYGON)
        && !(o.p(P_SIMPLIFY)
            || (z == maxzoom && o.p(P_SIMPLIFY_LOW))
            || (z < maxzoom && o.a(A_GRID_LOW_ZOOMS)))
        && !p.reduced
    {
        if t == VT_LINE && !o.p(P_SIMPLIFY_SHARED_NODES) {
            geom = remove_noop(geom, t, 32 - z - line_detail);
        }

        let already_marked = o.a(A_DETECT_SHARED_BORDERS) && t == VT_POLYGON;

        if !already_marked {
            if p.coalesced && t == VT_POLYGON {
                coalesce_polygon(&mut geom, false);
            }

            let mark = !(o.p(P_CLIPPING) || o.p(P_DUPLICATION));
            let retain = if t == VT_POLYGON { 4 } else { 0 };
            let mut ngeom = simplify_lines(
                &mut geom,
                z,
                tx as i32,
                ty as i32,
                line_detail,
                mark,
                p.simplification,
                retain,
                shared_nodes,
                &o.shared_nodes,
                o.shared_nodes.len() * std::mem::size_of::<tippecanoe::serial::Node>(),
                &o.shared_nodes_bloom,
            );

            if p.coalesced && o.p(P_SIMPLIFY_SHARED_NODES) {
                ngeom = simplify_lines(
                    &mut ngeom,
                    z,
                    tx as i32,
                    ty as i32,
                    line_detail,
                    mark,
                    0.1,
                    retain,
                    shared_nodes,
                    &[],
                    0,
                    &[],
                );
            }

            if t != VT_POLYGON || ngeom.len() >= 3 {
                geom = ngeom;
            }
        }
    }

    if t == VT_LINE && o.a(A_REVERSE) {
        geom = remove_noop(geom, t, 0);
        geom = reorder_lines(&geom);
    }

    p.geometry = geom;
    area
}

/// `simplification_worker` for one feature.
#[allow(clippy::too_many_arguments)]
fn simplify_and_scale(
    ctx: &Ctx<'_>,
    i: usize,
    feature: &mut TF,
    z: i32,
    tx: u32,
    ty: u32,
    maxzoom: i32,
    shared_nodes: &DrawVec,
) {
    let area = simplify_feature(ctx, feature, z, tx, ty, maxzoom, shared_nodes);

    let t = feature.t as i32;
    let out_detail = feature.extra_detail as i32;

    let mut geom = std::mem::take(&mut feature.geometry);
    to_tile_scale(&mut geom, z, out_detail);

    if t == VT_POLYGON {
        let before = geom.clone();

        coalesce_polygon(&mut geom, true);
        if ctx.o.a(A_DEBUG_POLYGON) {
            check_polygon(&mut geom);
        }

        if geom.len() < 3 {
            if area > 0.0 {
                geom = revive_polygon(&before, area, z, out_detail);
            } else {
                geom.clear();
            }
        }
    }

    if feature.index == 0 {
        feature.index = i as u64;
    }
    feature.geometry = geom;
}

/// Where the features of a layer go while the tile is built: the encoder, or
/// for a postfilter, tippecanoe's tile layers.
trait Tagger {
    fn feature(&mut self, t: i32, has_id: bool, id: u64, geom: usize, geometry: &DrawVec);
    fn tag_store(&mut self, key: &str, slot: u32, value: u32, v: &dyn Fn() -> MvtValue);
    fn tag_extra(&mut self, key: &str, v: &MvtValue);
    fn end_feature(&mut self);
}

impl Tagger for LayerEnc<'_> {
    fn feature(&mut self, t: i32, has_id: bool, id: u64, geom: usize, _geometry: &DrawVec) {
        LayerEnc::feature(self, t, has_id, id, geom);
    }

    fn tag_store(&mut self, key: &str, slot: u32, value: u32, _v: &dyn Fn() -> MvtValue) {
        LayerEnc::tag_store(self, key, slot, value);
    }

    fn tag_extra(&mut self, key: &str, v: &MvtValue) {
        LayerEnc::tag_extra(self, key, v);
    }

    fn end_feature(&mut self) {}
}

struct MvtBuilder {
    layer: MvtLayer,
    current: Option<MvtFeature>,
}

impl MvtBuilder {
    fn new(name: &str, extent: i64) -> MvtBuilder {
        MvtBuilder {
            layer: MvtLayer {
                name: name.to_string(),
                version: 2,
                extent,
                ..Default::default()
            },
            current: None,
        }
    }
}

impl Tagger for MvtBuilder {
    fn feature(&mut self, t: i32, has_id: bool, id: u64, _geom: usize, geometry: &DrawVec) {
        self.current = Some(MvtFeature {
            type_: t,
            geometry: geometry
                .iter()
                .map(|g| MvtGeometry::new(g.op as i32, g.x, g.y))
                .collect(),
            id,
            has_id,
            ..Default::default()
        });
    }

    fn tag_store(&mut self, key: &str, _slot: u32, _value: u32, v: &dyn Fn() -> MvtValue) {
        let feature = self.current.as_mut().unwrap();
        self.layer.tag(feature, key, &v());
    }

    fn tag_extra(&mut self, key: &str, v: &MvtValue) {
        let feature = self.current.as_mut().unwrap();
        self.layer.tag(feature, key, v);
    }

    fn end_feature(&mut self) {
        if let Some(f) = self.current.take() {
            self.layer.features.push(f);
        }
    }
}

/// `add_tilestats`, into the tile's own layer map.
fn add_tilestats(
    tilestats: &mut BTreeMap<String, LayermapEntry>,
    unmap: &mut Vec<String>,
    layername: &str,
    z: i32,
    key: &str,
    val: &SerialVal,
) {
    if !tilestats.contains_key(layername) {
        let mut lme = LayermapEntry::new(tilestats.len());
        lme.minzoom = z;
        lme.maxzoom = z;
        lme.retain = 1;
        let id = lme.id;
        tilestats.insert(layername.to_string(), lme);
        if id >= unmap.len() {
            unmap.resize(id + 1, String::new());
            unmap[id] = layername.to_string();
        }
    }
    let ts = tilestats.get_mut(layername).unwrap();
    add_to_tilestats(&mut ts.tilestats, key, val);
}

fn gzip(ctx: &Ctx<'_>, data: &[u8], libdeflate: bool) -> Vec<u8> {
    if !ctx.o.timing {
        return crate::compress::gzip(data, libdeflate);
    }
    use std::sync::atomic::Ordering::Relaxed;
    let t = std::time::Instant::now();
    let out = crate::compress::gzip(data, libdeflate);
    let c = &ctx.counters;
    c[1].fetch_add(1, Relaxed);
    c[2].fetch_add(data.len() as u64, Relaxed);
    c[3].fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
    out
}

/// Makes the tile that `rt` was read for, as tippecanoe's `write_tile` does.
pub fn make_tile(octx: &Ctx<'_>, ort: &ReadTile, mp: &MakeParams) -> MakeResult {
    let o = octx.o;
    let z = ort.z;
    let tx = ort.x;
    let ty = ort.y;
    let maxzoom = mp.maxzoom;
    let simplification = mp.simplification;
    let detail = if z == maxzoom {
        o.full_detail
    } else {
        o.low_detail
    };
    let min_detail = o.min_detail;
    let quiet = o.quiet;

    let mut prefilter = o.prefilter.as_deref();
    let mut postfilter = o.postfilter.as_deref();
    if z < o.minzoom {
        prefilter = None;
        postfilter = None;
    }
    // the names of the layers in res.tilestats, by id, for the filters
    let mut unmap: Vec<String> = Vec::new();

    let mut gamma = mp.th.gamma;
    let mut mingap = mp.th.mingap;
    let mut minextent = mp.th.minextent;
    let mut mindrop_sequence = mp.th.mindrop_sequence;
    let mut minattribute = mp.th.minattribute;

    let mut res = MakeResult {
        len: -1,
        tile: None,
        strategy: None,
        th_out: mp.th,
        tile_size_out: 0,
        feature_count_out: 0,
        still_dropping: false,
        tilestats: BTreeMap::new(),
        error: None,
        reusable: None,
        reuse_gap: u64::MAX,
        reuse_drop_sequence: u64::MAX,
        truncated: false,
        children_must_go_on: false,
        skipped: false,
    };

    let retain_points_multiplier = o.retain_points_multiplier;
    let max_tile_size = o.max_tile_size;
    let max_tile_features = o.max_tile_features;
    let cluster_distance = o.cluster_distance;
    let cluster_maxzoom = o.cluster_maxzoom;
    let drop_by_attribute = o.drop_by_attribute.as_deref().unwrap_or("");
    let drop_by_attribute_descending = o.drop_by_attribute_descending;

    let merge_fraction = 1.0;
    let mut mingap_fraction = 1.0;
    let mut minextent_fraction = 1.0;
    let mut mindrop_sequence_fraction = 1.0;
    let mut minattribute_fraction = 1.0;

    // only for -K
    let cluster_mingap: u64 = (((1i64 << (32 - z)) / 256 * cluster_distance as i64)
        .wrapping_mul((1i64 << (32 - z)) / 256 * cluster_distance as i64))
        as u64;

    let mut first_detail = detail;
    let mut second_detail = detail - 1;
    let mut trying_to_stop_early = false;
    let mut can_stop_early = true;
    let variable_depth = o.a(A_VARIABLE_DEPTH_PYRAMID);
    if variable_depth && !dropping_features(&mp.th, drop_by_attribute_descending) {
        // tippecanoe's estimate, with the multiply and add fused as clang does
        let estimated_output_tile_size = (ort.complexity as f64).mul_add(0.6693, -3.36e+04) as i64;
        if estimated_output_tile_size < (0.9 * max_tile_size as f64) as i64 && 30 - z > detail {
            first_detail = 30 - z;
            second_detail = detail;
            trying_to_stop_early = true;
        }
    }

    let mut detail_reduced: usize = 0;
    let next_detail = |line_detail: i32| {
        if line_detail == first_detail {
            second_detail
        } else {
            line_detail - 1
        }
    };

    let mark = !(o.p(P_CLIPPING) || o.p(P_DUPLICATION));

    macro_rules! reusable_at {
        ($line_detail:expr, $detail_reduced:expr) => {
            if $line_detail == first_detail && $detail_reduced == 0 && !variable_depth {
                Some(Thresholds {
                    gamma,
                    mingap,
                    minextent,
                    mindrop_sequence,
                    minattribute,
                })
            } else {
                None
            }
        };
    }

    let mut line_detail = first_detail;
    let mut first_time = true;
    while line_detail >= min_detail || line_detail == detail {
        if octx.failed.load(std::sync::atomic::Ordering::Relaxed) {
            res.len = -1;
            return res;
        }
        octx.counters[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let trace_start = std::time::Instant::now();

        let mut key_pool = KeyPool::default();
        let prefiltered;
        let pctx;
        let (ctx, rt): (&Ctx<'_>, &ReadTile) = match prefilter {
            Some(filter) => {
                prefiltered = prefilter_tile(
                    octx,
                    ort,
                    filter,
                    &mut res.tilestats,
                    &mut unmap,
                    postfilter.is_some(),
                    &mut key_pool,
                );
                pctx = Ctx::new(&prefiltered.store, o, octx.helpers.clone());
                (&pctx, &prefiltered.rt)
            }
            None => (octx, ort),
        };
        let store = ctx.store;
        let ax = Attrs { ctx, rt };

        let mut count: i64 = 0;
        let mut accum_area: f64 = 0.0;

        let mut previndex: u64 = 0;
        let mut density_previndex: u64 = 0;
        let mut merge_previndex: u64 = 0;
        let mut extent_previndex: u64 = 0;
        let scale = 1i64.wrapping_shl((64 - 2 * (z + 8)) as u32) as f64;
        let mut gap: f64 = 0.0;
        let mut density_gap: f64 = 0.0;
        let mut spacing: f64 = 0.0;

        let mut layers: Vec<Option<LayerFeatures>> = Vec::new();
        layers.resize_with(store.layers.len(), || None);

        let mut gaps: Vec<u64> = Vec::new();
        let mut gaps_increment: usize = 1;
        let mut extents: Vec<i64> = Vec::new();
        let mut extents_increment: usize = 1;
        let mut drop_sequences: Vec<u64> = Vec::new();
        let mut drop_sequences_increment: usize = 1;
        let mut attribute_values: Vec<f64> = Vec::new();
        let mut attribute_values_increment: usize = 1;

        let mut coalesced_area: f64 = 0.0;
        let mut shared_nodes = DrawVec::new();

        let mut tile_detail = line_detail;
        let mut skipped: usize = 0;
        let mut kept: usize = 0;

        let mut unsimplified_geometry_size: usize = 0;
        let mut simplified_geometry_through: usize = 0;

        let mut lead_features_count: usize = 0;
        let mut other_multiplier_cluster_features_count: usize = 0;

        let mut too_many_features = false;
        let mut too_many_bytes = false;

        let mut drop_rest = false;
        let mut dropping_by_rate = false;

        let mut strategy = Strategy {
            detail_reduced,
            ..Default::default()
        };

        let mut scratch = DrawVec::new();
        for (seq, c) in rt.cands.iter().enumerate() {
            let f = &store.features[c.fi as usize];
            let cg: &[tippecanoe::geometry::Draw] = rt.cand_geom(c, &mut scratch);
            let mut t = f.t;
            let mut extent = store.extent(c.fi as usize);
            let mut dropped = c.dropped;
            // the feature's geometry, once something has changed it
            let mut geom: Option<DrawVec> = None;

            let layer: &mut LayerFeatures =
                layers[f.layer as usize].get_or_insert_with(LayerFeatures::default);

            if t as i32 == VT_POINT {
                if extent_previndex >= f.index() {
                    extent = 1;
                } else {
                    let radius = ((f.index() - extent_previndex) as f64).sqrt() / 4.0;
                    extent = (std::f64::consts::PI * radius * radius) as i64;
                    if extent < 1 {
                        extent = 1;
                    }
                }

                extent_previndex = f.index();
            }

            if t as i32 == VT_POLYGON && o.a(A_GENERATE_POLYGON_LABEL_POINTS) {
                if line_is_too_small(cg, z, line_detail) {
                    continue;
                }
                if get_mp_area(cg) <= 0.0 {
                    continue;
                }

                let mut g: DrawVec = cg.to_vec();
                let ngeom = simplify_lines(
                    &mut g,
                    z,
                    tx as i32,
                    ty as i32,
                    line_detail,
                    mark,
                    0.0,
                    4,
                    &shared_nodes,
                    &[],
                    0,
                    &[],
                );
                if ngeom.is_empty() {
                    continue;
                }
                let anchors = checkerboard_anchors(
                    &ngeom,
                    tx as i32,
                    ty as i32,
                    z,
                    store.label_point(c.fi as usize),
                );
                if anchors.is_empty() {
                    continue;
                }
                geom = Some(anchors);
                t = VT_POINT as i8;
            }

            let mut drop_sequence: u64 = 0;
            if o.a(A_COALESCE_FRACTION_AS_NEEDED)
                || o.a(A_DROP_FRACTION_AS_NEEDED)
                || o.p(P_DYNAMIC_DROP)
            {
                drop_sequence = calculate_drop_sequence(f.feature_minzoom, f.index());
            }

            if f.feature_minzoom as i32 > z + 1 {
                dropping_by_rate = true;
            }

            if dropped == FEATURE_KEPT {
                drop_rest = false;
            } else {
                can_stop_early = false;
                if dropped != FEATURE_DROPPED
                    && dropped != FEATURE_ADDED_FOR_MULTIPLIER_DENSITY
                    && layer.multiplier_cluster_size >= retain_points_multiplier as usize
                {
                    dropped = FEATURE_DROPPED;
                }
            }

            macro_rules! sf_geom {
                () => {
                    match &geom {
                        Some(g) => g.as_slice(),
                        None => cg,
                    }
                };
            }

            if (dropped == FEATURE_DROPPED || drop_rest)
                && let Some(w) = find_feature_to_accumulate_onto(&layer.features, t, i64::MAX)
            {
                preserve_attributes(&ax, seq, &mut layer.features[w], &mut key_pool);
                strategy.dropped_by_rate += 1;
                can_stop_early = false;
                continue;
            }

            // drop_feature_unless_it_can_be_added_to_a_multiplier_cluster
            macro_rules! drop_unless_multiplier {
                () => {{
                    can_stop_early = false;
                    if let Some(w) = find_feature_to_accumulate_onto(&layer.features, t, i64::MAX) {
                        strategy.dropped_as_needed += 1;
                        if layer.multiplier_cluster_size < retain_points_multiplier as usize {
                            dropped = (layer.multiplier_cluster_size + 1) as i32;
                            false
                        } else {
                            preserve_attributes(&ax, seq, &mut layer.features[w], &mut key_pool);
                            drop_rest = true;
                            true
                        }
                    } else {
                        false
                    }
                }};
            }

            macro_rules! cluster_onto {
                ($w:expr, $average:expr) => {{
                    let target = &mut layer.features[$w];
                    target.clustered += 1;

                    let g = sf_geom!();
                    if $average
                        && target.t as i32 == VT_POINT
                        && target.geometry.len() == 1
                        && g.len() == 1
                    {
                        let mut x = target.geometry[0].x as f64 * target.clustered as f64;
                        let mut y = target.geometry[0].y as f64 * target.clustered as f64;
                        x += g[0].x as f64;
                        y += g[0].y as f64;
                        target.geometry[0].x = (x / (target.clustered + 1) as f64) as i64;
                        target.geometry[0].y = (y / (target.clustered + 1) as f64) as i64;
                    }

                    preserve_attributes(&ax, seq, target, &mut key_pool);
                    strategy.coalesced_as_needed += 1;
                    drop_rest = true;
                    can_stop_early = false;
                }};
            }

            macro_rules! coalesce_onto {
                ($w:expr, $add_area:expr) => {{
                    let g = sf_geom!();
                    let target = &mut layer.features[$w];
                    if t as i32 == VT_POINT || !line_is_too_small(g, z, line_detail) {
                        target.geometry.extend_from_slice(g);
                    }
                    target.coalesced = true;
                    if $add_area {
                        coalesced_area += extent as f64;
                    }
                    preserve_attributes(&ax, seq, target, &mut key_pool);
                    strategy.coalesced_as_needed += 1;
                    drop_rest = true;
                    can_stop_early = false;
                }};
            }

            if dropped == FEATURE_KEPT {
                if gamma > 0.0
                    && manage_gap(f.index(), &mut previndex, scale, gamma, &mut gap) != 0
                    && let Some(w) = find_feature_to_accumulate_onto(&layer.features, t, i64::MAX)
                {
                    preserve_attributes(&ax, seq, &mut layer.features[w], &mut key_pool);
                    strategy.dropped_by_gamma += 1;
                    drop_rest = true;
                    can_stop_early = false;
                    continue;
                }

                if z <= cluster_maxzoom && cluster_distance != 0 {
                    if (f.index() < merge_previndex || f.index() - merge_previndex < cluster_mingap)
                        && let Some(w) =
                            find_feature_to_accumulate_onto(&layer.features, t, i64::MAX)
                    {
                        cluster_onto!(w, !o.a(A_KEEP_POINT_CLUSTER_POSITION));
                        continue;
                    }
                } else if o.a(A_DROP_DENSEST_AS_NEEDED) {
                    let gap = rt.gap(seq);
                    add_sample_to(&mut gaps, gap, &mut gaps_increment, seq);
                    if gap < mingap && drop_unless_multiplier!() {
                        continue;
                    }
                } else if z <= cluster_maxzoom && o.a(A_CLUSTER_DENSEST_AS_NEEDED) {
                    let gap = rt.gap(seq);
                    add_sample_to(&mut gaps, gap, &mut gaps_increment, seq);
                    if gap < mingap
                        && let Some(w) =
                            find_feature_to_accumulate_onto(&layer.features, t, i64::MAX)
                    {
                        cluster_onto!(w, true);
                        continue;
                    }
                } else if o.a(A_COALESCE_DENSEST_AS_NEEDED) {
                    let gap = rt.gap(seq);
                    add_sample_to(&mut gaps, gap, &mut gaps_increment, seq);
                    if gap < mingap
                        && let Some(w) =
                            find_feature_to_accumulate_onto(&layer.features, t, i64::MAX)
                    {
                        coalesce_onto!(w, true);
                        continue;
                    }
                } else if o.a(A_DROP_SMALLEST_AS_NEEDED) {
                    add_sample_to(&mut extents, extent, &mut extents_increment, seq);
                    if minextent != 0
                        && extent as f64 + coalesced_area <= minextent as f64
                        && drop_unless_multiplier!()
                    {
                        continue;
                    }
                } else if o.a(A_COALESCE_SMALLEST_AS_NEEDED) {
                    add_sample_to(&mut extents, extent, &mut extents_increment, seq);
                    if minextent != 0
                        && extent as f64 + coalesced_area <= minextent as f64
                        && let Some(w) =
                            find_feature_to_accumulate_onto(&layer.features, t, minextent)
                    {
                        coalesce_onto!(w, true);
                        continue;
                    }
                } else if o.a(A_DROP_FRACTION_AS_NEEDED) || o.p(P_DYNAMIC_DROP) {
                    add_sample_to(
                        &mut drop_sequences,
                        drop_sequence,
                        &mut drop_sequences_increment,
                        seq,
                    );
                    if mindrop_sequence != 0
                        && drop_sequence <= mindrop_sequence
                        && drop_unless_multiplier!()
                    {
                        continue;
                    }
                } else if o.a(A_COALESCE_FRACTION_AS_NEEDED) {
                    add_sample_to(
                        &mut drop_sequences,
                        drop_sequence,
                        &mut drop_sequences_increment,
                        seq,
                    );
                    if mindrop_sequence != 0
                        && drop_sequence <= mindrop_sequence
                        && let Some(w) =
                            find_feature_to_accumulate_onto(&layer.features, t, i64::MAX)
                    {
                        coalesce_onto!(w, false);
                        continue;
                    }
                } else if o.a(A_DROP_BY_ATTRIBUTE_AS_NEEDED) {
                    let full = cand_full(rt, seq);
                    let probe = TF {
                        cand: seq as u32,
                        fi: c.fi,
                        t,
                        geometry: DrawVec::new(),
                        extra: Extra::from_full(full),
                        cluster_at: NO_CLUSTER_ATTRS,
                        extent,
                        index: f.index(),
                        reduced: false,
                        coalesced: false,
                        clustered: 0,
                        line_detail: line_detail as i8,
                        extra_detail: line_detail as i8,
                        spacing,
                        simplification,
                    };
                    let attr_val = find_attribute_value(&ax, &probe, drop_by_attribute);
                    let mut attr_numeric: f64 = 0.0;
                    let mut attr_valid = false;

                    if attr_val.type_ == MVT_DOUBLE {
                        attr_numeric = attr_val.numeric_value.double_value();
                        attr_valid = attr_numeric.is_finite();
                    } else if attr_val.type_ == MVT_FLOAT {
                        attr_numeric = attr_val.numeric_value.float_value() as f64;
                        attr_valid = attr_numeric.is_finite();
                    } else if attr_val.type_ == MVT_INT {
                        attr_numeric = attr_val.numeric_value.int_value() as f64;
                        attr_valid = true;
                    } else if attr_val.type_ == MVT_UINT {
                        attr_numeric = attr_val.numeric_value.uint_value() as f64;
                        attr_valid = true;
                    } else if attr_val.type_ == MVT_SINT {
                        attr_numeric = attr_val.numeric_value.sint_value() as f64;
                        attr_valid = true;
                    }

                    if attr_valid {
                        add_sample_to(
                            &mut attribute_values,
                            attr_numeric,
                            &mut attribute_values_increment,
                            seq,
                        );
                        let should_drop = if drop_by_attribute_descending {
                            minattribute != f64::INFINITY && attr_numeric > minattribute
                        } else {
                            minattribute != f64::NEG_INFINITY && attr_numeric < minattribute
                        };
                        if should_drop && drop_unless_multiplier!() {
                            continue;
                        }
                    }
                }
            }

            if o.a(A_CALCULATE_FEATURE_DENSITY) {
                let o_density_previndex = density_previndex as f64;
                if manage_gap(
                    f.index(),
                    &mut density_previndex,
                    scale,
                    1.0,
                    &mut density_gap,
                ) == 0
                {
                    spacing = (f.index() as f64 - o_density_previndex) / scale;
                }
            }

            let mut still_need_simplification_after_reduction = false;
            if t as i32 == VT_POLYGON {
                let mut simplified_away_by_reduction = false;

                let prevent_tiny = o.p(P_TINY_POLYGON_REDUCTION)
                    || (o.p(P_TINY_POLYGON_REDUCTION_AT_MAXZOOM) && z == maxzoom);
                if !prevent_tiny && !o.a(A_GRID_LOW_ZOOMS) {
                    let reduced = reduce_tiny_poly(
                        sf_geom!(),
                        z,
                        line_detail,
                        &mut still_need_simplification_after_reduction,
                        &mut simplified_away_by_reduction,
                        &mut accum_area,
                        o.tiny_polygon_size as f64,
                    );
                    geom = Some(reduced);
                    if simplified_away_by_reduction {
                        strategy.tiny_polygons += 1;
                    }
                    if geom.as_ref().unwrap().is_empty() {
                        continue;
                    }
                } else {
                    still_need_simplification_after_reduction = true;
                }
            } else {
                still_need_simplification_after_reduction = true;
            }

            if (t as i32 == VT_POLYGON || t as i32 == VT_LINE)
                && line_is_too_small(sf_geom!(), z, line_detail)
            {
                continue;
            }

            let sfindex = f.index();

            if !sf_geom!().is_empty() {
                let mut adjusted_max_tile_size = max_tile_size;
                if lead_features_count > 0 {
                    adjusted_max_tile_size = adjusted_max_tile_size.wrapping_mul(
                        lead_features_count + other_multiplier_cluster_features_count,
                    ) / lead_features_count;
                }
                let mut adjusted_max_tile_features = max_tile_features;
                if lead_features_count > 0 {
                    adjusted_max_tile_features = adjusted_max_tile_features.wrapping_mul(
                        lead_features_count + other_multiplier_cluster_features_count,
                    ) / lead_features_count;
                }

                let mut adjusted_feature_count =
                    lead_features_count + other_multiplier_cluster_features_count;
                if kept > 0 {
                    adjusted_feature_count =
                        adjusted_feature_count.wrapping_mul(skipped + kept) / kept;
                }

                if too_many_bytes
                    || (adjusted_feature_count > adjusted_max_tile_size && !o.p(P_KILOBYTE_LIMIT))
                {
                    skipped += 1;
                    too_many_bytes = true;
                } else if too_many_features
                    || (adjusted_feature_count > adjusted_max_tile_features
                        && !o.p(P_FEATURE_LIMIT))
                {
                    skipped += 1;
                    too_many_features = true;
                } else {
                    kept += 1;

                    if layer.features.is_empty() {
                        dropped = FEATURE_KEPT;
                    }

                    let full = cand_full(rt, seq);
                    let mut tf = TF {
                        cand: seq as u32,
                        fi: c.fi,
                        t,
                        geometry: match geom.take() {
                            Some(g) => g,
                            None => cg.to_vec(),
                        },
                        extra: Extra::from_full(full),
                        cluster_at: NO_CLUSTER_ATTRS,
                        extent,
                        index: f.index(),
                        reduced: !still_need_simplification_after_reduction,
                        coalesced: false,
                        clustered: 0,
                        line_detail: line_detail as i8,
                        extra_detail: line_detail as i8,
                        spacing,
                        simplification,
                    };

                    if trying_to_stop_early && line_detail == first_detail {
                        // only collinear points go, the rest are kept for overzooming
                        tf.simplification = 0.0;
                    }

                    if dropped == FEATURE_KEPT && retain_points_multiplier > 1 {
                        let x = tf.extra_mut();
                        x.full_keys.push(key_pool.pool(MULTIPLIER_FIRST));
                        x.full_values.push(SerialVal::new(MVT_BOOL, "true"));
                    }

                    if dropped == FEATURE_KEPT {
                        layer.multiplier_cluster_size = 1;
                        lead_features_count += 1;
                    } else if dropped == FEATURE_ADDED_FOR_MULTIPLIER_DENSITY {
                        other_multiplier_cluster_features_count += 1;
                    } else {
                        layer.multiplier_cluster_size += 1;
                        other_multiplier_cluster_features_count += 1;
                    }

                    shared_nodes.extend_from_slice(rt.cand_edges(seq));

                    if line_detail == detail && o.extra_detail >= 0 && z == maxzoom {
                        tile_detail = o.extra_detail.min(30 - z);
                        tf.extra_detail = tile_detail as i8;
                    }

                    unsimplified_geometry_size += tf.geometry.len() * 16;
                    layer.features.push(tf);

                    if unsimplified_geometry_size > 10 * 1024 * 1024
                        && !o.a(A_DETECT_SHARED_BORDERS)
                    {
                        // each feature on its own, so they can be done in parallel
                        if simplified_geometry_through < layer.features.len() {
                            let shared_nodes = &shared_nodes;
                            par_each_mut(
                                &ctx.helpers,
                                &mut layer.features[simplified_geometry_through..],
                                |_, f2| {
                                    simplify_feature(ctx, f2, z, tx, ty, maxzoom, shared_nodes);

                                    if f2.t as i32 == VT_POLYGON {
                                        coalesce_polygon(&mut f2.geometry, false);
                                    }
                                },
                            );
                            simplified_geometry_through = layer.features.len();
                        }

                        unsimplified_geometry_size = 0;
                    }
                }
            }

            merge_previndex = sfindex;
            coalesced_area = 0.0;
        }

        let trace_loop = trace_start.elapsed().as_secs_f64();
        if first_time {
            res.children_must_go_on = dropping_by_rate || rt.minzoom_feature_pending;
            first_time = false;
        }
        let mut adjusted_max_tile_size = max_tile_size;
        if lead_features_count > 0 {
            adjusted_max_tile_size = adjusted_max_tile_size
                .wrapping_mul(lead_features_count + other_multiplier_cluster_features_count)
                / lead_features_count;
        }
        let mut adjusted_max_tile_features = max_tile_features;
        if lead_features_count > 0 {
            adjusted_max_tile_features = adjusted_max_tile_features
                .wrapping_mul(lead_features_count + other_multiplier_cluster_features_count)
                / lead_features_count;
        }

        shared_nodes.sort();

        for l in layers.iter_mut().flatten() {
            for x in l.features.iter_mut().filter_map(|p| p.extra.as_deref_mut()) {
                if x.accs.is_empty() {
                    continue;
                }
                accum::settle(&mut x.full_values, &mut x.accs);
            }
        }

        // the layers in the order of their names
        let mut layer_list: Vec<(u32, LayerFeatures)> = layers
            .into_iter()
            .enumerate()
            .filter_map(|(i, l)| l.map(|l| (i as u32, l)))
            .collect();
        layer_list.sort_by_key(|(i, _)| ctx.layer_rank[*i as usize]);

        for (li, layer) in layer_list.iter_mut() {
            let layername = &store.layers[*li as usize].name;
            let features = &mut layer.features;

            if retain_points_multiplier > 1 {
                let mut feature_sequences: Vec<(usize, usize)> = features
                    .iter()
                    .enumerate()
                    .map(|(i, f)| (store.features[f.fi as usize].seq as i64 as usize, i))
                    .collect();
                feature_sequences.sort();
                for (i, &(_, j)) in feature_sequences.iter().enumerate() {
                    let x = features[j].extra_mut();
                    x.full_keys
                        .push(key_pool.pool("tippecanoe:retain_points_multiplier_sequence"));
                    x.full_values
                        .push(SerialVal::new(MVT_DOUBLE, &i.to_string()));
                }
            }

            for p in features.iter_mut() {
                if p.clustered > 0 {
                    p.cluster_at = p.extra().full_keys.len() as u32;
                }

                if let Some(x) = p.extra.as_deref_mut() {
                    for v in x.full_values.iter_mut() {
                        if let Some(found) = v.s.find('\0') {
                            v.s.truncate(found);
                        }
                    }
                }
                for j in 0..p.full_len() {
                    let (k, v) = p.full_at(j);
                    add_tilestats(&mut res.tilestats, &mut unmap, layername, z, k, &v);
                }
            }

            if o.a(A_DETECT_SHARED_BORDERS) {
                // find_common_edges reads only the type, index and geometry
                let mut sfs: Vec<Box<SerialFeature>> = features
                    .iter_mut()
                    .map(|f| {
                        Box::new(SerialFeature {
                            t: f.t,
                            index: f.index,
                            geometry: std::mem::take(&mut f.geometry),
                            renamed: -1,
                            ..Default::default()
                        })
                    })
                    .collect();
                find_common_edges(
                    &mut sfs,
                    z,
                    line_detail,
                    simplification,
                    maxzoom,
                    merge_fraction,
                );
                for (f, sf) in features.iter_mut().zip(sfs) {
                    f.geometry = sf.geometry;
                }
            }

            {
                let shared_nodes = &shared_nodes;
                // helper threads only pay off for a lot of geometry
                let work: usize = features
                    .iter()
                    .filter(|f| f.t as i32 != VT_POINT)
                    .map(|f| f.geometry.len())
                    .sum();
                if features.len() >= 16 && work >= 50_000 {
                    par_each_mut(&ctx.helpers, features, |i, f| {
                        simplify_and_scale(ctx, i, f, z, tx, ty, maxzoom, shared_nodes)
                    });
                } else {
                    for (i, f) in features.iter_mut().enumerate() {
                        simplify_and_scale(ctx, i, f, z, tx, ty, maxzoom, shared_nodes);
                    }
                }
            }

            for f in features.iter_mut() {
                if f.t as i32 == VT_POINT || draws_something(&f.geometry) {
                    f.coalesced = false;
                }
            }

            let layer_features = features;

            if o.a(A_REORDER) {
                layer_features.sort_by(by_less(|a: &TF, b: &TF| coalindexcmp(&ax, a, b) < 0));
            }

            if o.a(A_COALESCE) {
                let mut out: usize = 0;
                if !layer_features.is_empty() {
                    out += 1;
                }

                for x in 1..layer_features.len() {
                    let y = out - 1;

                    if coalcmp(&ax, &layer_features[x], &layer_features[y]) == 0 {
                        let g = std::mem::take(&mut layer_features[x].geometry);
                        layer_features[y].geometry.extend(g);
                        layer_features[y].coalesced = true;
                    } else {
                        layer_features.swap(out, x);
                        out += 1;
                    }
                }

                layer_features.truncate(out);
            }

            {
                let mut out: usize = 0;

                for x in 0..layer_features.len() {
                    let lf = &mut layer_features[x];
                    let t = lf.t as i32;

                    if lf.coalesced && t == VT_LINE {
                        lf.geometry = remove_noop(std::mem::take(&mut lf.geometry), t, 0);
                        if !(o.p(P_SIMPLIFY) || (z == maxzoom && o.p(P_SIMPLIFY_LOW))) {
                            lf.geometry = simplify_lines(
                                &mut lf.geometry,
                                32,
                                0,
                                0,
                                0,
                                mark,
                                simplification,
                                if t == VT_POLYGON { 4 } else { 0 },
                                &shared_nodes,
                                &[],
                                0,
                                &[],
                            );
                        }
                    }

                    if t == VT_POLYGON {
                        if lf.coalesced {
                            coalesce_polygon(&mut lf.geometry, true);
                        }

                        lf.geometry = close_poly(&mut lf.geometry);
                    }

                    if !lf.geometry.is_empty() {
                        layer_features.swap(out, x);
                        out += 1;
                    }
                }

                layer_features.truncate(out);
            }

            if o.p(P_INPUT_ORDER) {
                let mut clustered = assemble_multiplier_clusters(
                    retain_points_multiplier,
                    std::mem::take(layer_features),
                );
                clustered.sort_by(by_less(|a: &Vec<TF>, b: &Vec<TF>| {
                    store.features[a[0].fi as usize].seq < store.features[b[0].fi as usize].seq
                }));
                *layer_features = disassemble_multiplier_clusters(ctx, clustered);
            }

            if !o.order_by.is_empty() {
                let mut clustered = assemble_multiplier_clusters(
                    retain_points_multiplier,
                    std::mem::take(layer_features),
                );
                clustered.sort_by(by_less(|a: &Vec<TF>, b: &Vec<TF>| {
                    ordercmp(&ax, &o.order_by, &a[0], &b[0])
                }));
                *layer_features = disassemble_multiplier_clusters(ctx, clustered);
            }

            let limit_at_maxzoom = o.limit_tile_feature_count_at_maxzoom;
            let limit = o.limit_tile_feature_count;
            if z == maxzoom && limit_at_maxzoom != 0 {
                if layer_features.len() > limit_at_maxzoom {
                    layer_features.truncate(limit_at_maxzoom);
                    too_many_features = false;
                    too_many_bytes = false;
                    skipped = 0;
                }
            } else if limit != 0 && layer_features.len() > limit {
                can_stop_early = false;
                layer_features.truncate(limit);
                too_many_features = false;
                too_many_bytes = false;
                skipped = 0;
            }
        }

        let trace_select = trace_start.elapsed().as_secs_f64();
        let mut tile: Vec<LayerEnc<'_>> = Vec::new();
        let mut mvt_tile = MvtTile::default();
        let mut tile_geoms: Vec<&DrawVec> = Vec::new();
        let mut feature_count: usize = 0;

        for (li, lf) in layer_list.iter_mut() {
            let layer_features = &mut lf.features;
            feature_count += layer_features.len();
            for f in layer_features.iter_mut() {
                let t = f.t as i32;
                if t == VT_LINE || t == VT_POLYGON {
                    f.geometry = remove_noop(std::mem::take(&mut f.geometry), t, 0);
                }
            }

            let name = &store.layers[*li as usize].name;
            let extent = 1i64 << tile_detail;
            let mut enc = LayerEnc::new(&ctx.codes, name, extent);
            let mut mvt = postfilter.map(|_| MvtBuilder::new(name, extent));
            let layer: &mut dyn Tagger = match mvt.as_mut() {
                Some(m) => m,
                None => &mut enc,
            };

            for f in layer_features.iter() {
                let t = f.t as i32;

                if f.geometry.is_empty() {
                    continue;
                }

                let sfe = &store.features[f.fi as usize];
                layer.feature(
                    t,
                    sfe.has_id,
                    store.id(f.fi as usize),
                    tile_geoms.len(),
                    &f.geometry,
                );
                tile_geoms.push(&f.geometry);
                count += f.geometry.len() as i64;

                for a in ax.of(f) {
                    if ax.live(a) {
                        layer.tag_store(ax.key(a), ctx.key_slots[a.key as usize], a.value, &|| {
                            ctx.value(a.value)
                        });
                    }
                }
                for a in 0..f.full_len() {
                    let (k, sv) = f.full_at(a);
                    let v = stringified_to_mvt_value(sv.type_, &sv.s);
                    layer.tag_extra(k, &v);
                }

                if o.a(A_CALCULATE_FEATURE_DENSITY) {
                    let mut glow: i32 = 255;
                    if f.spacing > 0.0 {
                        glow = (1.0 / f.spacing) as i32;
                        if glow > 255 {
                            glow = 255;
                        }
                    }

                    let mut v = MvtValue {
                        type_: MVT_SINT,
                        ..Default::default()
                    };
                    v.numeric_value.set_sint_value(glow as i64);
                    layer.tag_extra("tippecanoe_feature_density", &v);

                    let sv = SerialVal::new(MVT_DOUBLE, &glow.to_string());
                    add_tilestats(
                        &mut res.tilestats,
                        &mut unmap,
                        name,
                        z,
                        "tippecanoe_feature_density",
                        &sv,
                    );
                }
                layer.end_feature();
            }

            if let Some(mvt) = mvt {
                if !mvt.layer.features.is_empty() {
                    mvt_tile.layers.push(mvt.layer);
                }
            } else if !enc.is_empty() {
                tile.push(enc);
            }
        }

        if let Some(filter) = postfilter {
            mvt_tile.layers = filter_layers(
                filter,
                &mvt_tile.layers,
                z as u32,
                tx,
                ty,
                &mut res.tilestats,
                0,
                &mut unmap,
                1 << tile_detail,
            );
        }
        let tile_is_empty = if postfilter.is_some() {
            mvt_tile.layers.is_empty()
        } else {
            tile.is_empty()
        };

        if z == 0 && rt.unclipped_features < rt.original_features / 2 && !o.clipbboxes {
            eprintln!("\n\nMore than half the features were clipped away at zoom level 0.");
            eprintln!("Is your data in the wrong projection? It should be in WGS84/EPSG:4326.");
        }

        if trying_to_stop_early
            && line_detail == first_detail
            && (!can_stop_early || rt.minzoom_feature_pending)
        {
            line_detail = next_detail(line_detail);
            continue;
        }

        let mut adjusted_feature_count = feature_count;
        if kept > 0 {
            adjusted_feature_count = adjusted_feature_count.wrapping_mul(skipped + kept) / kept;
        }

        if adjusted_feature_count > 0 && !tile_is_empty {
            if too_many_features
                || (adjusted_feature_count > adjusted_max_tile_features && !o.p(P_FEATURE_LIMIT))
            {
                if adjusted_feature_count > res.feature_count_out {
                    res.feature_count_out = adjusted_feature_count;
                }

                if !quiet {
                    eprintln!(
                        "tile {}/{}/{} has {} (estimated {}) features, >{}    ",
                        z,
                        tx,
                        ty,
                        feature_count,
                        adjusted_feature_count,
                        adjusted_max_tile_features
                    );
                }

                if trying_to_stop_early && line_detail == first_detail {
                    line_detail = next_detail(line_detail);
                    continue;
                }

                match retry_threshold(
                    ctx,
                    &mut res,
                    adjusted_max_tile_features as f64,
                    adjusted_feature_count as f64,
                    feature_count > layer_list.len(),
                    0.95,
                    &mut Retry {
                        gamma: &mut gamma,
                        mingap: &mut mingap,
                        minextent: &mut minextent,
                        mindrop_sequence: &mut mindrop_sequence,
                        minattribute: &mut minattribute,
                        mingap_fraction: &mut mingap_fraction,
                        minextent_fraction: &mut minextent_fraction,
                        mindrop_sequence_fraction: &mut mindrop_sequence_fraction,
                        minattribute_fraction: &mut minattribute_fraction,
                        gaps: &mut gaps,
                        extents: &mut extents,
                        drop_sequences: &mut drop_sequences,
                        attribute_values: &mut attribute_values,
                    },
                    true,
                ) {
                    RetryOutcome::Again => {
                        line_detail += 1;
                        line_detail = next_detail(line_detail);
                        continue;
                    }
                    RetryOutcome::Fail => {
                        res.len = -1;
                        return res;
                    }
                    RetryOutcome::Abort(message) => {
                        res.len = -1;
                        res.error = Some(message);
                        return res;
                    }
                    RetryOutcome::ReduceDetail => unreachable!(),
                }
            }

            let trace_build = trace_start.elapsed().as_secs_f64();
            let mut pbf: Vec<u8> = Vec::new();
            if postfilter.is_some() {
                pbf = mvt_tile.encode();
            }
            for layer in std::mem::take(&mut tile) {
                if let Err(message) = layer.finish(&tile_geoms, &ctx.helpers, &mut pbf) {
                    res.len = -1;
                    res.error = Some(message);
                    return res;
                }
            }
            let trace_encode = trace_start.elapsed().as_secs_f64();
            let pbf_len = pbf.len();
            if let Some(mlt) = &o.mlt {
                match crate::mlt::encode_tile(&pbf, mlt) {
                    Ok(bytes) => pbf = bytes,
                    Err(message) => {
                        res.len = -1;
                        res.error = Some(format!("tile {}/{}/{}: {}", z, tx, ty, message));
                        return res;
                    }
                }
            }

            // A tile that fits whatever zlib makes of it needs no zlib.
            let gzipped = !o.p(P_TILE_COMPRESSION);
            let fits_anyway = gzipped
                && !o.fast
                && !o.zlib_tiles
                && !too_many_bytes
                && (o.p(P_KILOBYTE_LIMIT) || {
                    let bound = crate::compress::zlib_bound(pbf.len());
                    let adjusted = match kept {
                        0 => Some(bound),
                        _ => bound.checked_mul(kept + skipped).map(|b| b / kept),
                    };
                    adjusted.is_some_and(|a| a <= adjusted_max_tile_size)
                });
            let compressed = if gzipped {
                gzip(ctx, &pbf, o.fast || fits_anyway)
            } else {
                pbf
            };
            if o.trace {
                let t0 = octx.started;
                eprintln!(
                    "trace {:.3} {}/{}/{} mingap {} cands {} kept {} skipped {} features {}: loop {:.3} layers {:.3} build {:.3} encode {:.3} gzip {:.3}; {} -> {} bytes",
                    t0.elapsed().as_secs_f64(),
                    z,
                    tx,
                    ty,
                    mingap,
                    rt.cands.len(),
                    kept,
                    skipped,
                    feature_count,
                    trace_loop,
                    trace_select - trace_loop,
                    trace_build - trace_select,
                    trace_encode - trace_build,
                    trace_start.elapsed().as_secs_f64() - trace_encode,
                    pbf_len,
                    compressed.len()
                );
            }

            let mut adjusted_tile_size = compressed.len();
            if kept > 0 {
                adjusted_tile_size = adjusted_tile_size.wrapping_mul(kept + skipped) / kept;
            }

            if !fits_anyway
                && (too_many_bytes
                    || (adjusted_tile_size > adjusted_max_tile_size && !o.p(P_KILOBYTE_LIMIT)))
            {
                if adjusted_tile_size > res.tile_size_out {
                    res.tile_size_out = adjusted_tile_size;
                }

                if !quiet {
                    if adjusted_tile_size != compressed.len() {
                        eprintln!(
                            "tile {}/{}/{} size is {} (probably really {}) with detail {}, >{}    ",
                            z,
                            tx,
                            ty,
                            compressed.len(),
                            adjusted_tile_size,
                            line_detail,
                            adjusted_max_tile_size
                        );
                    } else {
                        eprintln!(
                            "tile {}/{}/{} size is {} with detail {}, >{}    ",
                            z,
                            tx,
                            ty,
                            compressed.len(),
                            line_detail,
                            adjusted_max_tile_size
                        );
                    }
                }

                if trying_to_stop_early && line_detail == first_detail {
                    line_detail = next_detail(line_detail);
                    continue;
                }

                match retry_threshold(
                    ctx,
                    &mut res,
                    adjusted_max_tile_size as f64,
                    adjusted_tile_size as f64,
                    feature_count > layer_list.len(),
                    0.75,
                    &mut Retry {
                        gamma: &mut gamma,
                        mingap: &mut mingap,
                        minextent: &mut minextent,
                        mindrop_sequence: &mut mindrop_sequence,
                        minattribute: &mut minattribute,
                        mingap_fraction: &mut mingap_fraction,
                        minextent_fraction: &mut minextent_fraction,
                        mindrop_sequence_fraction: &mut mindrop_sequence_fraction,
                        minattribute_fraction: &mut minattribute_fraction,
                        gaps: &mut gaps,
                        extents: &mut extents,
                        drop_sequences: &mut drop_sequences,
                        attribute_values: &mut attribute_values,
                    },
                    false,
                ) {
                    RetryOutcome::Again => {
                        line_detail += 1;
                        line_detail = next_detail(line_detail);
                        continue;
                    }
                    RetryOutcome::Fail => {
                        res.len = -1;
                        return res;
                    }
                    RetryOutcome::Abort(message) => {
                        res.len = -1;
                        res.error = Some(message);
                        return res;
                    }
                    RetryOutcome::ReduceDetail => {
                        detail_reduced += 1;
                    }
                }
            } else {
                if trying_to_stop_early
                    && line_detail == first_detail
                    && !rt.minzoom_feature_pending
                {
                    strategy.truncated_zooms += 1;
                    res.truncated = true;
                }
                res.len = count;
                res.tile = Some(compressed);
                res.strategy = Some(strategy);
                res.reusable = reusable_at!(line_detail, detail_reduced);
                reuse_bounds(octx, ort, &mut res);
                return res;
            }
        } else {
            res.len = count;
            res.strategy = Some(strategy);
            res.reusable = reusable_at!(line_detail, detail_reduced);
            reuse_bounds(octx, ort, &mut res);
            return res;
        }

        line_detail = next_detail(line_detail);
    }

    eprintln!("could not make tile {}/{}/{} small enough", z, tx, ty);
    res.len = -1;
    res
}

struct Retry<'r> {
    gamma: &'r mut f64,
    mingap: &'r mut u64,
    minextent: &'r mut i64,
    mindrop_sequence: &'r mut u64,
    minattribute: &'r mut f64,
    mingap_fraction: &'r mut f64,
    minextent_fraction: &'r mut f64,
    mindrop_sequence_fraction: &'r mut f64,
    minattribute_fraction: &'r mut f64,
    gaps: &'r mut Vec<u64>,
    extents: &'r mut Vec<i64>,
    drop_sequences: &'r mut Vec<u64>,
    attribute_values: &'r mut Vec<f64>,
}

enum RetryOutcome {
    /// Try again with the new threshold.
    Again,
    /// Give up on the tile.
    Fail,
    /// Give up on the run, where tippecanoe exits.
    Abort(String),
    /// No threshold applies, so try again with less detail.
    ReduceDetail,
}

/// Chooses the next threshold after a tile came out too big, as the two
/// branches at the end of tippecanoe's `write_tile` do. `by_count` is the
/// feature count branch, the other the byte size branch; they differ in a
/// few factors and in what happens when no strategy applies.
#[allow(clippy::too_many_arguments)]
fn retry_threshold(
    ctx: &Ctx<'_>,
    res: &mut MakeResult,
    limit: f64,
    actual: f64,
    more_features_than_layers: bool,
    fraction_factor: f64,
    r: &mut Retry<'_>,
    by_count: bool,
) -> RetryOutcome {
    let o = ctx.o;
    let quiet = o.quiet;
    let drop_by_attribute = o.drop_by_attribute.as_deref().unwrap_or("");

    if o.a(A_INCREASE_GAMMA_AS_NEEDED) && *r.gamma < 10.0 {
        if *r.gamma < 1.0 {
            *r.gamma = 1.0;
        } else {
            *r.gamma *= 1.25;
        }

        if *r.gamma > res.th_out.gamma {
            res.th_out.gamma = *r.gamma;
            res.still_dropping = true;
        }

        if !quiet {
            eprintln!("Going to try gamma of {:.3} to make it fit", *r.gamma);
        }
        RetryOutcome::Again
    } else if *r.mingap < u64::MAX
        && (o.a(A_DROP_DENSEST_AS_NEEDED)
            || o.a(A_COALESCE_DENSEST_AS_NEEDED)
            || o.a(A_CLUSTER_DENSEST_AS_NEEDED))
    {
        *r.mingap_fraction = *r.mingap_fraction * limit / actual * 0.80;
        if *r.mingap_fraction > 0.80 {
            if !quiet {
                eprintln!(
                    "Need to drop features, but calculated that we should keep {:.1}% of features",
                    *r.mingap_fraction * 100.0
                );
            }
            *r.mingap_fraction = 0.80;
        }
        let m = choose_mingap(r.gaps, *r.mingap_fraction, *r.mingap);
        if m > *r.mingap {
            *r.mingap = m;
            if *r.mingap > res.th_out.mingap {
                res.th_out.mingap = *r.mingap;
                res.still_dropping = true;
            }
            if !quiet {
                eprintln!(
                    "Going to try keeping the sparsest {:.2}% of the features to make it fit",
                    *r.mingap_fraction * 100.0
                );
            }
            RetryOutcome::Again
        } else {
            RetryOutcome::Abort("Can't increase feature gap threshold further".to_string())
        }
    } else if o.a(A_DROP_SMALLEST_AS_NEEDED) || o.a(A_COALESCE_SMALLEST_AS_NEEDED) {
        *r.minextent_fraction = *r.minextent_fraction * limit / actual * 0.75;
        if *r.minextent_fraction > 0.80 {
            if !quiet {
                eprintln!(
                    "Need to drop features, but calculated that we should keep {:.1}% of features",
                    *r.minextent_fraction * 100.0
                );
            }
            *r.minextent_fraction = 0.80;
        }
        let m = choose_minextent(r.extents, *r.minextent_fraction, *r.minextent);
        if m > *r.minextent {
            *r.minextent = m;
            if *r.minextent > res.th_out.minextent {
                res.th_out.minextent = *r.minextent;
                res.still_dropping = true;
            }
            if !quiet {
                eprintln!(
                    "Going to try keeping the biggest {:.2}% of the features to make it fit",
                    *r.minextent_fraction * 100.0
                );
            }
            RetryOutcome::Again
        } else {
            RetryOutcome::Abort("Can't increase feature area threshold further".to_string())
        }
    } else if o.a(A_DROP_BY_ATTRIBUTE_AS_NEEDED) {
        *r.minattribute_fraction = *r.minattribute_fraction * limit / actual * 0.75;
        if *r.minattribute_fraction > 0.80 {
            if !quiet {
                eprintln!(
                    "Need to drop features, but calculated that we should keep {:.1}% of features",
                    *r.minattribute_fraction * 100.0
                );
            }
            *r.minattribute_fraction = 0.80;
        }

        let desc = o.drop_by_attribute_descending;
        if r.attribute_values.is_empty() && !quiet {
            eprintln!(
                "Warning: no features had a numeric value for attribute '{}'",
                drop_by_attribute
            );
        }
        let m = choose_minattribute(
            r.attribute_values,
            *r.minattribute_fraction,
            *r.minattribute,
            desc,
        );
        let better = if desc {
            m < *r.minattribute
        } else {
            m > *r.minattribute
        };
        if better {
            *r.minattribute = m;
            let propagate = if desc {
                *r.minattribute < res.th_out.minattribute
            } else {
                *r.minattribute > res.th_out.minattribute
            };
            if propagate {
                res.th_out.minattribute = *r.minattribute;
                res.still_dropping = true;
            }
            if !quiet {
                if by_count {
                    eprintln!(
                        "Trying to keep features with '{}' {} {:.6} to make it fit",
                        drop_by_attribute,
                        if desc { "<=" } else { ">=" },
                        *r.minattribute
                    );
                } else {
                    eprintln!(
                        "Going to try keeping features with attribute '{}' {} {:.6} to make it fit",
                        drop_by_attribute,
                        if desc { "<=" } else { ">=" },
                        *r.minattribute
                    );
                }
            }
            RetryOutcome::Again
        } else {
            RetryOutcome::Abort("Can't increase attribute threshold further".to_string())
        }
    } else if more_features_than_layers
        && (o.a(A_DROP_FRACTION_AS_NEEDED)
            || o.a(A_COALESCE_FRACTION_AS_NEEDED)
            || o.p(P_DYNAMIC_DROP))
    {
        *r.mindrop_sequence_fraction =
            *r.mindrop_sequence_fraction * limit / actual * fraction_factor;
        if *r.mindrop_sequence_fraction > 0.80 {
            if !quiet {
                eprintln!(
                    "Need to drop features, but calculated that we should keep {:.1}% of features",
                    *r.mindrop_sequence_fraction * 100.0
                );
            }
            *r.mindrop_sequence_fraction = 0.80;
        }
        let m = choose_mindrop_sequence(
            r.drop_sequences,
            *r.mindrop_sequence_fraction,
            *r.mindrop_sequence,
        );
        if m > *r.mindrop_sequence {
            *r.mindrop_sequence = m;
            if *r.mindrop_sequence > res.th_out.mindrop_sequence {
                if !o.p(P_DYNAMIC_DROP) {
                    res.th_out.mindrop_sequence = *r.mindrop_sequence;
                }
                res.still_dropping = true;
            }
            if !quiet {
                eprintln!(
                    "Going to try keeping {:.2}% of the features to make it fit",
                    *r.mindrop_sequence_fraction * 100.0
                );
            }
            RetryOutcome::Again
        } else {
            RetryOutcome::Abort("Can't increase feature count threshold further".to_string())
        }
    } else if by_count {
        eprintln!("Try using --drop-fraction-as-needed or --drop-densest-as-needed.");
        RetryOutcome::Fail
    } else {
        RetryOutcome::ReduceDetail
    }
}

/// Fills in the bounds that `reuse` checks.
fn reuse_bounds(ctx: &Ctx<'_>, rt: &ReadTile, res: &mut MakeResult) {
    let Some(old) = res.reusable else {
        return;
    };
    let o = ctx.o;
    let sequences =
        o.a(A_COALESCE_FRACTION_AS_NEEDED) || o.a(A_DROP_FRACTION_AS_NEEDED) || o.p(P_DYNAMIC_DROP);
    let store = ctx.store;
    for (i, c) in rt.cands.iter().enumerate() {
        if c.dropped != FEATURE_KEPT {
            continue;
        }
        if !rt.gaps.is_empty() && rt.gap(i) >= old.mingap {
            res.reuse_gap = res.reuse_gap.min(rt.gap(i));
        }
        if sequences {
            let f = &store.features[c.fi as usize];
            let ds = calculate_drop_sequence(f.feature_minzoom, f.index());
            if ds > old.mindrop_sequence {
                res.reuse_drop_sequence = res.reuse_drop_sequence.min(ds);
            }
        }
    }
}
