//! Sorting the store into tippecanoe's order and assigning each feature its
//! minimum zoom. See DESIGN.md.
//!
//! tippecanoe sorts the features by index and sequence with a radix sort
//! through temporary files and assigns the minimum zooms as it merges the
//! sorted runs. The keys are unique, so a parallel sort in memory gives the
//! same order, and the assignment is one pass over it in that order. The
//! guessing of `-zg`, `-Bg` and `-rg` follows main.cpp's `read_input`.

use std::io::Write;

use tippecanoe::errors::EXIT_NODATA;
use tippecanoe::geometry::{VT_LINE, VT_POINT, VT_POLYGON};
use tippecanoe::globals::MAX_ZOOM;
use tippecanoe::options::*;
use tippecanoe::projection::{decode_hilbert, decode_quadkey};
use tippecanoe::tile::manage_gap;

use crate::Error;
use crate::options::Options;
use crate::store::{Feature, FeatureStore};

/// Zoom levels and other choices that depend on the whole input, such as the
/// results of `-zg` and `-Bg`.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub minzoom: i32,
    pub maxzoom: i32,
    pub basezoom: i32,
    pub droprate: f64,
    /// tippecanoe's `geometry_scale`: the geometry in the store is in
    /// multiples of `1 << geometry_scale`.
    pub geometry_scale: i32,
    /// `-k`, or what `-kg` chose.
    pub cluster_maxzoom: i32,
    /// The first tile, from choose_first_zoom or `--one-tile`.
    pub iz: u32,
    pub ix: u32,
    pub iy: u32,
    /// With `-at`, the first tile's `estimated_complexity`: the size of
    /// tippecanoe's zoom 0 geometry file. 0 without `-at`.
    pub estimated_complexity: i64,
}

#[derive(Clone, Copy, Debug, Default)]
struct DropState {
    previndex: u64,
    interval: f64,
    /// floating point because interval is
    seq: f64,
}

fn prep_drop_states(maxzoom: i32, basezoom: i32, droprate: f64) -> Vec<DropState> {
    let mut ds = vec![DropState::default(); (maxzoom + 1) as usize];
    // Needs to be signed for interval calculation
    for (i, d) in ds.iter_mut().enumerate() {
        let i = i as isize;
        if i < basezoom as isize {
            d.interval = (droprate.ln() * (basezoom as isize - i) as f64).exp();
        }
    }
    ds
}

fn calc_feature_minzoom(
    ix: u64,
    t: i32,
    ds: &mut [DropState],
    maxzoom: i32,
    gamma: f64,
    o: &Options,
) -> i32 {
    let mut feature_minzoom: i32 = 0;

    if gamma >= 0.0
        && (t == VT_POINT
            || (o.additional.get(A_LINE_DROP) && t == VT_LINE)
            || (o.additional.get(A_POLYGON_DROP) && t == VT_POLYGON))
    {
        let maxzoom = maxzoom as isize;
        for i in (0..=maxzoom).rev() {
            ds[i as usize].seq += 1.0;
        }
        for i in (0..=maxzoom).rev() {
            if ds[i as usize].seq < 0.0 {
                feature_minzoom = (i + 1) as i32;

                // The feature we are pushing out
                // appears in zooms i + 1 through maxzoom,
                // so track where that was so we can make sure
                // not to cluster something else that is *too*
                // far away into it.
                for d in ds
                    .iter_mut()
                    .take(maxzoom as usize + 1)
                    .skip(i as usize + 1)
                {
                    d.previndex = ix;
                }

                break;
            } else {
                ds[i as usize].seq -= ds[i as usize].interval;
            }
        }

        // If this feature has been chosen only for a high zoom level,
        // check whether at a low zoom level it is nevertheless too far
        // from the last feature chosen for that low zoom, in which case
        // we will go ahead and push it out.

        let threshold = o.preserve_point_density_threshold;
        if threshold > 0 {
            let mut i: isize = 0;
            while i < feature_minzoom as isize && i < maxzoom {
                let a = ((1i64 << (32 - i)) as u64) / threshold;
                if ix.wrapping_sub(ds[i as usize].previndex) > a.wrapping_mul(a) {
                    feature_minzoom = i as i32;

                    for d in ds.iter_mut().take(maxzoom as usize + 1).skip(i as usize) {
                        d.previndex = ix;
                    }

                    break;
                }
                i += 1;
            }
        }
    }

    feature_minzoom
}

