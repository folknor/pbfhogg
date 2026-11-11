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
//! 2. Gap creates: when upserts of `item.kind` exist with `id <
//!    item.min_id`, flush coalescers + write them as one create run
//!    (`stream_output::CreateEncoder`).
//! 3. Item dispatch:
//!    - [`DrainItem::CopyRange`]: extend the contiguous-range
//!      `copy_file_range` coalescer; flush as a single
//!      `write_raw_copy` when the run breaks (next item is non-copy or
//!      a different contiguous range).
//!    - [`DrainItem::OwnedBytes`]: flush copy-range coalescer; push
//!      the frame into the passthrough chunk buffer for
//!      `write_raw_chunks`.
//!    - [`DrainItem::Rewritten`]: flush both coalescers; write each
//!      worker-framed chunk with `write_raw_owned`; advance the
//!      cursor past `blob_osm_last_key(min_id, max_id)` (the
//!      cursor-rule invariant - Rewrite advances, CopyRange/OwnedBytes
//!      do NOT).
//!
//! ## Node→way barrier
//!
//! Under `--locations-on-ways` the barrier is separate from item processing:
//! once the scanner's [`DrainMessage::NodeBoundary`] has arrived and every
//! seq below it is processed, the drain merges per-worker [`CoordSlots`]
//! into the published `Arc<loc_map>` and signals the scanner over
//! `barrier_tx`; the scanner, waiting at its first way or relation
//! descriptor, then dispatches way and relation work as it walks. No way or
//! relation item can arrive before that. The missing-node
//! refusal is taken there too, before publication: the merged map is final,
//! so an OSC way reference it cannot resolve fails the run at the barrier
//! rather than after the way phase.
//!
//! One pipeline run serves both phases: the scanner stops at the first way
//! or relation header while node items are still being processed, and
//! resumes its walk at the signal instead of starting a second run. Splitting
//! the node phase into its own run was considered and not taken: it would
//! change worker lifetime and scheduling on this measured path, and the
//! barrier's correctness does not need it.
//!
//! ## Trailing creates
//!
//! After the drain channel closes and the reorder buffer empties, the
//! drain ports the existing `types_to_flush` match from `rewrite.rs`
//! verbatim: emit any remaining upserts of the current and later kinds.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock, mpsc};

use crate::ElementKind;
use crate::commands::flush_passthrough_buf;
use crate::error::{Error, Result};
use crate::file_writer::FileWriter;
use crate::osc::sorted::SortedDiff;
use crate::writer::PbfWriter;

use super::descriptor::{DrainItem, DrainMessage, Route};
use super::failure::FailureSlot;
use super::loc_mode::{LocMap, LocMode};
use super::stats::MergeStats;
use super::stream_output::{
    CreateEncoder, emit_gap_creates, flush_remaining_upserts, has_gap_creates,
};
use super::streaming::CoordSlots;
use super::upsert_cursors::UpsertCursors;
use super::window::WindowReturn;

/// Drain → workers post-barrier `loc_map` handle. The drain calls
/// `set` exactly once at the node→way barrier; way-phase workers read
/// the published `Arc<FxHashMap>` to resolve OSC way refs.
pub(super) type LocMapHandle = Arc<OnceLock<Arc<LocMap>>>;

/// The drain's share of `--locations-on-ways`, everything the node→way
/// barrier consumes. Built with the scanner's and the workers' shares by
/// `rewrite::wire_locations`, so either all of it is there or the mode is
/// off. [`DrainLocations::publish`] consumes it at the barrier.
pub(super) struct DrainLocations {
    /// Clones of every worker's coordinate slot, merged at the barrier.
    pub coord_slots: CoordSlots,
    /// OSC-pre-seeded coords (`NodeLocationIndex::build_from_diff`), the
    /// seed the worker slots are merged into.
    pub seeded: LocMap,
    /// Drain → scanner barrier signal: one unit value after the map is
    /// published. Kept until the drain returns (see [`PublishedLocations`]).
    pub barrier_tx: mpsc::SyncSender<()>,
    /// Where the merged map is published for way-phase workers.
    pub loc_map_out: LocMapHandle,
    /// The distinct node IDs the OSC's ways reference
    /// (`NodeLocationIndex::total_referenced`). The merged map at the barrier
    /// holds only such IDs, so its size against this is the run's unresolved
    /// count, final at the barrier: nothing resolves later.
    pub nodes_needed: u64,
    /// Publish a map with unresolved references (they are written as
    /// `(0, 0)`) instead of refusing at the barrier.
    pub ignore_missing_nodes: bool,
}

