//! Everything a run needs to know, parsed from tippecanoe's command line options
//! or filled in by a library user. Replaces tippecanoe's global variables.
//!
//! [`parse_args`] follows the option loop of tippecanoe's main.cpp, and
//! [`Options::resolve`] the checks and adjustments main.cpp makes after it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use tippecanoe::attribute::AttributeOp;
use tippecanoe::errors::*;
use tippecanoe::geometry::ClipBbox;
use tippecanoe::getopt::{Getopt, LongOption, NO_ARGUMENT, REQUIRED_ARGUMENT};
use tippecanoe::globals::{MAX_ZOOM, ORDER_BY_SIZE, OrderField};
use tippecanoe::jsonpull::{JsonPull, JsonRef, JsonTree, JsonType};
use tippecanoe::mvt::{MVT_BOOL, MVT_DOUBLE, MVT_FLOAT, MVT_INT, MVT_NULL, MVT_STRING};
use tippecanoe::options::*;
use tippecanoe::projection::PROJECTIONS;
use tippecanoe::serial::SerialVal;
use tippecanoe::usage::{UsageRequiredOption, print_usage, strip_usage_headings};

use crate::Error;

/// Where the tiles go.
#[derive(Clone, Debug, Default)]
pub enum Output {
    #[default]
    None,
    Pmtiles(String),
    Mbtiles(String),
    Directory(String),
}

impl Output {
    /// The path given with `-o` or `-e`.
    pub fn path(&self) -> Option<&str> {
        match self {
            Output::None => None,
            Output::Pmtiles(p) | Output::Mbtiles(p) | Output::Directory(p) => Some(p),
        }
    }
}

/// tippecanoe's `prevent[]` or `additional[]` flags, indexed by the option
/// letters in `tippecanoe::options` (`P_*` and `A_*`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Flags(pub [bool; 256]);

impl Default for Flags {
    fn default() -> Self {
        Flags([false; 256])
    }
}

impl std::fmt::Debug for Flags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set: String = (0..256u32)
            .filter(|&c| self.0[c as usize])
            .filter_map(char::from_u32)
            .collect();
        write!(f, "Flags({:?})", set)
    }
}

impl Flags {
    pub fn get(&self, c: usize) -> bool {
        self.0[c]
    }

    pub fn set(&mut self, c: usize, v: bool) {
        self.0[c] = v;
    }
}

/// An input: tippecanoe's `struct source`. An empty `file` is standard input.
#[derive(Clone, Debug, Default)]
pub struct Source {
    pub layer: String,
    pub file: String,
    pub description: String,
    pub format: String,
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Input files named on the command line, after the `-L` sources.
    pub inputs: Vec<String>,
    pub output: Output,
    pub force: bool,
    pub quiet: bool,
    /// Worker threads; 0 means one per available CPU, as tippecanoe chooses.
    pub threads: usize,
    /// `--memory-budget`: the bytes of memory to stay near; what does not
    /// fit goes to temporary files. None means half the physical memory, as
    /// tippecanoe sizes its sorting.
    pub memory_budget: Option<u64>,
    /// Make different tiles from tippecanoe's where that is faster. See DESIGN.md.
    pub fast: bool,
    /// `--as-needed-per-tile`: a tile too big keeps the thresholds it raised
    /// for itself, rather than its zoom level being made again with the
    /// highest. Different tiles from tippecanoe's.
    pub as_needed_per_tile: bool,
    /// `--compression=zlib`: store every tile as zlib compresses it, so the
    /// archive is byte for byte tippecanoe's. Otherwise tiles that fit
    /// whatever zlib does are stored compressed with libdeflate.
    pub zlib_tiles: bool,
    /// `--tile-format`: MVT, as tippecanoe writes, or MapLibre Tiles.
    pub tile_format: crate::mlt::TileFormat,
    /// `--mlt-profile`: which encodings MLT tiles may use.
    pub mlt_profile: crate::mlt::MltProfile,
    /// `--bbox`: only the rows of GeoParquet and Arrow inputs whose
    /// geometries' bounding boxes overlap this longitude and latitude box,
    /// as minimum longitude, minimum latitude, maximum longitude, maximum
    /// latitude.
    pub bbox: Option<[f64; 4]>,

    /// `argv[0]`, for messages.
    pub program: String,
    /// The command line, for the metadata's `generator_options`.
    pub commandline: String,

    /// `-L` sources, which come before `inputs`.
    pub sources: Vec<Source>,
    /// `-l`: the layer name for every input.
    pub layer_name: Option<String>,
    /// `-n`
    pub name: Option<String>,
    /// `-N`
    pub description: Option<String>,
    /// `-A`
    pub attribution: Option<String>,
    /// `-P`
    pub read_parallel: bool,
    /// Index into `tippecanoe::projection::PROJECTIONS` (`-s`).
    pub projection: usize,
    /// `-s` with a CRS that tippecanoe doesn't have, such as `EPSG:25832`,
    /// which crs-defs defines: the input's coordinates are made longitude
    /// and latitude first.
    pub source_crs: Option<String>,
    /// `--h3`: below `--h3-base-zoom`, each layer's points binned into H3
    /// hexagons in a layer of their own; `--h3-categories` counts the
    /// values of an attribute in each.
    pub h3: bool,
    pub h3_base_zoom: Option<i32>,
    pub h3_categories: Option<String>,
    /// `--cluster-categories=kind`, or `kind:park,museum`: each feature
    /// gets `kind:<value>` of 1, which clustering and coalescing add up, so
    /// a cluster counts the values of its points; values not listed count
    /// as `kind:_other`.
    pub cluster_categories: Option<(String, Option<Vec<String>>)>,

    pub maxzoom: i32,
    pub minzoom: i32,
    /// `-zg` or `--smallest-maximum-zoom-guess`
    pub guess_maxzoom: bool,
    pub minimum_maxzoom: i32,
    pub basezoom: i32,
    pub basezoom_marker_width: f64,
    pub droprate: f64,
    pub gamma: f64,
    pub buffer: i32,
    /// `-R z/x/y`: the x and y of the one tile.
    pub one_tile: Option<(u32, u32)>,

    pub full_detail: i32,
    pub low_detail: i32,
    pub min_detail: i32,
    pub extra_detail: i32,

    pub exclude: BTreeSet<String>,
    pub include: BTreeSet<String>,
    pub exclude_all: bool,
    pub attribute_types: HashMap<String, i32>,
    pub attribute_accum: HashMap<String, AttributeOp>,
    pub attribute_descriptions: BTreeMap<String, String>,
    pub set_attributes: BTreeMap<String, SerialVal>,
    pub attribute_for_id: String,
    pub maximum_string_attribute_length: usize,

    /// `-J` or `-j`
    pub filter: Option<JsonTree>,
    pub prefilter: Option<String>,
    pub postfilter: Option<String>,
    pub tmpdir: String,

    pub prevent: Flags,
    pub additional: Flags,

    pub simplification: f64,
    pub maxzoom_simplification: f64,
    pub tiny_polygon_size: i32,

    pub max_tile_size: usize,
    pub max_tile_features: usize,
    pub limit_tile_feature_count: usize,
    pub limit_tile_feature_count_at_maxzoom: usize,

    pub cluster_distance: i32,
    pub cluster_maxzoom: i32,
    pub guess_cluster_maxzoom: bool,
    pub retain_points_multiplier: i32,
    pub drop_denser: u32,
    pub preserve_point_density_threshold: u64,
    pub preserve_multiplier_density_threshold: u64,
    pub extend_zooms_max: i64,

    pub order_by: Vec<OrderField>,
    pub order_by_size: bool,
    pub clip_bboxes: Vec<ClipBbox>,
    pub unidecode_data: Vec<String>,
    pub drop_by_attribute_as_needed_attribute: String,
    pub drop_by_attribute_descending: bool,

    pub max_tilestats_attributes: usize,
    pub max_tilestats_sample_values: usize,
    pub max_tilestats_values: usize,

    pub quiet_progress: bool,
    pub progress_interval: f64,
    pub json_progress: bool,
    /// `-F`
    pub forcetable: bool,

    /// How many segments tippecanoe splits parallel input into, its `CPUS`.
    /// It decides tippecanoe's sequence numbers, which `--generate-ids`
    /// turns into feature ids. 0 means as tippecanoe would choose it.
    pub tippecanoe_cpus: usize,

    /// Set by [`Options::resolve`]: tippecanoe's `geometry_scale`.
    pub geometry_scale: i32,

