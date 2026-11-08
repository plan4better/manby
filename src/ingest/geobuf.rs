//! Features from a Geobuf file, as tippecanoe's geobuf.cpp reads them.
//!
//! tippecanoe queues the features and parses each batch of `CPUS * 500 + 1`
//! in `CPUS` ranges, each numbered from its own offset into the batch, and
//! serializes bare geometries at once in its first segment. [`events`] finds
//! those batches and bare geometries in order, and each range or bare
//! geometry becomes a chunk, so the numbering comes out the same.

use std::borrow::Cow;
use std::collections::BTreeMap;

use tippecanoe::errors::{EXIT_IMPOSSIBLE, EXIT_JSON};
use tippecanoe::geometry::{Draw, DrawVec, VT_CLOSEPATH, VT_LINETO, VT_MOVETO};
use tippecanoe::jsonpull::{JsonPull, JsonType};
use tippecanoe::milo::dtoa_milo;
use tippecanoe::mvt::{MVT_BOOL, MVT_DOUBLE, MVT_NULL, MVT_STRING};
use tippecanoe::protozero::{Exception, PbfReader};
use tippecanoe::serial::SerialVal;

use super::chunk::{Chunk, MessageKind};
use super::serialize::{Config, InFeature, serialize_feature};
use crate::Error;

const POINT: i32 = 0;
const MULTIPOINT: i32 = 1;
const LINESTRING: i32 = 2;
const MULTILINESTRING: i32 = 3;
const POLYGON: i32 = 4;
const MULTIPOLYGON: i32 = 5;

/// What tippecanoe's uncaught protozero exception prints.
fn pbf<T>(r: tippecanoe::protozero::Result<T>) -> crate::Result<T> {
    r.map_err(|e| {
        let name = match e {
            Exception::VarintTooLong => "protozero::varint_too_long_exception",
            Exception::UnknownPbfWireType => "protozero::unknown_pbf_wire_type_exception",
            Exception::EndOfBuffer => "protozero::end_of_buffer_exception",
            Exception::InvalidTag => "protozero::invalid_tag_exception",
            Exception::InvalidLength => "protozero::invalid_length_exception",
        };
        Error::new(
            EXIT_IMPOSSIBLE,
            format!(
                "terminate called after throwing an instance of '{}'\n  what():  {}",
                name,
                e.what()
            ),
        )
    })
}

fn impossible(msg: String) -> Error {
    Error::new(EXIT_IMPOSSIBLE, msg)
}

fn ensure_dim(dim: usize) -> crate::Result<()> {
    if dim < 2 {
        return Err(impossible(format!(
            "Geometry has fewer than 2 dimensions: {}",
            dim
        )));
    }
    Ok(())
}

fn read_value(p: &mut PbfReader<'_>) -> crate::Result<SerialVal> {
    let mut sv = SerialVal {
        type_: MVT_NULL,
        s: "null".to_string(),
    };

    while pbf(p.next())? {
        match p.tag() {
            1 => {
                sv.type_ = MVT_STRING;
                sv.s = pbf(p.get_string())?;
            }
            2 => {
                sv.type_ = MVT_DOUBLE;
                sv.s = dtoa_milo(pbf(p.get_double())?);
            }
            3 => {
                sv.type_ = MVT_DOUBLE;
                sv.s = pbf(p.get_uint64())?.to_string();
            }
            4 => {
                sv.type_ = MVT_DOUBLE;
                sv.s = (pbf(p.get_uint64())? as i64).wrapping_neg().to_string();
            }
            5 => {
                sv.type_ = MVT_BOOL;
                sv.s = if pbf(p.get_bool())? { "true" } else { "false" }.to_string();
            }
            6 => {
                sv.type_ = MVT_STRING; // stringified JSON
                sv.s = pbf(p.get_string())?;
                if sv.s == "null" {
                    sv.type_ = MVT_NULL;
                }
            }
            _ => pbf(p.skip())?,
        }
    }

    Ok(sv)
}

fn read_point(cfg: &Config<'_>, coords: &[i64], dim: usize, e: f64) -> crate::Result<DrawVec> {
    ensure_dim(dim)?;
    if coords.len() < 2 {
        return Err(impossible("Geometry has fewer than 2 coordinates".into()));
    }
    let (x, y) = cfg
        .project
        .world(coords[0] as f64 / e, coords[1] as f64 / e, 32);
    Ok(vec![Draw::new(VT_MOVETO, x, y)])
}

