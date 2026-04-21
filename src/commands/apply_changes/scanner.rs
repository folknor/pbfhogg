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
//!   [`super::descriptor::WorkerOutput::OwnedPassthrough`]).
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
//!
//! Plan doc: `notes/apply-changes-opportunities.md`, "Synthesized design"
//! section and "Node→way barrier ownership" block.

#![allow(dead_code)]

use std::path::Path;
use std::sync::mpsc;

use crate::blob::BlobKind;
use crate::blob_meta::ElemKind;
use crate::error::Result;
use crate::read::header_walker::HeaderWalker;

use super::descriptor::{BlobDescriptor, DrainItem, ScannedBlob};
use super::diff_ranges::DiffRanges;

/// Scanner channel configuration. Which channels are populated depends
/// on the output backend: `drain_tx` (splice-capable backends) receives
/// fast-path `DrainItem::CopyRange` directly; under `--direct-io` every
/// passthrough descriptor routes through `candidate_tx` to a pread
/// helper instead.
pub(super) struct ScannerChannels {
    /// Candidate descriptors (overlap candidates requiring worker
    /// decompress + precise check, plus passthrough descriptors under
    /// `--direct-io` where workers pread the full frame).
    pub candidate_tx: mpsc::SyncSender<ScannedBlob>,
    /// Fast-path drain items for splice-capable backends. `None` under
    /// `--direct-io` output; all passthroughs route through
    /// `candidate_tx` in that case.
    pub drain_tx: Option<mpsc::SyncSender<DrainItem>>,
}

/// Scanner configuration + I/O channels.
pub(super) struct ScannerConfig<'a> {
    pub base_pbf: &'a Path,
    pub ranges: &'a DiffRanges,
    /// True when the output backend supports `copy_file_range` (buffered
    /// and io_uring paths). Under `--direct-io`, false, and all
    /// passthrough routing goes through `candidate_tx`.
    pub use_copy_range: bool,
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
}

/// Run the scanner to completion. Returns the number of OsmData
/// descriptors emitted (matching what the drain should ultimately see).
///
/// On error, closes its output channels by returning - workers and drain
/// will see `Disconnected` on their reads and exit.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn run_scanner(cfg: ScannerConfig<'_>) -> Result<u64> {
    // Destructure so clippy sees us taking ownership of the channels,
    // which matters: the SyncSenders dropping at scanner exit is what
    // signals end-of-input to the worker pool and drain.
    let ScannerConfig {
        base_pbf,
        ranges,
        use_copy_range,
        locations_on_ways,
        channels,
        barrier_rx,
    } = cfg;

    crate::debug::emit_marker("MERGE_SCANNER_START");

    let mut walker = HeaderWalker::open(base_pbf)?;
    let mut seq: u64 = 0;
    let mut pending_post_barrier: Vec<ScannerEmit> = Vec::new();
    let mut barrier_open = !locations_on_ways; // open by default when not --locations-on-ways
    let mut bytes_high_water_fastpath: usize = 0;
    let mut bytes_high_water_slowpath: usize = 0;

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
            None => (ElemKind::Node, None, false), // kind placeholder - workers correct it
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
        let is_fastpath = has_indexdata
            && use_copy_range
            && !ranges.range_overlaps(
                kind,
                id_range.map_or(0, |r| r.0),
                id_range.map_or(0, |r| r.1),
            );

        let item = if is_fastpath {
            // Convert directly to a DrainItem for the splice channel.
            // `into_drain_copy_range` requires indexdata, which we just
            // verified.
            ScannerEmit::Drain(
                descriptor
                    .into_drain_copy_range()
                    .ok_or_else(|| {
                        crate::error::new_error(crate::error::ErrorKind::Io(
                            std::io::Error::other(
                                "scanner: fast-path descriptor missing indexdata - \
                                 should be unreachable",
                            ),
                        ))
                    })?,
            )
        } else if has_indexdata
            && !ranges.range_overlaps(
                kind,
                id_range.map_or(0, |r| r.0),
                id_range.map_or(0, |r| r.1),
            )
        {
            // No-overlap indexed blob under --direct-io: route through
            // the worker pool so a worker can pread the full frame and
            // emit `WorkerOutput::OwnedPassthrough`.
            ScannerEmit::Candidate(ScannedBlob::Passthrough(descriptor))
        } else {
            ScannerEmit::Candidate(ScannedBlob::Candidate(descriptor))
        };

        // Track rough bytes in flight for the high-water counters that
        // the plan doc's "Trust measurement, not estimates" section
        // calls out.
        let approx_cost = scanner_emit_cost(&item);

        // Node→way barrier. Under --locations-on-ways, way/relation
        // descriptors are buffered once seen until the drain signals
        // loc_map ready. Node descriptors always emit immediately.
        if !barrier_open && matches!(kind, ElemKind::Way | ElemKind::Relation) {
            pending_post_barrier.push(item);
            seq += 1;
            continue;
        }

        dispatch_item(
            &channels,
            item,
            &mut bytes_high_water_fastpath,
            &mut bytes_high_water_slowpath,
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

    // End of input. If we buffered way/relation descriptors under the
    // barrier, wait for the drain's signal now (all node descriptors
    // are emitted; drain can finish node-kind processing and publish
    // loc_map).
    if !barrier_open
        && let Some(ref rx) = barrier_rx
    {
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
            &mut bytes_high_water_fastpath,
            &mut bytes_high_water_slowpath,
            approx_cost,
        )?;
    }

    crate::debug::emit_marker("MERGE_SCANNER_END");
    crate::debug::emit_counter(
        "merge_scanner_blobs_emitted",
        i64::try_from(seq).unwrap_or(i64::MAX),
    );
    crate::debug::emit_counter(
        "merge_scanner_to_drain_bytes_high_water",
        i64::try_from(bytes_high_water_fastpath).unwrap_or(i64::MAX),
    );
    crate::debug::emit_counter(
        "merge_scanner_to_workers_bytes_high_water",
        i64::try_from(bytes_high_water_slowpath).unwrap_or(i64::MAX),
    );

    Ok(seq)
}

