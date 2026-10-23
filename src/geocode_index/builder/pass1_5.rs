//! Pass 1.5: referenced-node collection (planet-scale memory optimization).
//!
//! Scan way blobs to collect node IDs referenced by geocode-relevant ways
//! (streets, building addresses, interpolation, admin members). The dense
//! node index in pass 2 only populates entries for these nodes, reducing
//! page cache working set from ~83 GB (all 10.4B nodes) to ~16 GB (~2B
//! referenced). Same pattern as ALTW pass 0.

use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::Result;
use crate::error::Error;
use crate::read::input::InputFile;
use crate::scan::classify::ScheduledBlob;

use crate::tunables::{NODE_PREALLOC_IDS_PER_BYTE, OVERFLOW_COMPACT_MIN};

/// Record one accepted node ref: through `set_atomic` when it falls inside
/// the pre-allocated range, otherwise onto this worker's overflow list.
#[inline]
fn record_referenced_node(
    referenced_nodes: &crate::idset::IdSet,
    overflow: &mut Vec<i64>,
    prealloc_max: i64,
    node_id: i64,
) {
    if node_id > prealloc_max {
        overflow.push(node_id);
    } else {
        referenced_nodes.set_atomic(node_id);
    }
}

/// Sort and deduplicate a worker's overflow list once it has doubled since
/// the last compaction (`compacted_len`, updated here).
fn compact_overflow_nodes(overflow: &mut Vec<i64>, compacted_len: &mut usize) {
    if overflow.len() < OVERFLOW_COMPACT_MIN || overflow.len() < compacted_len.saturating_mul(2) {
        return;
    }
    overflow.sort_unstable();
    overflow.dedup();
    *compacted_len = overflow.len();
}

/// Insert the workers' overflow lists after the parallel scan, where `set`
/// may grow the set under `&mut`.
fn insert_overflow_nodes(referenced_nodes: &mut crate::idset::IdSet, lists: Vec<Vec<i64>>) {
    for list in lists {
        for node_id in list {
            referenced_nodes.set(node_id);
        }
    }
}

/// Record the accepted refs of one geocode-relevant way.
fn record_way_refs(
    referenced: &crate::idset::IdSet,
    overflow_nodes: &mut Vec<i64>,
    prealloc_max: i64,
    max_node_id: i64,
    refs: &[i64],
    negative_node_ids: &mut Vec<i64>,
) -> u64 {
    let mut negative_count = 0_u64;
    for &r in refs {
        // Only refs in `0..=max_node_id`
        // can name a node Phase 2a will
        // see. Negative refs are skipped
        // as non-atomic `IdSet::set` does.
        // A ref above the claimed bound is a
        // dangling ref on honest input (a
        // simple-strategy extract keeps
        // ways whose nodes lie outside it,
        // with IDs that can exceed every
        // node present): no coordinate can
        // ever be written for it, so
        // dropping it here changes nothing
        // downstream. That holds only while
        // the bound is true, which is why
        // Phase 2a errors on any decoded
        // node above it. Accepted refs above
        // the size-bounded dense range go to
        // this worker's overflow list instead
        // of extending `pre_allocate`.
        if r < 0 {
            negative_count = negative_count.saturating_add(1);
            super::sample_id(negative_node_ids, r);
            continue;
        }
        if r > max_node_id {
            continue;
        }
        record_referenced_node(referenced, overflow_nodes, prealloc_max, r);
    }
    negative_count
}

/// Whether any worker has already reported an error.
fn has_first_err(slot: &Mutex<Option<Error>>) -> bool {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
}

/// Keep the first error any worker reports.
fn store_first_err(slot: &Mutex<Option<Error>>, e: Error) {
    let mut slot = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(e);
    }
}

/// Hand a finished worker's overflow list to the shared collection.
fn push_overflow_list(lists: &Mutex<Vec<Vec<i64>>>, overflow_nodes: Vec<i64>) {
    if !overflow_nodes.is_empty() {
        lists
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(overflow_nodes);
    }
}

/// Fold one worker's negative-ref tally and ID sample into the shared ones,
/// once per worker after its scan loop.
fn publish_negative_node_refs(
    shared_count: &AtomicU64,
    shared_sample: &Mutex<Vec<i64>>,
    count: u64,
    sample: Vec<i64>,
) {
    shared_count.fetch_add(count, Ordering::Relaxed);
    let mut shared_sample = shared_sample
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for id in sample {
        super::sample_id(&mut shared_sample, id);
    }
}

