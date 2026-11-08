//! `manby serve`: serves tilesets over HTTP while they are made and looked
//! at, with a map of them at `/`. Each tileset is served as tiles at
//! `/{name}/{z}/{x}/{y}.mvt` (`.mlt` for MapLibre Tiles), with its TileJSON at
//! `/{name}.json`, and a PMTiles archive also as itself, by its file name,
//! with range requests, for maps that read PMTiles. Every response allows
//! any origin and is not to be cached, so a tileset made again shows when the
//! map is reloaded.

pub mod tileset;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use tiny_http::{Header, Method, Request, Response, ResponseBox, StatusCode};
use tippecanoe::errors::{EXIT_ARGS, EXIT_OPEN, EXIT_SUCCESS};

use crate::{Error, Result};
use tileset::{Kind, Tileset};

const VIEWER: &str = include_str!("viewer.html");

const USAGE: &str = "Usage: manby serve [--port=8080] [--host=127.0.0.1] [--open] tileset ...
Serves PMTiles archives, MBTiles databases and directories of tiles over HTTP,
with a map of them at /.
  -p, --port=PORT  the port, 8080 by default, or 0 for any free port
  --host=HOST      the address to listen on, 127.0.0.1 by default
  --open           open the map in a browser";

#[derive(Clone, Debug, PartialEq)]
pub struct ServeOptions {
    pub host: String,
    pub port: u16,
    pub open: bool,
    pub tilesets: Vec<String>,
}

/// Parses `manby serve`'s arguments, the first being the program's name.
pub fn parse_args(argv: &[String]) -> Result<ServeOptions> {
    let mut o = ServeOptions {
        host: "127.0.0.1".to_string(),
        port: 8080,
        open: false,
        tilesets: Vec::new(),
    };
    let usage = |status: i32| Error::new(status, USAGE);
    let mut args = argv.iter().skip(1);
    while let Some(a) = args.next() {
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        let mut value = || inline.clone().or_else(|| args.next().cloned());
        match key {
            "-h" | "--help" => return Err(usage(EXIT_SUCCESS)),
            "-p" | "--port" => {
                o.port = value()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| usage(EXIT_ARGS))?
            }
            "--host" => o.host = value().ok_or_else(|| usage(EXIT_ARGS))?,
            "--open" => o.open = true,
            _ if key.starts_with('-') && key.len() > 1 => return Err(usage(EXIT_ARGS)),
            _ => o.tilesets.push(a.clone()),
        }
    }
    if o.tilesets.is_empty() {
        return Err(usage(EXIT_ARGS));
    }
    Ok(o)
}

pub struct Server {
    http: Arc<tiny_http::Server>,
    tilesets: Arc<Vec<Tileset>>,
    /// The map's address.
    pub url: String,
}

impl Server {
    /// Opens the tilesets and listens, without serving yet.
    pub fn bind(o: &ServeOptions) -> Result<Server> {
        let mut tilesets: Vec<Tileset> = Vec::new();
        for path in &o.tilesets {
            let stem = std::path::Path::new(path.trim_end_matches('/'))
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut base: String = stem
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            if base.is_empty() || base == "tilesets" {
                base = format!("tileset{}", tilesets.len() + 1);
            }
            let mut name = base.clone();
            let mut n = 2;
            while tilesets.iter().any(|t| t.name == name) {
                name = format!("{}-{}", base, n);
                n += 1;
            }
            let t = Tileset::new(path, name).map_err(|e| Error::new(EXIT_OPEN, e))?;
            t.opened().map_err(|e| Error::new(EXIT_OPEN, e))?;
            tilesets.push(t);
        }
        let http = tiny_http::Server::http((o.host.as_str(), o.port)).map_err(|e| {
            Error::new(
                EXIT_OPEN,
                format!("manby serve: can't listen on {}:{}: {}", o.host, o.port, e),
            )
        })?;
        let addr = http
            .server_addr()
            .to_ip()
            .ok_or_else(|| Error::new(EXIT_OPEN, "manby serve: not listening on an IP address"))?;
        let host = if addr.ip().is_unspecified() {
            "localhost".to_string()
        } else if addr.is_ipv6() {
            format!("[{}]", addr.ip())
        } else {
            addr.ip().to_string()
        };
        Ok(Server {
            http: Arc::new(http),
            tilesets: Arc::new(tilesets),
            url: format!("http://{}:{}/", host, addr.port()),
        })
    }

    pub fn tilesets(&self) -> &[Tileset] {
        &self.tilesets
    }

    /// Serves until the process ends, on a few threads.
    pub fn run(self) {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().clamp(2, 8))
            .unwrap_or(4);
        let workers: Vec<_> = (1..threads)
            .map(|_| {
                let http = self.http.clone();
                let tilesets = self.tilesets.clone();
                std::thread::spawn(move || work(&http, &tilesets))
            })
            .collect();
        work(&self.http, &self.tilesets);
        for w in workers {
            let _ = w.join();
        }
    }
}

