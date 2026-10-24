//! Single-threaded ordered drain actor for the descriptor-first
//! streaming pipeline.
//!
//! The drain consumes a unified [`DrainItem`] stream produced by both
//! the scanner (fast-path `CopyRange` for splice-capable backends) and
//! the worker pool (rewrites, false positives converted to CopyRange,
//! and `--direct-io` `OwnedBytes`). Items arrive out of seq order
//! (workers process in parallel) so the drain reorders them through a
//! `BTreeMap<seq, DrainItem>` keyed by global seq before processing.
//!
//! ## Per-item processing
//!
//! 1. Type transition: when `item.kind != last_type`, flush the
//!    `copy_file_range` coalescer + the `OwnedBytes` passthrough buffer,
//!    then call [`flush_remaining_upserts`] for the previous type.
//!    - At the **node→way** transition under `--locations-on-ways`,
//!      additionally merge per-worker [`CoordSlots`] into the published
//!      `Arc<loc_map>` and signal the scanner over `barrier_tx` (so
//!      it can release its buffered way/relation descriptors).
//! 2. Gap creates: when upserts of `item.kind` exist with `id <
//!    item.min_id`, flush coalescers + emit gap-create elements into a
//!    fresh `BlockBuilder` and flush.
//! 3. Item dispatch:
//!    - [`DrainItem::CopyRange`]: extend the contiguous-range
//!      `copy_file_range` coalescer; flush as a single
//!      `write_raw_copy` when the run breaks (next item is non-copy or
//!      a different contiguous range).
//!    - [`DrainItem::OwnedBytes`]: flush copy-range coalescer; push
//!      the frame into the passthrough chunk buffer for
//!      `write_raw_chunks`.
//!    - [`DrainItem::Rewritten`]: flush both coalescers; call
//!      `write_primitive_block_owned` per output block; advance the
//!      cursor past `blob_osm_last_key(min_id, max_id)` (the
//!      cursor-rule invariant - Rewrite advances, CopyRange/OwnedBytes
//!      do NOT).
//!
//! ## Trailing creates
//!
//! After the drain channel closes and the reorder buffer empties, the
//! drain ports the existing `types_to_flush` match from `rewrite.rs`
//! verbatim: emit any remaining upserts of the current and later kinds.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};

use rustc_hash::FxHashMap;

use crate::ElementKind;
use crate::block_builder::BlockBuilder;
use crate::commands::{flush_block, flush_passthrough_buf};
use crate::error::{Error, Result};
use crate::file_writer::FileWriter;
use crate::osc::CompactDiffOverlay;
use crate::writer::PbfWriter;

use super::descriptor::{DrainItem, Route};
use super::diff_ranges::{DiffRanges, UpsertCursors};
use super::failure::FailureSlot;
use super::loc_mode::LocMode;
use super::stats::MergeStats;
use super::stream_output::{
    emit_create_for_output, emit_gap_creates, flush_remaining_upserts, has_gap_creates,
};
use super::streaming::CoordSlots;

/// Drain → workers post-barrier `loc_map` handle. The drain calls
/// `set` exactly once at the node→way barrier; way-phase workers read
/// the published `Arc<FxHashMap>` to resolve OSC way refs.
pub(super) type LocMapHandle = Arc<OnceLock<Arc<FxHashMap<i64, (i32, i32)>>>>;

/// OSC-pre-seeded coords surrendered to the drain at setup time.
/// Wrapped in a `Mutex<Option<...>>` so the drain can take ownership at
/// the barrier through an immutable `&DrainConfig` borrow.
pub(super) type SeededLocations = std::sync::Mutex<Option<FxHashMap<i64, (i32, i32)>>>;

