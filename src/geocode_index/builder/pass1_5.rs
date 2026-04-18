//! Pass 1.5: referenced-node collection (planet-scale memory optimization).
//!
//! Scan way blobs to collect node IDs referenced by geocode-relevant ways
//! (streets, building addresses, interpolation, admin members). The dense
//! node index in pass 2 only populates entries for these nodes, reducing
//! page cache working set from ~83 GB (all 10.4B nodes) to ~16 GB (~2B
//! referenced). Same pattern as ALTW pass 0.

use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::Result;

#[hotpath::measure]
pub(super) fn run_pass1_5(
    way_schedule: &[(usize, u64, usize)],
    max_node_id: i64,
    shared_file: &std::sync::Arc<std::fs::File>,
    needed_admin_ways: &crate::commands::id_set_dense::IdSetDense,
) -> Result<crate::commands::id_set_dense::IdSetDense> {
    // One shared pre-allocated IdSetDense; workers write concurrently via
    // `set_atomic`. Replaces the previous per-worker `IdSetDense`
    // accumulation that grew to ~20 GB anon on Germany (~29.5 GB at planet)
    // because each worker allocated independent chunks across the full
    // planet ID range. Pattern follows `renumber_external/pass1.rs`
    // (plan item #7 in notes/geocode-build-opportunities.md).
    //
    // `pre_allocate(max_node_id)` requires `max_node_id` to cover every ID
    // the classify closure will call `set_atomic` on. We get it from
    // `build_pass2_schedules` above, which reads indexdata from node blobs.
    // If a node blob lacks indexdata (-- force path only), the upper bound
    // is under-reported and `set_atomic` would panic via its diagnostic
    // path.
    let mut referenced_nodes = crate::commands::id_set_dense::IdSetDense::new();
    referenced_nodes.pre_allocate(max_node_id);

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
        let literals = crate::commands::way_scanner::GeocodeTagLiterals::standard();
        let literals_ref = &literals;
        let needed_admin_ways_ref = needed_admin_ways;

        // Work-stealing dispatch over way blobs via AtomicUsize::fetch_add.
        let next_idx = AtomicUsize::new(0);
        let next_ref = &next_idx;
        // First error observed across all workers. Workers drain until
        // the schedule is empty or the shared slot is populated.
        let first_err: Mutex<Option<String>> = Mutex::new(None);
        let first_err_ref = &first_err;

        let decode_threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(1))
            .unwrap_or(4);

        std::thread::scope(|scope| {
            for _ in 0..decode_threads {
                let file = std::sync::Arc::clone(shared_file);
                scope.spawn(move || {
                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();

                    loop {
                        // Exit early if another worker has reported an error.
                        if first_err_ref.lock().unwrap_or_else(
                            std::sync::PoisonError::into_inner).is_some()
                        { return; }

                        let idx = next_ref.fetch_add(1, Ordering::Relaxed);
                        if idx >= way_schedule.len() { break; }
                        let (_seq, offset, size) = way_schedule[idx];

                        let result = (|| -> std::result::Result<(), String> {
                            read_buf.resize(size, 0);
                            file.read_exact_at(&mut read_buf, offset)
                                .map_err(|e| format!("pread at {offset}: {e}"))?;
                            decompress_buf.clear();
                            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                                .map_err(|e| e.to_string())?;
                            crate::commands::way_scanner::scan_way_geocode_tagged_refs(
                                &decompress_buf,
                                literals_ref,
                                &mut refs_buf,
                                &mut group_starts,
                                |way_id, flags, refs| {
                                    let is_admin = needed_admin_ways_ref.get(way_id);
                                    if flags.is_street || flags.is_building_addr
                                        || flags.is_interp || is_admin
                                    {
                                        for &r in refs {
                                            referenced_ref.set_atomic(r);
                                        }
                                    }
                                },
                            ).map_err(|e| e.to_string())?;
                            Ok(())
                        })();

                        if let Err(e) = result {
                            let mut slot = first_err_ref.lock().unwrap_or_else(
                                std::sync::PoisonError::into_inner);
                            if slot.is_none() { *slot = Some(e); }
                            return;
                        }
                    }
                });
            }
        });

        if let Some(e) = first_err.into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(e.into());
        }
    }
    crate::debug::emit_marker("GEOCODE_PASS1_5_SCAN_END");
    Ok(referenced_nodes)
}

/// Single header pass that produces everything Pass 1.5 and Pass 2a need to
/// start: the way-blob schedule (for Pass 1.5 + unused by Pass 2a), the
/// node-blob schedule (for Pass 2a), the max node ID from indexdata (so
/// Pass 1.5 can `pre_allocate` the shared `IdSetDense`), and a single
/// shared file handle reused across both phases' `pread_at` workers.
///
/// Consolidates two previously-separate header walks (Pass 1.5's
/// `build_way_schedule_and_max_node_id` and Pass 2a's
/// `build_classify_schedule(Node)`). At Europe the consolidated walk
/// costs ~16-17 s once, vs 16.6 s + 26.5 s = 43 s for the two-walk
/// shape - measured 2026-04-18 on `bf8f2038`.
///
/// Each schedule's `seq` is local to that schedule (node and way are
/// separate `0..n` ranges, as `parallel_classify_phase`'s ReorderBuffer
/// expects).
///
/// Blobs without indexdata are added to both schedules (conservative
/// fallback matching `build_classify_schedule`'s semantics) but cannot
/// contribute to `max_node_id`. `require_indexdata` at the
/// `build_geocode_index` entry rejects non-indexed input unless `--force`
/// is set, so in practice every blob carries indexdata here.
#[allow(clippy::type_complexity)]
pub(super) fn build_pass2_schedules(
    input_path: &Path,
) -> Result<(
    Vec<(usize, u64, usize)>,  // node_schedule
    Vec<(usize, u64, usize)>,  // way_schedule
    i64,                        // max_node_id
    std::sync::Arc<std::fs::File>,
)> {
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input_path)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let mut node_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut way_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut node_seq: usize = 0;
    let mut way_seq: usize = 0;
    let mut max_node_id: i64 = 0;
    while let Some(result_item) = scanner.next_header_with_data_offset() {
        let (hdr, _frame_offset, data_offset, data_size) = result_item?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        let Some(idx) = hdr.index() else {
            // No indexdata: conservatively include in both schedules so
            // neither phase silently drops data from a --force run.
            node_schedule.push((node_seq, data_offset, data_size));
            node_seq += 1;
            way_schedule.push((way_seq, data_offset, data_size));
            way_seq += 1;
            continue;
        };
        match idx.kind {
            crate::blob_index::ElemKind::Node => {
                if idx.max_id > max_node_id {
                    max_node_id = idx.max_id;
                }
                node_schedule.push((node_seq, data_offset, data_size));
                node_seq += 1;
            }
            crate::blob_index::ElemKind::Way => {
                way_schedule.push((way_seq, data_offset, data_size));
                way_seq += 1;
            }
            crate::blob_index::ElemKind::Relation => {}
        }
    }

    drop(scanner);
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input_path)
            .map_err(|e| format!("failed to open {}: {e}", input_path.display()))?,
    );

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("pass2_node_blobs", node_schedule.len() as i64);
        crate::debug::emit_counter("pass2_way_blobs", way_schedule.len() as i64);
        crate::debug::emit_counter("pass2_max_node_id", max_node_id);
    }

    Ok((node_schedule, way_schedule, max_node_id, shared_file))
}
