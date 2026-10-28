//! Stage 4: Assembly - emit enriched PBF.
//!
//! Re-reads the PBF, attaches coordinates from per-blob coord_payloads preads
//! to ways.
//! P2c: pread-from-workers with pre-scan schedule for parallel decompress + assembly.
//! Way blobs use wire-format reframe (no full decode); non-way blobs use BlockBuilder.

use std::path::Path;

use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::write::WriteOptions;
use crate::{Element, PrimitiveBlock};

use super::super::Stats;
use super::super::inject_metrics::{InjectMetrics, PinTally};
use super::super::{
    HeaderOverrides, Result, ensure_node_capacity_local, flush_local, writer_from_header,
};
use crate::ElementKind;
use crate::counters::record_max;
use crate::error::Error;
use crate::idset::IdSet;
use crate::owned::{dense_node_metadata, element_metadata};

use super::blob_meta::BlobMeta;
use super::coord_payloads::AbortOnDrop;

/// Blob descriptor for the stage 4 pre-scan schedule.
///
/// Unlike sparse pass 2, external requires a known kind and a way-sidecar
/// index. Share eligibility policy, but keep these backend-specific fields
/// and the measured dispatch/reorder pipelines local.
struct BlobDescriptor {
    /// Start of the on-disk 4-byte length prefix. Used by the consumer-side
    /// raw passthrough path to pread the entire framed blob (header + data)
    /// and hand it verbatim to `PbfWriter::write_raw_owned`.
    frame_offset: u64,
    /// `(data_offset - frame_offset) + data_size`. The exact number of
    /// bytes to pread for raw passthrough.
    frame_size: usize,
    data_offset: u64,
    data_size: usize,
    is_way_blob: bool,
    /// `true` when the blob can be passed through as raw compressed bytes
    /// without decompress + PrimitiveBlock decode + re-encode. Relations
    /// always qualify; node blobs qualify only when `keep_untagged_nodes`.
    /// Ways never qualify (they need coord_payloads splicing).
    is_passthrough: bool,
    /// Blob kind from indexdata. External join requires indexdata.
    kind: crate::ElementKind,
    /// Element count from indexdata, used to populate Stats on the
    /// passthrough path (no decode available for a live count).
    count: u64,
    /// Index of this way blob within `way_slot_starts` (0 for non-way blobs;
    /// only meaningful when `is_way_blob`).
    way_blob_idx: usize,
    /// The whole indexdata claim, held to what a decoded blob holds.
    claim: crate::blob_meta::BlobIndex,
}

crate::counters::counter_set! {
    /// Stage 4 decode-worker counters, shared by every worker; bumped once
    /// per scheduled blob. Consumer-side counters stay plain locals on the
    /// single consumer thread.
    struct Stage4WorkerCounters {
        pread_ms => "s4_pread_ms",
        decompress_ms => "s4_decompress_ms",
        assemble_ms => "s4_assemble_ms",
        way_reframe_ms => "s4_way_reframe_ms",
        nonway_assemble_ms => "s4_nonway_assemble_ms",
        way_blobs_processed => "s4_way_blobs_processed",
        nonway_blobs_processed => "s4_nonway_blobs_processed",
        send_ms => "s4_send_ms",
        blobs => "s4_blobs",
        bytes_read => "s4_bytes_read",
        pread_calls => "s4_pread_calls",
        max_worker_buf_bytes => "s4_max_worker_buf_bytes",
        coord_payload_pread_ms => "s4_coord_payload_pread_ms",
        coord_payload_bytes => "s4_coord_payload_bytes",
        /// Streaming-router readiness wait. `wait_ready` is called at the
        /// top of the way-blob branch, ahead of the input pread, so a decode
        /// worker never holds a decompressed block + StringTable resident
        /// while blocked. Counted only for a positive measured wait.
        readiness_wait_ns => "s4_readiness_wait_ms" / 1_000_000,
        readiness_wait_blobs => "s4_readiness_wait_blobs",
        readiness_wait_max_ns => "s4_readiness_wait_max_ms" / 1_000_000,
        /// Result-channel depth high-water as the workers count it: up
        /// before a send, down after the consumer's receive, so blocked
        /// senders count and it can exceed the channel's capacity by up to
        /// one per worker. A worker counts before it tries to send, so a
        /// value at or past capacity does not by itself prove the channel
        /// filled (`s4_send_ms`, the `WAIT_S4_SEND` span and
        /// `ordered_pool_result_send_wait_ns` are that evidence); staying
        /// well below it says the workers never could.
        channel_high_water => "s4_channel_high_water",
    }
}

/// Load the ref-count sidecar and compute prefix sums for slot_start values.
pub(super) fn load_ref_count_sidecar(path: &Path, total_slots: u64) -> Result<Vec<u64>> {
    load_ref_count_sidecar_in(path, total_slots)
        .map_err(|e| e.locate(crate::error::Location::in_file(path)))
}

fn load_ref_count_sidecar_in(path: &Path, total_slots: u64) -> Result<Vec<u64>> {
    use std::io::Read as _;
    let file = crate::read::input::open(path)?;
    let bytes = file.metadata()?.len();
    if bytes < 8 {
        return Err(Error::internal("ref count sidecar is too small"));
    }
    if bytes % 8 != 0 {
        return Err(Error::internal("ref count sidecar has non-aligned entries"));
    }
    let num_entries: usize = (bytes / 8 - 1)
        .try_into()
        .map_err(|_| Error::internal("ref count sidecar has too many entries"))?;
    // Only the prefix vector must survive: stages 2/3 use it for random
    // blob-to-slot lookup. Stream the counts instead of holding a second,
    // equally large encoded byte vector during prefix construction.
    let mut reader = std::io::BufReader::new(file);
    let mut slot_starts = Vec::with_capacity(num_entries);
    let mut cumulative: u64 = 0;
    let mut chunk = [0_u8; 8];
    for _ in 0..num_entries {
        reader.read_exact(&mut chunk)?;
        slot_starts.push(cumulative);
        cumulative = cumulative
            .checked_add(u64::from_le_bytes(chunk))
            .ok_or_else(|| Error::internal("ref count sidecar cumulative count overflow"))?;
    }
    reader.read_exact(&mut chunk)?;
    let trailer_total = u64::from_le_bytes(chunk);
    if trailer_total != total_slots {
        return Err(Error::internal(format!(
            "ref count sidecar total ({trailer_total}) != stage 1 total_slots ({total_slots})"
        )));
    }
    if cumulative != total_slots {
        return Err(Error::internal(format!(
            "ref count sidecar cumulative ({cumulative}) != total_slots ({total_slots})"
        )));
    }
    Ok(slot_starts)
}

// ---------------------------------------------------------------------------
// Stage 4: Assembly
// ---------------------------------------------------------------------------

