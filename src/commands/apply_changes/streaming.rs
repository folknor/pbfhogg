//! Worker pool for the descriptor-first streaming pipeline.
//!
//! Spawns `worker_count` long-lived threads that pull [`ScannedBlob`]
//! candidates from the scanner over `candidate_rx` and emit
//! [`DrainItem`]s to the drain over `drain_tx`. Each worker owns a
//! thread-local [`BlockBuilder`], decompress and parse scratch buffers,
//! and (under `--locations-on-ways`) a private `FxHashMap` accumulator
//! published into a per-worker `Arc<Mutex<_>>` slot for the drain to
//! merge at the node→way barrier.
//!
//! ## Per-blob worker logic
//!
//! - [`ScannedBlob::Candidate`]: pread the Blob protobuf body via
//!   `(frame_start + blob_offset, data_size)`, decompress through the
//!   thread-local scratch, extract node coords (checked, dense and plain
//!   encodings, see `node_coords.rs`) if the blob is a Node and
//!   `--locations-on-ways` is active, parse via
//!   [`PrimitiveBlock::from_vec_with_scratch`], precise-check via
//!   [`block_overlaps_diff`]. False positives emit a `DrainItem::CopyRange`
//!   carrying the original descriptor so the drain can `copy_file_range`
//!   the raw frame. An unindexed blob (`--force`) is scanned for the
//!   sidecars the writer would give it, and a false positive among those
//!   is reframed with them (`DrainItem::OwnedBytes`), so the output is
//!   wholly indexed; one no index can describe is re-encoded, omitted when
//!   it holds no element, or refused when it mixes kinds. True overlaps compute
//!   the inline-upsert range from the descriptor's `id_range` against the
//!   diff's sorted upsert vector and call [`rewrite_block_parallel`] on the
//!   worker's persistent `BlockBuilder`, emitting
//!   `DrainItem::Rewritten`.
//! - [`ScannedBlob::Passthrough`]: only routed through workers under
//!   `--direct-io` output (where the drain can't `copy_file_range`).
//!   Workers pread the **full framed bytes** via
//!   `(frame_start, frame_len)` and emit
//!   `DrainItem::OwnedBytes`.
//!
//! ## Failure
//!
//! Every error the pool returns - setup or per-blob - is published into
//! the shared [`FailureSlot`] while the failing thread still holds its
//! drain sender. The drain returns it as its own error, which unblocks the
//! scanner and the surviving workers. Per-blob errors carry the blob's
//! seq and frame offset.

use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ElementKind;
use crate::blob::decompress_blob_raw;
use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::error::{Error, Result};
use crate::writer::{Compression, frame_blob_pipelined, reframe_raw_with_index};

use super::classify::block_overlaps_diff;
use super::descriptor::{BlobDescriptor, DrainItem, DrainMessage, Route, ScannedBlob};
use super::diff_ranges::DiffRanges;
use super::drain::LocMapHandle;
use super::failure::FailureSlot;
use super::loc_mode::LocMode;
use super::node_coords::extract_node_coords;
use super::rewrite_block::rewrite_block_parallel;

use crate::osc::CompactDiffOverlay;

/// One worker's shared coord accumulator. Workers hold their own; the
/// drain holds clones of every slot.
pub(super) type CoordSlot = Arc<Mutex<FxHashMap<i64, (i32, i32)>>>;

/// Per-worker shared coord accumulator slots. The drain holds clones of
/// every slot; each worker writes only into its own. At the node→way
/// barrier the drain acquires every mutex, drains the maps, merges into
/// `Arc<loc_map>`, and signals the scanner.
pub(super) type CoordSlots = Vec<CoordSlot>;

