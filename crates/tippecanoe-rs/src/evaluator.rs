//! Port of evaluator.cpp, the feature filter expressions.

// The getters build their values the way the C++ does, field by field.
#![allow(clippy::field_reassign_with_default)]

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::errors::{EXIT_FILTER, EXIT_IMPOSSIBLE, EXIT_JSON, EXIT_OPEN};
use crate::geometry::{Draw, VT_LINE, VT_LINETO, VT_MOVETO, VT_POINT, VT_POLYGON};
use crate::jsonpull::{JsonPull, JsonRef, JsonTree, JsonType};
use crate::mvt::{
    MVT_BOOL, MVT_DOUBLE, MVT_FLOAT, MVT_INT, MVT_LINESTRING, MVT_NO_SUCH_KEY, MVT_NULL, MVT_POINT,
    MVT_POLYGON, MVT_SINT, MVT_STRING, MVT_UINT, MvtFeature, MvtLayer, MvtValue,
};
use crate::projection::lonlat2tile;

/// `strcmp`, on the parts of the strings before any NUL. Only the sign of the
/// result matters to the callers.
fn strcmp(a: &str, b: &str) -> i32 {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let a = &a[..a.iter().position(|&c| c == 0).unwrap_or(a.len())];
    let b = &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())];
    match a.cmp(b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

fn compare(one: &MvtValue, two: JsonRef<'_>, fail: &mut bool) -> i32 {
    match one.type_ {
        MVT_STRING => {
            if two.type_() != JsonType::String {
                *fail = true;
                return false as i32; // string vs non-string
            }

            strcmp(one.get_str(), two.string())
        }

        MVT_DOUBLE | MVT_FLOAT | MVT_INT | MVT_UINT | MVT_SINT => {
            if two.type_() != JsonType::Number {
                *fail = true;
                return false as i32; // number vs non-number
            }

            let v: f64 = match one.type_ {
                MVT_DOUBLE => one.numeric_value.double_value(),
                MVT_FLOAT => one.numeric_value.float_value() as f64,
                MVT_INT => one.numeric_value.int_value() as f64,
                MVT_UINT => one.numeric_value.uint_value() as f64,
                MVT_SINT => one.numeric_value.sint_value() as f64,
                _ => {
                    eprintln!("Internal error: bad mvt type {}", one.type_);
                    std::process::exit(EXIT_IMPOSSIBLE);
                }
            };

            if v < two.number() {
                -1
            } else if v > two.number() {
                1
            } else {
                0
            }
        }

        MVT_BOOL => {
            if two.type_() != JsonType::True && two.type_() != JsonType::False {
                *fail = true;
                return false as i32; // bool vs non-bool
            }

            let b = two.type_() != JsonType::False;
            (one.numeric_value.bool_value() & !b) as i32
        }

        MVT_NULL => {
            if two.type_() != JsonType::Null {
                *fail = true;
                return false as i32; // null vs non-null
            }

            0 // null equals null
        }

        _ => {
            eprintln!("Internal error: bad mvt type {}", one.type_);
            std::process::exit(EXIT_IMPOSSIBLE);
        }
    }
}

/// `fprintf(stderr, "...%s\n", json_stringify(f).c_str())`, with the bytes of
/// the JSON as the C++ prints them.
fn json_message(prefix: &str, f: JsonRef<'_>) {
    let mut msg = prefix.as_bytes().to_vec();
    let json = f.stringify_raw();
    let n = json.iter().position(|&c| c == 0).unwrap_or(json.len());
    msg.extend_from_slice(&json[..n]);
    msg.push(b'\n');
    let _ = std::io::stderr().write_all(&msg);
}

static COMPARISON_NOT_FOUND_WARNED: AtomicBool = AtomicBool::new(false);
static COMPARISON_MISMATCH_WARNED: AtomicBool = AtomicBool::new(false);
static IN_NOT_FOUND_WARNED: AtomicBool = AtomicBool::new(false);
static IN_MISMATCH_WARNED: AtomicBool = AtomicBool::new(false);

/// Runs a C++ `static bool warned` check: true the first time only.
fn first_warning(warned: &AtomicBool) -> bool {
    !warned.swap(true, Ordering::Relaxed)
}

// 0: false
// 1: true
// -1: incomparable (sql null), treated as false in final output
#[allow(clippy::only_used_in_recursion)]
fn eval(
    feature: &dyn Fn(&str) -> MvtValue,
    f: JsonRef<'_>,
    exclude_attributes: &mut BTreeSet<String>,
    unidecode_data: &[String],
    geom: Option<&FilterGeometry<'_>>,
) -> i32 {
    if f.type_() == JsonType::True {
        return 1;
    } else if f.type_() == JsonType::False {
        return 0;
    } else if f.type_() == JsonType::Null {
        return 1;
    }

    if f.type_() == JsonType::Number {
        if f.number() == 0.0 {
            return 0;
        } else {
            return 1;
        }
    }

    if f.type_() == JsonType::String {
        if f.string().is_empty() {
            return 0;
        } else {
            return 1;
        }
    }

    if f.type_() != JsonType::Array {
        json_message("Filter is not an array: ", f);
        std::process::exit(EXIT_FILTER);
    }

    let a = f.array();

    if a.is_empty() {
        json_message("Array too small in filter: ", f);
        std::process::exit(EXIT_FILTER);
    }

    if a[0].type_() != JsonType::String {
        json_message("Filter operation is not a string: ", f);
        std::process::exit(EXIT_FILTER);
    }

    let op = a[0].string();

    if op == "has" || op == "!has" {
        if a.len() != 2 {
            json_message("Wrong number of array elements in filter: ", f);
            std::process::exit(EXIT_FILTER);
        }

        if op == "has" {
            if a[1].type_() != JsonType::String {
                json_message("\"has\" key is not a string: ", f);
                std::process::exit(EXIT_FILTER);
            }
            return (feature(a[1].string()).type_ != MVT_NO_SUCH_KEY) as i32;
        }

        if op == "!has" {
            if a[1].type_() != JsonType::String {
                json_message("\"!has\" key is not a string: ", f);
                std::process::exit(EXIT_FILTER);
            }
            return (feature(a[1].string()).type_ == MVT_NO_SUCH_KEY) as i32;
        }
    }

    if op == "==" || op == "!=" || op == ">" || op == ">=" || op == "<" || op == "<=" {
        if a.len() != 3 {
            json_message("Wrong number of array elements in filter: ", f);
            std::process::exit(EXIT_FILTER);
        }
        if a[1].type_() != JsonType::String {
            json_message("comparison key is not a string: ", f);
            std::process::exit(EXIT_FILTER);
        }

        let ff = feature(a[1].string());
        if ff.type_ == MVT_NO_SUCH_KEY {
            if first_warning(&COMPARISON_NOT_FOUND_WARNED) {
                json_message("Warning: attribute not found for comparison: ", f);
            }
            if op == "!=" {
                return true as i32; //  attributes that aren't found are not equal
            }
            return false as i32; // not found: comparison is false
        }

        let mut fail = false;
        let cmp = compare(&ff, a[2], &mut fail);

        if fail {
            if first_warning(&COMPARISON_MISMATCH_WARNED) {
                json_message("Warning: mismatched type in comparison: ", f);
            }
            if op == "!=" {
                return true as i32; // mismatched types are not equal
            }
            return false as i32;
        }

        if op == "==" {
            return (cmp == 0) as i32;
        }
        if op == "!=" {
            return (cmp != 0) as i32;
        }
        if op == ">" {
            return (cmp > 0) as i32;
        }
        if op == ">=" {
            return (cmp >= 0) as i32;
        }
        if op == "<" {
            return (cmp < 0) as i32;
        }
        if op == "<=" {
            return (cmp <= 0) as i32;
        }

        json_message("Internal error: can't happen: ", f);
        std::process::exit(EXIT_IMPOSSIBLE);
    }

    if op == "all" || op == "any" || op == "none" {
        let mut v = op == "all";

        for elem in a.iter().skip(1) {
            let out = eval(feature, *elem, exclude_attributes, unidecode_data, geom);

            if out >= 0 {
                // nulls are ignored in boolean and/or expressions
                if op == "all" {
                    v = v && out != 0;
                    if !v {
                        break;
                    }
                } else {
                    v = v || out != 0;
                    if v {
                        break;
                    }
                }
            }
        }

        if op == "none" {
            return (!v) as i32;
        } else {
            return v as i32;
        }
    }

    if op == "in" || op == "!in" {
        if a.len() < 2 {
            json_message("Array too small in filter: ", f);
            std::process::exit(EXIT_FILTER);
        }

        if a[1].type_() != JsonType::String {
            json_message("\"!in\" key is not a string: ", f);
            std::process::exit(EXIT_FILTER);
        }

        let ff = feature(a[1].string());
        if ff.type_ == MVT_NO_SUCH_KEY {
            if first_warning(&IN_NOT_FOUND_WARNED) {
                json_message("Warning: attribute not found for comparison: ", f);
            }
            if op == "!in" {
                return true as i32; // attributes that aren't found are not in
            }
            return false as i32; // not found: comparison is false
        }

        let mut found = false;
        for elem in a.iter().skip(2) {
            let mut fail = false;
            let mut cmp = compare(&ff, *elem, &mut fail);

            if fail {
                if first_warning(&IN_MISMATCH_WARNED) {
                    json_message("Warning: mismatched type in comparison: ", f);
                }
                cmp = 1;
            }

            if cmp == 0 {
                found = true;
                break;
            }
        }

        if op == "in" {
            return found as i32;
        } else {
            return (!found) as i32;
        }
    }

    if op == "attribute-filter" {
        if a.len() != 3 {
            json_message("Wrong number of array elements in filter: ", f);
            std::process::exit(EXIT_FILTER);
        }

        if a[1].type_() != JsonType::String {
            json_message("\"attribute-filter\" key is not a string: ", f);
            std::process::exit(EXIT_FILTER);
        }

        let ok = eval(feature, a[2], exclude_attributes, unidecode_data, geom) > 0;
        if !ok {
            exclude_attributes.insert(a[1].string().to_string());
        }

        return true as i32;
    }

    // Not in tippecanoe: SQL's LIKE, with % for any run of characters and _ for
    // any one character. "ilike" ignores case. A number or boolean is matched
    // as its text, as a SQL cast to VARCHAR shows it.
    if op == "like" || op == "!like" || op == "ilike" || op == "!ilike" {
        if a.len() != 3 {
            json_message("Wrong number of array elements in filter: ", f);
            std::process::exit(EXIT_FILTER);
        }
        if a[1].type_() != JsonType::String || a[2].type_() != JsonType::String {
            json_message("\"like\" key and pattern must be strings: ", f);
            std::process::exit(EXIT_FILTER);
        }

        let negate = op.starts_with('!');
        let Some(text) = value_text(&feature(a[1].string())) else {
            return negate as i32; // not found or null: like the other negations, true
        };
        let found = if op.ends_with("ilike") {
            like(&text.to_lowercase(), &a[2].string().to_lowercase())
        } else {
            like(&text, a[2].string())
        };
        return (found != negate) as i32;
    }

    // Not in tippecanoe: whether the feature's geometry shares any point with a
    // GeoJSON Polygon or MultiPolygon in longitude and latitude.
    if op == "intersects" {
        if a.len() != 2 {
            json_message("Wrong number of array elements in filter: ", f);
            std::process::exit(EXIT_FILTER);
        }
        let Some(geom) = geom else {
            json_message(
                "\"intersects\" needs feature geometry, which only overzoom filters have: ",
                f,
            );
            std::process::exit(EXIT_FILTER);
        };
        return area_of(a[1]).intersects(geom) as i32;
    }

    json_message("Unknown filter ", f);
    std::process::exit(EXIT_FILTER);
}

/// A feature's geometry for "intersects": its draw commands in world coordinates
/// (2^32 to an axis, as `lonlat2tile` gives them at zoom 32), less `origin`.
pub struct FilterGeometry<'a> {
    pub draws: &'a [Draw],
    pub origin: (i64, i64),
    /// VT_POINT, VT_LINE or VT_POLYGON
    pub type_: i32,
}