/// One stage 4 decode worker's thread-local buffers, reused across blobs.
struct S4Local {
    read_buf: Vec<u8>,
    decompress_buf: Vec<u8>,
    bb: BlockBuilder,
    output_blocks: Vec<OwnedBlock>,
    way_reframe_scratch: WayReframeScratch,
    reframe_output: Vec<u8>,
    /// Per-blob coord payload buffer.
    coord_payload_buf: Vec<u8>,
}

/// Stage 4's consumer: decoded blobs from the workers and passthrough raw
/// frames it preads itself, written in schedule order. `write_raw_owned`
/// hands the pre-framed bytes to the writer thread verbatim. Its counters
/// are plain fields on the single consumer thread.
///
/// It holds the router because a failing run must abort it before the pool
/// joins its workers: an admitted worker may be parked in `wait_ready` for a
/// slot stage 3 has not published, and the join would otherwise wait for
/// stage 3 to get there. A returned error aborts through `abandon`; a panic
/// in either callback aborts through the guard each one holds, during the
/// unwind, which also runs before the join. A worker panic aborts through
/// the guard around each worker body (see `stage4_assembly`).
struct S4Consumer<'a> {
    router: &'a super::coord_payloads::ConcurrentBlobLocationRouter,
    interleaved: crate::ordered_pool::Interleaved<'a>,
    schedule: &'a [BlobDescriptor],
    file: &'a std::fs::File,
    input: &'a Path,
    frame_read_buf: Vec<u8>,
    writer: &'a mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    total_stats: Stats,
    bytes_written: u64,
    write_calls: u64,
    write_ms: u64,
    passthrough_blobs: u64,
    passthrough_pread_ms: u64,
    passthrough_bytes: u64,
    cancel: &'a crate::host::Invocation,
}

impl S4Consumer<'_> {
    /// pread the passthrough frame at schedule position `position` and write
    /// it verbatim.
    fn passthrough(&mut self, position: usize) -> Result<()> {
        use std::os::unix::fs::FileExt as _;
        self.cancel.check()?;
        let desc = &self.schedule[position];
        let (frame_offset, frame_size) = (desc.frame_offset, desc.frame_size);
        let t_pread = crate::counters::Stopwatch::start();
        self.frame_read_buf.resize(frame_size, 0);
        self.file
            .read_exact_at(&mut self.frame_read_buf, frame_offset)
            .map_err(|e| {
                Error::from(e).locate(crate::error::Location::at_offset(self.input, frame_offset))
            })?;
        self.passthrough_pread_ms += t_pread.elapsed_ms();
        self.passthrough_bytes += frame_size as u64;

        let t_w = crate::counters::Stopwatch::start();
        self.bytes_written += frame_size as u64;
        self.write_calls += 1;
        self.writer.write_raw_owned(std::mem::replace(
            &mut self.frame_read_buf,
            Vec::with_capacity(frame_size),
        ))?;
        self.write_ms += t_w.elapsed_ms();

        self.passthrough_blobs += 1;
        self.total_stats.blobs_passthrough += 1;
        match desc.kind {
            crate::ElementKind::Node => {
                self.total_stats.nodes_read += desc.count;
                self.total_stats.nodes_written += desc.count;
            }
            crate::ElementKind::Relation => {
                self.total_stats.relations_written += desc.count;
            }
            crate::ElementKind::Way => {
                // Ways never pass through (they need coord_payloads
                // splicing). Fall through without touching stats;
                // unreachable in practice but we avoid a hard panic here
                // since this is on the output path.
            }
        }
        Ok(())
    }
}

impl S4Consumer<'_> {
    fn before_item_inner(&mut self, ordinal: usize) -> crate::Result<()> {
        for position in self.interleaved.before(ordinal) {
            self.passthrough(position)?;
        }
        Ok(())
    }

    fn consume_inner(
        &mut self,
        ordinal: usize,
        (blocks, block_stats): (Vec<OwnedBlock>, Stats),
    ) -> crate::Result<()> {
        #[cfg(feature = "test-hooks")]
        self.cancel.faults().on_altw_stage4_consume(ordinal)?;
        #[cfg(not(feature = "test-hooks"))]
        let _ = ordinal;
        self.total_stats.merge(&block_stats);
        for block in blocks {
            self.bytes_written += block.bytes.len() as u64;
            self.write_calls += 1;
            let t_w = crate::counters::Stopwatch::start();
            self.writer.write_primitive_block_owned(block)?;
            self.write_ms += t_w.elapsed_ms();
        }
        Ok(())
    }
}

impl crate::ordered_pool::OrderedConsumer<(Vec<OwnedBlock>, Stats)> for S4Consumer<'_> {
    // Each callback disarms its guard on every return, an error included, so
    // only a panic aborts through it; a returned error reaches the router
    // through `abandon` with its own message.
    fn before_item(&mut self, ordinal: usize) -> crate::Result<()> {
        let guard = AbortOnDrop::new(self.router, "stage 4 consumer");
        let result = self.before_item_inner(ordinal);
        guard.disarm();
        result
    }

    fn consume(&mut self, ordinal: usize, item: (Vec<OwnedBlock>, Stats)) -> crate::Result<()> {
        let guard = AbortOnDrop::new(self.router, "stage 4 consumer");
        let result = self.consume_inner(ordinal, item);
        guard.disarm();
        result
    }

    fn abandon(&mut self, error: &Error) {
        self.router.abort(error.clone());
    }
}

