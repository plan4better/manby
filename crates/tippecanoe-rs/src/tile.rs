//! Port of tile.hpp and tile.cpp: turning the sorted features into tiles, zoom by zoom.
//!
//! The C++ shares its state between the tiling threads through raw pointers.
//! Here each tiling thread gets its own `WriteTileArgs` with `&mut` access to
//! what only it changes (its shards of the next zoom's temporary files, and the
//! layer tables of its tiling segment), and shared access to the rest.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::fs::FileExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicUsize, Ordering};

use rusqlite::Connection;

use crate::attribute::{AttributeOp, preserve_attribute};
use crate::clip::{
    clip_lines, clip_point, close_poly, coalesce_polygon, get_mp_area, line_is_too_small,
    reduce_tiny_poly, remove_noop, simple_clip_poly, to_tile_scale,
};
use crate::compression::{Compressor, Decompressor};
use crate::cstd::perror;
use crate::dirtiles::{Zxy, dir_erase_zoom, dir_write_tile};
use crate::errors::{
    EXIT_CLOSE, EXIT_IMPOSSIBLE, EXIT_INCOMPLETE, EXIT_OPEN, EXIT_PTHREAD, EXIT_READ, EXIT_SEEK,
    EXIT_STAT, EXIT_WRITE,
};
use crate::evaluator::evaluate;
use crate::geometry::{
    Draw, DrawVec, VT_LINE, VT_LINETO, VT_MOVETO, VT_POINT, VT_POLYGON, check_polygon,
    checkerboard_anchors, point_within_tile, quick_check, reorder_lines, simplify_lines, stairstep,
};
use crate::globals::{
    AtomicF64, CLIPBBOXES, CLUSTER_DISTANCE, CLUSTER_MAXZOOM, CPUS, EXTEND_ZOOMS_MAX, EXTRA_DETAIL,
    GEOMETRY_SCALE, LAST_PROGRESS, LIMIT_TILE_FEATURE_COUNT, LIMIT_TILE_FEATURE_COUNT_AT_MAXZOOM,
    LOGGER, MAX_TILE_FEATURES, MAX_TILE_SIZE, MAX_ZOOM, ORDER_BY, ORDER_BY_SIZE, OrderField,
    PRESERVE_MULTIPLIER_DENSITY_THRESHOLD, QUIET, QUIET_PROGRESS, RETAIN_POINTS_MULTIPLIER,
    TEMP_FILES, TINY_POLYGON_SIZE, additional, get, prevent,
};
use crate::jsonpull::{JsonPull, JsonRef};
use crate::mbtiles::{LayermapEntry, add_to_tilestats, mbtiles_erase_zoom, mbtiles_write_tile};
use crate::mvt::{
    MVT_BOOL, MVT_DOUBLE, MVT_FLOAT, MVT_INT, MVT_LINESTRING, MVT_NULL, MVT_POINT, MVT_POLYGON,
    MVT_SINT, MVT_STRING, MVT_UINT, MvtFeature, MvtGeometry, MvtLayer, MvtTile, MvtValue, compress,
    stringified_to_mvt_value,
};
use crate::options::*;
use crate::plugin::{
    PipeWriter, filter_layers, is_epipe, parse_feature, perror_io, setup_filter, wait_for_filter,
};
use crate::projection::decode_index;
use crate::serial::{
    FEATURE_ADDED_FOR_MULTIPLIER_DENSITY, FEATURE_DROPPED, FEATURE_KEPT, KeyPool, Node,
    SerialFeature, SerialFeaturePtr, SerialVal, StringPoolRef, deserialize_feature, fserialize_int,
    fserialize_uint, fwrite_check, serialize_feature,
};
use crate::shared_borders::find_common_edges;
use crate::text::bit_reverse;
use crate::thread::thread_create;
use crate::tippecanoe_main::{mkstemp_cloexec, progress_time};
use crate::write_json::{JsonWriter, layer_to_geojson};

#[derive(Debug, Default)]
pub struct AtomicStrategy {
    pub dropped_by_rate: AtomicUsize,
    pub dropped_by_gamma: AtomicUsize,
    pub dropped_as_needed: AtomicUsize,
    pub coalesced_as_needed: AtomicUsize,
    pub detail_reduced: AtomicUsize,
    pub tiny_polygons: AtomicUsize,
    pub truncated_zooms: AtomicUsize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Strategy {
    pub dropped_by_rate: usize,
    pub dropped_by_gamma: usize,
    pub dropped_as_needed: usize,
    pub coalesced_as_needed: usize,
    pub detail_reduced: usize,
    pub tiny_polygons: usize,
    pub truncated_zooms: usize,

    pub tile_size: usize,
    pub feature_count: usize,
}

impl Strategy {
    pub fn new(s: &AtomicStrategy, ts: usize, fc: usize) -> Strategy {
        Strategy {
            dropped_by_rate: s.dropped_by_rate.load(Ordering::Relaxed),
            dropped_by_gamma: s.dropped_by_gamma.load(Ordering::Relaxed),
            dropped_as_needed: s.dropped_as_needed.load(Ordering::Relaxed),
            coalesced_as_needed: s.coalesced_as_needed.load(Ordering::Relaxed),
            detail_reduced: s.detail_reduced.load(Ordering::Relaxed),
            tile_size: ts,
            feature_count: fc,
            tiny_polygons: s.tiny_polygons.load(Ordering::Relaxed),
            truncated_zooms: s.truncated_zooms.load(Ordering::Relaxed),
        }
    }
}

#[allow(dead_code)]
const CMD_BITS: i32 = 3;

/// Offset coordinates to keep them positive
#[allow(dead_code)]
const COORD_OFFSET: i64 = 4i64 << 32;

/// `SHIFT_RIGHT(a)`.
fn shift_right(a: i64) -> i64 {
    (a as f64 / (1i64 << get(&GEOMETRY_SCALE)) as f64).round() as i64
}

/// `sizeof(draw)` in the C++, where the coordinates are 40-bit bitfields. It
/// sets how soon write_tile() simplifies features that are piling up in memory.
const SIZEOF_DRAW: usize = 16;

/// `pow(2, 64)`.
const TWO_TO_THE_64: f64 = 18446744073709551616.0;

/// `db_lock`. It is held while a tile is written to either kind of output, and
/// the MBTiles connection's own mutex is taken inside it.
static DB_LOCK: Mutex<()> = Mutex::new(());
/// `var_lock`, for the `most`, `midx`, `midy` and `along` totals.
static VAR_LOCK: Mutex<()> = Mutex::new(());
// `task_lock` is the mutex around the task list in traverse_zooms().

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(_) => {
            perror("pthread_mutex_lock");
            std::process::exit(EXIT_PTHREAD);
        }
    }
}

/// Turns a C++ "less than" comparator into the `Ordering` that `sort_by` wants.
/// Both sorts are stable, so for a strict weak ordering they give the same order.
fn by_less<T>(less: impl Fn(&T, &T) -> bool) -> impl FnMut(&T, &T) -> std::cmp::Ordering {
    move |a, b| {
        if less(a, b) {
            std::cmp::Ordering::Less
        } else if less(b, a) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }
}

/// `(*layer_unmaps)[segment]` as one tiling thread sees it: the tables of the
/// input segments, which nothing changes during tiling, and the table of the
/// thread's own tiling segment, the only one it adds layers to.
#[derive(Clone, Copy)]
struct LayerUnmaps<'a> {
    input: &'a [Vec<String>],
    tiling_seg: usize,
    own: &'a [String],
}

impl<'a> LayerUnmaps<'a> {
    fn get(&self, segment: i32, layer: i64) -> &'a str {
        let seg = segment as usize;
        if seg < self.input.len() {
            &self.input[seg][layer as usize]
        } else if seg == self.tiling_seg {
            &self.own[layer as usize]
        } else {
            eprintln!("Internal error: no layer table for segment {}", seg);
            std::process::exit(EXIT_IMPOSSIBLE);
        }
    }
}

// convert serial feature geometry (drawvec) to output tile geometry (mvt_geometry)
fn to_feature(geom: &DrawVec) -> Vec<MvtGeometry> {
    let mut out = Vec::with_capacity(geom.len());

    for g in geom {
        out.push(MvtGeometry::new(g.op as i32, g.x, g.y));
    }

    out
}

// does this geometry have any non-zero-length linetos?
fn draws_something(geom: &DrawVec) -> bool {
    for i in 1..geom.len() {
        if geom[i].op == VT_LINETO && (geom[i].x != geom[i - 1].x || geom[i].y != geom[i - 1].y) {
            return true;
        }
    }

    false
}

// comparator for --preserve-input-order, to reorder features back to their original input sequence
fn preservecmp_clusters(a: &[SerialFeaturePtr], b: &[SerialFeaturePtr]) -> bool {
    preservecmp(&a[0], &b[0])
}

fn preservecmp(a: &SerialFeature, b: &SerialFeature) -> bool {
    a.seq < b.seq
}

