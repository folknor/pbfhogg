//! Parallel blob classification: pread-from-workers schedule + decode loop.
//!
//! Schedules a list of blob offsets across worker threads. Each worker
//! `pread`s a compressed blob, decompresses, and runs a caller-supplied
//! closure on the decoded `PrimitiveBlock`. The consumer thread merges
//! per-blob results.
//!
//! Two modes:
//!
//! - [`parallel_classify_phase`]: per-blob result `R` is sent to the
//!   consumer for each blob (worker holds only persistent scratch state
//!   `S`). Use for dense paths where per-worker accumulation would be
//!   unbounded at planet scale.
//!
//! - [`parallel_classify_accumulate`]: per-worker accumulator `S` is held
//!   for the whole scan and merged once at completion. Use for sparse
//!   paths where the accumulator has a known small bound.
//!
//! Schedule construction lives in [`build_classify_schedule`] and
//! [`build_classify_schedules_split`].

use crate::BoxResult as Result;

/// One entry in a classification schedule: `(seq, data_offset, data_size)`.
/// `seq` is a contiguous 0..n index local to the schedule; `data_offset` and
/// `data_size` address the blob's payload in the input PBF.
pub(crate) type ScheduleEntry = (usize, u64, usize);

/// Resolve a caller-supplied `threads: Option<usize>` override into a
/// concrete decode-thread count. `None` (the default for every existing
/// caller) picks `available_parallelism() - 2` clamped to ≥ 1, the
/// convention established by the pipelined reader and matching the
/// comment in `src/read/pipeline.rs::run_pipeline`. `Some(0)` is
/// treated identically to `None` so CLI flags that map "0 = auto"
/// pass through cleanly. `Some(n)` forces exactly `n` threads.
fn resolve_thread_count(threads: Option<usize>) -> usize {
    match threads {
        Some(n) if n > 0 => n,
        _ => std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(1))
            .unwrap_or(4),
    }
}

/// Build a classification schedule from a header-only scan, optionally
/// filtering by element type. Returns `(schedule, shared_file)` ready for
/// [`parallel_classify_phase`].
///
/// Only OsmData blobs are included. When `kind_filter` is `Some`, only blobs
/// whose indexdata matches the given element type (plus blobs without
/// indexdata) are included.
///
/// Walks headers via [`HeaderWalker`](crate::read::header_walker::HeaderWalker)
/// so blob bodies are not dragged into the page cache during the scan. The
/// shared `Arc<File>` handed back for `pread`-based body reads is the
/// walker's own file handle (opened with `posix_fadvise(RANDOM)`),
/// reused to avoid a second `open` at scan end.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn build_classify_schedule(
    input: &std::path::Path,
    kind_filter: Option<crate::blob_meta::ElemKind>,
) -> Result<(Vec<ScheduleEntry>, std::sync::Arc<std::fs::File>)> {
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input)?;
    // Consume the first blob (expected OSMHeader). Bug-for-bug equivalent
    // to the prior `next_header_skip_blob` call: on an empty file return
    // MissingHeader; on non-empty files the first blob is dropped without
    // kind validation (subsequent non-OsmData blobs are filtered below).
    let _ = walker.next_header()?
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))?;
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_END");

    crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_START");
    let mut schedule: Vec<ScheduleEntry> = Vec::new();
    let mut seq: usize = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) { continue; }
        if let Some(filter_kind) = kind_filter {
            if let Some(idx) = &meta.index {
                if idx.kind != filter_kind { continue; }
            }
        }
        schedule.push((seq, meta.data_offset, meta.data_size));
        seq += 1;
    }
    crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_END");

    crate::debug::emit_marker("SCHEDULE_SCANNER_DROP_START");
    let shared_file = std::sync::Arc::clone(walker.shared_file());
    drop(walker);
    crate::debug::emit_marker("SCHEDULE_SCANNER_DROP_END");

    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("schedule_blobs", schedule.len() as i64);
    Ok((schedule, shared_file))
}

