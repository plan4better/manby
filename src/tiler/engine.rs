//! The order of work: zoom levels, the passes tippecanoe makes over a zoom
//! level when a tile needs a stricter threshold, and the parallelism.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use tippecanoe::errors::{EXIT_ARGS, EXIT_IMPOSSIBLE, EXIT_INCOMPLETE};
use tippecanoe::globals::MAX_ZOOM;
use tippecanoe::jsonpull::JsonRef;
use tippecanoe::mbtiles::{LayermapEntry, merge_layermaps};
use tippecanoe::mvt::{MVT_NULL, MvtValue, stringified_to_mvt_value};
use tippecanoe::options::*;
use tippecanoe::tile::Strategy;

use super::encode::{ValueCodes, key_slot};
use super::make::{MakeParams, MakeResult, Thresholds, dropping_features, make_tile};
use super::read::{ReadParams, TileInput, read_tile, store_inputs};
use super::util::Helpers;
use super::{TileOptions, TileStats};
use crate::buf::Buf;
use crate::error::Error;
use crate::mem::{Rest, Spool, Stored};
use crate::sink::TileSink;
use crate::store::FeatureStore;
use rayon::prelude::*;

/// What every tile of a run shares.
pub struct Ctx<'a> {
    pub store: &'a FeatureStore,
    pub o: &'a TileOptions,
    pub value_null: Buf<bool>,
    /// The position of each layer when the layers are sorted by name.
    pub layer_rank: Vec<u32>,
    pub filter: Option<JsonRef<'a>>,
    /// The encodings of the values, for writing tiles.
    pub codes: ValueCodes,
    /// Each key's slot in tippecanoe's key dedup cache.
    pub key_slots: Vec<u32>,
    /// `--accumulate-attribute`'s operation for each of the store's keys.
    pub key_accum: Vec<Option<tippecanoe::attribute::AttributeOp>>,
    /// With `--accumulate-attribute`, each value as `atof` reads it.
    pub value_num: Buf<f64>,
    /// The threads that help with the work inside one tile.
    pub helpers: Arc<Helpers>,
    /// The memory for tile data that waits: inputs, and tiles not written yet.
    pub rest: Arc<Rest>,
    /// For `TileOptions::timing`: attempts, compressions, bytes compressed
    /// and nanoseconds spent compressing.
    pub counters: [std::sync::atomic::AtomicU64; 4],
    /// Set when a task fails, so that the others stop at their next attempt.
    pub failed: std::sync::atomic::AtomicBool,
    pub started: std::time::Instant,
}

impl<'a> Ctx<'a> {
    /// The store's value `i` as a tile value.
    pub fn value(&self, i: u32) -> MvtValue {
        let v = self.store.values.get(i);
        stringified_to_mvt_value(v.type_, v.s)
    }

    pub fn new(store: &'a FeatureStore, o: &'a TileOptions, helpers: Arc<Helpers>) -> Ctx<'a> {
        let value_null = store.values.iter().map(|v| v.type_ == MVT_NULL).collect();

        let mut order: Vec<usize> = (0..store.layers.len()).collect();
        order.sort_by(|&a, &b| store.layers[a].name.cmp(&store.layers[b].name));
        let mut layer_rank = vec![0u32; store.layers.len()];
        for (rank, &i) in order.iter().enumerate() {
            layer_rank[i] = rank as u32;
        }

        let codes = ValueCodes::new(&store.values);
        let key_slots = store.keys.iter().map(|k| key_slot(k)).collect();
        let key_accum = store.keys.iter().map(|k| o.accumulate_op(k)).collect();
        let value_num = if !o.accumulates() {
            Buf::new()
        } else {
            let mut nums = Buf::with_capacity(store.values.len());
            nums.resize(store.values.len(), 0.0);
            nums.par_chunks_mut(1 << 14)
                .enumerate()
                .for_each(|(c, part)| {
                    for (k, n) in part.iter_mut().enumerate() {
                        *n = super::accum::to_double(store.values.get(((c << 14) + k) as u32).s);
                    }
                });
            nums
        };

        Ctx {
            codes,
            key_slots,
            key_accum,
            value_num,
            helpers,
            rest: Rest::new(o.rest_memory),
            counters: Default::default(),
            failed: Default::default(),
            started: std::time::Instant::now(),
            store,
            o,
            value_null,
            layer_rank,
            filter: o.filter.as_ref().map(|t| t.root()),
        }
    }
}