/// What survives the barrier: the published map, for the drain's own create
/// runs, and the barrier sender. The sender is held until the drain returns
/// because the scanner's poll after each dispatch reads a closed barrier
/// channel as a failed drain; dropping it at publication would fail every
/// run.
struct PublishedLocations {
    map: Arc<LocMap>,
    _barrier_tx: mpsc::SyncSender<()>,
}

/// The drain's `--locations-on-ways` state; absent when the mode is off.
enum DrainLoc {
    /// Before the barrier: node items only, ways would be written `Pending`.
    Pending(DrainLocations),
    Published(PublishedLocations),
}

impl DrainLoc {
    /// The published map, if the barrier has run.
    fn published_map(&self) -> Option<Arc<LocMap>> {
        match self {
            Self::Pending(_) => None,
            Self::Published(p) => Some(Arc::clone(&p.map)),
        }
    }
}

/// How the drain writes ways in `loc`'s state. A free function over the one
/// field so callers can hold it beside `&mut` borrows of the others.
fn loc_mode(loc: Option<&DrainLoc>) -> LocMode<'_> {
    match loc {
        None => LocMode::Off,
        Some(DrainLoc::Pending(_)) => LocMode::Pending,
        Some(DrainLoc::Published(p)) => LocMode::Required(&p.map),
    }
}

/// Drain configuration. The writer is passed separately to `run_drain`
/// (it needs `&mut`, which doesn't compose with the immutable borrows
/// of the rest of the config in the inner item-processing functions), and so
/// is the `--locations-on-ways` payload, which the drain moves into its own
/// state and consumes at the barrier.
pub(super) struct DrainConfig<'a> {
    pub diff: &'a SortedDiff,
    /// Where create runs longer than one block are encoded and framed (the
    /// invocation's executor; `None` encodes them on the drain thread), and
    /// the output compression they are framed with.
    pub create_executor: Option<&'a crate::host::Executor>,
    pub compression: crate::writer::Compression,
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
    /// Unified stream from scanner (fast-path CopyRange, and under
    /// `--locations-on-ways` its one [`DrainMessage::NodeBoundary`]) and
    /// workers (Rewritten / converted FalsePositive CopyRange / OwnedBytes).
    pub drain_rx: mpsc::Receiver<DrainMessage>,
    /// The sequence window's only sender: one token back per processed seq.
    /// Dropped with the drain on every exit, which wakes a parked scanner.
    pub window: WindowReturn,
}

crate::counters::counter_set! {
    /// Drain-thread counters, surfaced as sidecar counters when the drain
    /// finishes. The drain is their only writer: the two high-water marks
    /// are raised with a plain load and store, not `record_max`, because no
    /// other thread can race the raise and a compare-exchange would add a
    /// locked operation for nothing. A separate set (and allocation) from
    /// the worker pool's `WorkerCounters`.
    pub(super) struct DrainCounters {
        pub items_processed => "merge_drain_items_processed",
        pub copy_range_calls => "merge_drain_copy_range_calls",
        pub copy_range_coalesced_items => "merge_drain_copy_range_coalesced_items",
        pub passthrough_chunks_flushed => "merge_drain_passthrough_chunks_flushed",
        pub rewrite_blocks_written => "merge_drain_rewrite_blocks_written",
        pub gap_creates_emitted => "merge_drain_gap_creates_emitted",
        /// Creates written at type transitions: the upserts of a kind left
        /// after its last blob (and of a skipped kind), counted per id.
        pub transition_creates_emitted => "merge_drain_transition_creates_emitted",
        pub trailing_creates_emitted => "merge_drain_trailing_creates_emitted",
        pub reorder_buffer_high_water_count => "merge_drain_reorder_buffer_high_water_count",
        pub reorder_buffer_high_water_bytes => "merge_drain_reorder_buffer_high_water_bytes",
        pub barrier_loc_map_size => "merge_drain_barrier_loc_map_size",
        /// Estimated bytes of the workers' coordinate slot tables the
        /// barrier merge consumed and freed (a table moved whole into the
        /// published map stays live and is not counted). Source storage
        /// released, not a net saving: the published map grows as it takes
        /// their entries.
        pub barrier_slot_bytes_freed => "merge_drain_barrier_slot_bytes_freed",
        /// Cumulative time the drain spent in a receive that ended with the
        /// reorder buffer empty. Waits with later seqs buffered behind a
        /// missing head are not counted, so this is not a head-of-line stall
        /// measure; `merge_scanner_window_wait_ns` shows when such a stall
        /// held dispatch back.
        pub reorder_gap_wait_ns => "merge_drain_reorder_gap_wait_ns",
        /// Most seqs reserved by the scanner and not yet processed here at
        /// once (exact, raised at reservation). It caps the drain channel's
        /// item occupancy, which the channel itself cannot report (the
        /// `--locations-on-ways` node boundary adds one slot), and is capped
        /// by `tunables::APPLY_CHANGES_SEQ_WINDOW`.
        pub seq_window_high_water => "merge_seq_window_high_water",
    }
}