/// Drain configuration. The writer is passed separately to `run_drain`
/// (it needs `&mut`, which doesn't compose with the immutable borrows
/// of the rest of the config in the inner item-processing functions).
pub(super) struct DrainConfig<'a> {
    pub ranges: &'a DiffRanges,
    pub diff: &'a CompactDiffOverlay,
    /// `Splice` when the output backend supports kernel-space
    /// `copy_file_range`, carrying the open input PBF that `write_raw_copy`
    /// splices from. `Owned` under `--direct-io`: the drain only ever sees
    /// `OwnedBytes` for passthroughs (workers preread the frame), and has no
    /// input file to splice from.
    ///
    /// The input is an `Arc<File>` rather than a `RawFd`: the splice is
    /// queued into the writer thread and runs later, so the descriptor has
    /// to be owned for that long rather than merely happening to still be
    /// open.
    pub route: Route<crate::read::input::InputFile>,
    /// `--locations-on-ways` mode is on. Drain merges `CoordSlots` at
    /// the node→way barrier and signals the scanner.
    pub locations_on_ways: bool,
    /// Per-worker coord slots. Drain acquires every mutex at the
    /// node→way barrier and merges into `loc_map_out`. `None` when
    /// `locations_on_ways == false`.
    pub coord_slots: Option<CoordSlots>,
    /// OSC-pre-seeded coords (from `NodeLocationIndex::build_from_diff`).
    /// Consumed at the barrier merge: drain extends this with every
    /// worker slot before publishing the merged map. Wrapped in a
    /// `Mutex<Option<...>>` so the drain can take ownership at the
    /// barrier even though `cfg` is borrowed immutably elsewhere.
    /// `None` when `locations_on_ways == false`.
    pub seeded_locations: Option<SeededLocations>,
    /// Drain → scanner barrier signal. Drain sends a single unit value
    /// after merging coord slots and publishing `loc_map_out`.
    /// `None` when `locations_on_ways == false`.
    pub barrier_tx: Option<mpsc::SyncSender<()>>,
    /// Published `Arc<loc_map>` for way-phase workers to read after the
    /// barrier. Drain calls `set` once with the merged map. `None` when
    /// `locations_on_ways == false`.
    pub loc_map_out: Option<LocMapHandle>,
    /// First producer failure (scanner or worker pool, errors and caught
    /// panics alike). Polled every loop iteration and once
    /// after the drain channel disconnects; a published failure is
    /// returned as the drain's own error. See `failure.rs` for the
    /// publish-before-disconnect protocol that makes the final check
    /// conclusive.
    pub failure: Arc<FailureSlot>,
}

/// Drain channels.
pub(super) struct DrainChannels {
    /// Unified DrainItem stream from scanner (fast-path CopyRange) and
    /// workers (Rewritten / converted FalsePositive CopyRange / OwnedBytes).
    pub drain_rx: mpsc::Receiver<DrainItem>,
    /// Scanner → drain signal carrying the seq of the last Node
    /// descriptor emitted (or `u64::MAX` if there were no node blobs).
    /// Drain triggers the `CoordSlots` merge + `loc_map` publish + scanner
    /// barrier signal as soon as `state.next_seq > last_node_seq`.
    /// `None` when `locations_on_ways == false`.
    ///
    /// The `u64::MAX` sentinel cannot collide with a real seq: blob seqs
    /// are assigned by `enumerate()` on the scanner's pread stream, and
    /// a realistic PBF tops out ~6 orders of magnitude below 2^44 blobs
    /// let alone 2^64.
    pub last_node_seq_rx: Option<mpsc::Receiver<u64>>,
}

/// Atomic counters surfaced as sidecar counters at end of `merge()`.
pub(super) struct DrainCounters {
    pub items_processed: AtomicU64,
    pub copy_range_calls: AtomicU64,
    pub copy_range_coalesced_items: AtomicU64,
    pub passthrough_chunks_flushed: AtomicU64,
    pub rewrite_blocks_written: AtomicU64,
    pub gap_creates_emitted: AtomicU64,
    pub trailing_creates_emitted: AtomicU64,
    pub reorder_buffer_high_water_count: AtomicU64,
    pub reorder_buffer_high_water_bytes: AtomicU64,
    pub barrier_loc_map_size: AtomicU64,
    /// Cumulative wall-clock time the drain spent waiting for the
    /// next-in-seq item (i.e., reorder gap stalls).
    pub reorder_gap_wait_ns: AtomicU64,
}

impl DrainCounters {
    pub(super) fn new() -> Self {
        Self {
            items_processed: AtomicU64::new(0),
            copy_range_calls: AtomicU64::new(0),
            copy_range_coalesced_items: AtomicU64::new(0),
            passthrough_chunks_flushed: AtomicU64::new(0),
            rewrite_blocks_written: AtomicU64::new(0),
            gap_creates_emitted: AtomicU64::new(0),
            trailing_creates_emitted: AtomicU64::new(0),
            reorder_buffer_high_water_count: AtomicU64::new(0),
            reorder_buffer_high_water_bytes: AtomicU64::new(0),
            barrier_loc_map_size: AtomicU64::new(0),
            reorder_gap_wait_ns: AtomicU64::new(0),
        }
    }