/// The attribute as the text a SQL cast to VARCHAR gives, or None for null.
fn value_text(v: &MvtValue) -> Option<String> {
    match v.type_ {
        MVT_STRING => Some(v.get_str().to_string()),
        MVT_DOUBLE => Some(v.numeric_value.double_value().to_string()),
        MVT_FLOAT => Some(v.numeric_value.float_value().to_string()),
        MVT_INT => Some(v.numeric_value.int_value().to_string()),
        MVT_UINT => Some(v.numeric_value.uint_value().to_string()),
        MVT_SINT => Some(v.numeric_value.sint_value().to_string()),
        MVT_BOOL => Some(v.numeric_value.bool_value().to_string()),
        _ => None,
    }
}

/// SQL LIKE: % matches any run of characters, _ any one character. Only the
/// pattern has wildcards: a % or _ in the text is a plain character.
fn like(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0, 0);
    // where the last % was, and the text position it is matched up to
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        // % first, so that a % in the text cannot match it as a plain character
        if pi < p.len() && p[pi] == '%' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '%')
}

type Pt = (i64, i64);

/// Twice the signed area of a, b, c: positive when c is left of a to b.
fn orient(a: Pt, b: Pt, c: Pt) -> i128 {
    (b.0 - a.0) as i128 * (c.1 - a.1) as i128 - (b.1 - a.1) as i128 * (c.0 - a.0) as i128
}