/// Run the drain to completion. Returns accumulated `MergeStats`.
/// Called by `rewrite::merge`, which owns the mutable output writer.
/// `locations` is `Some` under `--locations-on-ways`.
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
    locations: Option<DrainLocations>,
    writer: &mut PbfWriter<FileWriter>,
    counters: &DrainCounters,
) -> Result<MergeStats> {
    writer.cancel_token().marker("MERGE_DRAIN_START");

    let DrainChannels { drain_rx, window } = channels;

    let mut state = DrainState::new(
        CreateEncoder::new(cfg.create_executor, cfg.compression),
        locations.map(DrainLoc::Pending),
    );
    let mut stats = MergeStats::new();
    // The scanner's node boundary under `--locations-on-ways`, once
    // received: the exclusive end of the node seqs.
    let mut node_end_seq: Option<u64> = None;

    // Recv timeout for the main loop. The node boundary arrives on this
    // channel, so the barrier needs no polling; the timeout is what lets
    // the drain notice a published producer failure or a cancellation when
    // nothing else would wake it (a failed worker never delivers its seq,
    // while the scanner can sit in its barrier wait and the surviving
    // workers idle on the candidate channel).
    //
    // The 25 ms tradeoff is recorded on `crate::tunables::DRAIN_RECV_TIMEOUT`.
    let recv_timeout = crate::tunables::DRAIN_RECV_TIMEOUT;

    // The reorder map is bounded upstream, by the sequence window
    // (`window.rs`): the scanner takes a token per seq before dispatch and
    // the advance loop below returns it once that seq is processed, so the
    // map never holds more than `APPLY_CHANGES_SEQ_WINDOW` seqs. That is a
    // count, not bytes; bytes_in_buffer stays high-water telemetry. Never
    // gate this receive instead (on bytes or count) while next_seq is
    // missing: that worker sends on the same bounded channel, later items
    // can fill it ahead of the head, and the drain deadlocks. Worker-only
    // byte permits would starve the head behind later work the same way.
    //
    // Main loop: pull items (or timeout), reorder, advance, fire
    // barrier when ready. Exits when `drain_rx` disconnects AND the
    // reorder buffer is empty.
    let mut drain_disconnected = false;
    // The invocation's token, checked beside the producer-failure poll
    // below, once per receive: after a cancellation the drain stops within
    // one `DRAIN_RECV_TIMEOUT` plus the contiguous run of buffered items it
    // is already writing (the advance loop below does not check between
    // items). Returning it leaves through the drain's existing error
    // exit (receiver and barrier sender dropped, producers unblocked, both
    // joined by `merge`), the same one a failed output write takes.
    let cancel = writer.cancel_token().permit();
    loop {
        let recv_start = crate::counters::Stopwatch::start();
        let item_opt = if drain_disconnected {
            None
        } else {
            match drain_rx.recv_timeout(recv_timeout) {
                Ok(DrainMessage::Item(item)) => Some(item),
                Ok(DrainMessage::NodeBoundary { node_end_seq: end }) => {
                    #[cfg(pbfhogg_test_hooks)]
                    cancel
                        .faults()
                        .on_apply_changes_boundary(end, state.next_seq);
                    if node_end_seq.replace(end).is_some() {
                        return Err(Error::internal(
                            "drain: the scanner sent its node boundary twice",
                        ));
                    }
                    None
                }
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    drain_disconnected = true;
                    None
                }
            }
        };
        let wait_ns = recv_start.elapsed_ns();

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

        // Advance through contiguous seqs.
        while let Some(item) = state.buffer.remove(&state.next_seq) {
            state.bytes_in_buffer = state.bytes_in_buffer.saturating_sub(item.byte_cost());
            process_item(item, &cfg, &mut state, writer, &mut stats, counters)?;
            state.next_seq += 1;
            // After processing, so an item blocked handing output to the
            // writer still counts against the window.
            window.release();
        }
        #[cfg(pbfhogg_test_hooks)]
        cancel
            .faults()
            .apply_changes_drain_next_seq
            .store(state.next_seq, std::sync::atomic::Ordering::SeqCst);

        // The one barrier check, after every receive and advance: once the
        // boundary is known and every seq below it is processed, the node
        // phase is complete on sorted input (a node after a way on unsorted
        // input falls past the boundary and is refused later by the
        // output-order guard). Checking after the contiguous run rather than
        // inside it is safe because, before publication, the scanner has
        // dispatched no way or relation descriptor, so the run holds only
        // node items (`process_item` refuses any other). It fires whichever comes
        // last, the final node item or the boundary itself: when the
        // boundary arrives after the drain has already processed every node
        // item, this check publishes on that receive, which is the case the
        // old separate boundary channel needed an idle re-poll for.
        //
        // Carrying the boundary on this shared channel was not given a
        // planet A/B, by argument (reference/performance/apply-changes.md, "Node boundary
        // on the drain channel, not measured"): every message ahead of it is
        // node work this check waits for anyway, and the scanner blocks on
        // the barrier right after sending it, so a full channel cannot
        // delay the scanner past its barrier wait.
        if matches!(state.loc, Some(DrainLoc::Pending(_)))
            && node_end_seq.is_some_and(|end| state.next_seq >= end)
            && let Some(DrainLoc::Pending(pending)) = state.loc.take()
        {
            // Taken and replaced in one step: a failed publication leaves
            // the drain through this `?`, never with the state left empty.
            state.loc = Some(DrainLoc::Published(pending.publish(
                &cfg.failure,
                counters,
                &cancel,
            )?));
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
    cancel.marker("MERGE_TRAILING_CREATES_START");
    #[cfg(pbfhogg_test_hooks)]
    cancel
        .faults()
        .apply_changes_drain_past_nodes
        .store(true, Ordering::SeqCst);
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
    let trailing_loc = state.loc.as_ref().map(DrainLoc::published_map);
    let loc = trailing_loc
        .as_ref()
        .map_or(LocMode::Off, |map| LocMode::on(map.as_deref()));
    for &kind in types_to_flush {
        let (cursor, upserts) = state.cursors.get(kind, cfg.diff);
        let run = &upserts[cursor..];
        // Trailing creates are a second emission channel: they do not
        // pass through `dispatch_item`, so without these calls the
        // sortedness guard would be blind to exactly the last thing
        // written - the same shape of trailing blindness the write
        // path's stream-completeness check had. A single ID is its own
        // one-element span. `upserts` is sorted with `osm_id_cmp`, so a
        // run of trailing creates never trips the guard on its own; a
        // create ordered before base output already emitted does. The
        // whole run is checked before any of it is encoded; a refusal fails
        // the run unpublished either way.
        for &id in run {
            let key = crate::osm_id::osm_id_key(id);
            check_output_order(&mut state, kind, key, key)?;
        }
        state.creates.emit(
            kind,
            cursor..upserts.len(),
            cfg.diff,
            loc,
            writer,
            &mut stats,
        )?;
        let (cursor, _) = state.cursors.get_mut(kind, cfg.diff);
        *cursor = upserts.len();
        counters
            .trailing_creates_emitted
            .fetch_add(run.len() as u64, Ordering::Relaxed);
    }
    cancel.marker("MERGE_TRAILING_CREATES_END");

    writer.flush()?;

    cancel.marker("MERGE_DRAIN_END");
    counters
        .seq_window_high_water
        .store(window.high_water(), Ordering::Relaxed);
    counters.emit_all(&cancel);

    Ok(stats)
}

/// Drain working state. Carved out of `run_drain` so `process_item` can
/// take a single `&mut DrainState` instead of half a dozen mut refs.
struct DrainState<'a> {
    /// Reorder buffer keyed by global seq.
    buffer: BTreeMap<u64, DrainItem>,
    /// Sum of `byte_cost()` across all items currently buffered.
    bytes_in_buffer: usize,
    /// Next seq the drain expects to process (monotonically increasing).
    next_seq: u64,
    /// Last item kind processed. Drives type transitions.
    last_type: Option<ElementKind>,
    cursors: UpsertCursors,
    /// Writes every create run: gap creates, the creates left at a type
    /// transition and the trailing creates. Rewritten blobs arrive framed
    /// and bypass it.
    creates: CreateEncoder<'a>,
    /// Coalesced contiguous `copy_file_range` runs. `Some((start, end))`
    /// means a run is in progress; flushed when broken.
    copy_range_run: Option<(u64, u64)>,
    /// Coalesced `OwnedBytes` passthrough chunks. Flushed as a single
    /// `write_raw_chunks` when broken.
    passthrough_chunks: Vec<Vec<u8>>,
    /// `--locations-on-ways` state, `None` with the mode off: pending until
    /// the node→way barrier, then the published map, which the drain's own
    /// gap, transition and trailing creates are written against (workers
    /// read it through their `LocMapHandle`).
    loc: Option<DrainLoc>,
    /// Running (kind, `osm_id_key`) high-water mark across every item
    /// already dispatched. Drives [`check_output_order`]; see there for
    /// why this is a running max rather than a pairwise-adjacent check.
    order_watermark: crate::osm_id::OrderWatermark,
}