pub fn round_droprate(r: f64) -> f64 {
    (r * 100000.0).round() / 100000.0
}

/// tippecanoe's `choose_first_zoom`: the deepest tile at or above `minzoom`
/// that holds everything. Also clamps the bounds as it does.
fn choose_first_zoom(file_bbox: &mut [i64; 4], minzoom: i32, buffer: i32) -> (u32, u32, u32) {
    // If the bounding box extends off the plane on either side,
    // a feature wrapped across the date line, so the width of the
    // bounding box is the whole world.
    if file_bbox[0] < 0 {
        file_bbox[0] = 0;
        file_bbox[2] = (1i64 << 32) - 1;
    }
    if file_bbox[2] > (1i64 << 32) - 1 {
        file_bbox[0] = 0;
        file_bbox[2] = (1i64 << 32) - 1;
    }
    if file_bbox[1] < 0 {
        file_bbox[1] = 0;
    }
    if file_bbox[3] > (1i64 << 32) - 1 {
        file_bbox[3] = (1i64 << 32) - 1;
    }

    for z in (0..=minzoom as isize).rev() {
        let shift: i64 = 1i64 << (32 - z);
        let b = buffer as i64;

        let left = (file_bbox[0] - b * shift / 256) / shift;
        let top = (file_bbox[1] - b * shift / 256) / shift;
        let right = (file_bbox[2] + b * shift / 256) / shift;
        let bottom = (file_bbox[3] + b * shift / 256) / shift;

        if left == right && top == bottom {
            return (z as u32, left as u32, top as u32);
        }
    }
    (0, 0, 0)
}

/// What the tile counting of the base zoom guess keeps per zoom.
#[derive(Clone, Copy, Debug, Default)]
struct GuessTile {
    x: u32,
    y: u32,
    count: i64,
    fullcount: i64,
    gap: f64,
    previndex: u64,
}

/// tippecanoe's messages along the way.
struct Out {
    quiet: bool,
    err: Vec<u8>,
}

impl Out {
    fn note(&mut self, s: String) {
        if !self.quiet {
            self.err.extend_from_slice(s.as_bytes());
        }
    }

    fn always(&mut self, s: String) {
        self.err.extend_from_slice(s.as_bytes());
    }

    fn stdout(&mut self, s: String) {
        self.flush();
        print!("{}", s);
        let _ = std::io::stdout().flush();
    }

    fn flush(&mut self) {
        if !self.err.is_empty() {
            let _ = std::io::stderr().write_all(&self.err);
            self.err.clear();
        }
    }
}

/// Sorts `store.features` and fills in `feature_minzoom`. Returns the zoom plan.
pub fn order(store: &mut FeatureStore, options: &Options) -> crate::Result<Plan> {
    let threads = options.thread_count();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| Error::new(tippecanoe::errors::EXIT_PTHREAD, e.to_string()))?;
    let mut out = Out {
        quiet: options.quiet,
        err: Vec::new(),
    };
    let r = pool.install(|| order1(store, options, &mut out));
    out.flush();
    r
}

