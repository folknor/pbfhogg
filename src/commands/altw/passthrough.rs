//! Pass 2b: passthrough output path (indexdata present).
//!
//! Descriptor-first parallel pipeline lifted from
//! `external/stage4.rs`:
//!
//!   `HeaderWalker` builds a `Vec<BlobDescriptor>` (cheap, no body
//!   reads) -> partition into decode-eligible vs passthrough-eligible
//!   -> fixed-size worker pool runs `pread` + decompress + reframe (or
//!   re-encode) per descriptor -> bounded ordered channel feeds a
//!   single consumer thread that only writes -> writer pipeline
//!   compresses + writes in parallel.
//!
//! The previous shape was a read-batch-rayon-drain stop-and-wait
//! loop: each batch read, decoded, drained, flushed before reading
//! the next batch. Decode was parallel within a batch, but read +
//! decode + write never overlapped. The descriptor-first form lets
//! all three overlap: read on the consumer (passthrough preads on
//! demand) + workers (decode preads via shared file) + decode on
//! workers + compress + write on the writer pool.
//!
//! Relation blobs are always passthrough. Node blobs are passthrough
//! only when `keep_untagged_nodes` is set (otherwise per-element
//! filtering forces full decode + re-encode). Way blobs always go
//! through the wire-format reframe in `super::reframe`.

use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;

use crate::ElementKind;
use crate::blob::{BlobKind, DecompressPool, decompress_blob_raw};
use crate::blob_meta::BlobIndex;
use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::idset::IdSet;
use crate::read::header_walker::HeaderWalker;
use crate::read::input::PinnedInput;
use crate::write::WriteOptions;
use crate::writer::PbfWriter;

use crate::commands::{HeaderOverrides, flush_local, writer_from_header_bytes};

use super::Result;
use super::inject_metrics::InjectMetrics;
use super::reframe::{WayReframeScratch, reframe_way_blob_with_locations};
use super::{NodeIndex, Stats, process_block};

/// Per-blob descriptor produced once up front by the header walk and
/// consumed by both the worker pool (decode) and the consumer thread
/// (passthrough pread + write).
///
/// Unknown index kinds are allowed here for forced unindexed input; external
/// instead requires known kinds and carries a way-sidecar index. Only their
/// passthrough eligibility policy is shared, not their measured pipelines.
#[derive(Clone, Copy)]
struct BlobDescriptor {
    /// True 0-based blob index in the input, handed out by the header walk.
    /// Provenance for locating a failure; the schedule position is the
    /// ordering key.
    blob_index: u64,
    /// Start of the on-disk 4-byte length prefix. Used by the
    /// consumer-side passthrough path to pread the entire framed blob
    /// (header + data) and hand it verbatim to
    /// `PbfWriter::write_raw_owned`.
    frame_offset: u64,
    /// `(data_offset - frame_offset) + data_size`. The exact number of
    /// bytes to pread for raw passthrough.
    frame_size: usize,
    data_offset: u64,
    data_size: usize,
    is_way_blob: bool,
    /// `true` when the blob can be passed through as raw compressed
    /// bytes without decompress + decode + re-encode. Relations always
    /// qualify; node blobs qualify only when `keep_untagged_nodes`.
    /// Ways never qualify (they need location splicing).
    is_passthrough: bool,
    /// Blob kind from indexdata; `None` only for blobs with no
    /// indexdata header (only reachable on `--force` non-indexed
    /// input). Both decode and passthrough branches treat `None` as
    /// "decode" to preserve correctness.
    kind: Option<ElementKind>,
    /// Element count from indexdata. Used to populate stats on the
    /// passthrough path (no decode available for a live count).
    count: u64,
    /// The whole indexdata claim, which a way blob's reframe must find
    /// exact.
    claim: Option<BlobIndex>,
}