fn unsupported(ctx: &Ctx<'_>) -> Option<&'static str> {
    let store = ctx.store;
    if ctx.o.a(A_VARIABLE_DEPTH_PYRAMID)
        && (store.pool_offsets.len() != store.attrs.len()
            || store.segment_layers.is_empty()
            || store.initial_x.is_empty())
    {
        return Some("variable depth pyramids (-at) without the store's serialized layout");
    }
    None
}

/// How many threads tippecanoe would tile a zoom level with, given how many
/// of its temporary files have data.
fn tippecanoe_threads(o: &TileOptions, useful_threads: usize) -> usize {
    let mut threads = o.cpus;
    if threads > o.temp_files / 4 {
        threads = o.temp_files / 4;
    }
    if threads > useful_threads {
        threads = useful_threads;
    }
    for e in 0..30 {
        if threads >= (1usize << e) && threads < (1usize << (e + 1)) {
            threads = 1usize << e;
            break;
        }
    }
    if threads >= (1usize << 30) {
        threads = 1usize << 30;
    }
    threads.max(1)
}

/// The first tile's input: every feature of the store.
pub fn root_input(ctx: &Ctx<'_>) -> Option<TileInput> {
    let store = ctx.store;
    let o = ctx.o;
    if store.features.is_empty() {
        return None;
    }
    Some(TileInput::root(
        o.iz,
        o.ix,
        o.iy,
        store.features.len(),
        o.estimated_complexity,
    ))
}

/// How many tiles may be made at once within the memory budget, each
/// holding up to a full tile's features while it is made.
fn tiles_at_once(o: &TileOptions) -> usize {
    let per_tile = o.max_tile_features as u64 * 200 + o.max_tile_size as u64 * 8;
    (o.memory_budget / 2 / per_tile.max(1)).clamp(2, usize::MAX as u64) as usize
}

/// Why a level's spools are there when they are used.
const LIVE: &str = "a level's spools live until it is finalized";

/// How many zoom levels the tiles may run ahead of the last finished level.
const LOOKAHEAD: usize = 3;

/// A tile whose reading and making takes about this much memory or more
/// counts as big. Big tiles run together only as far as their memory
/// stays within `TileOptions::big_memory`.
const BIG_TASK: u64 = 16 << 20;

/// Reading a tile and making it in one pass of its zoom level. The first
/// read also writes the tile's features out for the next zoom; later passes
/// read the tile again from its input, as tippecanoe does, rather than keep
/// whole zoom levels of candidates in memory.
struct Task {
    level: usize,
    idx: usize,
    pass: usize,
    /// About how much memory the task takes, if it counts as big.
    big: u64,
    /// Where the tile and its children wait until they are needed.
    spool: Arc<Spool>,
    child_spool: Arc<Spool>,
    input: Arc<TileInput>,
    rp: ReadParams,
    complexity: Option<i64>,
    mp: MakeParams,
}

/// A tile waiting for a thread, with what its task needs that may change
/// before it starts. The task itself is made when it starts, since a level
/// can queue hundreds of thousands of tiles at once.
struct Ready {
    /// The level, and the order the tile was queued in.
    key: (usize, u64),
    idx: u32,
    pass: u32,
    maxzoom: i32,
    extend_zooms_max: i64,
    /// About how much memory the task takes, if it counts as big.
    big: u64,
}

impl PartialEq for Ready {
    fn eq(&self, o: &Ready) -> bool {
        self.key == o.key
    }
}

impl Eq for Ready {}

impl PartialOrd for Ready {
    fn partial_cmp(&self, o: &Ready) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Ready {
    fn cmp(&self, o: &Ready) -> std::cmp::Ordering {
        self.key.cmp(&o.key)
    }
}

/// What a task reports: the first read of a tile as soon as it is done, so
/// that the children can start while the tile is being made, then the tile.
enum Done {
    Read {
        level: usize,
        idx: usize,
        children: Vec<TileInput>,
        complexity: i64,
    },
    Made {
        level: usize,
        idx: usize,
        pass: usize,
        big: u64,
        result: Box<MakeResult>,
        tile: Option<Stored>,
    },
    Failed(Error),
}

/// A tile of a zoom level.
struct Slot {
    input: Arc<TileInput>,
    /// Whether the tile has been read, and so has written its children.
    read: bool,
    /// With `-at`: the estimated complexity, once counted.
    complexity: Option<i64>,
    /// What the current pass made of the tile.
    made: Option<Made>,
    /// Its bytes, until the level is done.
    tile: Option<Stored>,
    /// With `-at`: whether the tile's children must not try to stop early,
    /// from the pass in which the tile was first made.
    children_must_go_on: Option<bool>,
    /// With `-at`: the tile's children, until it is known whether the tile
    /// writes them.
    held: Vec<TileInput>,
}

/// What a tile keeps of how a pass made it.
struct Made {
    len: i64,
    skipped: bool,
    truncated: bool,
    /// The thresholds and counts under which the tile stands for the next
    /// pass too, if a higher threshold cannot change it.
    reuse: Option<Box<Reuse>>,
}

struct Reuse {
    th: Thresholds,
    gap: u64,
    drop_sequence: u64,
    strategy: Option<Strategy>,
}

/// The totals of a pass over a level, which its tiles add to as they are
/// made. They do not depend on the order the tiles come in.
struct Totals {
    /// tippecanoe's zoom counter.
    z: i32,
    /// The thresholds for another pass.
    next: Thresholds,
    strategy: Strategy,
    extend_zooms: bool,
    /// The message of the first tile in tile order that failed with one.
    error: Option<((u32, u32), String)>,
    /// Each tile's z, x, y and vertex count.
    history: Vec<(i32, u32, u32, i64)>,
}

impl Totals {
    fn new(z: i32, th: Thresholds) -> Totals {
        Totals {
            z,
            next: th,
            strategy: Strategy::default(),
            extend_zooms: false,
            error: None,
            history: Vec::new(),
        }
    }
}

/// The maximum zoom during a pass, and each tile's z, x, y and vertex count.
type PassTiles = (i32, Vec<(i32, u32, u32, i64)>);

/// One zoom level: the tiles that tippecanoe would read from one zoom's
/// temporary files.
struct Level {
    z: i32,
    /// tippecanoe's zoom counter when it starts on this level.
    loop_z: i32,
    nextzoom: Option<i32>,
    threads: usize,
    child_shards: i32,
    max_zoom_increment: i32,
    slots: Vec<Slot>,
    /// Where the level's inputs and tiles wait, and where its tiles write
    /// their children: the next level's inputs. Each spool goes, with its
    /// memory and its temporary file, when the level whose inputs it holds
    /// is finalized.
    spool: Option<Arc<Spool>>,
    child_spool: Option<Arc<Spool>>,
    /// Tiles whose first task waits for the level to start.
    waiting: Vec<usize>,
    /// Tiles not read yet.
    unread: usize,
    /// Tiles without a result in the current pass.
    missing: usize,
    /// For tippecanoe's count of temporary files with data: each input's
    /// parent tile and its own position.
    origins: Vec<((u32, u32), (u32, u32))>,
    inputs_complete: bool,
    started: bool,
    pass: usize,
    th: Thresholds,
    totals: Totals,
    outstanding: usize,
    /// Per pass: the maximum zoom then and each tile's z, x, y and vertex count.
    history: Vec<PassTiles>,
    tilestats: Vec<BTreeMap<String, LayermapEntry>>,
    zoom_tile_size: usize,
    zoom_feature_count: usize,
    strategy: Strategy,
    /// The zoom counter after the last pass, where the strategies go.
    strategy_z: i32,
    err: bool,
    done: bool,
    /// Whether any pass made a tile rather than skipping them all.
    wrote: bool,
}

impl Level {
    fn new(z: i32, loop_z: i32, o: &TileOptions, spool: Arc<Spool>, rest: &Arc<Rest>) -> Level {
        let th = Thresholds {
            gamma: o.gamma,
            mingap: 0,
            minextent: 0,
            mindrop_sequence: 0,
            minattribute: if o.drop_by_attribute_descending {
                f64::INFINITY
            } else {
                f64::NEG_INFINITY
            },
        };
        Level {
            z,
            loop_z,
            nextzoom: None,
            threads: 1,
            child_shards: 0,
            max_zoom_increment: 0,
            slots: Vec::new(),
            spool: Some(spool),
            child_spool: Some(Spool::new(&o.tmpdir, rest)),
            waiting: Vec::new(),
            unread: 0,
            missing: 0,
            origins: Vec::new(),
            inputs_complete: false,
            started: false,
            pass: 0,
            th,
            totals: Totals::new(loop_z, th),
            outstanding: 0,
            history: Vec::new(),
            tilestats: Vec::new(),
            zoom_tile_size: 0,
            zoom_feature_count: 0,
            strategy: Strategy::default(),
            strategy_z: z,
            err: false,
            done: false,
            wrote: false,
        }
    }

    /// Adds what a pass made of tile `idx` to the pass's totals.
    fn fold(&mut self, idx: usize, mut r: MakeResult, descending: bool) {
        let slot = &mut self.slots[idx];
        let (z, x, y) = (slot.input.z, slot.input.x, slot.input.y);
        let t = &mut self.totals;
        t.history.push((z, x, y, r.len));
        slot.made = Some(Made {
            len: r.len,
            skipped: r.skipped,
            truncated: r.truncated,
            reuse: r.reusable.map(|th| {
                Box::new(Reuse {
                    th,
                    gap: r.reuse_gap,
                    drop_sequence: r.reuse_drop_sequence,
                    strategy: r.strategy,
                })
            }),
        });
        if r.skipped {
            return;
        }
        self.wrote = true;
        t.z = t.z.max(z);

        if r.len < 0 {
            self.err = true;
            if let Some(m) = r.error.take()
                && t.error.as_ref().is_none_or(|(at, _)| (x, y) < *at)
            {
                t.error = Some(((x, y), m));
            }
        }

        if let Some(s) = &r.strategy {
            let a = &mut t.strategy;
            a.dropped_by_rate += s.dropped_by_rate;
            a.dropped_by_gamma += s.dropped_by_gamma;
            a.dropped_as_needed += s.dropped_as_needed;
            a.coalesced_as_needed += s.coalesced_as_needed;
            a.detail_reduced += s.detail_reduced;
            a.tiny_polygons += s.tiny_polygons;
            a.truncated_zooms += s.truncated_zooms;
        }

        if !r.tilestats.is_empty() {
            self.tilestats.push(std::mem::take(&mut r.tilestats));
            // merged as they come, as merging does not depend on the order
            if self.tilestats.len() > 64 {
                let merged = merge_tilestats(std::mem::take(&mut self.tilestats));
                self.tilestats.push(merged);
            }
        }

        let (n, out) = (&mut t.next, r.th_out);
        n.gamma = n.gamma.max(out.gamma);
        n.mingap = n.mingap.max(out.mingap);
        n.minextent = n.minextent.max(out.minextent);
        n.mindrop_sequence = n.mindrop_sequence.max(out.mindrop_sequence);
        if descending {
            n.minattribute = n.minattribute.min(out.minattribute);
        } else {
            n.minattribute = n.minattribute.max(out.minattribute);
        }
        self.zoom_tile_size = self.zoom_tile_size.max(r.tile_size_out);
        self.zoom_feature_count = self.zoom_feature_count.max(r.feature_count_out);
        if r.still_dropping {
            t.extend_zooms = true;
        }
    }

    /// The tiles in tippecanoe's order within a zoom.
    fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.slots.len()).collect();
        order.sort_by_key(|&i| (self.slots[i].input.x, self.slots[i].input.y));
        order
    }
}