/// Streaming pool configuration. Channels are passed separately so this
/// struct can be `Clone` cheaply across worker startup.
pub(super) struct StreamingConfig {
    /// The run's pinned base; the pool opens one description of it.
    pub base: crate::read::input::PinnedInput,
    pub ranges: Arc<DiffRanges>,
    pub diff: Arc<CompactDiffOverlay>,
    /// Number of worker threads. Should be `nproc - 2` (one each for the
    /// scanner and drain).
    pub worker_count: usize,
    /// `--locations-on-ways` mode. Workers extract node coords during
    /// the node phase only if true.
    pub locations_on_ways: bool,
    /// Sized to `worker_count`. Each worker writes into `slots[worker_id]`;
    /// the drain merges all slots at the node→way barrier. `None` when
    /// `locations_on_ways` is false.
    pub coord_slots: Option<CoordSlots>,
    /// Drain → workers post-barrier `loc_map`. `None` when
    /// `locations_on_ways` is false. Workers reading way/relation
    /// blobs check `get()` on this; the scanner barrier protocol
    /// guarantees the lock is set before any way descriptor is
    /// dispatched.
    pub loc_map_handle: Option<LocMapHandle>,
    /// Set of node IDs the workers need to extract coords for. Workers
    /// filter `extract_node_coords` output against this so we don't
    /// blow per-worker RSS by capturing every coord in the file.
    /// `None` when `locations_on_ways` is false.
    pub needed_set: Option<Arc<FxHashSet<i64>>>,
    /// Output compression. Workers frame each rewrite output via
    /// `frame_blob_pipelined` so the drain side avoids the writer's
    /// `rayon::spawn`-per-block dispatch (P1.5).
    pub compression: Compression,
    /// `Splice` when the output backend supports `copy_file_range` (so the
    /// drain accepts `DrainItem::CopyRange`). On `Owned` (when
    /// `--direct-io` is selected), false-positive candidates must be
    /// routed through the owned-passthrough path instead - the drain
    /// rejects `CopyRange` items up front.
    pub route: Route<()>,
    /// Shared with the drain; see the module doc's "Failure" section.
    pub failure: Arc<FailureSlot>,
    /// Test-only: panic in the worker loop when the dequeued blob's
    /// `seq` matches: the run's fault plan's
    /// `apply_changes_panic_at_blob_seq`, read once by the caller. Only
    /// compiled under the `test-hooks` feature.
    #[cfg(feature = "test-hooks")]
    pub panic_at_blob_seq: Option<u64>,
}

/// Channels owned by the streaming pool.
pub(super) struct StreamingChannels {
    /// Candidate descriptors from the scanner. Closed when the scanner
    /// finishes (which signals workers to drain and exit).
    pub candidate_rx: mpsc::Receiver<ScannedBlob>,
    /// Drain inputs. Workers send `DrainItem`s directly so the drain has
    /// one unified input stream (the
    /// scanner's fast-path `DrainItem::CopyRange` and worker outputs
    /// merge into the same unbounded reorder buffer keyed by global
    /// seq).
    pub drain_tx: mpsc::SyncSender<DrainMessage>,
}

crate::counters::counter_set! {
    /// Worker-pool counters, summed across workers and surfaced as sidecar
    /// counters when the pool finishes. A separate set (and allocation) from
    /// the drain thread's `DrainCounters`.
    pub(super) struct WorkerCounters {
        pub blobs_processed => "merge_streaming_blobs_processed",
        pub blobs_rewritten => "merge_streaming_blobs_rewritten",
        pub blobs_false_positive => "merge_streaming_blobs_false_positive",
        /// Candidates whose base elements the diff does not touch but whose ID
        /// span holds inline upserts (pure creates), so they are rewritten
        /// rather than passed through to keep the output sorted, plus
        /// unindexed blobs no single index describes, re-encoded so the
        /// output is indexed. A subset of `blobs_rewritten`.
        pub blobs_forced_rewrite => "merge_streaming_blobs_forced_rewrite",
        pub blobs_owned_passthrough => "merge_streaming_blobs_owned_passthrough",
        /// Decompress wall summed across all workers (cumulative ns).
        pub decompress_ns => "merge_streaming_decompress_ns",
        /// Parse wall summed across all workers.
        pub parse_ns => "merge_streaming_parse_ns",
        /// Precise-check wall summed across all workers.
        pub precise_ns => "merge_streaming_precise_ns",
        /// Rewrite wall summed across all workers.
        pub rewrite_ns => "merge_streaming_rewrite_ns",
        /// Coord-extraction wall summed across all workers (LOW only).
        pub coord_extract_ns => "merge_streaming_coord_extract_ns",
        /// Node coord pairs extracted across all workers (LOW only).
        pub coord_pairs_extracted => "merge_streaming_coord_pairs_extracted",
        /// Node blobs decompressed for coordinate extraction (LOW only).
        /// Bumped once per blob on the extraction branch, unlike
        /// `blobs_processed`, which counts every candidate and owned
        /// passthrough of every kind. Not emitted by this set: `merge`
        /// reads it into the stats it emits as `merge_loc_node_blobs_scanned`.
        pub loc_node_blobs_scanned,
        /// In-line framing wall summed across all workers (P1.5: avoids
        /// the writer's rayon-spawn-per-block dispatch).
        pub frame_ns => "merge_streaming_frame_ns",
    }
}