crate::counters::counter_set! {
    /// Cumulative pass-2 worker counters. Ns accumulators (blob-level
    /// timing granularity), converted to ms at emit time.
    struct Pass2Counters {
        pread_ns => "altw_pass2_pread_ms" / 1_000_000,
        decompress_ns => "altw_pass2_decompress_ms" / 1_000_000,
        way_reframe_ns => "altw_pass2_way_reframe_ms" / 1_000_000,
        nonway_ns => "altw_pass2_nonway_ms" / 1_000_000,
        send_ns => "altw_pass2_send_ms" / 1_000_000,
        bytes_read => "altw_pass2_bytes_read",
        way_blobs => "altw_pass2_way_blobs_decoded",
        nonway_blobs => "altw_pass2_nonway_blobs_decoded",
    }
}

/// Walk all blob headers and build the schedule. The OsmHeader blob (the
/// walker refuses a second one) is used to build the output header bytes.
///
/// This is the fourth header walk on the sparse path, and the marker pair
/// exists because its cost is invisible otherwise and swings enormously:
/// a walk is ~50 us/blob cold and near free warm, decided entirely by
/// whether the preceding phase left the headers in page cache. Measured
/// 2026-07-14 at north-america: ~1 s when the relation-member scan walked
/// just before it, 23.0 s when it did not. Do not try to remove the
/// redundant walks on the strength of their durations: unifying them was
/// built twice and reverted (2026-07-14). The saving equals the baseline's
/// cold-walk penalty only, near zero where the store fits page cache;
/// consolidating the other walks but not this one left it walking cold after
/// pass 1's eviction (+3.8 %). See reference/blob-density.md.
#[hotpath::measure]
fn build_schedule(
    input: &PinnedInput,
    overrides: &HeaderOverrides,
    keep_untagged_nodes: bool,
    inject_prepass: bool,
    host: &crate::host::Host,
) -> Result<(Vec<BlobDescriptor>, Vec<u8>, std::sync::Arc<std::fs::File>)> {
    host.invocation.marker("ALTW_PASS2_SCHEDULE_WALK_START");
    let mut walker = HeaderWalker::open(input, host)?;
    let mut header_bytes: Option<Vec<u8>> = None;
    let mut schedule: Vec<BlobDescriptor> = Vec::new();

    while let Some(meta) = walker.next_header()? {
        match meta.blob_type {
            BlobKind::OsmHeader => {
                let mut data_buf: Vec<u8> = vec![0; meta.data_size];
                walker
                    .shared_file()
                    .read_exact_at(&mut data_buf, meta.data_offset)
                    .map_err(|e| walker.locate_meta(&meta, e.into()))?;
                let header = crate::blob::decode_blob_to_headerblock(&data_buf)
                    .map_err(|e| walker.locate_meta(&meta, e))?;
                let bytes = crate::commands::output_header::build_one(
                    &header,
                    &super::altw_output_facts(inject_prepass),
                    overrides,
                    host,
                )?
                .bytes;
                header_bytes = Some(bytes);
            }
            BlobKind::Unknown(_) => continue,
            BlobKind::OsmData => {
                let kind = meta.index.as_ref().map(|idx| idx.kind);
                let count = meta.index.as_ref().map_or(0, |idx| idx.count);

                let is_way_blob = matches!(kind, Some(ElementKind::Way));
                let is_passthrough = super::blob_is_passthrough(kind, keep_untagged_nodes);

                schedule.push(BlobDescriptor {
                    blob_index: meta.blob_index,
                    frame_offset: meta.frame_start,
                    frame_size: meta.frame_size,
                    data_offset: meta.data_offset,
                    data_size: meta.data_size,
                    is_way_blob,
                    is_passthrough,
                    kind,
                    count,
                    claim: meta.index,
                });
            }
        }
    }

    let header_bytes =
        header_bytes.ok_or_else(|| crate::error::new_missing_header(input.path()))?;
    let shared_file = std::sync::Arc::clone(walker.shared_file());
    host.invocation.marker("ALTW_PASS2_SCHEDULE_WALK_END");
    Ok((schedule, header_bytes, shared_file))
}