    pub(super) fn emit(&self) {
        macro_rules! emit {
            ($name:literal, $field:ident) => {
                let v = self.$field.load(Ordering::Relaxed);
                crate::debug::emit_counter($name, i64::try_from(v).unwrap_or(i64::MAX));
            };
        }
        emit!("merge_drain_items_processed", items_processed);
        emit!("merge_drain_copy_range_calls", copy_range_calls);
        emit!(
            "merge_drain_copy_range_coalesced_items",
            copy_range_coalesced_items
        );
        emit!(
            "merge_drain_passthrough_chunks_flushed",
            passthrough_chunks_flushed
        );
        emit!("merge_drain_rewrite_blocks_written", rewrite_blocks_written);
        emit!("merge_drain_gap_creates_emitted", gap_creates_emitted);
        emit!(
            "merge_drain_trailing_creates_emitted",
            trailing_creates_emitted
        );
        emit!(
            "merge_drain_reorder_buffer_high_water_count",
            reorder_buffer_high_water_count
        );
        emit!(
            "merge_drain_reorder_buffer_high_water_bytes",
            reorder_buffer_high_water_bytes
        );
        emit!("merge_drain_barrier_loc_map_size", barrier_loc_map_size);
        emit!("merge_drain_reorder_gap_wait_ns", reorder_gap_wait_ns);
    }
}

