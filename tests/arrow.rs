//! GeoParquet and Arrow input make the tiles that tippecanoe makes from the
//! FlatGeobuf file GDAL writes of the same table.
//!
//! The pairs in tests/arrow/ are tippecanoe's test inputs, read by DuckDB and
//! written as GeoParquet, and through GDAL as FlatGeobuf without a spatial
//! index (`LAYER_CREATION_OPTIONS 'SPATIAL_INDEX=NO'`). manby's FlatGeobuf
//! reader is tippecanoe's, by the golden tests. The Arrow streams are made
//! here from the GeoParquet files. The `-bbox.fgb` files hold the rows that
//! overlap -110,30,-90,45, as DuckDB picks them by their geometries' extents,
//! for `--bbox`. The `-5070` files are the lines in NAD83 / Conus Albers,
//! as GeoPandas wrote them with PROJ.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use arrow_ipc::writer::StreamWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

const CASES: [&str; 4] = [
    "wineries",
    "ne_110m_admin_1_states_provinces_lines",
    "knox",
    "ne_110m_ocean",
];

const FLAGS: [&[&str]; 4] = [
    &["-z5"],
    &["-zg", "--drop-densest-as-needed"],
    &["-z6", "-r1", "--generate-ids"],
    &["-zg", "--coalesce-densest-as-needed", "-y", "name"],
];

/// A PMTiles archive's directories and tiles.
fn tiles(path: &Path) -> Vec<u8> {
    let mut f = File::open(path).unwrap();
    let mut h = [0u8; 127];
    f.read_exact(&mut h).unwrap();
    let at = |i: usize| u64::from_le_bytes(h[i..i + 8].try_into().unwrap());
    let mut out = Vec::new();
    // the root directory, the leaf directories and the tiles
    for (o, l) in [(8, 16), (40, 48), (56, 64)] {
        let mut b = vec![0u8; at(l) as usize];
        f.seek(SeekFrom::Start(at(o))).unwrap();
        f.read_exact(&mut b).unwrap();
        out.extend_from_slice(&b);
    }
    out
}

fn tile(input: &Path, out: &Path, flags: &[&str]) -> Vec<u8> {
    let mut argv: Vec<String> = ["tippecanoe", "-q", "-f", "-l", "layer"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    argv.extend(flags.iter().map(|s| s.to_string()));
    argv.extend([
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
        input.to_string_lossy().into_owned(),
    ]);
    let o = manby::options::parse_args(&argv).unwrap();
    manby::run(&o).unwrap();
    tiles(out)
}

/// The GeoParquet file as an Arrow IPC stream.
fn arrow_stream(parquet: &Path, out: &Path) {
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(parquet).unwrap())
        .unwrap()
        .with_batch_size(37)
        .build()
        .unwrap();
    let schema = arrow_array::RecordBatchReader::schema(&reader);
    let mut w = StreamWriter::try_new(File::create(out).unwrap(), &schema).unwrap();
    for b in reader {
        w.write(&b.unwrap()).unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn same_tiles_as_flatgeobuf() {
    let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/arrow");
    let dir = std::env::temp_dir().join(format!("manby-arrow-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for case in CASES {
        let stream = dir.join(format!("{}.arrows", case));
        arrow_stream(&data.join(format!("{}.parquet", case)), &stream);
        for flags in FLAGS {
            let fgb = tile(
                &data.join(format!("{}.fgb", case)),
                &dir.join("fgb.pmtiles"),
                flags,
            );
            assert!(!fgb.is_empty());
            let parquet = tile(
                &data.join(format!("{}.parquet", case)),
                &dir.join("parquet.pmtiles"),
                flags,
            );
            assert!(parquet == fgb, "{} {:?}: GeoParquet", case, flags);
            let arrow = tile(&stream, &dir.join("arrow.pmtiles"), flags);
            assert!(arrow == fgb, "{} {:?}: Arrow", case, flags);
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bbox_keeps_the_rows_that_overlap_it() {
    let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/arrow");
    let dir = std::env::temp_dir().join(format!("manby-arrow-bbox-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for case in ["ne_110m_admin_1_states_provinces_lines", "ne_110m_ocean"] {
        for flags in FLAGS {
            let fgb = tile(
                &data.join(format!("{}-bbox.fgb", case)),
                &dir.join("fgb.pmtiles"),
                flags,
            );
            let mut with_bbox = flags.to_vec();
            with_bbox.push("--bbox=-110,30,-90,45");
            let parquet = tile(
                &data.join(format!("{}.parquet", case)),
                &dir.join("parquet.pmtiles"),
                &with_bbox,
            );
            assert!(parquet == fgb, "{} {:?}", case, flags);
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn reprojected_from_another_crs() {
    let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/arrow");
    let dir = std::env::temp_dir().join(format!("manby-arrow-crs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let case = "ne_110m_admin_1_states_provinces_lines";
    for flags in FLAGS {
        let lonlat = tile(
            &data.join(format!("{}.parquet", case)),
            &dir.join("a.pmtiles"),
            flags,
        );
        // the CRS of the GeoParquet metadata
        let parquet = tile(
            &data.join(format!("{}-5070.parquet", case)),
            &dir.join("b.pmtiles"),
            flags,
        );
        assert!(parquet == lonlat, "GeoParquet {:?}", flags);
        // the CRS of -s
        let mut with_s = flags.to_vec();
        with_s.extend(["-s", "EPSG:5070"]);
        let geojson = tile(
            &data.join(format!("{}-5070.geojson", case)),
            &dir.join("c.pmtiles"),
            &with_s,
        );
        assert!(geojson == lonlat, "GeoJSON {:?}", flags);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