/// Per-worker scratch reused across decode descriptors.
struct WorkerScratch {
    read_buf: Vec<u8>,
    decompress_buf: Vec<u8>,
    bb: BlockBuilder,
    refs_buf: Vec<i64>,
    locations_buf: Vec<(i32, i32)>,
    pool: std::sync::Arc<DecompressPool>,
    way_scratch: WayReframeScratch,
    reframe_output: Vec<u8>,
    output: Vec<OwnedBlock>,
    /// String-table and group-range scratch for `parse_and_inline_with_scratch`
    /// on the non-way decode path. Retained across descriptors so the per-block
    /// `Vec<(u32, u32)>` allocation is eliminated.
    st_scratch: Vec<(u32, u32)>,
    gr_scratch: Vec<(u32, u32)>,
}

impl WorkerScratch {
    fn new(invocation: &crate::host::Invocation) -> Self {
        Self {
            read_buf: Vec::new(),
            decompress_buf: Vec::new(),
            bb: BlockBuilder::new(),
            refs_buf: Vec::new(),
            locations_buf: Vec::new(),
            pool: DecompressPool::new(invocation),
            way_scratch: WayReframeScratch::default(),
            reframe_output: Vec::new(),
            output: Vec::new(),
            st_scratch: Vec::new(),
            gr_scratch: Vec::new(),
        }
    }
}

