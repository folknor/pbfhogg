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

use crate::tunables::{
    ADMIN_CELL_COUNT_BUDGET, GEOCODE_ADDR_CHUNK, GEOCODE_INTERP_CHUNK, GEOCODE_NUM_BUCKETS,
    GEOCODE_STAGE_B_SORT_WINDOW, GEOCODE_STREET_CHUNK,
};

use super::Result;
use super::admin::AssembledPolygon;
use super::cover::{Vertex, cover_path_raw, cover_segment};
use super::pass2::SlimInterpWay;
use crate::error::Error;
use crate::scratch::{ScratchFile, ScratchPath, ScratchSubdir};

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

// The cover itself is `super::cover` (a conservative cover of the path linear
// in lat/lon, the reader's geometry). Its work grows with the level: a
// segment of a given length crosses twice as many cells per level. The work
// is bounded at the only place it can be bounded without discarding coverage,
// the level: `pack`/`unpack` (see `crate::coord_pack`) refuse any coordinate
// outside +/-90 latitude and +/-180 longitude, and the far endpoint is
// unwrapped around the near one, so the widest legal segment is fixed, its
// cell bound (`cover::worst_case_segment_cells`) a pure function of the level, and
// `MAX_BUILD_S2_LEVEL` is the largest level whose bound fits
// `MAX_SEGMENT_CELLS`. `BuildConfig::validate` rejects anything higher with
// an explicit error before a single blob is read.
//
// The budgets (`MAX_SEGMENT_CELLS`, `MAX_BUILD_S2_LEVEL`,
// `ADMIN_CELL_COUNT_BUDGET`) live in `crate::tunables`.

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
/// flood-fill cell budget. Admin indexing visits interior cells, and its edge
/// cells go into one set per polygon, so this one budget bounds its memory;
/// its edge-cover work is linear in the polygon's stored edges.
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

