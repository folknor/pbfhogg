//! Pass 3: S2 cell assignment + cell-index write.
//!
//! Fine + coarse S2 cell aggregation via a two-stage bucketed pipeline:
//! Stage A parses per-way S2 cells and partitions entries into 256 on-disk
//! buckets by top 8 bits of cell_id. Stage B processes one bucket at a time,
//! grouping by cell_id and writing the merged on-disk records. Also covers
//! admin-cell assignment (`assign_admin_cells`).

use std::io::{BufWriter, Write};
use std::path::Path;

use crate::coord_fmt::e7_to_deg;
use s2::cellid::CellID;
use s2::latlng::LatLng;
use s2::metric::MIN_WIDTHMETRIC;

use crate::geo;
use crate::tunables::{
    ADMIN_CELL_COUNT_BUDGET, GEOCODE_ADDR_CHUNK, GEOCODE_INTERP_CHUNK, GEOCODE_NUM_BUCKETS,
    GEOCODE_STAGE_B_SORT_WINDOW, GEOCODE_STREET_CHUNK, MAX_SEGMENT_SAMPLES, MAX_SEGMENT_SPAN_DEG,
    SEGMENT_STEP_WIDTH_FACTOR,
};

use super::Result;
use super::admin::AssembledPolygon;
use super::pass2::SlimInterpWay;
use crate::error::Error;

use super::super::format::*;

// ---------------------------------------------------------------------------
// Cell entry types
// ---------------------------------------------------------------------------

pub(super) struct AdminCellEntry {
    pub(super) cell_id: u64,
    pub(super) poly_index: u32,
    pub(super) is_interior: bool,
}

// ---------------------------------------------------------------------------
// S2 cell covering for line segments
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Segment sampling budget
//
// `cover_segment` walks a segment in steps of a fixed fraction of the
// globally smallest S2 cell width for the level, so the sample count for one
// segment is `segment_span / step(level)` and `step(level)` halves with every
// level. The count therefore doubles per level, without limit.
//
// The previous implementation capped the walk at 256 steps, which bounded the
// work but silently returned an incomplete cover for anything longer - the bug
// this pass set out to fix. Lifting the cap without bounding the level trades
// silent under-coverage for unbounded work: at level 30 a single
// antimeridian-crossing segment needs on the order of 10^10 samples and the
// same number of hash-set insertions, which is a hang and an OOM, not a build.
//
// Both are wrong, so the work is bounded at the only place where it can be
// bounded without ever discarding coverage: the level. The segment span is not
// data-dependent in the way it first appears - `pack`/`unpack` (see
// `crate::coord_pack`) refuse any coordinate outside +/-90 latitude and +/-180
// longitude, so two legal endpoints are at most `hypot(180, 360)` degrees apart
// in the flat metric this sampler measures with. The worst-case sample count is
// therefore a pure function of the level, computable up front, and
// `MAX_BUILD_S2_LEVEL` is the largest level whose worst case still fits the
// budget. `BuildConfig::validate` rejects anything higher with an explicit
// error before a single blob is read.
//
// This is a bound, not the right long-term answer. Sampling a segment at
// sub-cell resolution is inherently quadratic in the wrong places; the
// structural fix is an exact S2 edge traversal, or an `S2RegionCoverer` over a
// loop region as in `research/traccar-geocoder`. That rewrite remains deferred
// in ADR-0015 (`decisions/0015-defer-s2-admin-region-coverer.md`); this builder
// currently uses bounded segment sampling.
// ---------------------------------------------------------------------------

// The sampling and flood-fill budgets (`MAX_SEGMENT_SAMPLES`,
// `MAX_SEGMENT_SPAN_DEG`, `MAX_BUILD_S2_LEVEL`, `ADMIN_CELL_COUNT_BUDGET`)
// live in `crate::tunables`.

const fn max_admin_s2_level_for_cell_budget() -> u8 {
    let mut level = 0;
    let mut cells = 6u64;
    while level < 30 && cells <= ADMIN_CELL_COUNT_BUDGET / 4 {
        cells *= 4;
        level += 1;
    }
    level
}

/// Highest S2 level that keeps even a whole-sphere admin polygon inside the
/// flood-fill cell budget. Admin indexing visits interior cells, unlike the
/// segment sampler bounded by `MAX_BUILD_S2_LEVEL`.
pub(in crate::geocode_index) const MAX_ADMIN_S2_LEVEL: u8 = max_admin_s2_level_for_cell_budget();

const fn admin_s2_cell_count_upper_bound(level: u8) -> u64 {
    if level > 30 {
        u64::MAX
    } else {
        let mut count = 6u64;
        let mut current_level = 0u8;
        while current_level < level {
            count *= 4;
            current_level += 1;
        }
        count
    }
}

pub(in crate::geocode_index) fn admin_level_exceeds_cell_budget(level: u8) -> bool {
    admin_s2_cell_count_upper_bound(level) > ADMIN_CELL_COUNT_BUDGET
}

/// Sampling step for `level`, in degrees: a little under half the globally
/// smallest S2 cell width at that level, so no cell can be stepped over.
fn segment_step_degrees(level: u8) -> f64 {
    MIN_WIDTHMETRIC.value(level).to_degrees() * SEGMENT_STEP_WIDTH_FACTOR
}

/// Fold a possibly-unwrapped longitude in degrees back into (-180, 180].
fn normalize_longitude_degrees(lon_deg: f64) -> f64 {
    let folded = (lon_deg + 180.0).rem_euclid(360.0) - 180.0;
    // `rem_euclid` yields exactly -180.0 only for an input that is an exact
    // multiple of 360 below it, so this is a representable-value test, not an
    // approximate one.
    #[allow(clippy::float_cmp)]
    let on_west_edge = folded == -180.0;
    if on_west_edge { 180.0 } else { folded }
}

/// Samples the longest legal segment would need at `level`.
fn worst_case_segment_samples(level: u8) -> f64 {
    MAX_SEGMENT_SPAN_DEG / segment_step_degrees(level)
}

/// Whether `level` can make a single segment exceed the sampling budget.
pub(in crate::geocode_index) fn level_exceeds_sampling_budget(level: u8) -> bool {
    worst_case_segment_samples(level) > MAX_SEGMENT_SAMPLES
}