/// Assembly pass: re-read the PBF, attach coordinates from per-blob
/// coord_payloads preads to ways.
/// P2c: pread-from-workers with pre-scan schedule for parallel decompress + assembly.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn stage4_assembly(
    input: &crate::read::input::PinnedInput,
    output: &crate::output::Output,
    // The input's header, from the run's one front read; stage 4 does not
    // reopen the input for it.
    header: &crate::HeaderBlock,
    blob_meta: &[BlobMeta],
    coord_payloads_reader: &super::coord_payloads::ConcurrentBlobLocationRouter,
    per_way_rcs: &super::coord_payloads::PerWayRcs,
    way_slot_starts: &[u64],
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    member_way_ids: Option<&IdSet>,
    // Selects payload pin-bitmap framing, not the scratch latitude encoding:
    // coord_payloads already decoded ResolvedEntry before publishing bytes.
    inject_prepass: bool,
    inject_metrics: &InjectMetrics,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<Stats> {
    use std::os::unix::fs::FileExt;
    let WriteOptions {
        compression,
        backend,
        ..
    } = *write;
    #[cfg(feature = "test-hooks")]
    host.invocation.faults().on_altw_stage4_start()?;
    // Build the blob schedule from the shared metadata scan.
    host.invocation.marker("EXTJOIN_S4_SCHEDULE_START");
    let t_schedule = crate::counters::Stopwatch::start();

    let mut schedule: Vec<BlobDescriptor> = Vec::new();
    let mut way_sidecar_idx: usize = 0;
    let mut skipped_node_blobs: u64 = 0;

    // Stage 4 schedule diagnostics.
    let mut s4_node_blobs_total: u64 = 0;
    let mut s4_node_blobs_no_tagindex: u64 = 0;
    let mut s4_node_blobs_empty_tags: u64 = 0;
    let mut s4_node_blobs_kept_by_members: u64 = 0;
    let mut s4_node_blobs_kept_by_tags: u64 = 0;
    let mut s4_way_blobs: u64 = 0;
    let mut s4_relation_blobs: u64 = 0;

    for meta in blob_meta {
        // Count blob types for diagnostics.
        match meta.kind {
            crate::ElementKind::Node => s4_node_blobs_total += 1,
            crate::ElementKind::Way => s4_way_blobs += 1,
            crate::ElementKind::Relation => s4_relation_blobs += 1,
        }

        // P1b: skip node blobs with only untagged non-member nodes.
        // Both tests read claims, believed without a decode: a tagdata key
        // set falsely empty drops tagged nodes, and an understated ID range
        // hides relation members (reference/indexdata-trust.md).
        if !keep_untagged_nodes && matches!(meta.kind, crate::ElementKind::Node) {
            if !meta.has_tagindex {
                s4_node_blobs_no_tagindex += 1;
            } else if !meta.has_tags {
                s4_node_blobs_empty_tags += 1;
            }
            if meta.has_tags {
                s4_node_blobs_kept_by_tags += 1;
            } else {
                let has_members = relation_member_node_ids
                    .is_some_and(|ids| ids.any_in_range(meta.min_id, meta.max_id));
                if has_members {
                    s4_node_blobs_kept_by_members += 1;
                }
                if !has_members {
                    skipped_node_blobs += 1;
                    continue;
                }
            }
        }

        // Way blobs consume one sidecar entry each.
        let way_blob_idx = if matches!(meta.kind, crate::ElementKind::Way) {
            if way_sidecar_idx >= way_slot_starts.len() {
                return Err(Error::internal(
                    "ref count sidecar has fewer entries than way blobs in PBF",
                ));
            }
            let this_way_blob_idx = way_sidecar_idx;
            way_sidecar_idx += 1;
            this_way_blob_idx
        } else {
            0
        };

        let is_way_blob = matches!(meta.kind, crate::ElementKind::Way);
        let is_passthrough =
            super::super::blob_is_passthrough(Some(meta.kind), keep_untagged_nodes);

        #[allow(clippy::cast_possible_truncation)]
        let frame_size = (meta.data_offset - meta.frame_offset) as usize + meta.data_size;

        schedule.push(BlobDescriptor {
            frame_offset: meta.frame_offset,
            frame_size,
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            is_way_blob,
            is_passthrough,
            kind: meta.kind,
            count: meta.count,
            way_blob_idx,
            claim: meta.claim(),
        });
    }

    // Verify all sidecar entries were consumed.
    if way_sidecar_idx != way_slot_starts.len() {
        return Err(Error::internal(format!(
            "ref count sidecar has {} entries but only {} way blobs seen in PBF",
            way_slot_starts.len(),
            way_sidecar_idx,
        )));
    }
    host.invocation.marker("EXTJOIN_S4_SCHEDULE_END");

    // Open shared file for worker pread, a description of the pinned input.
    let shared_file = std::sync::Arc::new(crate::read::input::InputSource::from(input).open()?);
    let input = input.path();

    let mut writer = writer_from_header(
        output,
        compression,
        header,
        &super::super::altw_output_facts(inject_prepass),
        overrides,
        backend,
        host,
    )?;

    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );
    // Pre-streaming baseline had no cap (ran 22 threads on a 24-core
    // host); the initial Commit B run capped at .min(4) and measured a
    // +11.5% Europe regression because stage 4 decode throughput fell
    // off a cliff. The streaming overlap's readiness wait is
    // effectively zero-cost (s4_readiness_wait_max_ms=0 on Europe), so
    // stage 4 is still CPU/writer-bound. Keep uncapped.
    host.invocation
        .counter("s4_decode_thread_count", decode_threads);

    // Split schedule: passthrough-eligible blobs (relations always; node
    // blobs when keep_untagged_nodes=true) are copied by the consumer
    // thread via direct pread + write_raw_owned, between the decoded ones.
    // Decode blobs (ways and the remaining filtered node blobs) go to the
    // workers of the ordered pool, which runs over them alone, so raw-frame
    // traffic never takes a worker, a channel slot or a window slot. This
    // mirrors extract's `pread_execute`.
    let decode_at: Vec<usize> = schedule
        .iter()
        .enumerate()
        .filter(|(_, d)| !d.is_passthrough)
        .map(|(i, _)| i)
        .collect();
    let decode_at = &decode_at;
    let schedule = &schedule;

    // Worker-side cumulative counters.
    let worker_counters = Stage4WorkerCounters::new();
    let c = &worker_counters;
    let way_reframe_counters = WayReframeCounters::new();
    let way_reframe_cref = &way_reframe_counters;

    // Depth-gated WAIT span for decode workers blocked handing results
    // to the consumer (writer backpressure). Its marker pair appears in
    // `brokkr sidecar --durations`; `--stalls` reads `*_wait_ns` counters,
    // not these spans. Entered only after a failed try_send,
    // so unblocked sends never touch the gauge lock.
    let s4_send_stall = crate::counters::StallGauge::new(
        "WAIT_S4_SEND_START",
        "WAIT_S4_SEND_END",
        &host.invocation,
    );
    // The consumer's receive wait, in whole milliseconds per sample as it
    // always was.
    let s4_recv_ms = std::sync::atomic::AtomicU64::new(0);

    let mut consumer = S4Consumer {
        router: coord_payloads_reader,
        interleaved: crate::ordered_pool::Interleaved::new(decode_at, schedule.len()),
        schedule,
        file: &shared_file,
        input,
        frame_read_buf: Vec::new(),
        writer: &mut writer,
        total_stats: Stats::default(),
        bytes_written: 0,
        write_calls: 0,
        write_ms: 0,
        passthrough_blobs: 0,
        passthrough_pread_ms: 0,
        passthrough_bytes: 0,
        cancel: &host.invocation,
    };

    // Worker errors are located against the input once per worker pool, not
    // per blob.
    let source_arc: std::sync::Arc<Path> = std::sync::Arc::from(input);
    let source = &source_arc;
    // Worker threads: pread -> decompress -> PrimitiveBlock -> assemble.
    // Dedicated threads, NOT global rayon (PbfWriter uses rayon for
    // compression). Cancellation is checked on the consumer; workers parked
    // in `wait_ready` are woken by stage 3, which observes the same token
    // per bucket and aborts the router. A worker panic (task body or local
    // init) aborts the router through the worker's own guard.
    let mut workers = vec![(); decode_threads];
    crate::ordered_pool::run_ordered(
        decode_at.len(),
        &mut workers,
        crate::ordered_pool::PoolConfig {
            window: crate::ordered_pool::default_window(decode_threads),
            cancel: &host.invocation,
            waits: crate::ordered_pool::Waits {
                recv: Some(crate::ordered_pool::Timer::Ms(&s4_recv_ms)),
                send: Some(crate::ordered_pool::Timer::Ms(&c.send_ms)),
                send_stall: Some(&s4_send_stall),
                queue_high_water: Some(&c.channel_high_water),
                ..crate::ordered_pool::Waits::default()
            },
        },
        |_| {
            let guard = AbortOnDrop::new(coord_payloads_reader, "stage 4 worker");
            let local = S4Local {
                read_buf: Vec::new(),
                decompress_buf: Vec::new(),
                bb: BlockBuilder::new(),
                output_blocks: Vec::new(),
                way_reframe_scratch: WayReframeScratch::new(),
                reframe_output: Vec::new(),
                coord_payload_buf: Vec::new(),
            };
            guard.disarm();
            local
        },
        |(), l: &mut S4Local, ordinal| {
            use std::sync::atomic::Ordering::Relaxed;
            // A worker panic aborts the router during its unwind, so the
            // other workers parked in `wait_ready` wake and stage 3 stops.
            // The pool alone would not: it learns of a worker panic only
            // once every other worker has finished its admitted task, and
            // a parked one finishes only when stage 3 publishes its slot.
            // Disarmed only at the one return below, Ok or Err alike: a
            // returned error stays an ordered failure and reaches the
            // router through `abandon`.
            let guard = AbortOnDrop::new(coord_payloads_reader, "stage 4 worker");
            #[cfg(feature = "test-hooks")]
            host.invocation.faults().on_altw_stage4_work(ordinal);
            let desc = &schedule[decode_at[ordinal]];
            let file = &shared_file;

            // Only errors about the input frame itself are located
            // there. A router abort or a read of a worker scratch
            // file says nothing about this input blob and stays
            // unlocated.
            let at_frame = |e: Error| {
                e.locate(crate::error::Location::new(
                    Some(source),
                    None,
                    Some(crate::ByteOffset(desc.frame_offset)),
                ))
            };
            let S4Local {
                read_buf,
                decompress_buf,
                bb,
                output_blocks,
                way_reframe_scratch,
                reframe_output,
                coord_payload_buf,
            } = l;
            let result: crate::error::Result<(Vec<OwnedBlock>, Stats)> =
                (|| -> crate::error::Result<(Vec<OwnedBlock>, Stats)> {
                    // For way blobs: block on the router's readiness
                    // BEFORE the input pread/decompress so a stalled
                    // decode thread never holds an already-expanded
                    // block + StringTable resident. Non-way blobs skip
                    // this entirely - they never consume a coord payload.
                    if desc.is_way_blob {
                        #[cfg(feature = "test-hooks")]
                        host.invocation
                            .faults()
                            .on_altw_stage4_ready_wait(desc.way_blob_idx);
                        let t_wait = crate::counters::Stopwatch::start();
                        coord_payloads_reader.wait_ready(desc.way_blob_idx)?;
                        // One sample feeds the total, the count and
                        // the maximum.
                        let wait_ns = t_wait.elapsed_ns();
                        if wait_ns > 0 {
                            c.readiness_wait_ns.fetch_add(wait_ns, Relaxed);
                            c.readiness_wait_blobs.fetch_add(1, Relaxed);
                            record_max(&c.readiness_wait_max_ns, wait_ns);
                        }
                    }

                    let t0 = crate::counters::Stopwatch::start();
                    read_buf.resize(desc.data_size, 0);
                    file.read_exact_at(read_buf, desc.data_offset)
                        .map_err(|e| at_frame(Error::from(e)))?;
                    t0.record_ms(&c.pread_ms);
                    c.bytes_read.fetch_add(desc.data_size as u64, Relaxed);
                    c.pread_calls.fetch_add(1, Relaxed);

                    let t1 = crate::counters::Stopwatch::start();
                    crate::blob::decompress_blob_raw(read_buf, decompress_buf).map_err(at_frame)?;
                    t1.record_ms(&c.decompress_ms);

                    let t2 = crate::counters::Stopwatch::start();
                    output_blocks.clear();

                    if desc.is_way_blob {
                        // Read this blob's coord_payloads payload into a
                        // worker-local buffer before reframe.
                        let t_cpread = crate::counters::Stopwatch::start();
                        coord_payloads_reader
                            .pread_blob_payload(desc.way_blob_idx, coord_payload_buf)?;
                        t_cpread.record_ms(&c.coord_payload_pread_ms);
                        c.coord_payload_bytes
                            .fetch_add(coord_payload_buf.len() as u64, Relaxed);

                        // Wire-format reframe: splice locations without
                        // full PrimitiveBlock decode or BlockBuilder.
                        let (way_count, min_id, max_id) = reframe_way_blob_with_locations(
                            decompress_buf,
                            coord_payload_buf,
                            per_way_rcs.blob_record(desc.way_blob_idx),
                            desc.way_blob_idx,
                            reframe_output,
                            way_reframe_scratch,
                            way_reframe_cref,
                            member_way_ids,
                            inject_prepass,
                        )
                        .map_err(at_frame)?;
                        // The output index below is the reframe's count
                        // and range: a claim it does not match exactly (a
                        // blob claiming ways that holds none above all) is
                        // refused rather than re-indexed. Stage 1 refused
                        // it already; this guards what is written.
                        crate::blob_meta::IdTally::of(way_count, min_id, max_id)
                            .refuse_inexact(&desc.claim)
                            .map_err(at_frame)?;

                        let index = crate::blob_meta::BlobIndex {
                            kind: crate::ElementKind::Way,
                            min_id,
                            max_id,
                            count: way_count,
                            bbox: None,
                        };
                        let taken = std::mem::take(reframe_output);
                        reframe_output.reserve(taken.len());
                        output_blocks.push(OwnedBlock {
                            bytes: taken,
                            index,
                            tagdata: None,
                            way_members: inject_prepass.then(|| {
                                let payload = crate::commands::altw::reframe::way_members_payload(
                                    way_count,
                                    &way_reframe_scratch.member_ways,
                                );
                                inject_metrics
                                    .record_blob(way_reframe_scratch.pin_tally, payload.len());
                                payload
                            }),
                        });

                        let block_stats = Stats {
                            ways_written: way_count,
                            blobs_decoded: 1,
                            ..Stats::default()
                        };
                        // One sample, so the pair agrees exactly.
                        let elapsed = t2.elapsed_ms();
                        c.assemble_ms.fetch_add(elapsed, Relaxed);
                        c.way_reframe_ms.fetch_add(elapsed, Relaxed);
                        c.blobs.fetch_add(1, Relaxed);
                        c.way_blobs_processed.fetch_add(1, Relaxed);
                        return Ok((std::mem::take(output_blocks), block_stats));
                    }

                    // Non-way blobs: full PrimitiveBlock decode + BlockBuilder.
                    // `decompress_buf` is already an owned, unshared Vec, so
                    // parse it in place with `from_vec`; the old
                    // `PrimitiveBlock::new(Bytes::from(vec))` wrapped it as
                    // Bytes only to copy it straight back out via `to_vec()`.
                    let block = PrimitiveBlock::from_vec(std::mem::take(decompress_buf))
                        .map_err(at_frame)?;
                    // A way or relation under the node claim would be
                    // re-encoded among the nodes with no coordinate source
                    // and outside the member scan: refused, as the sparse
                    // backend refuses it. The tally holds the claim and the
                    // header's order to every node, since stage 2 decodes
                    // only the node blobs a referenced bucket reaches; node
                    // claims are non-negative here (the metadata scan
                    // refused any other), so one raw compare per node
                    // decides it, and a false verdict replays the nodes,
                    // cold, for the exact refusal.
                    block
                        .refuse_foreign_kind(Some(desc.kind))
                        .map_err(at_frame)?;
                    let mut tally = crate::blob_meta::AscendingTally::default();
                    let mut block_stats = assemble_block(
                        &block,
                        bb,
                        output_blocks,
                        keep_untagged_nodes,
                        relation_member_node_ids,
                        &mut tally,
                    )
                    .map_err(at_frame)?;
                    if !tally.holds(&desc.claim) {
                        return Err(at_frame(crate::blob_meta::replay_refusal(
                            &desc.claim,
                            super::blob_meta::EXTERNAL_ORDER,
                            block.elements_skip_metadata().map(|e| match e {
                                Element::DenseNode(n) => n.id(),
                                Element::Node(n) => n.id(),
                                Element::Way(w) => w.id(),
                                Element::Relation(r) => r.id(),
                            }),
                        )));
                    }
                    block_stats.blobs_decoded = 1;
                    flush_local(bb, output_blocks).map_err(at_frame)?;
                    let elapsed = t2.elapsed_ms();
                    c.assemble_ms.fetch_add(elapsed, Relaxed);
                    c.nonway_assemble_ms.fetch_add(elapsed, Relaxed);
                    c.nonway_blobs_processed.fetch_add(1, Relaxed);

                    if decompress_buf.capacity() == 0 {
                        *decompress_buf = Vec::new();
                    }

                    c.blobs.fetch_add(1, Relaxed);

                    Ok((std::mem::take(output_blocks), block_stats))
                })();

            // Track max live buffer bytes for this worker.
            {
                let worker_bytes = read_buf.capacity() as u64
                    + decompress_buf.capacity() as u64
                    + reframe_output.capacity() as u64;
                record_max(&c.max_worker_buf_bytes, worker_bytes);
            }

            guard.disarm();
            // The pool times the send (`s4_send_ms`), opens the
            // WAIT_S4_SEND span only when the result channel is full,
            // and records the channel's depth high-water
            // (`s4_channel_high_water`): workers count up before a
            // send and the consumer down after a receive.
            result
        },
        &mut consumer,
    )?;
    for position in consumer.interleaved.rest() {
        consumer.passthrough(position)?;
    }
    let S4Consumer {
        total_stats,
        bytes_written: s4_bytes_written,
        write_calls: s4_write_calls,
        write_ms: s4_write_ms,
        passthrough_blobs: s4_passthrough_blobs,
        passthrough_pread_ms: s4_passthrough_pread_ms,
        passthrough_bytes: s4_passthrough_bytes,
        ..
    } = consumer;
    let s4_recv_ms = s4_recv_ms.into_inner();

    // Time the final writer flush. `write_raw_owned` and
    // `write_primitive_block_owned` hand work to the writer thread
    // (compression + file I/O); were the pool's result channel made deeper,
    // backpressure that surfaces as `s4_send_ms` would shift into
    // the writer pipeline and only materialises here. Isolating
    // flush makes the channel-vs-writer attribution legible.
    let t_flush = crate::counters::Stopwatch::start();
    writer.flush()?;
    let s4_flush_ms: u64 = t_flush.elapsed_ms();
    let s4_output_bytes = std::fs::metadata(output.path())
        .map_err(|e| {
            Error::from(e).context(format!("stat output {}", output.destination().display()))
        })?
        .len();

    worker_counters.emit_all(&host.invocation);
    host.invocation
        .counter("s4_bytes_written", s4_bytes_written);
    host.invocation.counter("s4_write_calls", s4_write_calls);
    host.invocation
        .counter("s4_passthrough_blobs", s4_passthrough_blobs);
    host.invocation
        .counter("s4_passthrough_pread_ms", s4_passthrough_pread_ms);
    host.invocation
        .counter("s4_passthrough_bytes", s4_passthrough_bytes);
    host.invocation.counter("s4_consumer_recv_ms", s4_recv_ms);
    host.invocation.counter("s4_consumer_write_ms", s4_write_ms);
    host.invocation.counter("s4_flush_ms", s4_flush_ms);
    host.invocation.counter("s4_decode_threads", decode_threads);
    host.invocation.counter("s4_output_bytes", s4_output_bytes);
    host.invocation
        .counter("extjoin_skipped_node_blobs", skipped_node_blobs);
    host.invocation
        .counter("s4_node_blobs_total", s4_node_blobs_total);
    host.invocation
        .counter("s4_node_blobs_no_tagindex", s4_node_blobs_no_tagindex);
    host.invocation
        .counter("s4_node_blobs_empty_tags", s4_node_blobs_empty_tags);
    host.invocation
        .counter("s4_node_blobs_kept_by_tags", s4_node_blobs_kept_by_tags);
    host.invocation.counter(
        "s4_node_blobs_kept_by_members",
        s4_node_blobs_kept_by_members,
    );
    host.invocation.counter("s4_way_blobs", s4_way_blobs);
    host.invocation
        .counter("s4_relation_blobs", s4_relation_blobs);
    host.invocation
        .counter("s4_schedule_scan_ms", t_schedule.elapsed_ms());
    way_reframe_counters.emit(&host.invocation);

    Ok(total_stats)
}