/// c, collinear with a and b, lies within their bounding box.
fn on_segment(a: Pt, b: Pt, c: Pt) -> bool {
    a.0.min(b.0) <= c.0 && c.0 <= a.0.max(b.0) && a.1.min(b.1) <= c.1 && c.1 <= a.1.max(b.1)
}

fn segments_intersect(p1: Pt, p2: Pt, q1: Pt, q2: Pt) -> bool {
    let d1 = orient(q1, q2, p1);
    let d2 = orient(q1, q2, p2);
    let d3 = orient(p1, p2, q1);
    let d4 = orient(p1, p2, q2);
    if ((d1 > 0 && d2 < 0) || (d1 < 0 && d2 > 0)) && ((d3 > 0 && d4 < 0) || (d3 < 0 && d4 > 0)) {
        return true;
    }
    (d1 == 0 && on_segment(q1, q2, p1))
        || (d2 == 0 && on_segment(q1, q2, p2))
        || (d3 == 0 && on_segment(p1, p2, q1))
        || (d4 == 0 && on_segment(p1, p2, q2))
}

/// Whether p is inside rings that are filled even-odd (outer rings and holes).
fn inside_rings(rings: &[Vec<Pt>], p: Pt) -> bool {
    let mut inside = false;
    for ring in rings {
        for i in 0..ring.len() {
            if crosses_ray(ring[i], ring[(i + 1) % ring.len()], p) {
                inside = !inside;
            }
        }
    }
    inside
}

/// The edge a to b crosses the ray from p towards +x.
fn crosses_ray(a: Pt, b: Pt, p: Pt) -> bool {
    let (lo, hi) = if a.1 <= b.1 { (a, b) } else { (b, a) };
    lo.1 <= p.1 && p.1 < hi.1 && orient(lo, hi, p) > 0
}

/// A filter area in world coordinates, its edges bucketed into horizontal rows
/// so that a point or segment is only tested against the edges near it.
struct Area {
    rings: Vec<Vec<Pt>>,
    edges: Vec<(Pt, Pt)>,
    min: Pt,
    max: Pt,
    rows: Vec<Vec<u32>>,
    row_height: i64,
}

