//! Descriptor-first scanner: `HeaderWalker`-driven blob metadata emission.
//!
//! The scanner opens the base PBF via `HeaderWalker`, walks blob headers
//! in file order, and emits one [`BlobDescriptor`] per OsmData blob to
//! one of two channels based on the overlap check:
//!
//! - **No overlap + indexed**: fast-path. Routed as
//!   [`ScannedBlob::Passthrough`] either directly to the drain (when the
//!   output backend supports `copy_file_range`) or to the worker pool
//!   (under `--direct-io`, workers pread the full frame and emit
//!   `DrainItem::OwnedBytes`).
//! - **Overlap candidate / unindexed**: routed as
//!   [`ScannedBlob::Candidate`] to the worker pool for decompress +
//!   precise check + rewrite.
//!
//! ## Node→way barrier under `--locations-on-ways`
//!
//! Workers extract coords from node blobs opportunistically during the
//! node phase. The drain merges per-worker coord maps into `Arc<loc_map>`
//! at the node→way boundary and signals the scanner via the barrier
//! channel. The scanner then begins dispatching buffered way/relation
//! descriptors. Barrier ownership is **scanner-side** (not drain-side) so
//! that no way worker can start classify concurrent with a still-in-flight
//! node worker.

use std::path::Path;
use std::sync::mpsc;

use crate::ElementKind;
use crate::blob::BlobKind;
use crate::error::Result;
use crate::read::header_walker::HeaderWalker;

use super::descriptor::{BlobDescriptor, DrainItem, Route, ScannedBlob};
use super::diff_ranges::DiffRanges;

/// Scanner channel configuration. Which channels are populated depends
/// on the output backend: on the `Route::Splice` route (splice-capable
/// backends) the drain sender receives fast-path `DrainItem::CopyRange`
/// directly; on `Route::Owned` (`--direct-io`) every passthrough descriptor
/// routes through `candidate_tx` to a pread helper instead.
pub(super) struct ScannerChannels {
    /// Candidate descriptors (overlap candidates requiring worker
    /// decompress + precise check, plus passthrough descriptors under
    /// `--direct-io` where workers pread the full frame).
    pub candidate_tx: mpsc::SyncSender<ScannedBlob>,
    /// Where untouched blobs go. `Splice` carries the fast-path drain
    /// sender; `Owned` has none, and all passthroughs route through
    /// `candidate_tx`.
    pub route: Route<mpsc::SyncSender<DrainItem>>,
}

/// Scanner configuration + I/O channels.
pub(super) struct ScannerConfig<'a> {
    pub base_pbf: &'a Path,
    pub ranges: &'a DiffRanges,
    /// True when `--locations-on-ways` is active. Enables the node→way
    /// barrier: scanner buffers way/relation descriptors after the first
    /// way descriptor is seen, waits on `barrier_rx` for the drain's
    /// signal that `Arc<loc_map>` is published, then drains the buffer.
    pub locations_on_ways: bool,
    pub channels: ScannerChannels,
    /// Drain → scanner signal. Present when `locations_on_ways` is true.
    /// Drain sends one unit value after it has processed the last node
    /// blob and published the merged `Arc<loc_map>`.
    pub barrier_rx: Option<mpsc::Receiver<()>>,
    /// Scanner → drain signal carrying the seq of the last node blob
    /// emitted (or `u64::MAX` if no node blobs were ever emitted, which
    /// the drain treats as "publish now"). Sent at the first non-node
    /// descriptor or at EOF, so the drain knows when to merge
    /// `CoordSlots` and publish `Arc<loc_map>` without needing a way
    /// item to trigger the transition. Present when `locations_on_ways`
    /// is true.
    pub last_node_seq_tx: Option<mpsc::SyncSender<u64>>,
}

