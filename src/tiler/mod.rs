//! Making the tiles: a parallel walk of the tile quadtree. See DESIGN.md.
//!
//! The tiles are the ones tippecanoe's `traverse_zooms` makes from the same
//! features. Each tile is read once: its input features are clipped, given to
//! its children, and kept in memory as candidates. Choosing which candidates
//! go into the tile, and retrying with stricter thresholds when it is too big,
//! then works on those candidates without reading them again.

mod accum;
mod encode;
mod engine;
mod filter;
mod geom;
mod make;
mod read;
mod util;

#[cfg(test)]
pub(crate) mod oracle;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};

use tippecanoe::attribute::AttributeOp;
use tippecanoe::globals::OrderField;
use tippecanoe::jsonpull::JsonTree;
use tippecanoe::mbtiles::LayermapEntry;
use tippecanoe::serial::Node;

use crate::options::Options;
use crate::order::Plan;
use crate::sink::TileSink;
use crate::store::FeatureStore;

/// What the tiler reports back, for the metadata and the summary.
#[derive(Clone, Debug, Default)]
pub struct TileStats {
    pub tiles: u64,
    /// tippecanoe's per-zoom strategies, for the metadata's `strategies` entry.
    pub strategies: Vec<tippecanoe::tile::Strategy>,
    /// The zoom level actually reached, which can be lower than planned.
    pub maxzoom: i32,
    /// The tile at the maximum zoom with the most vertices, for the metadata's center.
    pub midx: u32,
    pub midy: u32,
    /// Attribute statistics added while tiling (accumulated attributes,
    /// cluster sizes and the like), to merge after the input's layer map.
    pub layermap: BTreeMap<String, LayermapEntry>,
    /// A tile could not be made small enough. As in tippecanoe, the tiles of
    /// the zooms up to `maxzoom` are complete.
    pub incomplete: bool,
}

/// Everything the tiler needs to know: tippecanoe's tiling options and the
/// zoom plan. `TileOptions::new` takes them from `Options` and `Plan`; tests
/// and library users can also fill one in directly.
#[derive(Clone, Debug)]
pub struct TileOptions {
    pub minzoom: i32,
    pub maxzoom: i32,
    pub basezoom: i32,
    pub droprate: f64,
    /// The first tile, normally 0/0/0.
    pub iz: i32,
    pub ix: u32,
    pub iy: u32,
    /// With `-at`: the first tile's `estimated_complexity`, the size of
    /// tippecanoe's zoom 0 temporary file.
    pub estimated_complexity: i64,
    pub geometry_scale: i32,
    pub buffer: i32,
    pub full_detail: i32,
    pub low_detail: i32,
    pub min_detail: i32,
    pub gamma: f64,
    pub simplification: f64,
    pub maxzoom_simplification: f64,
    /// tippecanoe's `prevent[]` and `additional[]` flags, by option letter.
    pub prevent: [bool; 256],
    pub additional: [bool; 256],
    pub max_tile_size: usize,
    pub max_tile_features: usize,
    pub cluster_distance: i32,
    pub cluster_maxzoom: i32,
    pub tiny_polygon_size: i32,
    pub limit_tile_feature_count: usize,
    pub limit_tile_feature_count_at_maxzoom: usize,
    pub extend_zooms_max: i64,
    pub retain_points_multiplier: i32,
    pub preserve_multiplier_density_threshold: u64,
    pub order_by: Vec<OrderField>,
    /// Whether any `--clip-bounding-box` was given.
    pub clipbboxes: bool,
    pub extra_detail: i32,
    pub attribute_accum: HashMap<String, AttributeOp>,
    /// `--cluster-categories`: keys starting `kind:`, which are summed.
    pub category_prefix: Option<String>,
    /// The parsed `-j` feature filter.
    pub filter: Option<JsonTree>,
    pub drop_by_attribute: Option<String>,
    pub drop_by_attribute_descending: bool,
    pub unidecode_data: Vec<String>,
    pub prefilter: Option<String>,
    pub postfilter: Option<String>,
    /// The shared nodes found while reading, for `--no-simplification-of-shared-nodes`.
    pub shared_nodes: Vec<Node>,
    pub shared_nodes_bloom: Vec<u8>,
    /// tippecanoe's `CPUS` and `TEMP_FILES`. They decide how many zoom levels
    /// below the minimum zoom tippecanoe skips at once, which affects clipping.
    pub cpus: usize,
    pub temp_files: usize,
    pub quiet: bool,
    /// Worker threads; 0 means one per available CPU.
    pub threads: usize,
    /// Compress with libdeflate instead of zlib (different tiles).
    pub fast: bool,
    /// `--as-needed-per-tile`: each tile keeps its own thresholds, and no
    /// zoom level is made again (different tiles).
    pub as_needed_per_tile: bool,
    /// Store every tile as zlib compresses it, as tippecanoe does.
    pub zlib_tiles: bool,
    /// Report each zoom level's passes and compression to stderr.
    pub timing: bool,
    /// Report every attempt at every tile to stderr.
    pub trace: bool,
    /// `-t`: where temporary files go.
    pub tmpdir: std::path::PathBuf,
    /// Bytes of tile data kept in memory while it waits for its tile to be
    /// read or written out; the rest waits in temporary files.
    pub rest_memory: usize,
    /// Memory for the tiles with the most data, which run together only as
    /// far as this allows: three quarters of the memory budget, up to 768 MB.
    /// Under a small budget the store waits in the temporary file, so the
    /// tiles being made take most of what is resident.
    pub big_memory: usize,
    /// The run's memory budget, which also bounds how many tiles are made at
    /// once.
    pub memory_budget: u64,
    /// Write MapLibre Tiles instead of MVT (`--tile-format=mlt`).
    pub mlt: Option<crate::mlt::MltSettings>,
}