    /// The size of the ranges the input is split into for parallel reading;
    /// 0 chooses it from the input size and the number of threads.
    pub chunk_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            inputs: Vec::new(),
            output: Output::None,
            force: false,
            quiet: false,
            threads: 0,
            memory_budget: None,
            fast: false,
            as_needed_per_tile: false,
            zlib_tiles: false,
            tile_format: crate::mlt::TileFormat::Mvt,
            mlt_profile: crate::mlt::MltProfile::Web,
            bbox: None,
            program: "manby".to_string(),
            commandline: String::new(),
            sources: Vec::new(),
            layer_name: None,
            name: None,
            description: None,
            attribution: None,
            read_parallel: false,
            projection: 0,
            source_crs: None,
            h3: false,
            h3_base_zoom: None,
            h3_categories: None,
            cluster_categories: None,
            maxzoom: 14,
            minzoom: 0,
            guess_maxzoom: false,
            minimum_maxzoom: 0,
            basezoom: -1,
            basezoom_marker_width: 1.0,
            droprate: 2.5,
            gamma: 0.0,
            buffer: 5,
            one_tile: None,
            full_detail: -1,
            low_detail: 12,
            min_detail: 7,
            extra_detail: -1,
            exclude: BTreeSet::new(),
            include: BTreeSet::new(),
            exclude_all: false,
            attribute_types: HashMap::new(),
            attribute_accum: HashMap::new(),
            attribute_descriptions: BTreeMap::new(),
            set_attributes: BTreeMap::new(),
            attribute_for_id: String::new(),
            maximum_string_attribute_length: 0,
            filter: None,
            prefilter: None,
            postfilter: None,
            tmpdir: "/tmp".to_string(),
            prevent: Flags::default(),
            additional: Flags::default(),
            simplification: 1.0,
            maxzoom_simplification: -1.0,
            tiny_polygon_size: 2,
            max_tile_size: 500000,
            max_tile_features: 200000,
            limit_tile_feature_count: 0,
            limit_tile_feature_count_at_maxzoom: 0,
            cluster_distance: 0,
            cluster_maxzoom: MAX_ZOOM,
            guess_cluster_maxzoom: false,
            retain_points_multiplier: 1,
            drop_denser: 0,
            preserve_point_density_threshold: 0,
            preserve_multiplier_density_threshold: 0,
            extend_zooms_max: 0,
            order_by: Vec::new(),
            order_by_size: false,
            clip_bboxes: Vec::new(),
            unidecode_data: Vec::new(),
            drop_by_attribute_as_needed_attribute: String::new(),
            drop_by_attribute_descending: false,
            max_tilestats_attributes: 1000,
            max_tilestats_sample_values: 1000,
            max_tilestats_values: 100,
            quiet_progress: false,
            progress_interval: 0.0,
            json_progress: false,
            forcetable: false,
            tippecanoe_cpus: 0,
            geometry_scale: 0,
            chunk_bytes: 0,
        }
    }
}

/// A size in bytes, with an optional K, M, G or T for powers of 1024.
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, shift) = match s.char_indices().last()? {
        (i, 'k' | 'K') => (&s[..i], 10),
        (i, 'm' | 'M') => (&s[..i], 20),
        (i, 'g' | 'G') => (&s[..i], 30),
        (i, 't' | 'T') => (&s[..i], 40),
        _ => (s, 0),
    };
    let n: f64 = num.parse().ok()?;
    (n.is_finite() && n >= 0.0).then(|| (n * (1u64 << shift) as f64) as u64)
}

impl Options {
    /// The memory budget in bytes.
    /// How the input's coordinates become world coordinates (`-s`).
    pub fn project(&self) -> crate::crs::Project {
        match &self.source_crs {
            Some(code) => crate::crs::Project::Crs(std::sync::Arc::new(
                crate::crs::Reprojection::new(code).expect("a CRS checked when parsed"),
            )),
            None => crate::crs::Project::Tippecanoe(PROJECTIONS[self.projection].project),
        }
    }

    /// The name of the input's CRS that a GeoJSON file's `crs` is expected
    /// to give.
    pub fn projection_alias(&self) -> String {
        match &self.source_crs {
            Some(code) => format!("urn:ogc:def:crs:{}", code.replacen(':', "::", 1)),
            None => PROJECTIONS[self.projection].alias.to_string(),
        }
    }

    /// `--h3`'s base zoom: given, or two zooms below the maximum.
    pub fn h3_base(&self) -> i32 {
        self.h3_base_zoom
            .unwrap_or(self.maxzoom - 2)
            .clamp(self.minzoom, self.maxzoom.max(self.minzoom))
    }

    pub fn memory_budget(&self) -> u64 {
        self.memory_budget
            .unwrap_or_else(|| tippecanoe::platform::calc_memsize() as u64 / 2)
    }

    /// The sources in tippecanoe's order: the `-L` sources, then the input
    /// files, or standard input if there are neither, with `-l` applied.
    pub fn all_sources(&self) -> Vec<Source> {
        let mut sources = self.sources.clone();
        for f in &self.inputs {
            sources.push(Source {
                file: f.clone(),
                ..Default::default()
            });
        }
        if sources.is_empty() {
            sources.push(Source::default());
        }
        if let Some(l) = &self.layer_name {
            for s in sources.iter_mut() {
                s.layer = l.clone();
            }
        }
        sources
    }

    /// The name tippecanoe gives the tileset: `-n`, or else the output path.
    pub fn tileset_name(&self) -> String {
        match (&self.name, self.output.path()) {
            (Some(n), _) => n.clone(),
            (None, Some(p)) => p.to_string(),
            (None, None) => String::new(),
        }
    }

    /// Whether `-C` or `-c` is in use, which moves the layer counts to tiling time.
    pub fn filters(&self) -> bool {
        self.prefilter.is_some() || self.postfilter.is_some()
    }

    /// The number of worker threads to use.
    pub fn thread_count(&self) -> usize {
        if self.threads > 0 {
            self.threads
        } else {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        }
    }

    /// The checks and adjustments tippecanoe's main.cpp makes once the options
    /// are parsed. Returns the warnings tippecanoe prints along the way.
    /// Running it more than once changes nothing more.
    pub fn resolve(&mut self) -> crate::Result<Vec<String>> {
        let mut warnings = Vec::new();
        let argv0 = self.program.clone();

        if self.tippecanoe_cpus == 0 {
            self.tippecanoe_cpus = tippecanoe_cpus()?;
        }

        if self.h3 && self.guess_maxzoom && self.h3_base_zoom.is_none() {
            return Err(Error::new(
                EXIT_ARGS,
                format!(
                    "{}: --h3 with -zg needs --h3-base-zoom, the zoom from which the points show",
                    argv0
                ),
            ));
        }

        // Wait until here to project the bounding box, so that the behavior is
        // the same no matter what order the projection and bounding box are
        // specified in
        let project = self.project();
        for c in self.clip_bboxes.iter_mut() {
            (c.minx, c.maxy) = project.world(c.lon1, c.lat1, 32);
            (c.maxx, c.miny) = project.world(c.lon2, c.lat2, 32);
        }

        if self.max_tilestats_sample_values < self.max_tilestats_values {
            self.max_tilestats_sample_values = self.max_tilestats_values;
        }

        if self.full_detail <= 0 {
            self.full_detail = 12;
        }

        if self.droprate == -3.0 && !self.guess_maxzoom {
            return Err(Error::new(
                EXIT_ARGS,
                "Can't use -rp without either -zg or --smallest-maximum-zoom-guess",
            ));
        }

        if self.maxzoom > MAX_ZOOM {
            self.maxzoom = MAX_ZOOM;
            warnings.push(format!("Highest supported zoom is -z{}", self.maxzoom));
        }

        // This previously dropped the maxzoom rather than the detail when they were in conflict,
        // which proved to be annoying.
        if !self.guess_maxzoom {
            if self.maxzoom > 32 - self.full_detail {
                self.full_detail = 32 - self.maxzoom;
                warnings.push(format!(
                    "Highest supported detail with maxzoom {} is {}",
                    self.maxzoom, self.full_detail
                ));
            }
            if self.maxzoom > 33 - self.low_detail {
                // that is, maxzoom - 1 > 32 - low_detail
                self.low_detail = 33 - self.maxzoom;
                warnings.push(format!(
                    "Highest supported low detail with maxzoom {} is {}",
                    self.maxzoom, self.low_detail
                ));
            }
        }
        if self.minzoom > self.maxzoom {
            return Err(Error::new(
                EXIT_ARGS,
                format!(
                    "{}: Minimum zoom -Z{} cannot be greater than maxzoom -z{}",
                    argv0, self.minzoom, self.maxzoom
                ),
            ));
        }

        if self.full_detail < self.min_detail {
            self.min_detail = self.full_detail;
            warnings.push(format!(
                "{}: Reducing minimum detail to match full detail {}",
                argv0, self.min_detail
            ));
        }

        if self.low_detail < self.min_detail {
            self.min_detail = self.low_detail;
            warnings.push(format!(
                "{}: Reducing minimum detail to match low detail {}",
                argv0, self.min_detail
            ));
        }

        if self.basezoom == -1 && !self.guess_maxzoom {
            // basezoom unspecified
            self.basezoom = self.maxzoom;
        }

        if self.extra_detail >= 0
            || self.prevent.get(P_SIMPLIFY_SHARED_NODES)
            || self.additional.get(A_EXTEND_ZOOMS)
            || self.extend_zooms_max > 0
        {
            self.geometry_scale = 0;
        } else {
            self.geometry_scale = 32 - (self.full_detail + self.maxzoom);
            if self.geometry_scale < 0 {
                self.geometry_scale = 0;
                if !self.guess_maxzoom {
                    // This shouldn't be able to happen any more. Can it still?
                    warnings.push(
                        "Full detail + maxzoom > 32, so you are asking for more detail than is available."
                            .to_string(),
                    );
                }
            }
        }

        if (self.basezoom < 0 || self.droprate < 0.0) && self.gamma < 0.0 {
            // Can't use randomized (as opposed to evenly distributed) dot dropping
            // if rate and base aren't known during feature reading.
            self.gamma = 0.0;
            warnings.push("Forcing -g0 since -B or -r is not known".to_string());
        }

        if let Output::None = self.output {
            return Err(Error::new(
                EXIT_ARGS,
                format!("{}: must specify -o out.mbtiles or -e directory", argv0),
            ));
        }

        if self.additional.get(A_DROP_BY_ATTRIBUTE_AS_NEEDED)
            && self.drop_by_attribute_as_needed_attribute.is_empty()
        {
            return Err(Error::new(
                EXIT_ARGS,
                format!(
                    "{}: --drop-by-attribute-as-needed requires an attribute name",
                    argv0
                ),
            ));
        }

        Ok(warnings)
    }
}