/// Cover a line segment by sampling intermediate points to find all S2 cells
/// the segment passes through at the given level.
///
/// Calls `emit(cell_id)` once for each sampled cell the segment crosses.
///
/// `level` must have passed [`level_exceeds_sampling_budget`] - that is what
/// `BuildConfig::validate` enforces. Given that, the sample count for any
/// legal pair of endpoints is at most `MAX_SEGMENT_SAMPLES`; the clamp below
/// is unreachable defense in depth against the formula and the domain drifting
/// apart, and the debug assertion fires in tests if they ever do.
fn cover_segment(
    lat1_e7: i32,
    lon1_e7: i32,
    lat2_e7: i32,
    lon2_e7: i32,
    level: u8,
    mut emit: impl FnMut(u64),
) {
    let lat1 = e7_to_deg(lat1_e7 as f64);
    let lon1 = e7_to_deg(lon1_e7 as f64);
    let lat2 = e7_to_deg(lat2_e7 as f64);
    // Unwrap the far endpoint around the near one. A segment from 179.9 to
    // -179.9 is 0.2 degrees wide, not 359.8: sampling it in the raw frame
    // walks the long way around the globe and covers half the planet in
    // cells. Sampled longitudes are normalized again below.
    let lon2 = lon1
        + e7_to_deg(
            geo::wrapped_longitude_delta_e7(i64::from(lon2_e7) - i64::from(lon1_e7)) as f64,
        );

    let c1 = CellID::from(LatLng::from_degrees(lat1, lon1)).parent(level as u64);
    let c2 = CellID::from(LatLng::from_degrees(lat2, lon2)).parent(level as u64);

    // Sample at less than half the globally-smallest S2 cell width for this
    // level. Unlike the old hand-written table, this covers every valid level,
    // and unlike the old 256-step cap it never returns a partial cover for a
    // long segment: the work is bounded by rejecting the level, not by
    // truncating the walk. See the budget commentary above.
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let seg_len_deg = (dlat.powi(2) + dlon.powi(2)).sqrt();

    let step_deg = segment_step_degrees(level);

    let exact_steps = (seg_len_deg / step_deg).ceil();
    debug_assert!(
        exact_steps <= MAX_SEGMENT_SAMPLES,
        "cover_segment at level {level} wants {exact_steps} samples, over the \
         {MAX_SEGMENT_SAMPLES} budget: BuildConfig::validate should have \
         rejected this level"
    );
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let steps = (exact_steps.min(MAX_SEGMENT_SAMPLES) as usize).max(1);
    let mut seen = rustc_hash::FxHashSet::default();
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let lat = lat1 + t * (lat2 - lat1);
        let lon = normalize_longitude_degrees(lon1 + t * (lon2 - lon1));
        let c = CellID::from(LatLng::from_degrees(lat, lon))
            .parent(level as u64)
            .0;
        if seen.insert(c) {
            emit(c);
        }
    }
    debug_assert!(seen.contains(&c1.0));
    debug_assert!(seen.contains(&c2.0));
}

/// Cover at the fine level and emit each coarse parent once per segment.
/// Common segments keep their coarse-cell set in a four-entry stack array;
/// a hash set is allocated only when a segment reaches a fifth distinct
/// coarse cell, so long segments remain correct without penalizing the usual
/// short segments with another allocation.
fn cover_segment_fused(
    lat1_e7: i32,
    lon1_e7: i32,
    lat2_e7: i32,
    lon2_e7: i32,
    fine_level: u8,
    coarse_level: u8,
    mut emit: impl FnMut(u64, Option<u64>),
) {
    let mut coarse_seen = [0u64; 4];
    let mut coarse_n = 0usize;
    let mut coarse_overflow: Option<rustc_hash::FxHashSet<u64>> = None;

    cover_segment(lat1_e7, lon1_e7, lat2_e7, lon2_e7, fine_level, |fine_cid| {
        let coarse_cid = CellID(fine_cid).parent(coarse_level as u64).0;
        let coarse_to_emit = if coarse_seen[..coarse_n].contains(&coarse_cid) {
            None
        } else if coarse_n < coarse_seen.len() {
            coarse_seen[coarse_n] = coarse_cid;
            coarse_n += 1;
            Some(coarse_cid)
        } else {
            let seen = coarse_overflow.get_or_insert_with(|| {
                let mut seen = rustc_hash::FxHashSet::default();
                seen.extend(coarse_seen[..coarse_n].iter().copied());
                seen
            });
            if seen.insert(coarse_cid) {
                Some(coarse_cid)
            } else {
                None
            }
        };
        emit(fine_cid, coarse_to_emit);
    });
}

/// Fused cover of every segment of one way, whose nodes are `nodes` (a span
/// of at most `u16::MAX` records, the width of `node_count`). Emits
/// `(fine, coarse, segment_index)` as [`cover_segment_fused`] does.
fn cover_way_segments(
    nodes: Table<'_, NodeCoord>,
    fine_level: u8,
    coarse_level: u8,
    mut emit: impl FnMut(u64, Option<u64>, u16),
) {
    let mut iter = nodes.iter();
    let Some(mut prev) = iter.next() else {
        return;
    };
    for (seg_idx, next) in iter.enumerate() {
        // The span holds at most u16::MAX nodes, so each segment index fits.
        #[allow(clippy::cast_possible_truncation)]
        let segment = seg_idx as u16;
        cover_segment_fused(
            prev.lat_e7,
            prev.lon_e7,
            next.lat_e7,
            next.lon_e7,
            fine_level,
            coarse_level,
            |fine, coarse| emit(fine, coarse, segment),
        );
        prev = next;
    }
}

// ---------------------------------------------------------------------------
// S2 cell assignment
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Bucketed cell assignment
// ---------------------------------------------------------------------------

// Bucket files hold `format::BucketRecord`s.

fn bucket_for_cell(cell_id: u64) -> usize {
    (cell_id >> 56) as usize
}

/// Ensure bucket writer exists, creating the file lazily.
fn ensure_bucket_writer(
    writers: &mut [Option<BufWriter<std::fs::File>>],
    bucket: usize,
    bucket_dir: &Path,
) -> Result<()> {
    if writers[bucket].is_none() {
        writers[bucket] = Some(super::create_data_file(&bucket_path(bucket_dir, bucket))?);
    }
    Ok(())
}

fn bucket_path(bucket_dir: &Path, bucket: usize) -> std::path::PathBuf {
    bucket_dir.join(format!("{bucket:03}"))
}

/// Name the bucket file in a write or flush failure. Called only on the
/// error path, so the per-record success path is unchanged.
#[cold]
fn bucket_write_error(e: std::io::Error, bucket_dir: &Path, bucket: usize) -> crate::error::Error {
    Error::from(e).context(format!(
        "failed to write {}",
        bucket_path(bucket_dir, bucket).display()
    ))
}

/// Append `record` to the bucket file its cell id selects, creating that
/// file on first use.
fn write_bucket_record(
    writers: &mut [Option<BufWriter<std::fs::File>>],
    bucket_dir: &Path,
    record: BucketRecord,
) -> Result<()> {
    let bucket = bucket_for_cell(record.cell_id);
    ensure_bucket_writer(writers, bucket, bucket_dir)?;
    writers[bucket]
        .as_mut()
        .expect("ensured")
        .write_all(&record.to_bytes())
        .map_err(|e| bucket_write_error(e, bucket_dir, bucket))
}

