//! Features from Arrow tables: GeoParquet files, and Arrow IPC files and
//! streams, such as DuckDB, GeoPandas, Polars and PyArrow write.
//!
//! tippecanoe does not read these. A table reads as tippecanoe reads the
//! FlatGeobuf file that GDAL writes from it without a spatial index
//! (`ogr2ogr -lco SPATIAL_INDEX=NO`): the same features in the same order,
//! numbered in the same batches, with the same attributes, so that the tiles
//! are the same.
//!
//! - The geometry is the column that the GeoParquet metadata names, or the
//!   first with a GeoArrow type, or a binary column named `geometry` or `geom`
//!   in well-known binary. Rows without a geometry are skipped.
//! - Struct columns become an attribute for each field, `name.field`, and
//!   lists and maps become JSON text, as GDAL makes them. Dates and times
//!   become ISO 8601 text, in UTC with a `Z` where the time has a zone.
//! - Coordinates are longitude and latitude, or Web Mercator where the CRS
//!   says so. A table in another CRS is refused.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::ops::Range;

use arrow_array::cast::AsArray;
use arrow_array::types::*;
use arrow_array::{Array, ArrayRef, Float64Array, RecordBatch};
use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef, TimeUnit};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use tippecanoe::errors::{EXIT_IMPOSSIBLE, EXIT_READ};
use tippecanoe::geometry::{Draw, DrawVec, VT_CLOSEPATH, VT_LINETO, VT_MOVETO};
use tippecanoe::jsonpull::{JsonPull, JsonRef, JsonType};
use tippecanoe::milo::dtoa_milo;
use tippecanoe::mvt::{MVT_BOOL, MVT_DOUBLE, MVT_STRING};
use tippecanoe::projection::PROJECTIONS;

use crate::crs::{Project, Reprojection};
use tippecanoe::serial::SerialVal;

use super::chunk::{Chunk, MessageKind};
use super::serialize::{Config, InFeature, serialize_feature};
use crate::Error;

/// The record batches of a table, in order.
pub type Batches = Box<dyn Iterator<Item = Result<RecordBatch, ArrowError>> + Send>;

/// Rows a GeoParquet reader decodes at a time.
const BATCH_ROWS: usize = 8192;

fn read_error(fname: &str, e: impl std::fmt::Display) -> Error {
    Error::new(EXIT_READ, format!("{}: {}", fname, e))
}

/// Whether a file name or `-L` format names a GeoParquet file.
pub fn is_parquet(format: &str, name: &str) -> bool {
    format == "parquet"
        || format == "geoparquet"
        || name.ends_with(".parquet")
        || name.ends_with(".geoparquet")
}

/// Whether a file name or `-L` format names an Arrow IPC file or stream.
pub fn is_arrow(format: &str, name: &str) -> bool {
    format == "arrow"
        || format == "arrows"
        || format == "feather"
        || name.ends_with(".arrow")
        || name.ends_with(".arrows")
        || name.ends_with(".feather")
        || name.ends_with(".ipc")
}

/// The file name without the extension of an Arrow or GeoParquet file, which
/// tippecanoe's layer naming does not know to take off.
pub fn without_extension(name: &str) -> &str {
    for ext in [
        ".geoparquet",
        ".parquet",
        ".arrows",
        ".arrow",
        ".feather",
        ".ipc",
    ] {
        if let Some(stem) = name.strip_suffix(ext)
            && !stem.is_empty()
            && !stem.ends_with('/')
        {
            return stem;
        }
    }
    name
}

/// The row groups a reader kept, of how many, where `--bbox` picked them.
pub type Kept = Option<(usize, usize)>;

/// A GeoParquet file's batches and schema, with only the columns that the
/// features use, and with `bbox` only the row groups that may overlap it.
pub fn open_parquet(
    file: File,
    fname: &str,
    cfg: &Config<'_>,
    bbox: Option<&WorldBox>,
) -> crate::Result<(SchemaRef, Batches, Kept)> {
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| read_error(fname, e))?;
    let full = Table::new(builder.schema(), fname, cfg)?;
    let mask = ProjectionMask::roots(builder.parquet_schema(), full.columns.clone());
    let mut kept = None;
    let mut builder = builder.with_projection(mask).with_batch_size(BATCH_ROWS);
    if let Some(b) = bbox {
        let groups = row_groups(builder.metadata(), builder.schema(), &full, b);
        kept = Some((groups.len(), builder.metadata().num_row_groups()));
        builder = builder.with_row_groups(groups);
    }
    let reader = builder.build().map_err(|e| read_error(fname, e))?;
    let schema = arrow_array::RecordBatchReader::schema(&reader);
    Ok((schema, Box::new(reader), kept))
}

/// `--bbox` in tippecanoe's world coordinates at zoom 32, where y grows to
/// the south, as the minimum and maximum x and y.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldBox {
    pub x0: i64,
    pub y0: i64,
    pub x1: i64,
    pub y1: i64,
}

impl WorldBox {
    /// The box of longitudes and latitudes `[minlon, minlat, maxlon, maxlat]`.
    pub fn new(b: [f64; 4]) -> WorldBox {
        let project = PROJECTIONS[0].project;
        WorldBox::of(project(b[0], b[1], 32), project(b[2], b[3], 32))
    }

    fn of(a: (i64, i64), b: (i64, i64)) -> WorldBox {
        WorldBox {
            x0: a.0.min(b.0),
            y0: a.1.min(b.1),
            x1: a.0.max(b.0),
            y1: a.1.max(b.1),
        }
    }

    fn overlaps(&self, o: &WorldBox) -> bool {
        self.x0 <= o.x1 && o.x0 <= self.x1 && self.y0 <= o.y1 && o.y0 <= self.y1
    }
}