/// Serves the tilesets, printing the map's address and then each TileJSON's
/// on standard output.
pub fn serve(o: &ServeOptions) -> Result<()> {
    let server = Server::bind(o)?;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", server.url);
    for t in server.tilesets() {
        let _ = writeln!(out, "{}: {}{}.json", t.name, server.url, t.name);
    }
    let _ = out.flush();
    drop(out);
    if o.open {
        open_browser(&server.url);
    }
    server.run();
    Ok(())
}

fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(program)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        eprintln!("manby serve: can't open {} with {}: {}", url, program, e);
    }
}

fn work(http: &tiny_http::Server, tilesets: &[Tileset]) {
    while let Ok(request) = http.recv() {
        let response = respond(&request, tilesets);
        let _ = request.respond(response);
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("a valid header")
}

fn request_header<'a>(r: &'a Request, name: &str) -> Option<&'a str> {
    r.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn text(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> ResponseBox {
    Response::from_data(body.into())
        .with_status_code(status)
        .with_chunked_threshold(usize::MAX)
        .with_header(header("Content-Type", content_type))
        .boxed()
}

fn empty(status: u16) -> ResponseBox {
    Response::empty(status).boxed()
}

/// The response to a request, with the headers every response has.
pub fn respond(request: &Request, tilesets: &[Tileset]) -> ResponseBox {
    let mut response = match request.method() {
        Method::Options => empty(204)
            .with_header(header("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS"))
            .with_header(header("Access-Control-Allow-Headers", "*"))
            .with_header(header("Access-Control-Max-Age", "86400")),
        Method::Get | Method::Head => route(request, tilesets),
        _ => empty(405).with_header(header("Allow", "GET, HEAD, OPTIONS")),
    };
    for h in [
        header("Access-Control-Allow-Origin", "*"),
        header(
            "Access-Control-Expose-Headers",
            "ETag, Content-Range, Content-Length, Accept-Ranges",
        ),
        header("Cache-Control", "no-cache"),
    ] {
        response.add_header(h);
    }
    response
}

fn route(request: &Request, tilesets: &[Tileset]) -> ResponseBox {
    let url = request.url();
    let path = percent_decode(url.split(['?', '#']).next().unwrap_or(""));
    let path = path.trim_start_matches('/');
    if path.is_empty() || path == "index.html" {
        return text(200, "text/html; charset=utf-8", VIEWER);
    }
    if path == "favicon.ico" {
        return empty(204);
    }
    let base = format!(
        "http://{}",
        request_header(request, "Host").unwrap_or("localhost")
    );
    if path == "tilesets.json" {
        return text(200, "application/json", list(tilesets));
    }
    if let Some(name) = path.strip_suffix(".json")
        && let Some(t) = tilesets.iter().find(|t| t.name == name)
    {
        return match t.opened() {
            Ok(o) => text(200, "application/json", tilejson(t, &o, &base)),
            Err(e) => text(503, "text/plain; charset=utf-8", e),
        };
    }
    if let Some((name, rest)) = path.split_once('/')
        && let Some(t) = tilesets.iter().find(|t| t.name == name)
    {
        return tile(t, rest);
    }
    if let Some(t) = tilesets.iter().find(|t| {
        t.kind == Kind::Pmtiles
            && t.path
                .file_name()
                .is_some_and(|f| f.to_string_lossy() == path)
    }) {
        return archive(request, t);
    }
    text(404, "text/plain; charset=utf-8", "not found")
}

/// `z/x/y.ext`, as a tile.
fn tile(t: &Tileset, zxy: &str) -> ResponseBox {
    let mut parts = zxy.splitn(3, '/');
    let (Some(z), Some(x), Some(y)) = (parts.next(), parts.next(), parts.next()) else {
        return text(404, "text/plain; charset=utf-8", "not found");
    };
    let y = y.split('.').next().unwrap_or("");
    let (Ok(z), Ok(x), Ok(y)) = (z.parse::<u8>(), x.parse::<u32>(), y.parse::<u32>()) else {
        return text(404, "text/plain; charset=utf-8", "not found");
    };
    let o = match t.opened() {
        Ok(o) => o,
        Err(e) => return text(503, "text/plain; charset=utf-8", e),
    };
    match o.tile(z, x, y) {
        Ok(Some(data)) => {
            let gzipped = data.starts_with(&[0x1f, 0x8b]);
            let mut r = text(
                200,
                if o.meta.mlt {
                    "application/vnd.maplibre-vector-tile"
                } else {
                    "application/x-protobuf"
                },
                data,
            );
            if gzipped {
                r.add_header(header("Content-Encoding", "gzip"));
            }
            r
        }
        Ok(None) => empty(204),
        Err(e) => text(500, "text/plain; charset=utf-8", e),
    }
}

/// A PMTiles archive as itself, or the byte range asked for.
fn archive(request: &Request, t: &Tileset) -> ResponseBox {
    let opened = File::open(&t.path).and_then(|f| {
        let m = f.metadata()?;
        Ok((f, m.len()))
    });
    let (mut file, len) = match opened {
        Ok(f) => f,
        Err(e) => return text(500, "text/plain; charset=utf-8", e.to_string()),
    };
    let etag = format!(
        "\"{}\"",
        tileset::Stamp::of(&t.path)
            .map(|s| s.tag())
            .unwrap_or_default()
    );
    let range = request_header(request, "Range").map(|r| parse_range(r, len));
    let (status, start, n) = match range {
        None => (200, 0, len),
        Some(Some((start, end))) => (206, start, end - start + 1),
        Some(None) => {
            return empty(416).with_header(header("Content-Range", &format!("bytes */{}", len)));
        }
    };
    if file.seek(SeekFrom::Start(start)).is_err() {
        return empty(500);
    }
    let body: Box<dyn Read + Send> = Box::new(file.take(n));
    // a length rather than chunks, so a HEAD request gives the archive's size
    let mut r = Response::new(StatusCode(status), Vec::new(), body, Some(n as usize), None)
        .with_chunked_threshold(usize::MAX)
        .with_header(header("Content-Type", "application/octet-stream"))
        .with_header(header("Accept-Ranges", "bytes"))
        .with_header(header("ETag", &etag));
    if status == 206 {
        r.add_header(header(
            "Content-Range",
            &format!("bytes {}-{}/{}", start, start + n - 1, len),
        ));
    }
    r
}

/// A single range, `bytes=a-b`, `bytes=a-` or `bytes=-n`, as the first and
/// last byte, or None if it can't be served.
fn parse_range(range: &str, len: u64) -> Option<(u64, u64)> {
    let spec = range.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        (len.saturating_sub(n), len - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() {
            len - 1
        } else {
            b.parse::<u64>().ok()?.min(len - 1)
        };
        (start, end)
    };
    (start <= end && start < len).then_some((start, end))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
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
    out
}

fn number(v: f64) -> String {
    tippecanoe::milo::dtoa_milo(v)
}

/// The tilesets, for the map.
fn list(tilesets: &[Tileset]) -> String {
    let items: Vec<String> = tilesets
        .iter()
        .map(|t| {
            let state = match t.opened() {
                Ok(o) => format!(
                    "\"format\":\"{}\",\"version\":{}",
                    if o.meta.mlt { "mlt" } else { "mvt" },
                    json_string(&o.stamp.tag())
                ),
                Err(e) => format!("\"error\":{}", json_string(&e)),
            };
            format!(
                "{{\"name\":{},\"tilejson\":{},{}}}",
                json_string(&t.name),
                json_string(&format!("/{}.json", t.name)),
                state
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// The TileJSON 3.0.0 of a tileset, with tile URLs on `base`.
pub fn tilejson(t: &Tileset, o: &tileset::Opened, base: &str) -> String {
    let m = &o.meta;
    let mut s = String::from("{\"tilejson\":\"3.0.0\"");
    s += &format!(
        ",\"name\":{}",
        json_string(m.name.as_deref().unwrap_or(&t.name))
    );
    for (k, v) in [
        ("description", &m.description),
        ("attribution", &m.attribution),
    ] {
        if let Some(v) = v {
            s += &format!(",\"{}\":{}", k, json_string(v));
        }
    }
    let tiles = format!(
        "{}/{}/{{z}}/{{x}}/{{y}}.{}?v={}",
        base,
        t.name,
        if m.mlt { "mlt" } else { "mvt" },
        o.stamp.tag()
    );
    s += &format!(
        ",\"scheme\":\"xyz\",\"tiles\":[{}],\"minzoom\":{},\"maxzoom\":{}",
        json_string(&tiles),
        m.minzoom,
        m.maxzoom
    );
    if let Some(b) = m.bounds {
        s += &format!(",\"bounds\":[{}]", b.map(number).join(","));
    }
    if let Some(c) = m.center {
        s += &format!(",\"center\":[{}]", c.map(number).join(","));
    }
    if m.mlt {
        s += ",\"encoding\":\"mlt\"";
    }
    s += &format!(",\"vector_layers\":{}}}", m.vector_layers);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=990-2000", 1000), Some((990, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-9", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
    }

    #[test]
    fn args() {
        let a = |s: &[&str]| parse_args(&s.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let o = a(&[
            "manby serve",
            "--port=0",
            "--host",
            "0.0.0.0",
            "--open",
            "a.pmtiles",
            "b",
        ])
        .unwrap();
        assert_eq!((o.port, o.host.as_str(), o.open), (0, "0.0.0.0", true));
        assert_eq!(o.tilesets, ["a.pmtiles", "b"]);
        assert_eq!(a(&["manby serve", "-p", "9000", "x"]).unwrap().port, 9000);
        assert_eq!(a(&["manby serve"]).unwrap_err().exit_code, EXIT_ARGS);
        assert_eq!(
            a(&["manby serve", "--help"]).unwrap_err().exit_code,
            EXIT_SUCCESS
        );
        assert_eq!(
            a(&["manby serve", "--bad", "x"]).unwrap_err().exit_code,
            EXIT_ARGS
        );
    }

    #[test]
    fn decoding() {
        assert_eq!(percent_decode("a%20b.pmtiles"), "a b.pmtiles");
        assert_eq!(percent_decode("100%"), "100%");
    }
}
