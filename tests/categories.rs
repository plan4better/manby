//! `--cluster-categories`: a cluster counts the values of its points, as
//! `kind:<value>`, which add up to its `point_count`.

use std::io::{Read, Write};

use tippecanoe::mvt::{MVT_DOUBLE, MVT_FLOAT, MVT_INT, MVT_SINT, MVT_UINT, MvtTile, MvtValue};

fn number(v: &MvtValue) -> f64 {
    match v.type_ {
        MVT_DOUBLE | MVT_FLOAT => v.numeric_value.double_value(),
        MVT_INT => v.numeric_value.int_value() as f64,
        MVT_UINT => v.numeric_value.uint_value() as f64,
        MVT_SINT => v.numeric_value.sint_value() as f64,
        t => panic!("a number of type {}", t),
    }
}

#[test]
fn clusters_count_their_categories() {
    let dir = std::env::temp_dir().join(format!("manby-categories-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("points.geojsonl");
    let mut f = std::fs::File::create(&input).unwrap();
    for i in 0..2000 {
        let kinds = ["park", "museum", "cafe", "library"];
        writeln!(
            f,
            r#"{{"type":"Feature","properties":{{"kind":"{}"}},"geometry":{{"type":"Point","coordinates":[{},{}]}}}}"#,
            kinds[i % 4],
            -10.0 + (i * 7919 % 2000) as f64 / 50.0,
            35.0 + (i * 104729 % 2000) as f64 / 80.0
        )
        .unwrap();
    }
    drop(f);
    let out = dir.join("c.pmtiles");
    let argv: Vec<String> = [
        "tippecanoe",
        "-q",
        "-f",
        "-z4",
        "-r1",
        "--cluster-distance=30",
        "--cluster-categories=kind:park,museum",
        "-l",
        "p",
        "-o",
        out.to_str().unwrap(),
        input.to_str().unwrap(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    manby::run(&manby::options::parse_args(&argv).unwrap()).unwrap();

    let t = manby::serve::tileset::Tileset::new(out.to_str().unwrap(), String::new()).unwrap();
    let data = t.opened().unwrap().tile(0, 0, 0).unwrap().unwrap();
    let mut mvt = Vec::new();
    flate2::read::GzDecoder::new(&data[..])
        .read_to_end(&mut mvt)
        .unwrap();
    let mut tile = MvtTile::default();
    let mut compressed = false;
    assert!(tile.try_decode(&mvt, &mut compressed).unwrap());
    let layer = &tile.layers[0];
    let mut points = 0.0;
    let mut clusters = 0;
    for f in &layer.features {
        let mut count = 1.0;
        let mut kinds = 0.0;
        for kv in f.tags.chunks(2) {
            let key = layer.keys[kv[0] as usize].as_str();
            let v = &layer.values[kv[1] as usize];
            if key == "point_count" {
                count = number(v);
                clusters += 1;
            } else if let Some(kind) = key.strip_prefix("kind:") {
                assert!(["park", "museum", "_other"].contains(&kind), "{}", kind);
                kinds += number(v);
            }
        }
        assert_eq!(kinds, count);
        points += count;
    }
    assert!(clusters > 0);
    assert_eq!(points, 2000.0);
    std::fs::remove_dir_all(&dir).unwrap();
}