impl Default for TileOptions {
    fn default() -> Self {
        TileOptions {
            minzoom: 0,
            maxzoom: 14,
            basezoom: 14,
            droprate: 2.5,
            iz: 0,
            ix: 0,
            iy: 0,
            estimated_complexity: 0,
            geometry_scale: 32 - (12 + 14),
            buffer: 5,
            full_detail: 12,
            low_detail: 12,
            min_detail: 7,
            gamma: 0.0,
            simplification: 1.0,
            maxzoom_simplification: -1.0,
            prevent: [false; 256],
            additional: [false; 256],
            max_tile_size: 500000,
            max_tile_features: 200000,
            cluster_distance: 0,
            cluster_maxzoom: tippecanoe::globals::MAX_ZOOM,
            tiny_polygon_size: 2,
            limit_tile_feature_count: 0,
            limit_tile_feature_count_at_maxzoom: 0,
            extend_zooms_max: 0,
            retain_points_multiplier: 1,
            preserve_multiplier_density_threshold: 0,
            order_by: Vec::new(),
            clipbboxes: false,
            extra_detail: -1,
            attribute_accum: HashMap::new(),
            category_prefix: None,
            filter: None,
            drop_by_attribute: None,
            drop_by_attribute_descending: false,
            unidecode_data: Vec::new(),
            prefilter: None,
            postfilter: None,
            shared_nodes: Vec::new(),
            shared_nodes_bloom: Vec::new(),
            cpus: 1,
            temp_files: 4,
            quiet: true,
            threads: 0,
            fast: false,
            as_needed_per_tile: false,
            zlib_tiles: false,
            timing: false,
            trace: false,
            tmpdir: std::path::PathBuf::from("/tmp"),
            rest_memory: 16 << 20,
            big_memory: 768 << 20,
            memory_budget: u64::MAX,
            mlt: None,
        }
    }
}

impl TileOptions {
    /// How a key's values are accumulated: `--accumulate-attribute`'s
    /// operation, or a sum for `--cluster-categories`' keys.
    pub fn accumulate_op(&self, key: &str) -> Option<AttributeOp> {
        self.attribute_accum.get(key).copied().or_else(|| {
            self.category_prefix
                .as_deref()
                .filter(|p| key.starts_with(p))
                .map(|_| AttributeOp::Sum)
        })
    }

    /// Whether any attribute is accumulated.
    pub fn accumulates(&self) -> bool {
        !self.attribute_accum.is_empty() || self.category_prefix.is_some()
    }