#[allow(clippy::too_many_arguments)]
fn read_line_part(
    cfg: &Config<'_>,
    coords: &[i64],
    dim: usize,
    e: f64,
    start: usize,
    end: usize,
    closed: bool,
) -> crate::Result<DrawVec> {
    ensure_dim(dim)?;

    let mut dv = DrawVec::new();
    let mut prev: Vec<i64> = vec![0; dim];
    let mut p: Vec<f64> = vec![0.0; dim];

    let mut i = start;
    while i + dim - 1 < end {
        if i + dim > coords.len() {
            return Err(impossible(format!(
                "Internal error: line segment {} vs {}",
                i + dim - 1,
                coords.len()
            )));
        }

        for d in 0..dim {
            prev[d] = prev[d].wrapping_add(coords[i + d]);
            p[d] = prev[d] as f64 / e;
        }

        let (x, y) = cfg.project.world(p[0], p[1], 32);
        let op = if i == start { VT_MOVETO } else { VT_LINETO };
        dv.push(Draw::new(op, x, y));
        i += dim;
    }

    if closed && !dv.is_empty() {
        dv.push(Draw::new(VT_LINETO, dv[0].x, dv[0].y));
    }

    Ok(dv)
}

fn read_multi_line(
    cfg: &Config<'_>,
    coords: &[i64],
    lengths: &[i32],
    dim: usize,
    e: f64,
    closed: bool,
) -> crate::Result<DrawVec> {
    if lengths.is_empty() {
        return read_line_part(cfg, coords, dim, e, 0, coords.len(), closed);
    }

    let mut dv = DrawVec::new();
    let mut here: usize = 0;
    for &len in lengths {
        let n = (len as isize as usize).wrapping_mul(dim);
        dv.extend(read_line_part(
            cfg,
            coords,
            dim,
            e,
            here,
            here.wrapping_add(n),
            closed,
        )?);
        here = here.wrapping_add(n);
    }
    Ok(dv)
}

fn read_multi_polygon(
    cfg: &Config<'_>,
    coords: &[i64],
    lengths: &[i32],
    dim: usize,
    e: f64,
) -> crate::Result<DrawVec> {
    ensure_dim(dim)?;

    if lengths.is_empty() {
        return read_line_part(cfg, coords, dim, e, 0, coords.len(), true);
    }

    let length = |n: usize| {
        lengths
            .get(n)
            .map(|&l| l as isize as usize)
            .ok_or_else(|| impossible("Geobuf polygon lengths are cut short".into()))
    };
    let polys = length(0)?;
    let mut n: usize = 1;
    let mut here: usize = 0;
    let mut dv = DrawVec::new();

    for _ in 0..polys {
        let rings = length(n)?;
        n += 1;

        for _ in 0..rings {
            let len = length(n)?.wrapping_mul(dim);
            dv.extend(read_line_part(
                cfg,
                coords,
                dim,
                e,
                here,
                here.wrapping_add(len),
                true,
            )?);
            here = here.wrapping_add(len);
            n += 1;
        }

        dv.push(Draw::new(VT_CLOSEPATH, 0, 0)); // mark that the next ring is outer
    }

    Ok(dv)
}

/// A geometry and its tippecanoe type.
pub struct Typed {
    dv: DrawVec,
    t: i8,
}

pub fn read_geometry(
    cfg: &Config<'_>,
    p: &mut PbfReader<'_>,
    dim: usize,
    e: f64,
) -> crate::Result<Vec<Typed>> {
    let mut ret: Vec<Typed> = Vec::new();
    let mut coords: Vec<i64> = Vec::new();
    let mut lengths: Vec<i32> = Vec::new();
    let mut type_: i32 = -1;

    while pbf(p.next())? {
        match p.tag() {
            1 => type_ = pbf(p.get_enum())?,
            2 => {
                for it in pbf(p.get_packed_uint32())? {
                    lengths.push(pbf(it)? as i32);
                }
            }
            3 => {
                for it in pbf(p.get_packed_sint64())? {
                    coords.push(pbf(it)?);
                }
            }
            4 => {
                let mut g = pbf(p.get_message())?;
                ret.extend(read_geometry(cfg, &mut g, dim, e)?);
            }
            _ => pbf(p.skip())?,
        }
    }

    let dv = match type_ {
        POINT => read_point(cfg, &coords, dim, e)?,
        MULTIPOINT | LINESTRING => read_line_part(cfg, &coords, dim, e, 0, coords.len(), false)?,
        POLYGON => read_multi_line(cfg, &coords, &lengths, dim, e, true)?,
        MULTILINESTRING => read_multi_line(cfg, &coords, &lengths, dim, e, false)?,
        MULTIPOLYGON => read_multi_polygon(cfg, &coords, &lengths, dim, e)?,
        // GeometryCollection
        _ => return Ok(ret),
    };

    ret.push(Typed {
        dv,
        t: (type_ / 2 + 1) as i8,
    });
    Ok(ret)
}

