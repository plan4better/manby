//! Coordinates in other coordinate reference systems, made longitude and
//! latitude with proj4rs by the PROJ strings of crs-defs
//! (<https://github.com/birkskyum/crs-defs>), which `definitions.txt.gz`
//! holds, a line per EPSG or ESRI code. `update.py` writes it from a release
//! of crs-defs. NOTICE.md has the terms of the EPSG and ESRI data.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, OnceLock};

use proj4rs::proj::Proj;
use tippecanoe::projection::PROJECTIONS;

static DEFINITIONS: &[u8] = include_bytes!("definitions.txt.gz");

fn table() -> &'static HashMap<String, String> {
    static TABLE: OnceLock<HashMap<String, String>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut text = String::new();
        flate2::read::GzDecoder::new(DEFINITIONS)
            .read_to_string(&mut text)
            .expect("the compiled-in definitions");
        text.lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(code, def)| (code.to_string(), def.to_string()))
            .collect()
    })
}

/// A CRS's code as `EPSG:25832`, from the ways it is written:
/// `epsg:25832`, `EPSG::25832`, `urn:ogc:def:crs:EPSG::25832`, or
/// `http://www.opengis.net/def/crs/EPSG/0/25832`.
pub fn normalize(name: &str) -> Option<String> {
    let name = name.trim();
    let (authority, code) = if let Some(rest) = name
        .strip_prefix("http://www.opengis.net/def/crs/")
        .or_else(|| name.strip_prefix("https://www.opengis.net/def/crs/"))
    {
        let mut parts = rest.split('/');
        let authority = parts.next()?;
        (authority, parts.next_back()?)
    } else if let Some(rest) = name.strip_prefix("urn:ogc:def:crs:") {
        let (authority, rest) = rest.split_once(':')?;
        (authority, rest.rsplit(':').next()?)
    } else {
        let (authority, code) = name.split_once(':')?;
        (authority, code.trim_start_matches(':'))
    };
    if code.is_empty() || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{}:{}", authority.to_ascii_uppercase(), code))
}

/// The PROJ string of a CRS that crs-defs has.
pub fn definition(name: &str) -> Option<&'static str> {
    table().get(&normalize(name)?).map(String::as_str)
}

/// The way from a CRS to longitude and latitude on WGS 84.
#[derive(Debug)]
pub struct Reprojection {
    pub code: String,
    from: Proj,
    to: Proj,
    degrees: bool,
}

impl Reprojection {
    pub fn new(name: &str) -> Result<Reprojection, String> {
        let code = normalize(name)
            .ok_or_else(|| format!("{} is not a CRS code such as EPSG:25832", name))?;
        let def =
            definition(&code).ok_or_else(|| format!("{} is not a CRS that manby knows", code))?;
        let from = Proj::from_proj_string(def).map_err(|e| format!("{}: {}", code, e))?;
        let to = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
            .map_err(|e| e.to_string())?;
        Ok(Reprojection {
            code,
            degrees: from.is_latlong(),
            from,
            to,
        })
    }

    /// The longitude and latitude of a point, or NaNs where the CRS has
    /// none for it.
    pub fn lonlat(&self, x: f64, y: f64) -> (f64, f64) {
        let mut p = if self.degrees {
            (x.to_radians(), y.to_radians())
        } else {
            (x, y)
        };
        match proj4rs::transform::transform(&self.from, &self.to, &mut p) {
            Ok(()) => (p.0.to_degrees(), p.1.to_degrees()),
            Err(_) => (f64::NAN, f64::NAN),
        }
    }
}

/// How a reader makes its coordinates tippecanoe's world coordinates: one of
/// tippecanoe's projections, or a CRS made longitude and latitude first.
#[derive(Clone, Debug)]
pub enum Project {
    Tippecanoe(fn(f64, f64, i32) -> (i64, i64)),
    Crs(Arc<Reprojection>),
}

impl Project {
    #[inline]
    pub fn world(&self, x: f64, y: f64, zoom: i32) -> (i64, i64) {
        match self {
            Project::Tippecanoe(f) => f(x, y, zoom),
            Project::Crs(r) => {
                let (lon, lat) = r.lonlat(x, y);
                (PROJECTIONS[0].project)(lon, lat, zoom)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        for name in [
            "EPSG:25832",
            "epsg:25832",
            "EPSG::25832",
            "urn:ogc:def:crs:EPSG::25832",
            "urn:ogc:def:crs:EPSG:9.9:25832",
            "http://www.opengis.net/def/crs/EPSG/0/25832",
        ] {
            assert_eq!(normalize(name).as_deref(), Some("EPSG:25832"), "{}", name);
        }
        assert_eq!(normalize("WGS 84"), None);
        assert!(
            definition("EPSG:28992")
                .unwrap()
                .starts_with("+proj=sterea")
        );
        assert!(definition("ESRI:54009").is_some());
        assert!(definition("EPSG:999999").is_none());
    }

    #[test]
    fn reprojects() {
        // against PROJ's cs2cs, within 20 cm: the origins of the Dutch and
        // Swiss grids, a point of UTM zone 32, and ETRS89
        for (code, x, y, lon, lat) in [
            ("EPSG:28992", 155000.0, 463000.0, 5.387203508, 52.155172301),
            ("EPSG:2056", 2600000.0, 1200000.0, 7.438632421, 46.951082772),
            ("EPSG:25832", 500000.0, 5500000.0, 9.0, 49.652542924),
            ("EPSG:4258", 11.4, 48.2, 11.4, 48.2),
        ] {
            let (a, b) = Reprojection::new(code).unwrap().lonlat(x, y);
            assert!(
                (a - lon).abs() < 2e-6 && (b - lat).abs() < 2e-6,
                "{}: {} {}",
                code,
                a,
                b
            );
        }
    }
}