fn parse_bucket_file(data: &[u8]) -> Result<Vec<BucketRecord>> {
    // A partial trailing record means Stage A was interrupted mid-flush
    // (ENOSPC, SIGKILL during write). `Table` would silently skip it and
    // drop real assignments with no diagnostic; error loudly instead.
    if !data.len().is_multiple_of(BucketRecord::SIZE) {
        return Err(Error::internal(format!(
            "geocode bucket file has partial trailing record ({} bytes, \
             not a multiple of {}); Stage A write was \
             likely interrupted (ENOSPC or crash)",
            data.len(),
            BucketRecord::SIZE,
        )));
    }
    Ok(Table::<BucketRecord>::new(data).iter().collect())
}

/// Parameters for the fused fine + coarse bucketed cell assignment
/// (plan item #4). Does one Stage A pass over streets / addrs / interps
/// at the fine level, deriving coarse-level cells via S2 parent on the
/// fly and de-duplicating per-segment. Runs Stage B once per tree to
/// produce the final cell + entry files.
// Named fine/coarse fields keep the producer's role assignments explicit.
// A second bundle of filename constants would not enforce matching levels,
// bucket directories or the reader's mmap roles, so retain these bindings.
pub(super) struct FusedCellAssignmentParams<'a> {
    pub(super) output_dir: &'a Path,
    /// The run's scratch directory, which holds the bucket trees.
    pub(super) scratch: &'a crate::scratch::RunScratch,
    pub(super) street_ways: Table<'a, StreetWay>,
    pub(super) street_nodes: Table<'a, NodeCoord>,
    pub(super) street_way_count: u32,
    pub(super) addr_points: Table<'a, AddrPoint>,
    pub(super) addr_point_count: u32,
    pub(super) interp_ways: &'a [SlimInterpWay],
    pub(super) interp_nodes: Table<'a, NodeCoord>,
    pub(super) fine_level: u8,
    pub(super) coarse_level: u8,
    pub(super) fine_cells_file: &'a str,
    pub(super) fine_street_entries_file: &'a str,
    pub(super) fine_addr_entries_file: &'a str,
    pub(super) fine_interp_entries_file: &'a str,
    pub(super) coarse_cells_file: &'a str,
    pub(super) coarse_street_entries_file: &'a str,
    pub(super) coarse_addr_entries_file: &'a str,
    pub(super) coarse_interp_entries_file: &'a str,
    pub(super) executor: &'a crate::host::Executor,
    /// Checked once per Stage A chunk and per Stage B sort window: the
    /// chunk boundaries are where this pass already returns to one thread.
    pub(super) cancel: &'a crate::host::Cancel,
}