/// Run the scanner to completion. Returns the number of OsmData
/// descriptors emitted (matching what the drain should ultimately see).
///
/// On error, closes its output channels by returning - workers and drain
/// will see `Disconnected` on their reads and exit.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn run_scanner(cfg: ScannerConfig<'_>, host: &crate::host::Host) -> Result<u64> {
    // Destructure so clippy sees us taking ownership of the channels,
    // which matters: the SyncSenders dropping at scanner exit is what
    // signals end-of-input to the worker pool and drain.
    let ScannerConfig {
        base_pbf,
        ranges,
        locations_on_ways,
        channels,
        barrier_rx,
        last_node_seq_tx,
    } = cfg;
    // The fast-path sender, present exactly on the splice route. Borrowed by
    // each `ScannerEmit::Drain` so an item can only be built where there is a
    // drain channel to send it on.
    let splice_tx = match &channels.route {
        Route::Splice(drain_tx) => Some(drain_tx),
        Route::Owned => None,
    };

    crate::debug::emit_marker("MERGE_SCANNER_START");

    let mut walker = HeaderWalker::open(base_pbf, &host.diagnostics)?;
    let mut seq: u64 = 0;
    // Intentionally unbounded under --locations-on-ways: the non-node
    // branch below appends and continues without polling barrier_rx. On a
    // sorted base it therefore retains every way/relation descriptor until
    // EOF, even if the drain published the map earlier. This is metadata,
    // not blob bodies, but each entry can retain its own tagdata allocation;
    // neither bounded output channel nor drain reorder telemetry bounds it.
    //
    // A local vector cap is not a safe independent change: returning early
    // rejects otherwise valid large inputs, while waiting or releasing the
    // tail changes scanner/barrier scheduling. A bounded design should wait
    // at the first non-node descriptor after announcing last_node_seq, with
    // the drain's late-boundary idle-fire path and producer-failure shutdown
    // preserved and validated. That needs an explicit barrier change; do not
    // disguise it as a channel-capacity or telemetry adjustment here.
    let mut pending_post_barrier: Vec<ScannerEmit<'_>> = Vec::new();
    let mut barrier_open = !locations_on_ways; // open by default when not --locations-on-ways
    let mut max_item_bytes_fastpath: usize = 0;
    let mut max_item_bytes_slowpath: usize = 0;
    // Tracks the seq of the most recently emitted Node descriptor.
    // Used to send `last_node_seq` to the drain at the node→way
    // transition (or at EOF if no way blobs exist).
    let mut last_node_seq: Option<u64> = None;
    let mut last_node_seq_sent = false;

    while let Some(meta) = walker.next_header()? {
        // Skip OsmHeader - merge() reads it separately during setup.
        if meta.blob_type != BlobKind::OsmData {
            continue;
        }

        // Derive element kind and id_range from indexdata when present.
        // Under --force (no indexdata), route unconditionally to the
        // worker pool; workers will fill in kind + id_range after
        // decompress + scan.
        let (kind, id_range, has_indexdata) = match meta.index.as_ref() {
            Some(idx) => (idx.kind, Some((idx.min_id, idx.max_id)), true),
            None => (ElementKind::Node, None, false), // kind placeholder - workers correct it
        };

        let blob_offset = usize::try_from(meta.data_offset - meta.frame_start).map_err(|_| {
            crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(
                "blob header too large to fit in usize - malformed PBF?",
            )))
        })?;

        let descriptor = BlobDescriptor {
            seq,
            frame_start: meta.frame_start,
            frame_len: meta.frame_size,
            blob_offset,
            data_size: meta.data_size,
            kind,
            id_range,
            index: meta.index,
            tagdata: meta.tagdata,
        };

        // Routing decision: fast-path only available when we have
        // indexdata AND the range doesn't overlap the diff. Otherwise
        // slow-path. Under `--direct-io`, the splice fast-path is
        // unavailable and passthroughs are routed through workers.
        //
        // Under `--locations-on-ways`, Node blobs CANNOT take the
        // splice fast-path: workers must decompress them to extract
        // coord tuples for the barrier merge, even when the blob
        // doesn't overlap the diff. Way/Relation blobs are still
        // eligible for the splice fast-path under LOW.
        //
        // "Node" here is an indexdata claim, not an observed payload kind.
        // A node blob mislabelled Way/Relation can bypass extraction (via
        // CopyRange or owned passthrough) and is placed after the coordinate
        // barrier. Pending location mode only guards rewritten ways; it
        // cannot validate these routes. Checking just candidate workers
        // would leave both passthrough routes unchecked. Rejecting dishonest
        // kinds requires payload verification before scanner routing and
        // barrier placement, including non-overlapping way/relation claims.
        // Keep that a deliberate indexdata trust-policy change: the measured
        // descriptor-first path avoids reading/decompressing those bodies,
        // and kind verification alone would still trust forged ID ranges.
        let low_node_must_decompress = locations_on_ways && matches!(kind, ElementKind::Node);
        let item = if let Some(drain_tx) = splice_tx
            && has_indexdata
            && !low_node_must_decompress
            && !ranges.range_overlaps(
                kind,
                id_range.map_or(0, |r| r.0),
                id_range.map_or(0, |r| r.1),
            ) {
            // Convert directly to a DrainItem for the splice channel.
            // `into_drain_copy_range` requires indexdata, which we just
            // verified.
            ScannerEmit::Drain(
                drain_tx,
                descriptor.into_drain_copy_range().ok_or_else(|| {
                    crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(
                        "scanner: fast-path descriptor missing indexdata - \
                                 should be unreachable",
                    )))
                })?,
            )
        } else if !low_node_must_decompress
            && has_indexdata
            && !ranges.range_overlaps(
                kind,
                id_range.map_or(0, |r| r.0),
                id_range.map_or(0, |r| r.1),
            )
        {
            // No-overlap indexed blob under --direct-io: route through
            // the worker pool so a worker can pread the full frame and
            // emit `DrainItem::OwnedBytes`. Skipped for Node
            // blobs under LOW - those route as `Candidate` below so
            // workers decompress and extract coords.
            ScannerEmit::Candidate(ScannedBlob::Passthrough(descriptor))
        } else {
            ScannerEmit::Candidate(ScannedBlob::Candidate(descriptor))
        };

        // Track the largest descriptor estimate, not channel occupancy.
        let approx_cost = scanner_emit_cost(&item);

        // Node→way barrier. Under --locations-on-ways, way/relation
        // descriptors are buffered once seen until the drain signals
        // loc_map ready. Node descriptors always emit immediately.
        if !barrier_open && matches!(kind, ElementKind::Way | ElementKind::Relation) {
            // First non-Node descriptor: tell the drain how many Node
            // outputs to expect before it merges CoordSlots and signals
            // back. Send happens before the buffer push so the drain
            // can pre-receive even while workers are still processing
            // queued Node candidates.
            if !last_node_seq_sent && let Some(tx) = last_node_seq_tx.as_ref() {
                let payload = last_node_seq.unwrap_or(u64::MAX);
                tx.send(payload).map_err(send_err)?;
                last_node_seq_sent = true;
            }
            pending_post_barrier.push(item);
            seq += 1;
            continue;
        }

        // Track the seq of every dispatched Node descriptor (including
        // those routed through the worker pool and those emitted on the
        // splice fast-path) so we can report `last_node_seq` to the drain.
        if matches!(kind, ElementKind::Node) {
            last_node_seq = Some(seq);
        }

        dispatch_item(
            &channels,
            item,
            &mut max_item_bytes_fastpath,
            &mut max_item_bytes_slowpath,
            approx_cost,
        )?;

        seq += 1;

        // Opportunistic barrier check: if the barrier is still closed
        // and we've just emitted a node, peek for the drain's signal
        // without blocking. The actual wait happens at first Way/Relation.
        if !barrier_open
            && let Some(ref rx) = barrier_rx
            && let Ok(()) = rx.try_recv()
        {
            barrier_open = true;
        }
    }

    // End of input. If --locations-on-ways and we never sent
    // last_node_seq (i.e., the file had no Way/Relation blobs at all),
    // tell the drain now so it can publish `loc_map` for the trailing-
    // creates path. The drain signals back via barrier_rx.
    if locations_on_ways
        && !last_node_seq_sent
        && let Some(tx) = last_node_seq_tx.as_ref()
    {
        let payload = last_node_seq.unwrap_or(u64::MAX);
        tx.send(payload).map_err(send_err)?;
    }

    // End of input. If we buffered way/relation descriptors under the
    // barrier, wait for the drain's signal now (all node descriptors
    // are emitted; drain can finish node-kind processing and publish
    // loc_map).
    //
    // At EOF with zero way/relation blobs this blocks on barrier_rx
    // unconditionally; the drain's idle-fire path (see drain.rs's
    // "Idle barrier fire" block) is the designed counterpart that
    // unblocks us when the drain has no buffered items but the
    // barrier condition is met. Do not remove the idle-fire without
    // also rewriting this wait.
    if !barrier_open && let Some(ref rx) = barrier_rx {
        crate::debug::emit_marker("MERGE_SCANNER_BARRIER_WAIT_START");
        rx.recv().map_err(|_| {
            crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(
                "drain closed barrier channel before signalling loc_map ready",
            )))
        })?;
        crate::debug::emit_marker("MERGE_SCANNER_BARRIER_WAIT_END");
    }

    // Drain the pending buffer in seq order.
    for item in pending_post_barrier.drain(..) {
        let approx_cost = scanner_emit_cost(&item);
        dispatch_item(
            &channels,
            item,
            &mut max_item_bytes_fastpath,
            &mut max_item_bytes_slowpath,
            approx_cost,
        )?;
    }

    crate::debug::emit_marker("MERGE_SCANNER_END");
    crate::debug::emit_counter(
        "merge_scanner_blobs_emitted",
        i64::try_from(seq).unwrap_or(i64::MAX),
    );
    // Deliberately retain the historical counter names and emission semantics.
    // Renaming these estimates breaks brokkr comparisons with measured runs.
    crate::debug::emit_counter(
        "merge_scanner_to_drain_bytes_high_water",
        i64::try_from(max_item_bytes_fastpath).unwrap_or(i64::MAX),
    );
    crate::debug::emit_counter(
        "merge_scanner_to_workers_bytes_high_water",
        i64::try_from(max_item_bytes_slowpath).unwrap_or(i64::MAX),
    );

    Ok(seq)
}