/// Run the worker pool to completion. Returns once the candidate channel
/// is closed and every worker has exited.
/// Called by `rewrite::merge` after the scanner and drain channels are ready.
///
/// Errors are not returned: every one, setup or per-blob, is published to
/// `cfg.failure` while the failing thread still holds a drain sender (the
/// drain's post-disconnect check relies on that ordering), and the drain
/// returns it. Worker panics are caught and published before sender release.
///
/// The `drain_tx` clone owned by this function is dropped on return;
/// downstream drain sees `Disconnected` once every worker's clone is
/// also dropped.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn run_workers(
    cfg: StreamingConfig,
    channels: StreamingChannels,
    counters: &WorkerCounters,
) {
    let StreamingConfig {
        base,
        ranges,
        diff,
        worker_count,
        locations_on_ways,
        coord_slots,
        loc_map_handle,
        needed_set,
        compression,
        route,
        failure,
        #[cfg(feature = "test-hooks")]
        panic_at_blob_seq,
    } = cfg;
    let StreamingChannels {
        candidate_rx,
        drain_tx,
    } = channels;

    // Setup failures publish while `drain_tx` is still bound here; it is
    // dropped by the return that follows.
    if locations_on_ways {
        let slots_len = coord_slots.as_ref().map_or(0, Vec::len);
        if slots_len != worker_count {
            failure.publish(Error::internal(format!(
                "streaming: locations_on_ways requires coord_slots.len()={worker_count}, got {slots_len}",
            )));
            return;
        }
    }

    crate::debug::emit_marker("MERGE_STREAMING_START");

    // One plain description of the pinned base for the whole pool, kept
    // separate from the scanner walker's FADV_RANDOM one and the drain's
    // splice source (advice and readahead state are per description; see
    // the copy-input comment in `rewrite::merge_to`, which also says what
    // is unmeasured about it).
    let shared_file = match crate::read::input::InputSource::from(&base).open() {
        Ok(file) => file,
        Err(e) => {
            failure.publish(e);
            return;
        }
    };

    // mpsc::Receiver isn't Sync, so workers serialize their `recv()` calls
    // through this Mutex. The guard spans a blocking `recv()`, so an idle
    // worker can hold it for as long as the channel stays empty (the whole
    // scanner barrier wait, under `--locations-on-ways`) while the others
    // queue behind it; it serializes only dequeueing, and every blob's work
    // runs after the guard is released.
    let candidate_rx = Mutex::new(candidate_rx);
    // Worker errors are located against the base PBF; captured once, not per
    // blob.
    let source: Arc<Path> = Arc::from(base.path());

    std::thread::scope(|scope| {
        for worker_id in 0..worker_count {
            let drain_tx = drain_tx.clone();
            let coord_slot = coord_slots.as_ref().map(|s| Arc::clone(&s[worker_id]));
            let loc_map_handle = loc_map_handle.as_ref().map(Arc::clone);
            let needed_set = needed_set.as_ref().map(Arc::clone);
            let file = &shared_file;
            let source = &source;
            let candidate_rx = &candidate_rx;
            let failure = &*failure;
            let ranges = &*ranges;
            let diff = &*diff;

            scope.spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_loop(
                        worker_id,
                        file,
                        source,
                        candidate_rx,
                        &drain_tx,
                        counters,
                        ranges,
                        diff,
                        locations_on_ways,
                        coord_slot.as_ref(),
                        loc_map_handle.as_ref(),
                        needed_set.as_deref(),
                        &compression,
                        route,
                        #[cfg(feature = "test-hooks")]
                        panic_at_blob_seq,
                    )
                }));
                // Publish before this closure returns: `drain_tx` (moved in
                // above) is only dropped after it.
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => failure.publish(e),
                    Err(payload) => failure.publish_panic("streaming worker", &*payload),
                }
                drop(drain_tx);
            });
        }
    });

    // Drop our SyncSender clone explicitly so the drain sees end-of-input
    // once every worker's clone is also dropped.
    drop(drain_tx);

    crate::debug::emit_marker("MERGE_STREAMING_END");
    counters.emit_all();
}

