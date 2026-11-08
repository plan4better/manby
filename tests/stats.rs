//! `manby stats` reports the same tiles and features for a PMTiles archive,
//! an MBTiles database and a directory of tiles made from the same features.

use std::path::{Path, PathBuf};

fn make(input: &Path, flags: &[&str]) {
    let mut argv: Vec<String> = ["tippecanoe", "-q", "-f", "-z6", "-l", "states"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    argv.extend(flags.iter().map(|s| s.to_string()));
    argv.push(input.to_string_lossy().into_owned());
    manby::run(&manby::options::parse_args(&argv).unwrap()).unwrap();
}

#[test]
fn same_report_from_each_kind_of_tileset() {
    let input = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/arrow/ne_110m_admin_1_states_provinces_lines.fgb");
    let dir = std::env::temp_dir().join(format!("manby-stats-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let paths = [
        dir.join("states.pmtiles"),
        dir.join("states.mbtiles"),
        dir.join("tiles"),
    ];
    make(&input, &["-o", paths[0].to_str().unwrap()]);
    make(&input, &["-o", paths[1].to_str().unwrap()]);
    make(&input, &["-e", paths[2].to_str().unwrap()]);

    let reports: Vec<_> = paths
        .iter()
        .map(|p| manby::stats::report(p.to_str().unwrap(), true, 5).unwrap())
        .collect();
    let first = &reports[0];
    assert_eq!(first.zooms.len(), 7);
    let header = std::fs::read(&paths[0]).unwrap();
    let addressed = u64::from_le_bytes(header[72..80].try_into().unwrap());
    assert_eq!(first.zooms.iter().map(|z| z.tiles).sum::<u64>(), addressed);
    for z in &first.zooms {
        assert!(z.p50 <= z.p99 && z.p99 <= z.max, "{:?}", z);
        let layer = &z.layers["states"];
        assert_eq!(layer.tiles, z.tiles);
        assert_eq!(layer.features, layer.lines);
        assert!(layer.features > 0);
    }
    assert_eq!(first.largest.len(), 5);
    assert!(first.largest.windows(2).all(|w| w[0].bytes >= w[1].bytes));
    for r in &reports[1..] {
        assert_eq!(r.zooms, first.zooms, "{}", r.tileset);
        assert_eq!(r.largest, first.largest, "{}", r.tileset);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