/// `integer_zoom`
fn integer_zoom(where_: &str, text: &str) -> crate::Result<i32> {
    let d = tippecanoe::cstd::atof(text);
    if !d.is_finite() || d != d.floor() || !(0.0..=32.0).contains(&d) {
        return Err(Error::new(
            EXIT_JSON,
            format!(
                "{}: Expected integer zoom level in \"tippecanoe\" GeoJSON extension, not {}",
                where_, text
            ),
        ));
    }
    Ok(d as i32)
}

/// `readFeature`: one feature message, which can hold several geometries.
#[allow(clippy::too_many_arguments)]
pub fn read_feature(
    chunk: &mut Chunk,
    cfg: &Config<'_>,
    fname: &str,
    p: &mut PbfReader<'_>,
    dim: usize,
    e: f64,
    keys: &[String],
    layername: &str,
) -> crate::Result<()> {
    let mut layername: Cow<'_, str> = Cow::Borrowed(layername);
    let mut dv: Vec<Typed> = Vec::new();
    let mut id: i64 = 0;
    let mut has_id = false;
    let mut values: Vec<SerialVal> = Vec::new();
    let mut other: BTreeMap<String, SerialVal> = BTreeMap::new();
    let mut full_keys: Vec<Cow<'_, str>> = Vec::new();
    let mut full_values: Vec<SerialVal> = Vec::new();

    let index = |list: &[usize], i: usize, n: usize, what: &str| -> crate::Result<usize> {
        if list[i] >= n {
            return Err(impossible(format!(
                "Out of bounds {}: {} in {}",
                what, list[i], n
            )));
        }
        Ok(list[i])
    };

    while pbf(p.next())? {
        match p.tag() {
            1 => {
                let mut g = pbf(p.get_message())?;
                dv.extend(read_geometry(cfg, &mut g, dim, e)?);
            }
            11 => {
                chunk.message(
                    MessageKind::Once("geobuf string id"),
                    None,
                    "Non-numeric feature IDs not supported",
                );
                pbf(p.skip())?;
            }
            12 => {
                has_id = true;
                id = pbf(p.get_sint64())?;
                if id < 0 {
                    chunk.message(
                        MessageKind::Once("geobuf id range"),
                        None,
                        format!("Out of range feature id {}", id),
                    );
                    has_id = false;
                }
            }
            13 => {
                let mut v = pbf(p.get_message())?;
                values.push(read_value(&mut v)?);
            }
            14 | 15 => {
                let custom = p.tag() == 15;
                let mut list: Vec<usize> = Vec::new();
                for it in pbf(p.get_packed_uint32())? {
                    list.push(pbf(it)? as usize);
                }

                let mut i = 0;
                while i + 1 < list.len() {
                    let k = index(&list, i, keys.len(), "key")?;
                    let v = index(&list, i + 1, values.len(), "value")?;
                    if custom {
                        other
                            .entry(keys[k].clone())
                            .or_insert_with(|| values[v].clone());
                    } else {
                        full_keys.push(Cow::Borrowed(keys[k].as_str()));
                        full_values.push(values[v].clone());
                    }
                    i += 2;
                }

                values.clear();
            }
            _ => pbf(p.skip())?,
        }
    }

    for d in dv {
        let mut sf = InFeature {
            t: d.t,
            geometry: d.dv,
            has_id,
            id: id as u64,
            tippecanoe_minzoom: -1,
            tippecanoe_maxzoom: -1,
            full_keys: full_keys.clone(),
            full_values: full_values.clone(),
        };

        if let Some(tip) = other.get("tippecanoe") {
            let s = tip.s.as_bytes();
            let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
            let mut jp = JsonPull::begin_string(&s[..end]);
            if let Some(o) = jp.read_tree() {
                let o = o.root();

                if let Some(min) = o.hash_get("minzoom")
                    && min.type_() == JsonType::Number
                {
                    sf.tippecanoe_minzoom = integer_zoom(fname, &dtoa_milo(min.number()))?;
                }
                if let Some(max) = o.hash_get("maxzoom")
                    && max.type_() == JsonType::Number
                {
                    sf.tippecanoe_maxzoom = integer_zoom(fname, &dtoa_milo(max.number()))?;
                }
                if let Some(tlayer) = o.hash_get("layer")
                    && tlayer.type_() == JsonType::String
                {
                    layername = Cow::Owned(tlayer.string().to_string());
                }
            }
        }

        serialize_feature(chunk, cfg, &mut sf, &layername);
    }
    Ok(())
}