struct Driver<'d, 's> {
    ctx: &'s Ctx<'s>,
    sink: &'s dyn TileSink,
    scope: &'d rayon::Scope<'s>,
    tx: Sender<Done>,
    /// Tasks waiting for a thread, shallower levels first.
    ready: BinaryHeap<Reverse<Ready>>,
    seq: u64,
    /// How many tasks the pool holds at once: enough that no thread waits for
    /// the coordinator, few enough that shallower levels keep their priority.
    /// Tasks never wait on rayon work inside, so a queued task cannot be
    /// picked up by a thread in the middle of another.
    max_in_flight: usize,
    levels: Vec<Level>,
    maxzoom: i32,
    extend_zooms_max: i64,
    finalized: usize,
    in_flight: usize,
    largest_written: i32,
    stopped: bool,
    most: i64,
    stats: TileStats,
    tilestats: Vec<BTreeMap<String, LayermapEntry>>,
    error: Option<Error>,
    timing: bool,
    started: std::time::Instant,
    /// `-at`: levels are made one after the other, since whether a tile is
    /// made depends on the outcome of its parent's level.
    variable_depth: bool,
    /// The estimated memory of the big tiles running, and its limit.
    running_big: u64,
    big_memory: u64,
}

/// Runs the tiling: a coordinator on the calling thread that hands the tiles
/// to the pool and applies tippecanoe's zoom-level rules to the results in
/// zoom order.
pub fn drive<'s>(
    ctx: &'s Ctx<'s>,
    root: Option<TileInput>,
    sink: &'s dyn TileSink,
    scope: &rayon::Scope<'s>,
    threads: usize,
) -> crate::Result<TileStats> {
    let o = ctx.o;
    if let Some(what) = unsupported(ctx) {
        return Err(Error::new(
            EXIT_ARGS,
            format!("manby does not support {} yet", what),
        ));
    }

    let (tx, rx) = channel::<Done>();
    let mut d = Driver {
        ctx,
        sink,
        scope,
        tx,
        ready: BinaryHeap::new(),
        seq: 0,
        max_in_flight: (threads.max(1) * 4).min(tiles_at_once(o)),
        levels: Vec::new(),
        maxzoom: o.maxzoom,
        extend_zooms_max: o.extend_zooms_max,
        finalized: 0,
        in_flight: 0,
        largest_written: -1,
        stopped: false,
        most: 0,
        stats: TileStats::default(),
        tilestats: Vec::new(),
        error: None,
        timing: o.timing,
        started: std::time::Instant::now(),
        variable_depth: o.a(A_VARIABLE_DEPTH_PYRAMID),
        running_big: 0,
        big_memory: o.big_memory as u64,
    };

    if let Some(root) = root {
        d.levels.push(Level::new(
            o.iz,
            o.iz,
            o,
            Spool::new(&o.tmpdir, &ctx.rest),
            &ctx.rest,
        ));
        let at = (root.x, root.y);
        d.add_input(0, at, root);
        d.level_inputs_complete(0);
    }

    loop {
        d.pump();
        if d.in_flight == 0 {
            break;
        }
        let done = match recv_soon(&rx) {
            Some(e) => e,
            None => break,
        };
        if let Done::Made { .. } | Done::Failed(_) = done {
            d.in_flight -= 1;
        }
        if d.error.is_some() {
            d.ready.clear();
            continue;
        }
        match done {
            Done::Read {
                level,
                idx,
                children,
                complexity,
            } => d.on_read(level, idx, children, complexity),
            Done::Made {
                level,
                idx,
                pass,
                big,
                result,
                tile,
            } => {
                d.running_big -= big;
                d.on_made(level, idx, pass, *result, tile)
            }
            Done::Failed(e) => d.error = Some(e),
        }
    }

    if let Some(e) = d.error {
        return Err(e);
    }

    let mut stats = d.stats;
    if !stats.incomplete && stats.strategies.len() < (d.maxzoom + 1) as usize {
        stats
            .strategies
            .resize((d.maxzoom + 1) as usize, Strategy::default());
    }
    let mut maxzoom = d.maxzoom;
    if stats.incomplete {
        maxzoom = stats.maxzoom;
    } else if d.largest_written >= 0 && maxzoom > d.largest_written {
        maxzoom = d.largest_written;
    }
    stats.maxzoom = maxzoom;
    stats.layermap = merge_tilestats(d.tilestats);
    Ok(stats)
}

