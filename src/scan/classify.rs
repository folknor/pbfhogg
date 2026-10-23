//! Parallel blob classification: pread-from-workers schedule + decode loop.
//!
//! Schedules a list of blob offsets across worker threads. Each worker
//! `pread`s a compressed blob, decompresses, and runs a caller-supplied
//! closure on the decoded `PrimitiveBlock`. The consumer thread merges
//! per-blob results.
//!
//! Four modes:
//!
//! - [`parallel_classify_phase`]: per-blob result `R` is sent to the
//!   consumer for each blob (worker holds only persistent scratch state
//!   `S`). Use for dense paths where per-worker accumulation would be
//!   unbounded at planet scale.
//!
//! - [`parallel_classify_phase_ordered`]: the same, with `merge` called in
//!   schedule order and dispatch held at most a fixed window ahead of the
//!   earliest unmerged blob, which bounds the reorder memory. Use where the
//!   merge's output must not depend on worker timing.
//!
//! - [`parallel_classify_accumulate`]: per-worker accumulator `S` is held
//!   for the whole scan and merged once at completion. Use for sparse
//!   paths where the accumulator has a known small bound.
//!
//! - [`parallel_scan_blobs_raw`]: same shape as `parallel_classify_phase`
//!   but the worker hands the caller `&[u8]` of decompressed blob bytes,
//!   skipping `PrimitiveBlock` construction. Use when the caller only
//!   needs a small wire-format subset (a couple of fields per element)
//!   and parsing the full block is wasted work.
//!
//! Schedule construction lives in [`build_classify_schedule`] and
//! [`build_classify_schedules_split`].

use crate::BoxResult as Result;

/// One entry in a classification schedule: `(seq, data_offset, data_size)`.
/// `seq` is an ordering key supplied by the schedule builder; it may be
/// sparse. `data_offset` and `data_size` address the blob's payload in the
/// input PBF.
pub(crate) type ScheduleEntry = (usize, u64, usize);

use crate::tunables::{
    DECODE_WORKERS_FALLBACK, DECODE_WORKERS_MIN, DECODE_WORKERS_RESERVED,
    DECOMPRESS_EXPANSION_GUESS, POOL_DESCRIPTOR_CHANNEL_DEPTH, POOL_RESULT_CHANNEL_DEPTH,
    WILLNEED_COALESCE_GAP_BYTES, WILLNEED_WINDOW_BYTES,
};

fn blob_body_within_file(data_offset: u64, data_size: usize, file_size: u64) -> bool {
    data_offset
        .checked_add(data_size as u64)
        .is_some_and(|end| end <= file_size)
}

/// Coalesce body ranges in a walker-derived schedule. Schedule entries retain
/// file order, so bodies separated by up to a BlobHeader-sized gap are joined.
/// A zero-sized body has no range to advise: passing length zero to
/// `posix_fadvise` would instead mean through EOF.
fn coalesce_willneed_ranges(schedule: &[ScheduleEntry]) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for &(_, offset, size) in schedule {
        let size = size as u64;
        if size == 0 {
            continue;
        }
        let Some(end) = offset.checked_add(size) else {
            continue;
        };
        if let Some((previous_offset, previous_size)) = ranges.last_mut()
            && previous_offset
                .checked_add(*previous_size)
                .and_then(|previous_end| previous_end.checked_add(WILLNEED_COALESCE_GAP_BYTES))
                .is_some_and(|end_with_gap| offset <= end_with_gap)
            && let Some(previous_end) = previous_offset.checked_add(*previous_size)
            && let Some(extension) = end.checked_sub(previous_end)
            && let Some(extended_size) = previous_size.checked_add(extension)
        {
            *previous_size = extended_size;
        } else {
            ranges.push((offset, size));
        }
    }
    ranges
}

/// Sliding WILLNEED state for a pread dispatcher. The dispatcher advises about
/// 256 MiB ahead as it hands entries to workers, rather than prefetching the
/// entire schedule before any reads begin.
pub(crate) struct WillneedPrefetch {
    ranges: Vec<(u64, u64)>,
    next_range: usize,
    next_offset: u64,
}

impl WillneedPrefetch {
    /// Prepare unconditional sliding WILLNEED prefetching for a dispatch
    /// schedule.
    pub(crate) fn new(schedule: &[ScheduleEntry]) -> Self {
        Self {
            ranges: coalesce_willneed_ranges(schedule),
            next_range: 0,
            next_offset: 0,
        }
    }