impl Area {
    fn from_geojson(g: JsonRef<'_>) -> Area {
        let fail = |msg: &str| -> ! {
            json_message(msg, g);
            std::process::exit(EXIT_FILTER);
        };
        if g.type_() != JsonType::Hash {
            fail("\"intersects\" needs a GeoJSON Polygon or MultiPolygon: ");
        }
        let kind = g
            .hash_get("type")
            .filter(|t| t.type_() == JsonType::String)
            .map(|t| t.string());
        let Some(coords) = g
            .hash_get("coordinates")
            .filter(|c| c.type_() == JsonType::Array)
        else {
            fail("\"intersects\" geometry has no coordinates: ");
        };
        let polygons = match kind {
            Some("Polygon") => vec![coords],
            Some("MultiPolygon") => coords.array(),
            _ => fail("\"intersects\" needs a GeoJSON Polygon or MultiPolygon: "),
        };

        let mut rings = Vec::new();
        for polygon in polygons {
            for ring in polygon.array() {
                let mut lonlat: Vec<(f64, f64)> = Vec::new();
                for c in ring.array() {
                    let xy = c.array();
                    if xy.len() < 2
                        || xy[0].type_() != JsonType::Number
                        || xy[1].type_() != JsonType::Number
                    {
                        fail("\"intersects\" coordinate is not [longitude, latitude]: ");
                    }
                    lonlat.push((xy[0].number(), xy[1].number()));
                }
                // Edges are straight in longitude and latitude, as SQL's ST_Intersects
                // treats them, but would be straight in Web Mercator once projected:
                // a 20 km edge would bend by about a metre. Points every 0.01 degree
                // keep that to millimetres.
                let mut pts: Vec<Pt> = Vec::new();
                for (i, &(lon, lat)) in lonlat.iter().enumerate() {
                    if i > 0 {
                        let (lon0, lat0) = lonlat[i - 1];
                        let steps =
                            ((lon - lon0).abs().max((lat - lat0).abs()) / 0.01).ceil() as usize;
                        for k in 1..steps {
                            let t = k as f64 / steps as f64;
                            pts.push(lonlat2tile(
                                lon0 + (lon - lon0) * t,
                                lat0 + (lat - lat0) * t,
                                32,
                            ));
                        }
                    }
                    pts.push(lonlat2tile(lon, lat, 32));
                }
                if pts.len() > 1 && pts[0] == pts[pts.len() - 1] {
                    pts.pop();
                }
                if pts.len() >= 3 {
                    rings.push(pts);
                }
            }
        }
        if rings.is_empty() {
            fail("\"intersects\" polygon has no ring: ");
        }

        let mut edges = Vec::new();
        let (mut min, mut max) = ((i64::MAX, i64::MAX), (i64::MIN, i64::MIN));
        for ring in &rings {
            for i in 0..ring.len() {
                let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
                edges.push((a, b));
                min = (min.0.min(a.0), min.1.min(a.1));
                max = (max.0.max(a.0), max.1.max(a.1));
            }
        }

        // about four edges to a row
        let nrows = (edges.len() / 4).clamp(1, 4096);
        let row_height = ((max.1 - min.1) / nrows as i64).max(1);
        let mut rows = vec![Vec::new(); nrows];
        for (i, (a, b)) in edges.iter().enumerate() {
            let (r0, r1) = (
                Self::row(min, row_height, nrows, a.1.min(b.1)),
                Self::row(min, row_height, nrows, a.1.max(b.1)),
            );
            for row in &mut rows[r0..=r1] {
                row.push(i as u32);
            }
        }
        Area {
            rings,
            edges,
            min,
            max,
            rows,
            row_height,
        }
    }

    fn row(min: Pt, row_height: i64, nrows: usize, y: i64) -> usize {
        (((y - min.1) / row_height).max(0) as usize).min(nrows - 1)
    }

    fn contains(&self, p: Pt) -> bool {
        if p.0 < self.min.0 || p.0 > self.max.0 || p.1 < self.min.1 || p.1 > self.max.1 {
            return false;
        }
        let r = Self::row(self.min, self.row_height, self.rows.len(), p.1);
        let mut inside = false;
        for &e in &self.rows[r] {
            let (a, b) = self.edges[e as usize];
            if crosses_ray(a, b, p) {
                inside = !inside;
            }
        }
        inside
    }

    /// The segment touches the area: an end inside, or crossing an edge.
    fn touches_segment(&self, p: Pt, q: Pt) -> bool {
        if p.0.max(q.0) < self.min.0
            || p.0.min(q.0) > self.max.0
            || p.1.max(q.1) < self.min.1
            || p.1.min(q.1) > self.max.1
        {
            return false;
        }
        if self.contains(p) || self.contains(q) {
            return true;
        }
        let nrows = self.rows.len();
        let (r0, r1) = (
            Self::row(self.min, self.row_height, nrows, p.1.min(q.1)),
            Self::row(self.min, self.row_height, nrows, p.1.max(q.1)),
        );
        self.rows[r0..=r1].iter().flatten().any(|&e| {
            let (a, b) = self.edges[e as usize];
            segments_intersect(p, q, a, b)
        })
    }

