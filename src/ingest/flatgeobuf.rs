//! Features from a FlatGeobuf file, as tippecanoe's flatgeobuf.cpp reads them.
//!
//! tippecanoe queues the features and parses each batch of `CPUS * 500 + 1`
//! in `CPUS` ranges, each numbered from its own offset into the batch. Here
//! each such range is a chunk, so the numbering comes out the same.

use std::borrow::Cow;

use tippecanoe::errors::EXIT_IMPOSSIBLE;
use tippecanoe::flatgeobuf_generated::{
    ColumnType, Feature, Geometry, GeometryType, size_prefixed_root_as_feature,
    size_prefixed_root_as_header,
};
use tippecanoe::geometry::{Draw, DrawVec, VT_CLOSEPATH, VT_LINETO, VT_MOVETO};
use tippecanoe::milo::dtoa_milo;
use tippecanoe::mvt::{MVT_BOOL, MVT_DOUBLE, MVT_STRING};
use tippecanoe::serial::SerialVal;

use super::chunk::Chunk;
use super::serialize::{Config, InFeature, serialize_feature};
use crate::Error;

const MAGICBYTES: usize = 8;

/// `struct NodeItem`: four doubles and an offset.
const SIZEOF_NODE_ITEM: u64 = 40;

/// What tippecanoe's uncaught C++ exception prints.
fn exception(type_: &str, what: &str) -> Error {
    Error::new(
        EXIT_IMPOSSIBLE,
        format!(
            "terminate called after throwing an instance of '{}'\n  what():  {}",
            type_, what
        ),
    )
}

// copied from https://github.com/flatgeobuf/flatgeobuf/blob/master/src/cpp/packedrtree.cpp#L365
fn packed_rtree_size(num_items: u64, node_size: u16) -> crate::Result<u64> {
    if node_size < 2 {
        return Err(exception(
            "std::invalid_argument",
            "Node size must be at least 2",
        ));
    }
    if num_items == 0 {
        return Err(exception(
            "std::invalid_argument",
            "Number of items must be greater than 0",
        ));
    }
    let node_size_min: u64 = node_size.clamp(2, 65535) as u64;
    // limit so that resulting size in bytes can be represented by uint64_t
    if num_items > 1u64 << 56 {
        return Err(exception(
            "std::overflow_error",
            "Number of items must be less than 2^56",
        ));
    }
    let mut n = num_items;
    let mut num_nodes = n;
    loop {
        n = n.div_ceil(node_size_min);
        num_nodes += n;
        if n == 1 {
            break;
        }
    }
    Ok(num_nodes * SIZEOF_NODE_ITEM)
}

fn read_points(cfg: &Config<'_>, geometry: &Geometry<'_>) -> DrawVec {
    let mut dv = DrawVec::new();
    if let Some(xy) = geometry.xy() {
        let mut i = 0;
        while i + 1 < xy.len() {
            let (x, y) = cfg.project.world(xy.get(i), xy.get(i + 1), 32);
            dv.push(Draw::new(VT_MOVETO, x, y));
            i += 2;
        }
    }
    dv
}

fn read_line_part(cfg: &Config<'_>, geometry: &Geometry<'_>) -> DrawVec {
    let mut dv = DrawVec::new();
    let Some(xy) = geometry.xy() else {
        return dv;
    };
    let ends = geometry.ends();
    let mut current_end: usize = 0;

    let mut i: u32 = 0;
    while (i as usize) + 1 < xy.len() {
        let (x, y) = cfg
            .project
            .world(xy.get(i as usize), xy.get(i as usize + 1), 32);
        let at_end = match &ends {
            Some(ends) => current_end < ends.len() && i == ends.get(current_end).wrapping_mul(2),
            None => false,
        };
        if i == 0 || at_end {
            dv.push(Draw::new(VT_MOVETO, x, y));
            if i > 0 {
                current_end += 1;
            }
        } else {
            dv.push(Draw::new(VT_LINETO, x, y));
        }
        i += 2;
    }
    dv
}

fn unsupported_geometry(t: GeometryType) -> Error {
    Error::new(
        EXIT_IMPOSSIBLE,
        format!("flatgeobuf has unsupported geometry type {}", t.0 as u32),
    )
}