/// The geometry's bounding box in world coordinates, or None for an empty
/// or null geometry. A geometry manby can't read counts as overlapping, so
/// that reading it reports it.
fn geometry_box(table: &Table, g: &dyn Array, row: usize, dv: &mut DrawVec) -> Option<WorldBox> {
    if g.is_null(row) {
        return None;
    }
    dv.clear();
    let read = if table.encoding == Encoding::Wkb {
        let mut w = Wkb {
            b: wkb_bytes(g, row),
            pos: 0,
            project: &table.project,
        };
        w.geometry(dv).is_ok()
    } else {
        geoarrow(g, row, table.encoding, &table.project, dv).is_some()
    };
    if !read {
        return Some(WorldBox {
            x0: i64::MIN,
            y0: i64::MIN,
            x1: i64::MAX,
            y1: i64::MAX,
        });
    }
    let mut points = dv.iter().filter(|d| d.op != VT_CLOSEPATH);
    let first = points.next()?;
    let mut b = WorldBox {
        x0: first.x,
        y0: first.y,
        x1: first.x,
        y1: first.y,
    };
    for d in points {
        b.x0 = b.x0.min(d.x);
        b.y0 = b.y0.min(d.y);
        b.x1 = b.x1.max(d.x);
        b.y1 = b.y1.max(d.y);
    }
    Some(b)
}

/// The rows of a batch whose geometries' bounding boxes overlap `bbox`.
pub fn filter_batch(
    table: &Table,
    batch: &RecordBatch,
    bbox: &WorldBox,
) -> Result<RecordBatch, ArrowError> {
    let g = batch.column(table.geometry).as_ref();
    let mut dv = DrawVec::new();
    let keep: arrow_array::BooleanArray = (0..batch.num_rows())
        .map(|row| Some(geometry_box(table, g, row, &mut dv).is_some_and(|b| b.overlaps(bbox))))
        .collect();
    arrow_select::filter::filter_record_batch(batch, &keep)
}

/// The statistics of a column of a row group as f64: its minimum or maximum.
fn statistic(s: Option<&parquet::file::statistics::Statistics>, max: bool) -> Option<f64> {
    use parquet::file::statistics::Statistics;
    match s? {
        Statistics::Double(v) => if max { v.max_opt() } else { v.min_opt() }.copied(),
        Statistics::Float(v) => if max { v.max_opt() } else { v.min_opt() }.map(|f| *f as f64),
        _ => None,
    }
}

/// A row group's bounding box `[xmin, ymin, xmax, ymax]` in world
/// coordinates. A box in another CRS is curved in world coordinates, so its
/// edges are followed, and it is made a little bigger.
fn stats_box(project: &Project, s: [f64; 4]) -> WorldBox {
    if let Project::Tippecanoe(p) = project {
        return WorldBox::of(p(s[0], s[1], 32), p(s[2], s[3], 32));
    }
    let mut b = WorldBox::of(project.world(s[0], s[1], 32), project.world(s[2], s[3], 32));
    const STEPS: usize = 8;
    for i in 0..=STEPS {
        let f = i as f64 / STEPS as f64;
        let x = s[0] + (s[2] - s[0]) * f;
        let y = s[1] + (s[3] - s[1]) * f;
        for p in [(x, s[1]), (x, s[3]), (s[0], y), (s[2], y)] {
            let (wx, wy) = project.world(p.0, p.1, 32);
            b = WorldBox::of((b.x0.min(wx), b.y0.min(wy)), (b.x1.max(wx), b.y1.max(wy)));
        }
    }
    let margin = ((b.x1 - b.x0).max(b.y1 - b.y0) / 100).max(1 << 10);
    WorldBox {
        x0: b.x0 - margin,
        y0: b.y0 - margin,
        x1: b.x1 + margin,
        y1: b.y1 + margin,
    }
}

/// The row groups of a GeoParquet file that may overlap `bbox`: by the
/// statistics of its GeoParquet 1.1 bounding box columns, or of its geometry
/// column's Parquet geospatial statistics, or all where there are neither.
pub fn row_groups(
    metadata: &parquet::file::metadata::ParquetMetaData,
    schema: &Schema,
    table: &Table,
    bbox: &WorldBox,
) -> Vec<usize> {
    let descr = metadata.file_metadata().schema_descr();
    let leaf =
        |path: &str| (0..descr.num_columns()).find(|&i| descr.column(i).path().string() == path);
    let geometry = schema.field(table.geometry).name();
    // the bounding box columns' paths, as `bbox.xmin`
    let covering: Option<[usize; 4]> = schema.metadata().get("geo").and_then(|geo| {
        let tree = json_tree(geo)?;
        let bbox = tree
            .root()
            .hash_get("columns")?
            .hash_get(geometry)?
            .hash_get("covering")?
            .hash_get("bbox")?;
        let path = |k: &str| -> Option<usize> {
            let parts = bbox.hash_get(k)?;
            let names: Vec<&str> = parts.array_iter().map(|p| p.string()).collect();
            leaf(&names.join("."))
        };
        Some([path("xmin")?, path("ymin")?, path("xmax")?, path("ymax")?])
    });
    let native = leaf(geometry);
    (0..metadata.num_row_groups())
        .filter(|&g| {
            let rg = metadata.row_group(g);
            let stats = if let Some([x0, y0, x1, y1]) = covering {
                (|| {
                    Some([
                        statistic(rg.column(x0).statistics(), false)?,
                        statistic(rg.column(y0).statistics(), false)?,
                        statistic(rg.column(x1).statistics(), true)?,
                        statistic(rg.column(y1).statistics(), true)?,
                    ])
                })()
            } else {
                native
                    .and_then(|i| rg.column(i).geo_statistics())
                    .and_then(|s| s.bounding_box())
                    .map(|b| [b.get_xmin(), b.get_ymin(), b.get_xmax(), b.get_ymax()])
            };
            match stats {
                // across the antimeridian, or no statistics: read it
                Some(s) if s[0] <= s[2] && s.iter().all(|v| v.is_finite()) => {
                    stats_box(&table.project, s).overlaps(bbox)
                }
                _ => true,
            }
        })
        .collect()
}

