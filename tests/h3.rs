//! `--h3`: hexagons below the base zoom, holding every point between them,
//! and the points from the base zoom.

use std::io::Read;
use std::path::PathBuf;

use tippecanoe::mvt::{MVT_DOUBLE, MVT_FLOAT, MVT_INT, MVT_SINT, MVT_UINT, MvtTile};

fn run(args: &[&str]) {
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    manby::run(&manby::options::parse_args(&argv).unwrap()).unwrap();
}

#[test]
fn hexagons_below_the_base_zoom() {
    let input = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/arrow/wineries.fgb");
    let dir = std::env::temp_dir().join(format!("manby-h3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("w.pmtiles");
    let all = dir.join("all.pmtiles");
    let input = input.to_str().unwrap();
    // every point in one tile, to count them
    run(&[
        "tippecanoe",
        "-q",
        "-f",
        "-z0",
        "-r1",
        "--no-feature-limit",
        "--no-tile-size-limit",
        "-l",
        "w",
        "-o",
        all.to_str().unwrap(),
        input,
    ]);
    let points = manby::stats::report(all.to_str().unwrap(), true, 0)
        .unwrap()
        .zooms[0]
        .layers["w"]
        .points;
    assert!(points > 50);

    run(&[
        "tippecanoe",
        "-q",
        "-f",
        "-z8",
        "--h3",
        "--h3-base-zoom=6",
        "-l",
        "w",
        "-o",
        out.to_str().unwrap(),
        input,
    ]);
    let report = manby::stats::report(out.to_str().unwrap(), true, 0).unwrap();
    for z in &report.zooms {
        let names: Vec<&str> = z.layers.keys().map(String::as_str).collect();
        if z.z < 6 {
            assert_eq!(names, ["w_h3"], "z{}", z.z);
            assert_eq!(z.layers["w_h3"].polygons, z.layers["w_h3"].features);
        } else {
            assert_eq!(names, ["w"], "z{}", z.z);
        }
    }

    // the hexagons of zoom 0 hold every point
    let t = manby::serve::tileset::Tileset::new(out.to_str().unwrap(), String::new()).unwrap();
    let data = t.opened().unwrap().tile(0, 0, 0).unwrap().unwrap();
    let mut mvt = Vec::new();
    flate2::read::GzDecoder::new(&data[..])
        .read_to_end(&mut mvt)
        .unwrap();
    let mut tile = MvtTile::default();
    let mut compressed = false;
    assert!(tile.try_decode(&mvt, &mut compressed).unwrap());
    let layer = tile.layers.iter().find(|l| l.name == "w_h3").unwrap();
    let key = layer.keys.iter().position(|k| k == "point_count").unwrap() as u32;
    let mut sum = 0.0;
    for f in &layer.features {
        for kv in f.tags.chunks(2) {
            if kv[0] == key {
                let v = &layer.values[kv[1] as usize];
                sum += match v.type_ {
                    MVT_DOUBLE | MVT_FLOAT => v.numeric_value.double_value(),
                    MVT_INT => v.numeric_value.int_value() as f64,
                    MVT_UINT => v.numeric_value.uint_value() as f64,
                    MVT_SINT => v.numeric_value.sint_value() as f64,
                    t => panic!("point_count of type {}", t),
                };
            }
        }
    }
    assert_eq!(sum, points as f64);
    std::fs::remove_dir_all(&dir).unwrap();
}