/// Process a single block for assembly.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn assemble_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    tally: &mut crate::blob_meta::AscendingTally,
) -> crate::Result<Stats> {
    let mut stats = Stats::default();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                tally.add(dn.id());
                stats.nodes_read += 1;
                let has_tags = dn.tags().next().is_some();
                if keep_untagged_nodes
                    || has_tags
                    || relation_member_node_ids.is_some_and(|ids| ids.get(dn.id()))
                {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = dense_node_metadata(dn)?;
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        dn.tags(),
                        meta.as_ref(),
                    );
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            Element::Node(n) => {
                tally.add(n.id());
                stats.nodes_read += 1;
                let has_tags = n.tags().next().is_some();
                if keep_untagged_nodes
                    || has_tags
                    || relation_member_node_ids.is_some_and(|ids| ids.get(n.id()))
                {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        n.tags(),
                        meta.as_ref(),
                    );
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            // Only node blobs are decoded here (relation blobs pass through,
            // way blobs are reframed), and the caller refused a block holding
            // another kind under the node claim.
            Element::Way(_) | Element::Relation(_) => {
                return Err(Error::internal(
                    "altw external stage 4: a decoded node blob holds a way or relation \
                     its kind check should have refused",
                ));
            }
        }
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Wire-format way reframe for stage 4
// ---------------------------------------------------------------------------

crate::counters::counter_set! {
    /// Sub-phase counters for the way reframe hot path.
    ///
    /// Timing is captured per blob (parse_block), per PrimitiveGroup
    /// (group_reframe) and per non-way field inside a group
    /// (unknown_field_copy), never per
    /// way. The earlier per-way timers were stripped after they confirmed
    /// the way path is not the wall-critical segment; per-way
    /// `Instant::now()` samples were a measurable cost at 1.16B ways/planet
    /// and provided no actionable signal beyond the blob-level attribution.
    /// Nanosecond accumulators, emitted in milliseconds.
    struct WayReframeCounters {
        parse_block_ns => "s4_way_parse_block_ms" / 1_000_000,
        unknown_field_copy_ns => "s4_way_unknown_field_copy_ms" / 1_000_000,
        group_reframe_ns => "s4_way_group_reframe_ms" / 1_000_000,
        refs_total => "s4_way_refs_total",
        refs_present => "s4_way_refs_present",
        max_refs_per_way => "s4_way_max_refs_per_way",
        lat_bytes => "s4_way_lat_bytes",
        lon_bytes => "s4_way_lon_bytes",
        ways_total => "s4_way_messages_total",
    }
}

impl WayReframeCounters {
    /// Emit the set, then the derived average, which is present only when
    /// at least one way was reframed.
    fn emit(&self, invocation: &crate::host::Invocation) {
        use std::sync::atomic::Ordering::Relaxed;
        self.emit_all(invocation);
        let total_refs = self.refs_total.load(Relaxed);
        let total_ways = self.ways_total.load(Relaxed);
        if let Some(avg) = total_refs.checked_div(total_ways) {
            invocation.counter("s4_way_avg_refs_per_way", avg);
        }
    }
}

/// Reusable scratch buffers for the way reframe path.
struct WayReframeScratch {
    group_ranges: Vec<(usize, usize)>,
    scalar_fields: Vec<u8>,
    reframed_way: Vec<u8>,
    packed_lats: Vec<u8>,
    packed_lons: Vec<u8>,
    pins: Vec<u8>,
    member_ways: Vec<bool>,
    group_out: Vec<u8>,
    /// This blob's field-20 tally under `--inject-prepass`; reset at the
    /// start of every reframe, read by the worker once it succeeds.
    pin_tally: PinTally,
}

impl WayReframeScratch {
    fn new() -> Self {
        Self {
            group_ranges: Vec::new(),
            scalar_fields: Vec::new(),
            reframed_way: Vec::new(),
            packed_lats: Vec::new(),
            packed_lons: Vec::new(),
            pins: Vec::new(),
            member_ways: Vec::new(),
            group_out: Vec::new(),
            pin_tally: PinTally::default(),
        }
    }
}

/// Wire-format reframe: splice LocationsOnWays fields (9, 10) into way
/// messages without full PrimitiveBlock decode. Copies string table, node
/// groups, relation groups, and all way fields verbatim. Per-way refcounts
/// come from the stage-1 sidecar record for this blob, so the hot path no
/// longer re-counts field-8 ref varints just to know how many coordinate
/// pairs to splice.
///
/// Returns `(way_count, min_way_id, max_way_id)`.
///
/// `coord_payload` is the per-blob delta-varint payload produced by stage 3.
/// Each way consumes `2*ref_count` varints, interleaved (lat, lon, lat,
/// lon, ...) with deltas reset per way. Because the raw varint encoding
/// matches PBF's packed field 9/10 byte-for-byte, we de-interleave by
/// copying raw varint bytes into `packed_lats` and `packed_lons` without
/// zigzag-decoding or re-encoding.
///
/// The whole-pipeline missing-location count is computed once in
/// `external_join` from the total and resolved slot counts. Stage 3 has
/// already replaced absent slots with zero coordinates in this payload;
/// those bytes cannot distinguish missing nodes from real Null Island nodes.
/// Do not derive a per-blob missing count here.
#[hotpath::measure]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn reframe_way_blob_with_locations(
    decompressed: &[u8],
    coord_payload: &[u8],
    per_way_refcount_record: &[u8],
    way_blob_idx: usize,
    output: &mut Vec<u8>,
    scratch: &mut WayReframeScratch,
    counters: &WayReframeCounters,
    member_way_ids: Option<&IdSet>,
    inject_prepass: bool,
) -> crate::Result<(u64, i64, i64)> {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};
    use std::sync::atomic::Ordering::Relaxed;

    scratch.member_ways.clear();
    if inject_prepass {
        scratch.pin_tally = PinTally::default();
    }

    // Level 1: PrimitiveBlock - find string table + groups. Shared with the
    // sparse reframe, including its coordinate-framing refusal: this path
    // appends decimicrodegree fields 9 / 10 alongside a copied granularity /
    // offset triple exactly as that one does.
    let t_block = crate::counters::Stopwatch::start();
    let (st_offset, st_len) = super::super::reframe::parse_way_block_top(
        decompressed,
        &mut scratch.group_ranges,
        &mut scratch.scalar_fields,
    )?;
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    t_block.record_ns(&counters.parse_block_ns);

    let mut refcount_cursor = Cursor::new(per_way_refcount_record);
    let expected_ways = refcount_cursor.read_varint().map_err(|e| {
        Error::from(e).context(format!("per-way sidecar blob {way_blob_idx} num_ways"))
    })?;
    #[allow(clippy::cast_possible_truncation)]
    let expected_ways_usize = expected_ways as usize;
    let mut sidecar_way_idx: usize = 0;

    output.clear();
    protohoggr::encode_bytes_field(output, 1, stringtable_bytes);

    let mut total_ways: u64 = 0;
    let mut min_way_id: i64 = i64::MAX;
    let mut max_way_id: i64 = i64::MIN;
    let mut blob_refs: u64 = 0;
    // Blob-local accumulators for the previously-per-way shared atomics.
    // Published once at function exit so 453M+ way iterations produce
    // ~57K atomic publishes instead of 4× that per way × 6 contending
    // workers. The atomics were previously a real chunk of the
    // "unaccounted" time inside s4_way_reframe_ms.
    let mut blob_refs_present: u64 = 0;
    let mut blob_max_refs_per_way: u64 = 0;
    let mut blob_lat_bytes: u64 = 0;
    let mut blob_lon_bytes: u64 = 0;

    // Payload cursor: position in `coord_payload` for the current way.
    // Advances as we consume varints across ways within this blob.
    let mut payload_pos: usize = 0;

    // Level 2: process each PrimitiveGroup.
    for &(gr_offset, gr_len) in &scratch.group_ranges {
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];
        scratch.group_out.clear();

        let mut gr_cursor = Cursor::new(group_bytes);
        while let Some((field, wire_type)) = gr_cursor
            .read_tag()
            .map_err(|e| Error::from(e).context("reframe gfield"))?
        {
            if field == 3 && wire_type == WIRE_LEN {
                // Way submessage - splice locations.
                let way_bytes = gr_cursor
                    .read_len_delimited()
                    .map_err(|e| Error::from(e).context("reframe way"))?;

                if sidecar_way_idx >= expected_ways_usize {
                    return Err(Error::internal(format!(
                        "blob {way_blob_idx}: encountered more way messages than the per-way sidecar record declares ({expected_ways_usize})"
                    )));
                }
                let ref_count = refcount_cursor.read_varint().map_err(|e| {
                    Error::from(e).context(format!(
                        "per-way sidecar blob {way_blob_idx} way {sidecar_way_idx}"
                    ))
                })?;
                sidecar_way_idx += 1;

                let mut way_id: i64 = 0;
                let mut has_existing_way_locations = false;

                let mut way_cursor = Cursor::new(way_bytes);
                while let Some((wf, wt)) = way_cursor
                    .read_tag()
                    .map_err(|e| Error::from(e).context("reframe wfield"))?
                {
                    match (wf, wt) {
                        (1, WIRE_VARINT) => {
                            way_id = way_cursor
                                .read_varint_i64()
                                .map_err(|e| Error::from(e).context("reframe id"))?;
                        }
                        // Both wire forms, as the sparse splice strips them:
                        // matching only the packed form let an unpacked stale
                        // lat / lon through the whole-way copy below, beside
                        // the fresh packed fields this reframe appends.
                        (9 | 10 | 20, WIRE_LEN | WIRE_VARINT) => {
                            has_existing_way_locations = true;
                            way_cursor
                                .skip_field(wt)
                                .map_err(|e| Error::from(e).context("reframe wskip"))?;
                        }
                        _ => {
                            way_cursor
                                .skip_field(wt)
                                .map_err(|e| Error::from(e).context("reframe wskip"))?;
                        }
                    }
                }

                if way_id < min_way_id {
                    min_way_id = way_id;
                }
                if way_id > max_way_id {
                    max_way_id = way_id;
                }
                scratch
                    .member_ways
                    .push(inject_prepass && member_way_ids.is_some_and(|ids| ids.get(way_id)));

                // De-interleave pre-encoded varints from coord_payload into
                // PBF packed fields 9/10. The raw varint bytes match PBF's
                // packed sint32 wire format 1:1, so we copy bytes without
                // zigzag decode + re-encode.
                scratch.packed_lats.clear();
                scratch.packed_lons.clear();
                scratch.pins.clear();
                for _ in 0..ref_count {
                    let lat_start = payload_pos;
                    while payload_pos < coord_payload.len()
                        && (coord_payload[payload_pos] & 0x80) != 0
                    {
                        payload_pos += 1;
                    }
                    if payload_pos >= coord_payload.len() {
                        return Err(Error::internal("coord_payload: truncated lat varint"));
                    }
                    payload_pos += 1;
                    scratch
                        .packed_lats
                        .extend_from_slice(&coord_payload[lat_start..payload_pos]);
                    let lon_start = payload_pos;
                    while payload_pos < coord_payload.len()
                        && (coord_payload[payload_pos] & 0x80) != 0
                    {
                        payload_pos += 1;
                    }
                    if payload_pos >= coord_payload.len() {
                        return Err(Error::internal("coord_payload: truncated lon varint"));
                    }
                    payload_pos += 1;
                    scratch
                        .packed_lons
                        .extend_from_slice(&coord_payload[lon_start..payload_pos]);
                }
                if inject_prepass {
                    let pin_ref_count = usize::try_from(ref_count)
                        .map_err(|_| Error::internal("coord_payload: ref count exceeds usize"))?;
                    let pin_bytes = pin_ref_count.div_ceil(8);
                    if payload_pos + pin_bytes > coord_payload.len() {
                        return Err(Error::internal("coord_payload: truncated pin bitmap"));
                    }
                    scratch
                        .pins
                        .extend_from_slice(&coord_payload[payload_pos..payload_pos + pin_bytes]);
                    payload_pos += pin_bytes;
                    scratch.pin_tally.record_way(&scratch.pins);
                }
                blob_refs_present += ref_count;
                if ref_count > blob_max_refs_per_way {
                    blob_max_refs_per_way = ref_count;
                }
                blob_lat_bytes += scratch.packed_lats.len() as u64;
                blob_lon_bytes += scratch.packed_lons.len() as u64;
                blob_refs += ref_count;

                // Build reframed way: original bytes + appended fields 9, 10.
                scratch.reframed_way.clear();
                if !has_existing_way_locations {
                    scratch.reframed_way.extend_from_slice(way_bytes);
                } else {
                    let mut copy_cursor = Cursor::new(way_bytes);
                    while let Some((wf, wt)) = copy_cursor
                        .read_tag()
                        .map_err(|e| Error::from(e).context("reframe copy tag"))?
                    {
                        let raw = copy_cursor
                            .read_raw_field(wt)
                            .map_err(|e| Error::from(e).context("reframe copy field"))?;
                        if !matches!((wf, wt), (9 | 10 | 20, WIRE_LEN | WIRE_VARINT)) {
                            protohoggr::encode_tag(&mut scratch.reframed_way, wf, wt);
                            scratch.reframed_way.extend_from_slice(raw);
                        }
                    }
                }
                if ref_count > 0 {
                    protohoggr::encode_bytes_field(
                        &mut scratch.reframed_way,
                        9,
                        &scratch.packed_lats,
                    );
                    protohoggr::encode_bytes_field(
                        &mut scratch.reframed_way,
                        10,
                        &scratch.packed_lons,
                    );
                    // Field order matches the sparse reframe (9, 10, 20) so the
                    // two backends produce byte-identical way payloads.
                    if inject_prepass && scratch.pins.iter().any(|&bit| bit != 0) {
                        protohoggr::encode_bytes_field(
                            &mut scratch.reframed_way,
                            20,
                            &scratch.pins,
                        );
                    }
                }

                protohoggr::encode_bytes_field(&mut scratch.group_out, 3, &scratch.reframed_way);
                total_ways += 1;
            } else {
                // Non-way field in the group - copy verbatim.
                let t_copy = crate::counters::Stopwatch::start();
                let raw = gr_cursor
                    .read_raw_field(wire_type)
                    .map_err(|e| Error::from(e).context("reframe gskip"))?;
                protohoggr::encode_tag(&mut scratch.group_out, field, wire_type);
                scratch.group_out.extend_from_slice(raw);
                t_copy.record_ns(&counters.unknown_field_copy_ns);
            }
        }

        let t_group = crate::counters::Stopwatch::start();
        protohoggr::encode_bytes_field(output, 2, &scratch.group_out);
        t_group.record_ns(&counters.group_reframe_ns);
    }

    // Append scalar fields (granularity, etc.).
    output.extend_from_slice(&scratch.scalar_fields);

    if sidecar_way_idx != expected_ways_usize {
        return Err(Error::internal(format!(
            "blob {way_blob_idx}: per-way sidecar record declared {expected_ways_usize} ways but the blob contained {sidecar_way_idx}"
        )));
    }
    if refcount_cursor.remaining() != 0 {
        return Err(Error::internal(format!(
            "per-way sidecar blob {way_blob_idx} has {} trailing bytes",
            refcount_cursor.remaining()
        )));
    }

    // Publish blob-local counter accumulators (one fetch_add each
    // instead of per-way). The shared-atomic `max_refs_per_way` still
    // needs a CAS loop because multiple blobs publish concurrently,
    // but now only once per blob.
    counters.refs_total.fetch_add(blob_refs, Relaxed);
    counters.refs_present.fetch_add(blob_refs_present, Relaxed);
    counters.lat_bytes.fetch_add(blob_lat_bytes, Relaxed);
    counters.lon_bytes.fetch_add(blob_lon_bytes, Relaxed);
    counters.ways_total.fetch_add(total_ways, Relaxed);
    if blob_max_refs_per_way > 0 {
        record_max(&counters.max_refs_per_way, blob_max_refs_per_way);
    }

    if payload_pos != coord_payload.len() {
        return Err(Error::internal(format!(
            "coord_payload: consumed {payload_pos} of {} bytes (trailing bytes indicate stage 3 over-production or version skew)",
            coord_payload.len()
        )));
    }

    Ok((total_ways, min_way_id, max_way_id))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod reframe_tests {
    use super::{WayReframeCounters, WayReframeScratch, reframe_way_blob_with_locations};

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            #[allow(clippy::cast_possible_truncation)]
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }

    fn len_delimited(field: u32, payload: &[u8], out: &mut Vec<u8>) {
        varint(u64::from((field << 3) | 2), out);
        varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    /// A block holding one way (id 42, one ref to node 9) followed by
    /// `stale`, raw way-field bytes the reframe must strip.
    fn way_block(stale: &[u8]) -> Vec<u8> {
        let mut way = vec![0x08, 42]; // field 1 (id) = 42
        len_delimited(8, &[18], &mut way); // refs: zigzag(9)
        way.extend_from_slice(stale);
        let mut group = Vec::new();
        len_delimited(3, &way, &mut group);
        let mut block = Vec::new();
        len_delimited(1, &[], &mut block);
        len_delimited(2, &group, &mut block);
        block
    }

    fn reframe(block: &[u8]) -> Vec<u8> {
        // One way with one ref; its coordinate pair as stage 3 encodes it.
        let refcounts = [1_u8, 1];
        let payload = [0x02_u8, 0x04]; // zigzag(1), zigzag(2)
        let mut out = Vec::new();
        let mut scratch = WayReframeScratch::new();
        let counters = WayReframeCounters::new();
        reframe_way_blob_with_locations(
            block,
            &payload,
            &refcounts,
            0,
            &mut out,
            &mut scratch,
            &counters,
            None,
            false,
        )
        .unwrap();
        out
    }

    /// Existing location fields are the reframe's to replace, in either wire
    /// form. Only the packed form used to be recognised, so a way carrying an
    /// unpacked stale lat or lon was copied whole and came out with the stale
    /// value beside the fresh packed one: two field-9 occurrences, which a
    /// reader concatenates into one coordinate too many.
    #[test]
    fn stale_locations_are_stripped_in_both_wire_forms() {
        let clean = reframe(&way_block(&[]));
        let mut unpacked = Vec::new();
        unpacked.extend_from_slice(&[0x48, 0x7E]); // field 9, varint
        unpacked.extend_from_slice(&[0x50, 0x7C]); // field 10, varint
        assert_eq!(
            reframe(&way_block(&unpacked)),
            clean,
            "unpacked stale fields"
        );
        let mut mixed = Vec::new();
        len_delimited(9, &[0x7E], &mut mixed);
        mixed.extend_from_slice(&[0x50, 0x7C]); // field 10, varint
        mixed.extend_from_slice(&[0xA0, 0x01, 0x01]); // field 20, varint
        assert_eq!(reframe(&way_block(&mixed)), clean, "mixed stale fields");
        let mut packed = Vec::new();
        len_delimited(9, &[0x7E], &mut packed);
        len_delimited(10, &[0x7C], &mut packed);
        assert_eq!(reframe(&way_block(&packed)), clean, "packed stale fields");
    }
}

#[cfg(test)]
mod ref_count_sidecar_tests {
    use super::load_ref_count_sidecar;

    #[test]
    fn streamed_prefixes_check_alignment_trailer_and_overflow() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("refcounts");
        let write = |counts: &[u64]| {
            let bytes: Vec<u8> = counts.iter().flat_map(|n| n.to_le_bytes()).collect();
            std::fs::write(&path, bytes).expect("sidecar");
        };
        write(&[3, 0, 2, 5]);
        assert_eq!(
            load_ref_count_sidecar(&path, 5).expect("prefixes"),
            [0, 3, 3]
        );
        assert!(load_ref_count_sidecar(&path, 4).is_err());
        write(&[u64::MAX, 1, 0]);
        assert!(load_ref_count_sidecar(&path, 0).is_err());
        write(&[0]);
        assert!(load_ref_count_sidecar(&path, 0).expect("empty").is_empty());
        std::fs::write(&path, [0_u8; 9]).expect("unaligned");
        assert!(load_ref_count_sidecar(&path, 0).is_err());
        std::fs::write(&path, b"").expect("short");
        assert!(load_ref_count_sidecar(&path, 0).is_err());
    }
}