/// Cover at the fine level and emit each coarse parent once per segment.
/// The coarse cover is the set of parents of the fine one, which is
/// conservative because the fine one is: a coarse cell the path meets holds
/// a fine cell the path meets.
/// Common segments keep their coarse-cell set in a four-entry stack array;
/// a hash set is allocated only when a segment reaches a fifth distinct
/// coarse cell, so long segments remain correct without penalizing the usual
/// short segments with another allocation.
fn cover_segment_fused(
    a: &Vertex,
    b: &Vertex,
    fine_level: u8,
    coarse_level: u8,
    mut emit: impl FnMut(u64, Option<u64>),
) {
    let mut coarse_seen = [0u64; 4];
    let mut coarse_n = 0usize;
    let mut coarse_overflow: Option<rustc_hash::FxHashSet<u64>> = None;

    cover_segment(a, b, fine_level, |fine_cid| {
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
    // Each node is placed on the sphere once and shared by the two segments
    // it ends.
    let mut iter = nodes.iter().map(|n| Vertex::new(n.lat_e7, n.lon_e7));
    let Some(mut prev) = iter.next() else {
        return;
    };
    for (seg_idx, next) in iter.enumerate() {
        // The span holds at most u16::MAX nodes, so each segment index fits.
        #[allow(clippy::cast_possible_truncation)]
        let segment = seg_idx as u16;
        cover_segment_fused(&prev, &next, fine_level, coarse_level, |fine, coarse| {
            emit(fine, coarse, segment);
        });
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

/// The fine and coarse cells an address point is filed under: its own cell
/// at each level, the coarse one taken as the fine one's parent. The
/// builder's post-write membership check derives its keys from this same
/// function, so the two cannot drift apart.
pub(super) fn addr_point_cells(
    lat_e7: i32,
    lon_e7: i32,
    fine_level: u8,
    coarse_level: u8,
) -> (u64, u64) {
    let ll = LatLng::from_degrees(e7_to_deg(f64::from(lat_e7)), e7_to_deg(f64::from(lon_e7)));
    let fine_cid = CellID::from(ll).parent(u64::from(fine_level)).0;
    let coarse_cid = CellID(fine_cid).parent(u64::from(coarse_level)).0;
    (fine_cid, coarse_cid)
}

/// Test-only: the fault plan's `geocode_misfile_address`, which files one
/// address point at one level under its antipode's cell instead of its own,
/// leaving every other entry (streets included) where it belongs.
#[cfg(pbfhogg_test_hooks)]
fn misfile_address(
    faults: &crate::test_hooks::FaultPlan,
    idx: usize,
    pt: AddrPoint,
    fine_cid: u64,
    coarse_cid: u64,
    fine_level: u8,
    coarse_level: u8,
) -> (u64, u64) {
    let Some((target, level)) = faults.geocode_misfile_address() else {
        return (fine_cid, coarse_cid);
    };
    if target != idx {
        return (fine_cid, coarse_cid);
    }
    let antipode_lon = if pt.lon_e7 > 0 {
        pt.lon_e7 - 1_800_000_000
    } else {
        pt.lon_e7 + 1_800_000_000
    };
    let (far_fine, far_coarse) =
        addr_point_cells(-pt.lat_e7, antipode_lon, fine_level, coarse_level);
    match level {
        crate::test_hooks::GeocodeCellLevel::Fine => (far_fine, coarse_cid),
        crate::test_hooks::GeocodeCellLevel::Coarse => (fine_cid, far_coarse),
    }
}

fn bucket_for_cell(cell_id: u64) -> usize {
    (cell_id >> 56) as usize
}

fn bucket_path(bucket_dir: &ScratchSubdir, bucket: usize) -> Result<ScratchPath> {
    bucket_dir.file(&format!("{bucket:03}"))
}

/// Name the bucket file in a write or flush failure. Called only on the
/// error path, so the per-record success path is unchanged.
#[cold]
fn bucket_write_error(
    e: std::io::Error,
    bucket_dir: &ScratchSubdir,
    bucket: usize,
) -> crate::error::Error {
    Error::from(e).context(format!(
        "failed to write scratch bucket {}/{bucket:03}",
        bucket_dir.display()
    ))
}

/// One contiguous run of a chunk's records, stably partitioned by bucket:
/// bucket `b`'s records are `bytes[bounds[b] * SIZE..bounds[b + 1] * SIZE]`,
/// in the order the run produced them.
struct BucketSlices {
    bytes: Vec<u8>,
    bounds: Vec<usize>,
}

impl BucketSlices {
    /// Counting sort by bucket. Stable: each record lands behind every
    /// earlier record of its bucket, which Stage B depends on (see
    /// [`append_chunk`]).
    fn partition(records: impl Iterator<Item = BucketRecord> + Clone) -> Self {
        let mut bounds = vec![0usize; GEOCODE_NUM_BUCKETS + 1];
        for record in records.clone() {
            bounds[bucket_for_cell(record.cell_id) + 1] += 1;
        }
        for b in 0..GEOCODE_NUM_BUCKETS {
            bounds[b + 1] += bounds[b];
        }
        let mut bytes = vec![0u8; bounds[GEOCODE_NUM_BUCKETS] * BucketRecord::SIZE];
        let mut cursor = bounds[..GEOCODE_NUM_BUCKETS].to_vec();
        for record in records {
            let slot = &mut cursor[bucket_for_cell(record.cell_id)];
            let at = *slot * BucketRecord::SIZE;
            bytes[at..at + BucketRecord::SIZE].copy_from_slice(&record.to_bytes());
            *slot += 1;
        }
        Self { bytes, bounds }
    }

    fn bucket(&self, b: usize) -> &[u8] {
        &self.bytes[self.bounds[b] * BucketRecord::SIZE..self.bounds[b + 1] * BucketRecord::SIZE]
    }
}

/// Append one chunk's records to both bucket trees: each entry gives one fine
/// record and at most one coarse record.
///
/// Byte identity of the index rests on the record order inside every bucket
/// file: Stage B's `sort_unstable_by_key` decides the order of a cell's list
/// entries from it. That order is the serial one, chunk by chunk, entry by
/// entry, fine and coarse kept apart: the entries are cut into contiguous
/// runs in order, each run is partitioned stably, and bucket `b` receives
/// run 0's records for `b`, then run 1's, and so on. Distinct buckets are
/// distinct files, so they are written in parallel. A bucket file is created
/// only when a record selects it, as with one record at a time.
fn append_chunk<E: Sync>(
    entries: &[E],
    fine: impl Fn(&E) -> BucketRecord + Sync,
    coarse: impl Fn(&E) -> Option<BucketRecord> + Sync,
    trees: [(&mut [Option<BufWriter<ScratchFile>>], &ScratchSubdir); 2],
    executor: &crate::host::Executor,
) -> Result<()> {
    use rayon::prelude::*;

    executor.install(|| {
        // The run length only sizes the parallel work; any length gives the
        // same bytes.
        let run_len = entries
            .len()
            .div_ceil(rayon::current_num_threads().saturating_mul(4))
            .max(1);
        let runs: Vec<[BucketSlices; 2]> = entries
            .par_chunks(run_len)
            .map(|run| {
                [
                    BucketSlices::partition(run.iter().map(&fine)),
                    BucketSlices::partition(run.iter().filter_map(&coarse)),
                ]
            })
            .collect();
        for (tree, (writers, bucket_dir)) in trees.into_iter().enumerate() {
            writers
                .par_iter_mut()
                .enumerate()
                .try_for_each(|(bucket, slot)| -> Result<()> {
                    for run in &runs {
                        let bytes = run[tree].bucket(bucket);
                        if bytes.is_empty() {
                            continue;
                        }
                        let writer = match slot.take() {
                            Some(writer) => slot.insert(writer),
                            None => slot.insert(BufWriter::new(crate::scratch::create_file(
                                &bucket_path(bucket_dir, bucket)?,
                            )?)),
                        };
                        writer
                            .write_all(bytes)
                            .map_err(|e| bucket_write_error(e, bucket_dir, bucket))?;
                    }
                    Ok(())
                })?;
        }
        Ok(())
    })
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
    /// Threads for Stage B's per-bucket workers, which run on their own
    /// threads (`crate::ordered_pool`), not on `executor`. Capped at the
    /// Stage B window, since no more buckets than that are ever admitted.
    pub(super) stage_b_workers: usize,
    /// Checked once per Stage A chunk and per Stage B sort window: the
    /// chunk boundaries are where this pass already returns to one thread.
    pub(super) cancel: &'a crate::host::Recorder,
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
        stage_b_workers,
        cancel,
    } = p;
    let stage_b_workers = *stage_b_workers;
    let invocation: &crate::host::Recorder = cancel;
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
    let mut fine_writers: Vec<Option<BufWriter<ScratchFile>>> =
        (0..GEOCODE_NUM_BUCKETS).map(|_| None).collect();
    let mut coarse_writers: Vec<Option<BufWriter<ScratchFile>>> =
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
    invocation.marker("GEOCODE_PASS3_STAGEA_STREETS_START");
    let mut chunk_start = 0u32;
    // loop-ends: one chunk per pass up to the count; the loads read results
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
                    // (ADR-0018) is removed on the unwind.
                    // Release builds compile this out entirely.
                    #[cfg(pbfhogg_test_hooks)]
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

        let record = |cell_id, wi, si| BucketRecord {
            cell_id,
            entry_type: BUCKET_ENTRY_STREET,
            index: wi,
            segment: si,
        };
        append_chunk(
            &entries,
            |&(fine_cid, _, wi, si)| record(fine_cid, wi, si),
            |&(_, coarse_opt, wi, si)| coarse_opt.map(|c| record(c, wi, si)),
            [
                (&mut fine_writers, &fine_bucket_dir),
                (&mut coarse_writers, &coarse_bucket_dir),
            ],
            executor,
        )?;
        chunk_start = chunk_end;
    }
    invocation.marker("GEOCODE_PASS3_STAGEA_STREETS_END");

    // Addr points: single cell per point, no cover_segment. Derive fine
    // and coarse via CellID.parent() chain from the same LatLng.
    invocation.marker("GEOCODE_PASS3_STAGEA_ADDR_START");
    let addr_count = addr_point_count as usize;
    let mut chunk_start = 0usize;
    // loop-ends: one chunk per pass up to the count; the loads read results
    while chunk_start < addr_count {
        cancel.check()?;
        let chunk_end = (chunk_start + GEOCODE_ADDR_CHUNK).min(addr_count);
        #[expect(
            clippy::expect_used,
            reason = "every index is below addr_point_count, a u32"
        )]
        let addr_entries: Vec<(u64, u64, u32)> = executor.install(|| {
            (chunk_start..chunk_end)
                .into_par_iter()
                .filter_map(|idx| {
                    // Same producer-owned file rule as the streets above.
                    let Some(pt) = addr_points.get(idx) else {
                        first_missing_addr.fetch_min(idx, AtomicOrdering::Relaxed);
                        return None;
                    };
                    let (fine_cid, coarse_cid) =
                        addr_point_cells(pt.lat_e7, pt.lon_e7, fine_level, coarse_level);
                    #[cfg(pbfhogg_test_hooks)]
                    let (fine_cid, coarse_cid) = misfile_address(
                        cancel.faults(),
                        idx,
                        pt,
                        fine_cid,
                        coarse_cid,
                        fine_level,
                        coarse_level,
                    );
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
        let record = |cell_id, idx| BucketRecord {
            cell_id,
            entry_type: BUCKET_ENTRY_ADDR,
            index: idx,
            segment: 0,
        };
        append_chunk(
            &addr_entries,
            |&(fine_cid, _, idx)| record(fine_cid, idx),
            |&(_, coarse_cid, idx)| Some(record(coarse_cid, idx)),
            [
                (&mut fine_writers, &fine_bucket_dir),
                (&mut coarse_writers, &coarse_bucket_dir),
            ],
            executor,
        )?;
        chunk_start = chunk_end;
    }
    invocation.marker("GEOCODE_PASS3_STAGEA_ADDR_END");

    // Interpolation (fused fine + coarse, same shape as streets)
    invocation.marker("GEOCODE_PASS3_STAGEA_INTERP_START");
    let mut interp_chunk_start = 0usize;
    // loop-ends: one chunk per pass up to the count; the loads read results
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
                        #[expect(
                            clippy::expect_used,
                            reason = "Pass 2 refuses an interpolation-way count over u32::MAX"
                        )]
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
        let record = |cell_id, wi, si| BucketRecord {
            cell_id,
            entry_type: BUCKET_ENTRY_INTERP,
            index: wi,
            segment: si,
        };
        append_chunk(
            &interp_entries,
            |&(fine_cid, _, wi, si)| record(fine_cid, wi, si),
            |&(_, coarse_opt, wi, si)| coarse_opt.map(|c| record(c, wi, si)),
            [
                (&mut fine_writers, &fine_bucket_dir),
                (&mut coarse_writers, &coarse_bucket_dir),
            ],
            executor,
        )?;
        interp_chunk_start = interp_chunk_end;
    }
    invocation.marker("GEOCODE_PASS3_STAGEA_INTERP_END");

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
    invocation.marker("GEOCODE_PASS3_STAGEB_FINE_START");
    let fine_count = run_stage_b(
        output_dir,
        scratch,
        &fine_bucket_dir,
        fine_cells_file,
        fine_street_entries_file,
        fine_addr_entries_file,
        fine_interp_entries_file,
        stage_b_workers,
        cancel,
    )?;
    scratch.remove_tree(&fine_bucket_dir);
    invocation.marker("GEOCODE_PASS3_STAGEB_FINE_END");

    invocation.marker("GEOCODE_PASS3_STAGEB_COARSE_START");
    let coarse_count = run_stage_b(
        output_dir,
        scratch,
        &coarse_bucket_dir,
        coarse_cells_file,
        coarse_street_entries_file,
        coarse_addr_entries_file,
        coarse_interp_entries_file,
        stage_b_workers,
        cancel,
    )?;
    scratch.remove_tree(&coarse_bucket_dir);
    invocation.marker("GEOCODE_PASS3_STAGEB_COARSE_END");

    Ok((fine_count, coarse_count))
}

/// Read one bucket file, free it, and sort its records by cell id. An absent
/// file is a bucket no record selected.
///
/// The sort is `sort_unstable_by_key` over the records in file order, and
/// both are part of the index's bytes: the order of a cell's list entries
/// is whatever this sort makes of the file order (see [`append_chunk`]).
fn read_sorted_bucket(
    scratch: &crate::scratch::RunScratch,
    bucket_dir: &ScratchSubdir,
    bucket_idx: usize,
) -> Result<Vec<BucketRecord>> {
    let bucket_path = bucket_path(bucket_dir, bucket_idx)?;
    let mut file = match crate::scratch::open(&bucket_path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::from(e).context(format!(
                "failed to open scratch bucket {}",
                bucket_path.display()
            )));
        }
    };
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut data).map_err(|e| {
        Error::from(e).context(format!(
            "failed to read scratch bucket {}",
            bucket_path.display()
        ))
    })?;
    drop(file);
    // Freed as soon as it is read to give the space back; the tree itself is
    // removed by the caller afterwards.
    scratch.unlink(&bucket_path).map_err(|e| {
        Error::from(e).context(format!(
            "failed to remove scratch bucket {}",
            bucket_path.display()
        ))
    })?;
    let mut entries = parse_bucket_file(&data)?;
    drop(data);
    entries.sort_unstable_by_key(|e| e.cell_id);
    Ok(entries)
}