/// Run the drain to completion. Returns accumulated `MergeStats`.
/// Called by `rewrite::merge`, which owns the mutable output writer.
///
/// Exits when `drain_rx` disconnects AND the reorder buffer is empty.
/// If the channel closes with a seq gap, returns an error rather than
/// silently emitting truncated output.
#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::needless_pass_by_value
)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn run_drain(
    cfg: DrainConfig<'_>,
    channels: DrainChannels,
    writer: &mut PbfWriter<FileWriter>,
    counters: &DrainCounters,
) -> Result<MergeStats> {
    crate::debug::emit_marker("MERGE_DRAIN_START");

    let DrainChannels {
        drain_rx,
        last_node_seq_rx,
    } = channels;

    let mut state = DrainState::new();
    let mut stats = MergeStats::new();
    let mut last_node_seq: Option<u64> = None;

    // Recv timeout for the main loop. Drain blocks up to this long on
    // an empty `drain_rx` before polling `last_node_seq_rx` and the
    // barrier-ready condition. The polling matters under
    // `--locations-on-ways`: if drain consumes the last node DrainItem
    // and then blocks on `recv()` *before* scanner sends
    // `last_node_seq`, both sides deadlock (drain blocks on items, but
    // workers can't emit any until scanner releases way descriptors,
    // which scanner won't do until drain signals the barrier). The
    // timeout breaks the cycle by letting drain re-poll `last_node_seq`
    // and fire the barrier from idle.
    //
    // The 25 ms tradeoff is recorded on `crate::tunables::DRAIN_RECV_TIMEOUT`.
    let recv_timeout = crate::tunables::DRAIN_RECV_TIMEOUT;

    // This reorder map has no admission budget: bytes_in_buffer is only
    // high-water telemetry. Do not stop receiving at a byte threshold when
    // next_seq is missing: that worker sends on the same bounded channel,
    // and later items can fill it ahead of the head, deadlocking the drain.
    // A real bound needs ordered admission before dispatch, with permits
    // retained through delivery, covering scanner passthroughs and workers.
    // Worker-only byte permits can also starve the head behind later work.
    // That changes scheduling on the measured splice/rewrite path and needs
    // memory-bound tests plus matched performance measurements before use.
    //
    // Main loop: pull items (or timeout), reorder, advance, fire
    // barrier when ready. Exits when `drain_rx` disconnects AND the
    // reorder buffer is empty.
    let mut drain_disconnected = false;
    // The invocation's token, checked beside the producer-failure poll
    // below: at most one `DRAIN_RECV_TIMEOUT` plus the item in hand after
    // a cancellation. Returning it leaves through the drain's existing error
    // exit (receiver and barrier sender dropped, producers unblocked, both
    // joined by `merge`), the same one a failed output write takes.
    let cancel = writer.cancel_token().clone();
    loop {
        let recv_start = std::time::Instant::now();
        let item_opt = if drain_disconnected {
            None
        } else {
            match drain_rx.recv_timeout(recv_timeout) {
                Ok(item) => Some(item),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    drain_disconnected = true;
                    None
                }
            }
        };
        let wait_ns = crate::elapsed::elapsed_ns_u64(recv_start);

        // A failed worker never delivers its seq; waiting for it would
        // stall the reorder buffer forever (and under LOW, the barrier).
        if let Some(e) = cfg.failure.take() {
            return Err(e);
        }
        cancel.check()?;

        if state.buffer.is_empty() {
            counters
                .reorder_gap_wait_ns
                .fetch_add(wait_ns, Ordering::Relaxed);
        }

        if let Some(item) = item_opt {
            let seq = item.seq();
            let cost = item.byte_cost();
            state.buffer.insert(seq, item);
            state.bytes_in_buffer += cost;

            // Track high-water marks.
            let buf_count = state.buffer.len() as u64;
            let prev_count = counters
                .reorder_buffer_high_water_count
                .load(Ordering::Relaxed);
            if buf_count > prev_count {
                counters
                    .reorder_buffer_high_water_count
                    .store(buf_count, Ordering::Relaxed);
            }
            let buf_bytes = state.bytes_in_buffer as u64;
            let prev_bytes = counters
                .reorder_buffer_high_water_bytes
                .load(Ordering::Relaxed);
            if buf_bytes > prev_bytes {
                counters
                    .reorder_buffer_high_water_bytes
                    .store(buf_bytes, Ordering::Relaxed);
            }
        }

        // Pick up the scanner's `last_node_seq` if it's been sent
        // (try_recv is cheap; sub-microsecond per iteration). Polled
        // every loop iteration, including after a recv timeout, so the
        // barrier-ready check below fires even when no items are
        // arriving.
        if cfg.locations_on_ways
            && last_node_seq.is_none()
            && let Some(rx) = last_node_seq_rx.as_ref()
            && let Ok(n) = rx.try_recv()
        {
            last_node_seq = Some(n);
        }

        // Advance through contiguous seqs.
        while let Some(item) = state.buffer.remove(&state.next_seq) {
            state.bytes_in_buffer = state.bytes_in_buffer.saturating_sub(item.byte_cost());
            process_item(item, &cfg, &mut state, writer, &mut stats, counters)?;
            state.next_seq += 1;

            // Barrier check after each processed item: if all node
            // items have been processed and the barrier hasn't fired
            // yet, fire it now.
            if cfg.locations_on_ways
                && !state.barrier_done
                && let Some(n) = last_node_seq
                && (n == u64::MAX || state.next_seq > n)
            {
                barrier_publish_loc_map(&cfg, &mut state, counters)?;
            }
        }

        // Idle barrier fire: if drain has nothing in the buffer but
        // the barrier is ready (e.g., the last node item was
        // processed before scanner sent `last_node_seq`), fire it now
        // so scanner unblocks. Without this, drain blocks on
        // recv_timeout, scanner blocks on barrier_rx, workers block
        // on candidate_rx -> three-way deadlock.
        if cfg.locations_on_ways
            && !state.barrier_done
            && let Some(n) = last_node_seq
            && (n == u64::MAX || state.next_seq > n)
        {
            barrier_publish_loc_map(&cfg, &mut state, counters)?;
        }

        // Exit as soon as all senders have closed. If the buffer is
        // empty we're done cleanly; if it still has items, a producer
        // dropped a seq and the post-loop checks below surface the
        // published failure or the gap. An `&& buffer.is_empty()` gate
        // would spin forever here, because the buffer holds seqs ahead
        // of the missing one and the reorder loop can't advance past it.
        if drain_disconnected {
            break;
        }
    }

    // Channel closed. A producer (scanner or worker) that returned an error
    // or panicked published it before releasing its sender (see
    // `failure.rs`), so checking here, after the disconnect, cannot miss one.
    if cfg.failure.has_failed() {
        return Err(cfg.failure.take().unwrap_or_else(|| {
            Error::internal("drain: a producer failed and its error was already reported")
        }));
    }

    // Reorder buffer should be empty. Scanner and worker errors and panics
    // are caught and published before their senders drop, so reaching this
    // with items left means a producer dropped a seq without failing: a
    // genuine producer bug, which is what the message points at.
    if !state.buffer.is_empty() {
        let first_remaining = state.buffer.keys().next().copied().unwrap_or(0);
        return Err(Error::internal(format!(
            "drain: channel closed with {} items still in reorder buffer; expected next seq {}, \
             smallest remaining seq {}. Producer dropped a seq.",
            state.buffer.len(),
            state.next_seq,
            first_remaining,
        )));
    }

    // Final flush of any in-flight coalescers.
    flush_copy_range(&mut state, &cfg, writer, counters)?;
    flush_passthrough_buf(&mut state.passthrough_chunks, writer)?;

    // Trailing creates: port `types_to_flush` match verbatim from
    // `rewrite.rs` so the cursor walk after end-of-stream emits creates
    // for any kinds whose blobs we never saw (or saw and finished).
    crate::debug::emit_marker("MERGE_TRAILING_CREATES_START");
    let types_to_flush = match state.last_type {
        None | Some(ElementKind::Node) => {
            &[ElementKind::Node, ElementKind::Way, ElementKind::Relation][..]
        }
        Some(ElementKind::Way) => &[ElementKind::Way, ElementKind::Relation][..],
        Some(ElementKind::Relation) => &[ElementKind::Relation][..],
    };
    // Cloned rather than borrowed from `state`: the trailing loop now takes
    // `&mut state` for the sortedness guard, so an outstanding `&state`
    // borrow across it would not compile. The clone is one Arc refcount.
    let loc_map_owned = state.loc_map.clone();
    let loc = LocMode::new(cfg.locations_on_ways, loc_map_owned.as_deref());
    for &kind in types_to_flush {
        loop {
            let (cursor, upserts) = state.cursors.get(kind, cfg.ranges);
            let Some(&id) = upserts.get(cursor) else {
                break;
            };
            // Trailing creates are a second emission channel: they do not
            // pass through `dispatch_item`, so without this call the
            // sortedness guard would be blind to exactly the last thing
            // written - the same shape of trailing blindness the write
            // path's stream-completeness check had. A single ID is its own
            // one-element span. `upserts` is sorted with `osm_id_cmp`, so a
            // run of trailing creates never trips the guard on its own; a
            // create ordered before base output already emitted does.
            check_output_order(&mut state, kind, id, id)?;
            emit_create_for_output(id, kind, cfg.diff, &mut state.bb, writer, &mut stats, loc)?;
            let (cursor, _) = state.cursors.get_mut(kind, cfg.ranges);
            *cursor += 1;
            counters
                .trailing_creates_emitted
                .fetch_add(1, Ordering::Relaxed);
        }
        flush_block(&mut state.bb, writer)?;
    }
    crate::debug::emit_marker("MERGE_TRAILING_CREATES_END");

    writer.flush()?;

    crate::debug::emit_marker("MERGE_DRAIN_END");
    counters.emit();

    Ok(stats)
}

