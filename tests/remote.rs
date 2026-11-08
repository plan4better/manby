//! GeoParquet read from a URL makes the tiles that the same file makes on
//! disk, with and without `--bbox`. The files are served here, with range
//! requests, as object storage serves them.
#![cfg(feature = "remote")]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

fn data() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/arrow")
}

/// Serves the files of `dir` at `http://127.0.0.1:{port}/`, with HEAD and
/// single byte ranges.
fn serve(dir: PathBuf) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let name = request.url().trim_start_matches('/').to_string();
            let Ok(mut file) = File::open(dir.join(&name)) else {
                let _ = request.respond(tiny_http::Response::empty(404));
                continue;
            };
            let len = file.metadata().unwrap().len();
            let range = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Range"))
                .and_then(|h| h.value.as_str().strip_prefix("bytes=").map(String::from));
            let (status, start, end) = match range.and_then(|r| {
                let (a, b) = r.split_once('-')?;
                if a.is_empty() {
                    let n: u64 = b.parse().ok()?;
                    Some((len - n.min(len), len - 1))
                } else {
                    let a: u64 = a.parse().ok()?;
                    let b = if b.is_empty() {
                        len - 1
                    } else {
                        b.parse::<u64>().ok()?.min(len - 1)
                    };
                    Some((a, b))
                }
            }) {
                Some((a, b)) => (206, a, b),
                None => (200, 0, len - 1),
            };
            let mut body = vec![0u8; (end - start + 1) as usize];
            file.seek(SeekFrom::Start(start)).unwrap();
            file.read_exact(&mut body).unwrap();
            // with a length, not chunked, as object storage answers HEAD too
            let mut response = tiny_http::Response::from_data(body)
                .with_status_code(status)
                .with_chunked_threshold(usize::MAX);
            response.add_header(
                tiny_http::Header::from_bytes(
                    &b"Content-Range"[..],
                    format!("bytes {}-{}/{}", start, end, len),
                )
                .unwrap(),
            );
            response.add_header(
                tiny_http::Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap(),
            );
            let _ = request.respond(response);
        }
    });
    format!("http://127.0.0.1:{}/", port)
}

fn tiles(path: &Path) -> Vec<u8> {
    let mut f = File::open(path).unwrap();
    let mut h = [0u8; 127];
    f.read_exact(&mut h).unwrap();
    let at = |i: usize| u64::from_le_bytes(h[i..i + 8].try_into().unwrap());
    let mut out = Vec::new();
    for (o, l) in [(8, 16), (40, 48), (56, 64)] {
        let mut b = vec![0u8; at(l) as usize];
        f.seek(SeekFrom::Start(at(o))).unwrap();
        f.read_exact(&mut b).unwrap();
        out.extend_from_slice(&b);
    }
    out
}

fn tile(input: &str, out: &Path, flags: &[&str]) -> manby::Result<Vec<u8>> {
    let mut argv: Vec<String> = ["tippecanoe", "-q", "-f", "-l", "layer"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    argv.extend(flags.iter().map(|s| s.to_string()));
    argv.extend([
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
        input.to_string(),
    ]);
    manby::run(&manby::options::parse_args(&argv)?)?;
    Ok(tiles(out))
}

#[test]
fn same_tiles_from_a_url() {
    let base = serve(data());
    let dir = std::env::temp_dir().join(format!("manby-remote-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("t.pmtiles");
    for case in ["ne_110m_admin_1_states_provinces_lines", "knox"] {
        let local = data().join(format!("{}.parquet", case));
        let url = format!("{}{}.parquet", base, case);
        for flags in [
            &["-z5"][..],
            &["-zg", "--drop-densest-as-needed"],
            &["-z5", "--bbox=-125,25,-80,40"],
        ] {
            let a = tile(local.to_str().unwrap(), &out, flags).unwrap();
            let b = tile(&url, &out, flags).unwrap();
            assert!(!a.is_empty());
            assert!(a == b, "{} {:?}", case, flags);
        }
    }
    let missing = tile(&format!("{}nothing.parquet", base), &out, &["-z5"]);
    assert!(missing.is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