fn order1(store: &mut FeatureStore, o: &Options, out: &mut Out) -> crate::Result<Plan> {
    let mut maxzoom = o.maxzoom;
    let minzoom = o.minzoom;
    let mut basezoom = o.basezoom;
    let mut droprate = o.droprate;
    let full_detail = o.full_detail;
    let low_detail = o.low_detail;
    let mut cluster_maxzoom = o.cluster_maxzoom;

    let (mut iz, mut ix, mut iy) = choose_first_zoom(&mut store.bbox, minzoom, o.buffer);
    if let Some((x, y)) = o.one_tile {
        iz = minzoom as u32;
        ix = x;
        iy = y;
    }

    store.sort_features();
    store.lay_out_in_order(o.memory_budget() / 4);
    store.number_values_in_order(o.memory_budget() / 4);

    let features = &store.features;
    let indices = features.len() as i64;
    if indices == 0 {
        return Err(Error::new(EXIT_NODATA, "Did not read any valid geometries"));
    }

    let dist_sum = store.dist_sum;
    let dist_count = store.dist_count;
    let area_sum = store.area_sum;

    if o.guess_maxzoom {
        let mut mean: f64 = 0.0;
        let mut count: usize = 0;
        let mut m2: f64 = 0.0;
        let mut dupes: usize = 0;

        for ip in 1..indices as usize {
            let (here, before) = (features[ip].ix, features[ip - 1].ix);
            if here != before {
                // https://en.wikipedia.org/wiki/Algorithms_for_calculating_variance#Welford's_online_algorithm
                let new_value = ((here - before) as f64).ln();
                count += 1;
                let delta = new_value - mean;
                mean += delta / count as f64;
                let delta2 = new_value - mean;
                m2 += delta * delta2;
            } else {
                dupes += 1;
            }
        }

        if count == 0 && dist_count == 0 && o.minimum_maxzoom == 0 {
            return Err(Error::new(
                EXIT_NODATA,
                "Can't guess maxzoom (-zg) without at least two distinct feature locations",
            ));
        }

        if count == 0 && dist_count == 0 {
            maxzoom = o.minimum_maxzoom;
            if droprate < 0.0 {
                droprate = 1.0;
            }
        } else if count > 0 {
            let stddev = (m2 / count as f64).sqrt();

            // Geometric mean is appropriate because distances between features
            // are typically lognormally distributed. Two standard deviations
            // below the mean should be enough to distinguish most features.
            let avg = mean.exp();
            let nearby = (mean - 1.5 * stddev).exp();

            // Convert approximately from tile units to feet.
            // See empirical data above for source
            let dist_ft = avg.sqrt() / 33.0;
            let nearby_ft = nearby.sqrt() / 33.0;

            // Go one zoom level beyond what is strictly necessary for nearby features.
            let want = nearby_ft / 2.0;

            maxzoom =
                ((360.0 / (0.00000274 * want)).ln() / 2f64.ln() - full_detail as f64).ceil() as i32;
            if maxzoom < 0 {
                maxzoom = 0;
            }
            out.note(format!(
                "Choosing a maxzoom of -z{} for features typically {} feet ({} meters) apart, ",
                maxzoom,
                dist_ft.ceil() as i32,
                (dist_ft / 3.28084).ceil() as i32
            ));
            out.note(format!(
                "and at least {} feet ({} meters) apart\n",
                nearby_ft.ceil() as i32,
                (nearby_ft / 3.28084).ceil() as i32
            ));

            let cluster_distance = o.cluster_distance;
            let mut changed = false;
            while maxzoom < 32 - full_detail
                && maxzoom < 33 - low_detail
                && maxzoom < cluster_maxzoom
                && cluster_distance > 0
            {
                let a = (1i64 << (32 - maxzoom)) / 256 * cluster_distance as i64;
                let zoom_mingap: u64 = a.wrapping_mul(a) as u64;
                if avg > zoom_mingap as f64 {
                    break;
                }

                maxzoom += 1;
                changed = true;
            }
            if changed {
                out.stdout(format!(
                    "Choosing a maxzoom of -z{} to keep most features distinct with cluster distance {} and cluster maxzoom {}\n",
                    maxzoom, cluster_distance, cluster_maxzoom
                ));
            }

            if droprate == -3.0 {
                // This mysterious formula is the result of eyeballing the appropriate drop rate
                // for several point tilesets using -zg and then fitting a curve to the pattern
                // that emerged.
                droprate = round_droprate((-0.7681 * stddev.ln() + 1.582).exp());

                if droprate < 0.0 {
                    droprate = 0.0;
                }

                out.note(format!("Choosing a drop rate of {:.6}\n", droprate));

                if dupes != 0 && droprate != 0.0 {
                    maxzoom = (maxzoom as f64
                        + ((((dupes + count) / count) as f64).ln() / droprate.ln()).round())
                        as i32;
                    out.note(format!(
                        "Increasing maxzoom to {} to account for {} duplicate feature locations\n",
                        maxzoom, dupes
                    ));
                }
            }
        }

        if dist_count != 0 {
            // no conversion to pseudo-feet here because that already happened within each feature
            let want2 = (dist_sum / dist_count as f64).exp() / 8.0;
            let mz = ((360.0 / (0.00000274 * want2)).ln() / 2f64.ln() - full_detail as f64).ceil()
                as i32;

            if mz > maxzoom || count == 0 {
                out.note(format!(
                    "Choosing a maxzoom of -z{} for resolution of about {} feet ({} meters) within features\n",
                    mz,
                    (dist_sum / dist_count as f64).exp() as i32,
                    ((dist_sum / dist_count as f64).exp() / 3.28084) as i32
                ));
                maxzoom = mz;
            }
        }

        if maxzoom < 0 {
            maxzoom = 0;
        }
        if maxzoom > 32 - full_detail {
            maxzoom = 32 - full_detail;
        }
        if maxzoom > 33 - low_detail {
            // that is, maxzoom - 1 > 32 - low_detail
            maxzoom = 33 - low_detail;
        }
        if maxzoom > o.maximum_maxzoom {
            out.note(format!(
                "Using largest maxzoom guess of -z{}\n",
                o.maximum_maxzoom
            ));
            maxzoom = o.maximum_maxzoom;
        }

        let mut total_tile_count: f64 = 0.0;
        for i in 1..=maxzoom {
            let tile_count = (area_sum / (((1i64 << (32 - i)) * (1i64 << (32 - i))) as f64)).ceil();
            total_tile_count += tile_count;

            // 2M tiles is an arbitrary limit, chosen to make tiling jobs
            // that seem like they should finish in a few minutes
            // actually finish in a few minutes.
            if total_tile_count > (2 * 1024 * 1024) as f64 {
                out.stdout(format!(
                    "Limiting maxzoom to -z{} to keep from generating {} tiles\n",
                    i - 1,
                    total_tile_count as i64
                ));
                maxzoom = i - 1;
                break;
            }
        }

        if basezoom == -2 && o.basezoom_marker_width == 1.0 {
            // -Bg, not -Bg###
            basezoom = maxzoom;
            out.note(format!("Using base zoom of -z{}\n", basezoom));
        }

        if maxzoom < o.minimum_maxzoom {
            out.note(format!(
                "Using minimum maxzoom of -z{}\n",
                o.minimum_maxzoom
            ));
            maxzoom = o.minimum_maxzoom;
        }

        if maxzoom < minzoom {
            out.note(format!(
                "Can't use {} for maxzoom because minzoom is {}\n",
                maxzoom, minzoom
            ));
            maxzoom = minzoom;
        }

        if basezoom == -1 {
            // basezoom unspecified
            basezoom = maxzoom;
        }
    }

    if cluster_maxzoom >= maxzoom && o.guess_cluster_maxzoom {
        cluster_maxzoom = maxzoom - 1;
        out.always(format!(
            "Choosing a cluster maxzoom of -k{} to make all features visible at maximum zoom {}\n",
            cluster_maxzoom, maxzoom
        ));
    }

    if basezoom < 0 || droprate < 0.0 {
        guess_basezoom_droprate(features, o, maxzoom, &mut basezoom, &mut droprate, out);
    }

    // tippecanoe assigns the minimum zooms as it merges, and again after
    // guessing, with the guessed values; one pass with the final values
    // gives the same.
    assign_minzooms(&mut store.features, o, maxzoom, basezoom, droprate);

    let estimated_complexity = if o.additional.get(A_VARIABLE_DEPTH_PYRAMID) {
        zoom0_size(store.serialized_bytes, iz, ix, iy)
    } else {
        0
    };

    Ok(Plan {
        minzoom,
        maxzoom,
        basezoom,
        droprate,
        geometry_scale: o.geometry_scale,
        cluster_maxzoom,
        iz,
        ix,
        iy,
        estimated_complexity,
    })
}