/// Per-worker hot loop. Pulls candidates until the channel closes; for
/// each candidate dispatches to the appropriate handler.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn worker_loop(
    _worker_id: usize,
    file: &std::fs::File,
    source: &Arc<Path>,
    candidate_rx: &Mutex<mpsc::Receiver<ScannedBlob>>,
    drain_tx: &mpsc::SyncSender<DrainMessage>,
    counters: &WorkerCounters,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    locations_on_ways: bool,
    coord_slot: Option<&CoordSlot>,
    loc_map_handle: Option<&LocMapHandle>,
    needed_set: Option<&FxHashSet<i64>>,
    compression: &Compression,
    route: Route<()>,
    #[cfg(feature = "test-hooks")] panic_at_blob_seq: Option<u64>,
) -> Result<()> {
    let mut read_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
    let mut bb = BlockBuilder::new();

    // Per-worker coord accumulator: gathered locally then flushed to the
    // shared slot in batches to amortise the mutex acquire. Empty when
    // `locations_on_ways` is false.
    let mut local_coords: FxHashMap<i64, (i32, i32)> = FxHashMap::default();
    let mut node_groups_scratch: Vec<(usize, usize)> = Vec::new();

    loop {
        let item = {
            // Poison recovery keeps the channel usable for the remaining
            // workers. Nothing can panic while the guard is held (it covers
            // only the receive), and a worker that panics elsewhere is caught
            // and published to the failure slot by `run_workers`.
            let rx = candidate_rx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match rx.recv() {
                Ok(item) => item,
                Err(_) => break, // channel closed by scanner
            }
        };

        // Test-only: arm this through the fault plan's
        // `apply_changes_panic_at_blob_seq` (`crate::test_hooks`)
        // to trigger a mid-stream worker panic at a deterministic point,
        // exercising panic publication, barrier shutdown and scratch cleanup.
        // Production builds don't compile this block.
        #[cfg(feature = "test-hooks")]
        {
            let seq = match &item {
                ScannedBlob::Candidate(d) | ScannedBlob::Passthrough(d) => d.seq,
            };
            if panic_at_blob_seq == Some(seq) {
                panic!("test-hooks: panic_at_blob_seq={seq} triggered in worker {_worker_id}");
            }
        }

        match item {
            ScannedBlob::Candidate(mut desc) => {
                handle_candidate(
                    file,
                    &mut desc,
                    drain_tx,
                    counters,
                    ranges,
                    diff,
                    &mut bb,
                    &mut read_buf,
                    &mut decompress_buf,
                    &mut st_scratch,
                    &mut gr_scratch,
                    locations_on_ways,
                    &mut local_coords,
                    &mut node_groups_scratch,
                    coord_slot,
                    loc_map_handle,
                    needed_set,
                    compression,
                    route,
                )
                .map_err(|e| blob_err(&desc, source, e))?;
            }
            ScannedBlob::Passthrough(desc) => {
                // Only seen here under --direct-io output (scanner routes
                // splice-capable passthroughs straight to the drain).
                // Scanner only emits Passthrough for indexed blobs
                // (`scanner.rs::is_fastpath` requires `has_indexdata`),
                // so `desc.index.count`, `desc.kind` and `desc.id_range`
                // are present and are used as given: they are indexdata
                // claims, believed and never checked against this
                // undecoded blob (reference/indexdata-trust.md).
                let count = desc.index.as_ref().map_or(0, |i| i.count);
                handle_owned_passthrough(file, &desc, drain_tx, counters, &mut read_buf, count)
                    .map_err(|e| blob_err(&desc, source, e))?;
            }
        }
    }

    // Nothing may be left to flush here. The candidate channel closes only
    // after the scanner has released its ways, which needs the barrier, so
    // the drain has already merged every slot: coordinates flushed now would
    // never reach the published map. Every node candidate flushes before it
    // sends its item, so a leftover means that rule broke, and it fails the
    // run rather than silently losing coordinates.
    if locations_on_ways && !local_coords.is_empty() {
        return Err(Error::internal(format!(
            "streaming: {} node coordinates were extracted but never flushed \
             before the coordinate map was published",
            local_coords.len()
        )));
    }

    Ok(())
}

