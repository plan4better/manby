//! Point features from the lat and lon columns of a CSV file, as tippecanoe's
//! geocsv.cpp reads them: serially, all in its first segment.

use std::borrow::Cow;
use std::io::BufRead;

use tippecanoe::csv::{csv_dequote, csv_getline, csv_split, is_number};
use tippecanoe::errors::{EXIT_CSV, EXIT_UTF8};
use tippecanoe::geometry::{Draw, DrawVec, VT_MOVETO};
use tippecanoe::mvt::{MVT_DOUBLE, MVT_NULL, MVT_STRING};
use tippecanoe::options::P_EMPTY_CSV_COLUMNS;
use tippecanoe::serial::SerialVal;
use tippecanoe::text::check_utf8;

use super::chunk::{Chunk, MessageKind};
use super::serialize::{Config, InFeature, serialize_feature};
use crate::Error;

/// `s.c_str()` of a line: the text up to its first NUL.
fn c_str(s: &[u8]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..end]).into_owned()
}

/// `parse_geocsv`: reads `f` into `chunk`.
pub fn parse(
    f: &mut dyn BufRead,
    fname: &str,
    cfg: &Config<'_>,
    layername: &str,
    chunk: &mut Chunk,
) -> crate::Result<()> {
    let mut header: Vec<String> = Vec::new();
    let mut latcol: isize = -1;
    let mut loncol: isize = -1;

    let s = csv_getline(f);
    if !s.is_empty() {
        let err = check_utf8(&s);
        if !err.is_empty() {
            return Err(Error::new(EXIT_UTF8, format!("{}: {}", fname, err)));
        }

        header = csv_split(&c_str(&s));

        for (i, h) in header.iter_mut().enumerate() {
            *h = csv_dequote(std::mem::take(h));

            let lower = h.to_ascii_lowercase();

            if lower == "y" || lower == "lat" || lower.contains("latitude") {
                latcol = i as isize;
            }
            if lower == "x"
                || lower == "lon"
                || lower == "lng"
                || lower == "long"
                || lower.contains("longitude")
            {
                loncol = i as isize;
            }
        }
    }

    if latcol < 0 || loncol < 0 {
        return Err(Error::new(
            EXIT_CSV,
            format!("{}: Can't find \"lat\" and \"lon\" columns", fname),
        ));
    }
    let (latcol, loncol) = (latcol as usize, loncol as usize);

    let mut seq: usize = 0;
    loop {
        let s = csv_getline(f);
        if s.is_empty() {
            break;
        }

        let err = check_utf8(&s);
        if !err.is_empty() {
            return Err(Error::new(EXIT_UTF8, format!("{}: {}", fname, err)));
        }

        seq += 1;
        let mut line = csv_split(&c_str(&s));

        if line.len() != header.len() {
            return Err(Error::new(
                EXIT_CSV,
                format!(
                    "{}:{}: Mismatched column count: {} in line, {} in header",
                    fname,
                    seq + 1,
                    line.len(),
                    header.len()
                ),
            ));
        }

        if line[loncol].is_empty() || line[latcol].is_empty() {
            chunk.message(
                MessageKind::Once("csv null geometry"),
                None,
                format!(
                    "{}:{}: null geometry (additional not reported)",
                    fname,
                    seq + 1
                ),
            );
            continue;
        }
        let lon = tippecanoe::cstd::atof(&line[loncol]);
        let lat = tippecanoe::cstd::atof(&line[latcol]);

        let (x, y) = cfg.project.world(lon, lat, 32);
        let dv: DrawVec = vec![Draw::new(VT_MOVETO, x, y)];

        let mut full_keys: Vec<Cow<'_, str>> = Vec::new();
        let mut full_values: Vec<SerialVal> = Vec::new();

        for i in 0..line.len() {
            if i != latcol && i != loncol {
                line[i] = csv_dequote(std::mem::take(&mut line[i]));

                let mut sv = SerialVal::default();
                if is_number(&line[i]) {
                    sv.type_ = MVT_DOUBLE;
                } else if line[i].is_empty() && cfg.o.prevent.get(P_EMPTY_CSV_COLUMNS) {
                    sv.type_ = MVT_NULL;
                    line[i] = "null".to_string();
                } else {
                    sv.type_ = MVT_STRING;
                }
                sv.s = line[i].clone();

                full_keys.push(Cow::Borrowed(header[i].as_str()));
                full_values.push(sv);
            }
        }

        let mut sf = InFeature {
            t: 1, // POINT
            geometry: dv,
            has_id: false,
            id: 0,
            tippecanoe_minzoom: -1,
            tippecanoe_maxzoom: -1,
            full_keys,
            full_values,
        };

        serialize_feature(chunk, cfg, &mut sf, layername);
    }

    Ok(())
}