/// `outBareGeometry` for each geometry of a top-level geometry message.
pub fn read_bare(
    chunk: &mut Chunk,
    cfg: &Config<'_>,
    p: &mut PbfReader<'_>,
    dim: usize,
    e: f64,
    layername: &str,
) -> crate::Result<()> {
    for d in read_geometry(cfg, p, dim, e)? {
        let mut sf = InFeature {
            t: d.t,
            geometry: d.dv,
            has_id: false,
            id: 0,
            tippecanoe_minzoom: -1,
            tippecanoe_maxzoom: -1,
            full_keys: Vec::new(),
            full_values: Vec::new(),
        };
        serialize_feature(chunk, cfg, &mut sf, layername);
    }
    Ok(())
}

/// A feature waiting in tippecanoe's queue.
#[derive(Clone, Copy)]
pub struct Queued<'a> {
    pub p: PbfReader<'a>,
    pub dim: usize,
    pub e: f64,
}

/// What tippecanoe does with a Geobuf file, in order.
pub enum Event<'a> {
    /// A top-level geometry, serialized at once in the first segment.
    Bare(Queued<'a>),
    /// A run of the queue, with the number of keys read by then.
    Batch(Vec<Queued<'a>>, usize),
}

/// Reads the top level of a Geobuf file into tippecanoe's order of events.
/// Also returns all the keys, of which each batch sees as many as had been
/// read when it ran.
pub fn events(src: &[u8], cpus: usize) -> crate::Result<(Vec<Event<'_>>, Vec<String>)> {
    fn queue_feature<'a>(
        queue: &mut Vec<Queued<'a>>,
        keys: usize,
        out: &mut Vec<Event<'a>>,
        cpus: usize,
    ) {
        if queue.len() > cpus * 500 {
            out.push(Event::Batch(std::mem::take(queue), keys));
        }
    }

    let mut keys: Vec<String> = Vec::new();
    let mut p = PbfReader::new(src);
    let mut out = Vec::new();
    let mut queue: Vec<Queued<'_>> = Vec::new();
    let mut dim: usize = 2;
    let mut e: f64 = 1e6;

    while pbf(p.next())? {
        match p.tag() {
            1 => keys.push(pbf(p.get_string())?),
            2 => dim = pbf(p.get_int64())? as usize,
            3 => e = 10f64.powf(pbf(p.get_int64())? as f64),
            4 => {
                let mut fc = pbf(p.get_message())?;
                while pbf(fc.next())? {
                    if fc.tag() == 1 {
                        let f = pbf(fc.get_message())?;
                        queue.push(Queued { p: f, dim, e });
                        queue_feature(&mut queue, keys.len(), &mut out, cpus);
                    } else {
                        pbf(fc.skip())?;
                    }
                }
            }
            5 => {
                let f = pbf(p.get_message())?;
                queue.push(Queued { p: f, dim, e });
                queue_feature(&mut queue, keys.len(), &mut out, cpus);
            }
            6 => {
                let g = pbf(p.get_message())?;
                out.push(Event::Bare(Queued { p: g, dim, e }));
            }
            _ => pbf(p.skip())?,
        }
    }
    if !queue.is_empty() {
        out.push(Event::Batch(queue, keys.len()));
    }
    Ok((out, keys))
}