/// Slow-path candidate: pread body, decompress, coord extraction
/// (Node + LOW), parse, precise check, false-positive or rewrite.
#[allow(clippy::too_many_arguments)]
// Sequential slow-path handler - the stages (pread, decompress, coord
// extract, parse, precise check, rewrite) share enough scratch state
// (read_buf, decompress_buf, group scratch, local_coords) that a split
// would fragment the descriptor pipeline without clarifying it.
#[allow(clippy::too_many_lines)]
fn handle_candidate(
    file: &std::fs::File,
    desc: &mut BlobDescriptor,
    drain_tx: &mpsc::SyncSender<DrainMessage>,
    counters: &WorkerCounters,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    read_buf: &mut Vec<u8>,
    decompress_buf: &mut Vec<u8>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
    locations_on_ways: bool,
    local_coords: &mut FxHashMap<i64, (i32, i32)>,
    node_groups_scratch: &mut Vec<(usize, usize)>,
    coord_slot: Option<&CoordSlot>,
    loc_map_handle: Option<&LocMapHandle>,
    needed_set: Option<&FxHashSet<i64>>,
    compression: &Compression,
    route: Route<()>,
) -> Result<()> {
    counters.blobs_processed.fetch_add(1, Ordering::Relaxed);

    let body_offset = desc.frame_start + desc.blob_offset as u64;
    pread_into(file, read_buf, body_offset, desc.data_size)?;

    let t_decompress = crate::counters::Stopwatch::start();
    decompress_blob_raw(read_buf, decompress_buf)?;
    t_decompress.record_ns(&counters.decompress_ns);

    // An unindexed blob (`--force`; the scanner sends every one here) gets
    // the sidecars the writer would give it, derived from the payload by the
    // same scan plain `cat` uses. They replace the scanner's placeholder
    // kind and range below, and a blob the diff leaves alone is reframed
    // with them rather than copied: a raw copy would leave it unindexed
    // beside the indexed blobs the rewrites produce, a partially indexed
    // output that every schedule walk (diff, sort, apply-changes itself)
    // refuses. Indexed blobs skip this; their claims are believed
    // (reference/indexdata-trust.md).
    let scanned = if desc.index.is_none() {
        Some(crate::blob_meta::scan_block_ids_and_tags(decompress_buf)?)
    } else {
        None
    };

    if locations_on_ways && desc.kind == ElementKind::Node {
        let t_extract = crate::counters::Stopwatch::start();
        let mut hits = 0u64;
        // Every node is decoded and validated; only the ones an OSC way
        // references are kept, so the per-worker map doesn't grow with
        // the whole base PBF's coord population. An extraction error is
        // fatal: dropping this blob's coordinates would surface later
        // only as an unexplained count of unresolved references.
        extract_node_coords(decompress_buf, node_groups_scratch, |id, lat, lon| {
            if needed_set.is_some_and(|ns| !ns.contains(&id)) {
                return;
            }
            local_coords.insert(id, (lat, lon));
            hits += 1;
        })?;
        counters
            .loc_node_blobs_scanned
            .fetch_add(1, Ordering::Relaxed);
        counters
            .coord_pairs_extracted
            .fetch_add(hits, Ordering::Relaxed);
        t_extract.record_ns(&counters.coord_extract_ns);
    }

    let t_parse = crate::counters::Stopwatch::start();
    let raw = std::mem::take(decompress_buf);
    let block = crate::PrimitiveBlock::from_vec_with_scratch(raw, st_scratch, gr_scratch)?;
    t_parse.record_ns(&counters.parse_ns);

    // What an unindexed blob's payload makes of it, decided before either
    // output branch below so an overlapping blob cannot bypass it.
    let derived = match scanned {
        None => None,
        Some((Some(index), tags)) => Some(Derived::Indexed {
            index,
            tagdata: tags
                .as_ref()
                .and_then(crate::blob_meta::TagIndex::serialize),
        }),
        Some((None, _)) => match single_kind_range(&block)? {
            Some((kind, min_id, max_id)) => Some(Derived::Reencode {
                kind,
                min_id,
                max_id,
            }),
            None => return send_drain(drain_tx, DrainItem::Omitted { seq: desc.seq }),
        },
    };

    let t_precise = crate::counters::Stopwatch::start();
    let overlaps = block_overlaps_diff(&block, ranges);
    t_precise.record_ns(&counters.precise_ns);

    // Sync per-worker coords into the shared slot before emitting the
    // DrainItem. This guarantees the drain's barrier merge (which fires
    // once it has processed every seq below the scanner's node boundary)
    // sees coords for every node blob
    // whose output has been processed - the `local_coords` map is
    // private and the drain reads only the shared `CoordSlot`s. For
    // non-Node blobs `local_coords` is empty so this is a no-op.
    if let Some(slot) = coord_slot
        && !local_coords.is_empty()
    {
        flush_local_coords(slot, local_coords);
    }

    // On the --force path the scanner fills `desc` with a placeholder
    // `kind=Node` and `id_range=None` because it can't distinguish blob
    // kinds without decompressing. The payload's own values replace them
    // so the drain credits `base_*` on the real kind, the rewrite slice
    // lookup and create dispatch see correct values, and type-transition
    // logic doesn't fire spurious Way->Node/Relation->Node flushes that
    // would drain remaining upserts as trailing creates. Indexed
    // descriptors carry their claimed values (the scanner sets `id_range`
    // exactly when `index` is present).
    let (effective_kind, effective_min_id, effective_max_id) = match (&derived, desc.id_range) {
        (Some(Derived::Indexed { index, .. }), _) => (index.kind, index.min_id, index.max_id),
        (
            Some(Derived::Reencode {
                kind,
                min_id,
                max_id,
            }),
            _,
        ) => (*kind, *min_id, *max_id),
        (None, Some((min, max))) => (desc.kind, min, max),
        (None, None) => {
            return Err(Error::internal(
                "streaming: indexed descriptor without an id range",
            ));
        }
    };
    let inline_upserts = upsert_slice(ranges, effective_kind, effective_min_id, effective_max_id);
    // A blob no single index describes cannot be copied with one, so it is
    // re-encoded even when the diff leaves it alone: `BlockBuilder` writes
    // indexed single-kind blocks.
    let must_reencode = matches!(derived, Some(Derived::Reencode { .. }));

    // Passthrough is only sound when the blob needs no edit AND no create
    // lands inside its ID span. `block_overlaps_diff` answers the first
    // question only: a pure create whose ID falls in the blob's interior
    // matches no base element, so `overlaps` is false while
    // `inline_upserts` is non-empty. Shipping the blob verbatim then
    // leaves that create to the *next* blob's gap-create pass (or to the
    // type transition), i.e. emitted after larger IDs - and
    // `build_header_bytes` stamps `Sort.Type_then_ID` unconditionally, so
    // the result is a PBF that claims an ordering it does not have.
    // Forcing the rewrite lets `rewrite_block_parallel` interleave the
    // create at its sorted position and lets the drain advance the upsert
    // cursor past this blob's range.
    if !overlaps && inline_upserts.is_empty() && !must_reencode {
        counters
            .blobs_false_positive
            .fetch_add(1, Ordering::Relaxed);
        if let Some(Derived::Indexed { index, tagdata }) = &derived {
            // The original compressed body under a rebuilt header carrying
            // the scanned sidecars: no recompression, on either route. Any
            // prepass sidecar the input header carried is dropped, which
            // the output header allows: it declares no way enrichment
            // (`Ways::Reencoded` in `build_header_bytes`).
            let frame_bytes =
                reframe_raw_with_index(read_buf, &index.serialize(), tagdata.as_deref())?;
            return send_drain(
                drain_tx,
                DrainItem::OwnedBytes {
                    seq: desc.seq,
                    frame_bytes,
                    kind: effective_kind,
                    id_range: (effective_min_id, effective_max_id),
                    count: index.count,
                },
            );
        }
        // An indexed blob: the drain needs an element count for per-kind
        // `base_*` stats (gap #7), and takes the `desc.index.count` claim,
        // believed like every other here (reference/indexdata-trust.md).
        if route.is_splice() {
            let item = desc.take_copy_range().ok_or_else(|| {
                Error::internal("streaming: false positive without indexdata reached the splice")
            })?;
            return send_drain(drain_tx, item);
        }
        // `--direct-io` output: the drain rejects `DrainItem::CopyRange` up front, so
        // route the false-positive through the owned-passthrough path
        // (pread the full frame, ship the bytes via
        // `DrainItem::OwnedBytes`). `handle_owned_passthrough` does
        // exactly that for the scanner-side passthrough case.
        let count = desc.index.as_ref().map_or(0, |i| i.count);
        return handle_owned_passthrough(file, desc, drain_tx, counters, read_buf, count);
    }
    if !overlaps {
        counters
            .blobs_forced_rewrite
            .fetch_add(1, Ordering::Relaxed);
    }

    // The drain publishes `loc_map` at the node→way barrier; the scanner
    // barrier protocol guarantees it is set before any way/relation
    // descriptor is dispatched to workers. Node blobs run before it, so
    // they see `LocMode::Pending`, under which writing a way is an error:
    // a way in a blob routed as nodes must never be written stripped of
    // its coordinates under a LocationsOnWays header.
    let loc_arc = if locations_on_ways {
        loc_map_handle.and_then(|h| h.get().cloned())
    } else {
        None
    };
    let loc = LocMode::new(locations_on_ways, loc_arc.as_deref());

    let t_rewrite = crate::counters::Stopwatch::start();
    let output = rewrite_block_parallel(&block, diff, bb, inline_upserts, effective_kind, loc)?;
    t_rewrite.record_ns(&counters.rewrite_ns);

    // P1.5: frame each output block in-line via per-thread scratch so the
    // drain side can `write_raw_owned` without the writer's
    // `rayon::spawn`-per-block dispatch (which was the dominant
    // contributor to `writer_pipeline_send_wait_ns` at planet).
    let t_frame = crate::counters::Stopwatch::start();
    let mut framed_chunks: Vec<Vec<u8>> = Vec::with_capacity(output.blocks.len());
    for OwnedBlock {
        bytes: block_bytes,
        index,
        tagdata,
        way_members,
    } in output.blocks
    {
        let indexdata = index.serialize();
        let parts = frame_blob_pipelined(
            &block_bytes,
            compression,
            Some(&indexdata),
            tagdata.as_deref(),
            way_members.as_deref(),
        )?;
        framed_chunks.push(parts.into_vec());
    }
    t_frame.record_ns(&counters.frame_ns);

    counters.blobs_rewritten.fetch_add(1, Ordering::Relaxed);
    send_drain(
        drain_tx,
        DrainItem::Rewritten {
            seq: desc.seq,
            framed_chunks,
            kind: effective_kind,
            id_range: (effective_min_id, effective_max_id),
            stats: output.stats,
        },
    )
}