    /// Advise enough coalesced ranges to keep the window ahead of `entry`.
    pub(crate) fn before_dispatch(
        &mut self,
        shared_file: &std::sync::Arc<std::fs::File>,
        entry: ScheduleEntry,
    ) {
        let Some(window_end) = entry
            .1
            .checked_add(entry.2 as u64)
            .and_then(|end| end.checked_add(WILLNEED_WINDOW_BYTES))
        else {
            return;
        };

        while let Some(&(range_offset, range_size)) = self.ranges.get(self.next_range) {
            let Some(range_end) = range_offset.checked_add(range_size) else {
                self.next_range += 1;
                self.next_offset = 0;
                continue;
            };
            let offset = if self.next_offset == 0 {
                range_offset
            } else {
                self.next_offset
            };
            if offset >= window_end {
                break;
            }
            let end = range_end.min(window_end);
            let Some(size) = end.checked_sub(offset) else {
                self.next_range += 1;
                self.next_offset = 0;
                continue;
            };
            if end == range_end {
                self.next_range += 1;
                self.next_offset = 0;
            } else {
                self.next_offset = end;
            }

            #[cfg(target_os = "linux")]
            {
                use std::os::unix::io::AsRawFd as _;

                let (Ok(offset), Ok(size)) = (offset.try_into(), size.try_into()) else {
                    continue;
                };
                // Safety: the File owns a valid descriptor for this call. As
                // with HeaderWalker's RANDOM hint, fadvise failures are advisory.
                unsafe {
                    libc::posix_fadvise(
                        shared_file.as_ref().as_raw_fd(),
                        offset,
                        size,
                        libc::POSIX_FADV_WILLNEED,
                    );
                }
            }

            #[cfg(not(target_os = "linux"))]
            let _ = shared_file;
        }
    }
}

/// Resolve a caller-supplied `threads: Option<usize>` override into a
/// concrete decode-thread count. `None` (the default for every existing
/// caller) picks `available_parallelism() - 2` clamped to ≥ 1, the
/// convention established by the pipelined reader and matching the
/// comment in `src/read/pipeline.rs::run_pipeline`. `Some(0)` is
/// treated identically to `None` so CLI flags that map "0 = auto"
/// pass through cleanly. `Some(n)` forces exactly `n` threads.
fn resolve_thread_count(threads: Option<usize>, resources: &dyn crate::host::Resources) -> usize {
    match threads {
        Some(n) if n > 0 => n,
        _ => crate::host::sized_workers(
            resources,
            DECODE_WORKERS_RESERVED,
            DECODE_WORKERS_MIN,
            DECODE_WORKERS_FALLBACK,
        ),
    }
}