/// Decode one descriptor. Way blobs go through the wire-format
/// reframe; everything else (Node decode, Unknown decode) goes through
/// the existing `PrimitiveBlock` + `BlockBuilder` path.
#[hotpath::measure]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_one(
    desc: &BlobDescriptor,
    file: &std::sync::Arc<std::fs::File>,
    node_index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    relation_member_way_ids: Option<&IdSet>,
    shared_node_ids: Option<&IdSet>,
    inject_prepass: bool,
    inject_metrics: &InjectMetrics,
    scratch: &mut WorkerScratch,
    counters: &Pass2Counters,
) -> crate::Result<(Vec<OwnedBlock>, Stats)> {
    let t_pread = crate::counters::Stopwatch::start();
    scratch.read_buf.resize(desc.data_size, 0);
    file.read_exact_at(&mut scratch.read_buf, desc.data_offset)
        .map_err(|e| crate::error::Error::from(e).context("pass 2 pread"))?;
    t_pread.record_ns(&counters.pread_ns);
    counters
        .bytes_read
        .fetch_add(desc.data_size as u64, Relaxed);
    scratch.output.clear();

    if desc.is_way_blob {
        let t_dc = crate::counters::Stopwatch::start();
        decompress_blob_raw(&scratch.read_buf, &mut scratch.decompress_buf)?;
        t_dc.record_ns(&counters.decompress_ns);
        let t_reframe = crate::counters::Stopwatch::start();
        let stats = reframe_way_blob_with_locations(
            &scratch.decompress_buf,
            node_index,
            &mut scratch.reframe_output,
            &mut scratch.way_scratch,
            shared_node_ids,
            relation_member_way_ids,
            inject_prepass,
        )?;
        // The output index below is the reframe's count and range, so a
        // claim it does not match exactly (a blob claiming ways that holds
        // none above all, which would be re-indexed as Way count 0 over IDs
        // 0..0) is refused rather than rewritten. Pass 0 already refused a
        // foreign kind in this blob.
        if let Some(claim) = &desc.claim {
            crate::blob_meta::IdTally::of(stats.way_count, stats.min_way_id, stats.max_way_id)
                .refuse_inexact(claim)?;
        }
        let index = BlobIndex {
            kind: ElementKind::Way,
            min_id: stats.min_way_id,
            max_id: stats.max_way_id,
            count: stats.way_count,
            bbox: None,
        };
        scratch.output.push(OwnedBlock {
            bytes: std::mem::take(&mut scratch.reframe_output),
            index,
            tagdata: None,
            way_members: inject_prepass.then(|| {
                let payload = crate::commands::altw::reframe::way_members_payload(
                    stats.way_count,
                    &scratch.way_scratch.member_ways,
                );
                inject_metrics.record_blob(scratch.way_scratch.pin_tally, payload.len());
                payload
            }),
        });
        let block_stats = Stats {
            ways_written: stats.way_count,
            missing_locations: stats.missing_locations,
            blobs_decoded: 1,
            ..Stats::default()
        };
        t_reframe.record_ns(&counters.way_reframe_ns);
        counters.way_blobs.fetch_add(1, Relaxed);
        return Ok((std::mem::take(&mut scratch.output), block_stats));
    }

    // Non-way decode (Node decode when keep_untagged_nodes=false; Unknown).
    // Decompress straight into a pooled buffer and parse it in place. The buffer
    // is fetched from and returned to the same `DecompressPool` (the block's
    // backing `Bytes` recycles it on drop), preserving the pooling economics of
    // the old `decompress_blob(.., Some(pool))` route while dropping the extra
    // whole-buffer copy that `parse_primitive_block_from_bytes_owned`'s
    // `Bytes -> to_vec` incurred. Also skips the `WireBlob::parse_slice` copy the
    // old route paid, going straight from the framed blob bytes via
    // `decompress_blob_raw` - the same wire-level path the way branch above uses.
    let t_dc = crate::counters::Stopwatch::start();
    let mut decompressed = crate::blob::pool_get(Some(&scratch.pool), 0);
    decompress_blob_raw(&scratch.read_buf, &mut decompressed)?;
    t_dc.record_ns(&counters.decompress_ns);
    let t_nonway = crate::counters::Stopwatch::start();
    let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
        decompressed,
        &scratch.pool,
        &mut scratch.st_scratch,
        &mut scratch.gr_scratch,
    )?;
    // A way in a blob claiming nodes would be written without locations (its
    // refs were never collected in pass 0) and outside every field-5
    // payload; a relation would have missed the member scan. Pass 1 refused
    // either already, since it decodes every node blob; this is the same
    // one-compare refusal at the decode that would publish it, the one
    // external's stage 4 makes. An unindexed blob (`--force`) carries no
    // claim and is handled by its elements' real kinds.
    block.refuse_foreign_kind(desc.kind)?;
    let block_stats = process_block(
        &block,
        &mut scratch.bb,
        &mut scratch.output,
        node_index,
        keep_untagged_nodes,
        relation_member_node_ids,
        &mut scratch.refs_buf,
        &mut scratch.locations_buf,
    )?;
    flush_local(&mut scratch.bb, &mut scratch.output)?;
    let mut block_stats = block_stats;
    block_stats.blobs_decoded = 1;
    t_nonway.record_ns(&counters.nonway_ns);
    counters.nonway_blobs.fetch_add(1, Relaxed);
    Ok((std::mem::take(&mut scratch.output), block_stats))
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn write_output_passthrough(
    input: &PinnedInput,
    output: &crate::output::BoundOutput,
    node_index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    relation_member_way_ids: Option<&IdSet>,
    shared_node_ids: Option<&IdSet>,
    inject_prepass: bool,
    inject_metrics: &InjectMetrics,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<Stats> {
    let (schedule, header_bytes, shared_file) =
        build_schedule(input, overrides, keep_untagged_nodes, inject_prepass, host)?;

    let mut writer = writer_from_header_bytes(
        output,
        write.compression,
        &header_bytes,
        write.backend.parallel_by_default(),
        host,
    )?;

    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );

    // Schedule composition diagnostics.
    {
        let (mut way_blobs, mut node_blobs, mut relation_blobs): (i64, i64, i64) = (0, 0, 0);
        for d in &schedule {
            match d.kind {
                Some(ElementKind::Way) => way_blobs += 1,
                Some(ElementKind::Node) => node_blobs += 1,
                Some(ElementKind::Relation) => relation_blobs += 1,
                None => {}
            }
        }
        host.invocation.counter("altw_pass2_way_blobs", way_blobs);
        host.invocation.counter("altw_pass2_node_blobs", node_blobs);
        host.invocation
            .counter("altw_pass2_relation_blobs", relation_blobs);
    }

    // Decode items go to the workers of the ordered pool, which runs over
    // them alone; passthrough items are copied by the consumer between them,
    // in schedule order, so input element ordering (nodes -> ways ->
    // relations) is preserved.
    let decode_at: Vec<usize> = schedule
        .iter()
        .enumerate()
        .filter(|(_, d)| !d.is_passthrough)
        .map(|(i, _)| i)
        .collect();
    host.invocation
        .counter("altw_pass2_decode_items", decode_at.len());
    host.invocation.counter(
        "altw_pass2_passthrough_items",
        schedule.len() - decode_at.len(),
    );
    host.invocation
        .counter("altw_pass2_decode_threads", decode_threads);
    let decode_at = &decode_at;
    let schedule = &schedule;

    // Worker-side cumulative counters + the depth-gated WAIT span for
    // workers blocked handing results to the consumer (same StallGauge
    // rationale as external stage 4: N concurrent blockers collapse to
    // one non-overlapping span per busy period so `brokkr sidecar
    // --durations` can pair the markers; `--stalls` reads `*_wait_ns`
    // counters, not these spans; entered only after a failed try_send).
    let p2_counters = Pass2Counters::default();
    let p2_send_stall = crate::counters::StallGauge::new(
        "WAIT_P2_SEND_START",
        "WAIT_P2_SEND_END",
        &host.invocation,
    );
    let p2_counters_ref = &p2_counters;
    // The input's identity, captured once; each worker borrows it.
    let source: std::sync::Arc<Path> = std::sync::Arc::from(input.path());
    let source = &source;
    // The consumer's receive wait, in nanoseconds.
    let p2_recv_ns = std::sync::atomic::AtomicU64::new(0);

    let mut consumer = Pass2Consumer {
        interleaved: crate::ordered_pool::Interleaved::new(decode_at, schedule.len()),
        schedule,
        file: &shared_file,
        frame_read_buf: Vec::new(),
        writer: &mut writer,
        total_stats: Stats::default(),
        write_ns: 0,
        pt_pread_ns: 0,
        pt_bytes: 0,
        pt_blobs: 0,
        blobs_dispatched: 0,
        cancel: &host.invocation,
    };

    // Cancellation is checked on the consumer, before each wait and on each
    // arrival; the early return is the write-error exit.
    let mut workers = vec![(); decode_threads];
    crate::ordered_pool::run_ordered(
        decode_at.len(),
        &mut workers,
        crate::ordered_pool::PoolConfig {
            window: crate::ordered_pool::default_window(decode_threads),
            cancel: &host.invocation,
            waits: crate::ordered_pool::Waits {
                recv: Some(crate::ordered_pool::Timer::Ns(&p2_recv_ns)),
                send: Some(crate::ordered_pool::Timer::Ns(&p2_counters.send_ns)),
                send_stall: Some(&p2_send_stall),
                ..crate::ordered_pool::Waits::default()
            },
        },
        |_| WorkerScratch::new(&host.invocation),
        |(), scratch: &mut WorkerScratch, ordinal| {
            let desc = &schedule[decode_at[ordinal]];
            decode_one(
                desc,
                &shared_file,
                node_index,
                keep_untagged_nodes,
                relation_member_node_ids,
                relation_member_way_ids,
                shared_node_ids,
                inject_prepass,
                inject_metrics,
                scratch,
                p2_counters_ref,
            )
            // Cold: only a failure builds the location, from the true
            // blob index and frame start the walker handed out.
            .map_err(|e| {
                e.locate(crate::error::Location::new(
                    Some(source),
                    Some(desc.blob_index),
                    Some(crate::ByteOffset(desc.frame_offset)),
                ))
            })
        },
        &mut consumer,
    )?;
    for position in consumer.interleaved.rest() {
        consumer.passthrough(position)?;
    }
    let Pass2Consumer {
        total_stats,
        write_ns: p2_write_ns,
        pt_pread_ns: p2_pt_pread_ns,
        pt_bytes: p2_pt_bytes,
        pt_blobs: p2_pt_blobs,
        blobs_dispatched,
        ..
    } = consumer;
    let p2_recv_ns = p2_recv_ns.into_inner();

    writer.flush()?;

    p2_counters.emit_all(&host.invocation);
    host.invocation
        .counter("altw_pass2_consumer_recv_ms", p2_recv_ns / 1_000_000);
    host.invocation
        .counter("altw_pass2_consumer_write_ms", p2_write_ns / 1_000_000);
    host.invocation.counter(
        "altw_pass2_passthrough_pread_ms",
        p2_pt_pread_ns / 1_000_000,
    );
    host.invocation
        .counter("altw_pass2_passthrough_bytes", p2_pt_bytes);
    host.invocation
        .counter("altw_pass2_passthrough_blobs", p2_pt_blobs);
    host.invocation
        .counter("altw_pass2_blobs_dispatched", blobs_dispatched);
    Ok(total_stats)
}