/// What an unindexed blob's payload says about it, for the sidecars the
/// output needs.
enum Derived {
    /// One index describes the blob: the scan's index and tag key set.
    Indexed {
        index: crate::blob_meta::BlobIndex,
        tagdata: Option<Vec<u8>>,
    },
    /// No index describes the blob, but its elements are all one kind (one
    /// group carries more than one element field), so a re-encode through
    /// `BlockBuilder` turns it into indexed single-kind blocks. The kind and
    /// numeric ID range come from its decoded elements.
    Reencode {
        kind: ElementKind,
        min_id: i64,
        max_id: i64,
    },
}

/// The kind and numeric `(min, max)` ID range of a block no single index
/// describes, walked from its decoded elements; `None` when it yields none.
///
/// The kind comes from the first decoded element, never `block_type()`:
/// that reads only each group's first field, so a group holding an empty
/// `DenseNodes` and then a way would pass for nodes.
///
/// Elements of more than one kind are refused. The drain places each blob
/// by one kind (type transitions, gap creates, the upsert cursor and the
/// order watermark), so another kind's modifications would be written
/// twice and its creates out of type order under the sorted stamp. Such a
/// blob reaches this only on `--force`: on indexed input it carries no
/// indexdata, and the scanner's walk refuses the partially indexed base.
fn single_kind_range(block: &crate::PrimitiveBlock) -> Result<Option<(ElementKind, i64, i64)>> {
    use crate::Element;

    let mut found: Option<(ElementKind, i64, i64)> = None;
    for element in block.elements_skip_metadata() {
        let (kind, id) = match &element {
            Element::DenseNode(dn) => (ElementKind::Node, dn.id()),
            Element::Node(n) => (ElementKind::Node, n.id()),
            Element::Way(w) => (ElementKind::Way, w.id()),
            Element::Relation(r) => (ElementKind::Relation, r.id()),
        };
        found = match found {
            None => Some((kind, id, id)),
            Some((k, min_id, max_id)) if k == kind => Some((k, min_id.min(id), max_id.max(id))),
            Some((k, _, _)) => {
                return Err(Error::invalid_data(format!(
                    "the blob mixes {} and {} elements, which apply-changes cannot \
                     place in type order. Re-encode the base so that each blob holds \
                     one kind of element (a plain cat splits such blobs), and rerun",
                    k.as_str(),
                    kind.as_str()
                )));
            }
        };
    }
    Ok(found)
}