/// Drain working state. Carved out of `run_drain` so `process_item` can
/// take a single `&mut DrainState` instead of half a dozen mut refs.
struct DrainState {
    /// Reorder buffer keyed by global seq.
    buffer: BTreeMap<u64, DrainItem>,
    /// Sum of `byte_cost()` across all items currently buffered.
    bytes_in_buffer: usize,
    /// Next seq the drain expects to process (monotonically increasing).
    next_seq: u64,
    /// Last item kind processed. Drives type transitions.
    last_type: Option<ElementKind>,
    cursors: UpsertCursors,
    /// Builder used for gap-create + trailing-create elements only.
    /// Rewrite blocks bypass this path entirely (drain forwards them
    /// straight to `writer.write_primitive_block_owned`).
    bb: BlockBuilder,
    /// Coalesced contiguous `copy_file_range` runs. `Some((start, end))`
    /// means a run is in progress; flushed when broken.
    copy_range_run: Option<(u64, u64)>,
    /// Coalesced `OwnedBytes` passthrough chunks. Flushed as a single
    /// `write_raw_chunks` when broken.
    passthrough_chunks: Vec<Vec<u8>>,
    /// Published loc_map after the barrier. Workers reading way blobs
    /// see this through `cfg.loc_map_out`; the drain uses this clone
    /// for its own gap-create / trailing-create paths in the way phase.
    loc_map: Option<Arc<FxHashMap<i64, (i32, i32)>>>,
    /// Set true once the node→way barrier has been crossed and
    /// `loc_map_out` published. Stays true for the rest of the run.
    barrier_done: bool,
    /// Running (kind, `osm_id_key`) high-water mark across every item
    /// already dispatched. Drives [`check_output_order`]; see there for
    /// why this is a running max rather than a pairwise-adjacent check.
    order_watermark: crate::osm_id::OrderWatermark,
}

impl DrainState {
    fn new() -> Self {
        Self {
            buffer: BTreeMap::new(),
            bytes_in_buffer: 0,
            next_seq: 0,
            last_type: None,
            cursors: UpsertCursors::new(),
            bb: BlockBuilder::new(),
            copy_range_run: None,
            passthrough_chunks: Vec::new(),
            loc_map: None,
            barrier_done: false,
            order_watermark: crate::osm_id::OrderWatermark::default(),
        }
    }
}

/// Refuse to emit an out-of-order blob into a file whose header stamps
/// `Sort.Type_then_ID`.
///
/// `build_header_bytes` writes `hb.sorted()` unconditionally, and it gates
/// on the *base* header's `is_sorted()` - but a header can lie (`degrade
/// --unsort` exists precisely to produce such files), and a pipeline bug
/// can misplace a create even when the base is honestly sorted. Either way
/// the failure mode is the worst shape available in this domain: a file
/// that claims an ordering it does not have, which every downstream
/// consumer then trusts. So the drain checks what it actually emits.
///
/// The watermark retains the last accepted kind and its greatest end bound.
/// Each incoming first bound must reach that bound: `10..30, 5..8` is
/// rejected immediately at 5. For accepted same-kind ranges, comparing the
/// previous end bound is sufficient too, since accepted bounds cannot move
/// backwards. The shared watermark expresses this invariant directly.
///
/// The bound compared is the OSM-order first id the blob's range proves
/// present (`osm_id::blob_osm_present_first_key`), so negative IDs (0, -1,
/// -2, ..., 1, 2, ...) order the way `osm_id_cmp` orders them everywhere
/// else and a valid mixed-sign blob after a negative one passes. Equality is
/// allowed through: a shared boundary ID between two blobs is a duplicate-ID
/// problem, not an ordering one, and is not this guard's business.
fn check_output_order(
    state: &mut DrainState,
    kind: ElementKind,
    min_id: i64,
    max_id: i64,
) -> Result<()> {
    if let Err(v) = state
        .order_watermark
        .observe(kind.ordinal(), min_id, max_id)
    {
        return Err(crate::error::new_unsorted(format!(
            "apply-changes: refusing to write an unsorted PBF under a Sort.Type_then_ID \
             header - a {} blob holding id {} follows {} output \
             already reaching id {}; the base is not in the order its header \
             declares.",
            kind.as_str(),
            v.first_id,
            ElementKind::from_ordinal(v.previous_kind).map_or("unknown", ElementKind::as_str),
            v.previous_id,
        )));
    }
    Ok(())
}

