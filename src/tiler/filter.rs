//! `--prefilter`: each attempt at a tile sends its candidates through the
//! filter command, as tippecanoe's prefilter writer thread does, and reads the
//! features the filter writes back.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::os::fd::{FromRawFd, IntoRawFd};

use tippecanoe::clip::close_poly;
use tippecanoe::geometry::{DrawVec, VT_POLYGON};
use tippecanoe::jsonpull::JsonPull;
use tippecanoe::mbtiles::LayermapEntry;
use tippecanoe::mvt::{MvtFeature, MvtGeometry, MvtLayer};
use tippecanoe::plugin::{PipeWriter, is_epipe, parse_feature, setup_filter, wait_for_filter};
use tippecanoe::serial::KeyPool;
use tippecanoe::write_json::{JsonWriter, layer_to_geojson};

use super::engine::Ctx;
use super::read::{Cand, CandExtra, Geoms, NO_LIST, ReadTile, needs_gaps};
use crate::store::Chunked;
use crate::store::{Feature, FeatureStore, Layer};

/// What the prefilter made of a tile's candidates: features of their own,
/// whose attributes arrive as full keys and values.
pub struct Prefiltered {
    pub store: FeatureStore,
    pub rt: ReadTile,
}

/// Runs the prefilter on the candidates of `rt`. `layermap` and `layer_unmap`
/// are the tile's own layer table, where `parse_feature` adds layers and
/// counts, as it does in the tiling segment's table in tippecanoe.
#[allow(clippy::too_many_arguments)]
pub fn prefilter_tile(
    ctx: &Ctx<'_>,
    rt: &ReadTile,
    filter: &str,
    layermap: &mut BTreeMap<String, LayermapEntry>,
    layer_unmap: &mut Vec<String>,
    postfilter: bool,
    key_pool: &mut KeyPool,
) -> Prefiltered {
    let z = rt.z;
    let tx = rt.x;
    let ty = rt.y;

    let mut write_to = -1;
    let mut read_from = -1;
    let mut pid: libc::pid_t = 0;
    setup_filter(
        filter,
        &mut write_to,
        &mut read_from,
        &mut pid,
        z as u32,
        tx,
        ty,
    );

    let mut parsed = Vec::new();
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || write_candidates(ctx, rt, write_to));

        // SAFETY: read_from is the read end of the filter's output pipe, owned here.
        let read_fp = unsafe { File::from_raw_fd(read_from) };
        {
            let mut jp = JsonPull::begin_file(Box::new(&read_fp));
            loop {
                let sf = parse_feature(
                    &mut jp,
                    z,
                    tx,
                    ty,
                    layermap,
                    0,
                    layer_unmap,
                    postfilter,
                    key_pool,
                );
                if sf.t < 0 {
                    break;
                }
                parsed.push(sf);
            }
        }
        // SAFETY: the descriptor came from into_raw_fd and is closed exactly once.
        unsafe { libc::close(read_fp.into_raw_fd()) };
        wait_for_filter(pid, "waitpid for prefilter\n");
        if let Err(e) = writer.join() {
            std::panic::resume_unwind(e);
        }
    });

    let mut out = Prefiltered {
        store: FeatureStore::default(),
        rt: ReadTile {
            z,
            x: tx,
            y: ty,
            cands: Chunked::default(),
            gaps: Chunked::default(),
            extras: Chunked::default(),
            geom: Geoms::default(),
            edges: Chunked::default(),
            attr_lists: Chunked::default(),
            full: Vec::new(),
            original_features: rt.original_features,
            unclipped_features: rt.unclipped_features,
            // tippecanoe's prefilter writer reads with a state of its own
            minzoom_feature_pending: false,
            children: Vec::new(),
            skip_unless_dropping: rt.skip_unless_dropping,
            complexity: rt.complexity,
        },
    };

    let mut layers: HashMap<String, u32> = HashMap::new();
    for sf in parsed {
        let name = layer_unmap
            .get(sf.layer as usize)
            .cloned()
            .unwrap_or_default();
        let layer = *layers.entry(name.clone()).or_insert_with(|| {
            out.store.layers.push(Layer {
                name: name.clone(),
                entry: LayermapEntry::new(out.store.layers.len()),
            });
            (out.store.layers.len() - 1) as u32
        });

        let fi = out.store.features.len() as u32;
        out.store.features.push(Feature {
            keep_index: true,
            ix: sf.index,
            seq: sf.seq as u64,
            geom_start: 0,
            geom_len: 0,
            attr_start: 0,
            attr_len: 0,
            layer,
            t: sf.t,
            has_id: sf.has_id,
            feature_minzoom: sf.feature_minzoom,
            tippecanoe_minzoom: sf.tippecanoe_minzoom as i8,
            tippecanoe_maxzoom: sf.tippecanoe_maxzoom as i8,
            segment: sf.segment as u16,
        });
        out.store
            .set_extras(fi as usize, sf.id, sf.extent, sf.label_point);

        let geom_start = out.rt.geom.len() as u32;
        let geom_len = sf.geometry.len() as u32;
        out.rt.geom.push(sf.geometry);
        out.rt.cands.push(Cand {
            fi,
            dropped: sf.dropped,
            geom_start,
            geom_len,
        });
        if needs_gaps(ctx.o, z) {
            out.rt.gaps.push(sf.gap);
        }
        out.rt.extras.push(CandExtra {
            edge_start: 0,
            edge_len: 0,
            attrs: NO_LIST,
            full: out.rt.full.len() as u32,
        });
        out.rt.full.push((sf.full_keys, sf.full_values));
    }

    out
}