/// `--direct-io` (or consumer-build false-positive) passthrough of an
/// indexed blob: pread the **full framed bytes** so the drain can
/// `write_raw_owned` without re-framing. `count` is the blob's element
/// count; the drain uses it to credit `base_<kind>` stats and must not be
/// zero or per-kind merge totals drift (gap #7).
fn handle_owned_passthrough(
    file: &std::fs::File,
    desc: &BlobDescriptor,
    drain_tx: &mpsc::SyncSender<DrainMessage>,
    counters: &WorkerCounters,
    read_buf: &mut Vec<u8>,
    count: u64,
) -> Result<()> {
    counters.blobs_processed.fetch_add(1, Ordering::Relaxed);
    counters
        .blobs_owned_passthrough
        .fetch_add(1, Ordering::Relaxed);

    pread_into(file, read_buf, desc.frame_start, desc.frame_len)?;

    // Move the bytes into the DrainItem; reset read_buf for the next
    // pread without keeping the per-blob allocation alive.
    let frame_bytes = std::mem::take(read_buf);

    send_drain(
        drain_tx,
        DrainItem::OwnedBytes {
            seq: desc.seq,
            frame_bytes,
            kind: desc.kind,
            id_range: desc.id_range.unwrap_or((0, 0)),
            count,
        },
    )
}