/// What the scanner is about to emit. Splits the dispatch shape from
/// the routing shape so `dispatch_item` doesn't need to re-derive
/// `is_fastpath` from the variant.
enum ScannerEmit<'a> {
    /// Splice-capable fast-path: send `DrainItem::CopyRange` straight to
    /// the drain over the borrowed splice-route sender.
    Drain(&'a mpsc::SyncSender<DrainItem>, DrainItem),
    /// Slow-path or `--direct-io` passthrough: route through the worker
    /// pool as a `ScannedBlob`.
    Candidate(ScannedBlob),
}

/// Dispatch a single `ScannerEmit` to the correct channel.
fn dispatch_item(
    channels: &ScannerChannels,
    item: ScannerEmit<'_>,
    max_item_bytes_fast: &mut usize,
    max_item_bytes_slow: &mut usize,
    approx_cost: usize,
) -> Result<()> {
    match item {
        ScannerEmit::Drain(tx, drain_item) => {
            *max_item_bytes_fast = (*max_item_bytes_fast).max(approx_cost);
            tx.send(drain_item).map_err(send_err)?;
        }
        ScannerEmit::Candidate(scanned) => {
            *max_item_bytes_slow = (*max_item_bytes_slow).max(approx_cost);
            channels.candidate_tx.send(scanned).map_err(send_err)?;
        }
    }
    Ok(())
}

/// Rough byte cost of a `ScannerEmit` for high-water accounting.
fn scanner_emit_cost(item: &ScannerEmit<'_>) -> usize {
    use crate::tunables::DESCRIPTOR_OVERHEAD;
    match item {
        ScannerEmit::Drain(_, d) => d.byte_cost(),
        ScannerEmit::Candidate(ScannedBlob::Passthrough(d) | ScannedBlob::Candidate(d)) => {
            DESCRIPTOR_OVERHEAD + d.tagdata.as_ref().map_or(0, |t| t.len())
        }
    }
}

fn send_err<T>(_: mpsc::SendError<T>) -> crate::error::Error {
    // Return a channel-closure error so merge can propagate it if the
    // drain has not already returned its own error.
    crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(
        "scanner dispatch channel closed by downstream",
    )))
}