/// Handle a type transition (`prev != item_kind`). Flushes coalescers,
/// triggers the node→way barrier when the transition matches, then
/// emits any remaining upserts of the previous kind.
#[allow(clippy::too_many_arguments)]
fn handle_type_transition(
    prev: ElementKind,
    item_kind: ElementKind,
    cfg: &DrainConfig<'_>,
    state: &mut DrainState,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    counters: &DrainCounters,
) -> Result<()> {
    flush_copy_range(state, cfg, writer, counters)?;
    let had_chunks = !state.passthrough_chunks.is_empty();
    flush_passthrough_buf(&mut state.passthrough_chunks, writer)?;
    if had_chunks {
        counters
            .passthrough_chunks_flushed
            .fetch_add(1, Ordering::Relaxed);
    }

    if cfg.locations_on_ways
        && !state.barrier_done
        && prev == ElementKind::Node
        && matches!(item_kind, ElementKind::Way | ElementKind::Relation)
    {
        barrier_publish_loc_map(cfg, state, counters)?;
    }

    let loc = LocMode::new(cfg.locations_on_ways, state.loc_map.as_deref());
    flush_remaining_upserts(
        prev,
        item_kind,
        cfg.ranges,
        cfg.diff,
        &mut state.cursors,
        &mut state.bb,
        writer,
        stats,
        loc,
    )
}