/// tippecanoe's `CPUS`: `TIPPECANOE_MAX_THREADS` or the number of CPUs,
/// rounded down to a power of 2.
pub fn tippecanoe_cpus() -> crate::Result<usize> {
    let cpus: usize = match std::env::var("TIPPECANOE_MAX_THREADS") {
        Ok(v) => require_int(&v, "TIPPECANOE_MAX_THREADS", "tippecanoe")? as usize,
        Err(_) => tippecanoe::platform::get_num_avail_cpus() as usize,
    };
    // At least 1, and guard against short struct index.segment
    let cpus = cpus.clamp(1, 32767);
    // Round down to a power of 2
    Ok(1 << ((cpus as f64).ln() / 2f64.ln()) as i32)
}

/// Sets tippecanoe-rs's globals from `options`.
///
/// TRANSITIONAL: the algorithms manby borrows from tippecanoe-rs read these
/// globals. This is the one place manby writes them, once per run, before any
/// borrowed function is called. Each borrowed function that manby replaces
/// with a parameterized copy removes a reason for this function to exist.
pub fn set_tippecanoe_globals(options: &Options, cluster_maxzoom: i32) {
    use std::sync::atomic::Ordering::Relaxed;
    use tippecanoe::globals as g;

    for c in 0..256 {
        g::set_prevent(c, options.prevent.get(c) as i32);
        g::set_additional(c, options.additional.get(c) as i32);
    }
    tippecanoe::projection::set_projection_or_exit(PROJECTIONS[options.projection].name);
    tippecanoe::projection::set_index_hilbert(options.additional.get(A_HILBERT));

    g::GEOMETRY_SCALE.store(options.geometry_scale, Relaxed);
    g::QUIET.store(options.quiet as i32, Relaxed);
    g::QUIET_PROGRESS.store(options.quiet_progress as i32, Relaxed);
    g::LOGGER.json_enabled.store(options.json_progress, Relaxed);
    g::PROGRESS_INTERVAL.store(options.progress_interval);
    g::EXTRA_DETAIL.store(options.extra_detail, Relaxed);
    let cpus = options.tippecanoe_cpus.max(1);
    g::CPUS.store(cpus, Relaxed);
    g::TEMP_FILES.store(cpus * 4, Relaxed);
    g::MEMSIZE.store(tippecanoe::platform::calc_memsize(), Relaxed);
    g::MAX_TILE_SIZE.store(options.max_tile_size, Relaxed);
    g::MAX_TILE_FEATURES.store(options.max_tile_features, Relaxed);
    g::CLUSTER_DISTANCE.store(options.cluster_distance, Relaxed);
    g::CLUSTER_MAXZOOM.store(cluster_maxzoom, Relaxed);
    *g::ATTRIBUTE_FOR_ID.write().unwrap() = options.attribute_for_id.clone();
    g::TINY_POLYGON_SIZE.store(options.tiny_polygon_size, Relaxed);
    g::LIMIT_TILE_FEATURE_COUNT.store(options.limit_tile_feature_count, Relaxed);
    g::LIMIT_TILE_FEATURE_COUNT_AT_MAXZOOM
        .store(options.limit_tile_feature_count_at_maxzoom, Relaxed);
    *g::SET_ATTRIBUTES.write().unwrap() = options.set_attributes.clone();
    g::EXTEND_ZOOMS_MAX.store(options.extend_zooms_max, Relaxed);
    g::RETAIN_POINTS_MULTIPLIER.store(options.retain_points_multiplier, Relaxed);
    g::MAXIMUM_STRING_ATTRIBUTE_LENGTH.store(options.maximum_string_attribute_length, Relaxed);
    g::PRESERVE_MULTIPLIER_DENSITY_THRESHOLD
        .store(options.preserve_multiplier_density_threshold, Relaxed);
    g::MAX_TILESTATS_ATTRIBUTES.store(options.max_tilestats_attributes, Relaxed);
    g::MAX_TILESTATS_SAMPLE_VALUES.store(options.max_tilestats_sample_values, Relaxed);
    g::MAX_TILESTATS_VALUES.store(options.max_tilestats_values, Relaxed);
    *g::ORDER_BY.write().unwrap() = options.order_by.clone();
    g::ORDER_BY_SIZE_ENABLED.store(options.order_by_size, Relaxed);
    *g::CLIPBBOXES.write().unwrap() = options.clip_bboxes.clone();
}

/// The length of the C string `s` would be: up to its first NUL.
fn cstr(s: &str) -> &str {
    match s.find('\0') {
        Some(n) => &s[..n],
        None => s,
    }
}

fn not_a_number(argv0: &str, what: &str, s: &str) -> Error {
    Error::new(
        EXIT_ARGS,
        format!("{}: {} must be a number (got {})", argv0, what, cstr(s)),
    )
}

/// `atoi_require`
fn require_int(s: &str, what: &str, argv0: &str) -> crate::Result<i32> {
    Ok(require_long(s, what, argv0)? as i32)
}

/// `atoll_require`
fn require_long(s: &str, what: &str, argv0: &str) -> crate::Result<i64> {
    let s1 = cstr(s);
    if s1.is_empty() {
        return Err(not_a_number(argv0, what, s));
    }
    let (ret, consumed) = tippecanoe::cstd::strtoll(s1, 10);
    if consumed < s1.len() {
        return Err(not_a_number(argv0, what, s));
    }
    Ok(ret)
}

/// `atof_require`
fn require_double(s: &str, what: &str, argv0: &str) -> crate::Result<f64> {
    let s1 = cstr(s);
    if s1.is_empty() {
        return Err(not_a_number(argv0, what, s));
    }
    let (ret, consumed) = tippecanoe::cstd::strtod(s1);
    if consumed < s1.len() {
        return Err(not_a_number(argv0, what, s));
    }
    Ok(ret)
}

/// `stringify_value` for a value given on the command line.
fn command_line_value(value: JsonRef<'_>, what: &str) -> crate::Result<SerialVal> {
    let (type_, s) = match value.type_() {
        JsonType::String => {
            let err = tippecanoe::text::check_utf8(value.raw_string_bytes());
            if !err.is_empty() {
                return Err(Error::new(EXIT_UTF8, format!("{}:1: {}", what, err)));
            }
            (MVT_STRING, value.string().to_string())
        }
        JsonType::Number => {
            let s = if value.large_unsigned() != 0 {
                value.large_unsigned().to_string()
            } else if value.large_signed() != 0 {
                value.large_signed().to_string()
            } else {
                tippecanoe::milo::dtoa_milo(value.number())
            };
            (MVT_DOUBLE, s)
        }
        JsonType::True => (MVT_BOOL, "true".to_string()),
        JsonType::False => (MVT_BOOL, "false".to_string()),
        JsonType::Null => (MVT_NULL, "null".to_string()),
        _ => (MVT_STRING, value.stringify()),
    };
    Ok(SerialVal { type_, s })
}

fn set_attribute_type(attribute_types: &mut HashMap<String, i32>, arg: &str) -> crate::Result<()> {
    let arg = cstr(arg);
    let s = arg.find(':').ok_or_else(|| {
        Error::new(
            EXIT_ARGS,
            format!("-T{} option must be in the form -Tname:type", arg),
        )
    })?;

    let name = arg[..s].to_string();
    let t = match &arg[s + 1..] {
        "int" => MVT_INT,
        "float" => MVT_FLOAT,
        "string" => MVT_STRING,
        "bool" => MVT_BOOL,
        other => {
            return Err(Error::new(
                EXIT_ARGS,
                format!(
                    "Attribute type ({}) must be int, float, string, or bool",
                    other
                ),
            ));
        }
    };

    attribute_types.entry(name).or_insert(t);
    Ok(())
}