/// One bucket's share of the four output files, with every list offset in
/// `cells` relative to the start of this bucket's list bytes; the writer
/// rebases them ([`rebase_cells`]).
#[derive(Default)]
struct EncodedBucket {
    cells: Vec<u8>,
    streets: Vec<u8>,
    addrs: Vec<u8>,
    interps: Vec<u8>,
    cell_count: usize,
    /// First and last cell id; `None` for an empty bucket.
    span: Option<(u64, u64)>,
}

/// Append one list (count prefix, then items) to `out`, refusing a count
/// over the on-disk `ListCount`.
///
/// INVARIANT: the on-disk list count is `ListCount` (u16), the framing
/// `format::Table::list` reads. Hard-error if any cell's per-type entry count
/// exceeds `u16::MAX`, rather than silently truncating. If this ever fires in
/// practice, bump the on-disk count to u32 and increment `FORMAT_VERSION`. Do
/// NOT restore a `.min(u16::MAX)` here - silent truncation made this a latent
/// bug for a long time.
fn encode_list<const N: usize>(
    out: &mut Vec<u8>,
    items: &[&BucketRecord],
    kind: &str,
    cell_id: u64,
    item: impl Fn(&BucketRecord) -> [u8; N],
) -> Result<u64> {
    if items.is_empty() {
        return Ok(NO_DATA_U64);
    }
    let offset = out.len() as u64;
    let count = ListCount::try_from(items.len()).map_err(|_| {
        Error::invalid_data(format!(
            "geocode Stage B: cell {} ({cell_id}) has {} {kind} entries, exceeds u16::MAX. \
             Bump on-disk count to u32 and increment FORMAT_VERSION.",
            CellID(cell_id),
            items.len()
        ))
    })?;
    out.extend_from_slice(&count.to_le_bytes());
    for e in items {
        out.extend_from_slice(&item(e));
    }
    Ok(offset)
}