impl<'a> DrainState<'a> {
    fn new(creates: CreateEncoder<'a>, loc: Option<DrainLoc>) -> Self {
        Self {
            buffer: BTreeMap::new(),
            bytes_in_buffer: 0,
            next_seq: 0,
            last_type: None,
            cursors: UpsertCursors::new(),
            creates,
            copy_range_run: None,
            passthrough_chunks: Vec::new(),
            loc,
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
/// The first bound compared is the blob's first element where a worker
/// decoded it, else the OSM-order first id its range proves present
/// (`descriptor::Placement::order_first_key`), so negative IDs (0, -1, -2,
/// ..., 1, 2, ...) order the way `osm_id_cmp` orders them everywhere else,
/// a valid mixed-sign blob after a negative one passes, and a decoded
/// mixed-sign blob whose first element sorts before the previous blob's
/// last is refused although its range alone cannot show it. The last bound
/// is the range's last key, which is the blob's last element: a decoded
/// blob's range has been verified exact. Equality is allowed through: a
/// shared boundary ID between two blobs is a duplicate-ID problem, not an
/// ordering one, and is not this guard's business.
fn check_output_order(
    state: &mut DrainState<'_>,
    kind: ElementKind,
    first: crate::osm_id::OsmIdKey,
    last: crate::osm_id::OsmIdKey,
) -> Result<()> {
    if let Err(v) = state
        .order_watermark
        .observe_keys(kind.ordinal(), first, last)
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
/// then emits any remaining upserts of the previous kind. The node→way
/// barrier is never published here: under `--locations-on-ways` it has
/// already fired before any way or relation item exists (see the one
/// barrier check in `run_drain`).
#[allow(clippy::too_many_arguments)]
fn handle_type_transition(
    prev: ElementKind,
    item_kind: ElementKind,
    cfg: &DrainConfig<'_>,
    state: &mut DrainState<'_>,
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

    let loc = loc_mode(state.loc.as_ref());
    writer
        .cancel_token()
        .marker("MERGE_TRANSITION_CREATES_START");
    let emitted = flush_remaining_upserts(
        prev,
        item_kind,
        cfg.diff,
        &mut state.cursors,
        &mut state.creates,
        writer,
        stats,
        loc,
    )?;
    writer.cancel_token().marker("MERGE_TRANSITION_CREATES_END");
    counters
        .transition_creates_emitted
        .fetch_add(emitted, Ordering::Relaxed);
    Ok(())
}

/// Emit gap creates for the current blob's kind: upserts whose ID
/// precedes `osm_first` in OSM order. Flushes coalescers first.
fn handle_gap_creates(
    item_kind: ElementKind,
    osm_first: i64,
    cfg: &DrainConfig<'_>,
    state: &mut DrainState<'_>,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    counters: &DrainCounters,
) -> Result<()> {
    if !has_gap_creates(item_kind, osm_first, cfg.diff, &state.cursors) {
        return Ok(());
    }
    flush_copy_range(state, cfg, writer, counters)?;
    flush_passthrough_buf(&mut state.passthrough_chunks, writer)?;
    let loc = loc_mode(state.loc.as_ref());
    emit_gap_creates(
        item_kind,
        osm_first,
        cfg.diff,
        &mut state.cursors,
        &mut state.creates,
        writer,
        stats,
        loc,
    )?;
    counters.gap_creates_emitted.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Process one in-order DrainItem. Handles type transitions, gap
/// creates, and per-variant dispatch.
#[allow(clippy::too_many_arguments)]
fn process_item(
    item: DrainItem,
    cfg: &DrainConfig<'_>,
    state: &mut DrainState<'_>,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    counters: &DrainCounters,
) -> Result<()> {
    counters.items_processed.fetch_add(1, Ordering::Relaxed);

    // An omitted blob writes nothing and has no kind, so it must not look
    // like a type transition or move the gap-create and order state.
    let Some(placement) = item.placement() else {
        return Ok(());
    };
    let item_kind = placement.kind;
    let (min_id, max_id) = placement.id_range;

    // Before the coordinate map is published only node items can reach the
    // drain: the scanner dispatches no way or relation descriptor until it
    // has the barrier signal, on both routes. A way or relation here would be
    // written with no map to resolve against, so the protocol is broken.
    if matches!(state.loc, Some(DrainLoc::Pending(_))) && item_kind != ElementKind::Node {
        return Err(Error::internal(format!(
            "drain: a {} item arrived before the node-to-way barrier",
            item_kind.as_str()
        )));
    }
    #[cfg(pbfhogg_test_hooks)]
    if item_kind != ElementKind::Node {
        writer
            .cancel_token()
            .faults()
            .apply_changes_drain_past_nodes
            .store(true, Ordering::SeqCst);
    }

    if let Some(prev) = state.last_type
        && prev != item_kind
    {
        handle_type_transition(prev, item_kind, cfg, state, writer, stats, counters)?;
    }
    state.last_type = Some(item_kind);

    // A reversed range names no real ID span. None is expected (an
    // unindexed blob with no element arrives as `DrainItem::Omitted`, and
    // `BlobIndex::deserialize` refuses an inverted range), but if one arrived, `blob_osm_first_id` would
    // return `i64::MAX` and `has_gap_creates` would emit every remaining
    // upsert of `item_kind` as a gap-create. Neither gap creates nor the
    // sortedness guard run on it; the dispatch below still writes the blob.
    if min_id <= max_id {
        check_output_order(
            state,
            item_kind,
            placement.order_first_key(),
            crate::osm_id::blob_osm_last_key(min_id, max_id),
        )?;
        handle_gap_creates(
            item_kind,
            placement.gap_bound(),
            cfg,
            state,
            writer,
            stats,
            counters,
        )?;
    }

    dispatch_variant(item, cfg, state, writer, stats, counters)
}

/// Per-variant dispatch (CopyRange / OwnedBytes / Rewritten). Mutates
/// `state` (coalescers, cursor) and `stats` (blob/byte counters).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn dispatch_variant(
    item: DrainItem,
    cfg: &DrainConfig<'_>,
    state: &mut DrainState<'_>,
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
            observed_first: _,
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
            // The element count is the indexdata `count` claim, folded into
            // the observed `base_*` totals. A worker's copy (a decoded false
            // positive or clean refresh scan, `observed_first` set) has had
            // that claim verified exact by its walk. The scanner's splice is
            // never decoded, so a false claim there moves the headline with
            // nothing to show it. Deliberate: claims are believed, not
            // verified, and counting here would cost the decode the splice
            // exists to skip (reference/indexdata-trust.md). `check --index`
            // verifies.
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
            observed_first: _,
            count,
            verbatim,
        } => {
            // Flush the copy_range coalescer so on-disk order is right.
            flush_copy_range(state, cfg, writer, counters)?;
            let frame_len = frame_bytes.len() as u64;
            state.passthrough_chunks.push(frame_bytes);
            // Bound the coalescer: an unchanged same-kind run would otherwise
            // be held whole until the next rewrite, gap create or kind change.
            // The batch holds at most the cap plus one frame, so the sum walks
            // a few dozen frames; output bytes do not depend on where it breaks.
            if state.passthrough_chunks.iter().map(Vec::len).sum::<usize>()
                >= crate::tunables::OWNED_PASSTHROUGH_FLUSH_BYTES
            {
                flush_passthrough_buf(&mut state.passthrough_chunks, writer)?;
                counters
                    .passthrough_chunks_flushed
                    .fetch_add(1, Ordering::Relaxed);
            }
            stats.bytes_passthrough += frame_len;
            stats.blobs_passthrough += 1;
            // The owned route's counterpart of a `CopyRange` is an index hit
            // as that is; a reframed unindexed blob is not, on either route.
            if verbatim {
                stats.blobs_index_hit += 1;
            }
            // Workers copy one validated original frame here. The same
            // 32 MiB body + 64 KiB header + 4-byte prefix bound applies;
            // this is not the accumulated passthrough buffer's length.
            #[allow(clippy::cast_possible_truncation)]
            stats.blob_sizes.push(frame_len as u32);
            // Credit per-kind base_* stats the same way the CopyRange arm
            // above does. Before this line `OwnedBytes` dropped per-kind
            // counts entirely, so any merge that routed passthroughs
            // through this arm (`--direct-io`, or a
            // false-positive on `Route::Owned`) reported
            // `base_nodes=base_ways=base_relations=0` for the
            // passthrough portion even though the output contained the
            // elements correctly. For an indexed blob `count` is the
            // indexdata claim, believed as in the CopyRange arm.
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
            observed_first: _,
            stats: per_blob_stats,
        } => {
            flush_copy_range(state, cfg, writer, counters)?;
            flush_passthrough_buf(&mut state.passthrough_chunks, writer)?;
            let mut rewrite_bytes: u64 = 0;
            // P1.5: workers already framed each chunk. Drain ships
            // them through `write_raw_owned` so the writer's
            // rayon-spawn-per-block path (and its
            // `pipeline_send_wait_ns` blowup) is bypassed entirely.
            // Each chunk's kind is its output block's; placement above and
            // the cursor use the input blob's `kind` and `id_range`.
            for (_, chunk) in framed_chunks {
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
            let (cursor, upserts) = state.cursors.get_mut(kind, cfg.diff);
            while *cursor < upserts.len() && crate::osm_id::osm_id_key(upserts[*cursor]) <= last {
                *cursor += 1;
            }
        }
        // `process_item` returns before dispatch for this variant.
        DrainItem::Omitted { .. } => {}
    }

    Ok(())
}

/// Flush the in-flight contiguous `copy_file_range` run as a single
/// `write_raw_copy(&input_file, start, len)` call. A run only exists on the
/// `Route::Splice` route, which `merge()` selects when the output backend can
/// splice.
fn flush_copy_range(
    state: &mut DrainState<'_>,
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

impl DrainLocations {
    /// Merge per-worker `CoordSlots` into the published `Arc<FxHashMap>`,
    /// then signal the scanner over `barrier_tx`, or refuse the run when the
    /// map leaves OSC way references unresolved. Called once under
    /// `--locations-on-ways`, from the barrier check in `run_drain`, once every
    /// seq below the scanner's node boundary is processed; it consumes the
    /// pending state, so it cannot run twice. On an error the barrier sender
    /// drops unsignalled with `self`, which stops a scanner waiting at the
    /// barrier, and the drain returns that error.
    fn publish(
        self,
        failure: &FailureSlot,
        counters: &DrainCounters,
        invocation: &crate::host::Recorder,
    ) -> Result<PublishedLocations> {
        invocation.marker("MERGE_DRAIN_BARRIER_START");

        let Self {
            coord_slots,
            seeded,
            barrier_tx,
            loc_map_out,
            nodes_needed,
            ignore_missing_nodes,
        } = self;

        #[cfg(pbfhogg_test_hooks)]
        invocation.faults().before_apply_changes_coord_merge();

        // Seed with OSC pre-seeded coords (from
        // `NodeLocationIndex::build_from_diff`'s `locations`) before
        // extending with per-worker accumulators. A worker slot recovered from
        // poison does not validate its contents: see `flush_local_coords` for
        // why a panicking worker still fails the run.
        //
        // Each slot's table is taken whole, leaving the slot an empty map
        // with no allocation, and consumed outside its lock, so its storage
        // is freed here. The workers keep the slots to the end of the run,
        // and a drained table would otherwise hold its node-phase capacity
        // (the worker's share of every kept coordinate) beside the published
        // map through the whole way phase. No coordinate is lost: every node
        // item flushed before it was sent, and only a node of a base lying
        // about its order can flush after this, which the order guard
        // refuses.
        let mut merged: LocMap = seeded;
        let mut freed_bytes = 0u64;
        for slot in &coord_slots {
            let local = std::mem::take(
                &mut *slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            if merged.is_empty() {
                merged = local;
            } else {
                freed_bytes += super::streaming::map_table_bytes(&local) as u64;
                merged.extend(local);
            }
        }
        counters
            .barrier_slot_bytes_freed
            .store(freed_bytes, Ordering::Relaxed);

        counters
            .barrier_loc_map_size
            .store(merged.len() as u64, Ordering::Relaxed);

        // The missing-node refusal, at the earliest point the count is known:
        // every node item is processed, nothing resolves after this merge, and
        // no way has been written. Leaving through the drain's error exit drops
        // the barrier sender unsignalled, so the scanner stops in its barrier
        // wait (at the first way or relation, or at EOF), the workers are
        // released, and `merge`
        // publishes nothing. A producer failure already published, or a
        // cancellation already requested, outranks the refusal: both are
        // checked here, at the decision, since the drain's own checks ran
        // before this receive's advance and merge.
        let missing = nodes_needed.saturating_sub(merged.len() as u64);
        if missing != 0 && !ignore_missing_nodes {
            // Test-only: a producer failure or a cancellation landing after the
            // drain's per-receive checks, which the checks below must rank.
            #[cfg(pbfhogg_test_hooks)]
            {
                let faults = invocation.faults();
                if faults
                    .apply_changes_fail_at_refusal
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    failure.publish(Error::internal(
                        "test-hooks: producer failure at the barrier refusal",
                    ));
                }
                if faults
                    .apply_changes_cancel_at_refusal
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    invocation.cancel();
                }
            }
            if let Some(e) = failure.take() {
                return Err(e);
            }
            invocation.check()?;
            invocation.counter("merge_loc_missing", missing);
            invocation.marker("MERGE_DRAIN_BARRIER_REFUSED");
            return Err(crate::error::new_missing_node_locations(
                missing,
                crate::error::Unresolved::ChangedWayNodes,
            ));
        }

        let map = Arc::new(merged);
        // Only this consuming call sets the handle, and only once; another
        // value there means something outside the barrier published a map.
        if loc_map_out.set(Arc::clone(&map)).is_err() {
            return Err(Error::internal(
                "drain: the coordinate map was published before the barrier",
            ));
        }

        #[cfg(pbfhogg_test_hooks)]
        invocation.faults().before_apply_changes_barrier_send();
        // A scanner that succeeds always receives this before exiting, small
        // inputs included: it blocks on it at its first way or relation
        // descriptor, or at end of input when there is none. A closed
        // receiver therefore means the scanner exited or unwound
        // early, and that cause is reported through the failure slot, so the
        // send error itself carries nothing to report.
        if let Err(e) = barrier_tx.send(()) {
            let _: mpsc::SendError<()> = e;
        }
        #[cfg(pbfhogg_test_hooks)]
        invocation
            .faults()
            .apply_changes_barrier_signalled
            .store(true, Ordering::SeqCst);

        invocation.marker("MERGE_DRAIN_BARRIER_END");
        Ok(PublishedLocations {
            map,
            _barrier_tx: barrier_tx,
        })
    }
}