fn sign(c: std::cmp::Ordering) -> i32 {
    match c {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

// comparator for --coalesce and --reorder:
// two features can be coalesced if they have
// * the same type
// * the same id, if any
// * the same attributes, according to metacmp
// * the same full_keys and full_values attributes
fn coalcmp(c1: &SerialFeature, c2: &SerialFeature) -> i32 {
    let mut cmp = c1.t as i32 - c2.t as i32;
    if cmp != 0 {
        return cmp;
    }

    if c1.has_id != c2.has_id {
        return c1.has_id as i32 - c2.has_id as i32;
    }

    if c1.has_id && c2.has_id {
        if c1.id < c2.id {
            return -1;
        }
        if c1.id > c2.id {
            return 1;
        }
    }

    cmp = metacmp(c1, c2);
    if cmp != 0 {
        return cmp;
    }

    if c1.full_keys.len() < c2.full_keys.len() {
        return -1;
    } else if c1.full_keys.len() > c2.full_keys.len() {
        return 1;
    }

    for i in 0..c1.full_keys.len() {
        if *c1.full_keys[i] < *c2.full_keys[i] {
            return -1;
        } else if *c1.full_keys[i] > *c2.full_keys[i] {
            return 1;
        }

        if c1.full_values[i].type_ < c2.full_values[i].type_ {
            return -1;
        } else if c1.full_values[i].type_ > c2.full_values[i].type_ {
            return 1;
        }

        if c1.full_values[i].s < c2.full_values[i].s {
            return -1;
        } else if c1.full_values[i].s > c2.full_values[i].s {
            return 1;
        }
    }

    0
}

// comparator for --reorder:
// features are ordered first by their attributes (according to coalcmp above)
// and then, if they are identical from that perspective, by their index (centroid)
// and geometry
fn coalindexcmp(c1: &SerialFeature, c2: &SerialFeature) -> i32 {
    let cmp = coalcmp(c1, c2);

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

fn coalindexcmp_comparator(a: &SerialFeaturePtr, o: &SerialFeaturePtr) -> bool {
    let cmp = coalindexcmp(a, o);
    cmp < 0
}

fn drop_sequence_cmp(a: &SerialFeaturePtr, b: &SerialFeaturePtr) -> bool {
    let a_seq = calculate_drop_sequence(a);
    let b_seq = calculate_drop_sequence(b);

    // sorts backwards, to put the features that would be dropped last, first here
    a_seq > b_seq
}

// retrieve an attribute key or value from the string pool and return it as mvt_value
fn retrieve_string(off: i64, stringpool: &StringPoolRef) -> MvtValue {
    let type_ = stringpool.type_at(off);
    let s = stringpool.str_at(off);

    stringified_to_mvt_value(type_, s)
}

// retrieve an attribute key from the string pool and return it as std::string
fn retrieve_std_string(off: i64, stringpool: &StringPoolRef) -> String {
    stringpool.str_at(off).to_string()
}

// retrieve the keys and values of a feature from the string pool
// and tag them onto an mvt_feature and mvt_layer
fn decode_meta(sf: &SerialFeature, layer: &mut MvtLayer, feature: &mut MvtFeature) {
    for i in 0..sf.keys.len() {
        let key = retrieve_std_string(sf.keys[i], &sf.stringpool);
        let value = retrieve_string(sf.values[i], &sf.stringpool);

        layer.tag(feature, &key, &value);
    }
}

// comparator used to check whether two features have identical keys and values,
// as determined by retrieving them from the string pool. The order of keys,
// not just the content of their values, must also be identical for them to compare equal.
fn metacmp(one: &SerialFeature, two: &SerialFeature) -> i32 {
    if one.keys.len() < two.keys.len() {
        return -1;
    } else if one.keys.len() > two.keys.len() {
        return 1;
    }

    let mut i = 0;
    while i < one.keys.len() && i < two.keys.len() {
        let key1 = one.stringpool.str_at(one.keys[i]);
        let key2 = two.stringpool.str_at(two.keys[i]);

        let cmp = sign(key1.as_bytes().cmp(key2.as_bytes()));
        if cmp != 0 {
            return cmp;
        }

        let off1 = one.values[i];
        let type1 = one.stringpool.type_at(off1);
        let s1 = one.stringpool.str_at(off1);

        let off2 = two.values[i];
        let type2 = two.stringpool.type_at(off2);
        let s2 = two.stringpool.str_at(off2);

        if type1 != type2 {
            return type1 - type2;
        }
        let cmp = sign(s1.as_bytes().cmp(s2.as_bytes()));
        if cmp != 0 {
            return cmp;
        }

        i += 1;
    }

    0
}

/// `mvt_value v; v.type = type;` with the numeric value left at zero.
fn mvt_value_of_type(type_: i32) -> MvtValue {
    MvtValue {
        type_,
        ..Default::default()
    }
}

// Retrieve the value of an attribute or pseudo-attribute (ORDER_BY_SIZE) for --order purposes.
fn find_attribute_value(c1: &SerialFeature, key: &str) -> MvtValue {
    if key == ORDER_BY_SIZE {
        let mut v = mvt_value_of_type(MVT_DOUBLE);
        v.numeric_value.set_double_value(c1.extent as f64);
        return v;
    }

    let keys1 = &c1.keys;
    let values1 = &c1.values;
    let stringpool1 = &c1.stringpool;

    for i in 0..keys1.len() {
        let key1 = stringpool1.str_at(keys1[i]);
        if key1 == key {
            return retrieve_string(values1[i], stringpool1);
        }
    }

    for i in 0..c1.full_keys.len() {
        if *c1.full_keys[i] == key {
            return stringified_to_mvt_value(c1.full_values[i].type_, &c1.full_values[i].s);
        }
    }

    let mut v = mvt_value_of_type(MVT_NULL);
    v.numeric_value.set_null_value(0);
    v
}

// Ensure that two mvt_values can be compared numerically by converting other numeric types to mvt_double
fn coerce_double(mut v: MvtValue) -> MvtValue {
    if v.type_ == MVT_INT {
        v.type_ = MVT_DOUBLE;
        let d = v.numeric_value.int_value() as f64;
        v.numeric_value.set_double_value(d);
    } else if v.type_ == MVT_UINT {
        v.type_ = MVT_DOUBLE;
        let d = v.numeric_value.uint_value() as f64;
        v.numeric_value.set_double_value(d);
    } else if v.type_ == MVT_SINT {
        v.type_ = MVT_DOUBLE;
        let d = v.numeric_value.sint_value() as f64;
        v.numeric_value.set_double_value(d);
    } else if v.type_ == MVT_FLOAT {
        v.type_ = MVT_DOUBLE;
        let d = v.numeric_value.float_value() as f64;
        v.numeric_value.set_double_value(d);
    }

    v
}

// comparator for ordering features for --order: for each sort key that the user has specified,
// compare features numerically according to that sort key until the keys are exhausted.
// If there is a tie, the feature with the earlier index (centroid) comes first.
fn ordercmp_clusters(
    order_by: &[OrderField],
    a: &[SerialFeaturePtr],
    b: &[SerialFeaturePtr],
) -> bool {
    ordercmp(order_by, &a[0], &b[0])
}

fn ordercmp(order_by: &[OrderField], a: &SerialFeature, b: &SerialFeature) -> bool {
    for field in order_by {
        let v1 = coerce_double(find_attribute_value(a, &field.name));
        let v2 = coerce_double(find_attribute_value(b, &field.name));

        if field.descending {
            if v2 < v1 {
                return true;
            } else if v1 < v2 {
                return false;
            } // else they are equal, so continue to the next attribute
        } else if v1 < v2 {
            return true;
        } else if v2 < v1 {
            return false;
        } // else they are equal, so continue to the next attribute
    }

    if a.index < b.index {
        return true;
    }

    false // greater than or equal
}

// For --retain-points-multiplier: Go through a list of features and return a list of clusters of features,
// creating a new cluster whenever the tippecanoe:retain_points_multiplier_first attribute is seen.
fn assemble_multiplier_clusters(features: Vec<SerialFeaturePtr>) -> Vec<Vec<SerialFeaturePtr>> {
    let mut clusters: Vec<Vec<SerialFeaturePtr>> = Vec::new();

    if get(&RETAIN_POINTS_MULTIPLIER) == 1 {
        for feature in features {
            clusters.push(vec![feature]);
        }
    } else {
        for feature in features {
            let mut is_cluster_start = false;

            for key in &feature.full_keys {
                if **key == "tippecanoe:retain_points_multiplier_first" {
                    is_cluster_start = true;
                    break;
                }
            }

            if is_cluster_start || clusters.is_empty() {
                clusters.push(Vec::new());
            }

            clusters.last_mut().unwrap().push(feature);
        }
    }

    clusters
}

// For --retain-points-multiplier: Flatten a list of clusters of features back into a list of features,
// moving the "tippecanoe:retain_points_multiplier_first" attribute onto the first feature of each cluster
// if it is not already there.
fn disassemble_multiplier_clusters(clusters: Vec<Vec<SerialFeaturePtr>>) -> Vec<SerialFeaturePtr> {
    let mut out = Vec::new();

    for mut cluster in clusters {
        // fix up the attributes so the first feature of the multiplier cluster
        // gets the marker attribute
        'outer: for i in 0..cluster.len() {
            for j in 0..cluster[i].full_keys.len() {
                if *cluster[i].full_keys[j] == "tippecanoe:retain_points_multiplier_first" {
                    let k = cluster[i].full_keys.remove(j);
                    let v = cluster[i].full_values.remove(j);

                    cluster[0].full_keys.push(k);
                    cluster[0].full_values.push(v);

                    break 'outer; // break outer
                }
            }
        }

        // sort the other features by their drop sequence, for consistency across zoom levels
        if cluster.len() > 1 {
            cluster[1..].sort_by(by_less(drop_sequence_cmp));
        }

        out.extend(cluster);
    }

    out
}

// Write out copies of a feature into the temporary files for the next zoom level
#[allow(clippy::too_many_arguments)]
fn rewrite(
    osf: &mut SerialFeature,
    z: i32,
    nextzoom: i32,
    maxzoom: i32,
    tx: u32,
    ty: u32,
    buffer: i32,
    within: &mut [bool],
    geompos: &[AtomicI64],
    start_geompos: &mut [i64],
    geomfile: &mut [Compressor],
    fname: &str,
    child_shards: i32,
    max_zoom_increment: i32,
    segment: i32,
    initial_x: &[u32],
    initial_y: &[u32],
) {
    if !osf.geometry.is_empty()
        && (nextzoom <= maxzoom || additional(A_EXTEND_ZOOMS) != 0 || get(&EXTEND_ZOOMS_MAX) > 0)
    {
        let span: i32 = 1 << (nextzoom - z);

        // Get the feature bounding box in pixel (256) coordinates at the child zoom
        // in order to calculate which sub-tiles it can touch including the buffer.
        let mut bbox2 = [0i64; 4];
        for (b, &o) in bbox2.iter_mut().zip(osf.bbox.iter()) {
            // Division instead of right-shift because coordinates can be negative
            *b = o / 1i32.wrapping_shl((32 - nextzoom - 8) as u32) as i64;
        }
        // Decrement the top and left edges so that any features that are
        // touching the edge can potentially be included in the adjacent tiles too.
        bbox2[0] -= (buffer + 1) as i64;
        bbox2[1] -= (buffer + 1) as i64;
        bbox2[2] += buffer as i64;
        bbox2[3] += buffer as i64;

        for b in bbox2.iter_mut() {
            if *b < 0 {
                *b = 0;
            }
            if *b >= 256 * span as i64 {
                *b = 256 * (span as i64 - 1);
            }

            *b /= 256;
        }

        // Offset from tile coordinates back to world coordinates
        let mut sx: u32 = 0;
        let mut sy: u32 = 0;
        if z != 0 {
            sx = tx << (32 - z);
            sy = ty << (32 - z);
        }

        let mut geom2 = DrawVec::with_capacity(osf.geometry.len());
        for g in &osf.geometry {
            geom2.push(Draw::new(
                g.op,
                shift_right(g.x + sx as i64),
                shift_right(g.y + sy as i64),
            ));
        }

        // Every child gets the same serialized feature, so it is only made once.
        let mut feature: Option<Vec<u8>> = None;

        let mut xo = bbox2[0] as i32;
        while xo as i64 <= bbox2[2] {
            let mut yo = bbox2[1] as i32;
            while yo as i64 <= bbox2[3] {
                let jx: u32 = tx.wrapping_mul(span as u32).wrapping_add(xo as u32);
                let jy: u32 = ty.wrapping_mul(span as u32).wrapping_add(yo as u32);

                // j is the shard that the child tile's data is being written to.
                //
                // Be careful: We can't jump more zoom levels than max_zoom_increment
                // because that could break the constraint that each of the children
                // of the current tile must have its own shard, because the data for
                // the child tile must be contiguous within the shard.
                //
                // But it's OK to spread children across all the shards, not just
                // the four that would normally result from splitting one tile,
                // because it will go through all the shards when it does the
                // next zoom.
                //
                // If child_shards is a power of 2 but not a power of 4, this will
                // shard X more widely than Y. XXX Is there a better way to do this
                // without causing collisions?

                let j = ((jx.wrapping_shl(max_zoom_increment as u32)
                    | (jy & ((1u32 << max_zoom_increment) - 1)))
                    & (child_shards as u32).wrapping_sub(1)) as usize;

                {
                    if !within[j] {
                        within[j] = true;
                        start_geompos[j] = geompos[j].load(Ordering::Relaxed); // no competition between threads

                        let estimated_complexity: i64 = 0; // placeholder, to be filled in later
                        fwrite_check(
                            &estimated_complexity.to_ne_bytes(),
                            std::mem::size_of::<i64>(),
                            1,
                            geomfile[j].fp(),
                            &geompos[j],
                            fname,
                        );
                        fserialize_int(geomfile[j].fp(), nextzoom, &geompos[j], fname);
                        fserialize_uint(
                            geomfile[j].fp(),
                            tx.wrapping_mul(span as u32).wrapping_add(xo as u32),
                            &geompos[j],
                            fname,
                        );
                        fserialize_uint(
                            geomfile[j].fp(),
                            ty.wrapping_mul(span as u32).wrapping_add(yo as u32),
                            &geompos[j],
                            fname,
                        );
                        geomfile[j].begin();
                    }

                    if feature.is_none() {
                        let saved = std::mem::replace(&mut osf.geometry, geom2.clone());
                        feature = Some(serialize_feature(
                            osf,
                            shift_right(initial_x[segment as usize] as i64),
                            shift_right(initial_y[segment as usize] as i64),
                        ));
                        osf.geometry = saved;
                    }
                    let feature = feature.as_ref().unwrap();

                    geomfile[j].serialize_long_long(feature.len() as i64, &geompos[j], fname);
                    geomfile[j].fwrite_check(feature, 1, feature.len(), &geompos[j], fname);
                }

                yo += 1;
            }
            xo += 1;
        }
    }
}

// This is the parameter block passed to each simplification worker thread.
// The C++ passes each worker the whole feature list and has it take every
// `tasks`th feature starting from `task`; here each worker is handed exactly
// those features, along with their positions in the list.
struct SimplificationWorkerArg<'a> {
    features: Vec<(usize, &'a mut SerialFeature)>,

    shared_nodes: &'a DrawVec,
    shared_nodes_map: &'a [Node],
    nodepos: usize,
    shared_nodes_bloom: &'a [u8],
}

// If a polygon has collapsed away to nothing during polygon cleaning,
// this is the function that tries to replace it with a rectangular placeholder
// so that the area of the feature is still somehow represented
fn revive_polygon(geom: &DrawVec, mut area: f64, z: i32, detail: i32) -> DrawVec {
    // From area in world coordinates to area in tile coordinates
    let divisor: i64 = 1i64 << (32 - detail - z);
    area /= divisor.wrapping_mul(divisor) as f64;

    if area == 0.0 {
        return DrawVec::new();
    }

    let height = area.sqrt().ceil() as i32;
    let mut width = (area / height as f64).round() as i32;
    if width == 0 {
        width = 1;
    }

    let mut sx: i64 = 0;
    let mut sy: i64 = 0;
    let mut n: i64 = 0;
    for g in geom {
        if g.op == VT_MOVETO || g.op == VT_LINETO {
            sx += g.x;
            sy += g.y;
            n += 1;
        }
    }

    if n > 0 {
        sx /= n;
        sy /= n;

        let width = width as i64;
        let height = height as i64;

        let out: DrawVec = vec![
            Draw::new(VT_MOVETO, sx - (width / 2), sy - (height / 2)),
            Draw::new(VT_LINETO, sx - (width / 2) + width, sy - (height / 2)),
            Draw::new(
                VT_LINETO,
                sx - (width / 2) + width,
                sy - (height / 2) + height,
            ),
            Draw::new(VT_LINETO, sx - (width / 2), sy - (height / 2) + height),
            Draw::new(VT_LINETO, sx - (width / 2), sy - (height / 2)),
        ];

        out
    } else {
        DrawVec::new()
    }
}

// This simplifies the geometry of one feature. It is generally called from the feature_simplification_worker
// but is broken out here so that it can be called from earlier in write_tile if coalesced geometries build up
// too much in memory.
fn simplify_feature(
    p: &mut SerialFeature,
    shared_nodes: &DrawVec,
    shared_nodes_map: &[Node],
    nodepos: usize,
    shared_nodes_bloom: &[u8],
) -> f64 {
    // Nothing else reads p.geometry before it is replaced at the end.
    let mut geom = std::mem::take(&mut p.geometry);
    let t = p.t as i32;
    let z = p.z;
    let line_detail = p.line_detail;
    let maxzoom = p.maxzoom;

    if additional(A_GRID_LOW_ZOOMS) != 0 && z < maxzoom {
        geom = stairstep(&mut geom, z, line_detail);
    }

    let mut area = 0.0;
    if t == VT_POLYGON {
        area = get_mp_area(&geom);
    }

    if (t == VT_LINE || t == VT_POLYGON)
        && !(prevent(P_SIMPLIFY) != 0
            || (z == maxzoom && prevent(P_SIMPLIFY_LOW) != 0)
            || (z < maxzoom && additional(A_GRID_LOW_ZOOMS) != 0))
    {
        // Now I finally remember why it doesn't simplify if the feature was reduced:
        // because it makes square placeholders look like weird triangular placeholders.
        // Only matters if simplification is set higher than the tiny polygon size.
        // Tiny polygons that are part of a tiny multipolygon will still get simplified.
        if !p.reduced {
            // These aren't necessarily actually no-ops until we scale down.
            // Don't do it if we are trying to preserve intersections, because
            // it might wipe out the intersection and spoil the matching even though
            // it would leave something else within the same tile pixel.
            if t == VT_LINE && prevent(P_SIMPLIFY_SHARED_NODES) == 0 {
                // continues to deduplicate to line_detail even if we have extra detail
                geom = remove_noop(geom, t, 32 - z - line_detail);
            }

            let mut already_marked = false;
            if additional(A_DETECT_SHARED_BORDERS) != 0 && t == VT_POLYGON {
                already_marked = true;
            }

            if !already_marked {
                if p.coalesced && t == VT_POLYGON {
                    // clean coalesced polygons before simplification to avoid
                    // introducing shards between shapes that otherwise would have
                    // unioned exactly
                    //
                    // don't try to scale up because these are still world coordinates
                    coalesce_polygon(&mut geom, false);
                }

                // continues to simplify to line_detail even if we have extra detail
                let mut ngeom = simplify_lines(
                    &mut geom,
                    z,
                    p.tx,
                    p.ty,
                    line_detail,
                    !(prevent(P_CLIPPING) != 0 || prevent(P_DUPLICATION) != 0),
                    p.simplification,
                    if t == VT_POLYGON { 4 } else { 0 },
                    shared_nodes,
                    shared_nodes_map,
                    nodepos,
                    shared_nodes_bloom,
                );

                if p.coalesced && prevent(P_SIMPLIFY_SHARED_NODES) != 0 {
                    // do another simplification to eliminate collinearities
                    // that were left behind at the former corners between
                    // coalesced geometries
                    ngeom = simplify_lines(
                        &mut ngeom,
                        z,
                        p.tx,
                        p.ty,
                        line_detail,
                        !(prevent(P_CLIPPING) != 0 || prevent(P_DUPLICATION) != 0),
                        0.1,
                        if t == VT_POLYGON { 4 } else { 0 },
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
    }

    if t == VT_LINE && additional(A_REVERSE) != 0 {
        geom = remove_noop(geom, t, 0);
        geom = reorder_lines(&geom);
    }

    p.geometry = geom;
    area
}

// This is the worker function that is called from multiple threads to
// simplify and clean the geometry of batches of features.
fn simplification_worker(a: SimplificationWorkerArg<'_>) {
    for (i, feature) in a.features {
        let area = simplify_feature(
            feature,
            a.shared_nodes,
            a.shared_nodes_map,
            a.nodepos,
            a.shared_nodes_bloom,
        );

        let t = feature.t as i32;
        let z = feature.z;
        let out_detail = feature.extra_detail;

        // Nothing else reads feature.geometry before it is replaced below.
        let mut geom = std::mem::take(&mut feature.geometry);
        to_tile_scale(&mut geom, z, out_detail);

        if t == VT_POLYGON {
            // Scaling may have made the polygon degenerate.
            // Give Clipper a chance to try to fix it.
            {
                let before = geom.clone();

                // we can try scaling up because this is now tile scale
                coalesce_polygon(&mut geom, true);
                if additional(A_DEBUG_POLYGON) != 0 {
                    check_polygon(&mut geom);
                }

                if geom.len() < 3 {
                    if area > 0.0 {
                        // area is in world coordinates, calculated before scaling down
                        geom = revive_polygon(&before, area, z, out_detail);
                    } else {
                        geom.clear();
                    }
                }
            }
        }

        if feature.index == 0 {
            feature.index = i as u64;
        }
        feature.geometry = geom;
    }
}

/// I really don't understand quite how this feature works any more, which is why I want to
/// get rid of the --gamma option. It does something with the feature spacing to calculate
/// whether each feature should be kept or is in a dense enough context that it should
/// be dropped
pub fn manage_gap(index: u64, previndex: &mut u64, scale: f64, gamma: f64, gap: &mut f64) -> i32 {
    if gamma > 0.0 {
        if *gap > 0.0 {
            if index == *previndex {
                return 1; // Exact duplicate: can't fulfil the gap requirement
            }

            if index < *previndex
                || ((index.wrapping_sub(*previndex) as f64 / scale).ln() * gamma).exp() >= *gap
            {
                // Dot is further from the previous than the nth root of the gap,
                // so produce it, and choose a new gap at the next point.
                *gap = 0.0;
            } else {
                return 1;
            }
        } else if index >= *previndex {
            *gap = (index - *previndex) as f64 / scale;

            if *gap == 0.0 {
                return 1; // Exact duplicate: skip
            } else if *gap < 1.0 {
                return 1; // Narrow dot spacing: need to stretch out
            } else {
                *gap = 0.0; // Wider spacing than minimum: so pass through unchanged
            }
        }

        *previndex = index;
    }

    0
}

/// `std::stable_sort` with `operator<`, for values where that is a total preorder.
fn stable_sort_less<T: PartialOrd>(v: &mut [T]) {
    v.sort_by(by_less(|a: &T, b: &T| a < b));
}

// This function is called to choose the new gap threshold for --drop-densest-as-needed
// and --coalesce-densest-as-needed.
fn choose_mingap(gaps: &mut [u64], f: f64, existing_gap: u64) -> u64 {
    stable_sort_less(gaps);

    let mut ix = (gaps.len().wrapping_sub(1) as f64 * (1.0 - f)) as usize;
    while ix + 1 < gaps.len() && gaps[ix] <= existing_gap {
        ix += 1;
    }

    gaps[ix]
}

// This function is called to choose the new "extent" threshold to try when a tile exceeds the
// tile size limit or feature limit and `--drop-smallest-as-needed` or `--coalesce-smallest-as-needed`
// has been set.
//
// The "extents" are the areas of the polygon features or the pseudo-areas associated with the
// linestring or point features that were examined for inclusion in the most recent
// iteration of this tile. (This includes features that were dropped because they were below
// the previous size threshold, but not features that were dropped by fractional point dropping).
// The extents are placed in order by the sort, from smallest to largest.
//
// The `fraction` is the proportion of these features that tippecanoe thinks should be retained to
// to make the tile small enough now. Because the extents are sorted from smallest to largest,
// the smallest extent threshold that will retain that fraction of features is found `fraction`
// distance from the end of the list, or at element `(1 - fraction) * (size() - 1)`.
//
// However, the extent found there may be the same extent that was used in the last iteration!
//
// (The "existing_extent" is the extent threshold that selected these features in the recent
// iteration. It is 0 the first time a tile is attempted, and gets higher on successive iterations
// as tippecanoe restricts the features to be kept to larger and larger features.)
//
// The features that are kept are those with a size >= the existing_extent, so if there are a large
// number of features with identical small areas, the new guess may not exclude enough features
// to actually choose a new threshold larger than the previous threshold.
//
// To address this, the array index `ix` of the new chosen extent is incremented toward the end
// of the list, until the possibilities run out or something higher than the old extent is found.
// If there are no higher extents available, the tile has already been reduced as much as possible
// and tippecanoe will exit with an error.
fn choose_minextent(extents: &mut [i64], f: f64, existing_extent: i64) -> i64 {
    stable_sort_less(extents);

    let mut ix = (extents.len().wrapping_sub(1) as f64 * (1.0 - f)) as usize;
    while ix + 1 < extents.len() && extents[ix] <= existing_extent {
        ix += 1;
    }

    extents[ix]
}

fn choose_mindrop_sequence(drop_sequences: &mut [u64], f: f64, existing_drop_sequence: u64) -> u64 {
    if drop_sequences.is_empty() {
        return u64::MAX;
    }

    stable_sort_less(drop_sequences);

    let mut ix = ((drop_sequences.len() - 1) as f64 * (1.0 - f)) as usize;
    while ix + 1 < drop_sequences.len() && drop_sequences[ix] <= existing_drop_sequence {
        ix += 1;
    }

    drop_sequences[ix]
}

fn choose_minattribute(
    attribute_values: &mut [f64],
    f: f64,
    existing_attribute: f64,
    descending: bool,
) -> f64 {
    if attribute_values.is_empty() {
        return existing_attribute;
    }

    stable_sort_less(attribute_values);

    if descending {
        // For descending: drop features > threshold, keep features <= threshold
        // ix points at the last value to keep
        let mut ix = ((attribute_values.len() - 1) as f64 * f) as usize;
        while ix > 0 && attribute_values[ix] >= existing_attribute {
            ix -= 1;
        }

        if attribute_values[ix] >= existing_attribute {
            return existing_attribute;
        }

        attribute_values[ix]
    } else {
        // For ascending: drop features < threshold, keep features >= threshold
        // ix points at the first value to keep
        let mut ix = ((attribute_values.len() - 1) as f64 * (1.0 - f)).ceil() as usize;
        if ix >= attribute_values.len() {
            ix = attribute_values.len() - 1;
        }
        while ix + 1 < attribute_values.len() && attribute_values[ix] <= existing_attribute {
            ix += 1;
        }

        if attribute_values[ix] <= existing_attribute {
            return existing_attribute;
        }

        attribute_values[ix]
    }
}

fn calculate_drop_sequence(sf: &SerialFeature) -> u64 {
    // std::max with 0ULL does nothing to an unsigned value
    let zoom: u64 = (sf.feature_minzoom as i64 as u64).min(31);
    let mut out: u64 = zoom << (64 - 5); // top bits are the zoom level: top-priority features are those that appear in the low zooms
    out |= bit_reverse(sf.index) & !(31u64 << (64 - 5)); // remaining bits are from the inverted indes, which should incrementally fill in spatially
    !out // lowest numbered feature gets dropped first
}

#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)]
struct Task {
    fileno: usize,
    todo: usize,
}

// This is the block of parameters that are passed to write_tile() to read a tile
// from the serialized form, do whatever needs to be done to it, and to write the
// MVT-format output to the output tileset.
//
// The _out parameters are thresholds calculated during tiling; they are collected
// by the caller to determine whether the zoom level needs to be done over with
// new thresholds.
#[allow(dead_code)]
struct WriteTileArgs<'a> {
    threadno: usize,
    /// The task list, behind what the C++ calls `task_lock`.
    tasks: &'a Mutex<Vec<Task>>,
    global_stringpool: &'a StringPoolRef,
    min_detail: i32,
    outdb: Option<&'a Mutex<Connection>>,
    outdir: Option<&'a str>,
    buffer: i32,
    fname: &'a str,
    /// This thread's shards of the next zoom's temporary files.
    geomfile: &'a mut [Compressor],
    geompos: &'a [AtomicI64],
    todo: f64,
    along: &'a AtomicI64,
    gamma: f64,
    gamma_out: f64,
    child_shards: i32,
    geomfd: &'a [AtomicI32],
    geom_size: &'a [i64],
    midx: &'a AtomicU32,
    midy: &'a AtomicU32,
    maxzoom: i32,
    minzoom: i32,
    basezoom: i32,
    droprate: f64,
    full_detail: i32,
    low_detail: i32,
    simplification: f64,
    most: &'a AtomicI64,
    pool_off: &'a [i64],
    initial_x: &'a [u32],
    initial_y: &'a [u32],
    running: &'a AtomicI32,
    err: i32,
    /// `(*layermaps)[tiling_seg]`.
    layermap: &'a mut BTreeMap<String, LayermapEntry>,
    /// `(*layer_unmaps)[segment]` for the input segments.
    input_unmaps: &'a [Vec<String>],
    /// `(*layer_unmaps)[tiling_seg]`.
    layer_unmap: &'a mut Vec<String>,
    pass: usize,
    mingap: u64,
    mingap_out: u64,
    minextent: i64,
    minextent_out: i64,
    mindrop_sequence: u64,
    mindrop_sequence_out: u64,
    // the infinity that excludes nothing, for the ascending order that
    // drop_by_attribute_descending also defaults to. dropping_features()
    // reads these, so they must not start out at a value that looks like
    // a threshold that has already been chosen.
    minattribute: f64,
    minattribute_out: f64,
    drop_by_attribute_as_needed_attribute: Option<&'a str>,
    drop_by_attribute_descending: bool,
    tile_size_out: usize,
    feature_count_out: usize,
    prefilter: Option<&'a str>,
    postfilter: Option<&'a str>,
    attribute_accum: &'a HashMap<String, AttributeOp>,
    still_dropping: bool,
    wrote_zoom: i32,
    tiling_seg: usize,
    filter: Option<JsonRef<'a>>,
    unidecode_data: &'a [String],
    strategy: &'a AtomicStrategy,
    zoom: i32,
    compressed: bool,
    shared_nodes_map: &'a [Node],
    nodepos: usize,
    shared_nodes_bloom: &'a [u8],
    skip_children: &'a BTreeSet<Zxy>, // what is being skipped at this zoom
    skip_children_out: BTreeSet<Zxy>, // what will be skipped in the next zoom
    first_dropping_pass: bool, // is this the first pass of this zoom that is dropping features?
}

// Is this zoom level discarding features to make its tiles fit? The thresholds
// start out excluding nothing, and only move inward once some tile in the zoom
// fails to fit and the whole zoom is retried with a threshold for discarding
// features. For mingap, minextent, and mindrop_sequence that starting point is
// zero; for minattribute it is the infinity on whichever side is being kept.
//
// Pyramid truncation is disabled while this is true, so that every tile in the
// zoom is reduced consistently, and tiles that were skipped because an ancestor
// truncated its pyramid are revived.
fn dropping_features(
    mingap: u64,
    minextent: i64,
    mindrop_sequence: u64,
    minattribute: f64,
    drop_by_attribute_descending: bool,
) -> bool {
    let dropping_by_attribute = if drop_by_attribute_descending {
        minattribute != f64::INFINITY
    } else {
        minattribute != f64::NEG_INFINITY
    };

    mingap != 0 || minextent != 0 || mindrop_sequence != 0 || dropping_by_attribute
}

// Clips a feature's geometry to the tile bounds at the specified zoom level
// with the specified buffer. Returns true if the feature was entirely clipped away
// by bounding box alone; otherwise returns false.
fn clip_to_tile(sf: &mut SerialFeature, z: i32, buffer: i64) -> bool {
    let mut quick = quick_check(&sf.bbox, z, buffer);

    if z == 0
        && (sf.bbox[0] <= (1i64 << 32) * buffer / 256
            || sf.bbox[2] >= (1i64 << 32) - ((1i64 << 32) * buffer / 256))
    {
        // If the geometry extends off the edge of the world, concatenate on another copy
        // shifted by 360 degrees, and then make sure both copies get clipped down to size.

        let n = sf.geometry.len();

        if sf.bbox[0] <= (1i64 << 32) * buffer / 256 {
            for i in 0..n {
                let g = sf.geometry[i];
                sf.geometry.push(Draw::new(g.op, g.x + (1i64 << 32), g.y));
            }
        }

        if sf.bbox[2] >= (1i64 << 32) - ((1i64 << 32) * buffer / 256) {
            for i in 0..n {
                let g = sf.geometry[i];
                sf.geometry.push(Draw::new(g.op, g.x - (1i64 << 32), g.y));
            }
        }

        sf.bbox[0] = 0;
        sf.bbox[2] = 1i64 << 32;

        quick = -1;
    }

    if quick == 0 {
        // entirely outside the tile
        return true;
    }

    // if quick == 3 the feature touches the buffer, not just the tile proper,
    // so we need to clip to add intersection points at the tile edge.

    // if quick == 2 it touches the buffer and beyond, so likewise

    // if quick == 1 we should be able to get away without clipping, because
    // the feature is entirely within the tile proper.

    // Can't accept the quick check if guaranteeing no duplication, since the
    // overlap might have been in the buffer.
    if quick != 1 || prevent(P_DUPLICATION) != 0 {
        let mut clipped = DrawVec::new();

        // Do the clipping, even if we are going to include the whole feature,
        // so that we can know whether the feature itself, or only the feature's
        // bounding box, touches the tile.

        let t = sf.t as i32;
        if t == VT_LINE {
            clipped = clip_lines(&mut sf.geometry, z, buffer);
        }
        if t == VT_POLYGON {
            clipped = simple_clip_poly(
                &mut sf.geometry,
                z,
                buffer as i32,
                &mut sf.edge_nodes,
                prevent(P_SIMPLIFY_SHARED_NODES) != 0,
            );
        }
        if t == VT_POINT {
            clipped = clip_point(&mut sf.geometry, z, buffer);
        }

        clipped = remove_noop(clipped, t, 0);

        // Must clip at z0 even if we don't want clipping, to handle features
        // that are duplicated across the date line

        if prevent(P_DUPLICATION) != 0 && z != 0 {
            if point_within_tile(
                (sf.bbox[0] + sf.bbox[2]) / 2,
                (sf.bbox[1] + sf.bbox[3]) / 2,
                z,
            ) {
                // sf.geometry is unchanged
            } else {
                sf.geometry.clear();
            }
        } else if prevent(P_CLIPPING) != 0 && z != 0 {
            if clipped.is_empty() {
                sf.geometry.clear();
            } else {
                // sf.geometry is unchanged
            }
        } else {
            sf.geometry = clipped;
        }
    }

    false
}

// Removes the attributes named in --exclude, if any, from the feature
fn remove_attributes(sf: &mut SerialFeature, exclude_attributes: &BTreeSet<String>) {
    for i in (0..sf.keys.len()).rev() {
        let key = sf.stringpool.str_at(sf.keys[i]);
        if exclude_attributes.contains(key) {
            sf.keys.remove(i);
            sf.values.remove(i);
        }
    }

    for i in (0..sf.full_keys.len()).rev() {
        if exclude_attributes.contains(sf.full_keys[i].as_str()) {
            sf.full_keys.remove(i);
            sf.full_values.remove(i);
        }
    }
}

// This map maintains the count for attributes that resulted from the "mean"
// --accumulate-attribute option so that features' attributes can be averaged in
// without knowing their total count in advance.
#[derive(Default)]
struct MultiplierState {
    count: BTreeMap<String, i32>,
}

fn skip_next_feature(geoms: &mut Decompressor, geompos_in: &AtomicI64, compressed: bool) -> bool {
    let mut len: i64 = 0;

    if geoms.deserialize_long_long(&mut len, geompos_in) == 0 {
        eprintln!("Unexpected physical EOF in feature stream");
        std::process::exit(EXIT_READ);
    }
    if len <= 0 {
        if compressed {
            geoms.end(geompos_in);
        }

        return false;
    }

    let mut s = vec![0u8; len as usize];
    let n = geoms.fread(&mut s, 1, len as usize, geompos_in) as usize;
    if n != s.len() {
        eprintln!("Short read ({} for {}) from geometry", n, s.len());
        std::process::exit(EXIT_READ);
    }

    true
}

#[derive(Default)]
struct NextFeatureState {
    previndex: u64,
    prev_not_dropped_index: u64,
    // set when a feature excluded by tippecanoe_minzoom first appears beyond the next
    // zoom, so variable-depth pyramids don't leaf above the feature's minzoom and prune it away
    minzoom_feature_pending: bool,
}

/// Prints the tiling progress, as next_feature() and write_tile() both do.
#[allow(clippy::too_many_arguments)]
fn report_progress(
    geompos_in: &AtomicI64,
    along: &AtomicI64,
    alongminus: i64,
    todo: f64,
    pass: usize,
    z: i32,
    maxzoom: i32,
    tx: u32,
    ty: u32,
    oprogress: &AtomicF64,
) {
    let passes = pass + 1;
    let progress = (((((geompos_in.load(Ordering::Relaxed) + along.load(Ordering::Relaxed)
        - alongminus) as f64
        / todo)
        + pass as f64)
        / passes as f64
        + z as f64)
        / (maxzoom + 1) as f64
        * 1000.0)
        .floor()
        / 10.0;
    if progress >= oprogress.load() + 0.1 {
        if get(&QUIET) == 0 && get(&QUIET_PROGRESS) == 0 && progress_time() {
            eprint!("  {:3.1}%  {}/{}/{}  \r", progress, z, tx, ty);
        }
        if LOGGER.json_enabled() && progress_time() {
            LOGGER.progress_tile(progress);
        }
        oprogress.store(progress);
    }
}

// This function is called repeatedly from write_tile() to retrieve the next feature
// from the input stream. If the stream is at an end, it returns a feature with the
// geometry type set to -2.
//
// Its parameters other than the state are those in run_prefilter_args, which is
// what the C++ prefilter thread passes it.
fn next_feature(
    a: &mut RunPrefilterArgs<'_>,
    multiplier_state: &mut MultiplierState,
    next_feature_state: &mut NextFeatureState,
) -> SerialFeature {
    let z = a.z;
    let retain_points_multiplier = get(&RETAIN_POINTS_MULTIPLIER);
    let extra_multiplier_zooms = (retain_points_multiplier as f64).ln() / a.droprate.ln();

    loop {
        let mut sf;
        let mut len: i64 = 0;

        if a.geoms.deserialize_long_long(&mut len, a.geompos_in) == 0 {
            eprintln!("Unexpected physical EOF in feature stream");
            std::process::exit(EXIT_READ);
        }
        if len <= 0 {
            if a.compressed {
                a.geoms.end(a.geompos_in);
            }

            sf = SerialFeature::default();
            sf.t = -2;
            return sf;
        }

        let mut s = vec![0u8; len as usize];
        let n = a.geoms.fread(&mut s, 1, len as usize, a.geompos_in) as usize;
        if n != s.len() {
            eprintln!("Short read ({} for {}) from geometry", n, s.len());
            std::process::exit(EXIT_READ);
        }

        sf = deserialize_feature(&s, z as u32, a.tx, a.ty, a.initial_x, a.initial_y);
        sf.stringpool = StringPoolRef {
            pool: a.global_stringpool.pool.clone(),
            base: a.global_stringpool.base + a.pool_off[sf.segment as usize] as usize,
        };

        // with fractional zoom level, so we can target a specific number
        // of features to keep with retain-points-multiplier, not just the
        // powers of the drop rate
        //
        // the shift-right is because we lose the bottom two bits of
        // point coordinate precision in the calculation in serial.cpp.
        //
        // the fractional part should actually be scaled in an exponential
        // curve instead of linear, but I can't figure out how to make it
        // come out right, and this should be close enough.
        let feature_minzoom =
            sf.feature_minzoom as f64 - (bit_reverse(sf.index >> 2) as f64 / TWO_TO_THE_64);

        report_progress(
            a.geompos_in,
            a.along,
            a.alongminus,
            a.todo,
            a.pass,
            z,
            a.maxzoom,
            a.tx,
            a.ty,
            a.oprogress,
        );

        *a.original_features += 1;

        if sf.gap == 0 && sf.index != next_feature_state.previndex {
            let ox: i64 = (1i64 << (32 - z)).wrapping_mul(a.tx as i64);
            let oy: i64 = (1i64 << (32 - z)).wrapping_mul(a.ty as i64);

            let (wx1, wy1) = decode_index(next_feature_state.previndex);

            for g in &sf.geometry {
                let dx: i64 = wx1 as i64 - (g.x + ox);
                let dy: i64 = wy1 as i64 - (g.y + oy);

                let d = dx.wrapping_mul(dx).wrapping_add(dy.wrapping_mul(dy)) as u64;
                if d > sf.gap {
                    sf.gap = d;
                }
            }
        }
        next_feature_state.previndex = sf.index;

        if clip_to_tile(&mut sf, z, a.buffer as i64) {
            continue;
        }

        if !sf.geometry.is_empty() {
            *a.unclipped_features += 1;
        } else {
            // XXX should continue, but affects test outputs
        }

        if a.first_time && a.write_children {
            /* only write out the next zoom once, even if we retry */
            if sf.tippecanoe_maxzoom == -1 || sf.tippecanoe_maxzoom >= a.nextzoom {
                let segment = sf.segment;
                rewrite(
                    &mut sf,
                    z,
                    a.nextzoom,
                    a.maxzoom,
                    a.tx,
                    a.ty,
                    a.buffer,
                    a.within,
                    a.geompos,
                    a.start_geompos,
                    a.geomfile,
                    a.fname,
                    a.child_shards,
                    a.max_zoom_increment,
                    segment,
                    a.initial_x,
                    a.initial_y,
                );
            }
        }

        if z < a.minzoom {
            continue;
        }

        if sf.tippecanoe_minzoom != -1 && z < sf.tippecanoe_minzoom {
            // a leaf at z carries only z-visible content, so an excluded feature at
            // any deeper minzoom (even z + 1) must block leafing here
            next_feature_state.minzoom_feature_pending = true;
            continue;
        }
        if sf.tippecanoe_maxzoom != -1 && z > sf.tippecanoe_maxzoom {
            continue;
        }

        if let Some(filter) = a.filter {
            let mut attributes: HashMap<String, MvtValue> = HashMap::new();
            let layername = &a.layer_unmaps[sf.segment as usize][sf.layer as usize];
            let mut exclude_attributes: BTreeSet<String> = BTreeSet::new();

            for i in 0..sf.keys.len() {
                let key = sf.stringpool.str_at(sf.keys[i]).to_string();

                let sv = SerialVal::new(
                    sf.stringpool.type_at(sf.values[i]),
                    sf.stringpool.str_at(sf.values[i]),
                );

                let val = stringified_to_mvt_value(sv.type_, &sv.s);
                attributes.entry(key).or_insert(val);
            }

            for i in 0..sf.full_keys.len() {
                let key = (*sf.full_keys[i]).clone();
                let val = stringified_to_mvt_value(sf.full_values[i].type_, &sf.full_values[i].s);

                attributes.entry(key).or_insert(val);
            }

            if sf.has_id {
                let mut v = mvt_value_of_type(MVT_UINT);
                v.numeric_value.set_uint_value(sf.id);

                attributes.entry("$id".to_string()).or_insert(v);
            }

            let mut v = mvt_value_of_type(MVT_STRING);

            if sf.t as i32 == MVT_POINT {
                v.set_string_value("Point");
            } else if sf.t as i32 == MVT_LINESTRING {
                v.set_string_value("LineString");
            } else if sf.t as i32 == MVT_POLYGON {
                v.set_string_value("Polygon");
            }

            attributes.entry("$type".to_string()).or_insert(v);

            let mut v2 = mvt_value_of_type(MVT_UINT);
            v2.numeric_value.set_uint_value(z as u64);

            attributes.entry("$zoom".to_string()).or_insert(v2);

            if !evaluate(
                &attributes,
                layername,
                filter,
                &mut exclude_attributes,
                a.unidecode_data,
            ) {
                continue;
            }

            if !exclude_attributes.is_empty() {
                remove_attributes(&mut sf, &exclude_attributes);
            }
        }

        if sf.tippecanoe_minzoom == -1 {
            sf.dropped = FEATURE_DROPPED; // dropped

            let layername = &a.layer_unmaps[sf.segment as usize][sf.layer as usize];
            if !multiplier_state.count.contains_key(layername) {
                multiplier_state.count.insert(layername.clone(), 0);
                sf.dropped = FEATURE_KEPT; // the first feature in each tile is always kept
            }
            let count = multiplier_state.count.get_mut(layername).unwrap();

            let pmdt = get(&PRESERVE_MULTIPLIER_DENSITY_THRESHOLD);
            if z as f64 >= feature_minzoom || sf.dropped == FEATURE_KEPT {
                *count = 0;
                sf.dropped = FEATURE_KEPT; // feature is kept
            } else if z as f64 + extra_multiplier_zooms >= feature_minzoom
                && *count + 1 < retain_points_multiplier
            {
                *count += 1;
                sf.dropped = *count;
            } else if pmdt > 0
                && sf
                    .index
                    .wrapping_sub(next_feature_state.prev_not_dropped_index)
                    > ((1i64 << (32 - z)) as u64 / pmdt)
                        .wrapping_mul((1i64 << (32 - z)) as u64 / pmdt)
            {
                sf.dropped = FEATURE_ADDED_FOR_MULTIPLIER_DENSITY;
            } else {
                sf.dropped = FEATURE_DROPPED;
            }
        } else {
            sf.dropped = FEATURE_KEPT;
        }

        if sf.dropped != FEATURE_DROPPED {
            next_feature_state.prev_not_dropped_index = sf.index;
        }

        // Remove nulls, now that the expression evaluation filter has run

        for i in (0..sf.keys.len()).rev() {
            let type_ = sf.stringpool.type_at(sf.values[i]);

            if type_ == MVT_NULL {
                sf.keys.remove(i);
                sf.values.remove(i);
            }
        }

        for i in (0..sf.full_keys.len()).rev() {
            if sf.full_values[i].type_ == MVT_NULL {
                sf.full_keys.remove(i);
                sf.full_values.remove(i);
            }
        }

        return sf;
    }
}

struct RunPrefilterArgs<'a> {
    geoms: &'a mut Decompressor,
    geompos_in: &'a AtomicI64,
    z: i32,
    tx: u32,
    ty: u32,
    initial_x: &'a [u32],
    initial_y: &'a [u32],
    original_features: &'a mut i64,
    unclipped_features: &'a mut i64,
    nextzoom: i32,
    maxzoom: i32,
    minzoom: i32,
    max_zoom_increment: i32,
    pass: usize,
    along: &'a AtomicI64,
    alongminus: i64,
    buffer: i32,
    within: &'a mut [bool],
    geomfile: &'a mut [Compressor],
    geompos: &'a [AtomicI64],
    start_geompos: &'a mut [i64],
    oprogress: &'a AtomicF64,
    todo: f64,
    fname: &'a str,
    child_shards: i32,
    /// `(*layer_unmaps)[segment]` for the input segments, which are the only
    /// segments of the features read from the temporary files.
    layer_unmaps: &'a [Vec<String>],
    global_stringpool: &'a StringPoolRef,
    pool_off: &'a [i64],
    prefilter_fp: Option<PipeWriter>,
    filter: Option<JsonRef<'a>>,
    unidecode_data: &'a [String],
    first_time: bool,
    write_children: bool,
    compressed: bool,
    droprate: f64,
}

fn run_prefilter(mut rpa: RunPrefilterArgs<'_>) {
    let mut prefilter_fp = rpa.prefilter_fp.take().unwrap();

    {
        let mut state = JsonWriter::new_file(&mut prefilter_fp);
        let mut multiplier_state = MultiplierState::default();
        let mut next_feature_state = NextFeatureState::default();

        loop {
            let mut sf = next_feature(&mut rpa, &mut multiplier_state, &mut next_feature_state);
            if sf.t < 0 {
                break;
            }

            let mut tmp_layer = MvtLayer {
                extent: 1i64 << 32,
                name: rpa.layer_unmaps[sf.segment as usize][sf.layer as usize].clone(),
                ..Default::default()
            };

            if sf.t as i32 == VT_POLYGON {
                sf.geometry = close_poly(&mut sf.geometry);
            }

            let mut tmp_feature = MvtFeature {
                type_: sf.t as i32,
                geometry: to_feature(&sf.geometry),
                id: sf.id,
                has_id: sf.has_id,
                dropped: sf.dropped,
                ..Default::default()
            };

            // Offset from tile coordinates back to world coordinates
            let mut sx: u32 = 0;
            let mut sy: u32 = 0;
            if rpa.z != 0 {
                sx = rpa.tx << (32 - rpa.z);
                sy = rpa.ty << (32 - rpa.z);
            }
            for g in tmp_feature.geometry.iter_mut() {
                g.x += sx as i64;
                g.y += sy as i64;
            }

            decode_meta(&sf, &mut tmp_layer, &mut tmp_feature);
            tmp_layer.features.push(tmp_feature);

            layer_to_geojson(
                &tmp_layer,
                0,
                0,
                0,
                false,
                true,
                false,
                true,
                sf.index,
                sf.seq,
                sf.extent,
                true,
                &mut state,
                0.0,
                &BTreeSet::new(),
            );
        }
    }

    if let Err(e) = prefilter_fp.fclose() {
        if is_epipe(&e) {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.load(Ordering::Relaxed) {
                eprintln!("Warning: broken pipe in prefilter");
                WARNED.store(true, Ordering::Relaxed);
            }
        } else {
            perror_io("fclose output to prefilter", &e);
            std::process::exit(EXIT_CLOSE);
        }
    }
}

/// `layermaps` and `layer_unmaps` here are the entries for `tiling_seg`.
#[allow(clippy::too_many_arguments)]
fn add_tilestats(
    layername: &str,
    z: i32,
    layermap: &mut BTreeMap<String, LayermapEntry>,
    tiling_seg: usize,
    layer_unmap: &mut Vec<String>,
    key: &str,
    val: &SerialVal,
) {
    let _ = tiling_seg;
    if !layermap.contains_key(layername) {
        let mut lme = LayermapEntry::new(layermap.len());
        lme.minzoom = z;
        lme.maxzoom = z;
        lme.retain = 1;
        let id = lme.id;

        layermap.insert(layername.to_string(), lme);

        if id >= layer_unmap.len() {
            layer_unmap.resize(id + 1, String::new());
            layer_unmap[id] = layername.to_string();
        }
    }
    let ts = match layermap.get_mut(layername) {
        Some(ts) => ts,
        None => {
            eprintln!("Internal error: layer {} not found", layername);
            std::process::exit(EXIT_IMPOSSIBLE);
        }
    };

    add_to_tilestats(&mut ts.tilestats, key, val);
}

fn promote_attribute(key: &str, p: &mut SerialFeature, key_pool: &mut KeyPool) {
    // If the feature being merged into has this key as a metadata reference,
    // promote it to a full_key so it can be modified

    for i in 0..p.keys.len() {
        if key == p.stringpool.str_at(p.keys[i]) {
            let sv = SerialVal::new(
                p.stringpool.type_at(p.values[i]),
                p.stringpool.str_at(p.values[i]),
            );

            p.full_keys.push(key_pool.pool(key));
            p.full_values.push(sv);

            p.keys.remove(i);
            p.values.remove(i);

            break;
        }
    }
}

#[allow(dead_code)]
fn promote_attribute_prefix(
    key: &str,
    prefixed_key: &str,
    p: &mut SerialFeature,
    key_pool: &mut KeyPool,
) {
    // does the prefixed attribute already exist as a full key?
    let mut found_as: isize = -1;
    for i in 0..p.full_keys.len() {
        if prefixed_key == *p.full_keys[i] {
            // yes, so we're done
            return;
        }
        if key == *p.full_keys[i] {
            found_as = i as isize;
        }
    }

    // or did we find the source as a full key? then copy it
    if found_as >= 0 {
        p.full_keys.push(key_pool.pool(prefixed_key));
        let v = p.full_values[found_as as usize].clone();
        p.full_values.push(v);

        return;
    }

    // does the prefix attribute exist as a reference key?
    found_as = -1;
    for i in 0..p.keys.len() {
        if prefixed_key == p.stringpool.str_at(p.keys[i]) {
            // yes, so promote it to a full key
            let sv = SerialVal::new(
                p.stringpool.type_at(p.values[i]),
                p.stringpool.str_at(p.values[i]),
            );

            p.full_keys.push(key_pool.pool(prefixed_key));
            p.full_values.push(sv);

            p.keys.remove(i);
            p.values.remove(i);

            return;
        } else if key == p.stringpool.str_at(p.keys[i]) {
            found_as = i as isize;
        }
    }

    // or did we find the source as a reference key? then copy it
    if found_as >= 0 {
        let f = found_as as usize;
        let sv = SerialVal::new(
            p.stringpool.type_at(p.values[f]),
            p.stringpool.str_at(p.values[f]),
        );

        p.full_keys.push(key_pool.pool(prefixed_key));
        p.full_values.push(sv);
    }

    // otherwise it does not exist, so preserve_attribute() will create it
}

// accumulate attribute values from sf onto p
fn preserve_attributes(
    attribute_accum: &HashMap<String, AttributeOp>,
    sf: &SerialFeature,
    p: &mut SerialFeature,
    key_pool: &mut KeyPool,
) {
    for i in 0..sf.keys.len() {
        let key = sf.stringpool.str_at(sf.keys[i]);

        if let Some(op) = attribute_accum.get(key) {
            let sv = SerialVal::new(
                sf.stringpool.type_at(sf.values[i]),
                sf.stringpool.str_at(sf.values[i]),
            );

            promote_attribute(key, p, key_pool);
            preserve_attribute(op, key, &sv, &mut p.full_keys, &mut p.full_values, key_pool);
        }
    }
    for i in 0..sf.full_keys.len() {
        let key: &str = &sf.full_keys[i];

        if let Some(op) = attribute_accum.get(key) {
            let sv = &sf.full_values[i];

            promote_attribute(key, p, key_pool); // promotes it in the target feature
            preserve_attribute(op, key, sv, &mut p.full_keys, &mut p.full_values, key_pool);
        }
    }
}

// This function finds the feature in `features` onto which the attributes or geometry
// of a feature that is being dropped (`sf`) will be accumulated or coalesced. It
// ordinarily returns the most recently-added feature from the same layer as the feature
// that is being dropped.
//
fn find_feature_to_accumulate_onto(
    features: &[SerialFeaturePtr],
    sf: &SerialFeature,
    out: &mut isize,
    layer_unmaps: &LayerUnmaps<'_>,
    maxextent: i64,
) -> bool {
    for i in (1..=features.len()).rev() {
        if features[i - 1].t == sf.t {
            let layername1 = layer_unmaps.get(features[i - 1].segment, features[i - 1].layer);
            let layername2 = layer_unmaps.get(sf.segment, sf.layer);

            if layername1 == layername2 && features[i - 1].extent <= maxextent {
                *out = (i - 1) as isize;
                return true;
            }
        }
    }

    false
}

// Keep only a sample of 100K extents for feature dropping,
// to avoid spending lots of memory on a complete list when there are
// hundreds of millions of features.
fn add_sample_to<T: Copy>(vals: &mut Vec<T>, val: T, increment: &mut usize, seq: usize) {
    if seq % *increment == 0 {
        vals.push(val);

        if vals.len() > 100000 {
            let mut tmp = Vec::new();

            let mut i = 0;
            while i < vals.len() {
                tmp.push(vals[i]);
                i += 2;
            }

            *increment *= 2;
            *vals = tmp;
        }
    }
}

fn coalesce_geometry(p: &mut SerialFeature, sf: &SerialFeature) {
    // XXX need another way to deduplicate here

    p.geometry.extend_from_slice(&sf.geometry);
}

// This is the structure that the features from each layer are accumulated into
#[derive(Default)]
struct LayerFeatures {
    features: Vec<SerialFeaturePtr>, // The features of this layer, so far
    multiplier_cluster_size: usize,  // The feature count of the current multiplier cluster
}

#[allow(clippy::too_many_arguments)]
fn drop_feature_unless_it_can_be_added_to_a_multiplier_cluster(
    layer: &mut LayerFeatures,
    sf: &mut SerialFeature,
    layer_unmaps: &LayerUnmaps<'_>,
    strategy: &mut Strategy,
    drop_rest: &mut bool,
    attribute_accum: &HashMap<String, AttributeOp>,
    key_pool: &mut KeyPool,
) -> bool {
    let mut which_serial_feature: isize = 0;

    if find_feature_to_accumulate_onto(
        &layer.features,
        sf,
        &mut which_serial_feature,
        layer_unmaps,
        i64::MAX,
    ) {
        strategy.dropped_as_needed += 1;
        if layer.multiplier_cluster_size < get(&RETAIN_POINTS_MULTIPLIER) as usize {
            // we have capacity to keep this feature as part of an existing multiplier cluster that isn't full yet
            // so do that instead of dropping it
            sf.dropped = (layer.multiplier_cluster_size + 1) as i32;
            return false; // converted rather than dropped
        } else {
            preserve_attributes(
                attribute_accum,
                sf,
                &mut layer.features[which_serial_feature as usize],
                key_pool,
            );
            *drop_rest = true;
            return true; // dropped
        }
    }

    false // did not drop because nothing could be found to accumulate attributes onto
}

fn skip_tile(geoms: &mut Decompressor, geompos_in: &AtomicI64, compressed_input: bool) {
    while skip_next_feature(geoms, geompos_in, compressed_input) {}
}

/// The C++ `static std::atomic<double> oprogress` of write_tile().
static OPROGRESS: AtomicF64 = AtomicF64::new(0.0);

/// The cluster attributes are averaged onto the feature they are merged into.
fn average_cluster_point(target: &mut SerialFeature, sf: &SerialFeature) {
    let mut x = target.geometry[0].x as f64 * target.clustered as f64;
    let mut y = target.geometry[0].y as f64 * target.clustered as f64;
    x += sf.geometry[0].x as f64;
    y += sf.geometry[0].y as f64;
    target.geometry[0].x = (x / (target.clustered + 1) as f64) as i64;
    target.geometry[0].y = (y / (target.clustered + 1) as f64) as i64;
}

#[allow(clippy::too_many_arguments)]
fn write_tile(
    geoms: &mut Decompressor,
    geompos_in: &AtomicI64,
    z: i32,
    tx: u32,
    ty: u32,
    detail: i32,
    alongminus: i64,
    arg: &mut WriteTileArgs<'_>,
    estimated_complexity: i64,
    write_children: bool,
) -> i64 {
    let global_stringpool = arg.global_stringpool;
    let min_detail = arg.min_detail;
    let outdb = arg.outdb;
    let outdir = arg.outdir;
    let buffer = arg.buffer;
    let fname = arg.fname;
    let geomfile: &mut [Compressor] = &mut *arg.geomfile;
    let geompos = arg.geompos;
    let minzoom = arg.minzoom;
    let maxzoom = arg.maxzoom;
    let todo = arg.todo;
    let along = arg.along;
    let mut gamma = arg.gamma;
    let child_shards = arg.child_shards;
    let pool_off = arg.pool_off;
    let initial_x = arg.initial_x;
    let initial_y = arg.initial_y;
    let running = arg.running;
    let simplification = arg.simplification;
    let layermap: &mut BTreeMap<String, LayermapEntry> = &mut *arg.layermap;
    let input_unmaps = arg.input_unmaps;
    let layer_unmap: &mut Vec<String> = &mut *arg.layer_unmap;
    let tiling_seg = arg.tiling_seg;
    let pass = arg.pass;
    let mut mingap = arg.mingap;
    let mut minextent = arg.minextent;
    let mut mindrop_sequence = arg.mindrop_sequence;
    let mut minattribute = arg.minattribute;
    let mut prefilter = arg.prefilter;
    let mut postfilter = arg.postfilter;
    let filter = arg.filter;
    let strategy_out = arg.strategy;
    let compressed_input = arg.compressed;
    let shared_nodes_map = arg.shared_nodes_map;
    let nodepos = arg.nodepos;
    let shared_nodes_bloom = arg.shared_nodes_bloom;
    let unidecode_data = arg.unidecode_data;
    let attribute_accum = arg.attribute_accum;
    let drop_by_attribute_descending = arg.drop_by_attribute_descending;
    let drop_by_attribute_as_needed_attribute =
        arg.drop_by_attribute_as_needed_attribute.unwrap_or("");
    let droprate = arg.droprate;

    let retain_points_multiplier = get(&RETAIN_POINTS_MULTIPLIER);
    let max_tile_size = get(&MAX_TILE_SIZE);
    let max_tile_features = get(&MAX_TILE_FEATURES);
    let cluster_distance = get(&CLUSTER_DISTANCE);
    let cluster_maxzoom = get(&CLUSTER_MAXZOOM);
    let quiet = get(&QUIET);

    let merge_fraction = 1.0;
    let mut mingap_fraction = 1.0;
    let mut minextent_fraction = 1.0;
    let mut mindrop_sequence_fraction = 1.0;
    let mut minattribute_fraction = 1.0;

    let oprogress = &OPROGRESS;
    let og = geompos_in.load(Ordering::Relaxed);

    // XXX is there a way to do this without floating point?
    let max_zoom_increment = ((child_shards as f64).ln() / 4f64.ln()) as i32;
    if child_shards < 4 || max_zoom_increment < 1 {
        eprintln!(
            "Internal error: {} shards, max zoom increment {}",
            child_shards, max_zoom_increment
        );
        std::process::exit(EXIT_IMPOSSIBLE);
    }
    if (((child_shards - 1) << 1) & child_shards) != child_shards {
        eprintln!("Internal error: {} shards not a power of 2", child_shards);
        std::process::exit(EXIT_IMPOSSIBLE);
    }

    let mut nextzoom = z + 1;
    if nextzoom < minzoom {
        if z + max_zoom_increment > minzoom {
            nextzoom = minzoom;
        } else {
            nextzoom = z + max_zoom_increment;
        }
    }

    // only for -K
    let cluster_mingap: u64 = (((1i64 << (32 - z)) / 256 * cluster_distance as i64)
        .wrapping_mul((1i64 << (32 - z)) / 256 * cluster_distance as i64))
        as u64;

    let mut first_detail = detail;
    let mut second_detail = detail - 1;
    let mut trying_to_stop_early = false;
    let mut can_stop_early = true;
    if additional(A_VARIABLE_DEPTH_PYRAMID) != 0
        && !dropping_features(
            mingap,
            minextent,
            mindrop_sequence,
            minattribute,
            drop_by_attribute_descending,
        )
    {
        // If we are trying to stop early, there is an extra first pass with full+extra detail,
        // and which loops if everything doesn't fit rather than trying to drop or union features.

        // empirical estimate from ne_10m_admin_0_countries, CPAD units, Cal fires.
        // only try to make an overzoomable final tile if it seems like it might work
        //
        // PORT: clang fuses this multiply and subtract into one fmadd.
        let estimated_output_tile_size =
            (estimated_complexity as f64).mul_add(0.6693, -3.36e+04) as i64;
        if estimated_output_tile_size < (0.9 * max_tile_size as f64) as i64 && 30 - z > detail {
            first_detail = 30 - z;
            second_detail = detail;
            trying_to_stop_early = true;
        }
    }

    let mut detail_reduced: usize = 0;
    let mut first_time = true;
    // This only loops if the tile data didn't fit, in which case the detail
    // goes down and the progress indicator goes backward for the next try.
    let next_detail = |line_detail: i32| {
        if line_detail == first_detail {
            second_detail
        } else {
            line_detail - 1
        }
    };
    let mut line_detail = first_detail;
    while line_detail >= min_detail || line_detail == detail {
        oprogress.store(0.0);

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

        let mut original_features: i64 = 0;
        let mut unclipped_features: i64 = 0;

        let mut layers: BTreeMap<String, LayerFeatures> = BTreeMap::new();

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

        let mut lead_features_count: usize = 0; // of the tile so far
        let mut other_multiplier_cluster_features_count: usize = 0; // of the tile so far

        let mut too_many_features = false;
        let mut too_many_bytes = false;

        let mut key_pool = KeyPool::default();

        let mut within: Vec<bool> = vec![false; child_shards as usize];
        let mut start_geompos: Vec<i64> = vec![-1; child_shards as usize];

        if geompos_in.load(Ordering::Relaxed) != og {
            if compressed_input {
                if geoms.within {
                    geoms.end(geompos_in);
                }

                geoms.begin();
            }

            if geoms.fseek(og) != 0 {
                perror("fseek geom");
                std::process::exit(EXIT_SEEK);
            }

            geompos_in.store(og, Ordering::Relaxed);
            geoms.clear_avail();
        }

        let mut prefilter_write: i32 = -1;
        let mut prefilter_read: i32 = -1;
        let mut prefilter_pid: libc::pid_t = 0;

        if z < minzoom {
            prefilter = None;
            postfilter = None;
        }

        // Read features, filter them, assign them to layers

        let mut multiplier_state = MultiplierState::default();
        let mut drop_rest = false; // are we dropping the remainder of a multiplier cluster whose first point was dropped?
        let mut dropping_by_rate = false; // are we dropping anything by rate in this tile, or keeping it only as part of a multiplier?

        let mut next_feature_state = NextFeatureState::default();

        let mut strategy = Strategy {
            detail_reduced,
            ..Default::default()
        };

        let rpa = RunPrefilterArgs {
            geoms: &mut *geoms,
            geompos_in,
            z,
            tx,
            ty,
            initial_x,
            initial_y,
            original_features: &mut original_features,
            unclipped_features: &mut unclipped_features,
            nextzoom,
            maxzoom,
            minzoom,
            max_zoom_increment,
            pass,
            along,
            alongminus,
            buffer,
            within: &mut within,
            geomfile: &mut *geomfile,
            geompos,
            start_geompos: &mut start_geompos,
            oprogress,
            todo,
            fname,
            child_shards,
            layer_unmaps: input_unmaps,
            global_stringpool,
            pool_off,
            prefilter_fp: None,
            filter,
            unidecode_data,
            first_time,
            write_children,
            compressed: compressed_input,
            droprate,
        };

        std::thread::scope(|scope| {
            let mut direct: Option<RunPrefilterArgs<'_>> = None;
            let mut prefilter_writer = None;
            let mut prefilter_read_fp: Option<File> = None;

            match prefilter {
                Some(prefilter) => {
                    setup_filter(
                        prefilter,
                        &mut prefilter_write,
                        &mut prefilter_read,
                        &mut prefilter_pid,
                        z as u32,
                        tx,
                        ty,
                    );
                    let mut rpa = rpa;
                    rpa.prefilter_fp = Some(PipeWriter::fdopen(prefilter_write));

                    // this does need to be a real thread, so we can pipe both to and from it
                    match std::thread::Builder::new()
                        .spawn_scoped(scope, move || run_prefilter(rpa))
                    {
                        Ok(w) => prefilter_writer = Some(w),
                        Err(_) => {
                            perror("pthread_create (prefilter writer)");
                            std::process::exit(EXIT_PTHREAD);
                        }
                    }

                    // SAFETY: prefilter_read is the read end of the filter's output pipe, owned here.
                    prefilter_read_fp = Some(unsafe { File::from_raw_fd(prefilter_read) });
                }
                None => direct = Some(rpa),
            }
            let mut prefilter_jp: Option<JsonPull<'_>> = prefilter_read_fp
                .as_ref()
                .map(|f| JsonPull::begin_file(Box::new(f)));

            for seq in 0usize.. {
                let mut which_serial_feature: isize = -1;

                let mut sf = match direct.as_mut() {
                    Some(rpa) => next_feature(rpa, &mut multiplier_state, &mut next_feature_state),
                    None => parse_feature(
                        prefilter_jp.as_mut().unwrap(),
                        z,
                        tx,
                        ty,
                        layermap,
                        tiling_seg,
                        layer_unmap,
                        postfilter.is_some(),
                        &mut key_pool,
                    ),
                };

                if sf.t < 0 {
                    break;
                }

                let layer_unmaps = LayerUnmaps {
                    input: input_unmaps,
                    tiling_seg,
                    own: layer_unmap,
                };

                let layername = layer_unmaps.get(sf.segment, sf.layer);
                if !layers.contains_key(layername) {
                    layers.insert(layername.to_string(), LayerFeatures::default());
                }
                let layer: &mut LayerFeatures = layers.get_mut(layername).unwrap();

                if sf.t as i32 == VT_POINT {
                    if extent_previndex >= sf.index {
                        sf.extent = 1;
                    } else {
                        let radius = ((sf.index - extent_previndex) as f64).sqrt() / 4.0;
                        sf.extent = (std::f64::consts::PI * radius * radius) as i64;
                        if sf.extent < 1 {
                            sf.extent = 1;
                        }
                    }

                    extent_previndex = sf.index;
                }

                // Make label anchors early in tiling, even though it requires simplifying early,
                // so that if there is no label anchor for this feature in this tile,
                // we find out now rather than after we have already decided that there
                // are too_many_bytes.
                //
                // label anchors also need to happen before as-needed dropping and coalescing,
                // so that the geometry type matches for find_feature_to_accumulate_onto.
                // (or it could happen much later, after all the features are accumulated,
                // but then it would be too late for too_many_bytes)
                if sf.t as i32 == VT_POLYGON && additional(A_GENERATE_POLYGON_LABEL_POINTS) != 0 {
                    // exclude features that are invisibly small at this zoom level
                    if line_is_too_small(&sf.geometry, z, line_detail) {
                        continue;
                    }
                    if sf.t as i32 == VT_POLYGON && get_mp_area(&sf.geometry) <= 0.0 {
                        continue;
                    }

                    let ngeom = simplify_lines(
                        &mut sf.geometry,
                        z,
                        tx as i32,
                        ty as i32,
                        line_detail,
                        !(prevent(P_CLIPPING) != 0 || prevent(P_DUPLICATION) != 0),
                        sf.simplification,
                        if sf.t as i32 == VT_POLYGON { 4 } else { 0 },
                        &shared_nodes,
                        &[],
                        0,
                        &[],
                    );
                    if ngeom.is_empty() {
                        continue;
                    }
                    sf.geometry =
                        checkerboard_anchors(&ngeom, tx as i32, ty as i32, z, sf.label_point);
                    if sf.geometry.is_empty() {
                        continue;
                    }
                    sf.t = VT_POINT as i8;
                }

                let mut drop_sequence: u64 = 0;
                if additional(A_COALESCE_FRACTION_AS_NEEDED) != 0
                    || additional(A_DROP_FRACTION_AS_NEEDED) != 0
                    || prevent(P_DYNAMIC_DROP) != 0
                {
                    drop_sequence = calculate_drop_sequence(&sf);
                }

                if sf.feature_minzoom as i32 > z + 1 {
                    // if there is a feature whose first appearance is beyond the next zoom,
                    // prevent stopping early at the next zoom
                    dropping_by_rate = true;
                }

                if sf.dropped == FEATURE_KEPT {
                    // this is a new multiplier cluster, so stop dropping features
                    // that were dropped because the previous lead feature was dropped
                    drop_rest = false;
                } else {
                    can_stop_early = false;

                    if sf.dropped != FEATURE_DROPPED
                        && sf.dropped != FEATURE_ADDED_FOR_MULTIPLIER_DENSITY
                    {
                        // Does the current multiplier cluster already have too many features?
                        // (Because we are dropping dynamically, and we have already filled the
                        // cluster with features that were dynamically dropped from being
                        // primary features)
                        // If so, we have to drop this one, even if it would potentially qualify
                        // as a secondary feature to be exposed by filtering
                        if layer.multiplier_cluster_size >= retain_points_multiplier as usize {
                            sf.dropped = FEATURE_DROPPED;
                        }
                    }
                }

                if (sf.dropped == FEATURE_DROPPED || drop_rest)
                    && find_feature_to_accumulate_onto(
                        &layer.features,
                        &sf,
                        &mut which_serial_feature,
                        &layer_unmaps,
                        i64::MAX,
                    )
                {
                    preserve_attributes(
                        attribute_accum,
                        &sf,
                        &mut layer.features[which_serial_feature as usize],
                        &mut key_pool,
                    );
                    strategy.dropped_by_rate += 1;
                    can_stop_early = false;
                    continue;
                }

                // only the first point of a multiplier cluster can be dropped
                // by any of these mechanisms. (but if one is, it drags the whole
                // cluster down with it by setting drop_rest).
                if sf.dropped == FEATURE_KEPT {
                    if gamma > 0.0
                        && manage_gap(sf.index, &mut previndex, scale, gamma, &mut gap) != 0
                        && find_feature_to_accumulate_onto(
                            &layer.features,
                            &sf,
                            &mut which_serial_feature,
                            &layer_unmaps,
                            i64::MAX,
                        )
                    {
                        preserve_attributes(
                            attribute_accum,
                            &sf,
                            &mut layer.features[which_serial_feature as usize],
                            &mut key_pool,
                        );
                        strategy.dropped_by_gamma += 1;
                        drop_rest = true;
                        can_stop_early = false;
                        continue;
                    }

                    if z <= cluster_maxzoom && cluster_distance != 0 {
                        // This still uses merge_previndex instead of sf.gap
                        // because the cluster size in -K is expecting to specify
                        // distances between points that are subject to dot-dropping,
                        // rather than wanting each feature to have a consistent
                        // idea of density between zooms.
                        if (sf.index < merge_previndex
                            || sf.index - merge_previndex < cluster_mingap)
                            && find_feature_to_accumulate_onto(
                                &layer.features,
                                &sf,
                                &mut which_serial_feature,
                                &layer_unmaps,
                                i64::MAX,
                            )
                        {
                            let target = &mut layer.features[which_serial_feature as usize];
                            target.clustered += 1;

                            if additional(A_KEEP_POINT_CLUSTER_POSITION) == 0
                                && target.t as i32 == VT_POINT
                                && target.geometry.len() == 1
                                && sf.geometry.len() == 1
                            {
                                average_cluster_point(target, &sf);
                            }

                            preserve_attributes(attribute_accum, &sf, target, &mut key_pool);
                            strategy.coalesced_as_needed += 1;
                            drop_rest = true;
                            can_stop_early = false;
                            continue;
                        }
                    } else if additional(A_DROP_DENSEST_AS_NEEDED) != 0 {
                        add_sample_to(&mut gaps, sf.gap, &mut gaps_increment, seq);
                        if sf.gap < mingap {
                            can_stop_early = false;
                            if drop_feature_unless_it_can_be_added_to_a_multiplier_cluster(
                                layer,
                                &mut sf,
                                &layer_unmaps,
                                &mut strategy,
                                &mut drop_rest,
                                attribute_accum,
                                &mut key_pool,
                            ) {
                                continue;
                            }
                        }
                    } else if z <= cluster_maxzoom && additional(A_CLUSTER_DENSEST_AS_NEEDED) != 0 {
                        // this is now just like coalesce-densest, except that instead of unioning the geometry,
                        // it averages the point locations
                        add_sample_to(&mut gaps, sf.gap, &mut gaps_increment, seq);
                        if sf.gap < mingap
                            && find_feature_to_accumulate_onto(
                                &layer.features,
                                &sf,
                                &mut which_serial_feature,
                                &layer_unmaps,
                                i64::MAX,
                            )
                        {
                            let target = &mut layer.features[which_serial_feature as usize];
                            target.clustered += 1;

                            if target.t as i32 == VT_POINT
                                && target.geometry.len() == 1
                                && sf.geometry.len() == 1
                            {
                                average_cluster_point(target, &sf);
                            }

                            preserve_attributes(attribute_accum, &sf, target, &mut key_pool);
                            strategy.coalesced_as_needed += 1;
                            drop_rest = true;
                            continue;
                        }
                    } else if additional(A_COALESCE_DENSEST_AS_NEEDED) != 0 {
                        add_sample_to(&mut gaps, sf.gap, &mut gaps_increment, seq);
                        if sf.gap < mingap
                            && find_feature_to_accumulate_onto(
                                &layer.features,
                                &sf,
                                &mut which_serial_feature,
                                &layer_unmaps,
                                i64::MAX,
                            )
                        {
                            let target = &mut layer.features[which_serial_feature as usize];
                            if sf.t as i32 == VT_POINT
                                || !line_is_too_small(&sf.geometry, z, line_detail)
                            {
                                coalesce_geometry(target, &sf);
                            }
                            target.coalesced = true;
                            coalesced_area += sf.extent as f64;
                            preserve_attributes(attribute_accum, &sf, target, &mut key_pool);
                            strategy.coalesced_as_needed += 1;
                            drop_rest = true;
                            can_stop_early = false;
                            continue;
                        }
                    } else if additional(A_DROP_SMALLEST_AS_NEEDED) != 0 {
                        add_sample_to(&mut extents, sf.extent, &mut extents_increment, seq);
                        // search here is for LLONG_MAX, not minextent, because we are dropping features, not coalescing them,
                        // so we shouldn't expect to find anything small that we can related this feature to.
                        if minextent != 0 && sf.extent as f64 + coalesced_area <= minextent as f64 {
                            can_stop_early = false;
                            if drop_feature_unless_it_can_be_added_to_a_multiplier_cluster(
                                layer,
                                &mut sf,
                                &layer_unmaps,
                                &mut strategy,
                                &mut drop_rest,
                                attribute_accum,
                                &mut key_pool,
                            ) {
                                continue;
                            }
                        }
                    } else if additional(A_COALESCE_SMALLEST_AS_NEEDED) != 0 {
                        add_sample_to(&mut extents, sf.extent, &mut extents_increment, seq);
                        if minextent != 0
                            && sf.extent as f64 + coalesced_area <= minextent as f64
                            && find_feature_to_accumulate_onto(
                                &layer.features,
                                &sf,
                                &mut which_serial_feature,
                                &layer_unmaps,
                                minextent,
                            )
                        {
                            let target = &mut layer.features[which_serial_feature as usize];
                            if sf.t as i32 == VT_POINT
                                || !line_is_too_small(&sf.geometry, z, line_detail)
                            {
                                coalesce_geometry(target, &sf);
                            }
                            target.coalesced = true;
                            coalesced_area += sf.extent as f64;
                            preserve_attributes(attribute_accum, &sf, target, &mut key_pool);
                            strategy.coalesced_as_needed += 1;
                            drop_rest = true;
                            can_stop_early = false;
                            continue;
                        }
                    } else if additional(A_DROP_FRACTION_AS_NEEDED) != 0
                        || prevent(P_DYNAMIC_DROP) != 0
                    {
                        add_sample_to(
                            &mut drop_sequences,
                            drop_sequence,
                            &mut drop_sequences_increment,
                            seq,
                        );
                        if mindrop_sequence != 0 && drop_sequence <= mindrop_sequence {
                            can_stop_early = false;
                            if drop_feature_unless_it_can_be_added_to_a_multiplier_cluster(
                                layer,
                                &mut sf,
                                &layer_unmaps,
                                &mut strategy,
                                &mut drop_rest,
                                attribute_accum,
                                &mut key_pool,
                            ) {
                                continue;
                            }
                        }
                    } else if additional(A_COALESCE_FRACTION_AS_NEEDED) != 0 {
                        add_sample_to(
                            &mut drop_sequences,
                            drop_sequence,
                            &mut drop_sequences_increment,
                            seq,
                        );
                        if mindrop_sequence != 0
                            && drop_sequence <= mindrop_sequence
                            && find_feature_to_accumulate_onto(
                                &layer.features,
                                &sf,
                                &mut which_serial_feature,
                                &layer_unmaps,
                                i64::MAX,
                            )
                        {
                            let target = &mut layer.features[which_serial_feature as usize];
                            if sf.t as i32 == VT_POINT
                                || !line_is_too_small(&sf.geometry, z, line_detail)
                            {
                                coalesce_geometry(target, &sf);
                            }
                            target.coalesced = true;
                            preserve_attributes(attribute_accum, &sf, target, &mut key_pool);
                            strategy.coalesced_as_needed += 1;
                            drop_rest = true;
                            can_stop_early = false;
                            continue;
                        }
                    } else if additional(A_DROP_BY_ATTRIBUTE_AS_NEEDED) != 0 {
                        let attr_val =
                            find_attribute_value(&sf, drop_by_attribute_as_needed_attribute);
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
                            if should_drop {
                                can_stop_early = false;
                                if drop_feature_unless_it_can_be_added_to_a_multiplier_cluster(
                                    layer,
                                    &mut sf,
                                    &layer_unmaps,
                                    &mut strategy,
                                    &mut drop_rest,
                                    attribute_accum,
                                    &mut key_pool,
                                ) {
                                    continue;
                                }
                            }
                        }
                    }
                }

                if additional(A_CALCULATE_FEATURE_DENSITY) != 0 {
                    // Gamma is always 1 for this calculation so there is a reasonable
                    // interpretation when no features are being dropped.
                    // The spacing is only calculated if a feature would be retained by
                    // that standard, so that duplicates aren't reported as infinitely dense.

                    let o_density_previndex = density_previndex as f64;
                    if manage_gap(
                        sf.index,
                        &mut density_previndex,
                        scale,
                        1.0,
                        &mut density_gap,
                    ) == 0
                    {
                        spacing = (sf.index as f64 - o_density_previndex) / scale;
                    }
                }

                let mut still_need_simplification_after_reduction = false;
                if sf.t as i32 == VT_POLYGON {
                    let mut simplified_away_by_reduction = false;

                    let prevent_tiny = prevent(P_TINY_POLYGON_REDUCTION) != 0
                        || (prevent(P_TINY_POLYGON_REDUCTION_AT_MAXZOOM) != 0 && z == maxzoom);
                    if !prevent_tiny && additional(A_GRID_LOW_ZOOMS) == 0 {
                        sf.geometry = reduce_tiny_poly(
                            &sf.geometry,
                            z,
                            line_detail,
                            &mut still_need_simplification_after_reduction,
                            &mut simplified_away_by_reduction,
                            &mut accum_area,
                            get(&TINY_POLYGON_SIZE) as f64,
                        );
                        if simplified_away_by_reduction {
                            strategy.tiny_polygons += 1;
                        }
                        if sf.geometry.is_empty() {
                            continue;
                        }
                    } else {
                        still_need_simplification_after_reduction = true; // reduction skipped, so always simplify
                    }
                } else {
                    still_need_simplification_after_reduction = true; // not a polygon, so simplify
                }

                if (sf.t as i32 == VT_POLYGON || sf.t as i32 == VT_LINE)
                    && line_is_too_small(&sf.geometry, z, line_detail)
                {
                    continue;
                }

                let sfindex = sf.index;

                if !sf.geometry.is_empty() {
                    // There are two adjustments that we need to make
                    // when determining if we have hit the feature count
                    // or byte size limit:
                    //
                    // The max_tile_size and max_tile_features are inflated
                    // to account for the number of multiplier clusters features
                    // we are carrying around in addition to their lead features.

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

                    // The number of features in the tile, meanwhile, is inflated
                    // to account for the number of features that we have skipped over
                    // because we were already over the limit, in addition to those
                    // that we have actually added to the layer (as either lead or
                    // multiplier features).

                    let mut adjusted_feature_count =
                        lead_features_count + other_multiplier_cluster_features_count;
                    if kept > 0 {
                        adjusted_feature_count =
                            adjusted_feature_count.wrapping_mul(skipped + kept) / kept;
                    }

                    if too_many_bytes
                        || (adjusted_feature_count > adjusted_max_tile_size
                            && prevent(P_KILOBYTE_LIMIT) == 0)
                    {
                        // Even being maximally conservative, each feature is still going to be
                        // at least one byte in the output tile, so this can't possibly work.
                        skipped += 1;
                        too_many_bytes = true;
                    } else if too_many_features
                        || (adjusted_feature_count > adjusted_max_tile_features
                            && prevent(P_FEATURE_LIMIT) == 0)
                    {
                        skipped += 1;
                        too_many_features = true;
                    } else {
                        kept += 1;

                        if layer.features.is_empty() {
                            // the first feature of the the tile is always kept.
                            // it may not have been marked kept in next_feature
                            // if the previous feature was nominally the first
                            // but has already been lost because its geometry was
                            // clipped away
                            sf.dropped = FEATURE_KEPT;
                        }

                        if sf.dropped == FEATURE_KEPT && retain_points_multiplier > 1 {
                            sf.full_keys
                                .push(key_pool.pool("tippecanoe:retain_points_multiplier_first"));
                            sf.full_values.push(SerialVal::new(MVT_BOOL, "true"));
                        }

                        if sf.dropped == FEATURE_KEPT {
                            layer.multiplier_cluster_size = 1;
                            lead_features_count += 1;
                        } else if sf.dropped == FEATURE_ADDED_FOR_MULTIPLIER_DENSITY {
                            other_multiplier_cluster_features_count += 1;
                        } else {
                            layer.multiplier_cluster_size += 1;
                            other_multiplier_cluster_features_count += 1;
                        }

                        shared_nodes.extend_from_slice(&sf.edge_nodes);

                        sf.reduced = !still_need_simplification_after_reduction;
                        sf.coalesced = false;
                        sf.z = z;
                        sf.tx = tx as i32;
                        sf.ty = ty as i32;
                        sf.line_detail = line_detail;
                        sf.extra_detail = line_detail;
                        sf.maxzoom = maxzoom;
                        sf.spacing = spacing;
                        sf.simplification = simplification;
                        sf.renamed = -1;
                        sf.clustered = 0;

                        if trying_to_stop_early && line_detail == first_detail {
                            // only remove collinearities;
                            // leave all other vertices for extreme overzooming
                            sf.simplification = 0.0;
                        }

                        let extra_detail = get(&EXTRA_DETAIL);
                        if line_detail == detail && extra_detail >= 0 && z == maxzoom {
                            sf.extra_detail = extra_detail;
                            // maximum allowed coordinate delta in geometries is 2^31 - 1
                            // so we need to stay under that, including the buffer
                            if sf.extra_detail >= 30 - z {
                                sf.extra_detail = 30 - z;
                            }
                            tile_detail = sf.extra_detail;
                        }

                        layer.features.push(Box::new(sf));

                        unsimplified_geometry_size +=
                            layer.features.last().unwrap().geometry.len() * SIZEOF_DRAW;
                        if unsimplified_geometry_size > 10 * 1024 * 1024
                            && additional(A_DETECT_SHARED_BORDERS) == 0
                        {
                            // we should be safe to simplify here with P_SIMPLIFY_SHARED_NODES, since they will
                            // have been assembled globally, although that also means that simplification
                            // may not be very effective for reducing memory usage.

                            while simplified_geometry_through < layer.features.len() {
                                let f = &mut layer.features[simplified_geometry_through];
                                simplify_feature(
                                    f,
                                    &shared_nodes,
                                    shared_nodes_map,
                                    nodepos,
                                    shared_nodes_bloom,
                                );

                                if f.t as i32 == VT_POLYGON {
                                    let mut to_clean = f.geometry.clone();

                                    // don't scale up because this is still world coordinates
                                    coalesce_polygon(&mut to_clean, false);
                                    f.geometry = to_clean;
                                }

                                simplified_geometry_through += 1;
                            }

                            unsimplified_geometry_size = 0;
                        }
                    }
                }

                merge_previndex = sfindex;
                coalesced_area = 0.0;
            }

            // We are done reading the features.
            // Close the prefilter if it was opened.

            if prefilter.is_some() {
                drop(prefilter_jp);
                if let Some(f) = prefilter_read_fp.take() {
                    // SAFETY: the descriptor came from into_raw_fd and is closed exactly once.
                    if unsafe { libc::close(f.into_raw_fd()) } != 0 {
                        perror("close output from prefilter");
                        std::process::exit(EXIT_CLOSE);
                    }
                }
                wait_for_filter(prefilter_pid, "waitpid for prefilter\n");
                if let Some(w) = prefilter_writer.take()
                    && let Err(e) = w.join()
                {
                    std::panic::resume_unwind(e);
                }
            }
        });

        // Close the output files for the next zoom level.

        for j in 0..child_shards as usize {
            if within[j] {
                let mut estimated_complexity_out =
                    geompos[j].load(Ordering::Relaxed) - start_geompos[j];

                if dropping_by_rate || next_feature_state.minzoom_feature_pending {
                    // large enough to make it not try to stop early
                    estimated_complexity_out = 1i64 << 32;
                }

                geomfile[j].serialize_long_long(0, &geompos[j], fname); // EOF
                geomfile[j].end(&geompos[j], fname);
                within[j] = false;

                if additional(A_VARIABLE_DEPTH_PYRAMID) != 0 {
                    let _ = geomfile[j].fp().flush();
                    match geomfile[j].fp().get_ref().write_at(
                        &estimated_complexity_out.to_ne_bytes(),
                        start_geompos[j] as u64,
                    ) {
                        Ok(8) => {}
                        _ => {
                            perror("pwrite complexity");
                            std::process::exit(EXIT_WRITE);
                        }
                    }
                }
            }
        }

        first_time = false;

        // Adjust tile size limit based on the ratio of multiplier cluster features to lead features
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

        // Operations on the features within each layer:
        //
        // Tag features with their sequence within the layer, if required for --retain-points-multiplier
        // Add cluster size attributes to clustered features.
        // Update tilestats if attribute accumulation earlier introduced new values.
        // Detect shared borders.
        // Simplify geometries.
        // Reorder and coalesce.
        // Sort back into input order or by attribute value

        shared_nodes.sort();

        for (layername, layer) in layers.iter_mut() {
            let features: &mut Vec<SerialFeaturePtr> = &mut layer.features;

            if retain_points_multiplier > 1 {
                // mapping from input sequence to current sequence within this tile
                let mut feature_sequences: Vec<(usize, usize)> = Vec::new();

                for (i, f) in features.iter().enumerate() {
                    feature_sequences.push((f.seq as usize, i));
                }

                // tag each feature with its sequence number within the layer
                // if the tile were sorted by input order
                //
                // these will be smaller numbers, and avoid the problem of the
                // original sequence number varying based on how many reader threads
                // there were reading the input
                feature_sequences.sort();
                for (i, &(_, j)) in feature_sequences.iter().enumerate() {
                    let sv = SerialVal::new(MVT_DOUBLE, &i.to_string());

                    features[j]
                        .full_keys
                        .push(key_pool.pool("tippecanoe:retain_points_multiplier_sequence"));
                    features[j].full_values.push(sv);
                }
            }

            for p in features.iter_mut() {
                if p.clustered > 0 {
                    let point_count: i64 = p.clustered + 1;

                    p.full_keys.push(key_pool.pool("clustered"));
                    p.full_values.push(SerialVal::new(MVT_BOOL, "true"));

                    p.full_keys.push(key_pool.pool("point_count"));
                    p.full_values
                        .push(SerialVal::new(MVT_DOUBLE, &point_count.to_string()));

                    p.full_keys.push(key_pool.pool("sqrt_point_count"));
                    p.full_values.push(SerialVal::new(
                        MVT_DOUBLE,
                        &format!(
                            "{:.6}",
                            (100.0 * (point_count as f64).sqrt()).round() / 100.0
                        ),
                    ));

                    p.full_keys.push(key_pool.pool("point_count_abbreviated"));
                    let abbrev = if point_count >= 10000 {
                        format!("{:.0}k", point_count as f64 / 1000.0)
                    } else if point_count >= 1000 {
                        format!("{:.1}k", point_count as f64 / 1000.0)
                    } else {
                        format!("{}", point_count)
                    };
                    p.full_values.push(SerialVal::new(MVT_STRING, &abbrev));
                }

                for j in 0..p.full_keys.len() {
                    // remove accumulation state
                    if let Some(found) = p.full_values[j].s.find('\0') {
                        p.full_values[j].s.truncate(found);
                    }
                    add_tilestats(
                        layername,
                        z,
                        layermap,
                        tiling_seg,
                        layer_unmap,
                        &p.full_keys[j],
                        &p.full_values[j],
                    );
                }
            }

            if additional(A_DETECT_SHARED_BORDERS) != 0 {
                find_common_edges(
                    features,
                    z,
                    line_detail,
                    simplification,
                    maxzoom,
                    merge_fraction,
                );
            }

            let mut tasks =
                (get(&CPUS) as f64 / running.load(Ordering::Relaxed) as f64).ceil() as i32;
            if tasks < 1 {
                tasks = 1;
            }

            {
                let mut parts: Vec<Vec<(usize, &mut SerialFeature)>> =
                    (0..tasks).map(|_| Vec::new()).collect();
                for (i, f) in features.iter_mut().enumerate() {
                    parts[i % tasks as usize].push((i, &mut **f));
                }
                let args: Vec<SimplificationWorkerArg<'_>> = parts
                    .into_iter()
                    .map(|part| SimplificationWorkerArg {
                        features: part,
                        shared_nodes: &shared_nodes,
                        shared_nodes_map,
                        nodepos,
                        shared_nodes_bloom,
                    })
                    .collect();

                if tasks > 1 {
                    std::thread::scope(|scope| {
                        let handles: Vec<_> = args
                            .into_iter()
                            .map(|a| thread_create(scope, move || simplification_worker(a)))
                            .collect();

                        for h in handles {
                            if let Err(e) = h.join() {
                                std::panic::resume_unwind(e);
                            }
                        }
                    });
                } else {
                    for a in args {
                        simplification_worker(a);
                    }
                }
            }

            for f in features.iter_mut() {
                let t = f.t as i32;

                {
                    if t == VT_POINT || draws_something(&f.geometry) {
                        f.coalesced = false;
                    }
                }
            }

            let layer_features = features;

            if additional(A_REORDER) != 0 {
                layer_features.sort_by(by_less(coalindexcmp_comparator));
            }

            if additional(A_COALESCE) != 0 {
                // coalesce adjacent identical features if requested

                let mut out: usize = 0;
                if !layer_features.is_empty() {
                    out += 1;
                }

                for x in 1..layer_features.len() {
                    let y = out.wrapping_sub(1);

                    if out > 0 && coalcmp(&layer_features[x], &layer_features[y]) == 0 {
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
                // clean up coalesced linestrings by simplification
                // and coalesced polygons by cleaning
                //
                // then close polygons

                let mut out: usize = 0;

                for x in 0..layer_features.len() {
                    let lf = &mut layer_features[x];
                    let t = lf.t as i32;

                    if lf.coalesced && t == VT_LINE {
                        lf.geometry = remove_noop(std::mem::take(&mut lf.geometry), t, 0);
                        if !(prevent(P_SIMPLIFY) != 0
                            || (z == maxzoom && prevent(P_SIMPLIFY_LOW) != 0))
                        {
                            // XXX revisit: why does this not take zoom into account?
                            lf.geometry = simplify_lines(
                                &mut lf.geometry,
                                32,
                                0,
                                0,
                                0,
                                !(prevent(P_CLIPPING) != 0 || prevent(P_DUPLICATION) != 0),
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
                            // we can try scaling up because this is tile coordinates
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

            if prevent(P_INPUT_ORDER) != 0 {
                let mut clustered = assemble_multiplier_clusters(std::mem::take(layer_features));
                clustered.sort_by(by_less(|a: &Vec<SerialFeaturePtr>, b| {
                    preservecmp_clusters(a, b)
                }));
                *layer_features = disassemble_multiplier_clusters(clustered);
            }

            {
                let order_by = ORDER_BY.read().unwrap();
                if !order_by.is_empty() {
                    let mut clustered =
                        assemble_multiplier_clusters(std::mem::take(layer_features));
                    clustered.sort_by(by_less(|a: &Vec<SerialFeaturePtr>, b| {
                        ordercmp_clusters(&order_by, a, b)
                    }));
                    *layer_features = disassemble_multiplier_clusters(clustered);
                }
            }

            let limit_at_maxzoom = get(&LIMIT_TILE_FEATURE_COUNT_AT_MAXZOOM);
            let limit = get(&LIMIT_TILE_FEATURE_COUNT);
            if z == maxzoom && limit_at_maxzoom != 0 {
                if layer_features.len() > limit_at_maxzoom {
                    // this is maxzoom; ok to stop early still because they said to limit abruptly
                    layer_features.truncate(limit_at_maxzoom);
                    too_many_features = false; // don't try to drop; we have already truncated
                    too_many_bytes = false; // don't try to drop; we have already truncated
                    skipped = 0; // doesn't matter that we skipped features; we have truncated
                }
            } else if limit != 0 && layer_features.len() > limit {
                can_stop_early = false;
                layer_features.truncate(limit);
                too_many_features = false; // don't try to drop; we have already truncated
                too_many_bytes = false; // don't try to drop; we have already truncated
                skipped = 0; // doesn't matter that we skipped features; we have truncated
            }
        }

        let mut tile = MvtTile::default();
        let mut feature_count: usize = 0;

        for (name, lf) in layers.iter_mut() {
            let layer_features = &mut lf.features;
            feature_count += layer_features.len();

            let mut layer = MvtLayer {
                name: name.clone(),
                version: 2,
                extent: 1i64 << tile_detail,
                ..Default::default()
            };

            for slot in layer_features.iter_mut() {
                let mut feature = MvtFeature::default();
                let f = &mut **slot;
                let t = f.t as i32;

                if t == VT_LINE || t == VT_POLYGON {
                    f.geometry = remove_noop(std::mem::take(&mut f.geometry), t, 0);
                }

                if f.geometry.is_empty() {
                    *slot = Box::default();
                    continue;
                }

                feature.type_ = t;
                feature.geometry = to_feature(&f.geometry);
                count += f.geometry.len() as i64;
                f.geometry.clear();

                feature.id = f.id;
                feature.has_id = f.has_id;

                decode_meta(f, &mut layer, &mut feature);
                for a in 0..f.full_keys.len() {
                    let sv = &f.full_values[a];
                    let v = stringified_to_mvt_value(sv.type_, &sv.s);
                    layer.tag(&mut feature, &f.full_keys[a], &v);
                }

                f.full_keys.clear();
                f.full_values.clear();

                if additional(A_CALCULATE_FEATURE_DENSITY) != 0 {
                    let mut glow: i32 = 255;
                    if f.spacing > 0.0 {
                        glow = (1.0 / f.spacing) as i32;
                        if glow > 255 {
                            glow = 255;
                        }
                    }

                    let mut v = mvt_value_of_type(MVT_SINT);
                    v.numeric_value.set_sint_value(glow as i64);
                    layer.tag(&mut feature, "tippecanoe_feature_density", &v);

                    let sv = SerialVal::new(MVT_DOUBLE, &glow.to_string());

                    add_tilestats(
                        &layer.name,
                        z,
                        layermap,
                        tiling_seg,
                        layer_unmap,
                        "tippecanoe_feature_density",
                        &sv,
                    );
                }

                layer.features.push(feature);
                *slot = Box::default();
            }

            if !layer.features.is_empty() {
                tile.layers.push(layer);
            }
        }

        if let Some(postfilter) = postfilter {
            tile.layers = filter_layers(
                postfilter,
                &tile.layers,
                z as u32,
                tx,
                ty,
                layermap,
                tiling_seg,
                layer_unmap,
                1 << tile_detail,
            );
        }

        if z == 0
            && unclipped_features < original_features / 2
            && CLIPBBOXES.read().unwrap().is_empty()
        {
            eprintln!("\n\nMore than half the features were clipped away at zoom level 0.");
            eprintln!("Is your data in the wrong projection? It should be in WGS84/EPSG:4326.");
        }

        report_progress(
            geompos_in, along, alongminus, todo, pass, z, maxzoom, tx, ty, oprogress,
        );

        if trying_to_stop_early
            && line_detail == first_detail
            && (!can_stop_early || next_feature_state.minzoom_feature_pending)
        {
            // didn't work, try a lower detail
            line_detail = next_detail(line_detail);
            continue;
        }

        // Again, adjust the retabulated feature count to estimate
        // how many total features there would have been if we hadn't
        // hit the limit and started dropping early.

        let mut adjusted_feature_count = feature_count;
        if kept > 0 {
            adjusted_feature_count = adjusted_feature_count.wrapping_mul(skipped + kept) / kept;
        }

        if adjusted_feature_count > 0 && !tile.layers.is_empty() {
            if too_many_features
                || (adjusted_feature_count > adjusted_max_tile_features
                    && prevent(P_FEATURE_LIMIT) == 0)
            {
                if adjusted_feature_count > arg.feature_count_out {
                    arg.feature_count_out = adjusted_feature_count;
                }

                if quiet == 0 {
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
                    // didn't work, try a lower detail
                    line_detail = next_detail(line_detail);
                    continue;
                }

                if additional(A_INCREASE_GAMMA_AS_NEEDED) != 0 && gamma < 10.0 {
                    if gamma < 1.0 {
                        gamma = 1.0;
                    } else {
                        gamma *= 1.25;
                    }

                    if gamma > arg.gamma_out {
                        arg.gamma_out = gamma;
                        arg.still_dropping = true;
                    }

                    if quiet == 0 {
                        eprintln!("Going to try gamma of {:.3} to make it fit", gamma);
                    }
                    line_detail += 1; // to keep it the same when the loop decrements it
                    line_detail = next_detail(line_detail);
                    continue;
                } else if mingap < u64::MAX
                    && (additional(A_DROP_DENSEST_AS_NEEDED) != 0
                        || additional(A_COALESCE_DENSEST_AS_NEEDED) != 0
                        || additional(A_CLUSTER_DENSEST_AS_NEEDED) != 0)
                {
                    mingap_fraction = mingap_fraction * adjusted_max_tile_features as f64
                        / adjusted_feature_count as f64
                        * 0.80;
                    if mingap_fraction > 0.80 {
                        if quiet == 0 {
                            eprintln!(
                                "Need to drop features, but calculated that we should keep {:.1}% of features",
                                mingap_fraction * 100.0
                            );
                        }
                        mingap_fraction = 0.80;
                    }
                    let m = choose_mingap(&mut gaps, mingap_fraction, mingap);
                    if m > mingap {
                        mingap = m;
                        if mingap > arg.mingap_out {
                            arg.mingap_out = mingap;
                            arg.still_dropping = true;
                        }
                        if quiet == 0 {
                            eprintln!(
                                "Going to try keeping the sparsest {:.2}% of the features to make it fit",
                                mingap_fraction * 100.0
                            );
                        }
                        line_detail += 1;
                        line_detail = next_detail(line_detail);
                        continue;
                    } else {
                        eprintln!("Can't increase feature gap threshold further");
                        std::process::exit(EXIT_INCOMPLETE);
                    }
                } else if additional(A_DROP_SMALLEST_AS_NEEDED) != 0
                    || additional(A_COALESCE_SMALLEST_AS_NEEDED) != 0
                {
                    minextent_fraction = minextent_fraction * adjusted_max_tile_features as f64
                        / adjusted_feature_count as f64
                        * 0.75;
                    if minextent_fraction > 0.80 {
                        if quiet == 0 {
                            eprintln!(
                                "Need to drop features, but calculated that we should keep {:.1}% of features",
                                minextent_fraction * 100.0
                            );
                        }
                        minextent_fraction = 0.80;
                    }
                    let m = choose_minextent(&mut extents, minextent_fraction, minextent);
                    if m > minextent {
                        minextent = m;
                        if minextent > arg.minextent_out {
                            arg.minextent_out = minextent;
                            arg.still_dropping = true;
                        }
                        if quiet == 0 {
                            eprintln!(
                                "Going to try keeping the biggest {:.2}% of the features to make it fit",
                                minextent_fraction * 100.0
                            );
                        }
                        line_detail += 1;
                        line_detail = next_detail(line_detail);
                        continue;
                    } else {
                        eprintln!("Can't increase feature area threshold further");
                        std::process::exit(EXIT_INCOMPLETE);
                    }
                } else if additional(A_DROP_BY_ATTRIBUTE_AS_NEEDED) != 0 {
                    minattribute_fraction = minattribute_fraction
                        * adjusted_max_tile_features as f64
                        / adjusted_feature_count as f64
                        * 0.75;
                    if minattribute_fraction > 0.80 {
                        if quiet == 0 {
                            eprintln!(
                                "Need to drop features, but calculated that we should keep {:.1}% of features",
                                minattribute_fraction * 100.0
                            );
                        }
                        minattribute_fraction = 0.80;
                    }

                    let desc = drop_by_attribute_descending;
                    if attribute_values.is_empty() && quiet == 0 {
                        eprintln!(
                            "Warning: no features had a numeric value for attribute '{}'",
                            drop_by_attribute_as_needed_attribute
                        );
                    }
                    let m = choose_minattribute(
                        &mut attribute_values,
                        minattribute_fraction,
                        minattribute,
                        desc,
                    );
                    let better = if desc {
                        m < minattribute
                    } else {
                        m > minattribute
                    };
                    if better {
                        minattribute = m;
                        let propagate = if desc {
                            minattribute < arg.minattribute_out
                        } else {
                            minattribute > arg.minattribute_out
                        };
                        if propagate {
                            arg.minattribute_out = minattribute;
                            arg.still_dropping = true;
                        }
                        if quiet == 0 {
                            eprintln!(
                                "Trying to keep features with '{}' {} {:.6} to make it fit",
                                drop_by_attribute_as_needed_attribute,
                                if desc { "<=" } else { ">=" },
                                minattribute
                            );
                        }
                        line_detail += 1;
                        line_detail = next_detail(line_detail);
                        continue;
                    } else {
                        eprintln!("Can't increase attribute threshold further");
                        std::process::exit(EXIT_INCOMPLETE);
                    }
                } else if feature_count > layers.len()
                    && (additional(A_DROP_FRACTION_AS_NEEDED) != 0
                        || additional(A_COALESCE_FRACTION_AS_NEEDED) != 0
                        || prevent(P_DYNAMIC_DROP) != 0)
                {
                    // The 95% is a guess to avoid too many retries
                    // and probably actually varies based on how much duplicated metadata there is

                    mindrop_sequence_fraction = mindrop_sequence_fraction
                        * adjusted_max_tile_features as f64
                        / adjusted_feature_count as f64
                        * 0.95;
                    if mindrop_sequence_fraction > 0.80 {
                        if quiet == 0 {
                            eprintln!(
                                "Need to drop features, but calculated that we should keep {:.1}% of features",
                                mindrop_sequence_fraction * 100.0
                            );
                        }
                        mindrop_sequence_fraction = 0.80;
                    }
                    let m = choose_mindrop_sequence(
                        &mut drop_sequences,
                        mindrop_sequence_fraction,
                        mindrop_sequence,
                    );
                    if m > mindrop_sequence {
                        mindrop_sequence = m;
                        if mindrop_sequence > arg.mindrop_sequence_out {
                            if prevent(P_DYNAMIC_DROP) == 0 {
                                arg.mindrop_sequence_out = mindrop_sequence;
                            }
                            arg.still_dropping = true;
                        }
                        if quiet == 0 {
                            eprintln!(
                                "Going to try keeping {:.2}% of the features to make it fit",
                                mindrop_sequence_fraction * 100.0
                            );
                        }
                        line_detail += 1; // to keep it the same when the loop decrements it
                        line_detail = next_detail(line_detail);
                        continue;
                    } else {
                        eprintln!("Can't increase feature count threshold further");
                        std::process::exit(EXIT_INCOMPLETE);
                    }
                } else {
                    eprintln!("Try using --drop-fraction-as-needed or --drop-densest-as-needed.");
                    return -1;
                }
            }

            let mut compressed: Vec<u8> = Vec::new();
            let pbf = tile.encode();

            tile.layers.clear();

            if prevent(P_TILE_COMPRESSION) == 0 {
                compress(&pbf, &mut compressed, true);
            } else {
                compressed = pbf;
            }

            // And similarly, adjust the compressed byte size to estimate
            // what it would have been if we hadn't stopped dropping features early

            let mut adjusted_tile_size = compressed.len();
            if kept > 0 {
                adjusted_tile_size = adjusted_tile_size.wrapping_mul(kept + skipped) / kept;
            }

            if too_many_bytes
                || (adjusted_tile_size > adjusted_max_tile_size && prevent(P_KILOBYTE_LIMIT) == 0)
            {
                if adjusted_tile_size > arg.tile_size_out {
                    arg.tile_size_out = adjusted_tile_size;
                }

                if quiet == 0 {
                    if adjusted_tile_size != compressed.len() {
                        eprintln!(
                            "tile {}/{}/{} size is {} (probably really {}) with detail {}, >{}    ",
                            z,
                            tx,
                            ty,
                            compressed.len() as i64,
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
                            compressed.len() as i64,
                            line_detail,
                            adjusted_max_tile_size
                        );
                    }
                }

                if trying_to_stop_early && line_detail == first_detail {
                    // didn't work, try a lower detail
                    line_detail = next_detail(line_detail);
                    continue;
                }

                if additional(A_INCREASE_GAMMA_AS_NEEDED) != 0 && gamma < 10.0 {
                    if gamma < 1.0 {
                        gamma = 1.0;
                    } else {
                        gamma *= 1.25;
                    }

                    if gamma > arg.gamma_out {
                        arg.gamma_out = gamma;
                        arg.still_dropping = true;
                    }

                    if quiet == 0 {
                        eprintln!("Going to try gamma of {:.3} to make it fit", gamma);
                    }
                    line_detail += 1; // to keep it the same when the loop decrements it
                } else if mingap < u64::MAX
                    && (additional(A_DROP_DENSEST_AS_NEEDED) != 0
                        || additional(A_COALESCE_DENSEST_AS_NEEDED) != 0
                        || additional(A_CLUSTER_DENSEST_AS_NEEDED) != 0)
                {
                    mingap_fraction = mingap_fraction * adjusted_max_tile_size as f64
                        / adjusted_tile_size as f64
                        * 0.80;
                    if mingap_fraction > 0.80 {
                        if quiet == 0 {
                            eprintln!(
                                "Need to drop features, but calculated that we should keep {:.1}% of features",
                                mingap_fraction * 100.0
                            );
                        }
                        mingap_fraction = 0.80;
                    }
                    let m = choose_mingap(&mut gaps, mingap_fraction, mingap);
                    if m > mingap {
                        mingap = m;
                        if mingap > arg.mingap_out {
                            arg.mingap_out = mingap;
                            arg.still_dropping = true;
                        }
                        if quiet == 0 {
                            eprintln!(
                                "Going to try keeping the sparsest {:.2}% of the features to make it fit",
                                mingap_fraction * 100.0
                            );
                        }
                        line_detail += 1;
                        line_detail = next_detail(line_detail);
                        continue;
                    } else {
                        eprintln!("Can't increase feature gap threshold further");
                        std::process::exit(EXIT_INCOMPLETE);
                    }
                } else if additional(A_DROP_SMALLEST_AS_NEEDED) != 0
                    || additional(A_COALESCE_SMALLEST_AS_NEEDED) != 0
                {
                    minextent_fraction = minextent_fraction * adjusted_max_tile_size as f64
                        / adjusted_tile_size as f64
                        * 0.75;
                    if minextent_fraction > 0.80 {
                        if quiet == 0 {
                            eprintln!(
                                "Need to drop features, but calculated that we should keep {:.1}% of features",
                                minextent_fraction * 100.0
                            );
                        }
                        minextent_fraction = 0.80;
                    }
                    let m = choose_minextent(&mut extents, minextent_fraction, minextent);
                    if m > minextent {
                        minextent = m;
                        if minextent > arg.minextent_out {
                            arg.minextent_out = minextent;
                            arg.still_dropping = true;
                        }
                        if quiet == 0 {
                            eprintln!(
                                "Going to try keeping the biggest {:.2}% of the features to make it fit",
                                minextent_fraction * 100.0
                            );
                        }
                        line_detail += 1;
                        line_detail = next_detail(line_detail);
                        continue;
                    } else {
                        eprintln!("Can't increase feature area threshold further");
                        std::process::exit(EXIT_INCOMPLETE);
                    }
                } else if additional(A_DROP_BY_ATTRIBUTE_AS_NEEDED) != 0 {
                    minattribute_fraction = minattribute_fraction * adjusted_max_tile_size as f64
                        / adjusted_tile_size as f64
                        * 0.75;
                    if minattribute_fraction > 0.80 {
                        if quiet == 0 {
                            eprintln!(
                                "Need to drop features, but calculated that we should keep {:.1}% of features",
                                minattribute_fraction * 100.0
                            );
                        }
                        minattribute_fraction = 0.80;
                    }
                    let desc2 = drop_by_attribute_descending;
                    if attribute_values.is_empty() && quiet == 0 {
                        eprintln!(
                            "Warning: no features had a numeric value for attribute '{}'",
                            drop_by_attribute_as_needed_attribute
                        );
                    }
                    let m = choose_minattribute(
                        &mut attribute_values,
                        minattribute_fraction,
                        minattribute,
                        desc2,
                    );
                    let better2 = if desc2 {
                        m < minattribute
                    } else {
                        m > minattribute
                    };
                    if better2 {
                        minattribute = m;
                        let propagate2 = if desc2 {
                            minattribute < arg.minattribute_out
                        } else {
                            minattribute > arg.minattribute_out
                        };
                        if propagate2 {
                            arg.minattribute_out = minattribute;
                            arg.still_dropping = true;
                        }
                        if quiet == 0 {
                            eprintln!(
                                "Going to try keeping features with attribute '{}' {} {:.6} to make it fit",
                                drop_by_attribute_as_needed_attribute,
                                if desc2 { "<=" } else { ">=" },
                                minattribute
                            );
                        }
                        line_detail += 1;
                        line_detail = next_detail(line_detail);
                        continue;
                    } else {
                        eprintln!("Can't increase attribute threshold further");
                        std::process::exit(EXIT_INCOMPLETE);
                    }
                } else if feature_count > layers.len()
                    && (additional(A_DROP_FRACTION_AS_NEEDED) != 0
                        || additional(A_COALESCE_FRACTION_AS_NEEDED) != 0
                        || prevent(P_DYNAMIC_DROP) != 0)
                {
                    mindrop_sequence_fraction = mindrop_sequence_fraction
                        * adjusted_max_tile_size as f64
                        / adjusted_tile_size as f64
                        * 0.75;
                    if mindrop_sequence_fraction > 0.80 {
                        if quiet == 0 {
                            eprintln!(
                                "Need to drop features, but calculated that we should keep {:.1}% of features",
                                mindrop_sequence_fraction * 100.0
                            );
                        }
                        mindrop_sequence_fraction = 0.80;
                    }
                    let m = choose_mindrop_sequence(
                        &mut drop_sequences,
                        mindrop_sequence_fraction,
                        mindrop_sequence,
                    );
                    if m > mindrop_sequence {
                        mindrop_sequence = m;
                        if mindrop_sequence > arg.mindrop_sequence_out {
                            if prevent(P_DYNAMIC_DROP) == 0 {
                                arg.mindrop_sequence_out = mindrop_sequence;
                            }
                            arg.still_dropping = true;
                        }
                        if quiet == 0 {
                            eprintln!(
                                "Going to try keeping {:.2}% of the features to make it fit",
                                mindrop_sequence_fraction * 100.0
                            );
                        }
                        line_detail += 1;
                        line_detail = next_detail(line_detail);
                        continue;
                    } else {
                        eprintln!("Can't increase feature count threshold further");
                        std::process::exit(EXIT_INCOMPLETE);
                    }
                } else {
                    detail_reduced += 1;
                }
            } else {
                let guard = lock(&DB_LOCK);

                if skipped > 0 || too_many_bytes || too_many_features {
                    eprintln!("Can't happen: writing tile even though we skipped");
                    std::process::exit(EXIT_IMPOSSIBLE);
                }

                if let Some(outdb) = outdb {
                    let db = lock(outdb);
                    mbtiles_write_tile(&db, z, tx as i32, ty as i32, &compressed);
                } else if let Some(outdir) = outdir {
                    dir_write_tile(outdir, z, tx as i32, ty as i32, &compressed);
                }

                drop(guard);

                if trying_to_stop_early
                    && line_detail == first_detail
                    && !next_feature_state.minzoom_feature_pending
                {
                    // We succeeded in stopping early (and no excluded feature is
                    // waiting for a deeper zoom). Prune the child tiles.

                    strategy.truncated_zooms += 1;
                    arg.skip_children_out
                        .insert(Zxy::new(z, tx as i32, ty as i32));
                }

                strategy_out.add_from(&strategy);
                return count;
            }
        } else {
            strategy_out.add_from(&strategy);
            return count;
        }

        line_detail = next_detail(line_detail);
    }

    eprintln!("could not make tile {}/{}/{} small enough", z, tx, ty);
    -1
}

fn run_thread(arg: &mut WriteTileArgs<'_>) -> Option<i32> {
    let mut err_or_null: Option<i32> = None;

    loop {
        let task = {
            let mut tasks = lock(arg.tasks);
            tasks.pop()
        };

        let task = match task {
            Some(task) => task,
            None => break,
        };

        let j = task.fileno;

        if arg.geomfd[j].load(Ordering::Relaxed) < 0 {
            // only one source file for zoom level 0
            continue;
        }
        if arg.geom_size[j] == 0 {
            continue;
        }

        // If this is zoom level 0, the geomfd will be uncompressed data,
        // because (at least for now) it needs to stay uncompressed during
        // the sort and post-sort maxzoom calculation and fixup so that
        // the sort can rearrange individual features and the fixup can
        // then adjust their minzooms without decompressing and recompressing
        // each feature.
        //
        // In higher zooms, it will be compressed data written out during the
        // previous zoom.

        // SAFETY: geomfd[j] is an open descriptor that only this thread uses in this pass.
        let geom = unsafe { File::from_raw_fd(arg.geomfd[j].load(Ordering::Relaxed)) };

        let mut dc = Decompressor::new(geom);

        let geompos = AtomicI64::new(0);
        let mut prevgeom: i64 = 0;

        loop {
            let mut z: i32 = 0;
            let mut x: u32 = 0;
            let mut y: u32 = 0;

            // These z/x/y are uncompressed so we can seek to the start of the
            // compressed feature data that immediately follows.

            let mut ec = [0u8; 8];
            if dc.fread(&mut ec, 8, 1, &geompos) != 1 {
                break;
            }
            let estimated_complexity = i64::from_ne_bytes(ec);
            if dc.deserialize_int(&mut z, &geompos) == 0 {
                break;
            }
            dc.deserialize_uint(&mut x, &geompos);
            dc.deserialize_uint(&mut y, &geompos);

            if arg.compressed {
                dc.begin();
            }

            let len: i64;

            let parent = Zxy::new(z - 1, (x / 2) as i32, (y / 2) as i32);
            let skip: bool;

            // Normally a tile writes the geometry for its children on pass 0, and the
            // later passes of the same zoom, which are only retries with new thresholds,
            // must not write it again.
            let mut write_children = arg.pass == 0;

            if arg.skip_children.contains(&parent) {
                if dropping_features(
                    arg.mingap,
                    arg.minextent,
                    arg.mindrop_sequence,
                    arg.minattribute,
                    arg.drop_by_attribute_descending,
                ) {
                    // The parent truncated its pyramid here, but the zoom has since had to
                    // start dropping features, so the truncation no longer holds and this
                    // tile has to be written after all. Its geometry is still in the stream.
                    skip = false;

                    // It was skipped on every earlier pass of this zoom, so its children
                    // have never been written out. Write them on the first pass that
                    // actually tiles it, rather than on pass 0 as usual. The thresholds
                    // only ever increase within a zoom, so that is exactly the pass on
                    // which this zoom started dropping.
                    write_children = arg.first_dropping_pass;
                } else {
                    // skip if the parent tile finished at the last zoom level and we aren't trying to drop
                    skip = true;
                }
            } else {
                // do the tile if the parent didn't finish early
                skip = false;
            }

            if skip {
                skip_tile(&mut dc, &geompos, arg.compressed);
                len = 1;
            } else {
                arg.wrote_zoom = z;
                let detail = if z == arg.maxzoom {
                    arg.full_detail
                } else {
                    arg.low_detail
                };
                let alongminus = geompos.load(Ordering::Relaxed);
                len = write_tile(
                    &mut dc,
                    &geompos,
                    z,
                    x,
                    y,
                    detail,
                    alongminus,
                    arg,
                    estimated_complexity,
                    write_children,
                );
            }

            {
                let _guard = lock(&VAR_LOCK);

                if z == arg.maxzoom {
                    let most = arg.most.load(Ordering::Relaxed);
                    if len > most {
                        arg.midx.store(x, Ordering::Relaxed);
                        arg.midy.store(y, Ordering::Relaxed);
                        arg.most.store(len, Ordering::Relaxed);
                    } else if len == most {
                        let a: u64 = ((x as u64) << 32) | y as u64;
                        let b: u64 = ((arg.midx.load(Ordering::Relaxed) as u64) << 32)
                            | arg.midy.load(Ordering::Relaxed) as u64;

                        if a < b {
                            arg.midx.store(x, Ordering::Relaxed);
                            arg.midy.store(y, Ordering::Relaxed);
                            arg.most.store(len, Ordering::Relaxed);
                        }
                    }
                }

                let g = geompos.load(Ordering::Relaxed);
                arg.along.fetch_add(g - prevgeom, Ordering::Relaxed);
                prevgeom = g;
            }

            if len < 0 {
                arg.err = z - 1;
                err_or_null = Some(arg.err);
                break;
            }
        }

        if arg.pass == 1 {
            // Since the fclose() has closed the underlying file descriptor
            arg.geomfd[j].store(-1, Ordering::Relaxed);
        } else {
            // SAFETY: duplicating and seeking a descriptor this thread owns.
            let newfd = unsafe { libc::dup(arg.geomfd[j].load(Ordering::Relaxed)) };
            if newfd < 0 {
                perror("dup geometry");
                std::process::exit(EXIT_OPEN);
            }
            // SAFETY: newfd was just opened above.
            if unsafe { libc::lseek(newfd, 0, libc::SEEK_SET) } < 0 {
                perror("lseek geometry");
                std::process::exit(EXIT_SEEK);
            }
            arg.geomfd[j].store(newfd, Ordering::Relaxed);
        }

        // PORT: the C++ checks the result of fclose(geom) here. Dropping the
        // decompressor closes the descriptor without a way to see the result,
        // which for a file that was only read cannot fail anyway.
        drop(dc);
    }

    // PORT: the C++ says `arg->running--`, which decrements the pointer rather
    // than the count, so the count of running threads stays where it started.
    err_or_null
}

/// `geomfd` and `geom_size` hold one file descriptor and size per temporary
/// geometry file, as the C++ arrays do. `outdb` is `None` when writing a
/// directory. Returns the zoom level actually reached.
#[allow(clippy::too_many_arguments)]
pub fn traverse_zooms(
    geomfd: &mut [i32],
    geom_size: &mut [i64],
    global_stringpool: &StringPoolRef,
    midx: &AtomicU32,
    midy: &AtomicU32,
    maxzoom: &mut i32,
    minzoom: i32,
    outdb: Option<&Mutex<Connection>>,
    outdir: Option<&str>,
    buffer: i32,
    fname: &str,
    tmpdir: &str,
    gamma: f64,
    full_detail: i32,
    low_detail: i32,
    min_detail: i32,
    pool_off: &[i64],
    initial_x: &[u32],
    initial_y: &[u32],
    simplification: f64,
    maxzoom_simplification: f64,
    layermaps: &mut Vec<BTreeMap<String, LayermapEntry>>,
    prefilter: Option<&str>,
    postfilter: Option<&str>,
    attribute_accum: &HashMap<String, AttributeOp>,
    filter: Option<JsonRef<'_>>,
    strategies: &mut Vec<Strategy>,
    iz: i32,
    shared_nodes_map: &[Node],
    nodepos: usize,
    shared_nodes_bloom: &[u8],
    basezoom: i32,
    droprate: f64,
    unidecode_data: &[String],
    drop_by_attribute_as_needed_attribute: Option<&str>,
    drop_by_attribute_descending: bool,
) -> i32 {
    LAST_PROGRESS.store(0.0);

    let cpus = get(&CPUS);
    let temp_files = get(&TEMP_FILES);

    // The existing layermaps are one table per input thread.
    // We need to add another one per *tiling* thread so that it can be
    // safely changed during tiling.
    let layermaps_off = layermaps.len();
    for _ in 0..cpus {
        layermaps.push(BTreeMap::new());
    }

    // Table to map segment and layer number back to layer name
    let mut layer_unmaps: Vec<Vec<String>> = Vec::new();
    for seg in 0..layermaps.len() {
        layer_unmaps.push(Vec::new());

        for (name, entry) in &layermaps[seg] {
            if entry.id >= layer_unmaps[seg].len() {
                layer_unmaps[seg].resize(entry.id + 1, String::new());
            }
            layer_unmaps[seg][entry.id] = name.clone();
        }
    }

    let mut skip_children: BTreeSet<Zxy> = BTreeSet::new();

    let mut z: i32;
    let mut largest_written: i32 = -1;

    z = iz;
    while z <= *maxzoom {
        let most = AtomicI64::new(0);

        let mut sub: Vec<Compressor> = Vec::with_capacity(temp_files);
        let subpos: Vec<AtomicI64> = (0..temp_files).map(|_| AtomicI64::new(0)).collect();
        let mut subfd: Vec<i32> = vec![0; temp_files];
        for (j, subfd_j) in subfd.iter_mut().enumerate() {
            let mut geomname = format!("{}/geom{}.XXXXXXXX", tmpdir, j);
            *subfd_j = mkstemp_cloexec(&mut geomname);
            // printf("%s\n", geomname.c_str());
            if *subfd_j < 0 {
                perror(&geomname);
                std::process::exit(EXIT_OPEN);
            }
            let fp = match OpenOptions::new().write(true).open(&geomname) {
                Ok(fp) => fp,
                Err(_) => {
                    perror(&geomname);
                    std::process::exit(EXIT_OPEN);
                }
            };
            sub.push(Compressor::new(fp));
            let _ = std::fs::remove_file(&geomname);
        }

        let mut useful_threads: usize = 0;
        let mut todo: i64 = 0;
        for &size in geom_size.iter().take(temp_files) {
            todo += size;
            if size > 0 {
                useful_threads += 1;
            }
        }

        let mut threads: usize = cpus;
        if threads > temp_files / 4 {
            threads = temp_files / 4;
        }
        // XXX is it useful to divide further if we know we are skipping
        // some zoom levels? Is it faster to have fewer CPUs working on
        // sharding, but more deeply, or more CPUs, less deeply?
        if threads > useful_threads {
            threads = useful_threads;
        }

        // Round down to a power of 2
        for e in 0..30 {
            if threads >= (1usize << e) && threads < (1usize << (e + 1)) {
                threads = 1usize << e;
                break;
            }
        }
        if threads >= (1usize << 30) {
            threads = 1usize << 30;
        }
        if threads < 1 {
            threads = 1;
        }

        // Assign temporary files to threads

        let mut tasks: Vec<Task> = vec![Task::default(); temp_files];

        let mut dispatch: Vec<Task> = Vec::new();

        for j in 0..temp_files {
            if geom_size[j] == 0 {
                continue;
            }

            tasks[j].fileno = j;
            tasks[j].todo = geom_size[j] as usize;
            dispatch.push(tasks[j]);
        }

        // PORT: the C++ sorts the pointers to the tasks here, which puts them in
        // the order of their addresses, the order they are already in.

        let mut err: i32 = i32::MAX;

        let mut zoom_gamma = gamma;
        let mut zoom_mingap: u64 = 0;
        let mut zoom_minextent: i64 = 0;
        let mut zoom_mindrop_sequence: u64 = 0;
        let mut zoom_minattribute = if drop_by_attribute_descending {
            f64::INFINITY
        } else {
            f64::NEG_INFINITY
        };
        let mut zoom_tile_size: usize = 0;
        let mut zoom_feature_count: usize = 0;
        let mut skip_children_out: BTreeSet<Zxy> = BTreeSet::new();
        let mut was_dropping = false;

        let mut pass: usize = 0;
        loop {
            // Tiles that were skipped because an ancestor truncated its pyramid are
            // revived on the pass where this zoom starts dropping features, and that
            // is the pass on which they have to write out their children.
            let is_dropping = dropping_features(
                zoom_mingap,
                zoom_minextent,
                zoom_mindrop_sequence,
                zoom_minattribute,
                drop_by_attribute_descending,
            );
            let first_dropping_pass = is_dropping && !was_dropping;
            was_dropping = is_dropping;

            let running = AtomicI32::new(threads as i32);
            let along = AtomicI64::new(0);
            let strategy = AtomicStrategy::default();
            skip_children_out.clear();

            // must be recreate with each pass, since child threads consume it
            let pass_dispatch = Mutex::new(dispatch.clone());

            let geomfd_shared: Vec<AtomicI32> =
                geomfd.iter().map(|&fd| AtomicI32::new(fd)).collect();

            let child_shards = temp_files / threads;
            let (input_unmaps, tiling_unmaps) = layer_unmaps.split_at_mut(layermaps_off);
            let input_unmaps: &[Vec<String>] = input_unmaps;
            let mut tiling_layermaps = layermaps[layermaps_off..].iter_mut();
            let mut tiling_unmaps = tiling_unmaps.iter_mut();
            let mut shard_chunks = sub.chunks_mut(child_shards);
            let mut pos_chunks = subpos.chunks(child_shards);

            let mut args: Vec<WriteTileArgs<'_>> = Vec::with_capacity(threads);
            for thread in 0..threads {
                args.push(WriteTileArgs {
                    threadno: thread,
                    global_stringpool,
                    min_detail,
                    outdb, // locked with db_lock
                    outdir,
                    buffer,
                    fname,
                    geomfile: shard_chunks.next().unwrap(),
                    geompos: pos_chunks.next().unwrap(),
                    todo: todo as f64,
                    along: &along, // locked with var_lock
                    gamma: zoom_gamma,
                    gamma_out: zoom_gamma,
                    mingap: zoom_mingap,
                    mingap_out: zoom_mingap,
                    minextent: zoom_minextent,
                    minextent_out: zoom_minextent,
                    mindrop_sequence: zoom_mindrop_sequence,
                    mindrop_sequence_out: zoom_mindrop_sequence,
                    minattribute: zoom_minattribute,
                    minattribute_out: zoom_minattribute,
                    drop_by_attribute_as_needed_attribute,
                    drop_by_attribute_descending,
                    tile_size_out: 0,
                    feature_count_out: 0,
                    child_shards: child_shards as i32,

                    simplification: if z == *maxzoom && maxzoom_simplification > 0.0 {
                        maxzoom_simplification
                    } else {
                        simplification
                    },

                    geomfd: &geomfd_shared,
                    geom_size,
                    midx, // locked with var_lock
                    midy, // locked with var_lock
                    maxzoom: *maxzoom,
                    minzoom,
                    basezoom,
                    droprate,
                    full_detail,
                    low_detail,
                    most: &most, // locked with var_lock
                    pool_off,
                    initial_x,
                    initial_y,
                    err: 0,
                    layermap: tiling_layermaps.next().unwrap(),
                    input_unmaps,
                    layer_unmap: tiling_unmaps.next().unwrap(),
                    tiling_seg: thread + layermaps_off,
                    prefilter,
                    postfilter,
                    attribute_accum,
                    filter,
                    unidecode_data,

                    tasks: &pass_dispatch,
                    running: &running,
                    pass,
                    wrote_zoom: -1,
                    still_dropping: false,
                    strategy: &strategy,
                    zoom: z,
                    compressed: z != iz,
                    shared_nodes_map,
                    nodepos,
                    shared_nodes_bloom,
                    skip_children: &skip_children,
                    skip_children_out: BTreeSet::new(),
                    first_dropping_pass,
                });
            }

            let retvals: Vec<Option<i32>> = std::thread::scope(|scope| {
                let handles: Vec<_> = args
                    .iter_mut()
                    .map(|a| thread_create(scope, move || run_thread(a)))
                    .collect();

                handles
                    .into_iter()
                    .map(|h| match h.join() {
                        Ok(r) => r,
                        Err(e) => std::panic::resume_unwind(e),
                    })
                    .collect()
            });

            let mut again = false;
            let mut extend_zooms = false;
            for thread in 0..threads {
                if let Some(e) = retvals[thread] {
                    err = e;
                }

                let a = &args[thread];

                for zxy in &a.skip_children_out {
                    skip_children_out.insert(zxy.clone());
                }

                if a.gamma_out > zoom_gamma {
                    zoom_gamma = a.gamma_out;
                    again = true;
                }
                if a.mingap_out > zoom_mingap {
                    zoom_mingap = a.mingap_out;
                    again = true;
                }
                if a.minextent_out > zoom_minextent {
                    zoom_minextent = a.minextent_out;
                    again = true;
                }
                if a.mindrop_sequence_out > zoom_mindrop_sequence {
                    zoom_mindrop_sequence = a.mindrop_sequence_out;
                    again = true;
                }
                let attr_propagate = if drop_by_attribute_descending {
                    a.minattribute_out < zoom_minattribute
                } else {
                    a.minattribute_out > zoom_minattribute
                };
                if attr_propagate {
                    zoom_minattribute = a.minattribute_out;
                    again = true;
                }
                if a.tile_size_out > zoom_tile_size {
                    zoom_tile_size = a.tile_size_out;
                }
                if a.feature_count_out > zoom_feature_count {
                    zoom_feature_count = a.feature_count_out;
                }

                // Zoom counter might be lower than reality if zooms are being skipped
                if a.wrote_zoom > z {
                    z = a.wrote_zoom;
                }
                if a.wrote_zoom > largest_written {
                    largest_written = a.wrote_zoom;
                }

                if a.still_dropping {
                    extend_zooms = true;
                }
            }

            drop(args);
            for (fd, shared) in geomfd.iter_mut().zip(&geomfd_shared) {
                *fd = shared.load(Ordering::Relaxed);
            }

            if extend_zooms
                && (additional(A_EXTEND_ZOOMS) != 0 || get(&EXTEND_ZOOMS_MAX) > 0)
                && z == *maxzoom
                && *maxzoom < MAX_ZOOM
            {
                *maxzoom += 1;
                if get(&EXTEND_ZOOMS_MAX) > 0 {
                    EXTEND_ZOOMS_MAX.fetch_sub(1, Ordering::Relaxed);
                }
            }

            if z as usize >= strategies.len() {
                strategies.resize(z as usize + 1, Strategy::default());
            }

            let s = Strategy::new(&strategy, zoom_tile_size, zoom_feature_count);
            strategies[z as usize] = s;

            if again {
                if let Some(outdb) = outdb {
                    mbtiles_erase_zoom(&lock(outdb), z);
                } else if let Some(outdir) = outdir {
                    dir_erase_zoom(outdir, z);
                }
            } else {
                break;
            }

            pass += 1;
        }

        skip_children = std::mem::take(&mut skip_children_out);

        for (j, sub_j) in sub.iter_mut().enumerate() {
            // Can be < 0 if there is only one source file, at z0
            // SAFETY: closing a descriptor this function owns.
            if geomfd[j] >= 0 && unsafe { libc::close(geomfd[j]) } != 0 {
                perror("close geom");
                std::process::exit(EXIT_CLOSE);
            }
            if sub_j.fclose() != 0 {
                perror("close subfile");
                std::process::exit(EXIT_CLOSE);
            }

            // SAFETY: libc::stat is plain data, and fstat fills it in.
            let mut geomst: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: geomst is a valid stat buffer.
            if unsafe { libc::fstat(subfd[j], &mut geomst) } != 0 {
                perror("stat geom\n");
                std::process::exit(EXIT_STAT);
            }

            geomfd[j] = subfd[j];
            geom_size[j] = geomst.st_size;
        }

        if err != i32::MAX {
            return err;
        }

        z += 1;
    }

    if largest_written >= 0 && *maxzoom > largest_written {
        *maxzoom = largest_written;
    }

    for &fd in geomfd.iter().take(temp_files) {
        // Can be < 0 if there is only one source file, at z0
        // SAFETY: closing a descriptor this function owns.
        if fd >= 0 && unsafe { libc::close(fd) } != 0 {
            perror("close geom");
            std::process::exit(EXIT_CLOSE);
        }
    }

    if get(&QUIET) == 0 {
        eprintln!();
    }
    *maxzoom
}

impl AtomicStrategy {
    pub fn add_from(&self, src: &Strategy) {
        self.dropped_by_rate
            .fetch_add(src.dropped_by_rate, Ordering::Relaxed);
        self.dropped_by_gamma
            .fetch_add(src.dropped_by_gamma, Ordering::Relaxed);
        self.dropped_as_needed
            .fetch_add(src.dropped_as_needed, Ordering::Relaxed);
        self.coalesced_as_needed
            .fetch_add(src.coalesced_as_needed, Ordering::Relaxed);
        self.detail_reduced
            .fetch_add(src.detail_reduced, Ordering::Relaxed);
        self.tiny_polygons
            .fetch_add(src.tiny_polygons, Ordering::Relaxed);
        self.truncated_zooms
            .fetch_add(src.truncated_zooms, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::VT_CLOSEPATH;

    /// What the C++ functions print for the cases in `harness_output`, from a
    /// harness built with verbatim copies of them.
    const EXPECTED: &str = "\
gamma 0: (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000)
gamma 0.5: (1 0 0000000000000000) (1 0 3df9000000000000) (0 1099511627776 0000000000000000) (1 1099511627776 3db4000000000000) (0 2199023255552 0000000000000000) (0 3298534883328 0000000000000000) (1 3298534883328 0000000000000000) (0 4398046511104 0000000000000000) (0 5 0000000000000000) (0 8796093022208 0000000000000000)
gamma 1: (1 0 0000000000000000) (1 0 3df9000000000000) (0 1099511627776 0000000000000000) (1 1099511627776 3db4000000000000) (0 2199023255552 0000000000000000) (0 3298534883328 0000000000000000) (1 3298534883328 0000000000000000) (0 4398046511104 0000000000000000) (0 5 0000000000000000) (0 8796093022208 0000000000000000)
gamma 1.5: (1 0 0000000000000000) (1 0 3df9000000000000) (0 1099511627776 0000000000000000) (1 1099511627776 3db4000000000000) (0 2199023255552 0000000000000000) (0 3298534883328 0000000000000000) (1 3298534883328 0000000000000000) (0 4398046511104 0000000000000000) (0 5 0000000000000000) (0 8796093022208 0000000000000000)
gamma 2: (1 0 0000000000000000) (1 0 3df9000000000000) (0 1099511627776 0000000000000000) (1 1099511627776 3db4000000000000) (0 2199023255552 0000000000000000) (0 3298534883328 0000000000000000) (1 3298534883328 0000000000000000) (0 4398046511104 0000000000000000) (0 5 0000000000000000) (0 8796093022208 0000000000000000)
gamma2 0: (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000)
gamma2 0.5: (1 0 0000000000000000) (1 0 3fd0000000000000) (0 137438953472 0000000000000000) (1 137438953472 3fc0000000000000) (0 274877906944 0000000000000000) (0 824633720832 0000000000000000) (1 824633720832 3dbc000000000000) (0 1099511627776 0000000000000000) (1 1099511627776 3fd0000000000000) (0 549755813888 0000000000000000) (0 2199023255555 0000000000000000) (1 2199023255555 3fdffffffffd0000) (0 2473901162496 0000000000000000) (0 4398046511104 0000000000000000)
gamma2 1: (1 0 0000000000000000) (1 0 3fd0000000000000) (0 137438953472 0000000000000000) (1 137438953472 3fc0000000000000) (0 274877906944 0000000000000000) (0 824633720832 0000000000000000) (1 824633720832 3dbc000000000000) (0 1099511627776 0000000000000000) (1 1099511627776 3fd0000000000000) (0 549755813888 0000000000000000) (0 2199023255555 0000000000000000) (1 2199023255555 3fdffffffffd0000) (0 2473901162496 0000000000000000) (0 4398046511104 0000000000000000)
gamma2 1.5: (1 0 0000000000000000) (1 0 3fd0000000000000) (0 137438953472 0000000000000000) (1 137438953472 3fc0000000000000) (0 274877906944 0000000000000000) (0 824633720832 0000000000000000) (1 824633720832 3dbc000000000000) (0 1099511627776 0000000000000000) (1 1099511627776 3fd0000000000000) (0 549755813888 0000000000000000) (0 2199023255555 0000000000000000) (1 2199023255555 3fdffffffffd0000) (0 2473901162496 0000000000000000) (0 4398046511104 0000000000000000)
gamma2 2: (1 0 0000000000000000) (1 0 3fd0000000000000) (0 137438953472 0000000000000000) (1 137438953472 3fc0000000000000) (0 274877906944 0000000000000000) (0 824633720832 0000000000000000) (1 824633720832 3dbc000000000000) (0 1099511627776 0000000000000000) (1 1099511627776 3fd0000000000000) (0 549755813888 0000000000000000) (0 2199023255555 0000000000000000) (1 2199023255555 3fdffffffffd0000) (0 2473901162496 0000000000000000) (0 4398046511104 0000000000000000)
seq3: 137438953472 302365697638 384829069721 632219185970 645963081317 838377616177 1168231104509 1223206685897 1278182267285 1498084592840 1759218604436 1759218604436 1786706395130 1896657557907 2721291278739 2762522964780
gamma3 0: (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000) (0 0 0000000000000000)
gamma3 0.5: (1 0 3fe0000000000000) (0 302365697638 0000000000000000) (1 302365697638 3fd3333333330000) (0 632219185970 0000000000000000) (1 632219185970 3fa9999999980000) (0 838377616177 0000000000000000) (0 1168231104509 0000000000000000) (1 1168231104509 3fc9999999980000) (0 1278182267285 0000000000000000) (1 1278182267285 3fe9999999998000) (0 1759218604436 0000000000000000) (1 1759218604436 0000000000000000) (1 1759218604436 3fb9999999980000) (0 1896657557907 0000000000000000) (0 2721291278739 0000000000000000) (1 2721291278739 3fc3333333320000)
gamma3 1: (1 0 3fe0000000000000) (0 302365697638 0000000000000000) (1 302365697638 3fd3333333330000) (0 632219185970 0000000000000000) (1 632219185970 3fa9999999980000) (0 838377616177 0000000000000000) (0 1168231104509 0000000000000000) (1 1168231104509 3fc9999999980000) (0 1278182267285 0000000000000000) (1 1278182267285 3fe9999999998000) (0 1759218604436 0000000000000000) (1 1759218604436 0000000000000000) (1 1759218604436 3fb9999999980000) (0 1896657557907 0000000000000000) (0 2721291278739 0000000000000000) (1 2721291278739 3fc3333333320000)
gamma3 1.5: (1 0 3fe0000000000000) (0 302365697638 0000000000000000) (1 302365697638 3fd3333333330000) (0 632219185970 0000000000000000) (1 632219185970 3fa9999999980000) (0 838377616177 0000000000000000) (0 1168231104509 0000000000000000) (1 1168231104509 3fc9999999980000) (0 1278182267285 0000000000000000) (1 1278182267285 3fe9999999998000) (0 1759218604436 0000000000000000) (1 1759218604436 0000000000000000) (1 1759218604436 3fb9999999980000) (0 1896657557907 0000000000000000) (0 2721291278739 0000000000000000) (1 2721291278739 3fc3333333320000)
gamma3 2: (1 0 3fe0000000000000) (0 302365697638 0000000000000000) (1 302365697638 3fd3333333330000) (0 632219185970 0000000000000000) (1 632219185970 3fa9999999980000) (0 838377616177 0000000000000000) (0 1168231104509 0000000000000000) (1 1168231104509 3fc9999999980000) (1 1168231104509 3fc9999999980000) (0 1498084592840 0000000000000000) (1 1498084592840 3fee666666660000) (1 1498084592840 3fee666666660000) (0 1786706395130 0000000000000000) (1 1786706395130 3fd9999999990000) (0 2721291278739 0000000000000000) (1 2721291278739 3fc3333333320000)
mingap 0.8 0 -> 2
mingap 0.8 3 -> 5
mingap 0.8 8 -> 9
mingap 0.8 9 -> 9
mingap 0.5 0 -> 3
mingap 0.5 3 -> 5
mingap 0.5 8 -> 9
mingap 0.5 9 -> 9
mingap 0.1 0 -> 8
mingap 0.1 3 -> 8
mingap 0.1 8 -> 9
mingap 0.1 9 -> 9
mingap 0.95 0 -> 1
mingap 0.95 3 -> 5
mingap 0.95 8 -> 9
mingap 0.95 9 -> 9
mingap 0 0 -> 9
mingap 0 3 -> 9
mingap 0 8 -> 9
mingap 0 9 -> 9
minextent 0.8 0 -> 2
minextent 0.8 3 -> 7
minextent 0.8 50 -> 100
minextent 0.8 100 -> 100
minextent 0.5 0 -> 7
minextent 0.5 3 -> 7
minextent 0.5 50 -> 100
minextent 0.5 100 -> 100
minextent 0.1 0 -> 50
minextent 0.1 3 -> 50
minextent 0.1 50 -> 100
minextent 0.1 100 -> 100
minextent 0.95 0 -> 2
minextent 0.95 3 -> 7
minextent 0.95 50 -> 100
minextent 0.95 100 -> 100
minextent 0 0 -> 100
minextent 0 3 -> 100
minextent 0 50 -> 100
minextent 0 100 -> 100
mindrop 0.8 0 -> 2
mindrop 0.8 3 -> 7
mindrop 0.8 9 -> 18446744073709551610
mindrop 0.5 0 -> 3
mindrop 0.5 3 -> 7
mindrop 0.5 9 -> 18446744073709551610
mindrop 0.1 0 -> 9
mindrop 0.1 3 -> 9
mindrop 0.1 9 -> 18446744073709551610
mindrop 0.95 0 -> 1
mindrop 0.95 3 -> 7
mindrop 0.95 9 -> 18446744073709551610
mindrop 0 0 -> 18446744073709551610
mindrop 0 3 -> 18446744073709551610
mindrop 0 9 -> 18446744073709551610
mindrop empty -> 18446744073709551615
minattr 0.8 -inf 0 -> 4008000000000000
minattr 0.8 -inf 1 -> fff0000000000000
minattr 0.8 inf 0 -> 7ff0000000000000
minattr 0.8 inf 1 -> 401d000000000000
minattr 0.8 3 0 -> 4016000000000000
minattr 0.8 3 1 -> 4000000000000000
minattr 0.8 7 0 -> 401d000000000000
minattr 0.8 7 1 -> 4016000000000000
minattr 0.5 -inf 0 -> 4008000000000000
minattr 0.5 -inf 1 -> fff0000000000000
minattr 0.5 inf 0 -> 7ff0000000000000
minattr 0.5 inf 1 -> 4008000000000000
minattr 0.5 3 0 -> 4016000000000000
minattr 0.5 3 1 -> 4000000000000000
minattr 0.5 7 0 -> 401d000000000000
minattr 0.5 7 1 -> 4008000000000000
minattr 0.1 -inf 0 -> 4022000000000000
minattr 0.1 -inf 1 -> fff0000000000000
minattr 0.1 inf 0 -> 7ff0000000000000
minattr 0.1 inf 1 -> bff0000000000000
minattr 0.1 3 0 -> 4022000000000000
minattr 0.1 3 1 -> bff0000000000000
minattr 0.1 7 0 -> 4022000000000000
minattr 0.1 7 1 -> bff0000000000000
minattr 0.95 -inf 0 -> 4000000000000000
minattr 0.95 -inf 1 -> fff0000000000000
minattr 0.95 inf 0 -> 7ff0000000000000
minattr 0.95 inf 1 -> 4020000000000000
minattr 0.95 3 0 -> 4016000000000000
minattr 0.95 3 1 -> 4000000000000000
minattr 0.95 7 0 -> 401d000000000000
minattr 0.95 7 1 -> 4016000000000000
minattr 0 -inf 0 -> 4022000000000000
minattr 0 -inf 1 -> fff0000000000000
minattr 0 inf 0 -> 7ff0000000000000
minattr 0 inf 1 -> bff0000000000000
minattr 0 3 0 -> 4022000000000000
minattr 0 3 1 -> bff0000000000000
minattr 0 7 0 -> 4022000000000000
minattr 0 7 1 -> bff0000000000000
dropseq 0 0 -> 18446744073709551615
dropseq 0 1 -> 18446744073709551615
dropseq 0 12345678901234 -> 17873533690969063423
dropseq 0 18446744073709551615 -> 17870283321406128128
dropseq 3 0 -> 16717361816799281151
dropseq 3 1 -> 16717361816799281151
dropseq 3 12345678901234 -> 16144151434058792959
dropseq 3 18446744073709551615 -> 16140901064495857664
dropseq 31 0 -> 576460752303423487
dropseq 31 1 -> 576460752303423487
dropseq 31 12345678901234 -> 3250369562935295
dropseq 31 18446744073709551615 -> 0
dropseq 40 0 -> 576460752303423487
dropseq 40 1 -> 576460752303423487
dropseq 40 12345678901234 -> 3250369562935295
dropseq 40 18446744073709551615 -> 0
dropseq -1 0 -> 576460752303423487
dropseq -1 1 -> 576460752303423487
dropseq -1 12345678901234 -> 3250369562935295
dropseq -1 18446744073709551615 -> 0
dropseq 127 0 -> 576460752303423487
dropseq 127 1 -> 576460752303423487
dropseq 127 12345678901234 -> 3250369562935295
dropseq 127 18446744073709551615 -> 0
";

    fn harness_output() -> String {
        use std::fmt::Write as _;
        let mut out = String::new();

        let scale = (1i64 << (64 - 2 * (5 + 8))) as f64;
        let gammas = [0.0, 0.5, 1.0, 1.5, 2.0];
        let run = |out: &mut String, label: &str, seq: &[u64]| {
            for &g in &gammas {
                let mut prev = 0u64;
                let mut gap = 0.0;
                write!(out, "{} {}:", label, g).unwrap();
                for &i in seq {
                    let r = manage_gap(i, &mut prev, scale, g, &mut gap);
                    write!(out, " ({} {} {:016x})", r, prev, gap.to_bits()).unwrap();
                }
                writeln!(out).unwrap();
            }
        };

        let idx = [
            0,
            100,
            1u64 << 40,
            (1u64 << 40) + 5,
            1u64 << 41,
            3u64 << 40,
            3u64 << 40,
            1u64 << 42,
            5,
            1u64 << 43,
        ];
        run(&mut out, "gamma", &idx);

        let seq2 = [
            0,
            1u64 << 36,
            1u64 << 37,
            (1u64 << 37) + (1u64 << 35),
            1u64 << 38,
            3u64 << 38,
            (3u64 << 38) + 7,
            1u64 << 40,
            (1u64 << 40) + (1u64 << 36),
            1u64 << 39,
            (1u64 << 41) + 3,
            (1u64 << 41) + (1u64 << 37),
            (1u64 << 41) + (1u64 << 38),
            1u64 << 42,
        ];
        run(&mut out, "gamma2", &seq2);

        let pcts: [u64; 16] = [
            50, 60, 30, 90, 5, 70, 120, 20, 20, 80, 95, 0, 10, 40, 300, 15,
        ];
        let mut seq3 = Vec::new();
        let mut acc = 0u64;
        for p in pcts {
            acc += p * (1u64 << 38) / 100;
            seq3.push(acc);
        }
        write!(out, "seq3:").unwrap();
        for i in &seq3 {
            write!(out, " {}", i).unwrap();
        }
        writeln!(out).unwrap();
        run(&mut out, "gamma3", &seq3);

        let fs = [0.8, 0.5, 0.1, 0.95, 0.0];
        let gaps: Vec<u64> = vec![5, 1, 9, 3, 3, 3, 7, 2, 8, 3];
        for f in fs {
            for e in [0u64, 3, 8, 9] {
                let mut g2 = gaps.clone();
                writeln!(
                    out,
                    "mingap {} {} -> {}",
                    f,
                    e,
                    choose_mingap(&mut g2, f, e)
                )
                .unwrap();
            }
        }
        let ext: Vec<i64> = vec![50, -1, 9, 3, 3, 30, 7, 2, 8, 3, 100];
        for f in fs {
            for e in [0i64, 3, 50, 100] {
                let mut g2 = ext.clone();
                writeln!(
                    out,
                    "minextent {} {} -> {}",
                    f,
                    e,
                    choose_minextent(&mut g2, f, e)
                )
                .unwrap();
            }
        }
        let ds: Vec<u64> = vec![u64::MAX - 5, 1, 9, 3, 3, 3, 7, 2, 8, 3];
        for f in fs {
            for e in [0u64, 3, 9] {
                let mut g2 = ds.clone();
                writeln!(
                    out,
                    "mindrop {} {} -> {}",
                    f,
                    e,
                    choose_mindrop_sequence(&mut g2, f, e)
                )
                .unwrap();
            }
        }
        writeln!(
            out,
            "mindrop empty -> {}",
            choose_mindrop_sequence(&mut [], 0.5, 3)
        )
        .unwrap();
        let av: Vec<f64> = vec![5.5, -1.0, 9.0, 3.0, 3.0, 3.0, 7.25, 2.0, 8.0, 3.0];
        for f in fs {
            for e in [f64::NEG_INFINITY, f64::INFINITY, 3.0, 7.0] {
                for d in 0..2 {
                    let mut g2 = av.clone();
                    let m = choose_minattribute(&mut g2, f, e, d != 0);
                    writeln!(out, "minattr {} {} {} -> {:016x}", f, e, d, m.to_bits()).unwrap();
                }
            }
        }
        for mz in [0i8, 3, 31, 40, -1, 127] {
            for ix in [0u64, 1, 12345678901234, u64::MAX] {
                let sf = SerialFeature {
                    feature_minzoom: mz,
                    index: ix,
                    ..Default::default()
                };
                writeln!(
                    out,
                    "dropseq {} {} -> {}",
                    mz,
                    ix,
                    calculate_drop_sequence(&sf)
                )
                .unwrap();
            }
        }

        out
    }

    #[test]
    fn matches_the_cpp_harness() {
        let out = harness_output();
        for (i, (a, b)) in out.lines().zip(EXPECTED.lines()).enumerate() {
            assert_eq!(a, b, "line {}", i + 1);
        }
        assert_eq!(out.lines().count(), EXPECTED.lines().count());
    }

    #[test]
    fn dropping_features_starts_at_the_thresholds_that_exclude_nothing() {
        assert!(!dropping_features(0, 0, 0, f64::NEG_INFINITY, false));
        assert!(!dropping_features(0, 0, 0, f64::INFINITY, true));
        assert!(dropping_features(0, 0, 0, f64::INFINITY, false));
        assert!(dropping_features(0, 0, 0, f64::NEG_INFINITY, true));
        assert!(dropping_features(1, 0, 0, f64::NEG_INFINITY, false));
        assert!(dropping_features(0, 1, 0, f64::NEG_INFINITY, false));
        assert!(dropping_features(0, 0, 1, f64::NEG_INFINITY, false));
        assert!(dropping_features(0, 0, 0, 3.5, false));
    }

    #[test]
    fn add_sample_to_halves_the_sample_when_it_gets_too_big() {
        let mut vals: Vec<usize> = Vec::new();
        let mut increment = 1;
        for seq in 0..100001 {
            add_sample_to(&mut vals, seq, &mut increment, seq);
        }
        assert_eq!(increment, 2);
        assert_eq!(vals.len(), 50001);
        assert_eq!(vals[1], 2);
        assert_eq!(*vals.last().unwrap(), 100000);

        add_sample_to(&mut vals, 100001, &mut increment, 100001);
        assert_eq!(vals.len(), 50001);
        add_sample_to(&mut vals, 100002, &mut increment, 100002);
        assert_eq!(vals.len(), 50002);
    }

    #[test]
    fn revive_polygon_makes_a_square_of_the_lost_area() {
        let geom = vec![
            Draw::new(VT_MOVETO, 100, 100),
            Draw::new(VT_LINETO, 300, 100),
            Draw::new(VT_LINETO, 300, 300),
            Draw::new(VT_LINETO, 100, 300),
            Draw::new(VT_LINETO, 100, 100),
            Draw::new(VT_CLOSEPATH, 0, 0),
        ];
        // z0 with 12 bits of detail: a tile pixel is 2^20 world units on a side
        let out = revive_polygon(&geom, 9.0 * (1u64 << 40) as f64, 0, 12);
        let xy: Vec<(i64, i64)> = out.iter().map(|d| (d.x, d.y)).collect();
        // the center is (900 / 5, 900 / 5), and the square is 3 by 3
        assert_eq!(
            xy,
            vec![(179, 179), (182, 179), (182, 182), (179, 182), (179, 179)]
        );
        assert_eq!(out[0].op, VT_MOVETO);
        assert!(out[1..].iter().all(|d| d.op == VT_LINETO));

        assert!(revive_polygon(&geom, 0.0, 0, 12).is_empty());
        assert!(revive_polygon(&vec![], 5.0, 0, 12).is_empty());
    }

    #[test]
    fn draws_something_needs_a_lineto_that_moves() {
        let dot = vec![Draw::new(VT_MOVETO, 1, 1), Draw::new(VT_LINETO, 1, 1)];
        assert!(!draws_something(&dot));
        let line = vec![Draw::new(VT_MOVETO, 1, 1), Draw::new(VT_LINETO, 1, 2)];
        assert!(draws_something(&line));
        let jump = vec![Draw::new(VT_MOVETO, 1, 1), Draw::new(VT_MOVETO, 5, 5)];
        assert!(!draws_something(&jump));
    }

    #[test]
    fn coalcmp_compares_type_id_and_full_attributes() {
        let mut key_pool = KeyPool::default();
        let feature = |t: i8, id: Option<u64>, attrs: &[(&str, i32, &str)], kp: &mut KeyPool| {
            let mut sf = SerialFeature {
                t,
                has_id: id.is_some(),
                id: id.unwrap_or(0),
                ..Default::default()
            };
            for &(k, ty, v) in attrs {
                sf.full_keys.push(kp.pool(k));
                sf.full_values.push(SerialVal::new(ty, v));
            }
            sf
        };
        let a = feature(1, None, &[("name", MVT_STRING, "a")], &mut key_pool);
        let b = feature(1, None, &[("name", MVT_STRING, "a")], &mut key_pool);
        let c = feature(1, None, &[("name", MVT_STRING, "b")], &mut key_pool);
        let d = feature(2, None, &[("name", MVT_STRING, "a")], &mut key_pool);
        let e = feature(1, Some(7), &[("name", MVT_STRING, "a")], &mut key_pool);
        let f = feature(1, None, &[("name", MVT_DOUBLE, "a")], &mut key_pool);
        let g = feature(1, None, &[], &mut key_pool);

        assert_eq!(coalcmp(&a, &b), 0);
        assert!(coalcmp(&a, &c) < 0 && coalcmp(&c, &a) > 0);
        assert!(coalcmp(&a, &d) < 0 && coalcmp(&d, &a) > 0);
        assert!(coalcmp(&a, &e) < 0 && coalcmp(&e, &a) > 0);
        assert!(coalcmp(&a, &f) < 0 && coalcmp(&f, &a) > 0);
        assert!(coalcmp(&g, &a) < 0 && coalcmp(&a, &g) > 0);

        let mut a2 = feature(1, None, &[("name", MVT_STRING, "a")], &mut key_pool);
        a2.index = 5;
        a2.geometry = vec![Draw::new(VT_MOVETO, 0, 1)];
        let mut b2 = feature(1, None, &[("name", MVT_STRING, "a")], &mut key_pool);
        b2.index = 5;
        b2.geometry = vec![Draw::new(VT_MOVETO, 0, 2)];
        assert!(coalindexcmp(&a2, &b2) < 0);
        b2.index = 4;
        assert!(coalindexcmp(&a2, &b2) > 0);
    }

    #[test]
    fn multiplier_clusters_put_the_marker_on_the_first_feature() {
        let marker = "tippecanoe:retain_points_multiplier_first";
        let mut key_pool = KeyPool::default();
        let mut features: Vec<SerialFeaturePtr> = Vec::new();
        for (seq, first) in [(0, true), (1, false), (2, false), (3, true), (4, false)] {
            let mut sf = SerialFeature {
                seq,
                index: seq as u64 + 1,
                ..Default::default()
            };
            sf.full_keys.push(key_pool.pool("n"));
            sf.full_values
                .push(SerialVal::new(MVT_DOUBLE, &seq.to_string()));
            if first {
                sf.full_keys.push(key_pool.pool(marker));
                sf.full_values.push(SerialVal::new(MVT_BOOL, "true"));
            }
            features.push(Box::new(sf));
        }

        let clusters = assemble_multiplier_clusters(features);
        assert_eq!(clusters.len(), 5); // retain_points_multiplier is 1 by default
        let mut clusters: Vec<Vec<SerialFeaturePtr>> =
            vec![clusters.into_iter().flatten().collect()];

        // a cluster whose marker is on a later feature
        clusters[0].swap(0, 1);
        let out = disassemble_multiplier_clusters(clusters);
        assert_eq!(out[0].seq, 1);
        assert_eq!(*out[0].full_keys.last().unwrap().as_str(), *marker);
        assert!(
            out[1..]
                .iter()
                .filter(|f| f.full_keys.iter().any(|k| **k == marker))
                .count()
                == 1
        );
        // the rest are in drop sequence order, last-dropped first
        let seqs: Vec<u64> = out[1..]
            .iter()
            .map(|f| calculate_drop_sequence(f))
            .collect();
        let mut sorted = seqs.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(seqs, sorted);
    }

    #[test]
    fn layer_unmaps_reads_input_and_own_segments() {
        let input = vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["c".to_string()],
        ];
        let own = vec!["d".to_string()];
        let lu = LayerUnmaps {
            input: &input,
            tiling_seg: 5,
            own: &own,
        };
        assert_eq!(lu.get(0, 1), "b");
        assert_eq!(lu.get(1, 0), "c");
        assert_eq!(lu.get(5, 0), "d");
    }

    #[test]
    fn atomic_strategy_adds_up() {
        let s = AtomicStrategy::default();
        let one = Strategy {
            dropped_by_rate: 1,
            dropped_by_gamma: 2,
            dropped_as_needed: 3,
            coalesced_as_needed: 4,
            detail_reduced: 5,
            tiny_polygons: 6,
            truncated_zooms: 7,
            tile_size: 100,
            feature_count: 200,
        };
        s.add_from(&one);
        s.add_from(&one);
        let total = Strategy::new(&s, 9, 10);
        assert_eq!(total.dropped_by_rate, 2);
        assert_eq!(total.dropped_by_gamma, 4);
        assert_eq!(total.dropped_as_needed, 6);
        assert_eq!(total.coalesced_as_needed, 8);
        assert_eq!(total.detail_reduced, 10);
        assert_eq!(total.tiny_polygons, 12);
        assert_eq!(total.truncated_zooms, 14);
        assert_eq!(total.tile_size, 9);
        assert_eq!(total.feature_count, 10);
    }
}