impl<'d, 's> Driver<'d, 's> {
    /// Hands ready tasks to the pool while threads are free.
    fn pump(&mut self) {
        let mut deferred = Vec::new();
        while self.in_flight < self.max_in_flight {
            let Some(Reverse(ready)) = self.ready.pop() else {
                break;
            };
            let big = ready.big;
            if big > 0 && self.running_big > 0 && self.running_big + big > self.big_memory {
                deferred.push(ready);
                continue;
            }
            self.running_big += big;
            self.in_flight += 1;
            let ctx = self.ctx;
            let tx = self.tx.clone();
            let t = self.task(&ready);
            self.scope.spawn(move |_| {
                // a task that panics fails the run; its message is printed
                // already
                let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_task(ctx, t, &tx)
                }))
                .unwrap_or_else(|_| Err(Error::new(EXIT_IMPOSSIBLE, String::new())));
                if done.is_err() {
                    ctx.failed.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                let _ = tx.send(done.unwrap_or_else(Done::Failed));
            });
        }
        for r in deferred {
            self.ready.push(Reverse(r));
        }
    }

    /// Adds a tile to level `k`, from its parent at `parent`.
    fn add_input(&mut self, k: usize, parent: (u32, u32), input: TileInput) {
        let level = &mut self.levels[k];
        let idx = level.slots.len();
        level.origins.push((parent, (input.x, input.y)));
        level.slots.push(Slot {
            input: Arc::new(input),
            read: false,
            complexity: None,
            made: None,
            tile: None,
            children_must_go_on: None,
            held: Vec::new(),
        });
        level.unread += 1;
        level.missing += 1;
        if level.started {
            self.spawn_tile(k, idx);
        } else {
            level.waiting.push(idx);
        }
    }

    /// Starts level `k` if it may start: its next zoom is known, its tiles
    /// may be made, and it is not too far ahead of the finished levels.
    fn try_start(&mut self, k: usize) {
        let level = &self.levels[k];
        if level.started
            || self.stopped
            || k >= self.finalized + LOOKAHEAD
            || level.nextzoom.is_none()
            || level.z > self.maxzoom
        {
            return;
        }
        self.levels[k].started = true;
        for idx in std::mem::take(&mut self.levels[k].waiting) {
            self.spawn_tile(k, idx);
        }
    }

    /// Queues the reading and making of tile `idx` of level `k` in the
    /// level's current pass, or skips it.
    fn spawn_tile(&mut self, k: usize, idx: usize) {
        let o = self.ctx.o;
        let level = &mut self.levels[k];
        let pass = level.pass;
        let slot = &mut level.slots[idx];
        if slot.input.skip_unless_dropping
            && !dropping_features(&level.th, o.drop_by_attribute_descending)
        {
            let th = level.th;
            level.fold(idx, skipped(th), o.drop_by_attribute_descending);
            level.missing -= 1;
            return;
        }
        let ws = slot.input.working_set(self.ctx.store, o);
        level.outstanding += 1;
        self.seq += 1;
        self.ready.push(Reverse(Ready {
            key: (k, self.seq),
            idx: idx as u32,
            pass: pass as u32,
            maxzoom: self.maxzoom,
            extend_zooms_max: self.extend_zooms_max,
            big: if ws >= BIG_TASK.min(self.big_memory / 64) {
                ws
            } else {
                0
            },
        }));
    }

    /// The task of a queued tile. Nothing it is made from changes while the
    /// tile waits, apart from what `Ready` keeps: a level's thresholds and
    /// its tiles' reads change only once its pass is done.
    fn task(&self, r: &Ready) -> Task {
        let o = self.ctx.o;
        let k = r.key.0;
        let idx = r.idx as usize;
        let pass = r.pass as usize;
        let level = &self.levels[k];
        let slot = &level.slots[idx];
        let loop_z = if pass == 0 { level.loop_z } else { level.z };
        let simplification = if loop_z == r.maxzoom && o.maxzoom_simplification > 0.0 {
            o.maxzoom_simplification
        } else {
            o.simplification
        };
        Task {
            level: k,
            idx,
            pass,
            big: r.big,
            spool: level.spool.clone().expect(LIVE),
            child_spool: level.child_spool.clone().expect(LIVE),
            input: slot.input.clone(),
            rp: ReadParams {
                nextzoom: level.nextzoom.unwrap(),
                maxzoom: r.maxzoom,
                extend_zooms_max: r.extend_zooms_max,
                children: !slot.read,
            },
            complexity: slot.complexity,
            mp: MakeParams {
                maxzoom: r.maxzoom,
                simplification,
                th: level.th,
            },
        }
    }

    /// All the inputs of level `k` are known: work out what tippecanoe's
    /// thread count would be, and from it the next zoom.
    fn level_inputs_complete(&mut self, k: usize) {
        let o = self.ctx.o;
        let level = &mut self.levels[k];
        level.inputs_complete = true;

        let useful_threads = if k == 0 {
            1
        } else {
            let prev = &self.levels[k - 1];
            let threads = prev.threads;
            let cs = prev.child_shards as u32;
            let mzi = prev.max_zoom_increment as u32;
            let mut parents: Vec<(u32, u32)> =
                self.levels[k].origins.iter().map(|(p, _)| *p).collect();
            parents.sort();
            parents.dedup();
            let mut shards = std::collections::BTreeSet::new();
            for (parent, child) in &self.levels[k].origins {
                let slot = parents.binary_search(parent).unwrap() % threads;
                let j = (child.0.wrapping_shl(mzi) | (child.1 & ((1u32 << mzi) - 1)))
                    & cs.wrapping_sub(1);
                shards.insert(slot * cs as usize + j as usize);
            }
            shards.len()
        };

        let level = &mut self.levels[k];
        level.threads = tippecanoe_threads(o, useful_threads);
        level.child_shards = (o.temp_files / level.threads) as i32;
        level.max_zoom_increment = ((level.child_shards as f64).ln() / 4f64.ln()) as i32;
        if level.child_shards < 4 || level.max_zoom_increment < 1 {
            self.error = Some(Error::new(
                EXIT_IMPOSSIBLE,
                format!(
                    "Internal error: {} shards, max zoom increment {}",
                    level.child_shards, level.max_zoom_increment
                ),
            ));
            return;
        }
        if (((level.child_shards - 1) << 1) & level.child_shards) != level.child_shards {
            self.error = Some(Error::new(
                EXIT_IMPOSSIBLE,
                format!(
                    "Internal error: {} shards not a power of 2",
                    level.child_shards
                ),
            ));
            return;
        }
        if level.nextzoom.is_none() {
            let mut nextzoom = level.z + 1;
            if nextzoom < o.minzoom {
                if level.z + level.max_zoom_increment > o.minzoom {
                    nextzoom = o.minzoom;
                } else {
                    nextzoom = level.z + level.max_zoom_increment;
                }
            }
            level.nextzoom = Some(nextzoom);
        }

        self.try_start(k);
        self.check_children_complete(k);
        self.check_pass(k);
    }

    /// The next level's inputs are complete once this level is all read.
    fn check_children_complete(&mut self, k: usize) {
        let level = &self.levels[k];
        if self.variable_depth || !level.inputs_complete || level.unread > 0 {
            return;
        }
        if k + 1 < self.levels.len() && !self.levels[k + 1].inputs_complete {
            self.level_inputs_complete(k + 1);
        }
    }

    fn on_read(&mut self, k: usize, idx: usize, children: Vec<TileInput>, complexity: i64) {
        let o = self.ctx.o;
        let level = &mut self.levels[k];
        let parent = (level.slots[idx].input.x, level.slots[idx].input.y);
        let slot = &mut level.slots[idx];
        slot.read = true;
        slot.complexity = Some(complexity);
        level.unread -= 1;
        if self.variable_depth {
            slot.held = children;
        } else if !children.is_empty() {
            if k + 1 == self.levels.len() {
                let z = children[0].z;
                let spool = self.levels[k].child_spool.clone().expect(LIVE);
                let mut next = Level::new(z, self.levels[k].z + 1, o, spool, &self.ctx.rest);
                if z + 1 >= o.minzoom {
                    next.nextzoom = Some(z + 1);
                }
                self.levels.push(next);
                self.try_start(k + 1);
            }
            for child in children {
                self.add_input(k + 1, parent, child);
            }
        }
        self.check_children_complete(k);
    }

    fn on_made(
        &mut self,
        k: usize,
        idx: usize,
        pass: usize,
        result: MakeResult,
        tile: Option<Stored>,
    ) {
        let level = &mut self.levels[k];
        debug_assert_eq!(pass, level.pass);
        level.outstanding -= 1;
        level.missing -= 1;
        let slot = &mut level.slots[idx];
        slot.tile = tile;
        if slot.children_must_go_on.is_none() {
            slot.children_must_go_on = Some(result.children_must_go_on);
        }
        level.fold(idx, result, self.ctx.o.drop_by_attribute_descending);
        self.check_pass(k);
    }

    fn check_pass(&mut self, k: usize) {
        let level = &self.levels[k];
        if level.done
            || !level.started
            || !level.inputs_complete
            || !level.waiting.is_empty()
            || level.missing > 0
        {
            return;
        }
        self.complete_pass(k);
    }

    /// Every tile of level `k` has been made in this pass: apply tippecanoe's
    /// rules for the zoom level.
    fn complete_pass(&mut self, k: usize) {
        let o = self.ctx.o;
        let maxzoom = self.maxzoom;
        let level = &mut self.levels[k];
        let fresh = Totals::new(level.z, level.th);
        let totals = std::mem::replace(&mut level.totals, fresh);
        let z = totals.z;
        let th = level.th;
        // With --as-needed-per-tile, each tile keeps the thresholds it raised
        // for itself, and the level is not made again with the highest.
        let next = if o.as_needed_per_tile {
            th
        } else {
            totals.next
        };
        let again = next.gamma > th.gamma
            || next.mingap > th.mingap
            || next.minextent > th.minextent
            || next.mindrop_sequence > th.mindrop_sequence
            || next.minattribute != th.minattribute;
        level.th = next;
        level.totals.next = next;
        let extend_zooms = totals.extend_zooms;
        let mut strategy = totals.strategy;
        level.history.push((maxzoom, totals.history));
        let error = totals.error.map(|(_, m)| m);

        if let Some(message) = error {
            self.error = Some(Error::new(EXIT_INCOMPLETE, message));
            return;
        }

        if extend_zooms
            && (o.a(A_EXTEND_ZOOMS) || self.extend_zooms_max > 0)
            && z == self.maxzoom
            && self.maxzoom < MAX_ZOOM
        {
            self.maxzoom += 1;
            if self.extend_zooms_max > 0 {
                self.extend_zooms_max -= 1;
            }
        }

        let level = &mut self.levels[k];
        strategy.tile_size = level.zoom_tile_size;
        strategy.feature_count = level.zoom_feature_count;
        level.strategy = strategy;
        level.strategy_z = z;

        let level = &mut self.levels[k];
        if self.timing {
            use std::sync::atomic::Ordering::Relaxed;
            let c: Vec<u64> = self
                .ctx
                .counters
                .iter()
                .map(|c| c.swap(0, Relaxed))
                .collect();
            eprintln!(
                "{:.3}s: z{} pass {} done: {} tiles; since last: {} attempts, {} compressions of {} MB taking {:.3}s",
                self.started.elapsed().as_secs_f64(),
                level.z,
                level.pass,
                level.slots.len(),
                c[0],
                c[1],
                c[2] >> 20,
                c[3] as f64 / 1e9
            );
        }

        if again {
            level.pass += 1;
            let th = level.th;
            let filters = o.prefilter.is_some() || o.postfilter.is_some();
            let reusable = !self.variable_depth && !filters;
            for idx in 0..self.levels[k].slots.len() {
                let level = &mut self.levels[k];
                let made = level.slots[idx].made.take();
                level.missing += 1;
                let reuse = made
                    .and_then(|m| m.reuse.map(|r| (m.len, r)))
                    .filter(|(_, r)| {
                        reusable
                            && r.th.gamma == th.gamma
                            && r.th.minextent == th.minextent
                            && r.th.minattribute == th.minattribute
                            && r.gap >= th.mingap
                            && r.drop_sequence > th.mindrop_sequence
                    });
                match reuse {
                    // a higher threshold leaves the tile as it is
                    Some((len, r)) => {
                        let result = MakeResult {
                            len,
                            strategy: r.strategy,
                            reusable: Some(th),
                            reuse_gap: r.gap,
                            reuse_drop_sequence: r.drop_sequence,
                            ..skipped(th)
                        };
                        let result = MakeResult {
                            skipped: false,
                            ..result
                        };
                        level.fold(idx, result, o.drop_by_attribute_descending);
                        level.missing -= 1;
                    }
                    None => self.spawn_tile(k, idx),
                }
            }
            self.check_pass(k);
            return;
        }

        level.done = true;
        if self.variable_depth {
            self.release_children(k);
        }
        self.try_finalize();

        // a level beyond the old maximum zoom may now be made
        for j in 0..self.levels.len() {
            if !self.levels[j].slots.is_empty() && self.levels[j].inputs_complete {
                self.try_start(j);
                self.check_pass(j);
            }
        }
    }

    /// With `-at`, once level `k` is done: the children of the tiles that
    /// wrote theirs become the next level, those of tiles that stopped early
    /// to be made only if their level drops features.
    fn release_children(&mut self, k: usize) {
        let o = self.ctx.o;
        if self.levels[k].err {
            return;
        }
        let order = self.levels[k].order();
        let level = &mut self.levels[k];

        let mut next: Vec<((u32, u32), TileInput)> = Vec::new();
        for &i in &order {
            let slot = &mut level.slots[i];
            let r = slot.made.as_ref().unwrap();
            if r.skipped {
                continue;
            }
            let (z, x, y) = (slot.input.z, slot.input.x, slot.input.y);
            let must_go_on = slot.children_must_go_on.unwrap_or(false);
            for mut child in std::mem::take(&mut slot.held) {
                child.skip_unless_dropping = r.truncated && child.z == z + 1;
                if must_go_on {
                    // large enough not to try to stop early
                    child.complexity = Some(1i64 << 32);
                }
                next.push(((x, y), child));
            }
        }
        if next.is_empty() {
            return;
        }

        debug_assert_eq!(k + 1, self.levels.len());
        let z = next[0].1.z;
        let spool = self.levels[k].child_spool.clone().expect(LIVE);
        let mut nl = Level::new(z, self.levels[k].z + 1, o, spool, &self.ctx.rest);
        if z + 1 >= o.minzoom {
            nl.nextzoom = Some(z + 1);
        }
        self.levels.push(nl);
        for (parent, child) in next {
            self.add_input(k + 1, parent, child);
        }
        self.level_inputs_complete(k + 1);
    }

    /// Writes out the finished levels in zoom order.
    fn try_finalize(&mut self) {
        while self.finalized < self.levels.len() && self.levels[self.finalized].done {
            if self.stopped {
                return;
            }
            let k = self.finalized;
            if let Err(e) = self.finalize(k) {
                self.error = Some(e);
                return;
            }
            self.finalized += 1;
            for j in self.finalized..self.levels.len() {
                self.try_start(j);
            }
        }

        // levels deeper than the maximum zoom are never made once every level
        // that could extend the maximum zoom is done
        if self.finalized < self.levels.len()
            && !self.levels[self.finalized].done
            && self.levels[self.finalized].z > self.maxzoom
        {
            self.stopped = true;
        }
    }

    fn finalize(&mut self, k: usize) -> crate::Result<()> {
        let t0 = std::time::Instant::now();
        let r = self.finalize1(k);
        if self.timing {
            eprintln!(
                "{:.3}s: finalize z{} took {:.3}s",
                self.started.elapsed().as_secs_f64(),
                self.levels[k].z,
                t0.elapsed().as_secs_f64()
            );
        }
        r
    }

    fn finalize1(&mut self, k: usize) -> crate::Result<()> {
        let order = self.levels[k].order();
        let level = &mut self.levels[k];

        self.most = 0;
        for (pass_maxzoom, tiles) in &level.history {
            for &(z, x, y, len) in tiles {
                if z == *pass_maxzoom {
                    let a: u64 = ((x as u64) << 32) | y as u64;
                    let b: u64 = ((self.stats.midx as u64) << 32) | self.stats.midy as u64;
                    if len > self.most || (len == self.most && a < b) {
                        self.stats.midx = x;
                        self.stats.midy = y;
                        self.most = len;
                    }
                }
            }
        }

        let z = level.strategy_z;
        if z as usize >= self.stats.strategies.len() {
            self.stats
                .strategies
                .resize(z as usize + 1, Strategy::default());
        }
        self.stats.strategies[z as usize] = level.strategy;
        if level.wrote && level.z > self.largest_written {
            self.largest_written = level.z;
        }

        for &i in &order {
            let slot = &level.slots[i];
            if let Some(tile) = &slot.tile {
                let bytes = tile.load_sequential()?;
                self.sink
                    .write_tile(slot.input.z as u8, slot.input.x, slot.input.y, &bytes)?;
                self.stats.tiles += 1;
            }
        }

        self.tilestats.append(&mut level.tilestats);
        if self.tilestats.len() > 64 {
            let merged = merge_tilestats(std::mem::take(&mut self.tilestats));
            self.tilestats.push(merged);
        }

        if level.err {
            self.stopped = true;
            self.stats.incomplete = true;
            self.stats.maxzoom = level.z - 1;
        }

        // free the level's memory
        level.slots = Vec::new();
        level.history = Vec::new();
        if let Some(spool) = level.spool.take() {
            for l in &mut self.levels {
                if l.child_spool
                    .as_ref()
                    .is_some_and(|c| Arc::ptr_eq(c, &spool))
                {
                    l.child_spool = None;
                }
            }
        }
        Ok(())
    }
}