/// Fused fine + coarse bucketed cell assignment. Returns
/// `(fine_cells_written, coarse_cells_written)`.
///
/// Savings come from halving the Stage A `cover_segment` work: each
/// segment is walked once at the fine S2 level, and every emitted fine
/// cell derives its coarse parent via `s2::CellID(cid).parent()`.
/// Multiple fine cells in a segment commonly share a coarse parent, so
/// per-segment dedup uses a 4-entry stack set for common segments and spills
/// to a hash set for longer ones. Addr points (single cell per point) trivially
/// derive coarse from fine without any cover work.
///
/// Stage B runs separately on each of the two bucket trees -
/// parallelised per plan item #3 (256-bucket parallel parse+sort,
/// serial group+write).
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
#[hotpath::measure]
pub(super) fn bucketed_cell_assignment_fused(
    p: &FusedCellAssignmentParams<'_>,
) -> Result<(u32, u32)> {
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering as AtomicOrdering};

    let FusedCellAssignmentParams {
        output_dir,
        scratch,
        street_ways,
        street_nodes,
        street_way_count,
        addr_points,
        addr_point_count,
        interp_ways,
        interp_nodes,
        fine_level,
        coarse_level,
        fine_cells_file,
        fine_street_entries_file,
        fine_addr_entries_file,
        fine_interp_entries_file,
        coarse_cells_file,
        coarse_street_entries_file,
        coarse_addr_entries_file,
        coarse_interp_entries_file,
        executor,
        cancel,
    } = p;
    let (street_ways, street_nodes, addr_points, interp_nodes) =
        (*street_ways, *street_nodes, *addr_points, *interp_nodes);
    let street_way_count = *street_way_count;
    let addr_point_count = *addr_point_count;
    let fine_level = *fine_level;
    let coarse_level = *coarse_level;

    // Create two bucket directories (one per level) in the run's scratch
    // directory, whose guard removes them on every exit path (mid-Stage-A
    // panic, Stage B error, etc.), so ~256 temp files per bucket dir don't
    // leak. They are not part of the index being built.
    //
    // No free-space check precedes them, unlike add-locations-to-ways'
    // sparse index and external join. The trees hold one record per covered
    // cell, and how many cells a street segment or interpolation way covers
    // is known only by covering it, which is Stage A itself. Before Stage A
    // the only figure is a lower bound from the counts pass 2 produced. A check against it would refuse only runs
    // that are certain to fail, and could save at most Stage A's work up to
    // the ENOSPC (nearly all of it, when the bound is dominated by records
    // written late), never passes 1 and 2, which have run by now; it could
    // never say the run fits. That is not worth a second sizing policy that
    // must track the bucket record format, so an ENOSPC surfaces as the
    // write error, failing the build before anything is published.
    let fine_bucket_dir = scratch.create_subdir(&format!("buckets-level{fine_level}"))?;
    let coarse_bucket_dir = scratch.create_subdir(&format!("buckets-level{coarse_level}"))?;

    // Two sets of bucket writers (lazy).
    let mut fine_writers: Vec<Option<BufWriter<std::fs::File>>> =
        (0..GEOCODE_NUM_BUCKETS).map(|_| None).collect();
    let mut coarse_writers: Vec<Option<BufWriter<std::fs::File>>> =
        (0..GEOCODE_NUM_BUCKETS).map(|_| None).collect();

    // Stage A: Chunked parallel compute at the FINE level; every emitted
    // fine cell derives its coarse parent via S2 `parent()` and
    // de-duplicates per-segment so each coarse cell is written at most
    // once per segment. Saves Europe's ~4.9 s coarse-streets Stage A
    // (measured pre-fusion UUID bf8f2038) - the coarse cover_segment
    // pass is entirely eliminated.

    // Lowest index of a way or point whose producer-written data is missing;
    // MAX means none. Only the failure path touches these, so the parallel
    // cover pays nothing for them on a well-formed build.
    let first_missing_street = AtomicU32::new(u32::MAX);
    let first_missing_addr = AtomicUsize::new(usize::MAX);
    let first_missing_interp = AtomicUsize::new(usize::MAX);

    // Streets (fused fine + coarse)
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_STREETS_START");
    let mut chunk_start = 0u32;
    while chunk_start < street_way_count {
        cancel.check()?;
        let chunk_end = chunk_start
            .saturating_add(GEOCODE_STREET_CHUNK)
            .min(street_way_count);
        #[allow(clippy::type_complexity)]
        let entries: Vec<(u64, Option<u64>, u32, u16)> = executor.install(|| {
            (chunk_start..chunk_end)
                .into_par_iter()
                .flat_map_iter(|way_idx| {
                    // Test-only: the fault plan's
                    // `geocode_streets_panic_at_way`. Simulates a
                    // mid-Stage-A crash inside a rayon worker. Rayon
                    // propagates the panic to the `.collect()` call site
                    // on the main thread; the run scratch directory
                    // holding `fine_bucket_dir` / `coarse_bucket_dir`
                    // (ADR-0003) is swept on the unwind.
                    // Release builds compile this out entirely.
                    #[cfg(feature = "test-hooks")]
                    if cancel
                        .faults()
                        .geocode_streets_panic_at_way
                        .load(std::sync::atomic::Ordering::Relaxed)
                        == way_idx
                    {
                        panic!("test-hooks: geocode pass3 stage A streets way {way_idx} panicking");
                    }
                    let rec = street_ways.get(way_idx as usize);
                    let mut out = Vec::new();
                    // Pass 2 wrote these files moments ago, so a record or node
                    // out of bounds is a builder bug, not bad input: skipping it
                    // would publish an index that silently lacks the street.
                    // The failure is recorded here and raised after the chunk's
                    // collect, so the success path stays a plain parallel cover
                    // with no extra validation walk.
                    let Some(rec) = rec else {
                        first_missing_street.fetch_min(way_idx, AtomicOrdering::Relaxed);
                        return out.into_iter();
                    };
                    // A way under two nodes has no segment and is never read.
                    if rec.node_count >= 2 {
                        let Ok(nodes) =
                            street_nodes.span_at(rec.node_offset, u64::from(rec.node_count))
                        else {
                            first_missing_street.fetch_min(way_idx, AtomicOrdering::Relaxed);
                            return out.into_iter();
                        };
                        out.reserve(nodes.len() * 2);
                        cover_way_segments(nodes, fine_level, coarse_level, |fine, coarse, seg| {
                            out.push((fine, coarse, way_idx, seg));
                        });
                    }
                    out.into_iter()
                })
                .collect()
        });
        let missing = first_missing_street.load(AtomicOrdering::Relaxed);
        if missing != u32::MAX {
            return Err(Error::internal(format!(
                "geocode Stage A: street way {missing} has a record or node outside the files Pass 2 wrote"
            )));
        }

        for &(fine_cid, coarse_opt, wi, si) in &entries {
            let record = |cell_id| BucketRecord {
                cell_id,
                entry_type: BUCKET_ENTRY_STREET,
                index: wi,
                segment: si,
            };
            write_bucket_record(&mut fine_writers, &fine_bucket_dir, record(fine_cid))?;
            if let Some(coarse_cid) = coarse_opt {
                write_bucket_record(&mut coarse_writers, &coarse_bucket_dir, record(coarse_cid))?;
            }
        }
        chunk_start = chunk_end;
    }
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_STREETS_END");

    // Addr points: single cell per point, no cover_segment. Derive fine
    // and coarse via CellID.parent() chain from the same LatLng.
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_ADDR_START");
    let addr_count = addr_point_count as usize;
    let mut chunk_start = 0usize;
    while chunk_start < addr_count {
        cancel.check()?;
        let chunk_end = (chunk_start + GEOCODE_ADDR_CHUNK).min(addr_count);
        let addr_entries: Vec<(u64, u64, u32)> = executor.install(|| {
            (chunk_start..chunk_end)
                .into_par_iter()
                .filter_map(|idx| {
                    // Same producer-owned file rule as the streets above.
                    let Some(pt) = addr_points.get(idx) else {
                        first_missing_addr.fetch_min(idx, AtomicOrdering::Relaxed);
                        return None;
                    };
                    let ll = LatLng::from_degrees(
                        e7_to_deg(pt.lat_e7 as f64),
                        e7_to_deg(pt.lon_e7 as f64),
                    );
                    let fine_cid = CellID::from(ll).parent(fine_level as u64).0;
                    let coarse_cid = CellID(fine_cid).parent(coarse_level as u64).0;
                    Some((
                        fine_cid,
                        coarse_cid,
                        u32::try_from(idx).expect("address index bounded by u32 count"),
                    ))
                })
                .collect()
        });
        let missing = first_missing_addr.load(AtomicOrdering::Relaxed);
        if missing != usize::MAX {
            return Err(Error::internal(format!(
                "geocode Stage A: address point {missing} is outside the file Pass 2 wrote"
            )));
        }
        for &(fine_cid, coarse_cid, idx) in &addr_entries {
            let record = |cell_id| BucketRecord {
                cell_id,
                entry_type: BUCKET_ENTRY_ADDR,
                index: idx,
                segment: 0,
            };
            write_bucket_record(&mut fine_writers, &fine_bucket_dir, record(fine_cid))?;
            write_bucket_record(&mut coarse_writers, &coarse_bucket_dir, record(coarse_cid))?;
        }
        chunk_start = chunk_end;
    }
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_ADDR_END");

    // Interpolation (fused fine + coarse, same shape as streets)
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_INTERP_START");
    let mut interp_chunk_start = 0usize;
    while interp_chunk_start < interp_ways.len() {
        cancel.check()?;
        let interp_chunk_end = (interp_chunk_start + GEOCODE_INTERP_CHUNK).min(interp_ways.len());
        #[allow(clippy::type_complexity)]
        let interp_entries: Vec<(u64, Option<u64>, u32, u16)> = executor.install(|| {
            (interp_chunk_start..interp_chunk_end)
                .into_par_iter()
                .flat_map_iter(|way_idx| {
                    let iw = &interp_ways[way_idx];
                    let mut out = Vec::new();
                    if iw.node_count >= 2 {
                        // Same producer-owned file rule as the streets above.
                        let Ok(nodes) =
                            interp_nodes.span_at(iw.node_file_offset, u64::from(iw.node_count))
                        else {
                            first_missing_interp.fetch_min(way_idx, AtomicOrdering::Relaxed);
                            return out.into_iter();
                        };
                        let way_index = u32::try_from(way_idx)
                            .expect("interpolation count checked before cell assignment");
                        out.reserve(nodes.len() * 2);
                        cover_way_segments(nodes, fine_level, coarse_level, |fine, coarse, seg| {
                            out.push((fine, coarse, way_index, seg));
                        });
                    }
                    out.into_iter()
                })
                .collect()
        });
        let missing = first_missing_interp.load(AtomicOrdering::Relaxed);
        if missing != usize::MAX {
            return Err(Error::internal(format!(
                "geocode Stage A: interpolation way {missing} has a node outside the file Pass 2 wrote"
            )));
        }
        for &(fine_cid, coarse_opt, wi, si) in &interp_entries {
            let record = |cell_id| BucketRecord {
                cell_id,
                entry_type: BUCKET_ENTRY_INTERP,
                index: wi,
                segment: si,
            };
            write_bucket_record(&mut fine_writers, &fine_bucket_dir, record(fine_cid))?;
            if let Some(coarse_cid) = coarse_opt {
                write_bucket_record(&mut coarse_writers, &coarse_bucket_dir, record(coarse_cid))?;
            }
        }
        interp_chunk_start = interp_chunk_end;
    }
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_INTERP_END");

    // Flush and drop both sets of bucket writers
    for (b, writer) in fine_writers.iter_mut().enumerate() {
        if let Some(writer) = writer {
            writer
                .flush()
                .map_err(|e| bucket_write_error(e, &fine_bucket_dir, b))?;
        }
    }
    for (b, writer) in coarse_writers.iter_mut().enumerate() {
        if let Some(writer) = writer {
            writer
                .flush()
                .map_err(|e| bucket_write_error(e, &coarse_bucket_dir, b))?;
        }
    }
    drop(fine_writers);
    drop(coarse_writers);

    // Stage B, twice - once per bucket tree. Drop the per-tree guard
    // as soon as the tree is consumed so the bucket files are freed
    // before the second tree's Stage B runs (disk pressure at planet
    // scale).
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEB_FINE_START");
    let fine_count = run_stage_b(
        output_dir,
        &fine_bucket_dir,
        fine_cells_file,
        fine_street_entries_file,
        fine_addr_entries_file,
        fine_interp_entries_file,
        executor,
        cancel,
    )?;
    scratch.remove_tree(&fine_bucket_dir);
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEB_FINE_END");

    crate::debug::emit_marker("GEOCODE_PASS3_STAGEB_COARSE_START");
    let coarse_count = run_stage_b(
        output_dir,
        &coarse_bucket_dir,
        coarse_cells_file,
        coarse_street_entries_file,
        coarse_addr_entries_file,
        coarse_interp_entries_file,
        executor,
        cancel,
    )?;
    scratch.remove_tree(&coarse_bucket_dir);
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEB_COARSE_END");

    Ok((fine_count, coarse_count))
}

