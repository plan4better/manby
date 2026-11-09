//! `--largest-maximum-zoom-guess` caps the maxzoom that `-zg` chooses, and
//! leaves a smaller guess alone.

use std::path::{Path, PathBuf};

/// Tiles `knox.fgb` with `flags` and returns the archive's maxzoom.
fn maxzoom(dir: &Path, name: &str, flags: &[&str]) -> u8 {
    let input = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/arrow/knox.fgb");
    let out = dir.join(format!("{name}.pmtiles"));
    let mut argv: Vec<String> = ["tippecanoe", "-q", "-f", "-l", "knox", "-o"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    argv.push(out.to_string_lossy().into_owned());
    argv.extend(flags.iter().map(|s| s.to_string()));
    argv.push(input.to_string_lossy().into_owned());
    manby::run(&manby::options::parse_args(&argv).unwrap()).unwrap();
    // PMTiles v3 header: max_zoom is the byte at offset 101
    std::fs::read(&out).unwrap()[101]
}

#[test]
fn caps_the_guess() {
    let dir = std::env::temp_dir().join(format!("manby-largest-guess-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let guessed = maxzoom(&dir, "guess", &["-zg"]);
    assert!(
        guessed > 3,
        "fixture should guess above z3, guessed z{guessed}"
    );

    let capped = maxzoom(&dir, "capped", &["--largest-maximum-zoom-guess=3"]);
    assert_eq!(capped, 3);

    let above = guessed as i32 + 2;
    let uncapped = maxzoom(
        &dir,
        "above",
        &[&format!("--largest-maximum-zoom-guess={above}")],
    );
    assert_eq!(uncapped, guessed);

    std::fs::remove_dir_all(&dir).unwrap();
}