fn set_attribute_value(
    set_attributes: &mut BTreeMap<String, SerialVal>,
    arg: &str,
    argv0: &str,
) -> crate::Result<()> {
    let arg = cstr(arg);
    if arg.starts_with('{') {
        let mut jp = JsonPull::begin_string(arg.as_bytes());
        let o = jp.read_tree().ok_or_else(|| {
            Error::new(
                EXIT_JSON,
                format!(
                    "{}: --set-attribute {}: {}",
                    argv0,
                    arg,
                    jp.error.unwrap_or("(null)")
                ),
            )
        })?;

        let root = o.root();
        if root.type_() != JsonType::Hash {
            return Err(Error::new(
                EXIT_JSON,
                format!("{}: --set-attribute {}: not a JSON object", argv0, arg),
            ));
        }

        for (i, (key, value)) in root.entries_iter().enumerate() {
            if key.type_() != JsonType::String {
                return Err(Error::new(
                    EXIT_JSON,
                    format!("{}: --set-attribute {}: key {} not a string", argv0, arg, i),
                ));
            }
            let val = command_line_value(value, "json")?;
            set_attributes
                .entry(key.string().to_string())
                .or_insert(val);
        }

        return Ok(());
    }

    let s = arg.find(':').ok_or_else(|| {
        Error::new(
            EXIT_ARGS,
            format!(
                "--set-attribute {} option must be in the form --set-attribute name:value",
                arg
            ),
        )
    })?;

    let value = arg[s + 1..].to_string();
    let first = value.bytes().next().unwrap_or(0);
    let type_ = if first.is_ascii_digit() || first == b'-' {
        MVT_DOUBLE
    } else {
        MVT_STRING
    };
    set_attributes
        .entry(arg[..s].to_string())
        .or_insert(SerialVal { type_, s: value });
    Ok(())
}

fn attribute_op(type_: &str) -> crate::Result<AttributeOp> {
    Ok(match type_ {
        "sum" => AttributeOp::Sum,
        "product" => AttributeOp::Product,
        "mean" => AttributeOp::Mean,
        "max" => AttributeOp::Max,
        "min" => AttributeOp::Min,
        "concat" => AttributeOp::Concat,
        "comma" => AttributeOp::Comma,
        "count" => AttributeOp::Count,
        _ => {
            return Err(Error::new(
                EXIT_ARGS,
                format!(
                    "Attribute method ({}) must be sum, product, mean, max, min, concat, comma, or count",
                    type_
                ),
            ));
        }
    })
}

/// `set_attribute_accum` for `-E`.
fn set_attribute_accum(
    attribute_accum: &mut HashMap<String, AttributeOp>,
    arg: &str,
    argv0: &str,
) -> crate::Result<()> {
    if arg.starts_with('{') {
        let mut jp = JsonPull::begin_string(arg.as_bytes());
        let o = jp.read_tree().ok_or_else(|| {
            Error::new(
                EXIT_JSON,
                format!("{}: -E{}: {}", argv0, arg, jp.error.unwrap_or("(null)")),
            )
        })?;
        let root = o.root();
        if root.type_() != JsonType::Hash {
            return Err(Error::new(
                EXIT_JSON,
                format!("{}: -E{}: not a JSON object", argv0, arg),
            ));
        }
        for (i, (key, value)) in root.entries_iter().enumerate() {
            if key.type_() != JsonType::String {
                return Err(Error::new(
                    EXIT_JSON,
                    format!("{}: -E{}: key {} not a string", argv0, arg, i),
                ));
            }
            if value.type_() != JsonType::String {
                return Err(Error::new(
                    EXIT_JSON,
                    format!("{}: -E{}: value {} not a string", argv0, arg, i),
                ));
            }
            let op = attribute_op(value.string())?;
            attribute_accum
                .entry(key.string().to_string())
                .or_insert(op);
        }
        return Ok(());
    }

    let s = arg.find(':').ok_or_else(|| {
        Error::new(
            EXIT_ARGS,
            format!("-E{} option must be in the form -Ename:method", cstr(arg)),
        )
    })?;
    let op = attribute_op(&arg[s + 1..])?;
    attribute_accum.entry(arg[..s].to_string()).or_insert(op);
    Ok(())
}

fn parse_json_source(arg: &str, src: &mut Source, argv0: &str) -> crate::Result<()> {
    let arg = cstr(arg);
    let mut jp = JsonPull::begin_string(arg.as_bytes());
    let o = jp.read_tree().ok_or_else(|| {
        Error::new(
            EXIT_JSON,
            format!("{}: -L{}: {}", argv0, arg, jp.error.unwrap_or("(null)")),
        )
    })?;

    let root = o.root();
    if root.type_() != JsonType::Hash {
        return Err(Error::new(
            EXIT_JSON,
            format!("{}: -L{}: not a JSON object", argv0, arg),
        ));
    }

    match root.hash_get("file") {
        Some(f) if f.type_() == JsonType::String => src.file = f.string().to_string(),
        _ => {
            return Err(Error::new(
                EXIT_JSON,
                format!("{}: -L{}: requires \"file\": filename", argv0, arg),
            ));
        }
    }
    let string = |key: &str| {
        root.hash_get(key)
            .filter(|v| v.type_() == JsonType::String)
            .map(|v| v.string().to_string())
    };
    if let Some(l) = string("layer") {
        src.layer = l;
    }
    if let Some(d) = string("description") {
        src.description = d;
    }
    if let Some(f) = string("format") {
        src.format = f;
    }
    Ok(())
}

/// A filter given with `-j` or read with `-J`.
fn parse_filter(s: &str) -> crate::Result<JsonTree> {
    let mut jp = JsonPull::begin_string(s.as_bytes());
    jp.read_tree().ok_or_else(|| {
        Error::new(
            EXIT_JSON,
            format!(
                "Could not parse filter {}\n{}",
                s,
                jp.error.unwrap_or("(null)")
            ),
        )
    })
}

fn read_filter(fname: &str) -> crate::Result<JsonTree> {
    let data = std::fs::read(fname)
        .map_err(|e| Error::new(EXIT_OPEN, format!("{}: {}", fname, os_message(&e))))?;
    let mut jp = JsonPull::begin_string(&data);
    jp.read_tree().ok_or_else(|| {
        Error::new(
            EXIT_JSON,
            format!("{}: {}", fname, jp.error.unwrap_or("(null)")),
        )
    })
}

/// What `perror` would print after the colon for `e`.
pub fn os_message(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(n) => tippecanoe::cstd::strerror(n),
        None => e.to_string(),
    }
}

/// libc `sscanf` with only numeric conversions, as main.cpp uses it.
fn sscanf_f64x4(s: &str) -> Option<[f64; 4]> {
    let cs = std::ffi::CString::new(cstr(s)).ok()?;
    let mut v = [0.0f64; 4];
    let [a, b, c, d] = &mut v;
    // SAFETY: the format has four %lf conversions, each given a pointer to an f64.
    let n = unsafe {
        libc::sscanf(
            cs.as_ptr(),
            c"%lf,%lf,%lf,%lf".as_ptr(),
            a as *mut f64,
            b as *mut f64,
            c as *mut f64,
            d as *mut f64,
        )
    };
    (n == 4).then_some(v)
}

fn sscanf_u32x3(s: &str) -> Option<[u32; 3]> {
    let cs = std::ffi::CString::new(cstr(s)).ok()?;
    let mut v = [0u32; 3];
    let [a, b, c] = &mut v;
    // SAFETY: the format has three %u conversions, each given a pointer to a u32.
    let n = unsafe {
        libc::sscanf(
            cs.as_ptr(),
            c"%u/%u/%u".as_ptr(),
            a as *mut u32,
            b as *mut u32,
            c as *mut u32,
        )
    };
    (n == 3).then_some(v)
}

/// Option values for the flags that tippecanoe keeps in `additional[]` and
/// `prevent[]`, which it sets through `flag` pointers. manby keeps no globals,
/// so these come back from getopt as values instead.
const ADD: i32 = 0x100;
const PREV: i32 = 0x200;

const fn heading(name: &'static str) -> LongOption {
    LongOption {
        name,
        has_arg: 0,
        flag: None,
        val: 0,
    }
}

const fn opt(name: &'static str, has_arg: i32, val: u8) -> LongOption {
    LongOption {
        name,
        has_arg,
        flag: None,
        val: val as i32,
    }
}

const fn add(name: &'static str, c: usize) -> LongOption {
    LongOption {
        name,
        has_arg: NO_ARGUMENT,
        flag: None,
        val: ADD + c as i32,
    }
}