/// An Arrow IPC file's batches and schema, or a stream's from `r`.
pub fn open_ipc<R: Read + Seek + Send + 'static>(
    mut r: R,
    seekable: bool,
    fname: &str,
) -> crate::Result<(SchemaRef, Batches)> {
    if seekable {
        let mut magic = [0u8; 6];
        let n = r.read(&mut magic).map_err(|e| read_error(fname, e))?;
        r.rewind().map_err(|e| read_error(fname, e))?;
        if n == 6 && &magic == b"ARROW1" {
            let reader = arrow_ipc::reader::FileReader::try_new_buffered(r, None)
                .map_err(|e| read_error(fname, e))?;
            return Ok((reader.schema(), Box::new(reader)));
        }
    }
    let reader = arrow_ipc::reader::StreamReader::try_new_buffered(r, None)
        .map_err(|e| read_error(fname, e))?;
    Ok((reader.schema(), Box::new(reader)))
}

/// An Arrow IPC stream from a pipe or standard input.
pub fn open_ipc_stream<R: Read + Send + 'static>(
    r: R,
    fname: &str,
) -> crate::Result<(SchemaRef, Batches)> {
    let reader = arrow_ipc::reader::StreamReader::try_new(BufReader::new(r), None)
        .map_err(|e| read_error(fname, e))?;
    Ok((reader.schema(), Box::new(reader)))
}

/// How the geometry column holds its geometries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Wkb,
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
}

impl Encoding {
    fn from_name(name: &str) -> Option<Encoding> {
        Some(match name.to_ascii_lowercase().as_str() {
            "wkb" | "geoarrow.wkb" => Encoding::Wkb,
            "point" | "geoarrow.point" => Encoding::Point,
            "linestring" | "geoarrow.linestring" => Encoding::LineString,
            "polygon" | "geoarrow.polygon" => Encoding::Polygon,
            "multipoint" | "geoarrow.multipoint" => Encoding::MultiPoint,
            "multilinestring" | "geoarrow.multilinestring" => Encoding::MultiLineString,
            "multipolygon" | "geoarrow.multipolygon" => Encoding::MultiPolygon,
            _ => return None,
        })
    }
}

/// A property: a column, or a field of struct columns, by its path of child
/// indices from the table's column.
#[derive(Clone, Debug)]
struct Property {
    name: String,
    path: Vec<usize>,
}

/// How to read a table's features: its geometry column and properties.
pub struct Table {
    geometry: usize,
    encoding: Encoding,
    project: Project,
    properties: Vec<Property>,
    /// The table's columns that the features use, for a reader that can
    /// leave the others out.
    pub columns: Vec<usize>,
    /// Columns left out, for a warning.
    pub skipped: Vec<String>,
}

fn json_tree(text: &str) -> Option<tippecanoe::jsonpull::JsonTree> {
    JsonPull::begin_string(text.as_bytes()).read_tree()
}

fn json_str<'a>(j: Option<JsonRef<'a>>) -> Option<&'a str> {
    j.filter(|v| v.type_() == JsonType::String)
        .map(|v| v.string())
}

/// What a CRS says about the coordinates.
#[derive(Debug, PartialEq)]
enum Crs {
    /// None given: longitude and latitude, unless `-s` says otherwise.
    Unknown,
    LonLat,
    WebMercator,
    Other(String),
}

fn crs_from_code(authority: &str, code: &str) -> Crs {
    match (authority.to_ascii_uppercase().as_str(), code) {
        ("EPSG", "4326" | "4979") | ("OGC", "CRS84" | "CRS84h") => Crs::LonLat,
        ("EPSG", "3857" | "900913" | "3785" | "102100" | "102113")
        | ("ESRI", "102100" | "102113") => Crs::WebMercator,
        _ => Crs::Other(format!("{}:{}", authority, code)),
    }
}

/// A CRS as GeoParquet or GeoArrow metadata gives it: PROJJSON, or a string
/// such as `EPSG:4326`.
fn crs_of(j: Option<JsonRef<'_>>) -> Crs {
    let Some(j) = j else { return Crs::Unknown };
    match j.type_() {
        JsonType::Null => Crs::Unknown,
        JsonType::String => {
            let s = j.string();
            if let Some(tree) = json_tree(s)
                && tree.root().type_() == JsonType::Hash
            {
                return crs_of(Some(tree.root()));
            }
            let s = s.trim();
            if s.is_empty() {
                return Crs::Unknown;
            }
            match s.split_once(':') {
                Some((a, c)) if !c.contains(':') => crs_from_code(a, c),
                _ if s.contains("CRS84") || s == "WGS84" => Crs::LonLat,
                _ => Crs::Other(s.chars().take(80).collect()),
            }
        }
        JsonType::Hash => {
            if let Some(id) = j.hash_get("id")
                && id.type_() == JsonType::Hash
                && let Some(authority) = json_str(id.hash_get("authority"))
            {
                let code = match id.hash_get("code") {
                    Some(c) if c.type_() == JsonType::Number => dtoa_milo(c.number()),
                    Some(c) if c.type_() == JsonType::String => c.string().to_string(),
                    _ => String::new(),
                };
                return crs_from_code(authority, &code);
            }
            let name = json_str(j.hash_get("name")).unwrap_or("");
            match name {
                "WGS 84" | "WGS 84 (CRS84)" => Crs::LonLat,
                "WGS 84 / Pseudo-Mercator" => Crs::WebMercator,
                _ => Crs::Other(if name.is_empty() {
                    "a CRS without a name".to_string()
                } else {
                    name.to_string()
                }),
            }
        }
        _ => Crs::Unknown,
    }
}