/// Emit gap creates for the current blob's kind: upserts whose ID
/// precedes `osm_first` in OSM order. Flushes coalescers first.
fn handle_gap_creates(
    item_kind: ElementKind,
    osm_first: i64,
    cfg: &DrainConfig<'_>,
    state: &mut DrainState,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    counters: &DrainCounters,
) -> Result<()> {
    if !has_gap_creates(item_kind, osm_first, cfg.ranges, &state.cursors) {
        return Ok(());
    }
    flush_copy_range(state, cfg, writer, counters)?;
    flush_passthrough_buf(&mut state.passthrough_chunks, writer)?;
    let loc = LocMode::new(cfg.locations_on_ways, state.loc_map.as_deref());
    emit_gap_creates(
        item_kind,
        osm_first,
        cfg.ranges,
        cfg.diff,
        &mut state.cursors,
        &mut state.bb,
        writer,
        stats,
        loc,
    )?;
    flush_block(&mut state.bb, writer)?;
    counters.gap_creates_emitted.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Process one in-order DrainItem. Handles type transitions, gap
/// creates, and per-variant dispatch.
#[allow(clippy::too_many_arguments)]
fn process_item(
    item: DrainItem,
    cfg: &DrainConfig<'_>,
    state: &mut DrainState,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    counters: &DrainCounters,
) -> Result<()> {
    counters.items_processed.fetch_add(1, Ordering::Relaxed);

    let item_kind = item.kind();
    let (min_id, max_id) = item.id_range();

    if let Some(prev) = state.last_type
        && prev != item_kind
    {
        handle_type_transition(prev, item_kind, cfg, state, writer, stats, counters)?;
    }
    state.last_type = Some(item_kind);

    // Skip gap-creates for the reversed-range sentinel `(i64::MAX, i64::MIN)`
    // that `infer_kind_and_range` (the --force non-indexed path in
    // `streaming.rs`) emits for an empty block, and for a block whose
    // `block_type()` names a kind none of its elements has: its min/max
    // accumulators never move off their starting values, so the range
    // stays reversed. Without this guard, `blob_osm_first_id` would
    // return `i64::MAX` and `has_gap_creates` would emit every remaining
    // upsert of `item_kind` as a gap-create. The dispatch below still writes
    // the rewritten blob; trailing-creates picks up any legitimate upserts.
    if min_id <= max_id {
        // Sortedness guard runs on the same non-sentinel condition as
        // gap-creates: the reversed sentinel `(i64::MAX, i64::MIN)` names
        // no real ID span, so it carries no ordering claim.
        check_output_order(state, item_kind, min_id, max_id)?;
        let osm_first = crate::osm_id::blob_osm_first_id(min_id, max_id);
        handle_gap_creates(item_kind, osm_first, cfg, state, writer, stats, counters)?;
    }

    dispatch_variant(item, cfg, state, writer, stats, counters)
}

/// Per-variant dispatch (CopyRange / OwnedBytes / Rewritten). Mutates
/// `state` (coalescers, cursor) and `stats` (blob/byte counters).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn dispatch_variant(
    item: DrainItem,
    cfg: &DrainConfig<'_>,
    state: &mut DrainState,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    counters: &DrainCounters,
) -> Result<()> {
    match item {
        DrainItem::CopyRange {
            seq: _,
            frame_start,
            frame_len,
            kind,
            id_range: _,
            index,
            tagdata: _,
        } => {
            if !cfg.route.is_splice() {
                return Err(Error::internal(
                    "drain: received CopyRange item on the owned route",
                ));
            }
            flush_passthrough_buf(&mut state.passthrough_chunks, writer)?;

            let new_end = frame_start + frame_len as u64;
            match state.copy_range_run {
                Some((run_start, run_end)) if run_end == frame_start => {
                    state.copy_range_run = Some((run_start, new_end));
                    counters
                        .copy_range_coalesced_items
                        .fetch_add(1, Ordering::Relaxed);
                }
                _ => {
                    flush_copy_range(state, cfg, writer, counters)?;
                    state.copy_range_run = Some((frame_start, new_end));
                }
            }

            stats.bytes_passthrough += frame_len as u64;
            stats.blobs_passthrough += 1;
            stats.blobs_index_hit += 1;
            // One original input frame, not the coalesced copy range: the
            // HeaderWalker caps its body below 32 MiB and header below 64 KiB
            // (plus the 4-byte prefix), so this sample fits u32.
            #[allow(clippy::cast_possible_truncation)]
            stats.blob_sizes.push(frame_len as u32);
            match kind {
                ElementKind::Node => stats.base_nodes += index.count,
                ElementKind::Way => stats.base_ways += index.count,
                ElementKind::Relation => stats.base_relations += index.count,
            }
        }
        DrainItem::OwnedBytes {
            seq: _,
            frame_bytes,
            kind,
            id_range: _,
            count,
        } => {
            // Flush the copy_range coalescer so on-disk order is right.
            flush_copy_range(state, cfg, writer, counters)?;
            let frame_len = frame_bytes.len() as u64;
            state.passthrough_chunks.push(frame_bytes);
            stats.bytes_passthrough += frame_len;
            stats.blobs_passthrough += 1;
            // Workers copy one validated original frame here. The same
            // 32 MiB body + 64 KiB header + 4-byte prefix bound applies;
            // this is not the accumulated passthrough buffer's length.
            #[allow(clippy::cast_possible_truncation)]
            stats.blob_sizes.push(frame_len as u32);
            // Credit per-kind base_* stats the same way the CopyRange arm
            // above does. Before this line `OwnedBytes` dropped per-kind
            // counts entirely, so any merge that routed passthroughs
            // through this arm (consumer build, `--direct-io`, or a
            // false-positive on `Route::Owned`) reported
            // `base_nodes=base_ways=base_relations=0` for the
            // passthrough portion even though the output contained the
            // elements correctly.
            match kind {
                ElementKind::Node => stats.base_nodes += count,
                ElementKind::Way => stats.base_ways += count,
                ElementKind::Relation => stats.base_relations += count,
            }
        }
        DrainItem::Rewritten {
            seq: _,
            framed_chunks,
            kind,
            id_range,
            stats: per_blob_stats,
        } => {
            flush_copy_range(state, cfg, writer, counters)?;
            flush_passthrough_buf(&mut state.passthrough_chunks, writer)?;
            let mut rewrite_bytes: u64 = 0;
            // P1.5: workers already framed each chunk. Drain ships
            // them through `write_raw_owned` so the writer's
            // rayon-spawn-per-block path (and its
            // `pipeline_send_wait_ns` blowup) is bypassed entirely.
            for chunk in framed_chunks {
                rewrite_bytes += chunk.len() as u64;
                writer.write_raw_owned(chunk)?;
                counters
                    .rewrite_blocks_written
                    .fetch_add(1, Ordering::Relaxed);
            }
            stats.bytes_rewritten += rewrite_bytes;
            stats.blobs_rewritten += 1;
            // Merge per-blob counters (deleted, diff_*, base_*) into
            // the run accumulator. This preserves the legacy stats
            // shape that callers + tests assert on.
            stats.merge_from(&per_blob_stats);

            // Cursor-rule invariant (see the Correctness invariants
            // section in the plan doc): Rewrite advances the cursor
            // past `blob_osm_last_key(min_id, max_id)`. Passthrough /
            // OwnedBytes do NOT - inline upserts in those ranges
            // become gap creates on the next same-type blob.
            let (min_id, max_id) = id_range;
            let last = crate::osm_id::blob_osm_last_key(min_id, max_id);
            let (cursor, upserts) = state.cursors.get_mut(kind, cfg.ranges);
            while *cursor < upserts.len() && crate::osm_id::osm_id_key(upserts[*cursor]) <= last {
                *cursor += 1;
            }
        }
    }

    Ok(())
}