fn read_geometry(
    cfg: &Config<'_>,
    geometry: &Geometry<'_>,
    h_geometry_type: GeometryType,
) -> crate::Result<DrawVec> {
    let mut geometry_type = h_geometry_type;
    if h_geometry_type == GeometryType::Unknown {
        geometry_type = geometry.type_();
    }

    match geometry_type {
        GeometryType::Point | GeometryType::MultiPoint => Ok(read_points(cfg, geometry)),
        GeometryType::LineString | GeometryType::MultiLineString | GeometryType::Polygon => {
            Ok(read_line_part(cfg, geometry))
        }
        GeometryType::MultiPolygon => {
            // if it is a GeometryCollection, parse Parts, ignore XY
            let mut dv = DrawVec::new();
            if let Some(parts) = geometry.parts() {
                for part in 0..parts.len() {
                    dv.extend(read_line_part(cfg, &parts.get(part)));
                    dv.push(Draw::new(VT_CLOSEPATH, 0, 0));
                }
            }
            Ok(dv)
        }
        _ => Err(unsupported_geometry(h_geometry_type)),
    }
}

fn bytes_at<const N: usize>(data: &[u8], at: usize) -> crate::Result<[u8; N]> {
    data.get(at..at + N)
        .map(|b| b.try_into().unwrap())
        .ok_or_else(|| Error::new(EXIT_IMPOSSIBLE, "flatgeobuf properties are cut short"))
}

/// The header of a FlatGeobuf file and where its features start.
pub struct Header<'a> {
    pub geometry_type: GeometryType,
    pub column_names: Vec<String>,
    pub column_types: Vec<ColumnType>,
    /// Feature ids by sequence, for an indexed file.
    pub sequence_ids: bool,
    /// The features, each with its 4-byte size prefix.
    pub features: Vec<&'a [u8]>,
}

/// `buf[start .. start + len]`, cut short at the end of `buf`.
fn clamp(buf: &[u8], start: usize, len: usize) -> &[u8] {
    let start = start.min(buf.len());
    let end = start.saturating_add(len).min(buf.len());
    &buf[start..end]
}

fn prefixed_size(buf: &[u8]) -> u32 {
    let mut b = [0u8; 4];
    for (i, c) in buf.iter().take(4).enumerate() {
        b[i] = *c;
    }
    u32::from_le_bytes(b)
}

/// Reads the header and finds the features, checking each as tippecanoe does.
pub fn read_header<'a>(src: &'a [u8], quiet: bool) -> crate::Result<Header<'a>> {
    let header_size = prefixed_size(clamp(src, MAGICBYTES, 4)) as usize;
    let header = size_prefixed_root_as_header(clamp(src, MAGICBYTES, header_size + 4))
        .map_err(|_| Error::new(EXIT_IMPOSSIBLE, "flatgeobuf header verification failed"))?;

    let mut column_names = Vec::new();
    let mut column_types = Vec::new();
    if let Some(columns) = header.columns() {
        for i in 0..columns.len() {
            let name = columns.get(i).name();
            let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            column_names.push(String::from_utf8_lossy(&name[..end]).into_owned());
            column_types.push(columns.get(i).type_());
        }
    }

    let node_size = header.index_node_size();
    let mut index_size: u64 = 0;
    let sequence_ids = node_size > 0;
    if sequence_ids {
        if !quiet {
            eprintln!("detected indexed FlatGeobuf: assigning feature IDs by sequence");
        }
        index_size = packed_rtree_size(header.features_count(), node_size)?;
    }

    let mut features = Vec::new();
    let mut start = ((MAGICBYTES + 4 + header_size) as u64).saturating_add(index_size) as usize;
    while start < src.len() {
        let feature_size = prefixed_size(&src[start..]) as usize;
        let buf = clamp(src, start, feature_size + 4);
        size_prefixed_root_as_feature(buf).map_err(|_| {
            Error::new(
                EXIT_IMPOSSIBLE,
                "flatgeobuf feature buffer verification failed",
            )
        })?;
        features.push(buf);
        start += 4 + feature_size;
    }

    Ok(Header {
        geometry_type: header.geometry_type(),
        column_names,
        column_types,
        sequence_ids,
        features,
    })
}

