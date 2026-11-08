//! manby builds vector tilesets from geographic features, like tippecanoe, but
//! several times faster. See DESIGN.md.
//!
//! [`run`] takes [`Options`] and writes the tileset, and [`join`] takes
//! [`JoinOptions`] and joins tilesets as tippecanoe's tile-join does. Nothing
//! in manby exits the process or keeps global state; errors come back as
//! [`Error`].

pub mod buf;
pub mod clean;
pub mod compress;
pub mod crs;
pub mod error;
pub mod ingest;
pub mod join;
pub mod mem;
pub mod mlt;
pub mod options;
pub mod order;
pub mod serve;
pub mod sink;
pub mod stats;
pub mod store;
pub mod tiler;

use tippecanoe::errors::EXIT_NODATA;
use tippecanoe::mbtiles::{make_metadata, merge_layermaps};
use tippecanoe::options::{P_TILE_COMPRESSION, P_TILE_STATS};
use tippecanoe::projection::tile2lonlat;

pub use error::{Error, Result};
pub use join::{JoinOptions, JoinSummary, join};
pub use options::Options;

/// What a run produced.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub features: u64,
    pub tiles: u64,
    pub minzoom: i32,
    pub maxzoom: i32,
    /// A tile could not be made small enough, so the tileset stops at
    /// `maxzoom`, below the zoom that was planned. tippecanoe exits with
    /// `EXIT_INCOMPLETE` then.
    pub incomplete: bool,
}

/// The tiler's options for a run.
pub fn tile_options(
    o: &Options,
    plan: &order::Plan,
    store: &store::FeatureStore,
) -> tiler::TileOptions {
    tiler::TileOptions::new(o, plan, store)
}

/// Reads the inputs, tiles them and writes the tileset that `options` describes.
pub fn run(options: &Options) -> Result<Summary> {
    let mut o = options.clone();
    for w in o.resolve()? {
        eprintln!("{}", w);
    }
    // TRANSITIONAL: see set_tippecanoe_globals.
    options::set_tippecanoe_globals(&o, o.cluster_maxzoom);
    let budget = o.memory_budget();
    buf::set_budget(budget, std::path::Path::new(&o.tmpdir));

    let sink = sink::open(
        &o.output,
        &sink::SinkSettings {
            force: o.force,
            allow_existing: o.forcetable,
            tile_compression: !o.prevent.get(P_TILE_COMPRESSION),
            tmpdir: o.tmpdir.clone().into(),
            memory_budget: (budget / 64).clamp(1 << 20, 16 << 20) as usize,
            program: o.program.clone(),
            tile_format: o.tile_format,
            ..Default::default()
        },
    )?;

    let mut store = ingest::read_inputs(&o)?;
    let plan = order::order(&mut store, &o)?;
    options::set_tippecanoe_globals(&o, plan.cluster_maxzoom);

    let stats = tiler::tile_with(&store, &tile_options(&o, &plan, &store), sink.as_ref())?;

    // The tiler can also extend the zoom levels beyond the plan.
    let minzoom = plan.minzoom;
    let maxzoom = stats.maxzoom;
    let incomplete = stats.incomplete;
    if incomplete {
        if maxzoom > minzoom {
            eprintln!(
                "\n\n\n*** NOTE TILES ONLY COMPLETE THROUGH ZOOM {} ***",
                maxzoom
            );
        } else {
            return Err(Error::new(
                EXIT_NODATA,
                format!("{}: No zoom levels were successfully written", o.program),
            ));
        }
    }

    // mbtiles-style bounding box and center
    let (midx, midy) = (stats.midx, stats.midy);
    let (mut minlon, mut maxlat) = tile2lonlat(midx as i64, midy as i64, maxzoom);
    let (mut maxlon, mut minlat) = tile2lonlat(
        midx.wrapping_add(1) as i64,
        midy.wrapping_add(1) as i64,
        maxzoom,
    );

    let mut midlat = (maxlat + minlat) / 2.0;
    let mut midlon = (maxlon + minlon) / 2.0;

    let b = store.bbox;
    (minlon, maxlat) = tile2lonlat(b[0], b[1], 32);
    (maxlon, minlat) = tile2lonlat(b[2], b[3], 32);

    midlat = midlat.clamp(minlat, maxlat);
    midlon = midlon.clamp(minlon, maxlon);

    // antimeridian-aware bounding box: whichever of the two is narrower
    let (b1, b2) = (store.bbox1, store.bbox2);
    let nb = if b2[2] - b2[0] < b1[2] - b1[0] {
        b2
    } else {
        b1
    };
    let (minlon2, maxlat2) = tile2lonlat(nb[0], nb[1], 32);
    let (maxlon2, minlat2) = tile2lonlat(nb[2], nb[3], 32);

    let mut merged = merge_layermaps(&[store.layermap(), stats.layermap.clone()]);
    for v in merged.values_mut() {
        v.minzoom = minzoom;
        v.maxzoom = maxzoom;
    }

    let fname = o.tileset_name();
    let metadata = make_metadata(
        &fname,
        minzoom,
        maxzoom,
        minlat,
        minlon,
        maxlat,
        maxlon,
        minlat2,
        minlon2,
        maxlat2,
        maxlon2,
        midlat,
        midlon,
        o.attribution.as_deref(),
        &merged,
        true,
        o.description.as_deref(),
        !o.prevent.get(P_TILE_STATS),
        &o.attribute_descriptions,
        "tippecanoe",
        &o.commandline,
        &stats.strategies,
        plan.basezoom,
        plan.droprate,
        o.retain_points_multiplier,
    );
    sink.finish(&metadata)?;

    Ok(Summary {
        features: store.features.len() as u64,
        tiles: stats.tiles,
        minzoom,
        maxzoom,
        incomplete,
    })
}