/// Classify-phase blob filter. Returns `(skip, tag_skip)`: `skip=true` means
/// the caller should skip the blob; `tag_skip=true` means the skip was driven
/// by the tag-index filter (the caller should bump its tag-skip counter).
///
/// Takes the already-parsed `BlobIndex` and raw `tagdata` bytes so callers
/// can feed it from either a `BlobHeader` (via `hdr.index()` / `hdr.tag_index()`
/// materialisation) or a `HeaderWalker` `BlobHeaderMeta` (which already
/// parses `index` up-front and holds raw `tagdata` bytes).
///
/// Lives here rather than in `tags_filter` because the tag index is a
/// schedule-building concern: `tags_count` reaches the same blobs through
/// [`build_classify_schedule_blob_filtered`] and must apply the identical rule,
/// and two copies of "when may I skip this blob" is exactly how a filter starts
/// dropping matches on one path and not the other.
pub(crate) fn blob_filter_check(
    index: Option<&crate::blob_meta::BlobIndex>,
    tagdata: Option<&[u8]>,
    filter: Option<&crate::blob_meta::BlobFilter>,
) -> (bool, bool) {
    let Some(filter) = filter else {
        return (false, false);
    };
    if let Some(idx) = index {
        let dominated = matches!(
            idx.kind,
            crate::ElementKind::Node if !filter.want_nodes
        ) || matches!(
            idx.kind,
            crate::ElementKind::Way if !filter.want_ways
        ) || matches!(
            idx.kind,
            crate::ElementKind::Relation if !filter.want_relations
        );
        if dominated {
            return (true, false);
        }
    }
    if filter.has_tag_filter()
        && let Some(bytes) = tagdata
        && let Some(tag_idx) = crate::blob_meta::TagIndex::deserialize(bytes)
        && !filter.wants_tag_index(&tag_idx)
    {
        return (true, true);
    }
    (false, false)
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
pub(crate) fn build_classify_schedule(
    input: &std::path::Path,
    kind_filter: Option<crate::ElementKind>,
    diagnostics: &crate::host::Diagnostics,
) -> Result<(Vec<ScheduleEntry>, std::sync::Arc<std::fs::File>)> {
    let (schedule, file, _skipped) =
        build_classify_schedule_blob_filtered(input, kind_filter, None, diagnostics)?;
    Ok((schedule, file))
}

/// [`build_classify_schedule`] plus an optional blob-level tag filter.
///
/// The third return value is how many blobs the tag index let us skip, for the
/// caller to report.
///
/// `blob_filter` must be derived from the *same* expressions the per-element
/// pass will apply, or blobs holding matches get dropped. `tags_filter`'s
/// `blob_filter_from_expressions` is the one producer; it deliberately clears
/// the whole key set when any expression's key matcher cannot be expressed as
/// key-set membership, because the filter is a union.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn build_classify_schedule_blob_filtered(
    input: &std::path::Path,
    kind_filter: Option<crate::ElementKind>,
    blob_filter: Option<&crate::blob_meta::BlobFilter>,
    diagnostics: &crate::host::Diagnostics,
) -> Result<(Vec<ScheduleEntry>, std::sync::Arc<std::fs::File>, u64)> {
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, diagnostics)?;
    // Consume the first blob (expected OSMHeader): on an empty file return
    // MissingHeader. `HeaderWalker::next_header` enforces header position -
    // an OSMHeader anywhere after the first frame is a hard error - but it
    // does not require the first frame to be an OSMHeader, so a file that
    // opens with an OSMData blob has that blob dropped here unread.
    // Subsequent non-OsmData blobs are filtered below.
    drop(
        walker
            .next_header()?
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))?,
    );
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_END");

    crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_START");
    let file_size = walker.file_size();
    let mut schedule: Vec<ScheduleEntry> = Vec::new();
    let mut seq: usize = 0;
    let mut blobs_skipped_by_tag: u64 = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        // A tag-key-only blob filter prunes through tagdata, so missing
        // indexdata only matters to a kind filter or an index-backed blob
        // filter (same predicate as the pipelined reader's note).
        if kind_filter.is_some()
            || blob_filter.is_some_and(crate::blob_meta::BlobFilter::uses_index)
        {
            walker.observe_filter_index(meta.index.as_ref());
        }
        if let Some(filter_kind) = kind_filter
            && let Some(idx) = &meta.index
            && idx.kind != filter_kind
        {
            continue;
        }
        let (skip, tag_skip) =
            blob_filter_check(meta.index.as_ref(), meta.tagdata.as_deref(), blob_filter);
        if skip {
            if tag_skip {
                blobs_skipped_by_tag += 1;
            }
            continue;
        }
        // Reject bogus blob-header data_size before it flows to pread
        // workers: a truncated file or a corrupt BlobHeader advertising
        // a data_size that extends past EOF would otherwise fail much
        // later in `read_exact_at` with an opaque UnexpectedEof, after
        // workers have already accepted the schedule. Cheap integer
        // check here catches the whole family.
        if !blob_body_within_file(meta.data_offset, meta.data_size, file_size) {
            return Err(format!(
                "blob at offset {} claims data_size {} but file is only {} bytes",
                meta.data_offset, meta.data_size, file_size,
            )
            .into());
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
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("schedule_tagidx_skipped_blobs", blobs_skipped_by_tag as i64);
    Ok((schedule, shared_file, blobs_skipped_by_tag))
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
///
/// The fifth return value is the highest `max_id` indexdata reports per kind,
/// indexed `[node, way, relation]`, or 0 for a kind whose blobs carry no
/// indexdata. It is a *sizing hint only* - indexdata is not verified anywhere,
/// so a consumer that allocates from it must still cope with an ID that
/// exceeds it (see `verify_ids`, which reports such an ID rather than
/// panicking). It exists so allocations track the file instead of a hard-coded
/// ceiling that the passage of time invalidates.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::type_complexity)]
pub(crate) fn build_classify_schedules_split(
    input: &std::path::Path,
    diagnostics: &crate::host::Diagnostics,
) -> Result<(
    Vec<ScheduleEntry>,
    Vec<ScheduleEntry>,
    Vec<ScheduleEntry>,
    std::sync::Arc<std::fs::File>,
    [i64; 3],
)> {
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, diagnostics)?;
    drop(
        walker
            .next_header()?
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))?,
    );
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_END");

    crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_START");
    let file_size = walker.file_size();
    let mut nodes: Vec<ScheduleEntry> = Vec::new();
    let mut ways: Vec<ScheduleEntry> = Vec::new();
    let mut rels: Vec<ScheduleEntry> = Vec::new();
    let mut max_ids: [i64; 3] = [0; 3];
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        // See `build_classify_schedule` for the rationale: reject
        // past-EOF blob bodies before the schedule is handed to pread
        // workers. Replicated across both schedule builders since they
        // share the same downstream consumer pattern.
        if !blob_body_within_file(meta.data_offset, meta.data_size, file_size) {
            return Err(format!(
                "blob at offset {} claims data_size {} but file is only {} bytes",
                meta.data_offset, meta.data_size, file_size,
            )
            .into());
        }
        let reported_max = meta.index.as_ref().map_or(0, |i| i.max_id);
        match meta.index.as_ref().map(|i| i.kind) {
            Some(crate::ElementKind::Node) => {
                max_ids[0] = max_ids[0].max(reported_max);
                nodes.push((nodes.len(), meta.data_offset, meta.data_size));
            }
            Some(crate::ElementKind::Way) => {
                max_ids[1] = max_ids[1].max(reported_max);
                ways.push((ways.len(), meta.data_offset, meta.data_size));
            }
            Some(crate::ElementKind::Relation) => {
                max_ids[2] = max_ids[2].max(reported_max);
                rels.push((rels.len(), meta.data_offset, meta.data_size));
            }
            None => {
                walker.observe_filter_index(None);
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
    Ok((nodes, ways, rels, shared_file, max_ids))
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
/// bounded by the contents of one input blob. `S` persists across blobs for
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
    resources: &dyn crate::host::Resources,
) -> Result<()> {
    parallel_classify_phase_abortable(
        shared_file,
        schedule,
        threads,
        worker_init,
        classify,
        |seq, r| {
            merge(seq, r);
            std::ops::ControlFlow::Continue(())
        },
        resources,
    )
}

/// [`parallel_classify_phase`] whose merge closure can stop the scan.
///
/// The merge runs on the calling thread and cannot return a `Result`, so
/// callers that write output from it capture the first write error in a local
/// and check it once the phase returns. Capturing alone does not *stop*
/// anything: the schedule still ran to completion, so a planet-scale run whose
/// very first write failed paid the entire decode before surfacing the error.
/// Returning `Break` drops the result receiver, which fails the workers' sends
/// and then the dispatcher's, unwinding the scan promptly.
///
/// Aborting is not cancellation: blobs already in flight still finish, and the
/// caller remains responsible for reporting why it stopped.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn parallel_classify_phase_abortable<S: Send, R: Send>(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[ScheduleEntry],
    threads: Option<usize>,
    worker_init: impl Fn() -> S + Send + Sync,
    classify: impl Fn(&crate::PrimitiveBlock, &mut S) -> R + Send + Sync,
    mut merge: impl FnMut(usize, R) -> std::ops::ControlFlow<()>,
    resources: &dyn crate::host::Resources,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    let mut willneed = WillneedPrefetch::new(schedule);
    if schedule.is_empty() {
        return Ok(());
    }

    let decode_threads = resolve_thread_count(threads, resources);

    let (desc_tx, desc_rx) =
        std::sync::mpsc::sync_channel::<ScheduleEntry>(POOL_DESCRIPTOR_CHANNEL_DEPTH);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<(usize, crate::error::Result<R>)>(
        POOL_RESULT_CHANNEL_DEPTH,
    );

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for &item in schedule {
                willneed.before_dispatch(shared_file, item);
                if desc_tx.send(item).is_err() {
                    break;
                }
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
                        let mut buf = crate::blob::pool_get(
                            Some(&worker_pool),
                            data_size * DECOMPRESS_EXPANSION_GUESS,
                        );
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf,
                            &worker_pool,
                            &mut st_scratch,
                            &mut gr_scratch,
                        )?;
                        Ok(classify_ref(&block, &mut state))
                    })();
                    if tx.send((s, r)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        for (seq, result) in result_rx {
            if merge(seq, result?).is_break() {
                // Dropping the receiver - the `for` loop owns it, and it falls
                // out of scope right after this statement - fails the workers'
                // `tx.send`, breaking their loops, which in turn fails the
                // dispatcher's `desc_tx.send`.
                break;
            }
        }
        Ok(())
    })?;

    Ok(())
}