fn is_binary(t: &DataType) -> bool {
    matches!(
        t,
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView
    )
}

/// Whether values of type `t` can be attributes.
fn readable(t: &DataType) -> bool {
    match t {
        DataType::Boolean
        | DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64
        | DataType::Float16
        | DataType::Float32
        | DataType::Float64
        | DataType::Decimal32(..)
        | DataType::Decimal64(..)
        | DataType::Decimal128(..)
        | DataType::Decimal256(..)
        | DataType::Utf8
        | DataType::LargeUtf8
        | DataType::Utf8View
        | DataType::Date32
        | DataType::Date64
        | DataType::Timestamp(..)
        | DataType::Time32(..)
        | DataType::Time64(..)
        | DataType::List(_)
        | DataType::LargeList(_)
        | DataType::FixedSizeList(..)
        | DataType::Map(..) => true,
        DataType::Dictionary(_, v) => readable(v),
        _ => false,
    }
}

impl Table {
    /// Finds the geometry column and the properties in `schema`.
    pub fn new(schema: &Schema, fname: &str, cfg: &Config<'_>) -> crate::Result<Table> {
        let fields = schema.fields();
        let index = |name: &str| fields.iter().position(|f| f.name() == name);

        let mut geometry = None;
        let mut encoding = Encoding::Wkb;
        let mut crs = Crs::Unknown;
        // the other geometry columns and the bounding box columns of GeoParquet
        let mut not_properties: Vec<usize> = Vec::new();

        if let Some(geo) = schema.metadata().get("geo")
            && let Some(tree) = json_tree(geo)
        {
            let root = tree.root();
            let primary = json_str(root.hash_get("primary_column"));
            if let Some(columns) = root.hash_get("columns")
                && columns.type_() == JsonType::Hash
            {
                for (k, v) in columns.entries() {
                    let Some(i) = index(k.string()) else {
                        continue;
                    };
                    if Some(k.string()) == primary {
                        geometry = Some(i);
                        encoding = json_str(v.hash_get("encoding"))
                            .and_then(Encoding::from_name)
                            .unwrap_or(Encoding::Wkb);
                        crs = match v.hash_get("crs") {
                            // missing means OGC:CRS84
                            None => Crs::LonLat,
                            c => crs_of(c),
                        };
                    } else {
                        not_properties.push(i);
                    }
                    if let Some(covering) = v.hash_get("covering")
                        && let Some(bbox) = covering.hash_get("bbox")
                        && let Some(xmin) = bbox.hash_get("xmin")
                        && xmin.type_() == JsonType::Array
                        && xmin.array_len() > 0
                        && let Some(c) = index(xmin.array_get(0).string())
                    {
                        not_properties.push(c);
                    }
                }
            }
        }

        if geometry.is_none() {
            for (i, f) in fields.iter().enumerate() {
                if let Some(name) = f.metadata().get("ARROW:extension:name")
                    && let Some(e) = Encoding::from_name(name)
                {
                    geometry = Some(i);
                    encoding = e;
                    crs = f
                        .metadata()
                        .get("ARROW:extension:metadata")
                        .and_then(|m| json_tree(m))
                        .map(|t| crs_of(t.root().hash_get("crs")))
                        .unwrap_or(Crs::Unknown);
                    break;
                }
            }
        }
        if geometry.is_none() {
            for name in ["geometry", "geom", "wkb_geometry", "the_geom", "wkb"] {
                if let Some(i) = index(name)
                    && is_binary(fields[i].data_type())
                {
                    geometry = Some(i);
                    break;
                }
            }
        }
        let Some(geometry) = geometry else {
            return Err(Error::new(
                EXIT_IMPOSSIBLE,
                format!(
                    "{}: no geometry column: no GeoParquet metadata, no GeoArrow column, and no column named geometry in well-known binary",
                    fname
                ),
            ));
        };
        if encoding == Encoding::Wkb && !is_binary(fields[geometry].data_type()) {
            return Err(Error::new(
                EXIT_IMPOSSIBLE,
                format!(
                    "{}: geometry column {} is {}, not well-known binary",
                    fname,
                    fields[geometry].name(),
                    fields[geometry].data_type()
                ),
            ));
        }

        let project = match crs {
            Crs::Unknown => cfg.project.clone(),
            Crs::LonLat => Project::Tippecanoe(PROJECTIONS[0].project),
            Crs::WebMercator => Project::Tippecanoe(PROJECTIONS[1].project),
            // made longitude and latitude by crs-defs' definition of it
            Crs::Other(name) => match Reprojection::new(&name) {
                Ok(r) => Project::Crs(std::sync::Arc::new(r)),
                Err(_) => {
                    return Err(Error::new(
                        EXIT_IMPOSSIBLE,
                        format!(
                            "{}: the coordinates are in {}, which manby doesn't know. Reproject them first, with DuckDB's ST_Transform or GeoPandas' to_crs for example",
                            fname, name
                        ),
                    ));
                }
            },
        };

        let mut properties = Vec::new();
        let mut skipped = Vec::new();
        let mut columns = vec![geometry];
        for (i, f) in fields.iter().enumerate() {
            if i == geometry || not_properties.contains(&i) {
                continue;
            }
            let before = properties.len();
            add_properties(f, f.name().clone(), vec![i], &mut properties, &mut skipped);
            if properties.len() > before {
                columns.push(i);
            }
        }
        columns.sort_unstable();

        Ok(Table {
            geometry,
            encoding,
            project,
            properties,
            columns,
            skipped,
        })
    }

    /// The columns of the table that its features use, by index.
    pub fn columns_of(schema: &Schema, fname: &str, cfg: &Config<'_>) -> crate::Result<Vec<usize>> {
        Ok(Table::new(schema, fname, cfg)?.columns)
    }
}