/// Group one sorted bucket by cell and encode its cell records and lists.
fn encode_bucket(entries: &[BucketRecord]) -> Result<EncodedBucket> {
    let mut out = EncodedBucket::default();
    let mut streets: Vec<&BucketRecord> = Vec::new();
    let mut addrs: Vec<&BucketRecord> = Vec::new();
    let mut interps: Vec<&BucketRecord> = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let cell_id = entries[i].cell_id;
        let group_start = i;
        while i < entries.len() && entries[i].cell_id == cell_id {
            i += 1;
        }
        // The reader binary-searches the cell table, so a cell not above
        // the one before it would be silently misread. The sort makes this
        // hold inside a bucket; the writer checks it across buckets.
        if let Some((_, last)) = out.span
            && cell_id <= last
        {
            return Err(Error::internal(format!(
                "geocode Stage B: cell {} ({cell_id}) is not above the previous cell {} ({last})",
                CellID(cell_id),
                CellID(last)
            )));
        }

        streets.clear();
        addrs.clear();
        interps.clear();
        for e in &entries[group_start..i] {
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
        let segment = |e: &BucketRecord| {
            SegmentRef {
                way_index: e.index,
                segment_index: e.segment,
            }
            .to_bytes()
        };
        let gc = GeoCell {
            cell_id,
            street_offset: encode_list(&mut out.streets, &streets, "street", cell_id, segment)?,
            addr_offset: encode_list(&mut out.addrs, &addrs, "addr", cell_id, |e| {
                AddrEntry {
                    addr_index: e.index,
                }
                .to_bytes()
            })?,
            interp_offset: encode_list(&mut out.interps, &interps, "interp", cell_id, segment)?,
        };
        out.cells.extend_from_slice(&gc.to_bytes());
        out.cell_count += 1;
        out.span = Some((out.span.map_or(cell_id, |(first, _)| first), cell_id));
    }
    Ok(out)
}

