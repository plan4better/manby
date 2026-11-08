//! Finding the GeoJSON features in a range of JSON and turning them into chunk
//! records: tippecanoe's geojson-loop.cpp, geojson.cpp and the parts of
//! read_json.cpp they use.

use std::borrow::Cow;

use tippecanoe::errors::{EXIT_JSON, EXIT_UTF8};
use tippecanoe::geometry::{Draw, DrawVec, VT_CLOSEPATH, VT_LINETO, VT_MOVETO, VT_POINT};
use tippecanoe::milo::dtoa_milo;
use tippecanoe::mvt::{MVT_BOOL, MVT_DOUBLE, MVT_NULL, MVT_STRING};
use tippecanoe::options::A_CONVERT_NUMERIC_IDS;
use tippecanoe::read_json::{
    GEOM_POINT, GEOM_POLYGON, GEOM_TYPES, GEOMETRY_NAMES, GEOMETRY_WITHIN, MB_GEOMETRY,
};
use tippecanoe::serial::SerialVal;

use super::chunk::{Chunk, Fatal, Message, MessageKind};
use super::json::{Kind, Parser, Ref};
use super::serialize::{Config, InFeature, serialize_feature};

/// `json_context`: the message tail that shows the JSON a problem was found in.
pub fn json_context(j: Ref<'_, '_>) -> Vec<u8> {
    let mut s = j.stringify();
    if s.len() >= 500 {
        s.truncate(497);
        s.extend_from_slice(b"...");
    }
    // fprintf("%s") stops at a NUL
    if let Some(n) = s.iter().position(|&c| c == 0) {
        s.truncate(n);
    }
    let mut out = b"in JSON object ".to_vec();
    out.extend_from_slice(&s);
    out
}

fn cat(what: &[u8], context: &[u8]) -> Vec<u8> {
    let mut m = what.to_vec();
    m.extend_from_slice(context);
    m
}

fn inside_properties(j: Ref<'_, '_>) -> bool {
    let mut jo = Some(j);
    while let Some(o) = jo {
        if let Some(parent) = o.parent()
            && parent.kind() == Kind::Hash
            && parent.hash_get(b"properties").map(|p| p.id) == Some(o.id)
        {
            return true;
        }
        jo = o.parent();
    }
    false
}

/// What reading a range needs besides the chunk it reads into.
pub struct Reading<'a> {
    pub cfg: &'a Config<'a>,
    /// The source's name for messages.
    pub fname: &'a str,
    /// The source's layer id and name.
    pub layer: i32,
    pub layername: &'a str,
}