/// Adds field `f` as a property, or each of its fields if it is a struct.
fn add_properties(
    f: &Field,
    name: String,
    path: Vec<usize>,
    properties: &mut Vec<Property>,
    skipped: &mut Vec<String>,
) {
    match f.data_type() {
        DataType::Struct(children) => {
            for (j, c) in children.iter().enumerate() {
                let mut p = path.clone();
                p.push(j);
                add_properties(c, format!("{}.{}", name, c.name()), p, properties, skipped);
            }
        }
        DataType::Null => {}
        t if readable(t) => properties.push(Property { name, path }),
        _ => skipped.push(name),
    }
}

/// A batch's arrays for the table's geometry and properties.
pub struct Resolved {
    geometry: ArrayRef,
    /// Each property's values, and the structs it is in, whose nulls are its own.
    properties: Vec<(ArrayRef, Vec<ArrayRef>)>,
}

impl Resolved {
    pub fn new(table: &Table, batch: &RecordBatch) -> Resolved {
        let properties = table
            .properties
            .iter()
            .map(|p| {
                let mut a = batch.column(p.path[0]).clone();
                let mut parents = Vec::new();
                for &j in &p.path[1..] {
                    let child = a.as_struct().column(j).clone();
                    parents.push(a);
                    a = child;
                }
                (a, parents)
            })
            .collect();
        Resolved {
            geometry: batch.column(table.geometry).clone(),
            properties,
        }
    }
}

/// Days since 1970-01-01 as a civil date.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

fn date(days: i64) -> String {
    let (y, m, d) = civil(days);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// A time of day from a count of `unit`s, with the fraction where there is one.
fn time_of_day(value: i64, unit: &TimeUnit) -> (i64, String) {
    let per_second: i64 = match unit {
        TimeUnit::Second => 1,
        TimeUnit::Millisecond => 1_000,
        TimeUnit::Microsecond => 1_000_000,
        TimeUnit::Nanosecond => 1_000_000_000,
    };
    let per_day = per_second * 86_400;
    let days = value.div_euclid(per_day);
    let rest = value.rem_euclid(per_day);
    let secs = rest / per_second;
    let frac = rest % per_second;
    let mut s = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if frac != 0 {
        let digits = per_second.ilog10() as usize;
        let f = format!("{:0width$}", frac, width = digits);
        s.push('.');
        s.push_str(f.trim_end_matches('0'));
    }
    (days, s)
}

fn dictionary_value(a: &dyn Array, i: usize) -> (ArrayRef, usize) {
    macro_rules! key {
        ($t:ty) => {{
            let d = a.as_dictionary::<$t>();
            (d.values().clone(), d.keys().value(i) as usize)
        }};
    }
    match a.data_type() {
        DataType::Dictionary(k, _) => match k.as_ref() {
            DataType::Int8 => key!(Int8Type),
            DataType::Int16 => key!(Int16Type),
            DataType::Int32 => key!(Int32Type),
            DataType::Int64 => key!(Int64Type),
            DataType::UInt8 => key!(UInt8Type),
            DataType::UInt16 => key!(UInt16Type),
            DataType::UInt32 => key!(UInt32Type),
            _ => key!(UInt64Type),
        },
        _ => unreachable!("not a dictionary"),
    }
}

/// Value `i` of `a` as an attribute: its MVT type and text, as tippecanoe
/// reads the FlatGeobuf column that GDAL would write. None if it is null.
fn value(a: &dyn Array, i: usize) -> Option<SerialVal> {
    if a.is_null(i) {
        return None;
    }
    let number = |s: String| {
        Some(SerialVal {
            type_: MVT_DOUBLE,
            s,
        })
    };
    let string = |s: String| {
        Some(SerialVal {
            type_: MVT_STRING,
            s,
        })
    };
    macro_rules! int {
        ($t:ty) => {
            number(a.as_primitive::<$t>().value(i).to_string())
        };
    }
    macro_rules! decimal {
        ($t:ty, $p:expr, $s:expr) => {{
            let text = <$t>::format_decimal(a.as_primitive::<$t>().value(i), *$p, *$s);
            number(dtoa_milo(text.parse::<f64>().unwrap_or(f64::NAN)))
        }};
    }
    match a.data_type() {
        DataType::Boolean => Some(SerialVal {
            type_: MVT_BOOL,
            s: if a.as_boolean().value(i) {
                "true"
            } else {
                "false"
            }
            .to_string(),
        }),
        DataType::Int8 => int!(Int8Type),
        DataType::Int16 => int!(Int16Type),
        DataType::Int32 => int!(Int32Type),
        DataType::Int64 => int!(Int64Type),
        DataType::UInt8 => int!(UInt8Type),
        DataType::UInt16 => int!(UInt16Type),
        DataType::UInt32 => int!(UInt32Type),
        DataType::UInt64 => int!(UInt64Type),
        // as FlatGeobuf's Float, widened
        DataType::Float16 => number(dtoa_milo(
            a.as_primitive::<Float16Type>().value(i).to_f32() as f64
        )),
        DataType::Float32 => number(dtoa_milo(a.as_primitive::<Float32Type>().value(i) as f64)),
        DataType::Float64 => number(dtoa_milo(a.as_primitive::<Float64Type>().value(i))),
        DataType::Decimal32(p, s) => decimal!(Decimal32Type, p, s),
        DataType::Decimal64(p, s) => decimal!(Decimal64Type, p, s),
        DataType::Decimal128(p, s) => decimal!(Decimal128Type, p, s),
        DataType::Decimal256(p, s) => decimal!(Decimal256Type, p, s),
        DataType::Utf8 => string(a.as_string::<i32>().value(i).to_string()),
        DataType::LargeUtf8 => string(a.as_string::<i64>().value(i).to_string()),
        DataType::Utf8View => string(a.as_string_view().value(i).to_string()),
        DataType::Date32 => string(date(a.as_primitive::<Date32Type>().value(i) as i64)),
        DataType::Date64 => string(date(
            a.as_primitive::<Date64Type>()
                .value(i)
                .div_euclid(86_400_000),
        )),
        DataType::Timestamp(unit, tz) => {
            let v = match unit {
                TimeUnit::Second => a.as_primitive::<TimestampSecondType>().value(i),
                TimeUnit::Millisecond => a.as_primitive::<TimestampMillisecondType>().value(i),
                TimeUnit::Microsecond => a.as_primitive::<TimestampMicrosecondType>().value(i),
                TimeUnit::Nanosecond => a.as_primitive::<TimestampNanosecondType>().value(i),
            };
            let (days, time) = time_of_day(v, unit);
            let zone = if tz.is_some() { "Z" } else { "" };
            string(format!("{}T{}{}", date(days), time, zone))
        }
        DataType::Time32(unit) => {
            let v = match unit {
                TimeUnit::Second => a.as_primitive::<Time32SecondType>().value(i) as i64,
                _ => a.as_primitive::<Time32MillisecondType>().value(i) as i64,
            };
            string(time_of_day(v, unit).1)
        }
        DataType::Time64(unit) => {
            let v = match unit {
                TimeUnit::Microsecond => a.as_primitive::<Time64MicrosecondType>().value(i),
                _ => a.as_primitive::<Time64NanosecondType>().value(i),
            };
            string(time_of_day(v, unit).1)
        }
        DataType::Dictionary(..) => {
            let (values, k) = dictionary_value(a, i);
            value(values.as_ref(), k)
        }
        DataType::List(_)
        | DataType::LargeList(_)
        | DataType::FixedSizeList(..)
        | DataType::Map(..)
        | DataType::Struct(_) => {
            let mut s = String::new();
            json(a, i, &mut s);
            string(s)
        }
        _ => None,
    }
}

fn json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json_items(values: &dyn Array, range: Range<usize>, out: &mut String) {
    out.push('[');
    for (n, j) in range.enumerate() {
        if n > 0 {
            out.push(',');
        }
        json(values, j, out);
    }
    out.push(']');
}

/// Value `i` of `a` as JSON, as GDAL writes a list, map or struct.
fn json(a: &dyn Array, i: usize, out: &mut String) {
    if a.is_null(i) {
        out.push_str("null");
        return;
    }
    match a.data_type() {
        DataType::List(_) => {
            let l = a.as_list::<i32>();
            let o = l.value_offsets();
            json_items(l.values().as_ref(), o[i] as usize..o[i + 1] as usize, out);
        }
        DataType::LargeList(_) => {
            let l = a.as_list::<i64>();
            let o = l.value_offsets();
            json_items(l.values().as_ref(), o[i] as usize..o[i + 1] as usize, out);
        }
        DataType::FixedSizeList(_, n) => {
            let l = a.as_fixed_size_list();
            let start = l.value_offset_at(i);
            json_items(l.values().as_ref(), start..start + *n as usize, out);
        }
        DataType::Map(..) => {
            let m = a.as_map();
            let o = m.value_offsets();
            let (keys, values) = (m.keys(), m.values());
            out.push('{');
            for (n, j) in (o[i] as usize..o[i + 1] as usize).enumerate() {
                if n > 0 {
                    out.push(',');
                }
                let k = value(keys.as_ref(), j).map(|v| v.s).unwrap_or_default();
                json_string(&k, out);
                out.push(':');
                json(values.as_ref(), j, out);
            }
            out.push('}');
        }
        DataType::Struct(fields) => {
            let s = a.as_struct();
            out.push('{');
            for (n, f) in fields.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                json_string(f.name(), out);
                out.push(':');
                json(s.column(n).as_ref(), i, out);
            }
            out.push('}');
        }
        DataType::Dictionary(..) => {
            let (values, k) = dictionary_value(a, i);
            json(values.as_ref(), k, out);
        }
        _ => match value(a, i) {
            Some(v) if v.type_ == MVT_STRING => json_string(&v.s, out),
            Some(v) if v.type_ == MVT_DOUBLE && v.s.parse::<f64>().is_ok_and(f64::is_finite) => {
                out.push_str(&v.s)
            }
            Some(v) if v.type_ == MVT_BOOL => out.push_str(&v.s),
            _ => out.push_str("null"),
        },
    }
}