/// Like [`build_classify_schedule`] but returns three per-kind schedules
/// from a single header pass. At planet / Europe scale the header walk is
/// itself ~15 s; callers that need all three kinds (currently `check_refs`)
/// would otherwise pay that cost three times.
///
/// Blobs lacking indexdata are included in all three schedules (matching
/// the per-kind behaviour of `build_classify_schedule(.., Some(kind))`,
/// which only skips blobs whose indexdata reports a mismatched kind).
/// Each schedule's `seq` is local to that schedule (so each is a valid
/// contiguous 0..n range ready for `parallel_classify_phase`).
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::type_complexity)]
pub(crate) fn build_classify_schedules_split(
    input: &std::path::Path,
) -> Result<(
    Vec<ScheduleEntry>,
    Vec<ScheduleEntry>,
    Vec<ScheduleEntry>,
    std::sync::Arc<std::fs::File>,
)> {
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input)?;
    let _ = walker.next_header()?
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))?;
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_END");

    crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_START");
    let mut nodes: Vec<ScheduleEntry> = Vec::new();
    let mut ways: Vec<ScheduleEntry> = Vec::new();
    let mut rels: Vec<ScheduleEntry> = Vec::new();
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) { continue; }
        match meta.index.as_ref().map(|i| i.kind) {
            Some(crate::blob_meta::ElemKind::Node) => {
                nodes.push((nodes.len(), meta.data_offset, meta.data_size));
            }
            Some(crate::blob_meta::ElemKind::Way) => {
                ways.push((ways.len(), meta.data_offset, meta.data_size));
            }
            Some(crate::blob_meta::ElemKind::Relation) => {
                rels.push((rels.len(), meta.data_offset, meta.data_size));
            }
            None => {
                // Unindexed: visible to every kind filter in the legacy path,
                // so replicate to all three schedules here.
                nodes.push((nodes.len(), meta.data_offset, meta.data_size));
                ways.push((ways.len(), meta.data_offset, meta.data_size));
                rels.push((rels.len(), meta.data_offset, meta.data_size));
            }
        }
    }
    crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_END");

    crate::debug::emit_marker("SCHEDULE_SCANNER_DROP_START");
    let shared_file = std::sync::Arc::clone(walker.shared_file());
    drop(walker);
    crate::debug::emit_marker("SCHEDULE_SCANNER_DROP_END");

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("schedule_node_blobs", nodes.len() as i64);
        crate::debug::emit_counter("schedule_way_blobs", ways.len() as i64);
        crate::debug::emit_counter("schedule_relation_blobs", rels.len() as i64);
    }
    Ok((nodes, ways, rels, shared_file))
}

/// Run a parallel classification phase: pread workers decompress and classify
/// blobs, sending compact results to a consumer that merges them into ID sets.
///
/// Each entry in `schedule` is `(seq, data_offset, data_size)`. Workers pread
/// the compressed blob data, decompress, build a `PrimitiveBlock`, run the
/// `classify` closure, and send the result. The consumer calls `merge(seq, r)`
/// for each result, forwarding the blob's schedule-order sequence number so
/// callers that care (e.g. `verify_ids`, which needs cross-blob monotonicity)
/// can reorder via `ReorderBuffer` or similar. Callers that don't care ignore
/// the seq argument.
///
/// **Note:** `merge` is called in arbitrary worker-completion order, not blob
/// file order. Callers that need file-order processing must buffer by seq.
/// Per-blob streaming classify: workers send `R` per blob, keep `S` for scratch.
///
/// Use for dense/hot paths (node classify, way classify) where per-worker
/// accumulation would be unbounded at planet scale. Each per-blob `R` is
/// bounded by blob size (~8000 elements). `S` persists across blobs for
/// scratch reuse (DenseNodeColumns, decompress buffers, etc.).
///
/// For sparse paths that want per-worker accumulation, use
/// [`parallel_classify_accumulate`].
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn parallel_classify_phase<S: Send, R: Send>(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[ScheduleEntry],
    threads: Option<usize>,
    worker_init: impl Fn() -> S + Send + Sync,
    classify: impl Fn(&crate::PrimitiveBlock, &mut S) -> R + Send + Sync,
    mut merge: impl FnMut(usize, R),
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() { return Ok(()); }

    let decode_threads = resolve_thread_count(threads);

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<ScheduleEntry>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<(usize, crate::error::Result<R>)>(32);

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for &item in schedule {
                if desc_tx.send(item).is_err() { break; }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(shared_file);
            let classify_ref = &classify;
            let worker_init_ref = &worker_init;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let worker_pool = crate::blob::DecompressPool::new();
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
                let mut state = worker_init_ref();

                loop {
                    let (s, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(d) => d,
                            Err(_) => break,
                        }
                    };

                    let r: crate::error::Result<R> = (|| {
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        let mut buf = crate::blob::pool_get_pub(&worker_pool, data_size * 4);
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf, &worker_pool, &mut st_scratch, &mut gr_scratch,
                        )?;
                        Ok(classify_ref(&block, &mut state))
                    })();
                    if tx.send((s, r)).is_err() { break; }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        for (seq, result) in result_rx {
            merge(seq, result?);
        }
        Ok(())
    })?;

    Ok(())
}