/// tippecanoe's `parse_json` with a `json_serialize_action`: reads every
/// feature in `input` into `chunk`. `found_features` is how many features the
/// same parse found before `input`.
pub fn parse(input: &[u8], r: &Reading<'_>, chunk: &mut Chunk, found_features: i64) {
    let mut p = Parser::new(input);
    let mut found_hashes: i64 = 0;
    let mut found_geometries: i64 = 0;
    let mut found_features_here: i64 = 0;

    loop {
        let jid = match p.read_hash() {
            Some(j) => j,
            None => {
                if let Some(error) = p.error {
                    let mut text = format!("{}: ", error).into_bytes();
                    if p.doc.root != super::json::NONE {
                        text.extend_from_slice(&json_context(p.doc.get(p.doc.root)));
                    }
                    chunk.error = Some(Message {
                        kind: MessageKind::Always,
                        parts: vec![(Some(p.line), text)],
                    });
                    chunk.cut_short = p.in_container();
                    chunk.error_at_end = p.at_end();
                }
                p.clear_root();
                break;
            }
        };
        if chunk.fatal.is_some() {
            break;
        }
        let j = p.doc.get(jid);

        found_hashes += 1;
        if found_hashes == 50 && found_features + found_features_here == 0 && found_geometries == 0
        {
            chunk.message(
                MessageKind::Always,
                Some(p.line),
                "Warning: not finding any GeoJSON features or geometries in input yet after 50 objects.",
            );
        }

        let type_ = match j.hash_get(b"type") {
            Some(t) if t.kind() == Kind::String => t,
            _ => continue,
        };

        if found_features + found_features_here == 0 {
            let mut is_geometry = GEOMETRY_NAMES
                .iter()
                .any(|name| type_.bytes() == name.as_bytes());

            if is_geometry && let Some(parent) = j.parent() {
                if parent.kind() == Kind::Array && parent.parent().is_some() {
                    let grandparent = parent.parent().unwrap();
                    if grandparent.kind() == Kind::Hash
                        && grandparent.hash_get(b"geometries").is_some()
                    {
                        // Parent of Parent must be a GeometryCollection
                        is_geometry = false;
                    }
                } else if parent.kind() == Kind::Hash && parent.hash_get(b"geometry").is_some() {
                    // Parent must be a Feature
                    is_geometry = false;
                }
            }

            if is_geometry && inside_properties(j) {
                is_geometry = false;
            }

            if is_geometry {
                found_geometries += 1;
                chunk.bare_before_feature = true;

                add_feature(r, chunk, p.line, j, false, None, None, None, j);
                p.free_last(jid);
                continue;
            }
        }

        if type_.bytes() != b"Feature" {
            if type_.bytes() == b"FeatureCollection" {
                check_crs(r, chunk, j);
                p.free_last(jid);
            }
            continue;
        }

        if found_features + found_features_here == 0 && found_geometries != 0 {
            chunk.message(
                MessageKind::Always,
                Some(p.line),
                "Warning: found a mixture of features and bare geometries",
            );
        }
        found_features_here += 1;

        let geometry = match j.hash_get(b"geometry") {
            Some(g) => g,
            None => {
                let m = cat(b"feature with no geometry: ", &json_context(j));
                chunk.message(MessageKind::Always, Some(p.line), m);
                p.free_last(jid);
                continue;
            }
        };

        let properties = j.hash_get(b"properties");
        if properties.is_none_or(|p| p.kind() != Kind::Hash && p.kind() != Kind::Null) {
            let m = cat(b"feature without properties hash: ", &json_context(j));
            chunk.message(MessageKind::Always, Some(p.line), m);
            p.free_last(jid);
            continue;
        }

        if inside_properties(j) {
            continue;
        }

        let tippecanoe = j.hash_get(b"tippecanoe");
        let id = j.hash_get(b"id");

        match geometry.hash_get(b"geometries") {
            Some(g) if g.kind() == Kind::Array => {
                add_feature(r, chunk, p.line, g, true, properties, id, tippecanoe, j);
            }
            _ => {
                add_feature(
                    r, chunk, p.line, geometry, false, properties, id, tippecanoe, j,
                );
            }
        }

        p.free_last(jid);
    }

    chunk.found_features = found_features_here;
    chunk.found_geometries = found_geometries;
    chunk.found_hashes = found_hashes;
}