/// What the scanner is about to emit. Splits the dispatch shape from
/// the routing shape so `dispatch_item` doesn't need to re-derive
/// `is_fastpath` from the variant.
enum ScannerEmit {
    /// Splice-capable fast-path: send `DrainItem::CopyRange` straight to
    /// the drain.
    Drain(DrainItem),
    /// Slow-path or `--direct-io` passthrough: route through the worker
    /// pool as a `ScannedBlob`.
    Candidate(ScannedBlob),
}

/// Dispatch a single `ScannerEmit` to the correct channel.
fn dispatch_item(
    channels: &ScannerChannels,
    item: ScannerEmit,
    bytes_hw_fast: &mut usize,
    bytes_hw_slow: &mut usize,
    approx_cost: usize,
) -> Result<()> {
    match item {
        ScannerEmit::Drain(drain_item) => {
            let tx = channels.drain_tx.as_ref().ok_or_else(|| {
                crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(
                    "scanner: fast-path emitted but drain_tx is None - misconfigured channels",
                )))
            })?;
            *bytes_hw_fast = (*bytes_hw_fast).max(approx_cost);
            tx.send(drain_item).map_err(send_err)?;
        }
        ScannerEmit::Candidate(scanned) => {
            *bytes_hw_slow = (*bytes_hw_slow).max(approx_cost);
            channels.candidate_tx.send(scanned).map_err(send_err)?;
        }
    }
    Ok(())
}

/// Rough byte cost of a `ScannerEmit` for high-water accounting.
fn scanner_emit_cost(item: &ScannerEmit) -> usize {
    const DESCRIPTOR_OVERHEAD: usize = 64;
    match item {
        ScannerEmit::Drain(d) => d.byte_cost(),
        ScannerEmit::Candidate(ScannedBlob::Passthrough(d) | ScannedBlob::Candidate(d)) => {
            DESCRIPTOR_OVERHEAD + d.tagdata.as_ref().map_or(0, |t| t.len())
        }
    }
}

fn send_err<T>(_: mpsc::SendError<T>) -> crate::error::Error {
    // Receiver gone - treat as a graceful shutdown from downstream.
    // The caller (merge) will still observe the scanner's return Ok,
    // because a closed receiver means downstream decided to stop. Map
    // to an Io error only when we need to propagate.
    crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(
        "scanner dispatch channel closed by downstream",
    )))
}