/// Reads well-known binary into tippecanoe's draw commands, as tippecanoe's
/// FlatGeobuf reader draws the same geometry.
struct Wkb<'a> {
    b: &'a [u8],
    pos: usize,
    project: &'a Project,
}

/// A geometry tippecanoe cannot read from FlatGeobuf either.
struct Unsupported(&'static str);

type WkbResult<T> = Result<T, Option<Unsupported>>;

impl Wkb<'_> {
    fn bytes<const N: usize>(&mut self) -> WkbResult<[u8; N]> {
        let b = self.b.get(self.pos..self.pos + N).ok_or(None)?;
        self.pos += N;
        Ok(b.try_into().unwrap())
    }

    fn u32(&mut self, le: bool) -> WkbResult<u32> {
        let b = self.bytes::<4>()?;
        Ok(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn f64(&mut self, le: bool) -> WkbResult<f64> {
        let b = self.bytes::<8>()?;
        Ok(if le {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }

    /// A geometry's byte order, type and coordinate count.
    fn header(&mut self) -> WkbResult<(bool, u32, usize)> {
        let le = self.bytes::<1>()?[0] == 1;
        let t = self.u32(le)?;
        // EWKB's flags, then ISO's thousands
        let mut dims = 2 + (t & 0x8000_0000 != 0) as usize + (t & 0x4000_0000 != 0) as usize;
        if t & 0x2000_0000 != 0 {
            self.u32(le)?; // SRID
        }
        let base = t & 0x0FFF_FFFF;
        dims += match base / 1000 {
            1 | 2 => 1,
            3 => 2,
            _ => 0,
        };
        Ok((le, base % 1000, dims))
    }

    fn point(&mut self, le: bool, dims: usize) -> WkbResult<(f64, f64)> {
        let x = self.f64(le)?;
        let y = self.f64(le)?;
        for _ in 2..dims {
            self.f64(le)?;
        }
        Ok((x, y))
    }

    fn draw(&self, op: i8, (x, y): (f64, f64)) -> Draw {
        let (x, y) = self.project.world(x, y, 32);
        Draw::new(op, x, y)
    }

    fn line(&mut self, le: bool, dims: usize, dv: &mut DrawVec) -> WkbResult<()> {
        let n = self.u32(le)?;
        for i in 0..n {
            let p = self.point(le, dims)?;
            dv.push(self.draw(if i == 0 { VT_MOVETO } else { VT_LINETO }, p));
        }
        Ok(())
    }

    fn polygon(&mut self, le: bool, dims: usize, dv: &mut DrawVec) -> WkbResult<()> {
        let rings = self.u32(le)?;
        for _ in 0..rings {
            self.line(le, dims, dv)?;
        }
        Ok(())
    }

    /// Reads a geometry into `dv`, giving its tippecanoe type.
    fn geometry(&mut self, dv: &mut DrawVec) -> WkbResult<i8> {
        let (le, t, dims) = self.header()?;
        Ok(match t {
            1 => {
                let p = self.point(le, dims)?;
                // POINT EMPTY
                if !(p.0.is_nan() && p.1.is_nan()) {
                    dv.push(self.draw(VT_MOVETO, p));
                }
                1
            }
            2 => {
                self.line(le, dims, dv)?;
                2
            }
            3 => {
                self.polygon(le, dims, dv)?;
                3
            }
            4..=6 => {
                let n = self.u32(le)?;
                for _ in 0..n {
                    let (le, part, dims) = self.header()?;
                    match (t, part) {
                        (4, 1) => {
                            let p = self.point(le, dims)?;
                            if !(p.0.is_nan() && p.1.is_nan()) {
                                dv.push(self.draw(VT_MOVETO, p));
                            }
                        }
                        (5, 2) => self.line(le, dims, dv)?,
                        (6, 3) => {
                            self.polygon(le, dims, dv)?;
                            dv.push(Draw::new(VT_CLOSEPATH, 0, 0));
                        }
                        _ => return Err(None),
                    }
                }
                (t - 3) as i8
            }
            7 => return Err(Some(Unsupported("GeometryCollection"))),
            _ => return Err(Some(Unsupported("geometry type"))),
        })
    }
}

/// The coordinates of a GeoArrow array: x and y columns, or interleaved.
enum Coords<'a> {
    Separated(&'a Float64Array, &'a Float64Array),
    Interleaved(&'a arrow_array::FixedSizeListArray, &'a Float64Array),
}

impl<'a> Coords<'a> {
    fn new(a: &'a dyn Array) -> Option<Coords<'a>> {
        match a.data_type() {
            DataType::Struct(_) => {
                let s = a.as_struct();
                let x = s.column(0).as_primitive_opt::<Float64Type>()?;
                let y = s.column(1).as_primitive_opt::<Float64Type>()?;
                Some(Coords::Separated(x, y))
            }
            DataType::FixedSizeList(..) => {
                let l = a.as_fixed_size_list();
                let v = l.values().as_primitive_opt::<Float64Type>()?;
                Some(Coords::Interleaved(l, v))
            }
            _ => None,
        }
    }

    fn get(&self, j: usize) -> (f64, f64) {
        match self {
            Coords::Separated(x, y) => (x.value(j), y.value(j)),
            Coords::Interleaved(l, v) => {
                let o = l.value_offset_at(j);
                (v.value(o), v.value(o + 1))
            }
        }
    }
}

/// The values and range of list `i` of `a`.
fn list(a: &dyn Array, i: usize) -> Option<(&ArrayRef, Range<usize>)> {
    match a.data_type() {
        DataType::List(_) => {
            let l = a.as_list::<i32>();
            let o = l.value_offsets();
            Some((l.values(), o[i] as usize..o[i + 1] as usize))
        }
        DataType::LargeList(_) => {
            let l = a.as_list::<i64>();
            let o = l.value_offsets();
            Some((l.values(), o[i] as usize..o[i + 1] as usize))
        }
        _ => None,
    }
}

/// Reads GeoArrow geometry `i` of `a` into `dv`, giving its tippecanoe type.
fn geoarrow(
    a: &dyn Array,
    i: usize,
    encoding: Encoding,
    project: &Project,
    dv: &mut DrawVec,
) -> Option<i8> {
    let draw = |op, (x, y): (f64, f64)| {
        let (x, y) = project.world(x, y, 32);
        Draw::new(op, x, y)
    };
    let line = |a: &dyn Array, i: usize, dv: &mut DrawVec| -> Option<()> {
        let (values, range) = list(a, i)?;
        let c = Coords::new(values.as_ref())?;
        for (n, j) in range.enumerate() {
            dv.push(draw(if n == 0 { VT_MOVETO } else { VT_LINETO }, c.get(j)));
        }
        Some(())
    };
    let polygon = |a: &dyn Array, i: usize, dv: &mut DrawVec| -> Option<()> {
        let (rings, range) = list(a, i)?;
        for r in range {
            line(rings.as_ref(), r, dv)?;
        }
        Some(())
    };
    Some(match encoding {
        Encoding::Point => {
            let p = Coords::new(a)?.get(i);
            if !(p.0.is_nan() && p.1.is_nan()) {
                dv.push(draw(VT_MOVETO, p));
            }
            1
        }
        Encoding::MultiPoint => {
            let (values, range) = list(a, i)?;
            let c = Coords::new(values.as_ref())?;
            for j in range {
                dv.push(draw(VT_MOVETO, c.get(j)));
            }
            1
        }
        Encoding::LineString => {
            line(a, i, dv)?;
            2
        }
        Encoding::MultiLineString => {
            let (parts, range) = list(a, i)?;
            for p in range {
                line(parts.as_ref(), p, dv)?;
            }
            2
        }
        Encoding::Polygon => {
            polygon(a, i, dv)?;
            3
        }
        Encoding::MultiPolygon => {
            let (parts, range) = list(a, i)?;
            for p in range {
                polygon(parts.as_ref(), p, dv)?;
                dv.push(Draw::new(VT_CLOSEPATH, 0, 0));
            }
            3
        }
        Encoding::Wkb => unreachable!("not GeoArrow"),
    })
}

fn wkb_bytes(a: &dyn Array, i: usize) -> &[u8] {
    match a.data_type() {
        DataType::Binary => a.as_binary::<i32>().value(i),
        DataType::LargeBinary => a.as_binary::<i64>().value(i),
        _ => a.as_binary_view().value(i),
    }
}

/// Reads row `row` of a batch into `chunk`, as tippecanoe's `readFeature`
/// reads a FlatGeobuf feature.
pub fn read_row(
    chunk: &mut Chunk,
    cfg: &Config<'_>,
    table: &Table,
    batch: &Resolved,
    row: usize,
    fname: &str,
    layername: &str,
) -> crate::Result<()> {
    let g = batch.geometry.as_ref();
    if g.is_null(row) {
        return Ok(());
    }
    let mut dv = DrawVec::new();
    let t = if table.encoding == Encoding::Wkb {
        let mut w = Wkb {
            b: wkb_bytes(g, row),
            pos: 0,
            project: &table.project,
        };
        match w.geometry(&mut dv) {
            Ok(t) => t,
            Err(Some(Unsupported(what))) => {
                if !chunk
                    .messages
                    .iter()
                    .any(|m| m.kind == MessageKind::Once("arrow unsupported"))
                {
                    chunk.message(
                        MessageKind::Once("arrow unsupported"),
                        None,
                        format!(
                            "{}: skipping a {}, which manby doesn't read (additional not reported)",
                            fname, what
                        ),
                    );
                }
                return Ok(());
            }
            Err(None) => {
                return Err(Error::new(
                    EXIT_IMPOSSIBLE,
                    format!(
                        "{}: a geometry in well-known binary is cut short or malformed",
                        fname
                    ),
                ));
            }
        }
    } else {
        match geoarrow(g, row, table.encoding, &table.project, &mut dv) {
            Some(t) => t,
            None => {
                return Err(Error::new(
                    EXIT_IMPOSSIBLE,
                    format!(
                        "{}: the geometry column is not GeoArrow {:?} with float64 coordinates",
                        fname, table.encoding
                    ),
                ));
            }
        }
    };

    let mut full_keys: Vec<Cow<'_, str>> = Vec::new();
    let mut full_values: Vec<SerialVal> = Vec::new();
    for (p, (values, parents)) in table.properties.iter().zip(&batch.properties) {
        if parents.iter().any(|s| s.is_null(row)) {
            continue;
        }
        if let Some(v) = value(values.as_ref(), row) {
            full_keys.push(Cow::Borrowed(p.name.as_str()));
            full_values.push(v);
        }
    }

    let mut sf = InFeature {
        t,
        geometry: dv,
        has_id: false,
        id: 0,
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
    fn dates_and_times() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(-1), "1969-12-31");
        assert_eq!(date(19_723), "2024-01-01");
        assert_eq!(date(11_016), "2000-02-29");
        assert_eq!(
            time_of_day(1_500, &TimeUnit::Millisecond),
            (0, "00:00:01.5".to_string())
        );
        assert_eq!(
            time_of_day(-1, &TimeUnit::Second),
            (-1, "23:59:59".to_string())
        );
        assert_eq!(
            time_of_day(86_400_000_000 + 3_723_000_001, &TimeUnit::Microsecond),
            (1, "01:02:03.000001".to_string())
        );
    }

    #[test]
    fn crs() {
        let projjson = r#"{"type":"ProjectedCRS","name":"WGS 84 / Pseudo-Mercator","id":{"authority":"EPSG","code":3857}}"#;
        let tree = json_tree(projjson).unwrap();
        assert_eq!(crs_of(Some(tree.root())), Crs::WebMercator);
        let tree = json_tree(r#"{"c":"OGC:CRS84","d":"EPSG:32633","e":null}"#).unwrap();
        let root = tree.root();
        assert_eq!(crs_of(root.hash_get("c")), Crs::LonLat);
        assert_eq!(
            crs_of(root.hash_get("d")),
            Crs::Other("EPSG:32633".to_string())
        );
        assert_eq!(crs_of(root.hash_get("e")), Crs::Unknown);
        assert_eq!(crs_of(None), Crs::Unknown);
    }

    #[test]
    fn wkb_with_elevation_and_srid() {
        // EWKB POINT Z with an SRID, big-endian
        let mut b = vec![0u8];
        b.extend_from_slice(&(0x8000_0000u32 | 0x2000_0000 | 1).to_be_bytes());
        b.extend_from_slice(&4326u32.to_be_bytes());
        for v in [11.4f64, 48.2, 500.0] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        let mut dv = DrawVec::new();
        let lonlat = Project::Tippecanoe(PROJECTIONS[0].project);
        let mut w = Wkb {
            b: &b,
            pos: 0,
            project: &lonlat,
        };
        assert!(matches!(w.geometry(&mut dv), Ok(1)));
        assert_eq!(w.pos, b.len());
        let (x, y) = (PROJECTIONS[0].project)(11.4, 48.2, 32);
        assert_eq!((dv[0].x, dv[0].y), (x, y));
    }
}