    fn intersects(&self, g: &FilterGeometry<'_>) -> bool {
        // the parts of the geometry, each a point, a line or a polygon ring
        let mut parts: Vec<Vec<Pt>> = Vec::new();
        for d in g.draws {
            let p = (d.x + g.origin.0, d.y + g.origin.1);
            if d.op == VT_MOVETO {
                parts.push(vec![p]);
            } else if d.op == VT_LINETO {
                match parts.last_mut() {
                    Some(part) => part.push(p),
                    None => parts.push(vec![p]),
                }
            }
        }

        match g.type_ {
            VT_POINT => parts.iter().flatten().any(|&p| self.contains(p)),
            // a single vertex is not a line: clipping to a tile leaves the vertices it cut off as lone moves
            VT_LINE => parts
                .iter()
                .any(|part| part.windows(2).any(|w| self.touches_segment(w[0], w[1]))),
            VT_POLYGON => {
                let crossing = parts.iter().any(|ring| {
                    (0..ring.len())
                        .any(|i| self.touches_segment(ring[i], ring[(i + 1) % ring.len()]))
                });
                // or a part of the area lies wholly inside the polygon. With no
                // edges crossing, each of the area's rings is wholly inside it or
                // wholly outside, so one vertex of each ring tells; only those
                // within the polygon's bounding box can be inside.
                let (mut lo, mut hi) = ((i64::MAX, i64::MAX), (i64::MIN, i64::MIN));
                for &p in parts.iter().flatten() {
                    lo = (lo.0.min(p.0), lo.1.min(p.1));
                    hi = (hi.0.max(p.0), hi.1.max(p.1));
                }
                crossing
                    || self.rings.iter().any(|ring| {
                        let p = ring[0];
                        lo.0 <= p.0
                            && p.0 <= hi.0
                            && lo.1 <= p.1
                            && p.1 <= hi.1
                            && inside_rings(&parts, p)
                    })
            }
            _ => false,
        }
    }
}

thread_local! {
    /// Each "intersects" area, projected once, by its node in the filter tree.
    static AREAS: RefCell<HashMap<(usize, usize), Rc<Area>>> = RefCell::new(HashMap::new());
}

fn area_of(g: JsonRef<'_>) -> Rc<Area> {
    let key = (g.tree() as *const _ as usize, g.id());
    AREAS.with(|areas| {
        areas
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| Rc::new(Area::from_geojson(g)))
            .clone()
    })
}

fn evaluate_getter(
    feature: &dyn Fn(&str) -> MvtValue,
    layer: &str,
    filter: JsonRef<'_>,
    exclude_attributes: &mut BTreeSet<String>,
    unidecode_data: &[String],
    geom: Option<&FilterGeometry<'_>>,
) -> bool {
    if filter.type_() != JsonType::Hash {
        json_message("Error: filter is not a hash: ", filter);
        std::process::exit(EXIT_JSON);
    }

    let mut ok = true;

    // the C++ looks the layer up by its C string
    let f = filter.hash_get(layer.split('\0').next().unwrap_or(""));
    if ok && let Some(f) = f {
        ok = eval(feature, f, exclude_attributes, unidecode_data, geom) > 0;
    }

    let f = filter.hash_get("*");
    if ok && let Some(f) = f {
        ok = eval(feature, f, exclude_attributes, unidecode_data, geom) > 0;
    }

    ok
}

pub fn read_filter(fname: &str) -> JsonTree {
    let fp = match std::fs::File::open(fname) {
        Ok(fp) => fp,
        Err(_) => {
            crate::cstd::perror(fname);
            std::process::exit(EXIT_OPEN);
        }
    };

    let mut jp = JsonPull::begin_file(Box::new(std::io::BufReader::new(fp)));
    let Some(filter) = jp.read_tree() else {
        eprintln!("{}: {}", fname, jp.error.unwrap_or("(null)"));
        std::process::exit(EXIT_JSON);
    };
    filter
}

pub fn parse_filter(s: &str) -> JsonTree {
    let mut jp = JsonPull::begin_string(s.as_bytes());
    let Some(filter) = jp.read_tree() else {
        eprintln!("Could not parse filter {}", s);
        eprintln!("{}", jp.error.unwrap_or("(null)"));
        std::process::exit(EXIT_JSON);
    };
    filter
}

/// The value a getter returns for an attribute that isn't there.
fn no_such_key() -> MvtValue {
    let mut v = MvtValue::default();
    v.type_ = MVT_NO_SUCH_KEY;
    v.numeric_value.set_null_value(0);
    v
}

/// `evaluate` over a map of attribute values.
pub fn evaluate(
    feature: &HashMap<String, MvtValue>,
    layer: &str,
    filter: JsonRef<'_>,
    exclude_attributes: &mut BTreeSet<String>,
    unidecode_data: &[String],
) -> bool {
    let getter = |key: &str| -> MvtValue {
        match feature.get(key) {
            Some(f) => f.clone(),
            None => no_such_key(),
        }
    };

    evaluate_getter(
        &getter,
        layer,
        filter,
        exclude_attributes,
        unidecode_data,
        None,
    )
}

/// `evaluate` over a feature of a decoded tile.
pub fn evaluate_feature(
    feat: &MvtFeature,
    layer: &MvtLayer,
    filter: JsonRef<'_>,
    exclude_attributes: &mut BTreeSet<String>,
    z: i32,
    unidecode_data: &[String],
) -> bool {
    evaluate_feature_geom(
        feat,
        layer,
        filter,
        exclude_attributes,
        z,
        unidecode_data,
        None,
    )
}