const fn prev(name: &'static str, c: usize) -> LongOption {
    LongOption {
        name,
        has_arg: NO_ARGUMENT,
        flag: None,
        val: PREV + c as i32,
    }
}

const REQ: i32 = REQUIRED_ARGUMENT;
const NO: i32 = NO_ARGUMENT;

// The options of tippecanoe's main.cpp, with tippecanoe's usage headings,
// and manby's own options at the end.

fn long_options_orig() -> Vec<LongOption> {
    vec![
        heading("Output tileset"),
        opt("output", REQ, b'o'),
        opt("output-to-directory", REQ, b'e'),
        opt("force", NO, b'f'),
        opt("allow-existing", NO, b'F'),
        heading("Tileset description and attribution"),
        opt("name", REQ, b'n'),
        opt("attribution", REQ, b'A'),
        opt("description", REQ, b'N'),
        heading("Input files and layer names"),
        opt("layer", REQ, b'l'),
        opt("named-layer", REQ, b'L'),
        heading("Parallel processing of input"),
        opt("read-parallel", NO, b'P'),
        heading("Projection of input"),
        opt("projection", REQ, b's'),
        heading("Zoom levels"),
        opt("maximum-zoom", REQ, b'z'),
        opt("minimum-zoom", REQ, b'Z'),
        opt("smallest-maximum-zoom-guess", REQ, b'~'),
        add("extend-zooms-if-still-dropping", A_EXTEND_ZOOMS),
        opt("extend-zooms-if-still-dropping-maximum", REQ, b'~'),
        add(
            "generate-variable-depth-tile-pyramid",
            A_VARIABLE_DEPTH_PYRAMID,
        ),
        opt("one-tile", REQ, b'R'),
        heading("Tile resolution"),
        opt("full-detail", REQ, b'd'),
        opt("low-detail", REQ, b'D'),
        opt("minimum-detail", REQ, b'm'),
        opt("extra-detail", REQ, b'~'),
        heading("Filtering feature attributes"),
        opt("exclude", REQ, b'x'),
        opt("include", REQ, b'y'),
        opt("exclude-all", NO, b'X'),
        heading("Modifying feature attributes"),
        opt("attribute-type", REQ, b'T'),
        opt("attribute-description", REQ, b'Y'),
        opt("accumulate-attribute", REQ, b'E'),
        prev("empty-csv-columns-are-null", P_EMPTY_CSV_COLUMNS),
        add("convert-stringified-ids-to-numbers", A_CONVERT_NUMERIC_IDS),
        opt("use-attribute-for-id", REQ, b'~'),
        prev("single-precision", P_SINGLE_PRECISION),
        opt("set-attribute", REQ, b'~'),
        opt("maximum-string-attribute-length", REQ, b'~'),
        heading("Filtering features by attributes"),
        opt("feature-filter-file", REQ, b'J'),
        opt("feature-filter", REQ, b'j'),
        heading("Dropping a fixed fraction of features by zoom level"),
        opt("drop-rate", REQ, b'r'),
        opt("retain-points-multiplier", REQ, b'~'),
        opt("base-zoom", REQ, b'B'),
        opt("drop-denser", REQ, b'~'),
        prev("limit-base-zoom-to-maximum-zoom", P_BASEZOOM_ABOVE_MAXZOOM),
        add("drop-lines", A_LINE_DROP),
        add("drop-polygons", A_POLYGON_DROP),
        opt("cluster-distance", REQ, b'K'),
        opt("cluster-maxzoom", REQ, b'k'),
        opt("preserve-point-density-threshold", REQ, b'~'),
        opt("preserve-multiplier-density-threshold", REQ, b'~'),
        heading("Dropping or merging a fraction of features to keep under tile size limits"),
        add("drop-densest-as-needed", A_DROP_DENSEST_AS_NEEDED),
        add("drop-fraction-as-needed", A_DROP_FRACTION_AS_NEEDED),
        add("drop-smallest-as-needed", A_DROP_SMALLEST_AS_NEEDED),
        opt("drop-by-attribute-as-needed", REQ, b'~'),
        opt("drop-by-attribute-order", REQ, b'~'),
        add("coalesce-densest-as-needed", A_COALESCE_DENSEST_AS_NEEDED),
        add("coalesce-fraction-as-needed", A_COALESCE_FRACTION_AS_NEEDED),
        add("coalesce-smallest-as-needed", A_COALESCE_SMALLEST_AS_NEEDED),
        prev("force-feature-limit", P_DYNAMIC_DROP),
        add("cluster-densest-as-needed", A_CLUSTER_DENSEST_AS_NEEDED),
        add("keep-point-cluster-position", A_KEEP_POINT_CLUSTER_POSITION),
        heading("Dropping tightly overlapping features"),
        opt("gamma", REQ, b'g'),
        add("increase-gamma-as-needed", A_INCREASE_GAMMA_AS_NEEDED),
        heading("Line and polygon simplification"),
        opt("simplification", REQ, b'S'),
        prev("no-line-simplification", P_SIMPLIFY),
        prev("simplify-only-low-zooms", P_SIMPLIFY_LOW),
        opt("simplification-at-maximum-zoom", REQ, b'~'),
        prev("no-tiny-polygon-reduction", P_TINY_POLYGON_REDUCTION),
        prev(
            "no-tiny-polygon-reduction-at-maximum-zoom",
            P_TINY_POLYGON_REDUCTION_AT_MAXZOOM,
        ),
        opt("tiny-polygon-size", REQ, b'~'),
        prev("no-simplification-of-shared-nodes", P_SIMPLIFY_SHARED_NODES),
        add("visvalingam", A_VISVALINGAM),
        heading("Attempts to improve shared polygon boundaries"),
        add("detect-shared-borders", A_DETECT_SHARED_BORDERS),
        add("grid-low-zooms", A_GRID_LOW_ZOOMS),
        heading("Controlling clipping to tile boundaries"),
        opt("buffer", REQ, b'b'),
        prev("no-clipping", P_CLIPPING),
        prev("no-duplication", P_DUPLICATION),
        heading("Reordering features within each tile"),
        prev("preserve-input-order", P_INPUT_ORDER),
        add("reorder", A_REORDER),
        add("coalesce", A_COALESCE),
        add("reverse", A_REVERSE),
        add("hilbert", A_HILBERT),
        opt("order-by", REQ, b'~'),
        opt("order-descending-by", REQ, b'~'),
        opt("order-smallest-first", NO, b'~'),
        opt("order-largest-first", NO, b'~'),
        heading("Adding calculated attributes"),
        add("calculate-feature-density", A_CALCULATE_FEATURE_DENSITY),
        add("generate-ids", A_GENERATE_IDS),
        add("calculate-feature-index", A_CALCULATE_INDEX),
        heading("Trying to correct bad source geometry"),
        add("detect-longitude-wraparound", A_DETECT_WRAPAROUND),
        prev("use-source-polygon-winding", P_USE_SOURCE_POLYGON_WINDING),
        prev(
            "reverse-source-polygon-winding",
            P_REVERSE_SOURCE_POLYGON_WINDING,
        ),
        opt("clip-bounding-box", REQ, b'~'),
        add(
            "convert-polygons-to-label-points",
            A_GENERATE_POLYGON_LABEL_POINTS,
        ),
        heading("Filtering tile contents"),
        opt("prefilter", REQ, b'C'),
        opt("postfilter", REQ, b'c'),
        heading("Setting or disabling tile size limits"),
        opt("maximum-tile-bytes", REQ, b'M'),
        opt("maximum-tile-features", REQ, b'O'),
        opt("limit-tile-feature-count", REQ, b'~'),
        opt("limit-tile-feature-count-at-maximum-zoom", REQ, b'~'),
        prev("no-feature-limit", P_FEATURE_LIMIT),
        prev("no-tile-size-limit", P_KILOBYTE_LIMIT),
        prev("no-tile-compression", P_TILE_COMPRESSION),
        prev("no-tile-stats", P_TILE_STATS),
        opt("tile-stats-attributes-limit", REQ, b'~'),
        opt("tile-stats-sample-values-limit", REQ, b'~'),
        opt("tile-stats-values-limit", REQ, b'~'),
        heading("Temporary storage"),
        opt("temporary-directory", REQ, b't'),
        heading("Progress indicator"),
        opt("quiet", NO, b'q'),
        opt("no-progress-indicator", NO, b'Q'),
        opt("progress-interval", REQ, b'U'),
        opt("json-progress", NO, b'u'),
        heading("Version"),
        opt("version", NO, b'v'),
        heading("manby"),
        opt("fast", NO, b'~'),
        opt("as-needed-per-tile", NO, b'~'),
        opt("compression", REQ, b'~'),
        opt("threads", REQ, b'~'),
        opt("memory-budget", REQ, b'~'),
        opt("tile-format", REQ, b'~'),
        opt("mlt-profile", REQ, b'~'),
        opt("bbox", REQ, b'~'),
        opt("h3", NO, b'~'),
        opt("h3-base-zoom", REQ, b'~'),
        opt("h3-categories", REQ, b'~'),
        opt("cluster-categories", REQ, b'~'),
        heading(""),
        opt("prevent", REQ, b'p'),
        opt("additional", REQ, b'a'),
        add("check-polygons", A_DEBUG_POLYGON),
        prev("no-polygon-splitting", P_POLYGON_SPLIT),
        add("prefer-radix-sort", A_PREFER_RADIX_SORT),
        opt("unidecode-data", REQ, b'~'),
        opt("help", NO, b'H'),
    ]
}