fn check_crs(r: &Reading<'_>, chunk: &mut Chunk, j: Ref<'_, '_>) {
    let name = j
        .hash_get(b"crs")
        .and_then(|crs| crs.hash_get(b"properties"))
        .and_then(|p| p.hash_get(b"name"));
    if let Some(name) = name
        && name.kind() == Kind::String
        && name.bytes() != r.cfg.projection_alias.as_bytes()
        && !r.cfg.o.quiet
    {
        let n = String::from_utf8_lossy(name.bytes());
        let n = n.split('\0').next().unwrap_or("");
        chunk.message(
            MessageKind::Always,
            None,
            format!(
                "{}: Warning: GeoJSON specified projection \"{}\", not the expected \"{}\".",
                r.fname, n, r.cfg.projection_alias
            ),
        );
        chunk.message(
            MessageKind::Always,
            None,
            format!(
                "{}: If \"{}\" is not the expected projection, use -s to specify the right one.",
                r.fname, r.cfg.projection_alias
            ),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn add_feature(
    r: &Reading<'_>,
    chunk: &mut Chunk,
    line: i32,
    geometry: Ref<'_, '_>,
    geometrycollection: bool,
    properties: Option<Ref<'_, '_>>,
    id: Option<Ref<'_, '_>>,
    tippecanoe: Option<Ref<'_, '_>>,
    feature: Ref<'_, '_>,
) {
    if geometrycollection {
        for g in geometry.array() {
            if chunk.fatal.is_some() {
                return;
            }
            serialize_geojson_feature(r, chunk, line, g, properties, id, tippecanoe, feature);
        }
    } else {
        serialize_geojson_feature(
            r, chunk, line, geometry, properties, id, tippecanoe, feature,
        );
    }
}

fn utf8(b: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(b)
}

/// What `%s` prints of a string: up to its first NUL.
fn cstr(s: &str) -> &str {
    match s.find('\0') {
        Some(n) => &s[..n],
        None => s,
    }
}

/// `integer_zoom`
fn integer_zoom(where_: &str, text: &str) -> Result<i32, Fatal> {
    let d = tippecanoe::cstd::atof(text);
    if !d.is_finite() || d != d.floor() || !(0.0..=32.0).contains(&d) {
        return Err((
            EXIT_JSON,
            vec![(
                None,
                format!(
                    "{}: Expected integer zoom level in \"tippecanoe\" GeoJSON extension, not {}",
                    where_, text
                )
                .into_bytes(),
            )],
        ));
    }
    Ok(d as i32)
}

/// `stringify_value`: the type and string tippecanoe keeps for an attribute value.
fn stringify_value(
    value: Ref<'_, '_>,
    line: i32,
    feature: Ref<'_, '_>,
) -> Result<SerialVal, Fatal> {
    let sv = match value.kind() {
        Kind::String => {
            let bytes = value.bytes();
            if !bytes.is_ascii() {
                let err = tippecanoe::text::check_utf8_raw(bytes);
                if !err.is_empty() {
                    let mut m = err;
                    m.extend_from_slice(b": ");
                    m.extend_from_slice(&json_context(feature));
                    return Err((EXIT_UTF8, vec![(Some(line), m)]));
                }
            }
            SerialVal {
                type_: MVT_STRING,
                s: utf8(bytes).into_owned(),
            }
        }
        Kind::Number => SerialVal {
            type_: MVT_DOUBLE,
            s: if value.large_unsigned() != 0 {
                value.large_unsigned().to_string()
            } else if value.large_signed() != 0 {
                value.large_signed().to_string()
            } else {
                dtoa_milo(value.number())
            },
        },
        Kind::True => SerialVal {
            type_: MVT_BOOL,
            s: "true".to_string(),
        },
        Kind::False => SerialVal {
            type_: MVT_BOOL,
            s: "false".to_string(),
        },
        Kind::Null => SerialVal {
            type_: MVT_NULL,
            s: "null".to_string(),
        },
        Kind::Hash | Kind::Array => SerialVal {
            type_: MVT_STRING,
            s: utf8(&value.stringify()).into_owned(),
        },
    };
    Ok(sv)
}

/// `parse_coordinates`
#[allow(clippy::too_many_arguments)]
fn parse_coordinates(
    t: i32,
    j: Ref<'_, '_>,
    out: &mut DrawVec,
    mut op: i8,
    r: &Reading<'_>,
    line: i32,
    feature: Ref<'_, '_>,
    chunk: &mut Chunk,
) -> Result<(), Fatal> {
    if j.kind() != Kind::Array {
        let m = cat(
            format!("expected array for geometry type {}: ", t).as_bytes(),
            &json_context(feature),
        );
        chunk.message(MessageKind::Always, Some(line), m);
        return Ok(());
    }

    let within = GEOMETRY_WITHIN[t as usize];
    if within >= 0 {
        for (i, elem) in j.array().enumerate() {
            if within == GEOM_POINT {
                if i == 0 || MB_GEOMETRY[t as usize] == VT_POINT {
                    op = VT_MOVETO;
                } else {
                    op = VT_LINETO;
                }
            }

            parse_coordinates(within, elem, out, op, r, line, feature, chunk)?;
        }
    } else if j.array_len() >= 2
        && j.array_get(0).kind() == Kind::Number
        && j.array_get(1).kind() == Kind::Number
    {
        let lon = j.array_get(0).number();
        let lat = j.array_get(1).number();
        let (x, y) = r.cfg.project.world(lon, lat, 32);

        if j.array_len() > 2 && chunk.first("dimensions") {
            chunk.messages.push(Message {
                kind: MessageKind::Once("dimensions"),
                parts: vec![
                    (
                        Some(line),
                        cat(b"ignoring dimensions beyond two: ", &json_context(j)),
                    ),
                    (
                        Some(line),
                        cat(b"ignoring dimensions beyond two: ", &json_context(feature)),
                    ),
                ],
            });
        }

        out.push(Draw::new(op, x, y));
    } else {
        return Err((
            EXIT_JSON,
            vec![
                (Some(line), cat(b"malformed point: ", &json_context(j))),
                (
                    Some(line),
                    cat(b"malformed point: ", &json_context(feature)),
                ),
            ],
        ));
    }

    if t == GEOM_POLYGON {
        // Note that this is not using the correct meaning of closepath.
        //
        // We are using it here to close an entire Polygon, to distinguish
        // the Polygons within a MultiPolygon from each other.
        //
        // This will be undone in fix_polygon(), which needs to know which
        // rings come from which Polygons so that it can make the winding order
        // of the outer ring be the opposite of the order of the inner rings.

        out.push(Draw::new(VT_CLOSEPATH, 0, 0));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn serialize_geojson_feature(
    r: &Reading<'_>,
    chunk: &mut Chunk,
    line: i32,
    geometry: Ref<'_, '_>,
    properties: Option<Ref<'_, '_>>,
    id: Option<Ref<'_, '_>>,
    tippecanoe: Option<Ref<'_, '_>>,
    feature: Ref<'_, '_>,
) {
    if let Err(e) = serialize_geojson_feature1(
        r, chunk, line, geometry, properties, id, tippecanoe, feature,
    ) {
        chunk.fatal = Some(e);
    }
}

#[allow(clippy::too_many_arguments)]
fn serialize_geojson_feature1(
    r: &Reading<'_>,
    chunk: &mut Chunk,
    line: i32,
    geometry: Ref<'_, '_>,
    properties: Option<Ref<'_, '_>>,
    id: Option<Ref<'_, '_>>,
    tippecanoe: Option<Ref<'_, '_>>,
    feature: Ref<'_, '_>,
) -> Result<(), Fatal> {
    let fname = r.fname;
    let geometry_type = match geometry.hash_get(b"type") {
        Some(t) => t,
        None => {
            if chunk.first("null geometry") {
                let m = cat(
                    b"null geometry (additional not reported): ",
                    &json_context(feature),
                );
                chunk.message(MessageKind::Once("null geometry"), Some(line), m);
            }
            return Ok(());
        }
    };

    if geometry_type.kind() != Kind::String {
        let m = cat(b"geometry type is not a string: ", &json_context(feature));
        chunk.message(MessageKind::Always, Some(line), m);
        return Ok(());
    }

    let coordinates = match geometry.hash_get(b"coordinates") {
        Some(c) if c.kind() == Kind::Array => c,
        _ => {
            let m = cat(
                b"feature without coordinates array: ",
                &json_context(feature),
            );
            chunk.message(MessageKind::Always, Some(line), m);
            return Ok(());
        }
    };

    let t = match GEOMETRY_NAMES
        .iter()
        .position(|n| geometry_type.bytes() == n.as_bytes())
    {
        Some(t) => t,
        None => {
            let name = utf8(geometry_type.bytes());
            let m = cat(
                format!("Can't handle geometry type {}: ", cstr(&name)).as_bytes(),
                &json_context(feature),
            );
            chunk.message(MessageKind::Always, Some(line), m);
            return Ok(());
        }
    };
    debug_assert!(t < GEOM_TYPES);

    let mut tippecanoe_minzoom = -1;
    let mut tippecanoe_maxzoom = -1;
    let mut tippecanoe_layername: Cow<'_, str> = Cow::Borrowed(r.layername);

    if let Some(tippecanoe) = tippecanoe {
        if let Some(min) = tippecanoe.hash_get(b"minzoom")
            && min.kind() == Kind::Number
        {
            tippecanoe_minzoom = integer_zoom(fname, &dtoa_milo(min.number()))?;
        }
        if let Some(max) = tippecanoe.hash_get(b"maxzoom")
            && max.kind() == Kind::Number
        {
            tippecanoe_maxzoom = integer_zoom(fname, &dtoa_milo(max.number()))?;
        }
        if let Some(ln) = tippecanoe.hash_get(b"layer")
            && ln.kind() == Kind::String
        {
            tippecanoe_layername = Cow::Owned(utf8(ln.bytes()).into_owned());
        }
    }

    let mut has_id = false;
    let mut id_value: u64 = 0;
    if let Some(id) = id {
        if id.kind() == Kind::Number {
            if id.number() >= 0.0 {
                let id_number = dtoa_milo(id.number());
                let (v, consumed) = tippecanoe::cstd::strtoull(&id_number, 10);
                id_value = v;

                if id.large_unsigned() != 0 {
                    id_value = id.large_unsigned();
                }

                if consumed < id_number.len() {
                    chunk.message(
                        MessageKind::Once("non-integer id"),
                        None,
                        format!(
                            "Warning: Can't represent non-integer feature ID {}",
                            id_number
                        ),
                    );
                } else if id.large_unsigned() == 0 && id_value.to_string() != id_number {
                    chunk.message(
                        MessageKind::Once("too-large id"),
                        None,
                        format!(
                            "Warning: Can't represent too-large feature ID {}",
                            id_number
                        ),
                    );
                } else {
                    has_id = true;
                }
            } else {
                chunk.message(
                    MessageKind::Once("negative id"),
                    None,
                    format!(
                        "Warning: Can't represent negative feature ID {}",
                        dtoa_milo(id.number())
                    ),
                );
            }
        } else {
            let mut converted = false;

            if r.cfg.o.additional.get(A_CONVERT_NUMERIC_IDS) && id.kind() == Kind::String {
                let s = utf8(id.bytes());
                let (v, consumed) = tippecanoe::cstd::strtoull(&s, 10);
                id_value = v;

                if consumed < cstr(&s).len() {
                    chunk.message(
                        MessageKind::Once("non-integer string id"),
                        None,
                        format!(
                            "Warning: Can't represent non-integer feature ID {}",
                            cstr(&s)
                        ),
                    );
                } else if id_value.to_string() != s {
                    chunk.message(
                        MessageKind::Once("too-large string id"),
                        None,
                        format!("Warning: Can't represent too-large feature ID {}", cstr(&s)),
                    );
                } else {
                    has_id = true;
                    converted = true;
                }
            }

            if !converted {
                let s = id.stringify();
                let s = utf8(&s);
                chunk.message(
                    MessageKind::Once("non-numeric id"),
                    None,
                    format!(
                        "Warning: Can't represent non-numeric feature ID {}",
                        cstr(&s)
                    ),
                );
            }
        }
    }

    let mut full_keys: Vec<Cow<'_, str>> = Vec::new();
    let mut full_values: Vec<SerialVal> = Vec::new();

    if let Some(properties) = properties
        && properties.kind() == Kind::Hash
    {
        for (key, value) in properties.entries() {
            if key.kind() == Kind::String {
                let sv = stringify_value(value, line, feature)?;
                full_keys.push(utf8(key.bytes()));
                full_values.push(sv);
            }
        }
    }

    let mut dv = DrawVec::new();
    parse_coordinates(
        t as i32,
        coordinates,
        &mut dv,
        VT_MOVETO,
        r,
        line,
        feature,
        chunk,
    )?;

    let mut sf = InFeature {
        t: MB_GEOMETRY[t] as i8,
        geometry: dv,
        has_id,
        id: id_value,
        tippecanoe_minzoom,
        tippecanoe_maxzoom,
        full_keys,
        full_values,
    };

    serialize_feature(chunk, r.cfg, &mut sf, &tippecanoe_layername);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::parse_args;

    /// The messages that `input` makes, by kind.
    fn messages(input: &str) -> Vec<MessageKind> {
        let argv: Vec<String> = ["tippecanoe", "-o", "out.pmtiles"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = parse_args(&argv).unwrap();
        let cfg = Config::new(&o);
        let r = Reading {
            cfg: &cfg,
            fname: "in.geojson",
            layer: 0,
            layername: "in",
        };
        let mut chunk = Chunk::new(&["in".to_string()]);
        parse(input.as_bytes(), &r, &mut chunk, 0);
        chunk.messages.iter().map(|m| m.kind).collect()
    }

    /// A once-only message is made once per chunk, not for each vertex or
    /// feature: each stringifies its feature (#9).
    #[test]
    fn once_only_messages_made_once() {
        let coords: Vec<String> = (0..1000)
            .map(|i| format!("[{},{},0]", 10.0 + i as f64 * 1e-5, 50.0))
            .collect();
        let line = format!(
            r#"{{"type":"Feature","properties":{{}},"geometry":{{"type":"LineString","coordinates":[{}]}}}}"#,
            coords.join(",")
        );
        let input = format!("{}\n{}\n", line, line);
        assert_eq!(messages(&input), [MessageKind::Once("dimensions")]);

        let null = r#"{"type":"Feature","properties":{},"geometry":{}}"#;
        let input = format!("{}\n{}\n", null, null);
        assert_eq!(messages(&input), [MessageKind::Once("null geometry")]);
    }
}