/// How long the coordinator waits for the next result before it sleeps.
const COORDINATOR_SPIN: std::time::Duration = std::time::Duration::from_micros(200);

/// The next result, waited for awake for a moment before sleeping. Results
/// come every few microseconds while small tiles are being made, and each
/// time the coordinator sleeps, waking it costs more than that (much more
/// in a virtual machine, where it wakes a virtual processor), while the
/// pool runs out of tasks.
fn recv_soon(rx: &Receiver<Done>) -> Option<Done> {
    let started = std::time::Instant::now();
    loop {
        match rx.try_recv() {
            Ok(done) => return Some(done),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) if started.elapsed() > COORDINATOR_SPIN => {
                return rx.recv().ok();
            }
            Err(TryRecvError::Empty) => std::hint::spin_loop(),
        }
    }
}

/// Reads a tile, reporting the first read at once, and makes it. Its children
/// and the tile itself go into the spools while they wait.
fn run_task(ctx: &Ctx<'_>, t: Task, tx: &Sender<Done>) -> crate::Result<Done> {
    let mut rt = read_tile(ctx, &t.input, &t.rp, t.complexity)?;
    if t.rp.children {
        let mut children = std::mem::take(&mut rt.children);
        store_inputs(&mut children, &t.child_spool)?;
        let _ = tx.send(Done::Read {
            level: t.level,
            idx: t.idx,
            children,
            complexity: rt.complexity,
        });
    }
    let mut result = make_tile(ctx, &rt, &t.mp);
    drop(rt);
    let tile = match result.tile.take() {
        Some(bytes) => Some(t.spool.store(&bytes)?),
        None => None,
    };
    Ok(Done::Made {
        level: t.level,
        idx: t.idx,
        pass: t.pass,
        big: t.big,
        result: Box::new(result),
        tile,
    })
}

/// The result for a tile skipped because its parent stopped early: tippecanoe
/// reads past it and counts it as one vertex.
fn skipped(th: Thresholds) -> MakeResult {
    MakeResult {
        len: 1,
        tile: None,
        strategy: None,
        th_out: th,
        tile_size_out: 0,
        feature_count_out: 0,
        still_dropping: false,
        tilestats: BTreeMap::new(),
        error: None,
        reusable: None,
        reuse_gap: u64::MAX,
        reuse_drop_sequence: u64::MAX,
        truncated: false,
        children_must_go_on: false,
        skipped: true,
    }
}

/// Merges the attribute statistics of several tiles. Every entry came from
/// tiling, so each stays marked as one to keep, as tippecanoe's tiling
/// segments are until the final merge.
fn merge_tilestats(maps: Vec<BTreeMap<String, LayermapEntry>>) -> BTreeMap<String, LayermapEntry> {
    match maps.len() {
        0 => BTreeMap::new(),
        1 => maps.into_iter().next().unwrap(),
        _ => {
            let mut merged = merge_layermaps(&maps);
            for e in merged.values_mut() {
                e.retain = 1;
            }
            merged
        }
    }
}