/// `readFeature`
pub fn read_feature(
    chunk: &mut Chunk,
    cfg: &Config<'_>,
    h: &Header<'_>,
    buf: &[u8],
    feature_sequence_id: i64,
    layername: &str,
) -> crate::Result<()> {
    // checked by read_header
    let feature: Feature<'_> = size_prefixed_root_as_feature(buf).unwrap();
    let geometry = feature
        .geometry()
        .ok_or_else(|| unsupported_geometry(h.geometry_type))?;
    let dv = read_geometry(cfg, &geometry, h.geometry_type)?;

    let mut geometry_type = h.geometry_type;
    if h.geometry_type == GeometryType::Unknown {
        geometry_type = geometry.type_();
    }
    let drawvec_type: i8 = match geometry_type {
        GeometryType::Point | GeometryType::MultiPoint => 1,
        GeometryType::LineString | GeometryType::MultiLineString => 2,
        GeometryType::Polygon | GeometryType::MultiPolygon => 3,
        _ => return Err(unsupported_geometry(h.geometry_type)),
    };

    let mut full_keys: Vec<Cow<'_, str>> = Vec::new();
    let mut full_values: Vec<SerialVal> = Vec::new();

    // assume tabular schema with columns in header
    if let Some(properties) = feature.properties() {
        let data = properties.bytes();
        let mut p_pos: usize = 0;
        while p_pos < data.len() {
            let col_idx = u16::from_ne_bytes(bytes_at::<2>(data, p_pos)?) as usize;
            let col_type = *h.column_types.get(col_idx).ok_or_else(|| {
                Error::new(EXIT_IMPOSSIBLE, "flatgeobuf property of a missing column")
            })?;
            let at = p_pos + 2;

            let (type_, s, len) = match col_type {
                ColumnType::Byte => (
                    MVT_DOUBLE,
                    i8::from_ne_bytes(bytes_at::<1>(data, at)?).to_string(),
                    1,
                ),
                ColumnType::UByte => (MVT_DOUBLE, bytes_at::<1>(data, at)?[0].to_string(), 1),
                ColumnType::Bool => (
                    MVT_BOOL,
                    if bytes_at::<1>(data, at)?[0] != 0 {
                        "true"
                    } else {
                        "false"
                    }
                    .to_string(),
                    1,
                ),
                ColumnType::Short => (
                    MVT_DOUBLE,
                    i16::from_ne_bytes(bytes_at::<2>(data, at)?).to_string(),
                    2,
                ),
                ColumnType::UShort => (
                    MVT_DOUBLE,
                    u16::from_ne_bytes(bytes_at::<2>(data, at)?).to_string(),
                    2,
                ),
                ColumnType::Int => (
                    MVT_DOUBLE,
                    i32::from_ne_bytes(bytes_at::<4>(data, at)?).to_string(),
                    4,
                ),
                ColumnType::UInt => (
                    MVT_DOUBLE,
                    u32::from_ne_bytes(bytes_at::<4>(data, at)?).to_string(),
                    4,
                ),
                ColumnType::Long => (
                    MVT_DOUBLE,
                    i64::from_ne_bytes(bytes_at::<8>(data, at)?).to_string(),
                    8,
                ),
                ColumnType::ULong => (
                    MVT_DOUBLE,
                    u64::from_ne_bytes(bytes_at::<8>(data, at)?).to_string(),
                    8,
                ),
                ColumnType::Float => (
                    MVT_DOUBLE,
                    dtoa_milo(f32::from_ne_bytes(bytes_at::<4>(data, at)?) as f64),
                    4,
                ),
                ColumnType::Double => (
                    MVT_DOUBLE,
                    dtoa_milo(f64::from_ne_bytes(bytes_at::<8>(data, at)?)),
                    8,
                ),
                ColumnType::String | ColumnType::Json | ColumnType::DateTime => {
                    let val_len = u32::from_ne_bytes(bytes_at::<4>(data, at)?) as usize;
                    let s = data.get(at + 4..at + 4 + val_len).ok_or_else(|| {
                        Error::new(EXIT_IMPOSSIBLE, "flatgeobuf properties are cut short")
                    })?;
                    (
                        MVT_STRING,
                        String::from_utf8_lossy(s).into_owned(),
                        4 + val_len,
                    )
                }
                _ => {
                    // Binary is not representable in MVT
                    return Err(Error::new(
                        EXIT_IMPOSSIBLE,
                        format!(
                            "flatgeobuf has unsupported column type {}",
                            col_type.0 as u32
                        ),
                    ));
                }
            };
            p_pos += 2 + len;
            full_keys.push(Cow::Borrowed(h.column_names[col_idx].as_str()));
            full_values.push(SerialVal { type_, s });
        }
    }

    let mut sf = InFeature {
        t: drawvec_type,
        geometry: dv,
        has_id: feature_sequence_id >= 0,
        id: feature_sequence_id as u64,
        tippecanoe_minzoom: -1,
        tippecanoe_maxzoom: -1,
        full_keys,
        full_values,
    };
    serialize_feature(chunk, cfg, &mut sf, layername);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtree_size() {
        assert_eq!(packed_rtree_size(1, 16).unwrap(), 2 * 40);
        assert_eq!(packed_rtree_size(16, 16).unwrap(), 17 * 40);
        assert_eq!(packed_rtree_size(17, 16).unwrap(), (17 + 2 + 1) * 40);
        assert!(packed_rtree_size(0, 16).is_err());
    }
}
