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
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

use crate::blob::{BlobKind, DecompressPool, decompress_blob_raw};
use crate::blob_meta::{BlobIndex, ElemKind};
use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::idset::IdSet;
use crate::read::header_walker::HeaderWalker;
use crate::reorder_buffer::ReorderBuffer;
use crate::writer::{Compression, PbfWriter};

use crate::commands::{
    HeaderOverrides, build_output_header, flush_local, writer_from_header_bytes_parallel,
};

use super::Result;
use super::reframe::{WayReframeScratch, reframe_way_blob_with_locations};
use super::{NodeIndex, Stats, process_block};

/// Per-blob descriptor produced once up front by the header walk and
/// consumed by both the worker pool (decode) and the consumer thread
/// (passthrough pread + write).
#[derive(Clone, Copy)]
struct BlobDescriptor {
    seq: usize,
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
    kind: Option<ElemKind>,
    /// Element count from indexdata. Used to populate stats on the
    /// passthrough path (no decode available for a live count).
    count: u64,
}

/// Cumulative pass-2 worker counters. Ns accumulators (blob-level
/// timing granularity), converted to ms at emit time.
#[derive(Default)]
struct Pass2Counters {
    pread_ns: AtomicU64,
    decompress_ns: AtomicU64,
    way_reframe_ns: AtomicU64,
    nonway_ns: AtomicU64,
    send_ns: AtomicU64,
    bytes_read: AtomicU64,
    way_blobs: AtomicU64,
    nonway_blobs: AtomicU64,
}