/// Flush the in-flight contiguous `copy_file_range` run as a single
/// `write_raw_copy(&input_file, start, len)` call. A run only exists on the
/// `Route::Splice` route, which `merge()` selects only when the
/// `linux-direct-io` feature is compiled.
#[cfg(feature = "linux-direct-io")]
fn flush_copy_range(
    state: &mut DrainState,
    cfg: &DrainConfig<'_>,
    writer: &mut PbfWriter<FileWriter>,
    counters: &DrainCounters,
) -> Result<()> {
    let Some((start, end)) = state.copy_range_run.take() else {
        return Ok(());
    };
    let Route::Splice(input_file) = &cfg.route else {
        return Err(Error::internal(
            "drain: copy range run pending on the owned route",
        ));
    };
    let len = end - start;
    writer.write_raw_copy(input_file, start, len)?;
    counters.copy_range_calls.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// No-op variant under non-Linux / no-`linux-direct-io` builds. The
/// CopyRange branch in `dispatch_variant` only fires on
/// `Route::Splice`, which `merge()` only ever selects when the feature is
/// enabled - so this body is unreachable at runtime, but the symbol must
/// compile so the call sites do.
#[cfg(not(feature = "linux-direct-io"))]
fn flush_copy_range(
    state: &mut DrainState,
    _cfg: &DrainConfig<'_>,
    _writer: &mut PbfWriter<FileWriter>,
    _counters: &DrainCounters,
) -> Result<()> {
    if state.copy_range_run.take().is_some() {
        return Err(Error::internal(
            "drain: copy_file_range path requires linux-direct-io feature",
        ));
    }
    Ok(())
}

/// Merge per-worker `CoordSlots` into the published `Arc<FxHashMap>`,
/// then signal the scanner over `barrier_tx`. Called exactly once at
/// the node→way transition under `--locations-on-ways`.
fn barrier_publish_loc_map(
    cfg: &DrainConfig<'_>,
    state: &mut DrainState,
    counters: &DrainCounters,
) -> Result<()> {
    crate::debug::emit_marker("MERGE_DRAIN_BARRIER_START");

    let slots = cfg
        .coord_slots
        .as_ref()
        .ok_or_else(|| Error::internal("drain: locations_on_ways true but coord_slots is None"))?;
    let loc_map_out = cfg
        .loc_map_out
        .as_ref()
        .ok_or_else(|| Error::internal("drain: locations_on_ways true but loc_map_out is None"))?;

    // Seed with OSC pre-seeded coords (from
    // `NodeLocationIndex::build_from_diff`'s `locations`) before
    // extending with per-worker accumulators. Take ownership via the
    // Mutex so subsequent calls (defensive; barrier should fire once)
    // see an empty map.
    let mut merged: FxHashMap<i64, (i32, i32)> = cfg
        .seeded_locations
        .as_ref()
        .and_then(|m| {
            m.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        })
        .unwrap_or_default();
    for slot in slots {
        let mut local = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if merged.is_empty() {
            std::mem::swap(&mut merged, &mut *local);
        } else {
            merged.extend(local.drain());
        }
    }

    counters
        .barrier_loc_map_size
        .store(merged.len() as u64, Ordering::Relaxed);

    let arc = Arc::new(merged);
    // OnceLock::set fails only if already set; under our own protocol
    // the drain calls this exactly once, so a set failure is a bug,
    // not a steady-state condition.
    if loc_map_out.set(Arc::clone(&arc)).is_err() {
        return Err(Error::internal(
            "drain: loc_map_out already published - barrier ran twice?",
        ));
    }
    state.loc_map = Some(arc);
    state.barrier_done = true;

    if let Some(barrier_tx) = cfg.barrier_tx.as_ref() {
        // Send is fire-and-forget: scanner may have already exited
        // (small datasets) and dropped its receiver. Either way the
        // drain is done with the barrier; a closed channel here is not
        // an error.
        // Closed receiver = scanner already exited; benign at end-of-stream.
        if let Err(e) = barrier_tx.send(()) {
            let _: mpsc::SendError<()> = e;
        }
    }

    crate::debug::emit_marker("MERGE_DRAIN_BARRIER_END");
    Ok(())
}
