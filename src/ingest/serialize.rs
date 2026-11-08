//! What tippecanoe's `serialize_feature` (the one in serial.cpp that the input
//! readers call) does to a feature, ending in a chunk record instead of the
//! temporary files. The record holds what tippecanoe's tiling reads back.

use std::borrow::Cow;
use std::collections::HashMap;

use tippecanoe::clip::{
    clip_lines_bbox, clip_point_bbox, distance_from_line, fix_polygon, get_area, remove_noop,
    simple_clip_poly_bbox,
};
use tippecanoe::geometry::{
    Draw, DrawVec, VT_LINE, VT_LINETO, VT_MOVETO, VT_POINT, VT_POLYGON, center_of_mass_mp,
    polygon_to_anchor,
};
use tippecanoe::milo::dtoa_milo;
use tippecanoe::mvt::{MVT_DOUBLE, MVT_STRING};
use tippecanoe::options::*;
use tippecanoe::projection::{encode_hilbert, encode_quadkey, encode_vertex};
use tippecanoe::serial::{SerialVal, Vertex, coerce_value};

use super::chunk::{Chunk, MessageKind};
use crate::options::Options;
use crate::store::{Attr, Feature};

/// What reading needs from the options, worked out once.
pub struct Config<'o> {
    pub o: &'o Options,
    pub project: crate::crs::Project,
    pub projection_alias: String,
    /// `--h3`: how points are binned into hexagons.
    pub h3: Option<std::sync::Arc<super::h3::Setup>>,
    pub encode_index: fn(u32, u32) -> u64,
    pub geometry_scale: i32,
    pub want_dist: bool,
    pub filters: bool,
    pub uses_gamma: bool,
    /// Whether tippecanoe keeps the index in the serialized feature.
    pub keep_index: bool,
    /// Whether tippecanoe calculates the extent.
    pub want_extent: bool,
    pub set_attributes: Vec<(String, SerialVal)>,
}