/// Admission state of [`parallel_classify_phase_ordered`]: how many results
/// the consumer has merged, and whether the scan is shutting down.
type OrderedCredit = (std::sync::Mutex<(usize, bool)>, std::sync::Condvar);

/// Marks the ordered scan closed on drop, waking a dispatcher parked on
/// admission credit. The consumer holds one for its whole loop (an early
/// `Err` return or a panicking merge must not leave the dispatcher parked,
/// or the scope never joins); each worker holds one that fires only when the
/// worker panics, because the result it was producing will never arrive and
/// the window could otherwise never advance past it.
struct CloseOnDrop<'a> {
    credit: &'a OrderedCredit,
    only_if_panicking: bool,
}

/// Park until schedule position `position` is admitted (`position < merged +
/// window`). Returns `false` when the scan closed instead.
fn wait_for_credit(credit: &OrderedCredit, position: usize, window: usize) -> bool {
    let (lock, cvar) = credit;
    let mut state = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while !state.1 && position >= state.0.saturating_add(window) {
        state = cvar
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    !state.1
}

/// Publish the consumer's merged count and wake the dispatcher.
fn publish_merged(credit: &OrderedCredit, merged: usize) {
    let (lock, cvar) = credit;
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .0 = merged;
    cvar.notify_one();
}

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        if self.only_if_panicking && !std::thread::panicking() {
            return;
        }
        let (lock, cvar) = self.credit;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1 = true;
        cvar.notify_all();
    }
}