/// Emit the negative-ref counter and, when any were skipped, a warning with
/// the bounded ID sample.
fn report_negative_node_refs(count: &AtomicU64, sample: Mutex<Vec<i64>>, host: &crate::host::Host) {
    let negative_count = count.load(Ordering::Relaxed);
    crate::debug::emit_counter(
        "geocode_negative_node_refs",
        super::counter_value(negative_count),
    );
    if negative_count > 0 {
        let sample = sample
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "geocode.pass1_5.negative_node_refs",
            "skipped {negative_count} negative node refs; node IDs (sample): {sample:?}"
        );
    }
}

#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub(super) fn run_pass1_5(
    way_schedule: &[ScheduledBlob],
    max_node_id: i64,
    input: &InputFile,
    needed_admin_ways: &crate::idset::IdSet,
    host: &crate::host::Host,
) -> Result<crate::idset::IdSet> {
    let shared_file = input.file();
    let file_len = shared_file.metadata()?.len();
    let prealloc_max =
        crate::idset::IdSet::file_bounded_max_id(max_node_id, file_len, NODE_PREALLOC_IDS_PER_BYTE);

    // One shared pre-allocated IdSet; workers write concurrently via
    // `set_atomic`. Replaces the previous per-worker `IdSet`
    // accumulation that grew to ~20 GB anon on Germany (~29.5 GB at planet)
    // because each worker allocated independent chunks across the full
    // planet ID range. Pattern follows `renumber_external/pass1.rs`.
    //
    // `max_node_id` can come from indexdata for indexed blobs. Bound the
    // eager bitmap by input size; workers keep accepted references above
    // that bound in local ID lists, inserted after the scan.
    let mut referenced_nodes = crate::idset::IdSet::new();
    referenced_nodes.pre_allocate(prealloc_max);

    // Custom worker pool that bypasses PrimitiveBlock construction
    // entirely (plan item #2). Workers pread the blob bytes, decompress,
    // and call `scan_way_geocode_tagged_refs` which walks the wire-format
    // way records directly: resolve geocode tag literals once per blob
    // against the raw string table, then for each Way parse only id,
    // keys, vals, and refs - no StringTable UTF-8 validation, no
    // group_ranges allocation, no full PrimitiveBlock materialisation.
    crate::debug::emit_marker("GEOCODE_PASS1_5_SCAN_START");
    {
        let referenced_ref = &referenced_nodes;
        let literals = crate::scan::way::GeocodeTagLiterals::standard();
        let literals_ref = &literals;
        let needed_admin_ways_ref = needed_admin_ways;

        // Work-stealing dispatch over way blobs via AtomicUsize::fetch_add.
        let next_idx = AtomicUsize::new(0);
        let next_ref = &next_idx;
        // First error observed across all workers. Workers drain until
        // the schedule is empty or the shared slot is populated.
        let first_err: Mutex<Option<Error>> = Mutex::new(None);
        let first_err_ref = &first_err;
        let overflow_lists: Mutex<Vec<Vec<i64>>> = Mutex::new(Vec::new());
        let overflow_lists_ref = &overflow_lists;
        let negative_node_ref_count = AtomicU64::new(0);
        let negative_node_ref_count_ref = &negative_node_ref_count;
        let negative_node_ids: Mutex<Vec<i64>> = Mutex::new(Vec::new());
        let negative_node_ids_ref = &negative_node_ids;

        let decode_threads = crate::host::sized_workers(
            &*host.resources,
            crate::tunables::DECODE_WORKERS_RESERVED,
            crate::tunables::DECODE_WORKERS_MIN,
            crate::tunables::DECODE_WORKERS_FALLBACK,
        );

        std::thread::scope(|scope| {
            for _ in 0..decode_threads {
                let file = std::sync::Arc::clone(shared_file);
                scope.spawn(move || {
                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();
                    let mut overflow_nodes: Vec<i64> = Vec::new();
                    let mut overflow_compacted_len = 0_usize;
                    let mut negative_node_ref_count = 0_u64;
                    let mut negative_node_id_sample = Vec::new();

                    loop {
                        // Exit early if another worker has reported an error.
                        if has_first_err(first_err_ref) {
                            return;
                        }

                        let idx = next_ref.fetch_add(1, Ordering::Relaxed);
                        if idx >= way_schedule.len() {
                            break;
                        }
                        let entry = &way_schedule[idx];

                        let result = (|| -> Result<()> {
                            // Per way blob; a cancellation leaves through the
                            // first-error slot like any blob failure (this
                            // pool has no consumer, and no sequence to keep).
                            host.cancel.check()?;
                            read_buf.resize(entry.data_size(), 0);
                            file.read_exact_at(&mut read_buf, entry.data_offset())?;
                            decompress_buf.clear();
                            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;
                            crate::scan::way::scan_way_geocode_tagged_refs(
                                &decompress_buf,
                                literals_ref,
                                &mut refs_buf,
                                &mut group_starts,
                                |way_id, flags, refs| {
                                    let is_admin = needed_admin_ways_ref.get(way_id);
                                    if flags.is_street
                                        || flags.is_building_addr
                                        || flags.is_interp
                                        || is_admin
                                    {
                                        negative_node_ref_count = negative_node_ref_count
                                            .saturating_add(record_way_refs(
                                                referenced_ref,
                                                &mut overflow_nodes,
                                                prealloc_max,
                                                max_node_id,
                                                refs,
                                                &mut negative_node_id_sample,
                                            ));
                                    }
                                },
                            )?;
                            compact_overflow_nodes(
                                &mut overflow_nodes,
                                &mut overflow_compacted_len,
                            );
                            Ok(())
                        })();

                        if let Err(e) = result {
                            store_first_err(first_err_ref, input.locate_blob(entry, e));
                            return;
                        }
                    }

                    publish_negative_node_refs(
                        negative_node_ref_count_ref,
                        negative_node_ids_ref,
                        negative_node_ref_count,
                        negative_node_id_sample,
                    );
                    push_overflow_list(overflow_lists_ref, overflow_nodes);
                });
            }
        });

        if let Some(e) = first_err
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(e);
        }

        report_negative_node_refs(&negative_node_ref_count, negative_node_ids, host);

        insert_overflow_nodes(
            &mut referenced_nodes,
            overflow_lists
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
    crate::debug::emit_marker("GEOCODE_PASS1_5_SCAN_END");
    Ok(referenced_nodes)
}

/// Single header pass that produces everything Pass 1.5 and Pass 2a need to
/// start: the way-blob schedule (for Pass 1.5 + unused by Pass 2a), the
/// node-blob schedule (for Pass 2a), the node-ID bound (used for Pass 1.5's
/// size-bounded `IdSet` pre-allocation and verified by Phase 2a), and a
/// single shared file handle reused across both phases' `pread_at` workers.
///
/// Consolidates two previously-separate header walks (Pass 1.5's
/// `build_way_schedule_and_max_node_id` and Pass 2a's
/// `build_classify_schedule(Node)`). At Europe the consolidated walk
/// costs ~16-17 s once, vs 16.6 s + 26.5 s = 43 s for the two-walk
/// shape - measured 2026-04-18 on `bf8f2038`.
///
/// Each schedule is in file order and its `seq` is local to that schedule
/// (node and way are separate `0..n` ranges).
///
/// The node-ID bound is the largest indexdata `max_id` over node blobs,
/// widened by the real maximum of every blob that carries no indexdata.
/// `require_indexdata` at the `build_geocode_index` entry rejects
/// non-indexed input unless `--force` is set; with `--force`, each
/// unindexed blob is decoded once here (in parallel) to learn which kinds
/// it holds and its largest node ID. Indexdata claims still supply the upper
/// bound checked against decoded nodes in Phase 2a; Pass 1.5 separately caps
/// dense `IdSet` pre-allocation by input file size.
#[allow(clippy::type_complexity)]
pub(super) fn build_pass2_schedules(
    input_path: &Path,
    host: &crate::host::Host,
) -> Result<(
    Vec<ScheduledBlob>, // node_schedule
    Vec<ScheduledBlob>, // way_schedule
    i64,                // max_node_id
    InputFile,
)> {
    let mut walker = crate::read::header_walker::HeaderWalker::open(input_path, host)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input_path))?;

    // Each schedule is put into file order once both are complete (see
    // `into_file_order` below).
    let mut node_schedule: Vec<ScheduledBlob> = Vec::new();
    let mut way_schedule: Vec<ScheduledBlob> = Vec::new();
    // OsmData blobs without indexdata (a `--force` run). Their kinds and
    // node-ID range are established by decoding them, below.
    let mut unindexed: Vec<ScheduledBlob> = Vec::new();
    let mut max_node_id: i64 = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let entry = ScheduledBlob::from_meta(&meta)?;
        let Some(idx) = meta.index.as_ref() else {
            unindexed.push(entry);
            continue;
        };
        match idx.kind {
            crate::ElementKind::Node => {
                if idx.max_id > max_node_id {
                    max_node_id = idx.max_id;
                }
                node_schedule.push(entry);
            }
            crate::ElementKind::Way => way_schedule.push(entry),
            crate::ElementKind::Relation => {}
        }
    }

    let shared_file = walker.input();
    drop(walker);

    // `max_node_id` must bound every node ID Phase 2a will see: Pass 1.5
    // drops refs above it as dangling, and Phase 2a turns a node above it
    // into an error. For a blob with no indexdata there is no claim to read,
    // so decode it and take the real answer - which also says exactly which
    // phases need the blob, instead of feeding every unindexed blob to both.
    // A blob holding both nodes and ways (legal, never indexed by pbfhogg)
    // goes into both schedules; Phase 2a completes before Phase 2b starts,
    // so the in-blob order of the two kinds does not matter.
    if !unindexed.is_empty() {
        let mut found: Vec<Option<UnindexedBlobKinds>> = vec![None; unindexed.len()];
        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &unindexed,
            None,
            || (),
            |block, _scratch: &mut ()| {
                let mut kinds = UnindexedBlobKinds::default();
                for element in block.elements_skip_metadata() {
                    let node_id = match element {
                        crate::Element::Node(node) => node.id(),
                        crate::Element::DenseNode(node) => node.id(),
                        crate::Element::Way(_) => {
                            kinds.has_ways = true;
                            continue;
                        }
                        crate::Element::Relation(_) => continue,
                    };
                    kinds.has_nodes = true;
                    kinds.max_node_id = kinds.max_node_id.max(node_id);
                }
                Ok(kinds)
            },
            |position, kinds| {
                if let Some(slot) = found.get_mut(position) {
                    *slot = Some(kinds);
                }
            },
            host,
        )?;
        for (entry, kinds) in unindexed.iter().zip(found) {
            let kinds = kinds.ok_or_else(|| {
                Error::internal(format!(
                    "unindexed blob at offset {} was never decoded",
                    entry.frame_start()
                ))
            })?;
            if kinds.has_nodes {
                max_node_id = max_node_id.max(kinds.max_node_id);
                node_schedule.push(*entry);
            }
            if kinds.has_ways {
                way_schedule.push(*entry);
            }
        }
    }
    into_file_order(&mut node_schedule);
    into_file_order(&mut way_schedule);

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("pass2_node_blobs", node_schedule.len() as i64);
        crate::debug::emit_counter("pass2_way_blobs", way_schedule.len() as i64);
        crate::debug::emit_counter("pass2_max_node_id", max_node_id);
    }

    Ok((node_schedule, way_schedule, max_node_id, shared_file))
}