    /// The tiling options of a run, from the command line options, the zoom
    /// plan and what reading found (the shared nodes).
    pub fn new(o: &Options, plan: &Plan, store: &FeatureStore) -> TileOptions {
        TileOptions {
            minzoom: plan.minzoom,
            maxzoom: plan.maxzoom,
            basezoom: plan.basezoom,
            droprate: plan.droprate,
            iz: plan.iz as i32,
            ix: plan.ix,
            iy: plan.iy,
            estimated_complexity: plan.estimated_complexity,
            geometry_scale: plan.geometry_scale,
            buffer: o.buffer,
            full_detail: o.full_detail,
            low_detail: o.low_detail,
            min_detail: o.min_detail,
            gamma: o.gamma,
            simplification: o.simplification,
            maxzoom_simplification: o.maxzoom_simplification,
            prevent: o.prevent.0,
            additional: o.additional.0,
            max_tile_size: o.max_tile_size,
            max_tile_features: o.max_tile_features,
            cluster_distance: o.cluster_distance,
            cluster_maxzoom: plan.cluster_maxzoom,
            tiny_polygon_size: o.tiny_polygon_size,
            limit_tile_feature_count: o.limit_tile_feature_count,
            limit_tile_feature_count_at_maxzoom: o.limit_tile_feature_count_at_maxzoom,
            extend_zooms_max: o.extend_zooms_max,
            retain_points_multiplier: o.retain_points_multiplier,
            preserve_multiplier_density_threshold: o.preserve_multiplier_density_threshold,
            order_by: o.order_by.clone(),
            clipbboxes: !o.clip_bboxes.is_empty(),
            extra_detail: o.extra_detail,
            attribute_accum: o.attribute_accum.clone(),
            category_prefix: o
                .cluster_categories
                .as_ref()
                .map(|(a, _)| format!("{}:", a)),
            filter: o.filter.clone(),
            drop_by_attribute: (!o.drop_by_attribute_as_needed_attribute.is_empty())
                .then(|| o.drop_by_attribute_as_needed_attribute.clone()),
            drop_by_attribute_descending: o.drop_by_attribute_descending,
            unidecode_data: o.unidecode_data.clone(),
            prefilter: o.prefilter.clone(),
            postfilter: o.postfilter.clone(),
            shared_nodes: store.shared_nodes.clone(),
            shared_nodes_bloom: store.shared_nodes_bloom.clone(),
            cpus: o.tippecanoe_cpus.max(1),
            temp_files: (o.tippecanoe_cpus * 4).max(4),
            quiet: o.quiet,
            threads: o.threads,
            fast: o.fast,
            as_needed_per_tile: o.as_needed_per_tile,
            zlib_tiles: o.zlib_tiles,
            timing: false,
            trace: false,
            tmpdir: std::path::PathBuf::from(&o.tmpdir),
            rest_memory: (o.memory_budget() / 64).min(16 << 20) as usize,
            big_memory: (o.memory_budget() / 4 * 3).min(768 << 20) as usize,
            memory_budget: o.memory_budget(),
            mlt: (o.tile_format == crate::mlt::TileFormat::Mlt).then_some(
                crate::mlt::MltSettings {
                    profile: o.mlt_profile,
                    gzipped: !o.prevent.get(tippecanoe::options::P_TILE_COMPRESSION),
                },
            ),
        }
    }

    fn p(&self, c: usize) -> bool {
        self.prevent[c]
    }

    fn a(&self, c: usize) -> bool {
        self.additional[c]
    }
}

/// Makes every tile of `plan` from `store` and hands them to `sink`.
pub fn tile(
    store: &FeatureStore,
    plan: &Plan,
    options: &Options,
    sink: &dyn TileSink,
) -> crate::Result<TileStats> {
    tile_with(store, &TileOptions::new(options, plan, store), sink)
}

/// Makes the tiles of `store` as `topts` describes and hands them to `sink`.
pub fn tile_with(
    store: &FeatureStore,
    topts: &TileOptions,
    sink: &dyn TileSink,
) -> crate::Result<TileStats> {
    let threads = if topts.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        topts.threads
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| crate::Error::new(tippecanoe::errors::EXIT_PTHREAD, e.to_string()))?;
    let started = std::time::Instant::now();
    let helpers = std::sync::Arc::new(util::Helpers::new(threads.saturating_sub(1)));
    let ctx = pool.install(|| engine::Ctx::new(store, topts, helpers));
    let root = pool.install(|| engine::root_input(&ctx));
    if topts.timing {
        eprintln!("setup {:.3}s", started.elapsed().as_secs_f64());
    }
    pool.in_place_scope(|scope| engine::drive(&ctx, root, sink, scope, threads))
}