/// [`parallel_classify_phase`] whose `merge` runs in schedule order, with
/// memory bounded by `window`.
///
/// Results still complete in worker order; the consumer parks each in a
/// `window`-slot ring and merges the contiguous run from the earliest
/// unmerged position. Ordering alone would not bound that ring - one slow
/// blob at the head lets workers race arbitrarily far ahead - so dispatch
/// carries an admission credit tied to the head: schedule position `k` is
/// handed to a worker only once `k < merged + window`. At most `window`
/// results are therefore alive between classify and merge, and workers only
/// stall when the head blob is a full window behind the fastest one.
///
/// Unlike running fixed batches back to back, merge overlaps classification
/// and the workers persist (with their `S` scratch and decompress pools)
/// across the whole schedule; a batch barrier would idle every worker on the
/// slowest blob of each batch and serialize the merge behind the workers,
/// which matters on merge-heavy phases.
///
/// `merge` receives the entry's own `seq`, which may be sparse; ordering is
/// by schedule position.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn parallel_classify_phase_ordered<S: Send, R: Send>(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[ScheduleEntry],
    threads: Option<usize>,
    window: usize,
    worker_init: impl Fn() -> S + Send + Sync,
    classify: impl Fn(&crate::PrimitiveBlock, &mut S) -> R + Send + Sync,
    mut merge: impl FnMut(usize, R),
    resources: &dyn crate::host::Resources,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    let mut willneed = WillneedPrefetch::new(schedule);
    if schedule.is_empty() {
        return Ok(());
    }
    let window = window.max(1);
    let decode_threads = resolve_thread_count(threads, resources);

    let credit: OrderedCredit = (std::sync::Mutex::new((0, false)), std::sync::Condvar::new());
    let (desc_tx, desc_rx) =
        std::sync::mpsc::sync_channel::<(usize, ScheduleEntry)>(POOL_DESCRIPTOR_CHANNEL_DEPTH);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) =
        std::sync::mpsc::sync_channel::<(usize, usize, crate::error::Result<R>)>(
            POOL_RESULT_CHANNEL_DEPTH,
        );

    std::thread::scope(|scope| -> Result<()> {
        let credit_ref = &credit;
        scope.spawn(move || {
            for (position, &item) in schedule.iter().enumerate() {
                if !wait_for_credit(credit_ref, position, window) {
                    break;
                }
                willneed.before_dispatch(shared_file, item);
                if desc_tx.send((position, item)).is_err() {
                    break;
                }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(shared_file);
            let classify_ref = &classify;
            let worker_init_ref = &worker_init;
            scope.spawn(move || {
                let _close = CloseOnDrop {
                    credit: credit_ref,
                    only_if_panicking: true,
                };
                let mut read_buf: Vec<u8> = Vec::new();
                let worker_pool = crate::blob::DecompressPool::new();
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
                let mut state = worker_init_ref();

                loop {
                    let (position, (s, data_offset, data_size)) = {
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
                        let mut buf = crate::blob::pool_get(
                            Some(&worker_pool),
                            data_size * DECOMPRESS_EXPANSION_GUESS,
                        );
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf,
                            &worker_pool,
                            &mut st_scratch,
                            &mut gr_scratch,
                        )?;
                        Ok(classify_ref(&block, &mut state))
                    })();
                    if tx.send((position, s, r)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        let _close = CloseOnDrop {
            credit: credit_ref,
            only_if_panicking: false,
        };
        // Position `k` is admitted only while `k < next + window`, so the
        // live positions are distinct modulo the ring length.
        let ring = window.min(schedule.len());
        let mut pending: Vec<Option<(usize, R)>> = (0..ring).map(|_| None).collect();
        let mut next = 0_usize;
        for (position, seq, result) in result_rx {
            pending[position % ring] = Some((seq, result?));
            let before = next;
            while let Some((seq, out)) = pending[next % ring].take() {
                merge(seq, out);
                next += 1;
            }
            if next != before {
                publish_merged(credit_ref, next);
            }
        }
        Ok(())
    })?;

    Ok(())
}

/// Per-blob streaming scan over decompressed blob bytes (no `PrimitiveBlock`
/// construction).
///
/// Same shape as [`parallel_classify_phase`] but the worker hands the
/// caller `&[u8]` of decompressed blob bytes instead of a parsed
/// `PrimitiveBlock`. Callers are expected to walk the wire format
/// themselves via the helpers in `crate::scan::way`, `crate::scan::node`,
/// `crate::scan::relation`, etc.
///
/// Use when the caller only needs a small subset of the blob's fields
/// and parsing the full block (StringTable, group_ranges, dense_nodes
/// columns) is wasted work. ALTW pass 0 (way-ref node-id collection)
/// is the canonical caller: it iterates only `Way.refs()` and never
/// touches strings or info, so going through `PrimitiveBlock` was
/// dropping per-blob StringTable parse + `(u32, u32)` scratch
/// allocations onto the hot path for nothing.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn parallel_scan_blobs_raw<S: Send, R: Send>(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[ScheduleEntry],
    threads: Option<usize>,
    worker_init: impl Fn() -> S + Send + Sync,
    classify: impl Fn(&[u8], &mut S) -> crate::error::Result<R> + Send + Sync,
    mut merge: impl FnMut(usize, R),
    resources: &dyn crate::host::Resources,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    let mut willneed = WillneedPrefetch::new(schedule);
    if schedule.is_empty() {
        return Ok(());
    }

    let decode_threads = resolve_thread_count(threads, resources);

    let (desc_tx, desc_rx) =
        std::sync::mpsc::sync_channel::<ScheduleEntry>(POOL_DESCRIPTOR_CHANNEL_DEPTH);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<(usize, crate::error::Result<R>)>(
        POOL_RESULT_CHANNEL_DEPTH,
    );

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for &item in schedule {
                willneed.before_dispatch(shared_file, item);
                if desc_tx.send(item).is_err() {
                    break;
                }
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
                let mut decompress_buf: Vec<u8> = Vec::new();
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
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;
                        classify_ref(&decompress_buf, &mut state)
                    })();
                    if tx.send((s, r)).is_err() {
                        break;
                    }
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
/// closure members (~13 MB per worker, e.g.
/// `tags_filter::collect_relation_member_closure`). These are sparse paths
/// where `S` is dominated by a small set of relation-local IDs or metadata.
///
/// Borderline: per-worker `IdSet` accumulation of node IDs during
/// way classify (geocode Pass 1.5). A worker can legitimately touch node
/// IDs across the full planet range via referenced-node unions, so the
/// worst-case per-worker bitmap is ~1.3 GB at planet scale (10.4 B node
/// IDs × 1 bit). Shipping at 14.59 GB peak RSS (planet) - OK in practice,
/// but a candidate for the same [`parallel_classify_phase`] migration
/// described below. If you add another caller like this, measure first.
///
/// **A previous caller in this category was migrated to
/// [`parallel_classify_phase`] on 2026-04-28** (commit `17b116c`,
/// `tags_filter::collect_way_node_dependencies`). Per-worker IdSet was the
/// 24 GB anon RSS peak at planet `--invert-match w/highway=primary`
/// (UUID `9044c456`); after the migration to per-blob `Vec<i64>` of node
/// refs through the 32-slot result channel, the same phase peaks at
/// ~7 GB total (UUID `7e74981a`). Mechanism: per-worker bitmap × N
/// workers is bounded by ID space × N, not by element count, so a few
/// dozen workers each with a planet-wide bitmap saturates RSS. Per-blob
/// `Vec<i64>` size follows the refs in that input blob, and the bounded result
/// channel caps the queue depth.
///
/// Unsafe: per-worker `Vec<i64>` accumulation of node IDs during dense
/// node classify (would be O(billions of i64) per worker). Use
/// [`parallel_classify_phase`] instead - its per-blob merge is bounded
/// by the contents of one input blob.
///
/// # When you can't migrate to `parallel_classify_phase`
///
/// `parallel_classify_phase`'s `classify` closure is `Fn + Sync`
/// (immutable borrow of captures) and the `merge` closure is `FnMut`
/// (mutable borrow). They run concurrently - classify on workers,
/// merge on the consumer thread. If your classify needs `&X` while
/// merge needs `&mut X` for the same `X`, the borrow checker correctly
/// rejects the closure pair. This is exactly the case in
/// `tags_filter::collect_relation_member_closure`'s convergence loop:
/// classify reads `&included_relation_ids`, merge calls
/// `included_relation_ids.set_if_new(id)`. Stay on
/// `parallel_classify_accumulate` for those - the merge runs after
/// the parallel scope, so there's no read/write conflict, and
/// per-worker `Vec<i64>` member-list state is bounded by member counts
/// (relation closure: ~tens of MB / worker at planet) rather than by
/// ID space.
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
    resources: &dyn crate::host::Resources,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    let mut willneed = WillneedPrefetch::new(schedule);
    if schedule.is_empty() {
        return Ok(());
    }

    let decode_threads = resolve_thread_count(threads, resources);

    let (desc_tx, desc_rx) =
        std::sync::mpsc::sync_channel::<ScheduleEntry>(POOL_DESCRIPTOR_CHANNEL_DEPTH);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) =
        std::sync::mpsc::sync_channel::<crate::error::Result<S>>(decode_threads);

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for &item in schedule {
                willneed.before_dispatch(shared_file, item);
                if desc_tx.send(item).is_err() {
                    break;
                }
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
                            let guard =
                                rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            match guard.recv() {
                                Ok(d) => d,
                                Err(_) => return Ok(()),
                            }
                        };

                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        let mut buf = crate::blob::pool_get(
                            Some(&worker_pool),
                            data_size * DECOMPRESS_EXPANSION_GUESS,
                        );
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf,
                            &worker_pool,
                            &mut st_scratch,
                            &mut gr_scratch,
                        )?;
                        classify_ref(&block, &mut state);
                    }
                })();

                match result {
                    Ok(()) => {
                        tx.send(Ok(state)).ok();
                    }
                    Err(e) => {
                        tx.send(Err(e)).ok();
                    }
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{ScheduleEntry, WillneedPrefetch, blob_body_within_file, coalesce_willneed_ranges};

    /// Write a PBF with `blobs` single-node OSMData blobs and return its path.
    fn write_many_blob_pbf(dir: &std::path::Path, blobs: usize) -> std::path::PathBuf {
        use crate::block_builder::{BlockBuilder, HeaderBuilder};
        use crate::writer::{Compression, PbfWriter};

        let path = dir.join("many.osm.pbf");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::None,
        );
        writer
            .write_header(&HeaderBuilder::new().build().unwrap())
            .unwrap();
        for i in 0..blobs {
            let mut block = BlockBuilder::new();
            block.add_node(i64::try_from(i).unwrap() + 1, 0, 0, [("k", "v")], None);
            writer
                .write_primitive_block(block.take().unwrap().unwrap())
                .unwrap();
        }
        writer.flush().unwrap();
        path
    }

    /// A merge closure returning `Break` must stop the scan, not merely stop
    /// merging.
    ///
    /// `getparents` and `repack` capture the first write error in a local
    /// because the merge cannot return a `Result`. Capturing alone left the
    /// phase running the whole schedule: at planet scale a failed first write
    /// cost the entire decode before the error surfaced. This pins that
    /// `Break` actually unwinds the dispatcher and the workers.
    #[test]
    fn a_breaking_merge_stops_the_scan_rather_than_draining_it() {
        let dir = crate::path_guard::tests::scratch();
        const BLOBS: usize = 400;
        let path = write_many_blob_pbf(dir.path(), BLOBS);

        let (schedule, shared_file) =
            super::build_classify_schedule(&path, None, &crate::host::Diagnostics::discard())
                .unwrap();
        assert_eq!(
            schedule.len(),
            BLOBS,
            "the fixture must actually have many blobs, or 'stopped early' is \
             indistinguishable from 'finished'"
        );

        let classified = std::sync::atomic::AtomicUsize::new(0);
        let mut merged = 0_usize;
        super::parallel_classify_phase_abortable(
            &shared_file,
            &schedule,
            Some(2),
            || (),
            |_block, ()| {
                classified.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
            |_seq, ()| {
                merged += 1;
                std::ops::ControlFlow::Break(())
            },
            &crate::host::MachineProbe,
        )
        .unwrap();

        assert_eq!(merged, 1, "the merge broke on its first call");
        // The descriptor and result channels are bounded (16 and 32), so at
        // most a few dozen blobs can be in flight when the receiver drops.
        // Half the schedule is a wide margin that still fails decisively
        // against the old behaviour, which classified all 400.
        let classified = classified.into_inner();
        assert!(
            classified < BLOBS / 2,
            "breaking must unwind the scan; {classified} of {BLOBS} blobs were \
             decoded after the merge asked to stop"
        );

        // Paired counterpart: a merge that never breaks must still see every
        // blob, so the assertion above is about `Break` and not about the
        // phase having become lossy.
        let all = std::sync::atomic::AtomicUsize::new(0);
        super::parallel_classify_phase(
            &shared_file,
            &schedule,
            Some(2),
            || (),
            |_block, ()| {},
            |_seq, ()| {
                all.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
            &crate::host::MachineProbe,
        )
        .unwrap();
        assert_eq!(all.into_inner(), BLOBS, "a non-breaking scan is exhaustive");
    }

    /// The ordered phase merges in schedule order whatever the completion
    /// order, passes each entry's own (here sparse) `seq` through, and never
    /// lets classification run more than `window` blobs past the earliest
    /// unmerged one.
    #[test]
    fn ordered_phase_merges_in_schedule_order_within_its_window() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = crate::path_guard::tests::scratch();
        const BLOBS: usize = 40;
        const WINDOW: usize = 4;
        let path = write_many_blob_pbf(dir.path(), BLOBS);
        let (mut schedule, shared_file) =
            super::build_classify_schedule(&path, None, &crate::host::Diagnostics::discard())
                .unwrap();
        assert_eq!(schedule.len(), BLOBS);
        for entry in &mut schedule {
            entry.0 *= 2;
        }

        let classified = AtomicUsize::new(0);
        let mut merged = Vec::new();
        super::parallel_classify_phase_ordered(
            &shared_file,
            &schedule,
            Some(3),
            WINDOW,
            || (),
            |_block, ()| {
                classified.fetch_add(1, Ordering::Relaxed);
            },
            |seq, ()| {
                // Every classified blob was admitted while fewer than
                // `merged.len() + WINDOW` positions were open.
                let seen = classified.load(Ordering::Relaxed);
                assert!(
                    seen <= merged.len() + WINDOW,
                    "{seen} blobs classified with {} merged and a window of {WINDOW}",
                    merged.len()
                );
                merged.push(seq);
            },
            &crate::host::MachineProbe,
        )
        .unwrap();
        assert_eq!(merged, (0..BLOBS).map(|n| n * 2).collect::<Vec<_>>());
    }

    #[test]
    fn willneed_prefetch_defaults_to_coalesced_schedule() {
        let schedule: [ScheduleEntry; 2] = [(0, 100, 20), (1, 144, 5)];

        let prefetch = WillneedPrefetch::new(&schedule);

        assert_eq!(prefetch.ranges, vec![(100, 49)]);
        assert_eq!(prefetch.next_range, 0);
        assert_eq!(prefetch.next_offset, 0);
    }

    #[test]
    fn willneed_ranges_coalesce_abutting_body_ranges() {
        let schedule: [ScheduleEntry; 3] = [(0, 100, 20), (1, 120, 30), (2, 5_000, 5)];

        assert_eq!(
            coalesce_willneed_ranges(&schedule),
            vec![(100, 50), (5_000, 5)]
        );
    }

    #[test]
    fn willneed_ranges_coalesce_realistic_blob_header_gaps() {
        let schedule: [ScheduleEntry; 4] = [
            (0, 100, 20),
            // Real PBF payloads have a four-byte header length plus the
            // BlobHeader between bodies. These represent 24- and 4096-byte
            // gaps, both intentionally bridged by the prefetch range.
            (1, 144, 5),
            (2, 4_245, 10),
            (3, 8_352, 10),
        ];

        assert_eq!(
            coalesce_willneed_ranges(&schedule),
            vec![(100, 4_155), (8_352, 10)]
        );
    }

    /// The unchecked form was `data_offset + data_size as u64 > file_size`,
    /// which wraps in release for a corrupt header advertising a huge size -
    /// so the check that exists specifically to catch corrupt blobs passed.
    #[test]
    fn corrupt_blob_body_range_cannot_wrap_past_eof() {
        assert!(!blob_body_within_file(u64::MAX - 4, 8, u64::MAX));
        assert!(!blob_body_within_file(0, usize::MAX, 100));
        assert!(blob_body_within_file(92, 8, 100));
        assert!(!blob_body_within_file(93, 8, 100));
    }
}