/// tippecanoe's `run_prefilter`: writes the candidates to the filter as GeoJSON.
fn write_candidates(ctx: &Ctx<'_>, rt: &ReadTile, write_to: i32) {
    let store = ctx.store;
    let mut fp = PipeWriter::fdopen(write_to);
    {
        let mut state = JsonWriter::new_file(&mut fp);
        let mut scratch = DrawVec::new();
        for (i, c) in rt.cands.iter().enumerate() {
            let f = &store.features[c.fi as usize];
            let mut tmp_layer = MvtLayer {
                extent: 1i64 << 32,
                name: store.layers[f.layer as usize].name.clone(),
                ..Default::default()
            };

            let mut geometry: DrawVec = rt.cand_geom(c, &mut scratch).to_vec();
            if f.t as i32 == VT_POLYGON {
                geometry = close_poly(&mut geometry);
            }

            let mut sx: u32 = 0;
            let mut sy: u32 = 0;
            if rt.z != 0 {
                sx = rt.x << (32 - rt.z);
                sy = rt.y << (32 - rt.z);
            }

            let mut tmp_feature = MvtFeature {
                type_: f.t as i32,
                geometry: geometry
                    .iter()
                    .map(|g| MvtGeometry::new(g.op as i32, g.x + sx as i64, g.y + sy as i64))
                    .collect(),
                id: store.id(c.fi as usize),
                has_id: f.has_id,
                dropped: c.dropped,
                ..Default::default()
            };

            let attrs = rt.extra(i).attrs;
            let attrs = if attrs != NO_LIST {
                &rt.attr_lists[attrs as usize][..]
            } else {
                store.feature_attrs(f)
            };
            for a in attrs {
                if !ctx.value_null[a.value as usize] {
                    tmp_layer.tag(
                        &mut tmp_feature,
                        &store.keys[a.key as usize],
                        &ctx.value(a.value),
                    );
                }
            }
            tmp_layer.features.push(tmp_feature);

            layer_to_geojson(
                &tmp_layer,
                0,
                0,
                0,
                false,
                true,
                false,
                true,
                f.index(),
                f.seq as i64,
                store.extent(c.fi as usize),
                true,
                &mut state,
                0.0,
                &BTreeSet::new(),
            );
        }
    }

    if let Err(e) = fp.fclose()
        && !is_epipe(&e)
    {
        eprintln!("fclose output to prefilter: {}", e);
    }
}