/// What decoding an unindexed blob says about it: which of the two Pass 2
/// phases need it, and the largest node ID it holds.
#[derive(Clone, Copy, Default)]
struct UnindexedBlobKinds {
    has_nodes: bool,
    has_ways: bool,
    max_node_id: i64,
}

/// Put a schedule into file order; its positions are then its order.
fn into_file_order(schedule: &mut [ScheduledBlob]) {
    schedule.sort_unstable_by_key(ScheduledBlob::frame_start);
}

#[cfg(test)]
mod tests {
    use super::{
        NODE_PREALLOC_IDS_PER_BYTE, OVERFLOW_COMPACT_MIN, compact_overflow_nodes,
        insert_overflow_nodes, record_referenced_node,
    };

    #[test]
    fn indexdata_node_bound_scales_with_input_size() {
        let bound = |claimed, file_len| {
            crate::idset::IdSet::file_bounded_max_id(claimed, file_len, NODE_PREALLOC_IDS_PER_BYTE)
        };
        assert_eq!(bound(i64::MAX, 4096), 4096 * 64);

        let planet_bytes = 80_u64 * 1024 * 1024 * 1024;
        assert_eq!(bound(60_000_000_000, planet_bytes), 60_000_000_000);
        assert_eq!(bound(i64::MAX, u64::MAX), i64::MAX);
    }

