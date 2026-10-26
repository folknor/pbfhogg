//! Apply-changes entry point: build the descriptor-first streaming
//! pipeline, join scanner and workers with the drain, and surface stats.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use rustc_hash::FxHashMap;

use crate::write::WriteOptions;

use crate::commands::input_front::InputFront;
use crate::commands::output_header::{Bbox, OutputFacts, Replication, Ways};
use crate::commands::{HeaderOverrides, writer_from_header_bytes};
use crate::read::input::PinnedInput;

use super::descriptor::{DrainMessage, Route};
use super::diff_ranges::DiffRanges;
use super::drain::{
    self, DrainChannels, DrainConfig, DrainCounters, LocMapHandle, SeededLocations,
};
use super::failure::FailureSlot;
use super::node_locations::NodeLocationIndex;
use super::scanner::{self, ScannerChannels, ScannerConfig};
use super::stats::MergeStats;
use super::streaming::{self, CoordSlots, StreamingChannels, StreamingConfig, WorkerCounters};

use super::Result;

use crate::tunables::{
    APPLY_CHANGES_WORKERS_FALLBACK, APPLY_CHANGES_WORKERS_MIN, APPLY_CHANGES_WORKERS_RESERVED,
    CANDIDATE_CHANNEL_CAPACITY, DRAIN_CHANNEL_CAPACITY,
};

/// Public command options.
#[derive(Default)]
pub struct MergeOptions {
    /// Output compression, backend and the shared input flags. The parallel
    /// writer is this command's default backend, so a `Buffered` backend
    /// here builds `PbfWriter::to_path_parallel`.
    pub write: WriteOptions,
    pub locations_on_ways: bool,
    /// Permit unresolved way-node references to retain the legacy `(0, 0)`
    /// fallback under `--locations-on-ways`.
    pub ignore_missing_nodes: bool,
    /// Worker-pool size for the descriptor-first pipeline. `Auto` keeps the
    /// `nproc - 2` heuristic (leaves two cores for scanner + drain, minimum
    /// 2 workers). `Fixed(n)` runs exactly `n`, one included: the scanner,
    /// the workers and the drain are separate threads, and nothing in the
    /// node-to-way barrier or its failure shutdown depends on a second
    /// worker (the barrier fault tests run at one worker too).
    pub parallelism: crate::host::Parallelism,
    /// Accept a change whose `version` is lower than the version already
    /// held for that object in the overlay. Off by default; see
    /// [`crate::osc::CompactDiffOverlay::allow_version_regression`].
    pub allow_version_regression: bool,
}

impl MergeOptions {
    /// Refuse option values and combinations the pipeline cannot honour,
    /// before any input is opened. [`merge`] calls it first; a caller can
    /// call it earlier to refuse before reserving an output.
    ///
    /// - `force` with `locations_on_ways`: on an indexed base `force` does
    ///   nothing, and on an unindexed one the combination is unsound (the
    ///   scanner cannot tell node blobs from way blobs, which defeats the
    ///   node-to-way barrier), so `force` can never take effect there.
    ///
    /// # Errors
    ///
    /// `InvalidArgument` naming the refused option.
    pub fn validate(&self) -> Result<()> {
        if self.write.force && self.locations_on_ways {
            return Err(crate::Error::invalid_argument(
                "apply-changes: force has no effect with locations_on_ways, which \
                 requires an indexed base (an unindexed base would silently lose \
                 LocationsOnWays on base ways)",
            ));
        }
        Ok(())
    }
}