/// Walk all blob headers and build the schedule. The first OsmHeader
/// blob is consumed and used to build the output header bytes.
#[hotpath::measure]
fn build_schedule(
    input: &Path,
    overrides: &HeaderOverrides,
    keep_untagged_nodes: bool,
    inject_prepass: bool,
) -> Result<(Vec<BlobDescriptor>, Vec<u8>, std::sync::Arc<std::fs::File>)> {
    let mut walker = HeaderWalker::open(input)?;
    let mut header_bytes: Option<Vec<u8>> = None;
    let mut schedule: Vec<BlobDescriptor> = Vec::new();
    let mut seq: usize = 0;

    while let Some(meta) = walker.next_header()? {
        match meta.blob_type {
            BlobKind::OsmHeader => {
                let mut data_buf: Vec<u8> = vec![0; meta.data_size];
                walker
                    .shared_file()
                    .read_exact_at(&mut data_buf, meta.data_offset)
                    .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                let header = crate::blob::decode_blob_to_headerblock(&data_buf)?;
                let bytes = build_output_header(&header, true, overrides, |hb| {
                    let mut hb = hb.optional_feature("LocationsOnWays");
                    if inject_prepass {
                        hb = hb.optional_feature(crate::HeaderBlock::WAY_MEMBERS_V1);
                        hb = hb.optional_feature(crate::HeaderBlock::SHARED_NODE_PINS_V1);
                    }
                    hb
                })?;
                header_bytes = Some(bytes);
            }
            BlobKind::Unknown(_) => continue,
            BlobKind::OsmData => {
                let kind = meta.index.as_ref().map(|idx| idx.kind);
                let count = meta.index.as_ref().map_or(0, |idx| idx.count);

                let is_way_blob = matches!(kind, Some(ElemKind::Way));
                let is_passthrough = matches!(kind, Some(ElemKind::Relation))
                    || matches!(kind, Some(ElemKind::Node) if keep_untagged_nodes);

                schedule.push(BlobDescriptor {
                    seq,
                    frame_offset: meta.frame_start,
                    frame_size: meta.frame_size,
                    data_offset: meta.data_offset,
                    data_size: meta.data_size,
                    is_way_blob,
                    is_passthrough,
                    kind,
                    count,
                });
                seq += 1;
            }
        }
    }

    let header_bytes = header_bytes
        .ok_or_else(|| -> Box<dyn std::error::Error> { "no OSMHeader blob found".into() })?;
    let shared_file = std::sync::Arc::clone(walker.shared_file());
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
    fn new() -> Self {
        Self {
            read_buf: Vec::new(),
            decompress_buf: Vec::new(),
            bb: BlockBuilder::new(),
            refs_buf: Vec::new(),
            locations_buf: Vec::new(),
            pool: DecompressPool::new(),
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
    scratch: &mut WorkerScratch,
    counters: &Pass2Counters,
) -> std::result::Result<(Vec<OwnedBlock>, Stats), String> {
    let t_pread = std::time::Instant::now();
    scratch.read_buf.resize(desc.data_size, 0);
    file.read_exact_at(&mut scratch.read_buf, desc.data_offset)
        .map_err(|e| format!("pass 2 pread: {e}"))?;
    #[allow(clippy::cast_possible_truncation)]
    counters
        .pread_ns
        .fetch_add(t_pread.elapsed().as_nanos() as u64, Relaxed);
    counters
        .bytes_read
        .fetch_add(desc.data_size as u64, Relaxed);
    scratch.output.clear();

    if desc.is_way_blob {
        let t_dc = std::time::Instant::now();
        decompress_blob_raw(&scratch.read_buf, &mut scratch.decompress_buf)
            .map_err(|e| e.to_string())?;
        #[allow(clippy::cast_possible_truncation)]
        counters
            .decompress_ns
            .fetch_add(t_dc.elapsed().as_nanos() as u64, Relaxed);
        let t_reframe = std::time::Instant::now();
        let stats = reframe_way_blob_with_locations(
            &scratch.decompress_buf,
            node_index,
            &mut scratch.reframe_output,
            &mut scratch.way_scratch,
            shared_node_ids,
            relation_member_way_ids,
            inject_prepass,
        )?;
        let index = BlobIndex {
            kind: ElemKind::Way,
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
                crate::commands::altw::inject_metrics::record_field5_bytes(payload.len());
                payload
            }),
        });
        let block_stats = Stats {
            ways_written: stats.way_count,
            missing_locations: stats.missing_locations,
            blobs_decoded: 1,
            ..Stats::default()
        };
        #[allow(clippy::cast_possible_truncation)]
        counters
            .way_reframe_ns
            .fetch_add(t_reframe.elapsed().as_nanos() as u64, Relaxed);
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
    let t_dc = std::time::Instant::now();
    let mut decompressed = crate::blob::pool_get_pub(&scratch.pool, 0);
    decompress_blob_raw(&scratch.read_buf, &mut decompressed).map_err(|e| e.to_string())?;
    #[allow(clippy::cast_possible_truncation)]
    counters
        .decompress_ns
        .fetch_add(t_dc.elapsed().as_nanos() as u64, Relaxed);
    let t_nonway = std::time::Instant::now();
    let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
        decompressed,
        &scratch.pool,
        &mut scratch.st_scratch,
        &mut scratch.gr_scratch,
    )
    .map_err(|e| e.to_string())?;
    // A Way in a blob the index classified as non-way would be emitted without
    // field 9/10/20 and would be absent from every field-5 payload, silently
    // breaking the WayMembers superset. Reject it, mirroring the external
    // backend's assemble_block guard.
    if inject_prepass
        && block
            .elements_skip_metadata()
            .any(|e| matches!(e, crate::Element::Way(_)))
    {
        return Err(format!(
            "--inject-prepass: blob {} classified as non-way contains a Way element; \
             refusing to emit unenriched ways",
            desc.seq,
        ));
    }
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
    #[allow(clippy::cast_possible_truncation)]
    counters
        .nonway_ns
        .fetch_add(t_nonway.elapsed().as_nanos() as u64, Relaxed);
    counters.nonway_blobs.fetch_add(1, Relaxed);
    Ok((std::mem::take(&mut scratch.output), block_stats))
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn write_output_passthrough(
    input: &Path,
    output: &Path,
    node_index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    relation_member_way_ids: Option<&IdSet>,
    shared_node_ids: Option<&IdSet>,
    inject_prepass: bool,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    let (schedule, header_bytes, shared_file) =
        build_schedule(input, overrides, keep_untagged_nodes, inject_prepass)?;

    let mut writer =
        writer_from_header_bytes_parallel(output, compression, &header_bytes, direct_io, false)?;

    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    // Schedule composition diagnostics.
    {
        let (mut way_blobs, mut node_blobs, mut relation_blobs): (i64, i64, i64) = (0, 0, 0);
        for d in &schedule {
            match d.kind {
                Some(ElemKind::Way) => way_blobs += 1,
                Some(ElemKind::Node) => node_blobs += 1,
                Some(ElemKind::Relation) => relation_blobs += 1,
                None => {}
            }
        }
        crate::debug::emit_counter("altw_pass2_way_blobs", way_blobs);
        crate::debug::emit_counter("altw_pass2_node_blobs", node_blobs);
        crate::debug::emit_counter("altw_pass2_relation_blobs", relation_blobs);
    }

    // Partition: passthrough items are pre-seeded into the reorder
    // buffer at their global seq positions; decode items go through
    // the worker channel.
    let (decode_items, passthrough_items): (Vec<BlobDescriptor>, Vec<BlobDescriptor>) =
        schedule.into_iter().partition(|d| !d.is_passthrough);
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("altw_pass2_decode_items", decode_items.len() as i64);
        crate::debug::emit_counter(
            "altw_pass2_passthrough_items",
            passthrough_items.len() as i64,
        );
        crate::debug::emit_counter("altw_pass2_decode_threads", decode_threads as i64);
    }

    // Worker-side cumulative counters + the depth-gated WAIT span for
    // workers blocked handing results to the consumer (same StallGauge
    // rationale as external stage 4: N concurrent blockers collapse to
    // one non-overlapping span per busy period so `--stalls` can pair
    // the markers; entered only after a failed try_send).
    let p2_counters = Pass2Counters::default();
    let p2_send_stall = super::external::StallGauge::new("WAIT_P2_SEND_START", "WAIT_P2_SEND_END");
    let p2_counters_ref = &p2_counters;
    let p2_send_stall_ref = &p2_send_stall;

    // Consumer-side counters (single thread; plain locals).
    let mut p2_recv_ns: u64 = 0;
    let mut p2_write_ns: u64 = 0;
    let mut p2_pt_pread_ns: u64 = 0;
    let mut p2_pt_bytes: u64 = 0;
    let mut p2_pt_blobs: u64 = 0;

    type DecodedItem = (usize, std::result::Result<(Vec<OwnedBlock>, Stats), String>);
    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<BlobDescriptor>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);

    let batches_dispatched = AtomicI64::new(0);

    let mut total_stats = Stats::default();

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher: feed only decode-eligible descriptors into the
        // worker channel. Passthrough descriptors bypass workers and
        // never travel through this channel.
        scope.spawn(move || {
            for desc in decode_items {
                if desc_tx.send(desc).is_err() {
                    break;
                }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = decoded_tx.clone();
            let file = std::sync::Arc::clone(&shared_file);
            scope.spawn(move || {
                let mut scratch = WorkerScratch::new();
                loop {
                    let desc = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(d) => d,
                            Err(_) => break,
                        }
                    };
                    let result = decode_one(
                        &desc,
                        &file,
                        node_index,
                        keep_untagged_nodes,
                        relation_member_node_ids,
                        relation_member_way_ids,
                        shared_node_ids,
                        inject_prepass,
                        &mut scratch,
                        p2_counters_ref,
                    );
                    let t_send = std::time::Instant::now();
                    match tx.try_send((desc.seq, result)) {
                        Ok(()) => {}
                        Err(std::sync::mpsc::TrySendError::Full(item)) => {
                            let _stall = p2_send_stall_ref.track();
                            if tx.send(item).is_err() {
                                break;
                            }
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    p2_counters_ref
                        .send_ns
                        .fetch_add(t_send.elapsed().as_nanos() as u64, Relaxed);
                }
            });
        }
        drop(desc_rx);
        drop(decoded_tx);

        // Consumer: reorder + write. Pre-seed passthrough items at
        // their global seq positions; decode results arrive on
        // `decoded_rx` and are inserted by seq. The drain pops only
        // contiguous ready items so input element ordering (nodes ->
        // ways -> relations) is preserved.
        enum ConsumerItem {
            Decoded(std::result::Result<(Vec<OwnedBlock>, Stats), String>),
            Passthrough {
                frame_offset: u64,
                frame_size: usize,
                count: u64,
                kind: Option<ElemKind>,
            },
        }

        let mut reorder: ReorderBuffer<ConsumerItem> =
            ReorderBuffer::with_capacity(passthrough_items.len() + decode_threads);

        for desc in &passthrough_items {
            reorder.push(
                desc.seq,
                ConsumerItem::Passthrough {
                    frame_offset: desc.frame_offset,
                    frame_size: desc.frame_size,
                    count: desc.count,
                    kind: desc.kind,
                },
            );
        }

        let mut frame_read_buf: Vec<u8> = Vec::new();

        let mut drain = |reorder: &mut ReorderBuffer<ConsumerItem>,
                         total_stats: &mut Stats,
                         frame_read_buf: &mut Vec<u8>,
                         writer: &mut PbfWriter<crate::file_writer::FileWriter>|
         -> Result<()> {
            while let Some(item) = reorder.pop_ready() {
                match item {
                    ConsumerItem::Decoded(result) => {
                        let (blocks, block_stats) =
                            result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                        total_stats.merge(&block_stats);
                        let t_write = std::time::Instant::now();
                        for OwnedBlock {
                            bytes: block_bytes,
                            index,
                            tagdata,
                            way_members,
                        } in blocks
                        {
                            writer.write_primitive_block_owned(
                                block_bytes,
                                index,
                                tagdata.as_deref(),
                                way_members.as_deref(),
                            )?;
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        {
                            p2_write_ns += t_write.elapsed().as_nanos() as u64;
                        }
                        // Emit the progress counter every 64 blobs (the
                        // decode-all path's cadence); per-blob emission was
                        // ~22 K FIFO writes at europe for no extra signal.
                        let n = batches_dispatched
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        if n % 64 == 0 {
                            crate::debug::emit_counter("altw_pass2_blobs_dispatched", n);
                        }
                    }
                    ConsumerItem::Passthrough {
                        frame_offset,
                        frame_size,
                        count,
                        kind,
                    } => {
                        let t_pt = std::time::Instant::now();
                        frame_read_buf.resize(frame_size, 0);
                        shared_file
                            .read_exact_at(frame_read_buf, frame_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        #[allow(clippy::cast_possible_truncation)]
                        {
                            p2_pt_pread_ns += t_pt.elapsed().as_nanos() as u64;
                        }
                        p2_pt_bytes += frame_size as u64;
                        p2_pt_blobs += 1;
                        let t_write = std::time::Instant::now();
                        writer.write_raw_owned(std::mem::replace(
                            frame_read_buf,
                            Vec::with_capacity(frame_size),
                        ))?;
                        #[allow(clippy::cast_possible_truncation)]
                        {
                            p2_write_ns += t_write.elapsed().as_nanos() as u64;
                        }
                        total_stats.blobs_passthrough += 1;
                        match kind {
                            Some(ElemKind::Node) => {
                                total_stats.nodes_read += count;
                                total_stats.nodes_written += count;
                            }
                            Some(ElemKind::Relation) => {
                                total_stats.relations_written += count;
                            }
                            Some(ElemKind::Way) | None => {
                                // Ways never pass through (they need
                                // location splicing). `None` means a
                                // blob with no indexdata reached the
                                // consumer as passthrough, which the
                                // schedule filter excludes by
                                // construction (only Relation/Node
                                // can be passthrough). Stats untouched.
                            }
                        }
                    }
                }
            }
            Ok(())
        };

        // Drain any passthrough prefix before the first decode result.
        drain(
            &mut reorder,
            &mut total_stats,
            &mut frame_read_buf,
            &mut writer,
        )?;

        loop {
            let t_recv = std::time::Instant::now();
            let msg = decoded_rx.recv();
            #[allow(clippy::cast_possible_truncation)]
            {
                p2_recv_ns += t_recv.elapsed().as_nanos() as u64;
            }
            let (seq_num, item) = match msg {
                Ok(v) => v,
                Err(_) => break,
            };
            reorder.push(seq_num, ConsumerItem::Decoded(item));
            drain(
                &mut reorder,
                &mut total_stats,
                &mut frame_read_buf,
                &mut writer,
            )?;
        }

        // Final drain for passthrough tails (relations sit at EOF in
        // sorted PBFs - no trailing decode push to trigger pop_ready).
        drain(
            &mut reorder,
            &mut total_stats,
            &mut frame_read_buf,
            &mut writer,
        )?;

        Ok(())
    })?;

    writer.flush()?;

    #[allow(clippy::cast_possible_wrap)]
    {
        let ns_to_ms = |ns: u64| (ns / 1_000_000) as i64;
        crate::debug::emit_counter(
            "altw_pass2_pread_ms",
            ns_to_ms(p2_counters.pread_ns.load(Relaxed)),
        );
        crate::debug::emit_counter(
            "altw_pass2_decompress_ms",
            ns_to_ms(p2_counters.decompress_ns.load(Relaxed)),
        );
        crate::debug::emit_counter(
            "altw_pass2_way_reframe_ms",
            ns_to_ms(p2_counters.way_reframe_ns.load(Relaxed)),
        );
        crate::debug::emit_counter(
            "altw_pass2_nonway_ms",
            ns_to_ms(p2_counters.nonway_ns.load(Relaxed)),
        );
        crate::debug::emit_counter(
            "altw_pass2_send_ms",
            ns_to_ms(p2_counters.send_ns.load(Relaxed)),
        );
        crate::debug::emit_counter(
            "altw_pass2_bytes_read",
            p2_counters.bytes_read.load(Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "altw_pass2_way_blobs_decoded",
            p2_counters.way_blobs.load(Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "altw_pass2_nonway_blobs_decoded",
            p2_counters.nonway_blobs.load(Relaxed) as i64,
        );
        crate::debug::emit_counter("altw_pass2_consumer_recv_ms", ns_to_ms(p2_recv_ns));
        crate::debug::emit_counter("altw_pass2_consumer_write_ms", ns_to_ms(p2_write_ns));
        crate::debug::emit_counter("altw_pass2_passthrough_pread_ms", ns_to_ms(p2_pt_pread_ns));
        crate::debug::emit_counter("altw_pass2_passthrough_bytes", p2_pt_bytes as i64);
        crate::debug::emit_counter("altw_pass2_passthrough_blobs", p2_pt_blobs as i64);
        crate::debug::emit_counter(
            "altw_pass2_blobs_dispatched",
            batches_dispatched.load(std::sync::atomic::Ordering::Relaxed),
        );
    }
    Ok(total_stats)
}