/// The size of tippecanoe's zoom 0 geometry file: the complexity placeholder,
/// the first tile's zoom, x and y, the features, and the end marker.
fn zoom0_size(feature_bytes: u64, iz: u32, ix: u32, iy: u32) -> i64 {
    let mut header = Vec::new();
    tippecanoe::serial::serialize_int(&mut header, iz as i32);
    tippecanoe::serial::serialize_uint(&mut header, ix);
    tippecanoe::serial::serialize_uint(&mut header, iy);
    8 + header.len() as i64 + feature_bytes as i64 + 1
}

/// The base zoom and drop rate guessing of `-Bg` and `-rg`.
fn guess_basezoom_droprate(
    features: &[Feature],
    o: &Options,
    maxzoom: i32,
    basezoom: &mut i32,
    droprate: &mut f64,
    out: &mut Out,
) {
    let gamma = o.gamma;
    let decode = if o.additional.get(A_HILBERT) {
        decode_hilbert
    } else {
        decode_quadkey
    };
    let mut tile = [GuessTile::default(); MAX_ZOOM as usize + 1];
    let mut max = [GuessTile::default(); MAX_ZOOM as usize + 1];

    for f in features {
        let (xx, yy) = decode(f.ix);

        for z in 0..=MAX_ZOOM {
            let zu = z as usize;
            let (mut xxx, mut yyy) = (0u32, 0u32);
            if z != 0 {
                // These are tile numbers, not pixels,
                // so shift, not round
                xxx = xx >> (32 - z);
                yyy = yy >> (32 - z);
            }

            let scale = (1i64 << (64 - 2 * (z + 8))) as f64;

            if tile[zu].x != xxx || tile[zu].y != yyy {
                if tile[zu].count > max[zu].count {
                    max[zu] = tile[zu];
                }

                tile[zu] = GuessTile {
                    x: xxx,
                    y: yyy,
                    ..Default::default()
                };
            }

            tile[zu].fullcount += 1;

            if manage_gap(
                f.ix,
                &mut tile[zu].previndex,
                scale,
                gamma,
                &mut tile[zu].gap,
            ) != 0
            {
                continue;
            }

            tile[zu].count += 1;
        }
    }

    for z in (0..=MAX_ZOOM as usize).rev() {
        if tile[z].count > max[z].count {
            max[z] = tile[z];
        }
    }

    let max_features = (50000.0 / (o.basezoom_marker_width * o.basezoom_marker_width)) as i32;

    let obasezoom = *basezoom;
    if *basezoom < 0 {
        *basezoom = MAX_ZOOM;

        for z in (0..=MAX_ZOOM).rev() {
            if max[z as usize].count < max_features as i64 {
                *basezoom = z;
            }
        }

        let b = *basezoom as usize;
        out.note(format!(
            "Choosing a base zoom of -B{} to keep {} features in tile {}/{}/{}.\n",
            basezoom, max[b].count, basezoom, max[b].x, max[b].y
        ));
    }

    if obasezoom < 0 && *basezoom > maxzoom && o.prevent.get(P_BASEZOOM_ABOVE_MAXZOOM) {
        *basezoom = maxzoom;
    }

    if obasezoom < 0 && *basezoom > maxzoom {
        out.always(
            "Couldn't find a suitable base zoom. Working from the other direction.\n".into(),
        );
        if gamma == 0.0 {
            out.always("You might want to try -g1 to limit near-duplicates.\n".into());
        }

        let mz = maxzoom as usize;
        if *droprate < 0.0 {
            if maxzoom == 0 {
                *droprate = 2.5;
            } else {
                *droprate = round_droprate(
                    ((max[0].count as f64 / max[mz].count as f64).ln() / maxzoom as f64).exp(),
                );
                out.note(format!(
                    "Choosing a drop rate of -r{:.6} to get from {} to {} in {} zooms\n",
                    droprate, max[mz].count, max[0].count, maxzoom
                ));
            }
        }

        *basezoom = 0;
        for z in 0..=maxzoom {
            let zoomdiff =
                (max[z as usize].count as f64 / max_features as f64).ln() / droprate.ln();
            if zoomdiff + z as f64 > *basezoom as f64 {
                *basezoom = (zoomdiff + z as f64).ceil() as i32;
            }
        }

        out.note(format!(
            "Choosing a base zoom of -B{} to keep {:.6} features in tile {}/{}/{}.\n",
            basezoom,
            max[mz].count as f64 * (droprate.ln() * (maxzoom - *basezoom) as f64).exp(),
            maxzoom,
            max[mz].x,
            max[mz].y
        ));
    } else if *droprate < 0.0 {
        *droprate = 1.0;

        for z in (0..*basezoom).rev() {
            let zu = z as usize;
            let mut interval = (droprate.ln() * (*basezoom - z) as f64).exp();

            if max[zu].count as f64 / interval >= max_features as f64 {
                interval = max[zu].count as f64 / max_features as f64;
                *droprate = round_droprate((interval.ln() / (*basezoom - z) as f64).exp());
                interval = (droprate.ln() * (*basezoom - z) as f64).exp();

                out.note(format!(
                    "Choosing a drop rate of -r{:.6} to keep {:.6} features in tile {}/{}/{}.\n",
                    droprate,
                    max[zu].count as f64 / interval,
                    z,
                    max[zu].x,
                    max[zu].y
                ));
            }
        }
    }

    if gamma > 0.0 {
        let mut effective: i32 = 0;

        for z in 0..maxzoom {
            if max[z as usize].count < max[z as usize].fullcount {
                effective = z + 1;
            }
        }

        if effective == 0 {
            out.note("With gamma, effective base zoom is 0, so no effective drop rate\n".into());
        } else {
            let e = effective as usize;
            let interval_0 = (droprate.ln() * *basezoom as f64).exp();
            let mut interval_eff = (droprate.ln() * (*basezoom - effective) as f64).exp();
            if effective > *basezoom {
                interval_eff = 1.0;
            }

            let scaled_0 = max[0].count as f64 / interval_0;
            let scaled_eff = max[e].count as f64 / interval_eff;

            let rate_at_0 = scaled_0 / max[0].fullcount as f64;
            let rate_at_eff = scaled_eff / max[e].fullcount as f64;

            let eff_drop = ((rate_at_eff / rate_at_0).ln() / effective as f64).exp();

            out.note(format!(
                "With gamma, effective base zoom of {}, effective drop rate of {:.6}\n",
                effective, eff_drop
            ));
        }
    }
}