/// Resize `buf` to `len` and pread `len` bytes at `offset`.
fn pread_into(file: &std::fs::File, buf: &mut Vec<u8>, offset: u64, len: usize) -> Result<()> {
    buf.resize(len, 0);
    Ok(file.read_exact_at(buf, offset)?)
}

/// Compute the inline-upsert slice for one blob: upserts whose OSM ID
/// key falls in `[blob_osm_first(min,max), blob_osm_last(min,max)]`.
fn upsert_slice(ranges: &DiffRanges, kind: ElementKind, min_id: i64, max_id: i64) -> &[i64] {
    let upserts = ranges.upserts(kind);
    let first = crate::osm_id::blob_osm_first_key(min_id, max_id);
    let last = crate::osm_id::blob_osm_last_key(min_id, max_id);
    let start = upserts.partition_point(|&id| crate::osm_id::osm_id_key(id) < first);
    let end = upserts[start..].partition_point(|&id| crate::osm_id::osm_id_key(id) <= last) + start;
    &upserts[start..end]
}

/// Drain `local` into the worker's shared coord slot. Acquires the mutex
/// for the duration of the merge; the drain reads via the same Arc so
/// the publish is atomic from its perspective.
///
/// A poisoned slot is recovered, but its contents are not evidence of a
/// complete node phase: a worker panic invalidates the run. The panicking
/// worker publishes its caught panic shortly after unwinding, and never
/// delivers the node item it was processing, so the drain cannot publish a
/// map built without it; the failure is checked again at shutdown.
fn flush_local_coords(
    slot: &Arc<Mutex<FxHashMap<i64, (i32, i32)>>>,
    local: &mut FxHashMap<i64, (i32, i32)>,
) {
    if local.is_empty() {
        return;
    }
    let mut shared = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if shared.is_empty() {
        std::mem::swap(&mut *shared, local);
    } else {
        shared.extend(local.drain());
    }
}

/// Send to the drain. Fails only once the drain has dropped its receiver,
/// which it does on its own error or cancellation; the worker propagates
/// this error, which ends it, and the drain's cause is what the run reports.
fn send_drain(drain_tx: &mpsc::SyncSender<DrainMessage>, item: DrainItem) -> Result<()> {
    drain_tx
        .send(DrainMessage::Item(item))
        .map_err(|_| Error::internal("streaming: drain closed worker-to-drain channel"))
}

/// Locate a per-blob error at the base blob that failed: the file, the
/// walker's blob ordinal and the frame offset. `source` is captured once per
/// pool, so there is no per-blob reference counting.
fn blob_err(desc: &BlobDescriptor, source: &Arc<Path>, e: Error) -> Error {
    e.context("apply-changes: base blob")
        .locate(crate::error::Location::new(
            Some(source),
            Some(desc.blob_index),
            Some(crate::ByteOffset(desc.frame_start)),
        ))
}