impl<'o> Config<'o> {
    pub fn new(o: &'o Options) -> Config<'o> {
        let a = |c| o.additional.get(c);
        let uses_gamma = o.gamma != 0.0;
        Config {
            o,
            project: o.project(),
            projection_alias: o.projection_alias(),
            h3: o.h3.then(|| {
                std::sync::Arc::new(super::h3::Setup::new(
                    o.minzoom,
                    o.h3_base(),
                    &o.attribute_accum,
                    o.h3_categories.clone(),
                ))
            }),
            encode_index: if a(A_HILBERT) {
                encode_hilbert
            } else {
                encode_quadkey
            },
            geometry_scale: o.geometry_scale,
            want_dist: o.guess_maxzoom,
            filters: o.filters(),
            uses_gamma,
            keep_index: a(A_DROP_DENSEST_AS_NEEDED)
                || a(A_COALESCE_DENSEST_AS_NEEDED)
                || a(A_CLUSTER_DENSEST_AS_NEEDED)
                || a(A_CALCULATE_FEATURE_DENSITY)
                || a(A_DROP_SMALLEST_AS_NEEDED)
                || a(A_COALESCE_SMALLEST_AS_NEEDED)
                || a(A_DROP_FRACTION_AS_NEEDED)
                || a(A_COALESCE_FRACTION_AS_NEEDED)
                || o.prevent.get(P_DYNAMIC_DROP)
                || a(A_INCREASE_GAMMA_AS_NEEDED)
                || a(A_GENERATE_POLYGON_LABEL_POINTS)
                || uses_gamma
                || o.retain_points_multiplier > 1
                || o.preserve_multiplier_density_threshold > 0
                || o.cluster_distance != 0,
            want_extent: a(A_DROP_SMALLEST_AS_NEEDED)
                || a(A_COALESCE_SMALLEST_AS_NEEDED)
                || o.order_by_size
                || o.guess_maxzoom,
            set_attributes: o
                .set_attributes
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    #[allow(non_snake_case)]
    fn SHIFT_RIGHT(&self, a: i64) -> i64 {
        ((a as f64) / ((1i64 << self.geometry_scale) as f64)).round() as i64
    }

    #[allow(non_snake_case)]
    fn SHIFT_LEFT(&self, a: i64) -> i64 {
        // Offset coordinates to keep them positive
        const COORD_OFFSET: i64 = 4i64 << 32;
        ((a + (COORD_OFFSET >> self.geometry_scale)) << self.geometry_scale) - COORD_OFFSET
    }
}

/// A feature as a reader hands it over, tippecanoe's `serial_feature` before
/// serialization.
pub struct InFeature<'s> {
    pub t: i8,
    pub geometry: DrawVec,
    pub has_id: bool,
    pub id: u64,
    pub tippecanoe_minzoom: i32,
    pub tippecanoe_maxzoom: i32,
    pub full_keys: Vec<Cow<'s, str>>,
    pub full_values: Vec<SerialVal>,
}

fn strip_zeroes(s: &str) -> &str {
    // Doesn't do anything special with '-' followed by leading zeros
    // since integer IDs must be positive
    s.trim_start_matches('0')
}

/// What `%s` prints of `s`: up to its first NUL.
fn cstr(s: &str) -> &str {
    match s.find('\0') {
        Some(n) => &s[..n],
        None => s,
    }
}

/// The `initial_x` and `initial_y` that `scale_geometry` sets from the first
/// point of the first feature of a segment.
fn initial_point(cfg: &Config<'_>, geom: &DrawVec) -> Option<(u32, u32)> {
    let g = geom
        .iter()
        .find(|g| g.op == VT_MOVETO || g.op == VT_LINETO)?;
    let (x, y) = (g.x, g.y);
    let world = 0..1i64 << 32;
    if !world.contains(&x) || !world.contains(&y) {
        Some((1 << 31, 1 << 31))
    } else {
        Some((
            cfg.SHIFT_LEFT(cfg.SHIFT_RIGHT(x)) as u32,
            cfg.SHIFT_LEFT(cfg.SHIFT_RIGHT(y)) as u32,
        ))
    }
}

/// tippecanoe's `scale_geometry`, without the initial offsets of the temporary
/// files, which [`initial_point`] works out on its own.
fn scale_geometry(cfg: &Config<'_>, bbox: &mut [i64; 4], geom: &mut DrawVec) {
    let mut offset: i64 = 0;
    let mut prev: i64 = 0;
    let mut has_prev = false;
    let scale = 1.0 / ((1 << cfg.geometry_scale) as f64);
    let wraparound = cfg.o.additional.get(A_DETECT_WRAPAROUND);
    let grid = cfg.o.additional.get(A_GRID_LOW_ZOOMS);

    for g in geom.iter_mut() {
        if g.op == VT_MOVETO || g.op == VT_LINETO {
            let mut x = g.x;
            let y = g.y;

            if wraparound {
                if g.op == VT_LINETO {
                    x += offset;
                    if has_prev {
                        // jumps at least 180° but not exactly 360°,
                        // which in some data sets is an intentional
                        // line across the world
                        if x - prev > (1i64 << 31) && x - prev != (1i64 << 32) {
                            offset -= 1i64 << 32;
                            x -= 1i64 << 32;
                        } else if prev - x > (1i64 << 31) && prev - x != (1i64 << 32) {
                            offset += 1i64 << 32;
                            x += 1i64 << 32;
                        }
                    }

                    has_prev = true;
                    prev = x;
                } else {
                    offset = 0;
                    prev = x;
                }
            }

            if x < bbox[0] {
                bbox[0] = x;
            }
            if y < bbox[1] {
                bbox[1] = y;
            }
            if x > bbox[2] {
                bbox[2] = x;
            }
            if y > bbox[3] {
                bbox[3] = y;
            }

            if grid {
                // If we are gridding, snap to the maxzoom grid in case the incoming data
                // is already supposed to be aligned to tile boundaries (but is not, exactly,
                // because of rounding error during projection).
                g.x = ((x as f64) * scale).round() as i64;
                g.y = ((y as f64) * scale).round() as i64;
            } else {
                g.x = cfg.SHIFT_RIGHT(x);
                g.y = cfg.SHIFT_RIGHT(y);
            }
        }
    }
}

/// `serialize_feature(sst, sf, layername)`: everything tippecanoe does to a
/// feature on its way into the temporary files. Returns false if the feature
/// was clipped away, which tippecanoe does not count.
pub fn serialize_feature(
    chunk: &mut Chunk,
    cfg: &Config<'_>,
    sf: &mut InFeature<'_>,
    layername: &str,
) -> bool {
    let o = cfg.o;
    let mut sf_bbox: [i64; 4] = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];

    for g in &sf.geometry {
        if g.op == VT_MOVETO || g.op == VT_LINETO {
            // standard -180 to 180 world plane
            //
            // mask X, since it wraps around
            // pin Y, since it doesn't

            let mut x = g.x & 0xFFFFFFFF;
            let y = g.y.clamp(0, 0xFFFFFFFF);

            let b1 = &mut chunk.file_bbox1;
            b1[0] = b1[0].min(x);
            b1[1] = b1[1].min(y);
            b1[2] = b1[2].max(x);
            b1[3] = b1[3].max(y);

            // shift the western hemisphere 360 degrees to the east
            if x < 0x80000000 {
                // prime meridian
                x += 0x100000000;
            }

            let b2 = &mut chunk.file_bbox2;
            b2[0] = b2[0].min(x);
            b2[1] = b2[1].min(y);
            b2[2] = b2[2].max(x);
            b2[3] = b2[3].max(y);
        }
    }

    if chunk.initial.is_none() {
        chunk.initial = initial_point(cfg, &sf.geometry);
    }

    // try to remind myself that the geometry in this function is in SCALED COORDINATES
    let mut scaled_geometry: DrawVec = std::mem::take(&mut sf.geometry);
    scale_geometry(cfg, &mut sf_bbox, &mut scaled_geometry);
    let t = sf.t as i32;

    // This has to happen after scaling so that the wraparound detection has happened first.
    // Otherwise the inner/outer calculation will be confused by bad geometries.
    if t == VT_POLYGON {
        scaled_geometry = fix_polygon(
            &scaled_geometry,
            o.prevent.get(P_USE_SOURCE_POLYGON_WINDING),
            o.prevent.get(P_REVERSE_SOURCE_POLYGON_WINDING),
        );
    }

    for c in &o.clip_bboxes {
        let (x1, y1, x2, y2) = (
            cfg.SHIFT_RIGHT(c.minx),
            cfg.SHIFT_RIGHT(c.miny),
            cfg.SHIFT_RIGHT(c.maxx),
            cfg.SHIFT_RIGHT(c.maxy),
        );
        if t == VT_POLYGON {
            scaled_geometry = simple_clip_poly_bbox(
                &mut scaled_geometry,
                x1,
                y1,
                x2,
                y2,
                o.prevent.get(P_SIMPLIFY_SHARED_NODES),
            );
        } else if t == VT_LINE {
            scaled_geometry = clip_lines_bbox(&mut scaled_geometry, x1, y1, x2, y2);
        } else if t == VT_POINT {
            scaled_geometry = clip_point_bbox(&mut scaled_geometry, x1, y1, x2, y2);
        }

        scaled_geometry = remove_noop(scaled_geometry, t, 0);

        sf_bbox = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
        for g in &scaled_geometry {
            let x = cfg.SHIFT_LEFT(g.x);
            let y = cfg.SHIFT_LEFT(g.y);
            sf_bbox[0] = sf_bbox[0].min(x);
            sf_bbox[1] = sf_bbox[1].min(y);
            sf_bbox[2] = sf_bbox[2].max(x);
            sf_bbox[3] = sf_bbox[3].max(y);
        }
    }

    if scaled_geometry.is_empty() {
        // Feature was clipped away
        return false;
    }

    if o.prevent.get(P_SIMPLIFY_SHARED_NODES) {
        // can leave no geometry, for a line or polygon whose points all fall
        // on one point of the world grid; the feature is kept, as in the C++
        scaled_geometry = remove_noop(scaled_geometry, t, 0);
        shared_nodes(chunk, cfg, t, &scaled_geometry);
    }

    let mut generate_id = false;
    if !sf.has_id && o.additional.get(A_GENERATE_IDS) {
        sf.has_id = true;
        generate_id = true;
    }

    if cfg.want_dist {
        let mut locs: Vec<u64> = Vec::new();
        for g in &scaled_geometry {
            if g.op == VT_MOVETO || g.op == VT_LINETO {
                locs.push((cfg.encode_index)(
                    cfg.SHIFT_LEFT(g.x) as u32,
                    cfg.SHIFT_LEFT(g.y) as u32,
                ));
            }
        }
        locs.sort_unstable();
        let mut n: usize = 0;
        let mut sum: f64 = 0.0;
        for i in 1..locs.len() {
            if locs[i - 1] != locs[i] {
                sum += ((locs[i] - locs[i - 1]) as f64).ln();
                n += 1;
            }
        }
        if n > 0 {
            let avg = (sum / n as f64).exp();
            // Convert approximately from tile units to feet
            // See comment about empirical data in main.cpp
            let dist_ft = avg.sqrt() / 33.0;

            chunk.dist_terms.push(dist_ft.ln() * n as f64);
            chunk.dist_count += n;
        }
    }

    let mut extent: f64 = 0.0;
    if cfg.want_extent {
        if t == VT_POLYGON {
            let mut i = 0;
            while i < scaled_geometry.len() {
                if scaled_geometry[i].op == VT_MOVETO {
                    let mut j = i + 1;
                    while j < scaled_geometry.len() && scaled_geometry[j].op == VT_LINETO {
                        j += 1;
                    }
                    extent += (cfg.SHIFT_LEFT(cfg.SHIFT_LEFT(1)) as f64)
                        * get_area(&scaled_geometry, i, j);
                    i = j - 1;
                }
                i += 1;
            }
        } else if t == VT_LINE {
            let mut dist: f64 = 0.0;
            for i in 1..scaled_geometry.len() {
                if scaled_geometry[i].op == VT_LINETO {
                    let xd = cfg.SHIFT_LEFT(scaled_geometry[i].x - scaled_geometry[i - 1].x) as f64;
                    let yd = cfg.SHIFT_LEFT(scaled_geometry[i].y - scaled_geometry[i - 1].y) as f64;
                    dist += (xd * xd + yd * yd).sqrt();
                }
            }
            // treat lines as having the area of a circle with the line as diameter
            extent = std::f64::consts::PI * (dist / 2.0) * (dist / 2.0);
        }

        // VT_POINT extent will be calculated in write_tile from the distance between adjacent features.
    }

    // Clamp before converting, since converting a double that is out of range
    // for a long long is undefined.
    let sf_extent: i64 = if extent >= i64::MIN as f64 && extent < i64::MAX as f64 {
        extent as i64
    } else if extent < 0.0 {
        i64::MIN
    } else {
        i64::MAX // also the NaN case
    };

    if cfg.want_dist && t == VT_POLYGON {
        chunk.area_terms.push(extent);
    }

    let midx: i64;
    let midy: i64;
    let a = |c| o.additional.get(c);

    if t == VT_POINT {
        // keep old behavior, which loses one bit of precision at the bottom
        midx = (sf_bbox[0] / 2 + sf_bbox[2] / 2) & ((1i64 << 32) - 1);
        midy = (sf_bbox[1] / 2 + sf_bbox[3] / 2) & ((1i64 << 32) - 1);
    } else if (a(A_DROP_DENSEST_AS_NEEDED) || a(A_COALESCE_DENSEST_AS_NEEDED)) && t == VT_POLYGON {
        // This probably should really apply to all polygons,
        // but I hate to change the feature sequence in all the
        // test fixtures again
        let scaled_center = center_of_mass_mp(&scaled_geometry);
        midx = cfg.SHIFT_LEFT(scaled_center.x) & ((1i64 << 32) - 1);
        midy = cfg.SHIFT_LEFT(scaled_center.y) & ((1i64 << 32) - 1);
    } else if scaled_geometry.is_empty() {
        // Emptied by --no-simplification-of-shared-nodes. The C++ reads past the
        // end of the empty geometry here; the bbox center is a defined stand-in.
        midx = (sf_bbox[0] / 2 + sf_bbox[2] / 2) & ((1i64 << 32) - 1);
        midy = (sf_bbox[1] / 2 + sf_bbox[3] / 2) & ((1i64 << 32) - 1);
    } else {
        // An arbitrary but predictable point from the geometry, chosen by a
        // hash of the geometry, as the index point.
        let mut ix: usize = 0;
        for g in &scaled_geometry {
            ix = ix.wrapping_add((g.x + g.y) as usize);
        }
        ix %= scaled_geometry.len();

        // If off the edge of the plane, mask to bring it back into the addressable area
        midx = cfg.SHIFT_LEFT(scaled_geometry[ix].x) & ((1i64 << 32) - 1);
        midy = cfg.SHIFT_LEFT(scaled_geometry[ix].y) & ((1i64 << 32) - 1);
    }

    let bbox_index: u64 = (cfg.encode_index)(midx as u32, midy as u32);
    if a(A_CALCULATE_INDEX) {
        sf.full_keys.push(Cow::Borrowed("tippecanoe:index"));
        sf.full_values.push(SerialVal {
            type_: MVT_DOUBLE,
            s: bbox_index.to_string(),
        });
    }

    let mut label_point: u64 = 0;
    if t == VT_POLYGON && a(A_GENERATE_POLYGON_LABEL_POINTS) {
        let mut dv = polygon_to_anchor(&scaled_geometry);
        if !dv.is_empty() {
            dv[0].x = cfg.SHIFT_LEFT(dv[0].x) & ((1i64 << 32) - 1);
            dv[0].y = cfg.SHIFT_LEFT(dv[0].y) & ((1i64 << 32) - 1);
            label_point = (cfg.encode_index)(dv[0].x as u32, dv[0].y as u32);
        }
    }

    let layer = chunk.layer(layername);
    if !cfg.filters {
        let l = &mut chunk.layers[layer as usize];
        if t == VT_POINT {
            l.points += 1;
        } else if t == VT_LINE {
            l.lines += 1;
        } else if t == VT_POLYGON {
            l.polygons += 1;
        }
    }

    for (k, v) in &cfg.set_attributes {
        match sf.full_keys.iter().position(|fk| fk.as_ref() == k.as_str()) {
            Some(i) => sf.full_values[i] = v.clone(),
            None => {
                sf.full_keys.push(Cow::Owned(k.clone()));
                sf.full_values.push(v.clone());
            }
        }
    }

    // --cluster-categories: the value to count, read before -x and -y
    let category = o.cluster_categories.as_ref().and_then(|(attr, values)| {
        let i = sf
            .full_keys
            .iter()
            .position(|k| k.as_ref() == attr.as_str())?;
        let v = cstr(&sf.full_values[i].s);
        let v = match values {
            Some(list) if !list.iter().any(|l| l == v) => "_other",
            _ => v,
        };
        Some(format!("{}:{}", attr, v))
    });

    let mut id = sf.id;
    let mut has_id = sf.has_id;
    let mut i = sf.full_keys.len() as isize - 1;
    while i >= 0 {
        let iu = i as usize;
        i -= 1;

        if !o.attribute_types.is_empty() {
            coerce(
                &sf.full_keys[iu],
                &mut sf.full_values[iu],
                &o.attribute_types,
            );
        }

        if o.prevent.get(P_SINGLE_PRECISION)
            && sf.full_values[iu].type_ == MVT_DOUBLE
            // don't coerce integers to floats, since that is counterproductive
            && sf.full_values[iu].s.contains('.')
        {
            sf.full_values[iu].s =
                dtoa_milo(tippecanoe::cstd::atof(&sf.full_values[iu].s) as f32 as f64);
        }

        if sf.full_keys[iu] == o.attribute_for_id.as_str() {
            let v = &sf.full_values[iu];
            if v.type_ != MVT_DOUBLE && !a(A_CONVERT_NUMERIC_IDS) {
                chunk.message(
                    MessageKind::Once("id not a number"),
                    None,
                    format!(
                        "Warning: Attribute \"{}\"=\"{}\" as feature ID is not a number",
                        cstr(&sf.full_keys[iu]),
                        cstr(&v.s)
                    ),
                );
            } else {
                let s = &v.s;
                let (u, consumed) = tippecanoe::cstd::strtoull(s, 10);
                let id_value = u as i64;

                if consumed < cstr(s).len() {
                    chunk.message(
                        MessageKind::Once("non-integer id"),
                        None,
                        format!(
                            "Warning: Can't represent non-integer feature ID {}",
                            cstr(s)
                        ),
                    );
                } else if id_value.to_string() != strip_zeroes(s) {
                    chunk.message(
                        MessageKind::Once("too-large id attribute"),
                        None,
                        format!("Warning: Can't represent too-large feature ID {}", cstr(s)),
                    );
                } else {
                    id = id_value as u64;
                    has_id = true;
                    generate_id = false;

                    sf.full_keys.remove(iu);
                    sf.full_values.remove(iu);
                    continue;
                }
            }
        }

        let key = sf.full_keys[iu].as_ref();
        let excluded = if o.exclude_all {
            !o.include.contains(key)
        } else {
            o.exclude.contains(key)
        };
        if excluded {
            sf.full_keys.remove(iu);
            sf.full_values.remove(iu);
            continue;
        }
    }

    if let Some(key) = category {
        sf.full_keys.push(Cow::Owned(key));
        sf.full_values.push(SerialVal {
            type_: MVT_DOUBLE,
            s: "1".to_string(),
        });
    }

    let max_samples = o.max_tilestats_sample_values;
    let attr_start = chunk.attrs.len() as u64;
    for (k, v) in sf.full_keys.iter().zip(sf.full_values.iter_mut()) {
        if !cfg.filters {
            let stats = &mut chunk.layers[layer as usize].stats;
            if let Some(st) = stats.get_mut(k.as_ref()) {
                st.add(v.type_, &v.s, max_samples);
            } else if v.type_ != tippecanoe::mvt::MVT_NULL {
                let mut st = super::chunk::Stat::default();
                st.add(v.type_, &v.s, max_samples);
                stats.insert(k.to_string(), st);
            }
        }

        if o.maximum_string_attribute_length > 0
            && v.type_ == MVT_STRING
            && v.s.len() > o.maximum_string_attribute_length
        {
            v.s = tippecanoe::text::truncate_string(&v.s, o.maximum_string_attribute_length);
        }

        // The string pool keeps C strings, which end at a NUL.
        let key = chunk.keys.intern(MVT_STRING, cstr(k).as_bytes());
        let value = chunk.values.intern(v.type_, cstr(&v.s).as_bytes());
        chunk.attrs.push(Attr { key, value });
    }

    // The geometry as the tiling reads it back from the temporary files.
    let geom_start = chunk.geometry.len() as u64;
    crate::store::Geometry::encode(&mut chunk.geometry, &scaled_geometry);
    chunk.geometry_vertices += scaled_geometry.len() as u64;

    let fb = &mut chunk.file_bbox;
    fb[0] = fb[0].min(sf_bbox[0]);
    fb[1] = fb[1].min(sf_bbox[1]);
    fb[2] = fb[2].max(sf_bbox[2]);
    fb[3] = fb[3].max(sf_bbox[3]);

    if generate_id {
        chunk.generate_ids.push(chunk.features.len() as u32);
    }

    // --h3: a point is counted into its hexagons, and shows from the base zoom
    let mut tippecanoe_minzoom = sf.tippecanoe_minzoom;
    if let Some(h3) = &cfg.h3
        && t == VT_POINT
    {
        let points = scaled_geometry
            .iter()
            .filter(|g| g.op == VT_MOVETO)
            .map(|g| (cfg.SHIFT_LEFT(g.x), cfg.SHIFT_LEFT(g.y)));
        chunk.h3.get_or_insert_with(Default::default).add(
            h3,
            layername,
            points,
            &sf.full_keys,
            &sf.full_values,
        );
        tippecanoe_minzoom = tippecanoe_minzoom.max(h3.base);
    }

    let seq = chunk.features.len() as u64;
    chunk.features.push(Feature {
        keep_index: cfg.keep_index,
        ix: bbox_index,
        seq,
        geom_start,
        geom_len: scaled_geometry.len() as u32,
        attr_start,
        attr_len: (chunk.attrs.len() as u64 - attr_start) as u32,
        layer,
        t: sf.t,
        has_id,
        feature_minzoom: 0,
        tippecanoe_minzoom: tippecanoe_minzoom as i8,
        tippecanoe_maxzoom: sf.tippecanoe_maxzoom as i8,
        segment: 0,
    });
    let i = chunk.features.len() - 1;
    crate::store::set_sparse(&mut chunk.ids, i, if has_id { id } else { 0 });
    crate::store::set_sparse(&mut chunk.extents, i, sf_extent);
    crate::store::set_sparse(&mut chunk.label_points, i, label_point);

    true
}

/// `coerce_value` for an attribute with a type given by `-T`.
fn coerce(key: &str, v: &mut SerialVal, types: &HashMap<String, i32>) {
    if types.contains_key(key) {
        coerce_value(key, &mut v.type_, &mut v.s, types);
    }
}

/// `add_scaled_node`
fn add_scaled_node(chunk: &mut Chunk, cfg: &Config<'_>, g: Draw) {
    let x = cfg.SHIFT_LEFT(g.x);
    let y = cfg.SHIFT_LEFT(g.y);
    chunk.nodes.push(encode_vertex(x as u32, y as u32));
}

/// The vertices and nodes of `--no-simplification-of-shared-nodes`: every
/// vertex with its neighbors, and the points that must not be simplified away.
fn shared_nodes(chunk: &mut Chunk, cfg: &Config<'_>, t: i32, sg: &DrawVec) {
    if t != VT_POLYGON && t != VT_LINE {
        return;
    }
    let mut i = 0;
    while i < sg.len() {
        if sg[i].op == VT_MOVETO {
            let mut j = i + 1;
            while j < sg.len() && sg[j].op == VT_LINETO {
                j += 1;
            }

            if t == VT_POLYGON && j - i >= 4 {
                for k in i..j - 1 {
                    // % (j - i - 1) because we don't want the duplicate last point
                    chunk.vertices.push(Vertex::new(
                        sg[(k - i) % (j - i - 1) + i],
                        sg[(k - i + 1) % (j - i - 1) + i],
                        sg[(k - i + 2) % (j - i - 1) + i],
                    ));
                }
            } else if t == VT_LINE && j - i >= 2 {
                let mut k = i;
                while k + 2 < j {
                    chunk
                        .vertices
                        .push(Vertex::new(sg[k], sg[k + 1], sg[k + 2]));
                    k += 1;
                }
            }

            // since the starting point is never simplified away,
            // don't let it be simplified away in any other polygons either.
            // Needs to appear twice here so that the check below will see
            // it as appearing in multiple features.
            add_scaled_node(chunk, cfg, sg[i]);

            if t == VT_LINE && j - i >= 2 {
                // linestrings also need to preserve the last point
                add_scaled_node(chunk, cfg, sg[j - 1]);
            } else if t == VT_POLYGON && j - i >= 4 {
                // To avoid letting polygons get simplified away to nothing,
                // also keep the furthest-away point from the initial point
                // (which Douglas-Peucker simplification would keep anyway,
                // if its search weren't being split up by polygon side).
                let mut far: f64 = 0.0;
                let mut which = i;
                for k in i + 1..j - 1 {
                    let xd = (sg[k].x - sg[i].x) as f64;
                    let yd = (sg[k].y - sg[i].y) as f64;
                    let d = xd * xd + yd * yd;
                    if d > far || ((d == far) && (sg[k] < sg[which])) {
                        far = d;
                        which = k;
                    }
                }

                add_scaled_node(chunk, cfg, sg[which]);

                // And, likewise, the point most distant from those two points,
                // which probably would also be the one that Douglas-Peucker
                // would keep next.
                far = 0.0;
                let mut which2 = i;
                for k in i + 1..j - 1 {
                    let d = distance_from_line(
                        sg[k].x,
                        sg[k].y,
                        sg[i].x,
                        sg[i].y,
                        sg[which].x,
                        sg[which].y,
                    );
                    if (d > far) || ((d == far) && (sg[k] < sg[which2])) {
                        far = d;
                        which2 = k;
                    }
                }

                add_scaled_node(chunk, cfg, sg[which2]);
            }

            i = j - 1;
        }
        i += 1;
    }
}