/// Byte position of `GeoCell`'s three list offsets (street, addr, interp),
/// each a little-endian u64. Pinned against the record's own encoding by
/// `rebase_cells_matches_absolute_encoding`.
const GEO_CELL_OFFSET_FIELDS: [usize; 3] = [8, 16, 24];

/// Add `bases` (street, addr, interp) to every present list offset of the
/// encoded cells; `NO_DATA_U64` stays as it is.
fn rebase_cells(cells: &mut [u8], bases: [u64; 3]) {
    if bases == [0; 3] {
        return;
    }
    for cell in cells.as_chunks_mut::<{ GeoCell::SIZE }>().0 {
        for (&at, base) in GEO_CELL_OFFSET_FIELDS.iter().zip(bases) {
            let field = &mut cell[at..at + 8];
            let mut raw = [0u8; 8];
            raw.copy_from_slice(field);
            let offset = u64::from_le_bytes(raw);
            if offset != NO_DATA_U64 {
                field.copy_from_slice(&(offset + base).to_le_bytes());
            }
        }
    }
}

/// Stage B for one bucket tree: per-bucket read, sort and encode on
/// `workers` threads through `crate::ordered_pool`, with the calling thread
/// writing each bucket's bytes in bucket order. Run once per tree by
/// [`bucketed_cell_assignment_fused`].
///
/// At most `GEOCODE_STAGE_B_SORT_WINDOW` buckets are admitted and not yet
/// written; a slow bucket (the cell-id top byte is badly skewed at planet)
/// holds back only the writer, while the buckets behind it are sorted and
/// encoded. Each admitted bucket holds its records (16 bytes each) while it
/// encodes and its encoded bytes after, so the bound is in buckets, not
/// bytes. Bytes are the serial ones: bucket order makes cell ids ascend
/// across buckets (checked at each bucket boundary), and each bucket's
/// lists are rebased onto the bytes written before it. A failure surfaces
/// for the lowest failing bucket.
#[allow(clippy::too_many_arguments)]
// Filenames are output roles; the fine/coarse caller also owns the matching
// bucket directory. Keep those assignments together at each call site.
fn run_stage_b(
    output_dir: &Path,
    scratch: &crate::scratch::RunScratch,
    bucket_dir: &ScratchSubdir,
    cells_file: &str,
    street_entries_file: &str,
    addr_entries_file: &str,
    interp_entries_file: &str,
    workers: usize,
    cancel: &crate::host::Recorder,
) -> Result<u32> {
    let mut cells_out = super::create_data_file(&output_dir.join(cells_file))?;
    let mut street_out = super::create_data_file(&output_dir.join(street_entries_file))?;
    let mut addr_out = super::create_data_file(&output_dir.join(addr_entries_file))?;
    let mut interp_out = super::create_data_file(&output_dir.join(interp_entries_file))?;

    // Offsets of the next list in each entries file.
    let mut bases = [0u64; 3];
    let mut total_cells: u32 = 0;
    let mut prev_cell_id: Option<u64> = None;

    let window = std::num::NonZeroUsize::new(GEOCODE_STAGE_B_SORT_WINDOW)
        .unwrap_or(std::num::NonZeroUsize::MIN);
    let mut shards = vec![(); workers.clamp(1, window.get())];
    crate::ordered_pool::run_ordered(
        GEOCODE_NUM_BUCKETS,
        &mut shards,
        crate::ordered_pool::PoolConfig {
            window,
            cancel,
            waits: crate::ordered_pool::Waits::default(),
        },
        |_| (),
        |(), (), bucket_idx| {
            let entries = read_sorted_bucket(scratch, bucket_dir, bucket_idx)?;
            encode_bucket(&entries)
        },
        &mut crate::ordered_pool::ConsumeFn(
            |_bucket: usize, mut out: EncodedBucket| -> Result<()> {
                let Some((first, last)) = out.span else {
                    return Ok(());
                };
                if let Some(prev) = prev_cell_id
                    && first <= prev
                {
                    return Err(Error::internal(format!(
                        "geocode Stage B: cell {} ({first}) is not above the previous cell {} ({prev})",
                        CellID(first),
                        CellID(prev)
                    )));
                }
                rebase_cells(&mut out.cells, bases);
                cells_out.write_all(&out.cells)?;
                street_out.write_all(&out.streets)?;
                addr_out.write_all(&out.addrs)?;
                interp_out.write_all(&out.interps)?;
                bases[0] += out.streets.len() as u64;
                bases[1] += out.addrs.len() as u64;
                bases[2] += out.interps.len() as u64;
                total_cells = u32::try_from(out.cell_count)
                    .ok()
                    .and_then(|n| total_cells.checked_add(n))
                    .ok_or_else(|| {
                        Error::invalid_data(
                            "geocode cell count exceeds u32::MAX; bump FORMAT_VERSION",
                        )
                    })?;
                prev_cell_id = Some(last);
                Ok(())
            },
        ),
    )?;

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

    // Per-polygon work is independent: edge-cell cover + flood fill read the
    // polygon vertices in isolation and produce a disjoint set of
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
    #[expect(
        clippy::expect_used,
        reason = "the builder refuses an admin-polygon count over u32::MAX before cell assignment"
    )]
    let poly_index = u32::try_from(poly_idx).expect("admin count checked before cell assignment");

    let hole_slices: Vec<&[(f64, f64)]> = hole_rings.iter().map(Vec::as_slice).collect();

    // Edge cells: every cell the boundary of the reader's point-in-polygon
    // predicate passes through. That predicate is
    // `geo::point_in_ring_with_antimeridian` over these very rings (the
    // reader parses the stored vertices the same way), so its boundary is
    // each ring's planar edges in `geo::pip_ring_frame`, including
    // `point_in_ring`'s implicit closing edge from the last vertex to the
    // first (zero length for a closed ring as given, 360 degrees per turn for
    // an unwrapped ring that winds around a pole), and, for an unwrapped
    // ring, the antimeridian over the ring's latitude range: its +/-360
    // retries are not periodic, so the answer can change across +/-180 where
    // no edge runs (above and below the ring's latitudes every retry is
    // outside, so the seam carries no change there). The antimeridian is a
    // grid line of every S2 face it crosses, so that change only ever sits on
    // cell boundaries; covering it keeps the closed cells on both sides out of
    // the interior too. With all of it covered no cell outside `edge_cells`
    // holds a change of the predicate, so the centre test below decides a
    // whole cell.
    let mut edge_cells = rustc_hash::FxHashSet::default();
    for ring in std::iter::once(&ext_f64).chain(hole_rings.iter()) {
        cover_pip_ring_boundary(ring, admin_level, &mut edge_cells);
    }

    let mut entries: Vec<AdminCellEntry> = edge_cells
        .iter()
        .map(|&cid| AdminCellEntry {
            cell_id: cid,
            poly_index,
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
                poly_index,
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

/// Insert into `cells` every cell at `level` on the boundary the point-in-ring
/// test has for `ring` (`(lon, lat)` degrees, as `geo::point_in_ring` takes
/// it); see `admin_cells_for_polygon`.
fn cover_pip_ring_boundary(ring: &[(f64, f64)], level: u8, cells: &mut rustc_hash::FxHashSet<u64>) {
    let frame = crate::geo::pip_ring_frame(ring);
    let n = frame.len();
    if n == 0 {
        return;
    }
    for i in 0..n {
        let (lon1, lat1) = frame[i];
        let (lon2, lat2) = frame[(i + 1) % n];
        cover_path_raw((lat1, lon1), (lat2, lon2), level, |cell| {
            cells.insert(cell);
        });
    }
    if matches!(frame, std::borrow::Cow::Owned(_)) {
        let (lat_min, lat_max) = frame
            .iter()
            .fold((90.0f64, -90.0f64), |(lo, hi), &(_, lat)| {
                (lo.min(lat), hi.max(lat))
            });
        cover_path_raw((lat_min, 180.0), (lat_max, 180.0), level, |cell| {
            cells.insert(cell);
        });
    }
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

    /// Records with many equal cell ids across a few buckets and all three
    /// entry types, in a fixed pseudo-random order.
    fn mixed_records(n: u32) -> Vec<BucketRecord> {
        let mut state = 0x9e37_79b9_u32;
        (0..n)
            .map(|i| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let bucket = u64::from(state >> 29) * 37; // 8 buckets, spread out
                let cell = u64::from((state >> 8) & 0x1f); // 32 cells per bucket
                BucketRecord {
                    cell_id: (bucket << 56) | (cell << 20) | 1,
                    entry_type: [BUCKET_ENTRY_STREET, BUCKET_ENTRY_ADDR, BUCKET_ENTRY_INTERP]
                        [(state % 3) as usize],
                    index: i,
                    segment: (state & 0xff) as u16,
                }
            })
            .collect()
    }

    /// Concatenating each run's bucket slice, run by run, gives exactly the
    /// records the serial writer would append to that bucket, in its order,
    /// whatever the run length.
    #[test]
    fn partitioned_runs_keep_the_serial_bucket_order() {
        let records = mixed_records(5_000);
        for run_len in [1, 7, 333, 5_000] {
            let runs: Vec<BucketSlices> = records
                .chunks(run_len)
                .map(|run| BucketSlices::partition(run.iter().copied()))
                .collect();
            for bucket in 0..GEOCODE_NUM_BUCKETS {
                let serial: Vec<u8> = records
                    .iter()
                    .filter(|r| bucket_for_cell(r.cell_id) == bucket)
                    .flat_map(|r| r.to_bytes())
                    .collect();
                let parted: Vec<u8> = runs
                    .iter()
                    .flat_map(|r| r.bucket(bucket).to_vec())
                    .collect();
                assert_eq!(parted, serial, "bucket {bucket}, run length {run_len}");
            }
        }
    }

    /// Encoding bucket by bucket and rebasing each onto the bytes before it
    /// gives the bytes of one encode over all buckets in order, which is the
    /// serial Stage B's output.
    #[test]
    fn rebased_bucket_encodings_equal_one_absolute_encoding() {
        let mut records = mixed_records(5_000);
        let mut sorted_buckets: Vec<Vec<BucketRecord>> = Vec::new();
        for bucket in 0..GEOCODE_NUM_BUCKETS {
            let mut entries: Vec<BucketRecord> = records
                .iter()
                .copied()
                .filter(|r| bucket_for_cell(r.cell_id) == bucket)
                .collect();
            entries.sort_unstable_by_key(|e| e.cell_id);
            sorted_buckets.push(entries);
        }
        records = sorted_buckets.concat();
        let whole = encode_bucket(&records).expect("whole encode");

        let mut joined = EncodedBucket::default();
        let mut bases = [0u64; 3];
        for entries in &sorted_buckets {
            let mut part = encode_bucket(entries).expect("bucket encode");
            rebase_cells(&mut part.cells, bases);
            bases[0] += part.streets.len() as u64;
            bases[1] += part.addrs.len() as u64;
            bases[2] += part.interps.len() as u64;
            joined.cells.extend_from_slice(&part.cells);
            joined.streets.extend_from_slice(&part.streets);
            joined.addrs.extend_from_slice(&part.addrs);
            joined.interps.extend_from_slice(&part.interps);
            joined.cell_count += part.cell_count;
        }
        assert!(whole.cell_count > 32, "fixture must span several buckets");
        assert_eq!(joined.cell_count, whole.cell_count);
        assert_eq!(joined.cells, whole.cells);
        assert_eq!(joined.streets, whole.streets);
        assert_eq!(joined.addrs, whole.addrs);
        assert_eq!(joined.interps, whole.interps);
    }

    #[test]
    fn rebase_cells_matches_absolute_encoding() {
        let cell = |street, addr, interp| GeoCell {
            cell_id: 0x0102_0304_0506_0708,
            street_offset: street,
            addr_offset: addr,
            interp_offset: interp,
        };
        let mut bytes = cell(5, NO_DATA_U64, 0).to_bytes().to_vec();
        rebase_cells(&mut bytes, [100, 200, 300]);
        assert_eq!(bytes, cell(105, NO_DATA_U64, 300).to_bytes().to_vec());
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

    fn cover_e7(lat1: i32, lon1: i32, lat2: i32, lon2: i32, level: u8, emit: impl FnMut(u64)) {
        cover_segment(
            &Vertex::new(lat1, lon1),
            &Vertex::new(lat2, lon2),
            level,
            emit,
        );
    }

    #[test]
    fn cover_segment_same_cell() {
        // Two very close points in Copenhagen - should be in the same S2 cell at level 17.
        let lat1 = 556_762_000; // 55.6762
        let lon1 = 125_683_000; // 12.5683
        let lat2 = 556_762_100; // 55.67621 - ~0.1m away
        let lon2 = 125_683_100;

        let mut cells = Vec::new();
        cover_e7(lat1, lon1, lat2, lon2, 17, |c| cells.push(c));

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
        cover_e7(lat1, lon1, lat2, lon2, 17, |c| cells.push(c));

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
        cover_e7(lat1, lon1, lat2, lon2, 17, |c| cells.push(c));

        assert!(cells.contains(&c1), "first endpoint cell must be included");
        assert!(cells.contains(&c2), "second endpoint cell must be included");
    }

    /// A vertex at exactly lon -180 off the poles sits on S2 face 3's
    /// midline, where -180 and +180 land in different cells at every level.
    /// The endpoint cells computed from the vertex as given must still be in
    /// the cover, for a segment running along the antimeridian and for one
    /// leaving it westward.
    #[test]
    fn cover_segment_includes_endpoint_cells_at_lon_minus_180() {
        for (lat1, lon1, lat2, lon2) in [
            (0, -1_800_000_000, 100_000_000, -1_800_000_000),
            (100_000_000, -1_800_000_000, 0, -1_800_000_000),
            (0, -1_800_000_000, 0, 1_799_000_000),
            (0, 1_799_000_000, 0, -1_800_000_000),
        ] {
            let endpoint = |lat_e7: i32, lon_e7: i32| {
                CellID::from(LatLng::from_degrees(
                    f64::from(lat_e7) * 1e-7,
                    f64::from(lon_e7) * 1e-7,
                ))
                .parent(12)
                .0
            };
            let mut cells = Vec::new();
            cover_e7(lat1, lon1, lat2, lon2, 12, |c| cells.push(c));
            assert!(
                cells.contains(&endpoint(lat1, lon1)),
                "first endpoint cell missing for ({lat1}, {lon1}) -> ({lat2}, {lon2})"
            );
            assert!(
                cells.contains(&endpoint(lat2, lon2)),
                "second endpoint cell missing for ({lat1}, {lon1}) -> ({lat2}, {lon2})"
            );
        }
    }

    /// An early implementation capped the walk at 256 steps, so it could emit
    /// at most 257 cells for a segment of any length. The lower bound here is
    /// what makes that a failure rather than a pass.
    #[test]
    fn cover_segment_long_segment_is_complete_and_unique() {
        let mut cells = Vec::new();
        cover_e7(
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
            &Vertex::new(556_700_000, 125_600_000),
            &Vertex::new(556_700_000, 135_000_000),
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

    /// With the edge cover conservative in the point-in-polygon predicate's
    /// own geometry, a cell flagged interior holds no change of that
    /// predicate: every point of it is inside. Checked on a grid of points in
    /// each flagged cell, for an ordinary ring with a hole, and for a ring
    /// whose unwrapped longitudes span more than 360 degrees, where the
    /// predicate's +/-360 retries change its answer across the antimeridian
    /// with no edge there.
    #[test]
    fn interior_flagged_cells_are_wholly_inside() {
        let ring = |pts: &[(i32, i32)]| -> Vec<NodeCoord> {
            pts.iter()
                .map(|&(lon_deg, lat_deg)| NodeCoord {
                    lat_e7: lat_deg * 10_000_000,
                    lon_e7: lon_deg * 10_000_000,
                })
                .collect()
        };
        let mut holed = ring(&[(0, 0), (9, 1), (10, 10), (1, 9), (0, 0)]);
        holed.push(RING_SENTINEL);
        holed.extend(ring(&[(3, 3), (6, 4), (5, 6), (3, 3)]));
        let seam = ring(&[
            (170, 0),
            (-70, 30),
            (50, 60),
            (170, 60),
            (-70, 60),
            (50, 30),
            (170, 0),
        ]);
        for (vertices, level) in [(holed, 7u8), (seam, 4u8)] {
            let poly = AssembledPolygon {
                admin_level: 8,
                name_offset: 0,
                country_code_offset: 0,
                area: 1.0,
                vertices,
            };
            let (ext, holes) = parse_polygon_rings(poly.vertices.iter().copied());
            let hole_slices: Vec<&[(f64, f64)]> = holes.iter().map(Vec::as_slice).collect();
            let entries = admin_cells_for_polygon(0, &poly, level);
            let interior: Vec<_> = entries.iter().filter(|e| e.is_interior).collect();
            assert!(!interior.is_empty(), "fixture must have interior cells");
            if level == 4 {
                // The antimeridian is a grid line of every face it crosses, so
                // the predicate's change there sits on cell boundaries: the
                // cells on its two sides at lat 45 hold opposite answers at
                // the shared boundary point, and neither may be interior.
                assert!(
                    crate::geo::point_in_polygon(179.999, 45.0, &ext, &hole_slices)
                        && !crate::geo::point_in_polygon(-179.999, 45.0, &ext, &hole_slices),
                    "fixture must change its answer across the antimeridian at lat 45"
                );
                for lon in [179.999, -179.999] {
                    let cell = CellID::from(LatLng::from_degrees(45.0, lon)).parent(4).0;
                    assert!(
                        entries.iter().any(|e| e.cell_id == cell && !e.is_interior),
                        "the seam cell at lon {lon} must be an edge cell"
                    );
                }
            }
            for entry in interior {
                let cell = s2::cell::Cell::from(CellID(entry.cell_id));
                let corners: Vec<s2::point::Point> = (0..4).map(|k| cell.vertex(k)).collect();
                const N: usize = 24;
                for a in 0..=N {
                    for b in 0..=N {
                        let (s, t) = (a as f64 / N as f64, b as f64 / N as f64);
                        let mix = |k: usize| corners[k].0;
                        let v = mix(0) * ((1.0 - s) * (1.0 - t))
                            + mix(1) * (s * (1.0 - t))
                            + mix(2) * (s * t)
                            + mix(3) * ((1.0 - s) * t);
                        let ll = LatLng::from(s2::point::Point(v.normalize()));
                        assert!(
                            crate::geo::point_in_polygon(
                                ll.lng.deg(),
                                ll.lat.deg(),
                                &ext,
                                &hole_slices
                            ),
                            "interior cell {:x} at level {level} holds an outside point \
                             ({}, {})",
                            entry.cell_id,
                            ll.lat.deg(),
                            ll.lng.deg()
                        );
                    }
                }
            }
        }
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