/// Assigns every feature its minimum zoom from the drop rate and base zoom,
/// in sorted order, as tippecanoe does while merging (or in its fix-up pass,
/// with `--drop-denser`).
fn assign_minzooms(
    features: &mut [Feature],
    o: &Options,
    maxzoom: i32,
    basezoom: i32,
    droprate: f64,
) {
    let gamma = o.gamma;
    let mut ds = prep_drop_states(maxzoom, basezoom, droprate);
    let drop_denser = o.drop_denser;

    if drop_denser == 0 {
        for f in features.iter_mut() {
            f.feature_minzoom =
                calc_feature_minzoom(f.ix, f.t as i32, &mut ds, maxzoom, gamma, o) as i8;
        }
        return;
    }

    // The features that --drop-denser does not take keep the minimum zoom
    // that the merge gave them, which is 0 for the types it does not drop.
    for f in features.iter_mut() {
        f.feature_minzoom = 0;
    }

    let mut ddv: Vec<(u64, usize)> = Vec::new();
    let mut previndex: u64 = 0;
    for (ip, f) in features.iter_mut().enumerate() {
        let t = f.t as i32;
        if t == VT_POINT
            || (o.additional.get(A_LINE_DROP) && t == VT_LINE)
            || (o.additional.get(A_POLYGON_DROP) && t == VT_POLYGON)
        {
            let ix = f.ix;
            if ix % 100 < drop_denser as u64 {
                ddv.push((ix.wrapping_sub(previndex), ip));
                previndex = ix;
            } else {
                f.feature_minzoom = calc_feature_minzoom(ix, t, &mut ds, maxzoom, gamma, o) as i8;
            }
        }
    }

    // largest gap sorts first
    ddv.sort_by(|a, b| b.0.cmp(&a.0));

    let mut i: usize = 0;
    for z in 0..=basezoom {
        let keep_fraction = 1.0 / (droprate.ln() * (basezoom - z) as f64).exp();
        let keep_count = (ddv.len() as f64 * keep_fraction) as usize;

        while i < keep_count && i < ddv.len() {
            features[ddv[i].1].feature_minzoom = z as i8;
            i += 1;
        }
    }
    while i < ddv.len() {
        features[ddv[i].1].feature_minzoom = basezoom as i8;
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn zoom0_file_size() {
        // placeholder, three one-byte varints, the features, the end marker
        assert_eq!(super::zoom0_size(100, 0, 0, 0), 8 + 3 + 100 + 1);
        // zoom 1 is zigzag 2, and 200 takes two bytes
        assert_eq!(super::zoom0_size(100, 1, 200, 1), 8 + 4 + 100 + 1);
    }

    use super::*;

    #[test]
    fn drop_states() {
        let ds = prep_drop_states(4, 3, 2.5);
        assert_eq!(ds[3].interval, 0.0);
        assert_eq!(ds[4].interval, 0.0);
        assert_eq!(ds[2].interval, 2.5f64.ln().exp());
        assert_eq!(ds[0].interval, (2.5f64.ln() * 3.0).exp());
        assert_eq!(round_droprate(2.123456789), 2.12346);
    }

    #[test]
    fn minzooms_follow_the_drop_rate() {
        let o = Options::default();
        let mut ds = prep_drop_states(3, 3, 2.0);
        let zooms: Vec<i32> = (0..16)
            .map(|i| calc_feature_minzoom(i, VT_POINT, &mut ds, 3, 0.0, &o))
            .collect();
        // one in 8 from z0, one in 4 from z1, one in 2 from z2, all at z3
        assert_eq!(zooms, vec![0, 2, 3, 1, 3, 2, 3, 0, 3, 2, 3, 1, 3, 2, 3, 0]);
        assert_eq!(calc_feature_minzoom(99, VT_LINE, &mut ds, 3, 0.0, &o), 0);
    }

    #[test]
    fn first_zoom() {
        let mut b = [0x40000000, 0x40000000, 0x40000010, 0x40000010];
        assert_eq!(choose_first_zoom(&mut b, 5, 0), (5, 8, 8));
        let mut b = [-5, 10, 100, 20];
        assert_eq!(choose_first_zoom(&mut b, 3, 5).0, 0);
        assert_eq!(b, [0, 10, (1i64 << 32) - 1, 20]);
    }
}