/// The short options for getopt: the options whose value is a printable letter.
fn getopt_string(long_options: &[LongOption]) -> String {
    let mut s = String::new();
    for lo in long_options {
        if lo.val > ' ' as i32 && lo.val < 0x7F {
            s.push(lo.val as u8 as char);
            if lo.has_arg == REQUIRED_ARGUMENT {
                s.push(':');
            }
        }
    }
    s
}

/// The usage message, as an error with `status`.
fn usage(argv0: &str, status: i32) -> Error {
    let forms = ["[options] [file.json ...]"];
    let required = [
        UsageRequiredOption {
            name: "output",
            placeholder: "output.mbtiles",
            alternation: 1,
        },
        UsageRequiredOption {
            name: "output-to-directory",
            placeholder: "directory",
            alternation: 1,
        },
    ];
    let mut out: Vec<u8> = Vec::new();
    print_usage(&mut out, argv0, &forms, &long_options_orig(), &required);
    let mut msg = String::from_utf8_lossy(&out).into_owned();
    if msg.ends_with('\n') {
        msg.pop();
    }
    Error::new(status, msg)
}

/// A byte as `%c` prints it.
fn letter(c: u8) -> String {
    String::from_utf8_lossy(&[c]).into_owned()
}

/// Parses a tippecanoe-style command line (`argv[0]` included). Errors carry
/// the full message to print, and so does the usage message for `--help`
/// (with exit status 0).
pub fn parse_args(argv: &[String]) -> crate::Result<Options> {
    let mut argv: Vec<String> = argv.to_vec();
    let argv0 = argv.first().cloned().unwrap_or_else(|| "manby".to_string());

    if argv.len() <= 1 {
        // with no arguments at all, there is nothing to complain about
        // specifically, so say in general what the arguments should be
        return Err(usage(&argv0, EXIT_ARGS));
    }

    let mut o = Options {
        program: argv0.clone(),
        commandline: tippecanoe::text::format_commandline(&argv),
        ..Default::default()
    };

    let long_options_orig = long_options_orig();
    let long_options = strip_usage_headings(&long_options_orig);
    let getopt_str = getopt_string(&long_options);

    let mut out_mbtiles: Option<String> = None;
    let mut out_dir: Option<String> = None;

    let mut g = Getopt::new();
    let mut option_index: usize = 0;
    loop {
        let i = g.getopt_long(
            &mut argv,
            &getopt_str,
            &long_options,
            Some(&mut option_index),
        );
        if i == -1 {
            break;
        }
        let optarg: String = g.optarg.clone().unwrap_or_default();
        let arg = cstr(&optarg);
        let err_args = |msg: String| Err(Error::new(EXIT_ARGS, msg));

        if (ADD..ADD + 256).contains(&i) {
            o.additional.set((i - ADD) as usize, true);
            continue;
        }
        if (PREV..PREV + 256).contains(&i) {
            o.prevent.set((i - PREV) as usize, true);
            continue;
        }

        match i as u8 {
            b'~' => {
                let opt = long_options[option_index].name;
                match opt {
                    "tile-stats-attributes-limit" => {
                        o.max_tilestats_attributes = tippecanoe::cstd::atoi(arg) as usize
                    }
                    "tile-stats-sample-values-limit" => {
                        o.max_tilestats_sample_values = tippecanoe::cstd::atoi(arg) as usize
                    }
                    "tile-stats-values-limit" => {
                        o.max_tilestats_values = tippecanoe::cstd::atoi(arg) as usize
                    }
                    "clip-bounding-box" => match sscanf_f64x4(arg) {
                        Some(v) => o.clip_bboxes.push(ClipBbox {
                            lon1: v[0],
                            lat1: v[1],
                            lon2: v[2],
                            lat2: v[3],
                            ..Default::default()
                        }),
                        None => {
                            return err_args(format!(
                                "{}: Can't parse bounding box --{}={}",
                                argv0, opt, arg
                            ));
                        }
                    },
                    "use-attribute-for-id" => o.attribute_for_id = arg.to_string(),
                    "set-attribute" => set_attribute_value(&mut o.set_attributes, &optarg, &argv0)?,
                    "smallest-maximum-zoom-guess" => {
                        o.maxzoom = MAX_ZOOM;
                        o.guess_maxzoom = true;
                        o.minimum_maxzoom = require_int(&optarg, "Minimum maxzoom", &argv0)?;
                        if o.minimum_maxzoom > MAX_ZOOM {
                            return err_args(format!(
                                "{}: {}: minimum maxzoom can be at most {}",
                                argv0, arg, MAX_ZOOM
                            ));
                        }
                    }
                    "tiny-polygon-size" => o.tiny_polygon_size = tippecanoe::cstd::atoi(arg),
                    "extra-detail" => {
                        o.extra_detail = require_int(&optarg, "Extra detail", &argv0)?;
                        if o.extra_detail > 30 {
                            // So the maximum geometry delta of just under 2 tile extents
                            // is less than 2^31
                            return err_args(format!(
                                "{}: --extra-detail can be at most 30",
                                argv0
                            ));
                        }
                    }
                    "order-by" => o.order_by.push(OrderField::new(arg.to_string(), false)),
                    "order-descending-by" => {
                        o.order_by.push(OrderField::new(arg.to_string(), true))
                    }
                    "order-smallest-first" => {
                        o.order_by
                            .push(OrderField::new(ORDER_BY_SIZE.to_string(), false));
                        o.order_by_size = true;
                    }
                    "order-largest-first" => {
                        o.order_by
                            .push(OrderField::new(ORDER_BY_SIZE.to_string(), true));
                        o.order_by_size = true;
                    }
                    "drop-by-attribute-as-needed" => {
                        if arg.is_empty() {
                            return err_args(format!(
                                "{}: --drop-by-attribute-as-needed requires a non-empty attribute name",
                                argv0
                            ));
                        }
                        o.drop_by_attribute_as_needed_attribute = arg.to_string();
                        o.additional.set(A_DROP_BY_ATTRIBUTE_AS_NEEDED, true);
                    }
                    "drop-by-attribute-order" => match arg {
                        "asc" => o.drop_by_attribute_descending = false,
                        "desc" => o.drop_by_attribute_descending = true,
                        _ => {
                            return err_args(format!(
                                "{}: {}: order must be one of: asc, desc",
                                argv0, arg
                            ));
                        }
                    },
                    "simplification-at-maximum-zoom" => {
                        o.maxzoom_simplification =
                            require_double(&optarg, "Mazoom simplification", &argv0)?;
                        if o.maxzoom_simplification <= 0.0 {
                            return err_args(format!(
                                "{}: --simplification-at-maximum-zoom must be > 0",
                                argv0
                            ));
                        }
                    }
                    "limit-tile-feature-count" => {
                        o.limit_tile_feature_count =
                            require_long(&optarg, "Limit tile feature count", &argv0)? as usize
                    }
                    "limit-tile-feature-count-at-maximum-zoom" => {
                        o.limit_tile_feature_count_at_maxzoom =
                            require_long(&optarg, "Limit tile feature count at maxzoom", &argv0)?
                                as usize
                    }
                    "drop-denser" => {
                        o.drop_denser = require_int(&optarg, "Drop denser rate", &argv0)? as u32;
                        if o.drop_denser > 100 {
                            return err_args(format!(
                                "{}: --drop-denser can be at most 100",
                                argv0
                            ));
                        }
                    }
                    "preserve-point-density-threshold" => {
                        o.preserve_point_density_threshold =
                            require_long(&optarg, "Preserve point density threshold", &argv0)?
                                as u64
                    }
                    "preserve-multiplier-density-threshold" => {
                        o.preserve_multiplier_density_threshold =
                            require_long(&optarg, "Preserve multiplier density threshold", &argv0)?
                                as u64
                    }
                    "extend-zooms-if-still-dropping-maximum" => {
                        o.extend_zooms_max = require_long(
                            &optarg,
                            "Maximum number by which to extend zooms",
                            &argv0,
                        )?
                    }
                    "retain-points-multiplier" => {
                        o.retain_points_multiplier = require_long(
                            &optarg,
                            "Multiply the fraction of points retained by zoom level",
                            &argv0,
                        )? as i32
                    }
                    "unidecode-data" => {
                        o.unidecode_data = tippecanoe::text::read_unidecode(arg);
                    }
                    "maximum-string-attribute-length" => {
                        o.maximum_string_attribute_length =
                            require_long(&optarg, "Maximum string attribute length", &argv0)?
                                as usize
                    }
                    "fast" => o.fast = true,
                    "as-needed-per-tile" => o.as_needed_per_tile = true,
                    "compression" => match arg {
                        "zlib" => o.zlib_tiles = true,
                        "libdeflate" => o.zlib_tiles = false,
                        _ => {
                            return err_args(format!(
                                "{}: --compression must be zlib or libdeflate, not {}",
                                argv0, arg
                            ));
                        }
                    },
                    "threads" => {
                        o.threads = require_int(&optarg, "Threads", &argv0)?.max(0) as usize
                    }
                    "memory-budget" => match parse_size(arg) {
                        Some(n) if n > 0 => o.memory_budget = Some(n),
                        _ => {
                            return err_args(format!(
                                "{}: --memory-budget must be a size such as 4G or 500M, not {}",
                                argv0, arg
                            ));
                        }
                    },
                    "tile-format" => match crate::mlt::TileFormat::parse(arg) {
                        Some(crate::mlt::TileFormat::Mlt) if !crate::mlt::AVAILABLE => {
                            return err_args(format!("{}: {}", argv0, crate::mlt::NOT_BUILT));
                        }
                        Some(f) => o.tile_format = f,
                        None => {
                            return err_args(format!(
                                "{}: --tile-format must be mvt or mlt, not {}",
                                argv0, arg
                            ));
                        }
                    },
                    "h3" => o.h3 = true,
                    "h3-base-zoom" => {
                        o.h3 = true;
                        o.h3_base_zoom = Some(require_int(&optarg, "H3 base zoom", &argv0)?)
                    }
                    "h3-categories" => {
                        o.h3 = true;
                        o.h3_categories = Some(arg.to_string())
                    }
                    "cluster-categories" => {
                        o.cluster_categories = Some(match arg.split_once(':') {
                            Some((attr, values)) => (
                                attr.to_string(),
                                Some(values.split(',').map(String::from).collect()),
                            ),
                            None => (arg.to_string(), None),
                        })
                    }
                    "bbox" => match sscanf_f64x4(arg) {
                        Some(b) if b[0] < b[2] && b[1] < b[3] => o.bbox = Some(b),
                        _ => {
                            return err_args(format!(
                                "{}: --bbox must be minlon,minlat,maxlon,maxlat, with the minimums below the maximums, not {}",
                                argv0, arg
                            ));
                        }
                    },
                    "mlt-profile" => match crate::mlt::MltProfile::parse(arg) {
                        Some(p) => o.mlt_profile = p,
                        None => {
                            return err_args(format!(
                                "{}: --mlt-profile must be web, native, min-size or fast, not {}",
                                argv0, arg
                            ));
                        }
                    },
                    _ => {
                        return err_args(format!("{}: Unrecognized option --{}", argv0, opt));
                    }
                }
            }

            b'n' => o.name = Some(arg.to_string()),
            b'N' => o.description = Some(arg.to_string()),
            b'l' => o.layer_name = Some(arg.to_string()),
            b'A' => o.attribution = Some(arg.to_string()),

            b'L' => {
                let mut src = Source::default();
                if optarg.starts_with('{') {
                    parse_json_source(&optarg, &mut src, &argv0)?;
                } else {
                    match arg.find(':') {
                        Some(cp) if cp != 0 => {
                            src.layer = arg[..cp].to_string();
                            src.file = arg[cp + 1..].to_string();
                        }
                        _ => return err_args(format!("{}: -L requires layername:file", argv0)),
                    }
                }
                o.sources.push(src);
            }

            b'z' => {
                if arg == "g" {
                    o.maxzoom = MAX_ZOOM;
                    o.guess_maxzoom = true;
                } else {
                    o.maxzoom = require_int(&optarg, "Maxzoom", &argv0)?;
                }
            }

            b'Z' => o.minzoom = require_int(&optarg, "Minzoom", &argv0)?,

            b'R' => match sscanf_u32x3(arg) {
                Some([z, x, y]) => {
                    o.minzoom = z as i32;
                    o.maxzoom = z as i32;
                    o.one_tile = Some((x, y));
                }
                None => return err_args("--one-tile argument must be z/x/y".to_string()),
            },

            b'B' | b'r' => {
                let letter_ = i as u8;
                let first = arg.bytes().next().unwrap_or(0);
                if arg == "g" {
                    if letter_ == b'B' {
                        o.basezoom = -2;
                    } else {
                        o.droprate = -2.0;
                    }
                } else if letter_ == b'r' && arg == "p" {
                    o.droprate = -3.0;
                } else if first == b'g' || first == b'f' {
                    if letter_ == b'B' {
                        o.basezoom = -2;
                    } else {
                        o.droprate = -2.0;
                    }
                    let rest = &optarg[1..];
                    if first == b'g' {
                        o.basezoom_marker_width = require_double(rest, "Marker width", &argv0)?;
                    } else {
                        o.basezoom_marker_width =
                            (50000.0 / require_double(rest, "Marker width", &argv0)?).sqrt();
                    }
                    if o.basezoom_marker_width == 0.0
                        || require_double(rest, "Marker width", &argv0)? == 0.0
                    {
                        return err_args(format!(
                            "{}: Must specify value >0 with -{}{}",
                            argv0,
                            letter(letter_),
                            letter(first)
                        ));
                    }
                } else if letter_ == b'B' {
                    o.basezoom = require_int(&optarg, "Basezoom", &argv0)?;
                    if o.basezoom == 0 && arg != "0" {
                        return err_args(format!("{}: Couldn't understand -B{}", argv0, arg));
                    }
                } else {
                    o.droprate = require_double(&optarg, "Drop rate", &argv0)?;
                }
            }

            b'K' => {
                o.cluster_distance = require_int(&optarg, "Cluster distance", &argv0)?;
                if o.cluster_distance > 255 {
                    return err_args(format!(
                        "{}: --cluster-distance {} is too big; limit is 255",
                        argv0, o.cluster_distance
                    ));
                }
            }

            b'k' => {
                if arg == "g" {
                    o.cluster_maxzoom = MAX_ZOOM - 1;
                    o.guess_cluster_maxzoom = true;
                } else {
                    o.cluster_maxzoom = require_int(&optarg, "Cluster maxzoom", &argv0)?;
                }
            }

            b'd' => {
                o.full_detail = require_int(&optarg, "Full detail", &argv0)?;
                if o.full_detail > 30 {
                    // So the maximum geometry delta of just under 2 tile extents
                    // is less than 2^31
                    return err_args(format!("{}: --full-detail can be at most 30", argv0));
                }
            }

            b'D' => {
                o.low_detail = require_int(&optarg, "Low detail", &argv0)?;
                if o.low_detail > 30 {
                    return err_args(format!("{}: --low-detail can be at most 30", argv0));
                }
            }

            b'm' => o.min_detail = require_int(&optarg, "Min detail", &argv0)?,

            b'o' | b'e' => {
                if let Some(prev) = out_mbtiles.as_ref().or(out_dir.as_ref()) {
                    return err_args(format!(
                        "{}: Can't specify both {} and {} as output",
                        argv0, prev, arg
                    ));
                }
                if i as u8 == b'o' {
                    out_mbtiles = Some(arg.to_string());
                } else {
                    out_dir = Some(arg.to_string());
                }
            }

            b'x' => {
                o.exclude.insert(arg.to_string());
            }
            b'y' => {
                o.exclude_all = true;
                o.include.insert(arg.to_string());
            }
            b'X' => o.exclude_all = true,

            b'Y' => match arg.find(':') {
                Some(cp) if cp != 0 => {
                    o.attribute_descriptions
                        .entry(arg[..cp].to_string())
                        .or_insert_with(|| arg[cp + 1..].to_string());
                }
                _ => return err_args(format!("{}: -Y requires attribute:description", argv0)),
            },

            b'J' => o.filter = Some(read_filter(arg)?),
            b'j' => o.filter = Some(parse_filter(arg)?),

            b'b' => {
                o.buffer = require_int(&optarg, "Buffer", &argv0)?;
                if o.buffer > 127 {
                    // So the maximum geometry delta is under 2 tile extents,
                    // from less than half a tile beyond one side to less than
                    // half a tile beyond the other.
                    return err_args(format!("{}: --buffer can be at most 127", argv0));
                }
            }

            b'f' => o.force = true,
            b'F' => o.forcetable = true,

            b't' => {
                o.tmpdir = arg.to_string();
                if !o.tmpdir.starts_with('/') {
                    eprintln!("Warning: temp directory {} doesn't begin with /", o.tmpdir);
                }
            }

            b'g' => o.gamma = require_double(&optarg, "Gamma", &argv0)?,
            b'q' => o.quiet = true,
            b'Q' => o.quiet_progress = true,
            b'u' => {
                o.quiet = true;
                o.json_progress = true;
            }
            b'U' => o.progress_interval = require_double(&optarg, "Progress interval", &argv0)?,

            b'p' | b'a' => {
                let is_prevent = i as u8 == b'p';
                for &c in arg.as_bytes() {
                    let code = if is_prevent { PREV } else { ADD } + c as i32;
                    if long_options.iter().any(|lo| lo.val == code) {
                        if is_prevent {
                            o.prevent.set(c as usize, true);
                        } else {
                            o.additional.set(c as usize, true);
                        }
                    } else {
                        return err_args(format!(
                            "{}: Unknown option -{}{}",
                            argv0,
                            letter(i as u8),
                            letter(c)
                        ));
                    }
                }
            }

            b'v' => {
                return Err(Error::new(
                    EXIT_SUCCESS,
                    format!(
                        "manby {} (tippecanoe {})",
                        env!("CARGO_PKG_VERSION"),
                        tippecanoe::version::VERSION
                    ),
                ));
            }

            b'P' => o.read_parallel = true,

            b's' => match PROJECTIONS
                .iter()
                .position(|p| p.name == arg || p.alias == arg)
            {
                Some(p) => {
                    o.projection = p;
                    o.source_crs = None;
                }
                None => match crate::crs::Reprojection::new(arg) {
                    Ok(r) => {
                        o.projection = 0;
                        o.source_crs = Some(r.code);
                    }
                    Err(_) => {
                        return err_args(format!("Unknown projection (-s): {}", arg));
                    }
                },
            },

            b'S' => {
                o.simplification = require_double(&optarg, "Simplification", &argv0)?;
                if o.simplification <= 0.0 {
                    return err_args(format!("{}: --simplification must be > 0", argv0));
                }
            }

            b'M' => o.max_tile_size = require_long(&optarg, "Max tile size", &argv0)? as usize,
            b'O' => {
                o.max_tile_features = require_long(&optarg, "Max tile features", &argv0)? as usize
            }
            b'c' => o.postfilter = Some(arg.to_string()),
            b'C' => o.prefilter = Some(arg.to_string()),
            b'T' => set_attribute_type(&mut o.attribute_types, &optarg)?,
            b'E' => set_attribute_accum(&mut o.attribute_accum, arg, &argv0)?,

            _ => {
                let mut msg = String::new();
                if i != b'H' as i32 && i != b'?' as i32 {
                    msg = format!("Unknown option -{}\n", letter(i as u8));
                }
                let status = if i == b'H' as i32 {
                    EXIT_SUCCESS
                } else {
                    EXIT_ARGS
                };
                let u = usage(&argv0, status);
                return Err(Error::new(status, msg + &u.message));
            }
        }
    }

    if let Some(out) = out_mbtiles {
        o.output = if tippecanoe::pmtiles_file::pmtiles_has_suffix(&out) {
            Output::Pmtiles(out)
        } else {
            Output::Mbtiles(out)
        };
    }
    if let Some(dir) = out_dir {
        o.output = Output::Directory(dir);
    }

    o.inputs = argv[g.optind.min(argv.len())..]
        .iter()
        .map(|a| cstr(a).to_string())
        .collect();

    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split(' ').map(|a| a.to_string()).collect()
    }

    #[test]
    fn tile_format() {
        use crate::mlt::{MltProfile, TileFormat};
        let o = parse_args(&args("tippecanoe -o out.pmtiles in.json")).unwrap();
        assert_eq!(o.tile_format, TileFormat::Mvt);
        assert_eq!(o.mlt_profile, MltProfile::Web);
        let o = parse_args(&args("tippecanoe -o out.pmtiles --tile-format=mvt in.json")).unwrap();
        assert_eq!(o.tile_format, TileFormat::Mvt);
        let mlt = parse_args(&args(
            "tippecanoe -o out.pmtiles --tile-format=mlt --mlt-profile=native in.json",
        ));
        if crate::mlt::AVAILABLE {
            let o = mlt.unwrap();
            assert_eq!(o.tile_format, TileFormat::Mlt);
            assert_eq!(o.mlt_profile, MltProfile::Native);
        } else {
            assert!(mlt.unwrap_err().message.contains(crate::mlt::NOT_BUILT));
        }
        assert!(parse_args(&args("tippecanoe -o x.pmtiles --tile-format=png in.json")).is_err());
        assert!(parse_args(&args("tippecanoe -o x.pmtiles --mlt-profile=tiny in.json")).is_err());
    }

    #[test]
    fn compression() {
        let o = parse_args(&args("tippecanoe -o out.pmtiles in.json")).unwrap();
        assert!(!o.zlib_tiles);
        let o = parse_args(&args(
            "tippecanoe -o out.pmtiles --compression=zlib in.json",
        ))
        .unwrap();
        assert!(o.zlib_tiles);
        let o = parse_args(&args(
            "tippecanoe -o x.pmtiles --compression libdeflate in.json",
        ))
        .unwrap();
        assert!(!o.zlib_tiles);
        assert!(parse_args(&args("tippecanoe -o x.pmtiles --compression=zstd in.json")).is_err());
    }

    #[test]
    fn benchmark_command_lines() {
        let o = parse_args(&args(
            "tippecanoe -q -f -o out.pmtiles -zg --drop-densest-as-needed -P in.geojsonl",
        ))
        .unwrap();
        assert!(o.quiet && o.force && o.read_parallel && o.guess_maxzoom);
        assert_eq!(o.maxzoom, MAX_ZOOM);
        assert!(o.additional.get(A_DROP_DENSEST_AS_NEEDED));
        assert!(matches!(o.output, Output::Pmtiles(ref p) if p == "out.pmtiles"));
        assert_eq!(o.inputs, vec!["in.geojsonl".to_string()]);

        let mut o = parse_args(&args(
            "tippecanoe -f -o out.mbtiles -z12 -Z0 --coalesce-densest-as-needed --fast --as-needed-per-tile in.json",
        ))
        .unwrap();
        assert!(o.fast && o.as_needed_per_tile);
        assert!(matches!(o.output, Output::Mbtiles(_)));
        o.tippecanoe_cpus = 4;
        o.resolve().unwrap();
        assert_eq!(o.full_detail, 12);
        assert_eq!(o.basezoom, 12);
        assert_eq!(o.geometry_scale, 32 - (12 + 12));
    }

    #[test]
    fn flags_and_errors() {
        let o = parse_args(&args("tippecanoe -o x.mbtiles -pi -ab --no-tile-stats a b")).unwrap();
        assert!(o.prevent.get(P_INPUT_ORDER));
        assert!(o.prevent.get(P_TILE_STATS));
        assert!(o.additional.get(A_DETECT_SHARED_BORDERS));
        assert_eq!(o.inputs.len(), 2);

        let e = parse_args(&args("tippecanoe -o x.mbtiles -pZ")).unwrap_err();
        assert_eq!(e.exit_code, EXIT_ARGS);
        assert_eq!(e.message, "tippecanoe: Unknown option -pZ");

        let e = parse_args(&args("tippecanoe -o x.mbtiles -zx")).unwrap_err();
        assert_eq!(e.message, "tippecanoe: Maxzoom must be a number (got x)");

        let e = parse_args(&args("tippecanoe -H")).unwrap_err();
        assert_eq!(e.exit_code, EXIT_SUCCESS);
        assert!(e.message.starts_with("Usage: tippecanoe [options]"));

        let mut o = parse_args(&args("tippecanoe -z3 -Z5")).unwrap();
        o.tippecanoe_cpus = 1;
        assert_eq!(o.resolve().unwrap_err().exit_code, EXIT_ARGS);
    }

    #[test]
    fn attribute_options() {
        let o = parse_args(&[
            "tippecanoe".to_string(),
            "-o".to_string(),
            "x.mbtiles".to_string(),
            "--set-attribute".to_string(),
            "{\"a\":1,\"b\":\"x\"}".to_string(),
            "--set-attribute=c:-5".to_string(),
            "-Tfoo:int".to_string(),
            "-Ebar:sum".to_string(),
            "-L{\"file\":\"in.json\",\"layer\":\"l\",\"description\":\"d\"}".to_string(),
        ])
        .unwrap();
        assert_eq!(o.set_attributes["a"].s, "1");
        assert_eq!(o.set_attributes["b"].type_, MVT_STRING);
        assert_eq!(o.set_attributes["c"].type_, MVT_DOUBLE);
        assert_eq!(o.attribute_types["foo"], MVT_INT);
        assert_eq!(o.attribute_accum["bar"], AttributeOp::Sum);
        assert_eq!(o.sources[0].file, "in.json");
        assert_eq!(o.sources[0].description, "d");
        let s = o.all_sources();
        assert_eq!(s.len(), 1);
    }
}