fn build_header_bytes(
    header: &crate::HeaderBlock,
    locations_on_ways: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<Vec<u8>> {
    // apply-changes requires Sort.Type_then_ID on the base PBF
    // regardless of --locations-on-ways: the upsert slicing in
    // `streaming::upsert_slice` computes per-block `osm_id_key`
    // bounds that assume canonical ordering, and
    // `rewrite_block.rs::rewrite_block` advances its `upsert_cursor`
    // using `osm_id_cmp` against the iterating element, which only
    // produces correct output if elements arrive in canonical order.
    // Previously the check only fired for --locations-on-ways; the
    // general path silently dropped creates on malformed unsorted
    // input.
    if !header.is_sorted() {
        return Err(crate::error::new_unsorted(
            "apply-changes requires a sorted base PBF (Sort.Type_then_ID). \
             All nodes must precede all ways, and elements within a kind \
             must be ordered by ID.",
        ));
    }
    if locations_on_ways && !header.has_locations_on_ways() {
        return Err(crate::Error::invalid_data(
            "apply-changes with locations_on_ways requires the base PBF to have LocationsOnWays. \
             Run add-locations-to-ways first to bootstrap coordinates.",
        ));
    }
    // Sorted by construction. Under -l every way keeps or gets inline
    // coordinates; the prepass fields of spliced blobs are stale against the
    // changed relations and ways, so they are not declared. Without -l a
    // rewritten way blob loses its coordinates. An OSC can create or move a
    // node outside the base's bbox, so no bbox is declared. The output is
    // past the base's replication position, at one the OSC cannot name (it
    // has no sequence number, nor proof it follows the base without a gap),
    // so only the feed's base URL is kept; the caller stamps the new
    // position with `HeaderOverrides` when it knows it. osmium apply-changes
    // drops all three fields; keeping the URL keeps pyosmium-up-to-date and
    // osm2pgsql-replication on the base's feed instead of their planet
    // default when they fall back to the newest object timestamp.
    let facts = OutputFacts {
        bbox: Bbox::Drop,
        replication: Replication::FeedOnly,
        ..OutputFacts::sorted(if locations_on_ways {
            Ways::ReencodedWithLocations
        } else {
            Ways::Reencoded
        })
    };
    Ok(crate::commands::output_header::build_one(header, &facts, overrides, host)?.bytes)
}

/// Apply an OSC diff to a base PBF, producing an updated sorted PBF.
///
/// Descriptor-first streaming pipeline: scanner walks blob headers via
/// `HeaderWalker` and emits `DrainItem::CopyRange` (splice fast-path)
/// or `ScannedBlob::Candidate` (overlap candidates, routed to workers).
/// Workers pread bodies, decompress, precise-check, and either rewrite
/// inline (`DrainItem::Rewritten`) or convert false positives to
/// `DrainItem::CopyRange`. The drain reorders the unified stream by seq
/// and writes the output PBF, coalescing contiguous `copy_file_range`
/// runs.
///
/// Under `--locations-on-ways`, workers extract node coords during the
/// node phase into per-worker `Arc<Mutex<FxHashMap>>` slots; the drain
/// merges them at the node→way barrier and publishes the
/// `Arc<FxHashMap>` for way-phase workers via `LocMapHandle`.
///
/// # Errors
///
/// Returns an error if the base PBF or OSC file cannot be read, the
/// output path aliases the base PBF, the output file cannot be written, or
/// any PBF parsing/encoding fails. The base is read in several passes, so
/// it must be a regular file (`InvalidArgument` otherwise), and is pinned for the run (see
/// [`PinnedInput`](crate::read::input::PinnedInput)): where a later open
/// cannot reach the pinned file, a base replaced mid-run fails with
/// `InputReplaced`.
pub fn merge(
    base_pbf: &Path,
    osc_file: &Path,
    output_pbf: crate::output::Output,
    opts: &MergeOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<MergeStats> {
    host.cancel.check()?;
    let result = merge_to(base_pbf, osc_file, &output_pbf, opts, overrides, host)?;
    output_pbf.publish(&host.cancel)?;
    Ok(result)
}

/// [`merge`] writing to a borrowed output it does not publish.
#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::cast_precision_loss
)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn merge_to(
    base_pbf: &Path,
    osc_file: &Path,
    output_pbf: &crate::output::Output,
    opts: &MergeOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<MergeStats> {
    // Option refusals first: none of them needs the input.
    opts.validate()?;

    // `merge` is public library API, so direct callers need the same refusal
    // as CLI callers. Writing goes to a staging file, so the base is never
    // truncated, but publication would replace the base the run read from.
    // Identity is the destination's, never the staging file's. A
    // not-yet-created output cannot be the base, so the CLI's extra
    // planned-path check is not needed here.
    if crate::path_guard::same_file(base_pbf, output_pbf.destination())? {
        return Err(crate::Error::invalid_argument(format!(
            "apply-changes output '{}' aliases base input '{}'",
            output_pbf.destination().display(),
            base_pbf.display()
        )));
    }

    let MergeOptions {
        write:
            WriteOptions {
                compression,
                backend,
                direct_reads: direct_io,
                force,
            },
        locations_on_ways,
        ignore_missing_nodes,
        parallelism,
        allow_version_regression,
    } = *opts;
    // `validate` refused `force` with `locations_on_ways`: under force the
    // scanner tags every unindexed blob as a placeholder `Node` (its real
    // kind is not known until decompress), which defeats the node-to-way
    // barrier that gates the worker pool until the node-coord loc_map is
    // published. So with `-l` an unindexed base is always refused here.
    //
    // The base is read through several open file descriptions (the front
    // read here, the splice source, the scanner's header walker and the
    // worker pool's), and each is derived from this one pin, so a file
    // renamed over the base mid-run cannot be read partly from each: where
    // `/proc/self/fd` reopens the pin every open reaches the pinned file,
    // otherwise on Unix a replaced base fails the run (`PinnedInput` says
    // what it does off Unix). Taken before any other open of it.
    let base = PinnedInput::pin(base_pbf, host)?;

    // One open answers both the indexdata question and the header, in that
    // order, so a forced run's Note and the missing-indexdata refusal still
    // precede a header decode failure. `merge_header_read_ms` therefore
    // times the whole front read: the open, the header frame, the probe to
    // the first data blob's header and the decode, not the header alone.
    let header_start = crate::counters::Stopwatch::start();
    let mut front = InputFront::open(&base, direct_io, host)?;
    front.require_indexdata(
        force,
        "base PBF has no blob-level indexdata. Without indexdata, every blob must be \
         decompressed to classify elements (significantly slower).",
        host,
    )?;

    // The base header's refusals (unsorted base, `-l` without
    // LocationsOnWays) come before the OSC is parsed: the header read
    // happens either way, so ordering it first costs nothing and keeps a
    // multi-minute OSC parse from preceding a refusal it cannot change.
    let header_bytes = build_header_bytes(front.header()?, locations_on_ways, overrides, host)
        .map_err(|e| e.locate(crate::error::Location::in_file(base_pbf)))?;
    drop(front);
    let header_read_ms = header_start.elapsed_ms();

    let osc_start = crate::counters::Stopwatch::start();
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "apply_changes.parse_osc",
        "Parsing OSC diff: {}",
        osc_file.display()
    );
    let diff = Arc::new({
        let mut overlay = crate::osc::CompactDiffOverlay::new();
        overlay.allow_version_regression(allow_version_regression);
        // The shared parser also emits merge_changes_decompress_ns once per
        // file: input-reader time (file reads, plus decompression for gzip),
        // not the full parse wall measured by osc_start here. Its owner,
        // TimedRead in osc/xml_parse.rs, documents the attribution.
        crate::osc::parse::parse_osc_file_into_until(osc_file, &mut overlay, &host.cancel)?;
        overlay
    });
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "apply_changes.diff_counts",
        "Diff: {} nodes, {} ways, {} relations ({} del nodes, {} del ways, {} del rels)",
        diff.node_count(),
        diff.way_count(),
        diff.relation_count(),
        diff.deleted_ids(crate::ElementKind::Node).len(),
        diff.deleted_ids(crate::ElementKind::Way).len(),
        diff.deleted_ids(crate::ElementKind::Relation).len(),
    );
    let diff_heap_bytes = diff.heap_size_estimate() as u64;
    let osc_parse_ms = osc_start.elapsed_ms();

    let diffranges_start = crate::counters::Stopwatch::start();
    let ranges = Arc::new(DiffRanges::from_diff(&diff));
    let diffranges_ms = diffranges_start.elapsed_ms();
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "apply_changes.diff_ranges",
        "Diff ID ranges: {} node IDs, {} way IDs, {} rel IDs",
        ranges.node_ids.len(),
        ranges.way_ids.len(),
        ranges.rel_ids.len(),
    );

    // --locations-on-ways setup. The descriptor-first pipeline fuses
    // prefill into the worker pool, so we no longer call
    // `prefill_from_base` here. Build_from_diff still produces the
    // OSC-pre-seeded coords (`locations`) and the still-needed-from-base
    // ID set (`needed_set`).
    let (seeded_locations, needed_set, loc_stats_pre) = if locations_on_ways {
        let idx = NodeLocationIndex::build_from_diff(&diff);
        // Counted separately from the two maps: a referenced node the diff
        // deletes is in neither, and must still count as needed so that it
        // shows up as missing.
        let total_needed = idx.total_referenced;
        let seeded = idx.locations.len();
        let still_needed = idx.needed_set.len();
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "apply_changes.low_needed",
            "  locations on ways: {total_needed} node IDs needed, {seeded} from diff, {still_needed} from base"
        );
        (
            Some(idx.locations),
            Some(Arc::new(idx.needed_set)),
            (total_needed, seeded as u64, still_needed as u64),
        )
    } else {
        (None, None, (0, 0, 0))
    };

    let writer_setup_start = crate::counters::Stopwatch::start();
    // Apply-changes' output pass is writer-bound at default compression, so
    // an unspecified (buffered) backend becomes the parallel writer.
    let backend = backend.parallel_by_default();
    let mut writer =
        writer_from_header_bytes(output_pbf, compression, &header_bytes, backend, host)?;
    let writer_setup_ms = writer_setup_start.elapsed_ms();

    // copy_file_range input setup, opened only on the splice route (the
    // drain is its one reader; a `--direct-io` output takes no CopyRange).
    // The handle is shared rather than borrowed: `write_raw_copy` queues the
    // splice into the writer thread, so the input has to stay open past the
    // point this function returns from the call.
    //
    // The base is opened again by the scanner's header walker
    // (`FADV_RANDOM`) and once for the whole worker pool (plain preads of
    // candidate bodies), besides this plain splice source. Separate open
    // file descriptions are kept here, so the walker's RANDOM advice and
    // each reader's readahead state stay off the others' reads; whether
    // sharing one would cost anything is unmeasured (the classify schedules
    // hand their walker's descriptor to their worker pools). All of them
    // derive from `base`, so they are descriptions of one file.
    let splice = backend.supports_copy_range();
    // Test-only: exercise the owned route on any backend.
    #[cfg(feature = "test-hooks")]
    let splice = splice
        && !host
            .cancel
            .faults()
            .apply_changes_force_owned_route
            .load(std::sync::atomic::Ordering::Relaxed);
    let splice_source = if splice {
        Some(crate::read::input::InputFile::open(&base)?)
    } else {
        None
    };

    // Worker count: a fixed parallelism if supplied, one included, else the
    // default `nproc - 2` heuristic (leaves two cores for the scanner and
    // drain threads; its minimum of two is a sizing choice, separate from
    // what an explicit count may be).
    //
    // A single worker is safe through the barrier and its shutdown: worker
    // panics are caught and published to FailureSlot, the drain sees them
    // within one receive timeout, drops its receiver and barrier sender,
    // and merge joins both producers; a failed worker's pool leaves its
    // scope and drops the candidate receiver, releasing a scanner blocked in
    // send. Barrier progress rests on the scanner's boundary message, the
    // drain's barrier check after each receive, and workers flushing coord
    // slots before sending node outputs, none of which needs a second
    // worker. `tests/fault_apply_changes.rs` runs a late boundary, empty,
    // node-only and way-only inputs, and failures while dispatch or the
    // barrier wait is blocked, on both routes, at one worker and more.
    let worker_count = parallelism.resolve(|| {
        crate::host::sized_workers(
            &*host.resources,
            APPLY_CHANGES_WORKERS_RESERVED,
            APPLY_CHANGES_WORKERS_MIN,
            APPLY_CHANGES_WORKERS_FALLBACK,
        )
    });
    crate::debug::emit_counter("merge_worker_count", worker_count);

    // Channels.
    let (drain_tx, drain_rx) = mpsc::sync_channel::<DrainMessage>(DRAIN_CHANNEL_CAPACITY);
    let (candidate_tx, candidate_rx) =
        mpsc::sync_channel::<super::descriptor::ScannedBlob>(CANDIDATE_CHANNEL_CAPACITY);
    // One decision, three views of it: the scanner holds a passthrough drain
    // sender only on the splice route (its node-boundary sender under
    // `--locations-on-ways`, below, is separate), the workers learn which route false positives
    // take, and the drain gets the input file to splice from. Only clone the
    // sender for the scanner if we're going to use it: otherwise the unused
    // clone keeps `drain_rx` alive past worker exit and the drain thread
    // deadlocks waiting for a sender that will never drop until merge()
    // returns - which only happens after the drain joins.
    let (scanner_route, worker_route, drain_route) = match splice_source {
        Some(input_file) => (
            Route::Splice(drain_tx.clone()),
            Route::Splice(()),
            Route::Splice(input_file),
        ),
        None => (Route::Owned, Route::Owned, Route::Owned),
    };

    // --locations-on-ways barrier wiring. The scanner's node boundary goes
    // to the drain on the drain channel itself, through the scanner's own
    // sender (moved into the scanner, so it drops when the scanner does).
    let (barrier_tx_opt, barrier_rx_opt, boundary_tx_opt) = if locations_on_ways {
        let (b_tx, b_rx) = mpsc::sync_channel::<()>(1);
        (Some(b_tx), Some(b_rx), Some(drain_tx.clone()))
    } else {
        (None, None, None)
    };

    let coord_slots: Option<CoordSlots> = if locations_on_ways {
        Some(
            (0..worker_count)
                .map(|_| Arc::new(Mutex::new(FxHashMap::default())))
                .collect(),
        )
    } else {
        None
    };
    let loc_map_handle: Option<LocMapHandle> = if locations_on_ways {
        Some(Arc::new(OnceLock::new()))
    } else {
        None
    };

    let worker_counters = Arc::new(WorkerCounters::new());
    let drain_counters = Arc::new(DrainCounters::new());
    let failure = Arc::new(FailureSlot::new());

    let scanner_cfg = ScannerConfig {
        base: &base,
        ranges: &ranges,
        locations_on_ways,
        channels: ScannerChannels {
            candidate_tx,
            route: scanner_route,
        },
        barrier_rx: barrier_rx_opt,
        boundary_tx: boundary_tx_opt,
    };

    let streaming_cfg = StreamingConfig {
        base: base.clone(),
        ranges: Arc::clone(&ranges),
        diff: Arc::clone(&diff),
        worker_count,
        locations_on_ways,
        coord_slots: coord_slots
            .as_ref()
            .map(|s| s.iter().map(Arc::clone).collect()),
        loc_map_handle: loc_map_handle.as_ref().map(Arc::clone),
        needed_set: needed_set.as_ref().map(Arc::clone),
        compression,
        route: worker_route,
        failure: Arc::clone(&failure),
        #[cfg(feature = "test-hooks")]
        cancel: host.cancel.clone(),
    };
    let streaming_channels = StreamingChannels {
        candidate_rx,
        drain_tx,
    };

    let drain_cfg = DrainConfig {
        ranges: &ranges,
        diff: &diff,
        route: drain_route,
        locations_on_ways,
        coord_slots,
        barrier_tx: barrier_tx_opt,
        loc_map_out: loc_map_handle,
        seeded_locations: seeded_locations.map(|l| SeededLocations::new(Some(l))),
        failure: Arc::clone(&failure),
    };
    let drain_channels = DrainChannels { drain_rx };

    crate::debug::emit_marker("MERGE_LOOP_START");
    let pipeline_start = crate::counters::Stopwatch::start();

    // Drain runs in this (the merge() caller's) thread because it owns
    // `&mut writer`. Scanner and worker pool run in scoped threads.
    //
    // Producers publish failures before releasing their last drain sender.
    // A drain error drops its receiver and barrier sender before we join,
    // unblocking scanner dispatch, barrier waits and worker sends. Join both
    // threads on every path so an implicit scope join cannot discard a cause.
    let scanner_failure = Arc::clone(&failure);
    let scanner_sender_guard = streaming_channels.drain_tx.clone();
    let stats = std::thread::scope(|scope| -> Result<MergeStats> {
        let scanner_handle = scope.spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                scanner::run_scanner(scanner_cfg, host)
            }));
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => scanner_failure.publish(e),
                Err(payload) => scanner_failure.publish_panic("scanner", &*payload),
            }
            // Keep the channel connected until publication, even though
            // run_scanner has already dropped its own channels on return.
            drop(scanner_sender_guard);
        });
        let workers_counters_inner = Arc::clone(&worker_counters);
        let workers_handle = scope.spawn(move || {
            streaming::run_workers(streaming_cfg, streaming_channels, &workers_counters_inner);
        });

        let drain_stats = drain::run_drain(drain_cfg, drain_channels, &mut writer, &drain_counters);

        let scanner_join = scanner_handle.join();
        let workers_join = workers_handle.join();
        if let Err(payload) = scanner_join {
            failure.publish_panic("scanner", &*payload);
        }
        if let Err(payload) = workers_join {
            failure.publish_panic("worker pool", &*payload);
        }
        // The drain checks producer failures before reporting a seq gap.
        // Keep its error: a later scanner send failure can be a consequence
        // of the drain dropping its receiver after an output write failed.
        let stats = drain_stats?;
        if let Some(e) = failure.take() {
            return Err(e);
        }
        Ok(stats)
    })?;

    let pipeline_ms = pipeline_start.elapsed_ms();
    crate::debug::emit_marker("MERGE_LOOP_END");

    let mut final_stats = stats;
    final_stats.diff_heap_bytes = diff_heap_bytes;
    if locations_on_ways {
        final_stats.loc_nodes_needed = loc_stats_pre.0;
        final_stats.loc_nodes_from_diff = loc_stats_pre.1;
        // Resolution is counted from the merged map's UNIQUE KEYS, never
        // from the worker tuple counter. `coord_pairs_extracted` increments
        // once per extracted tuple, including when `local_coords.insert`
        // replaces an id already seen - so a base PBF carrying two records
        // for one required node would inflate the count and let it cover a
        // genuinely absent id, silently reinstating the (0,0) fallback that
        // the guard below exists to refuse.
        let resolved = drain_counters
            .barrier_loc_map_size
            .load(std::sync::atomic::Ordering::Relaxed);
        final_stats.loc_nodes_from_base = resolved.saturating_sub(loc_stats_pre.1);
        final_stats.loc_missing = loc_stats_pre.0.saturating_sub(resolved);
        final_stats.loc_node_blobs_scanned = worker_counters
            .loc_node_blobs_scanned
            .load(std::sync::atomic::Ordering::Relaxed);
    }

    if locations_on_ways && final_stats.loc_missing != 0 && !ignore_missing_nodes {
        return Err(crate::error::new_missing_node_locations(
            final_stats.loc_missing,
        ));
    }

    // No summary here: the caller renders the returned stats, as for every
    // other command. Writing it here too printed it twice on the CLI.

    // Surface phase counters that callers expect from the legacy shape.
    crate::debug::emit_counter("merge_osc_parse_ms", osc_parse_ms);
    crate::debug::emit_counter("merge_diffranges_ms", diffranges_ms);
    crate::debug::emit_counter("merge_header_read_ms", header_read_ms);
    crate::debug::emit_counter("merge_writer_setup_ms", writer_setup_ms);
    crate::debug::emit_counter("merge_pipeline_ms", pipeline_ms);

    crate::debug::emit_counter("merge_blobs_passthrough", final_stats.blobs_passthrough);
    crate::debug::emit_counter("merge_blobs_rewritten", final_stats.blobs_rewritten);
    crate::debug::emit_counter("merge_blobs_index_hit", final_stats.blobs_index_hit);
    crate::debug::emit_counter("merge_total_elements", final_stats.total_elements());
    crate::debug::emit_counter("merge_deleted", final_stats.deleted);
    crate::debug::emit_counter("merge_diff_nodes", final_stats.diff_nodes);
    crate::debug::emit_counter("merge_diff_ways", final_stats.diff_ways);
    crate::debug::emit_counter("merge_diff_relations", final_stats.diff_relations);
    crate::debug::emit_counter("merge_bytes_passthrough", final_stats.bytes_passthrough);
    crate::debug::emit_counter("merge_bytes_rewritten", final_stats.bytes_rewritten);
    crate::debug::emit_counter("merge_diff_heap_bytes", final_stats.diff_heap_bytes);
    if locations_on_ways {
        crate::debug::emit_counter("merge_loc_needed", final_stats.loc_nodes_needed);
        crate::debug::emit_counter("merge_loc_from_diff", final_stats.loc_nodes_from_diff);
        crate::debug::emit_counter("merge_loc_from_base", final_stats.loc_nodes_from_base);
        crate::debug::emit_counter("merge_loc_missing", final_stats.loc_missing);
        crate::debug::emit_counter(
            "merge_loc_node_blobs_scanned",
            final_stats.loc_node_blobs_scanned,
        );
    }

    Ok(final_stats)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn merge_refuses_output_equal_to_base_before_reading_inputs() {
        let dir = crate::path_guard::tests::scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let original: &[u8] = b"base file must survive";
        std::fs::write(&base, original).expect("write base sentinel");

        // Overwrite permission granted, so the alias refusal itself is
        // what stops the run (without it the existing base is refused as an
        // existing output first).
        let error = merge(
            &base,
            &osc,
            crate::output::Output::new(
                &base,
                crate::output::Overwrite::Replace,
                &crate::host::Diagnostics::discard(),
            )
            .unwrap(),
            &MergeOptions::default(),
            &HeaderOverrides::default(),
            &crate::host::Host::for_tests(),
        )
        .err()
        .expect("the base input must not also be the output");

        assert!(
            error.to_string().contains("aliases base input"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read(&base).expect("read base sentinel").as_slice(),
            original
        );
    }
}