/// Stage B helper (plan item #3): parallel per-bucket read+parse+sort,
/// serial group+write. Extracted so the fused fine+coarse driver
/// (`bucketed_cell_assignment_fused`) can run it once per tree.
///
/// Runs byte-identically to the pre-parallel sequential Stage B because
/// the serial group+write phase walks the bucket-ordered
/// `sorted_buckets` Vec, preserving the debug_assert on cross-bucket
/// cell_id monotonicity.
#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::too_many_arguments
)]
// Filenames are output roles; the fine/coarse caller also owns the matching
// bucket directory. Keep those assignments together at each call site.
fn run_stage_b(
    output_dir: &Path,
    bucket_dir: &Path,
    cells_file: &str,
    street_entries_file: &str,
    addr_entries_file: &str,
    interp_entries_file: &str,
    executor: &crate::host::Executor,
    cancel: &crate::host::Cancel,
) -> Result<u32> {
    use rayon::prelude::*;

    let mut cells_out = super::create_data_file(&output_dir.join(cells_file))?;
    let mut street_out = super::create_data_file(&output_dir.join(street_entries_file))?;
    let mut addr_out = super::create_data_file(&output_dir.join(addr_entries_file))?;
    let mut interp_out = super::create_data_file(&output_dir.join(interp_entries_file))?;

    let mut street_byte_offset: u64 = 0;
    let mut addr_byte_offset: u64 = 0;
    let mut interp_byte_offset: u64 = 0;
    let mut total_cells: u32 = 0;
    let mut prev_cell_id: u64 = 0;

    // Bound the parse/sort lookahead. Keeping every sorted bucket alive here
    // defeats the external bucketing and costs more RAM than the 15-byte
    // scratch records because a decoded BucketRecord is 16 bytes.
    for window_start in (0..GEOCODE_NUM_BUCKETS).step_by(GEOCODE_STAGE_B_SORT_WINDOW) {
        cancel.check()?;
        let window_end = (window_start + GEOCODE_STAGE_B_SORT_WINDOW).min(GEOCODE_NUM_BUCKETS);
        let sorted_buckets: Vec<Vec<BucketRecord>> = executor.install(|| {
            (window_start..window_end)
                .into_par_iter()
                .map(|bucket_idx| -> Result<Vec<BucketRecord>> {
                    let bucket_path = bucket_path(bucket_dir, bucket_idx);
                    if !bucket_path.exists() {
                        return Ok(Vec::new());
                    }
                    let data = crate::read::input::read(&bucket_path)?;
                    // A file in a run-scratch bucket tree, freed as soon as it
                    // is read to give the space back; `RunScratch` removes
                    // the tree afterwards and names only its direct children.
                    #[allow(clippy::disallowed_methods)]
                    let removed = std::fs::remove_file(&bucket_path);
                    removed.map_err(|e| {
                        Error::from(e).context(format!(
                            "failed to remove scratch bucket {}",
                            bucket_path.display()
                        ))
                    })?;
                    if data.is_empty() {
                        return Ok(Vec::new());
                    }
                    let mut entries = parse_bucket_file(&data)?;
                    drop(data);
                    entries.sort_unstable_by_key(|e| e.cell_id);
                    Ok(entries)
                })
                .collect::<Result<_>>()
        })?;

        for entries in &sorted_buckets {
            if entries.is_empty() {
                continue;
            }

            let mut i = 0;
            let mut streets: Vec<&BucketRecord> = Vec::new();
            let mut addrs: Vec<&BucketRecord> = Vec::new();
            let mut interps: Vec<&BucketRecord> = Vec::new();
            while i < entries.len() {
                let cell_id = entries[i].cell_id;
                let group_start = i;
                while i < entries.len() && entries[i].cell_id == cell_id {
                    i += 1;
                }
                let group = &entries[group_start..i];
                let cell_token = CellID(cell_id);

                streets.clear();
                addrs.clear();
                interps.clear();
                for e in group {
                    match e.entry_type {
                        BUCKET_ENTRY_STREET => streets.push(e),
                        BUCKET_ENTRY_ADDR => addrs.push(e),
                        BUCKET_ENTRY_INTERP => interps.push(e),
                        other => {
                            return Err(Error::internal(format!(
                                "geocode Stage B: unknown bucket entry type {other} in cell {cell_id}"
                            )));
                        }
                    }
                }

                // Write street entries.
                // INVARIANT: the on-disk list count is `ListCount` (u16), the
                // framing `format::Table::list` reads. Hard-error if any cell's
                // per-type entry count exceeds `u16::MAX`, rather than silently truncating.
                // If this ever fires in practice, bump the on-disk count to u32 and
                // increment `FORMAT_VERSION`. Do NOT restore a `.min(u16::MAX)` here -
                // silent truncation made this a latent bug for a long time.
                let street_count = if streets.is_empty() {
                    None
                } else {
                    let count = ListCount::try_from(streets.len()).map_err(|_| {
                    Error::invalid_data(format!(
                        "geocode Stage B: cell {cell_token} ({cell_id}) has {} street entries, exceeds u16::MAX. \
                     Bump on-disk count to u32 and increment FORMAT_VERSION.",
                        streets.len()
                    ))
                })?;
                    street_out.write_all(&count.to_le_bytes())?;
                    for e in &streets {
                        street_out.write_all(
                            &SegmentRef {
                                way_index: e.index,
                                segment_index: e.segment,
                            }
                            .to_bytes(),
                        )?;
                    }
                    Some(count)
                };

                let addr_count = if addrs.is_empty() {
                    None
                } else {
                    let count = ListCount::try_from(addrs.len()).map_err(|_| {
                    Error::invalid_data(format!(
                        "geocode Stage B: cell {cell_token} ({cell_id}) has {} addr entries, exceeds u16::MAX. \
                     Bump on-disk count to u32 and increment FORMAT_VERSION.",
                        addrs.len()
                    ))
                })?;
                    addr_out.write_all(&count.to_le_bytes())?;
                    for e in &addrs {
                        addr_out.write_all(
                            &AddrEntry {
                                addr_index: e.index,
                            }
                            .to_bytes(),
                        )?;
                    }
                    Some(count)
                };

                let interp_count = if interps.is_empty() {
                    None
                } else {
                    let count = ListCount::try_from(interps.len()).map_err(|_| {
                    Error::invalid_data(format!(
                        "geocode Stage B: cell {cell_token} ({cell_id}) has {} interp entries, exceeds u16::MAX. \
                     Bump on-disk count to u32 and increment FORMAT_VERSION.",
                        interps.len()
                    ))
                })?;
                    interp_out.write_all(&count.to_le_bytes())?;
                    for e in &interps {
                        interp_out.write_all(
                            &SegmentRef {
                                way_index: e.index,
                                segment_index: e.segment,
                            }
                            .to_bytes(),
                        )?;
                    }
                    Some(count)
                };

                let gc = GeoCell {
                    cell_id,
                    street_offset: street_count.map_or(NO_DATA_U64, |_| street_byte_offset),
                    addr_offset: addr_count.map_or(NO_DATA_U64, |_| addr_byte_offset),
                    interp_offset: interp_count.map_or(NO_DATA_U64, |_| interp_byte_offset),
                };
                cells_out.write_all(&gc.to_bytes())?;
                debug_assert!(
                    cell_id > prev_cell_id || total_cells == 0,
                    "bucket ordering violated: cell {} ({cell_id}) <= prev {} ({prev_cell_id})",
                    CellID(cell_id),
                    CellID(prev_cell_id),
                );
                prev_cell_id = cell_id;
                total_cells = total_cells.checked_add(1).ok_or_else(|| {
                    Error::invalid_data("geocode cell count exceeds u32::MAX; bump FORMAT_VERSION")
                })?;

                if let Some(count) = street_count {
                    street_byte_offset += list_byte_len::<SegmentRef>(count);
                }
                if let Some(count) = addr_count {
                    addr_byte_offset += list_byte_len::<AddrEntry>(count);
                }
                if let Some(count) = interp_count {
                    interp_byte_offset += list_byte_len::<SegmentRef>(count);
                }
            }
        }
        // `sorted_buckets` drops here before the next lookahead window.
    }

    cells_out.flush()?;
    street_out.flush()?;
    addr_out.flush()?;
    interp_out.flush()?;

    // Bucket-dir cleanup is handled by the run scratch directory: on
    // success the caller removes the tree right after this function
    // returns; on error the run directory's guard sweeps it.

    Ok(total_cells)
}