/// Per-worker accumulation classify: workers accumulate into `S` across
/// all blobs, send `S` once at completion.
///
/// # When to use
///
/// The per-worker `S` is held for the duration of the whole scan and only
/// merged at the end. The safe usage envelope is determined by the upper
/// bound on per-worker `S` memory at the largest scale you support,
/// multiplied by the number of decode threads.
///
/// Safe: relation classify (~68 MB per worker at planet) and relation
/// closure members (~13 MB per worker). These are sparse paths where `S`
/// is dominated by a small set of relation-local IDs or metadata.
///
/// Borderline: per-worker `IdSet` accumulation of node IDs during
/// way classify (geocode Pass 1.5). A worker can legitimately touch node
/// IDs across the full planet range via referenced-node unions, so the
/// worst-case per-worker bitmap is ~1.3 GB at planet scale (10.4 B node
/// IDs × 1 bit). Shipping at 14.59 GB peak RSS (planet) - OK in practice,
/// but on the rewrite list in `notes/geocode-build-opportunities.md`.
/// If you add another caller like this, measure first.
///
/// Unsafe: per-worker `Vec<i64>` accumulation of node IDs during dense
/// node classify (would be O(billions of i64) per worker). Use
/// [`parallel_classify_phase`] instead - its per-blob merge is bounded
/// by blob size (~8 000 elements).
///
/// If you change this comment, also update the caller audit in the
/// geocode Pass 1.5 call site and the TODO item tracking it.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn parallel_classify_accumulate<S: Send>(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[ScheduleEntry],
    threads: Option<usize>,
    worker_init: impl Fn() -> S + Send + Sync,
    classify: impl Fn(&crate::PrimitiveBlock, &mut S) + Send + Sync,
    mut merge: impl FnMut(S),
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() { return Ok(()); }

    let decode_threads = resolve_thread_count(threads);

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<ScheduleEntry>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<crate::error::Result<S>>(decode_threads);

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for &item in schedule {
                if desc_tx.send(item).is_err() { break; }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(shared_file);
            let classify_ref = &classify;
            let worker_init_ref = &worker_init;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let worker_pool = crate::blob::DecompressPool::new();
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
                let mut state = worker_init_ref();

                let result: crate::error::Result<()> = (|| {
                    loop {
                        let (_s, data_offset, data_size) = {
                            let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            match guard.recv() {
                                Ok(d) => d,
                                Err(_) => return Ok(()),
                            }
                        };

                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        let mut buf = crate::blob::pool_get_pub(&worker_pool, data_size * 4);
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf, &worker_pool, &mut st_scratch, &mut gr_scratch,
                        )?;
                        classify_ref(&block, &mut state);
                    }
                })();

                match result {
                    Ok(()) => { tx.send(Ok(state)).ok(); }
                    Err(e) => { tx.send(Err(e)).ok(); }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        for result in result_rx {
            merge(result?);
        }
        Ok(())
    })?;

    Ok(())
}