    #[test]
    fn references_above_the_dense_bound_survive_overflow_insertion() {
        let mut referenced = crate::idset::IdSet::new();
        referenced.pre_allocate(0);
        let mut overflow = Vec::new();
        let high_id = (1_i64 << 44) + 7;

        record_referenced_node(&referenced, &mut overflow, 0, 0);
        record_referenced_node(&referenced, &mut overflow, 0, high_id);
        record_referenced_node(&referenced, &mut overflow, 0, high_id);
        assert_eq!(overflow, vec![high_id, high_id]);
        insert_overflow_nodes(&mut referenced, vec![overflow]);

        assert!(referenced.get(0));
        assert!(referenced.get(high_id));
        // The pre-allocated chunk plus one chunk for the overflow ID.
        assert_eq!(referenced.allocated_chunk_count(), 2);
    }

    #[test]
    fn overflow_lists_compact_only_after_doubling() {
        let mut compacted = 0;
        let mut small = vec![3, 1, 3];
        compact_overflow_nodes(&mut small, &mut compacted);
        assert_eq!(
            small,
            vec![3, 1, 3],
            "below the minimum, nothing is touched"
        );

        let mut list: Vec<i64> = std::iter::successors(Some(0_i64), |v| Some((v + 1) % 10))
            .take(OVERFLOW_COMPACT_MIN)
            .collect();
        compact_overflow_nodes(&mut list, &mut compacted);
        assert_eq!(list, (0..10).collect::<Vec<i64>>());
        assert_eq!(compacted, 10);
    }
}