#[hotpath::measure]
pub(super) fn assign_admin_cells(
    polygons: &[AssembledPolygon],
    admin_level: u8,
    executor: &crate::host::Executor,
) -> Vec<AdminCellEntry> {
    use rayon::prelude::*;

    // Per-polygon work is independent: edge-cell cover + centroid flood-fill
    // read the polygon vertices in isolation and produce a disjoint set of
    // `AdminCellEntry`s. `par_iter().flat_map_iter().collect()` preserves
    // input ordering so downstream `entries.sort_unstable_by_key(cell_id)`
    // in `write_admin_index` gives a byte-identical result to the sequential
    // path. Plan item #6 - Europe admin flood-fill was 10.7 s sequential
    // (UUID bf8f2038); expected ~5× on 12 cores.
    executor.install(|| {
        polygons
            .par_iter()
            .enumerate()
            .flat_map_iter(|(poly_idx, poly)| {
                admin_cells_for_polygon(poly_idx, poly, admin_level).into_iter()
            })
            .collect()
    })
}

fn admin_cells_for_polygon(
    poly_idx: usize,
    poly: &AssembledPolygon,
    admin_level: u8,
) -> Vec<AdminCellEntry> {
    // Parse vertices into rings (exterior + holes) separated by RING_SENTINEL
    let (ext_f64, hole_rings) = parse_polygon_rings(poly.vertices.iter().copied());
    if ext_f64.len() < 3 {
        return Vec::new();
    }

    let hole_slices: Vec<&[(f64, f64)]> = hole_rings.iter().map(Vec::as_slice).collect();

    // Producer invariant: assemble_rings emits only explicitly closed rings;
    // simplify_ring retains both endpoints, including the repeated first point.
    // Thus windows(2) includes the closing edge for exterior and hole rings.
    // Adding another last-to-first segment would cover a zero-length edge on
    // every produced ring. Generic parse_polygon_rings also accepts open rings,
    // but this function consumes AssembledPolygon from the builder.
    // Edge cells: cover each ring segment using cover_segment
    let mut edge_cells = rustc_hash::FxHashSet::default();
    for v in poly.vertices.windows(2) {
        if v[0] == RING_SENTINEL || v[1] == RING_SENTINEL {
            continue;
        }
        cover_segment(
            v[0].lat_e7,
            v[0].lon_e7,
            v[1].lat_e7,
            v[1].lon_e7,
            admin_level,
            |cid| {
                edge_cells.insert(cid);
            },
        );
    }

    let mut entries: Vec<AdminCellEntry> = edge_cells
        .iter()
        .map(|&cid| AdminCellEntry {
            cell_id: cid,
            poly_index: u32::try_from(poly_idx)
                .expect("admin count checked before cell assignment"),
            is_interior: false,
        })
        .collect();

    // Seed from every inside neighbor of every edge cell. This produces real
    // interior points for concave polygons and reaches disconnected pockets
    // that a single representative point cannot reach.
    let mut visited = rustc_hash::FxHashSet::default();
    let mut queue = std::collections::VecDeque::new();
    for &edge_id in &edge_cells {
        let edge = CellID(edge_id);
        for seed in edge.all_neighbors(admin_level as u64) {
            if edge_cells.contains(&seed.0) || !visited.insert(seed.0) {
                continue;
            }
            let center = LatLng::from(seed);
            if crate::geo::point_in_polygon(
                center.lng.deg(),
                center.lat.deg(),
                &ext_f64,
                &hole_slices,
            ) {
                queue.push_back(seed);
            }
        }
    }

    while let Some(cell) = queue.pop_front() {
        if edge_cells.contains(&cell.0) {
            continue;
        }

        let center_ll = s2::latlng::LatLng::from(cell);
        // Test with holes - cells inside enclaves are NOT interior
        if crate::geo::point_in_polygon(
            center_ll.lng.deg(),
            center_ll.lat.deg(),
            &ext_f64,
            &hole_slices,
        ) {
            entries.push(AdminCellEntry {
                cell_id: cell.0,
                poly_index: u32::try_from(poly_idx)
                    .expect("admin count checked before cell assignment"),
                is_interior: true,
            });
            for n in cell.all_neighbors(admin_level as u64) {
                if !visited.contains(&n.0) {
                    visited.insert(n.0);
                    queue.push_back(n);
                }
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunables::MAX_BUILD_S2_LEVEL;

    #[test]
    fn stage_b_retains_at_most_eight_sorted_buckets() {
        let window = std::hint::black_box(GEOCODE_STAGE_B_SORT_WINDOW);
        assert!(window > 0);
        assert!(window <= 8);
        assert!(window < GEOCODE_NUM_BUCKETS);
    }

    #[test]
    fn bucket_file_parses_whole_records_and_refuses_a_partial_tail() {
        let records = [
            BucketRecord {
                cell_id: 7,
                entry_type: BUCKET_ENTRY_STREET,
                index: 1,
                segment: 2,
            },
            BucketRecord {
                cell_id: 3,
                entry_type: BUCKET_ENTRY_ADDR,
                index: 4,
                segment: 0,
            },
        ];
        let mut data: Vec<u8> = records
            .into_iter()
            .flat_map(BucketRecord::to_bytes)
            .collect();
        assert_eq!(parse_bucket_file(&data).expect("whole records"), records);
        data.pop();
        let err = parse_bucket_file(&data).expect_err("a partial tail must not be skipped");
        assert!(err.to_string().contains("partial trailing record"), "{err}");
    }

    #[test]
    fn cover_way_segments_numbers_each_segment() {
        let nodes: Vec<u8> = [(0, 0), (1_000, 0), (2_000, 0)]
            .into_iter()
            .flat_map(|(lat_e7, lon_e7)| NodeCoord { lat_e7, lon_e7 }.to_bytes())
            .collect();
        let mut segments = Vec::new();
        cover_way_segments(Table::new(&nodes), 17, 14, |_, _, seg| segments.push(seg));
        segments.dedup();
        assert_eq!(segments, vec![0, 1]);
        let mut none = 0;
        cover_way_segments(Table::new(&nodes[..NodeCoord::SIZE]), 17, 14, |_, _, _| {
            none += 1;
        });
        assert_eq!(none, 0, "a single node has no segment");
    }

    #[test]
    fn interpolation_stage_a_has_a_finite_chunk_bound() {
        let chunk = std::hint::black_box(GEOCODE_INTERP_CHUNK);
        assert!(chunk > 0);
        assert!(chunk <= GEOCODE_ADDR_CHUNK);
    }

    #[test]
    fn cover_segment_same_cell() {
        // Two very close points in Copenhagen - should be in the same S2 cell at level 17.
        let lat1 = 556_762_000; // 55.6762
        let lon1 = 125_683_000; // 12.5683
        let lat2 = 556_762_100; // 55.67621 - ~0.1m away
        let lon2 = 125_683_100;

        let mut cells = Vec::new();
        cover_segment(lat1, lon1, lat2, lon2, 17, |c| cells.push(c));

        assert_eq!(
            cells.len(),
            1,
            "very close points should produce exactly 1 cell"
        );
    }

    #[test]
    fn cover_segment_cross_cell() {
        // Two points about 1km apart - should cross multiple S2 cells at level 17.
        // Level 17 cells are roughly 150m on a side.
        let lat1 = 556_700_000; // 55.6700
        let lon1 = 125_600_000; // 12.5600
        let lat2 = 556_800_000; // 55.6800 - ~1.1km north
        let lon2 = 125_700_000; // 12.5700

        let mut cells = Vec::new();
        cover_segment(lat1, lon1, lat2, lon2, 17, |c| cells.push(c));

        assert!(
            cells.len() >= 2,
            "points ~1km apart should cross at least 2 level-17 cells, got {}",
            cells.len()
        );
    }

    #[test]
    fn cover_segment_endpoints_included() {
        // Verify both endpoint cells are always in the output.
        let lat1 = 556_700_000;
        let lon1 = 125_600_000;
        let lat2 = 556_800_000;
        let lon2 = 125_700_000;

        let c1 = CellID::from(LatLng::from_degrees(55.6700, 12.5600))
            .parent(17)
            .0;
        let c2 = CellID::from(LatLng::from_degrees(55.6800, 12.5700))
            .parent(17)
            .0;

        let mut cells = Vec::new();
        cover_segment(lat1, lon1, lat2, lon2, 17, |c| cells.push(c));

        assert!(cells.contains(&c1), "first endpoint cell must be included");
        assert!(cells.contains(&c2), "second endpoint cell must be included");
    }

    /// The old implementation capped the walk at 256 steps, so it could emit
    /// at most 257 cells for a segment of any length. The lower bound here is
    /// what makes that a failure rather than a pass.
    #[test]
    fn cover_segment_long_segment_is_complete_and_unique() {
        let mut cells = Vec::new();
        cover_segment(
            550_000_000,
            120_000_000,
            550_000_000,
            130_000_000,
            MAX_BUILD_S2_LEVEL,
            |c| cells.push(c),
        );
        let unique: rustc_hash::FxHashSet<_> = cells.iter().copied().collect();
        assert_eq!(
            cells.len(),
            unique.len(),
            "each crossed cell must be emitted once"
        );
        assert!(
            cells.len() > 1_000,
            "the old 256-step cap must not survive: got {} cells",
            cells.len()
        );
    }

    #[test]
    fn fused_segment_deduplicates_after_four_coarse_cells() {
        let mut entries = Vec::new();
        cover_segment_fused(
            556_700_000,
            125_600_000,
            556_700_000,
            135_000_000,
            17,
            14,
            |fine, coarse| entries.push((fine, coarse)),
        );

        let mut fine_seen = rustc_hash::FxHashSet::default();
        let mut coarse_seen = rustc_hash::FxHashSet::default();
        for &(fine, coarse) in &entries {
            assert!(fine_seen.insert(fine), "fine cells must be unique");
            if let Some(coarse) = coarse {
                assert_eq!(
                    CellID(fine).parent(14).0,
                    coarse,
                    "coarse entry must be the fine cell's parent"
                );
                assert!(
                    coarse_seen.insert(coarse),
                    "a coarse cell must be emitted once even after the four-cell stack set fills"
                );
            }
        }
        assert!(
            coarse_seen.len() > 4,
            "fixture must cross more than four coarse cells, got {}",
            coarse_seen.len()
        );
    }

    /// `MAX_BUILD_S2_LEVEL` is not a taste judgement, it is exactly the
    /// boundary of `MAX_SEGMENT_SAMPLES`. Walking every level and asserting
    /// the boundary lands where the constant says it does means the constant
    /// cannot be raised without raising the budget it is derived from - which
    /// is the whole enforcement mechanism for the unbounded-work hazard.
    #[test]
    fn max_build_level_is_exactly_the_sampling_budget_boundary() {
        for level in 0..=MAX_BUILD_S2_LEVEL {
            assert!(
                !level_exceeds_sampling_budget(level),
                "level {level} is accepted but its worst case is {} samples, over the \
                 {MAX_SEGMENT_SAMPLES} budget",
                worst_case_segment_samples(level)
            );
        }
        assert!(
            level_exceeds_sampling_budget(MAX_BUILD_S2_LEVEL + 1),
            "level {} must be over budget, otherwise the cap is lower than it needs to be \
             and rejects levels that build fine",
            MAX_BUILD_S2_LEVEL + 1
        );
        // The budget must actually bind somewhere below the S2 format limit,
        // or the whole bound is vacuous.
        assert!(
            level_exceeds_sampling_budget(30),
            "level 30 must be over budget: it is the level that motivated the bound"
        );
    }

    #[test]
    fn max_admin_level_is_exactly_the_flood_fill_cell_budget_boundary() {
        for level in 0..=MAX_ADMIN_S2_LEVEL {
            assert!(
                !admin_level_exceeds_cell_budget(level),
                "level {level} must fit the admin flood-fill cell budget"
            );
        }
        assert_eq!(
            admin_s2_cell_count_upper_bound(MAX_ADMIN_S2_LEVEL),
            ADMIN_CELL_COUNT_BUDGET,
            "the accepted ceiling must use the full configured cell budget"
        );
        assert!(
            admin_level_exceeds_cell_budget(MAX_ADMIN_S2_LEVEL + 1),
            "level {} must exceed the admin flood-fill cell budget",
            MAX_ADMIN_S2_LEVEL + 1
        );
    }

    /// The analytic worst case is only a bound if the implementation really
    /// respects it. A full-longitude segment is the widest thing a legal pair
    /// of coordinates can produce, so its actual emission count must fall
    /// under `worst_case_segment_samples` for the level.
    #[test]
    fn widest_legal_segment_stays_under_its_analytic_worst_case() {
        const LEVEL: u8 = 12;
        let mut emitted = 0usize;
        cover_segment(
            -900_000_000,
            -1_800_000_000,
            900_000_000,
            1_800_000_000,
            LEVEL,
            |_| emitted += 1,
        );
        let bound = worst_case_segment_samples(LEVEL);
        assert!(
            (emitted as f64) <= bound,
            "widest legal segment emitted {emitted} cells at level {LEVEL}, over the \
             analytic bound of {bound}"
        );
        assert!(
            emitted > 1_000,
            "the widest legal segment must produce a real cover, got {emitted} cells"
        );
    }

    #[test]
    fn concave_polygon_gets_interior_cells_when_vertex_mean_is_outside() {
        // A U shape whose vertex average lies in the open notch.
        let vertices = vec![
            (0, 0),
            (0, 60_000_000),
            (60_000_000, 60_000_000),
            (60_000_000, 40_000_000),
            (20_000_000, 40_000_000),
            (20_000_000, 20_000_000),
            (60_000_000, 20_000_000),
            (60_000_000, 0),
            (0, 0),
        ]
        .into_iter()
        .map(|(lat_e7, lon_e7)| NodeCoord { lat_e7, lon_e7 })
        .collect();
        let poly = AssembledPolygon {
            admin_level: 8,
            name_offset: 0,
            country_code_offset: 0,
            area: 1.0,
            vertices,
        };
        // The fixture is only meaningful if its vertex average really does
        // land in the open notch: that average is what the old centroid seed
        // used, and a fixture whose average happened to be inside would pass
        // against the bug it was written to catch.
        let (sum_lat, sum_lon, n) = poly
            .vertices
            .iter()
            .fold((0i64, 0i64, 0i64), |(sl, sn, c), v| {
                (sl + i64::from(v.lat_e7), sn + i64::from(v.lon_e7), c + 1)
            });
        let (mean_lat, mean_lon) = (
            e7_to_deg(sum_lat as f64 / n as f64),
            e7_to_deg(sum_lon as f64 / n as f64),
        );
        let (ext, holes) = parse_polygon_rings(poly.vertices.iter().copied());
        let hole_slices: Vec<&[(f64, f64)]> = holes.iter().map(Vec::as_slice).collect();
        assert!(
            !crate::geo::point_in_polygon(mean_lon, mean_lat, &ext, &hole_slices),
            "fixture is not concave enough: the vertex mean ({mean_lat}, {mean_lon}) is \
             inside the polygon, so the old centroid seed would have worked"
        );

        let entries = admin_cells_for_polygon(0, &poly, 8);
        assert!(
            entries.iter().any(|entry| entry.is_interior),
            "concave polygon must retain interior coverage"
        );
    }

    /// A U shape and a shape with two separate lobes are different tests. The
    /// U only proves the seed can find *an* interior point; this proves the
    /// seeding reaches interior regions that are disconnected from each other
    /// in the cell grid.
    ///
    /// Two 3-degree square lobes joined by a corridor 0.01 degrees tall. A
    /// level-8 cell is roughly 1.4 degrees across, so every corridor cell is
    /// an edge cell and the two lobes' interior cells cannot reach each other
    /// by flood fill. Any seeding strategy that starts from one point reaches
    /// at most one lobe.
    #[test]
    fn two_disconnected_interior_lobes_are_both_covered() {
        let vertices = vec![
            (0, 0),
            (30_000_000, 0),
            (30_000_000, 30_000_000),
            (15_100_000, 30_000_000),
            (15_100_000, 100_000_000),
            (30_000_000, 100_000_000),
            (30_000_000, 130_000_000),
            (0, 130_000_000),
            (0, 100_000_000),
            (15_000_000, 100_000_000),
            (15_000_000, 30_000_000),
            (0, 30_000_000),
            (0, 0),
        ]
        .into_iter()
        .map(|(lat_e7, lon_e7)| NodeCoord { lat_e7, lon_e7 })
        .collect();
        let poly = AssembledPolygon {
            admin_level: 8,
            name_offset: 0,
            country_code_offset: 0,
            area: 1.0,
            vertices,
        };

        let entries = admin_cells_for_polygon(0, &poly, 8);
        let mut west = 0usize;
        let mut east = 0usize;
        for entry in entries.iter().filter(|e| e.is_interior) {
            let lon = LatLng::from(CellID(entry.cell_id)).lng.deg();
            if lon < 3.0 {
                west += 1;
            } else if lon > 10.0 {
                east += 1;
            }
        }
        assert!(
            west > 0,
            "the west lobe must have interior cells (west {west}, east {east})"
        );
        assert!(
            east > 0,
            "the east lobe must have interior cells - a single seed reaches only one \
             lobe (west {west}, east {east})"
        );
    }
}