/// `evaluate_feature`, with the feature's geometry for "intersects".
pub fn evaluate_feature_geom(
    feat: &MvtFeature,
    layer: &MvtLayer,
    filter: JsonRef<'_>,
    exclude_attributes: &mut BTreeSet<String>,
    z: i32,
    unidecode_data: &[String],
    geom: Option<&FilterGeometry<'_>>,
) -> bool {
    let getter = |key: &str| -> MvtValue {
        if key == "$id" && feat.has_id {
            let mut v = MvtValue::default();
            v.type_ = MVT_UINT;
            v.numeric_value.set_uint_value(feat.id);
            return v;
        }

        if key == "$type" {
            let mut v = MvtValue::default();
            v.type_ = MVT_STRING;

            if feat.type_ == MVT_POINT {
                v.set_string_value("Point");
            } else if feat.type_ == MVT_LINESTRING {
                v.set_string_value("LineString");
            } else if feat.type_ == MVT_POLYGON {
                v.set_string_value("Polygon");
            }
            return v;
        }

        if key == "$zoom" {
            let mut v2 = MvtValue::default();
            v2.type_ = MVT_UINT;
            v2.numeric_value.set_uint_value(z as u64);
            return v2;
        }

        let mut i = 0;
        while i + 1 < feat.tags.len() {
            if layer.keys[feat.tags[i] as usize] == key {
                return layer.values[feat.tags[i + 1] as usize].clone();
            }
            i += 2;
        }

        no_such_key()
    };

    evaluate_getter(
        &getter,
        &layer.name,
        filter,
        exclude_attributes,
        unidecode_data,
        geom,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::stringified_to_mvt_value;

    /// Results from the C++ for a feature with n=5, s="abc" and b=false in
    /// layer "layer", and the attributes each filter excluded.
    #[test]
    fn filters_match_the_cpp() {
        let mut attrs = HashMap::new();
        attrs.insert("n".to_string(), stringified_to_mvt_value(MVT_DOUBLE, "5"));
        attrs.insert("s".to_string(), stringified_to_mvt_value(MVT_STRING, "abc"));
        attrs.insert("b".to_string(), stringified_to_mvt_value(MVT_BOOL, "false"));

        let cases: &[(&str, bool, &[&str])] = &[
            (r#"{"layer": ["==", "n", 5]}"#, true, &[]),
            (r#"{"*": ["<", "s", "abd"]}"#, true, &[]),
            (r#"{"layer": [">", "b", true]}"#, false, &[]),
            (r#"{"layer": [">", "b", false]}"#, false, &[]),
            // false compares equal to true, as in the C++
            (r#"{"layer": ["==", "b", true]}"#, true, &[]),
            (r#"{"layer": ["in", "n", 1, 2, 5]}"#, true, &[]),
            (r#"{"layer": ["!in", "n", "x", 5]}"#, false, &[]),
            (r#"{"layer": ["has", "n"]}"#, true, &[]),
            (r#"{"layer": ["!has", "zz"]}"#, true, &[]),
            (
                r#"{"layer": ["any", ["==", "n", 4], ["==", "s", "abc"]]}"#,
                true,
                &[],
            ),
            (r#"{"layer": ["none", ["==", "n", 4]]}"#, true, &[]),
            (
                r#"{"layer": ["all", ["attribute-filter", "s", ["==", "n", 4]], true]}"#,
                true,
                &["s"],
            ),
            (r#"{"layer": 0}"#, false, &[]),
            (r#"{"layer": "x"}"#, true, &[]),
            (r#"{"layer": null}"#, true, &[]),
            (r#"{"other": false}"#, true, &[]),
        ];

        for &(filter, expected, excluded) in cases {
            let tree = parse_filter(filter);
            let mut exclude = BTreeSet::new();
            assert_eq!(
                evaluate(&attrs, "layer", tree.root(), &mut exclude, &[]),
                expected,
                "{filter}"
            );
            let excluded: BTreeSet<String> = excluded.iter().map(|s| s.to_string()).collect();
            assert_eq!(exclude, excluded, "{filter}");
        }
    }

    #[test]
    fn like_matches_sql() {
        assert!(like("Lidl Berlin", "Lidl%"));
        assert!(like("Lidl Berlin", "%Berlin"));
        assert!(like("Lidl Berlin", "%dl B%"));
        assert!(like("abc", "a_c"));
        assert!(!like("abc", "a_"));
        assert!(like("", "%"));
        assert!(!like("Lidl", "lidl"));
        assert!(like("aXbXc", "%X%c"));
        assert!(!like("aXbXd", "%X%c"));
        assert!(like("Straße", "%ß%"));
    }

    // % and _ are wildcards only in the pattern; in the text they are plain
    // characters. Expected values are DuckDB's LIKE without ESCAPE, which also
    // takes a backslash as a plain character.
    #[test]
    fn like_text_with_wildcard_characters_matches_sql() {
        assert!(like("a%b", "a%"));
        assert!(like("x%y%z", "x%z"));
        assert!(like("x%y%z", "%y%"));
        assert!(like("50%x", "50%"));
        assert!(like(r"50%[x](y)\z", "50%"));
        assert!(like("Rate 50%", "%50%"));
        assert!(like("a_b", "a_%"));
        assert!(like(r"50%[x](y)\z", r"%\z"));
        assert!(like(r"50%[x](y)\z", "%[x](y)%"));
        assert!(like(r"a\b", r"a\%"));
        assert!(!like("a%", r"a\%"));
        assert!(like(r"a\bc", r"a\_c"));
        assert!(!like("abc", r"a\_c"));
        assert!(!like("a_c", r"a\_c"));
        assert!(like("[", "["));
        assert!(like("(", "("));
        assert!(!like("a", "."));
        assert!(like("a", "_"));
        assert!(like("%", "%%"));
        assert!(!like("x%y", "x%z"));
        assert!(like(&"Ärger".to_lowercase(), &"ä%".to_lowercase()));
    }

    #[test]
    fn like_filters() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".to_string(),
            stringified_to_mvt_value(MVT_STRING, "Lidl Berlin"),
        );
        attrs.insert("n".to_string(), stringified_to_mvt_value(MVT_DOUBLE, "105"));
        attrs.insert(
            "rate".to_string(),
            stringified_to_mvt_value(MVT_STRING, "A%b"),
        );
        let cases: &[(&str, bool)] = &[
            (r#"{"*": ["ilike", "rate", "a%"]}"#, true),
            (r#"{"*": ["!ilike", "rate", "a%"]}"#, false),
            (r#"{"*": ["like", "rate", "a%"]}"#, false),
            (r#"{"*": ["like", "name", "Lidl%"]}"#, true),
            (r#"{"*": ["like", "name", "lidl%"]}"#, false),
            (r#"{"*": ["ilike", "name", "lidl%"]}"#, true),
            (r#"{"*": ["!ilike", "name", "%aldi%"]}"#, true),
            (r#"{"*": ["!like", "name", "%Berlin"]}"#, false),
            (r#"{"*": ["ilike", "n", "10%"]}"#, true),
            (r#"{"*": ["ilike", "missing", "%"]}"#, false),
            (r#"{"*": ["!ilike", "missing", "%"]}"#, true),
            (
                r#"{"*": ["all", ["ilike", "name", "%berlin"], ["==", "n", 105]]}"#,
                true,
            ),
        ];
        for &(filter, expected) in cases {
            let tree = parse_filter(filter);
            let mut exclude = BTreeSet::new();
            assert_eq!(
                evaluate(&attrs, "l", tree.root(), &mut exclude, &[]),
                expected,
                "{filter}"
            );
        }
    }

    #[test]
    fn intersects_points_lines_and_polygons() {
        // a 1 x 1 degree square
        let tree = parse_filter(
            r#"{"*": ["intersects", {"type": "Polygon", "coordinates": [[[10, 10], [11, 10], [11, 11], [10, 11], [10, 10]]]}]}"#,
        );
        let at = |lon: f64, lat: f64| lonlat2tile(lon, lat, 32);
        let draw = |op: i8, p: (i64, i64), origin: (i64, i64)| Draw {
            x: p.0 - origin.0,
            op,
            y: p.1 - origin.1,
            necessary: 0,
        };
        let check = |type_: i32, parts: &[&[(f64, f64)]], origin: (i64, i64)| {
            let mut draws = Vec::new();
            for part in parts {
                for (i, &(lon, lat)) in part.iter().enumerate() {
                    draws.push(draw(
                        if i == 0 { VT_MOVETO } else { VT_LINETO },
                        at(lon, lat),
                        origin,
                    ));
                }
            }
            let geom = FilterGeometry {
                draws: &draws,
                origin,
                type_,
            };
            let getter = |_: &str| no_such_key();
            let mut exclude = BTreeSet::new();
            evaluate_getter(&getter, "l", tree.root(), &mut exclude, &[], Some(&geom))
        };
        let o = (0, 0);
        let shifted = (1 << 31, 1 << 30); // the same, relative to a tile's origin
        assert!(check(VT_POINT, &[&[(10.5, 10.5)]], o));
        assert!(check(VT_POINT, &[&[(10.5, 10.5)]], shifted));
        assert!(!check(VT_POINT, &[&[(12.0, 12.0)]], o));
        assert!(check(VT_POINT, &[&[(12.0, 12.0)], &[(10.2, 10.8)]], o)); // multipoint, one inside
        // a line through the square with no vertex inside it
        assert!(check(VT_LINE, &[&[(9.0, 10.5), (12.0, 10.5)]], o));
        assert!(!check(VT_LINE, &[&[(9.0, 12.0), (12.0, 12.5)]], o));
        // clipping a line to a tile leaves the vertices it cut off as lone moves:
        // they are no longer part of the line, even inside the square
        assert!(!check(
            VT_LINE,
            &[&[(10.5, 10.5)], &[(9.0, 12.0), (12.0, 12.5)]],
            o
        ));
        // a polygon around the whole square, one beside it, one overlapping a corner
        assert!(check(
            VT_POLYGON,
            &[&[
                (0.0, 0.0),
                (20.0, 0.0),
                (20.0, 20.0),
                (0.0, 20.0),
                (0.0, 0.0)
            ]],
            o
        ));
        assert!(!check(
            VT_POLYGON,
            &[&[
                (12.0, 12.0),
                (13.0, 12.0),
                (13.0, 13.0),
                (12.0, 13.0),
                (12.0, 12.0)
            ]],
            o
        ));
        assert!(check(
            VT_POLYGON,
            &[&[
                (10.8, 10.8),
                (12.0, 10.8),
                (12.0, 12.0),
                (10.8, 12.0),
                (10.8, 10.8)
            ]],
            shifted
        ));
    }

    #[test]
    fn intersects_follows_straight_edges_in_longitude_and_latitude() {
        // the long sloped edge from (-71.10, 42.42) to (-71.19, 42.23): a point
        // halfway along it in degrees, nudged about 1.6 cm east and west
        let tree = parse_filter(
            r#"{"*": ["intersects", {"type": "Polygon", "coordinates": [[[-71.19, 42.23], [-70.99, 42.23], [-70.92, 42.40], [-71.10, 42.42], [-71.19, 42.23]]]}]}"#,
        );
        let check = |lon: f64, lat: f64| {
            let p = lonlat2tile(lon, lat, 32);
            let draws = [Draw::new(VT_MOVETO, p.0, p.1)];
            let geom = FilterGeometry {
                draws: &draws,
                origin: (0, 0),
                type_: VT_POINT,
            };
            let getter = |_: &str| no_such_key();
            evaluate_getter(
                &getter,
                "l",
                tree.root(),
                &mut BTreeSet::new(),
                &[],
                Some(&geom),
            )
        };
        assert!(check(-71.145 + 0.0000002, 42.325));
        assert!(!check(-71.145 - 0.0000002, 42.325));
    }

    #[test]
    fn intersects_respects_holes_and_multipolygons() {
        let tree = parse_filter(
            r#"{"*": ["intersects", {"type": "MultiPolygon", "coordinates": [
                [[[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]], [[1, 1], [3, 1], [3, 3], [1, 3], [1, 1]]],
                [[[10, 10], [11, 10], [11, 11], [10, 11], [10, 10]]]]}]}"#,
        );
        let check = |lon: f64, lat: f64| {
            let p = lonlat2tile(lon, lat, 32);
            let draws = [Draw {
                x: p.0,
                op: VT_MOVETO,
                y: p.1,
                necessary: 0,
            }];
            let geom = FilterGeometry {
                draws: &draws,
                origin: (0, 0),
                type_: VT_POINT,
            };
            let getter = |_: &str| no_such_key();
            evaluate_getter(
                &getter,
                "l",
                tree.root(),
                &mut BTreeSet::new(),
                &[],
                Some(&geom),
            )
        };
        assert!(check(0.5, 0.5)); // in the ring
        assert!(!check(2.0, 2.0)); // in the hole
        assert!(check(10.5, 10.5)); // in the second polygon
        assert!(!check(5.0, 5.0));
    }

    #[test]
    fn intersects_a_polygon_around_any_part_of_a_multipolygon() {
        // the same two parts: a square with a hole, and a small square apart
        let tree = parse_filter(
            r#"{"*": ["intersects", {"type": "MultiPolygon", "coordinates": [
                [[[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]], [[1, 1], [3, 1], [3, 3], [1, 3], [1, 1]]],
                [[[10, 10], [11, 10], [11, 11], [10, 11], [10, 10]]]]}]}"#,
        );
        // a square polygon from (lo, lo) to (hi, hi), its edges touching no part
        let check = |lo: f64, hi: f64| {
            let corners = [(lo, lo), (hi, lo), (hi, hi), (lo, hi), (lo, lo)];
            let draws: Vec<Draw> = corners
                .iter()
                .enumerate()
                .map(|(i, &(lon, lat))| {
                    let p = lonlat2tile(lon, lat, 32);
                    Draw::new(if i == 0 { VT_MOVETO } else { VT_LINETO }, p.0, p.1)
                })
                .collect();
            let geom = FilterGeometry {
                draws: &draws,
                origin: (0, 0),
                type_: VT_POLYGON,
            };
            let getter = |_: &str| no_such_key();
            evaluate_getter(
                &getter,
                "l",
                tree.root(),
                &mut BTreeSet::new(),
                &[],
                Some(&geom),
            )
        };
        assert!(check(9.0, 12.0)); // around the second part only
        assert!(check(-1.0, 5.0)); // around the first part only
        assert!(check(-1.0, 12.0)); // around both
        assert!(!check(1.5, 2.5)); // in the hole of the first part
        assert!(!check(6.0, 8.0)); // between the parts
    }

    #[test]
    fn feature_getters() {
        let mut layer = MvtLayer {
            name: "l".to_string(),
            ..Default::default()
        };
        let mut feat = MvtFeature {
            type_: MVT_LINESTRING,
            id: 1234,
            has_id: true,
            ..Default::default()
        };
        layer.tag(&mut feat, "k", &stringified_to_mvt_value(MVT_STRING, "v"));

        let check = |filter: &str, z: i32| {
            let tree = parse_filter(filter);
            let mut exclude = BTreeSet::new();
            evaluate_feature(&feat, &layer, tree.root(), &mut exclude, z, &[])
        };
        assert!(check(r#"{"l": ["==", "$type", "LineString"]}"#, 0));
        assert!(check(r#"{"l": ["==", "$id", 1234]}"#, 0));
        assert!(check(r#"{"l": [">=", "$zoom", 3]}"#, 3));
        assert!(!check(r#"{"l": [">=", "$zoom", 3]}"#, 2));
        assert!(check(r#"{"*": ["==", "k", "v"]}"#, 0));
    }
}