/// Pass 2's consumer: decoded blobs from the workers and passthrough raw
/// frames it preads itself, written in schedule order. Its counters are
/// plain fields on the single consumer thread.
struct Pass2Consumer<'a> {
    interleaved: crate::ordered_pool::Interleaved<'a>,
    schedule: &'a [BlobDescriptor],
    file: &'a std::fs::File,
    frame_read_buf: Vec<u8>,
    writer: &'a mut PbfWriter<crate::file_writer::FileWriter>,
    total_stats: Stats,
    write_ns: u64,
    pt_pread_ns: u64,
    pt_bytes: u64,
    pt_blobs: u64,
    blobs_dispatched: u64,
    cancel: &'a crate::host::Invocation,
}

impl Pass2Consumer<'_> {
    /// pread the passthrough frame at schedule position `position` and write
    /// it verbatim.
    fn passthrough(&mut self, position: usize) -> Result<()> {
        self.cancel.check()?;
        let desc = &self.schedule[position];
        let frame_size = desc.frame_size;
        let t_pt = crate::counters::Stopwatch::start();
        self.frame_read_buf.resize(frame_size, 0);
        self.file
            .read_exact_at(&mut self.frame_read_buf, desc.frame_offset)?;
        self.pt_pread_ns += t_pt.elapsed_ns();
        self.pt_bytes += frame_size as u64;
        self.pt_blobs += 1;
        let t_write = crate::counters::Stopwatch::start();
        self.writer.write_raw_owned(std::mem::replace(
            &mut self.frame_read_buf,
            Vec::with_capacity(frame_size),
        ))?;
        self.write_ns += t_write.elapsed_ns();
        self.total_stats.blobs_passthrough += 1;
        match desc.kind {
            Some(ElementKind::Node) => {
                self.total_stats.nodes_read += desc.count;
                self.total_stats.nodes_written += desc.count;
            }
            Some(ElementKind::Relation) => {
                self.total_stats.relations_written += desc.count;
            }
            Some(ElementKind::Way) | None => {
                // Ways never pass through (they need location splicing).
                // `None` means a blob with no indexdata reached the consumer
                // as passthrough, which the schedule filter excludes by
                // construction (only Relation/Node can be passthrough).
                // Stats untouched.
            }
        }
        Ok(())
    }
}

impl crate::ordered_pool::OrderedConsumer<(Vec<OwnedBlock>, Stats)> for Pass2Consumer<'_> {
    fn before_item(&mut self, ordinal: usize) -> crate::Result<()> {
        for position in self.interleaved.before(ordinal) {
            self.passthrough(position)?;
        }
        Ok(())
    }

    fn consume(
        &mut self,
        _ordinal: usize,
        (blocks, block_stats): (Vec<OwnedBlock>, Stats),
    ) -> crate::Result<()> {
        self.total_stats.merge(&block_stats);
        let t_write = crate::counters::Stopwatch::start();
        for block in blocks {
            self.writer.write_primitive_block_owned(block)?;
        }
        self.write_ns += t_write.elapsed_ns();
        // Emit the progress counter every 64 blobs (the decode-all path's
        // cadence); per-blob emission was ~22 K FIFO writes at europe for no
        // extra signal.
        self.blobs_dispatched += 1;
        if self.blobs_dispatched.is_multiple_of(64) {
            self.cancel
                .counter("altw_pass2_blobs_dispatched", self.blobs_dispatched);
        }
        Ok(())
    }
}
