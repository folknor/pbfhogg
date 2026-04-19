//! Stage 4: Assembly - emit enriched PBF.
//!
//! Re-reads the PBF, attaches coordinates from per-blob coord_payloads preads
//! to ways.
//! P2c: pread-from-workers with pre-scan schedule for parallel decompress + assembly.
//! Way blobs use wire-format reframe (no full decode); non-way blobs use BlockBuilder.

use std::path::Path;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::Compression;
use crate::{Element, PrimitiveBlock};

use super::super::add_locations_to_ways::Stats;
use super::super::id_set_dense::IdSetDense;
use super::super::{
    dense_node_metadata, element_metadata,
    ensure_node_capacity_local, ensure_relation_capacity_local,
    flush_local, HeaderOverrides, Result, writer_from_header,
};

use super::blob_meta::BlobMeta;

/// Blob descriptor for the stage 4 pre-scan schedule.
struct BlobDescriptor {
    seq: usize,
    /// Start of the on-disk 4-byte length prefix. Used by the consumer-side
    /// raw passthrough path to pread the entire framed blob (header + data)
    /// and hand it verbatim to `PbfWriter::write_raw_owned`.
    frame_offset: u64,
    /// `(data_offset - frame_offset) + data_size`. The exact number of
    /// bytes to pread for raw passthrough.
    frame_size: usize,
    data_offset: u64,
    data_size: usize,
    slot_start: u64,
    is_way_blob: bool,
    /// `true` when the blob can be passed through as raw compressed bytes
    /// without decompress + PrimitiveBlock decode + re-encode. Relations
    /// always qualify; node blobs qualify only when `keep_untagged_nodes`.
    /// Ways never qualify (they need coord_payloads splicing).
    is_passthrough: bool,
    /// Blob kind from indexdata; `None` only for blobs with no indexdata
    /// header, which never reach stage 4 in practice (require_indexdata
    /// gates the whole pipeline) but the field stays optional to match
    /// the scanner API.
    kind: Option<crate::blob_index::ElemKind>,
    /// Element count from indexdata, used to populate Stats on the
    /// passthrough path (no decode available for a live count).
    count: u64,
    /// Index of this way blob within `way_slot_starts` (0 for non-way blobs;
    /// only meaningful when `is_way_blob`).
    way_blob_idx: usize,
}

/// Load the ref-count sidecar and compute prefix sums for slot_start values.
pub(super) fn load_ref_count_sidecar(path: &Path, total_slots: u64) -> Result<Vec<u64>> {
    let data = std::fs::read(path)
        .map_err(|e| format!("failed to read ref count sidecar: {e}"))?;
    if data.len() < 8 {
        return Err("ref count sidecar is too small".into());
    }
    // Last 8 bytes are the trailer (total ref count).
    let trailer_bytes: [u8; 8] = data[data.len() - 8..].try_into()
        .map_err(|_| "ref count sidecar trailer read failed")?;
    let trailer_total = u64::from_le_bytes(trailer_bytes);
    if trailer_total != total_slots {
        return Err(format!(
            "ref count sidecar total ({trailer_total}) != stage 1 total_slots ({total_slots})"
        ).into());
    }

    let entry_bytes = &data[..data.len() - 8];
    if entry_bytes.len() % 8 != 0 {
        return Err("ref count sidecar has non-aligned entries".into());
    }
    let num_entries = entry_bytes.len() / 8;
    let mut slot_starts = Vec::with_capacity(num_entries);
    let mut cumulative: u64 = 0;
    for chunk in entry_bytes.chunks_exact(8) {
        slot_starts.push(cumulative);
        let count = u64::from_le_bytes(chunk.try_into()
            .map_err(|_| "ref count sidecar entry read failed")?);
        cumulative += count;
    }
    if cumulative != total_slots {
        return Err(format!(
            "ref count sidecar cumulative ({cumulative}) != total_slots ({total_slots})"
        ).into());
    }
    Ok(slot_starts)
}

// ---------------------------------------------------------------------------
// Stage 4: Assembly
// ---------------------------------------------------------------------------

/// Assembly pass: re-read the PBF, attach coordinates from per-blob
/// coord_payloads preads to ways.
/// P2c: pread-from-workers with pre-scan schedule for parallel decompress + assembly.
/// See notes/p2c-parallel-assembly-spec.md.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn stage4_assembly(
    input: &Path,
    output: &Path,
    blob_meta: &[BlobMeta],
    coord_payloads_reader: &super::coord_payloads::BlobLocationRouter,
    per_way_rcs: &super::coord_payloads::PerWayRcs,
    way_slot_starts: &[u64],
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSetDense>,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    use std::os::unix::fs::FileExt;
    // Build the blob schedule from the shared metadata scan.
    crate::debug::emit_marker("EXTJOIN_S4_SCHEDULE_START");
    let t_schedule = std::time::Instant::now();

    // Also read the header for the writer (need a regular BlobReader for this).
    let mut header_reader = crate::blob::BlobReader::open(input, direct_io)?;
    let header_blob = header_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let header = header_blob.to_headerblock()?;
    drop(header_reader);

    let mut schedule: Vec<BlobDescriptor> = Vec::new();
    let mut way_sidecar_idx: usize = 0;
    let mut skipped_node_blobs: u64 = 0;
    let mut seq: usize = 0;

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
            crate::blob_index::ElemKind::Node => s4_node_blobs_total += 1,
            crate::blob_index::ElemKind::Way => s4_way_blobs += 1,
            crate::blob_index::ElemKind::Relation => s4_relation_blobs += 1,
        }

        // P1b: skip node blobs with only untagged non-member nodes.
        if !keep_untagged_nodes && matches!(meta.kind, crate::blob_index::ElemKind::Node) {
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

        // Way blobs consume sidecar entries for slot_start.
        let (slot_start, way_blob_idx) = if matches!(meta.kind, crate::blob_index::ElemKind::Way) {
            if way_sidecar_idx >= way_slot_starts.len() {
                return Err("ref count sidecar has fewer entries than way blobs in PBF".into());
            }
            let start = way_slot_starts[way_sidecar_idx];
            let this_way_blob_idx = way_sidecar_idx;
            way_sidecar_idx += 1;
            (start, this_way_blob_idx)
        } else {
            (0, 0)
        };

        let is_way_blob = matches!(meta.kind, crate::blob_index::ElemKind::Way);
        // Passthrough eligibility mirrors the dense-path rule in
        // write_output_passthrough (add_locations_to_ways.rs):
        //   - Relation blobs: always.
        //   - Node blobs: only when keep_untagged_nodes is set (no
        //     per-element filtering needed; the blob is kept as-is).
        //   - Ways: never (they need coord_payloads splicing).
        let is_passthrough = matches!(meta.kind, crate::blob_index::ElemKind::Relation)
            || (matches!(meta.kind, crate::blob_index::ElemKind::Node) && keep_untagged_nodes);

        #[allow(clippy::cast_possible_truncation)]
        let frame_size = (meta.data_offset - meta.frame_offset) as usize + meta.data_size;

        schedule.push(BlobDescriptor {
            seq,
            frame_offset: meta.frame_offset,
            frame_size,
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            slot_start,
            is_way_blob,
            is_passthrough,
            kind: Some(meta.kind),
            count: meta.count,
            way_blob_idx,
        });
        seq += 1;
    }

    // Verify all sidecar entries were consumed.
    if way_sidecar_idx != way_slot_starts.len() {
        return Err(format!(
            "ref count sidecar has {} entries but only {} way blobs seen in PBF",
            way_slot_starts.len(), way_sidecar_idx,
        ).into());
    }
    crate::debug::emit_marker("EXTJOIN_S4_SCHEDULE_END");

    // Open shared file for worker pread.
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?
    );

    let mut writer = writer_from_header(
        output,
        compression,
        &header,
        true,
        overrides,
        |hb| hb.optional_feature("LocationsOnWays"),
        direct_io,
        false,
    )?;

    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    // Split schedule: passthrough-eligible blobs (relations always; node
    // blobs when keep_untagged_nodes=true) are handled by the consumer
    // thread via direct pread + write_raw_owned. Decode blobs (ways and
    // the remaining filtered node blobs) go to workers. This mirrors the
    // extract.rs pread_execute pattern so raw-frame traffic never hits
    // the worker channel.
    let (decode_items, passthrough_items): (Vec<BlobDescriptor>, Vec<BlobDescriptor>) =
        schedule.into_iter().partition(|d| !d.is_passthrough);

    type DecodedItem = (usize, crate::error::Result<(Vec<OwnedBlock>, Stats)>);
    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<BlobDescriptor>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    // Channel capacity 32 - the 256-depth A/B probe confirmed that
    // `s4_send_ms` pressure is steady-state consumer/compression
    // saturation, not burst absorption. A deeper channel just moves
    // the wait into the writer pipeline with no wall change.
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);

    let mut total_stats = Stats::default();

    // Worker-side cumulative counters.
    let s4_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s4_decompress_ms = std::sync::atomic::AtomicU64::new(0);
    let s4_assemble_ms = std::sync::atomic::AtomicU64::new(0);
    let s4_way_reframe_ms = std::sync::atomic::AtomicU64::new(0);
    let s4_nonway_assemble_ms = std::sync::atomic::AtomicU64::new(0);
    let s4_way_blobs_processed = std::sync::atomic::AtomicU64::new(0);
    let s4_nonway_blobs_processed = std::sync::atomic::AtomicU64::new(0);
    let s4_send_ms = std::sync::atomic::AtomicU64::new(0);
    let s4_blobs = std::sync::atomic::AtomicU64::new(0);
    let s4_bytes_read = std::sync::atomic::AtomicU64::new(0);
    let s4_pread_calls = std::sync::atomic::AtomicU64::new(0);
    let s4_max_worker_buf_bytes = std::sync::atomic::AtomicU64::new(0);
    let s4_coord_payload_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s4_coord_payload_bytes = std::sync::atomic::AtomicU64::new(0);
    // Channel depth high-water (workers ++ before send, consumer --
    // after recv). Reaching `decoded_tx` capacity means the channel
    // was the binding queue at some point; staying well below capacity
    // says workers were never able to fill it (so consumer/writer is
    // the limiter, not the channel).
    let s4_channel_depth = std::sync::atomic::AtomicUsize::new(0);
    let s4_channel_high_water = std::sync::atomic::AtomicUsize::new(0);
    // Consumer-side passthrough telemetry.
    let mut s4_passthrough_blobs: u64 = 0;
    let mut s4_passthrough_pread_ms: u64 = 0;
    let mut s4_passthrough_bytes: u64 = 0;
    let s4_pread_ref = &s4_pread_ms;
    let s4_decompress_ref = &s4_decompress_ms;
    let s4_assemble_ref = &s4_assemble_ms;
    let s4_way_reframe_ref = &s4_way_reframe_ms;
    let s4_nonway_assemble_ref = &s4_nonway_assemble_ms;
    let s4_way_blobs_ref = &s4_way_blobs_processed;
    let s4_nonway_blobs_ref = &s4_nonway_blobs_processed;
    let s4_send_ref = &s4_send_ms;
    let s4_blobs_ref = &s4_blobs;
    let s4_bytes_read_ref = &s4_bytes_read;
    let s4_pread_calls_ref = &s4_pread_calls;
    let s4_max_worker_buf_ref = &s4_max_worker_buf_bytes;
    let s4_coord_payload_pread_ref = &s4_coord_payload_pread_ms;
    let s4_coord_payload_bytes_ref = &s4_coord_payload_bytes;
    let s4_channel_depth_ref = &s4_channel_depth;
    let s4_channel_high_water_ref = &s4_channel_high_water;
    let way_reframe_counters = WayReframeCounters::new();
    let way_reframe_cref = &way_reframe_counters;

    // Consumer-side counters.
    let mut s4_recv_ms: u64 = 0;
    let mut s4_write_ms: u64 = 0;
    let mut s4_bytes_written: u64 = 0;
    let mut s4_write_calls: u64 = 0;

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher: feed only decode-eligible blobs into the worker
        // channel. Passthrough blobs bypass workers entirely (see
        // consumer loop below) and never travel through this channel.
        scope.spawn(move || {
            for desc in decode_items {
                if desc_tx.send(desc).is_err() {
                    break;
                }
            }
        });

        // Worker threads: pread → decompress → PrimitiveBlock → assemble.
        // Dedicated threads, NOT global rayon (PbfWriter uses rayon for compression).
        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = decoded_tx.clone();
            let file = std::sync::Arc::clone(&shared_file);
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;
                let mut read_buf: Vec<u8> = Vec::new();
                let mut decompress_buf: Vec<u8> = Vec::new();
                let mut bb = BlockBuilder::new();
                let mut output_blocks: Vec<OwnedBlock> = Vec::new();
                let mut way_reframe_scratch = WayReframeScratch::new();
                let mut reframe_output: Vec<u8> = Vec::new();
                // Per-blob coord payload buffer (prototype path only). Reused
                // across blobs within this worker.
                let mut coord_payload_buf: Vec<u8> = Vec::new();

                loop {
                    let desc = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(d) => d,
                            Err(_) => break,
                        }
                    };

                    let result: crate::error::Result<(Vec<OwnedBlock>, Stats)> = (|| {
                        let t0 = std::time::Instant::now();
                        read_buf.resize(desc.data_size, 0);
                        file.read_exact_at(&mut read_buf, desc.data_offset)
                            .map_err(|e| crate::error::new_error(
                                crate::error::ErrorKind::Io(e)
                            ))?;
                        #[allow(clippy::cast_possible_truncation)]
                        s4_pread_ref.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);
                        s4_bytes_read_ref.fetch_add(desc.data_size as u64, Relaxed);
                        s4_pread_calls_ref.fetch_add(1, Relaxed);

                        let t1 = std::time::Instant::now();
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;
                        #[allow(clippy::cast_possible_truncation)]
                        s4_decompress_ref.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                        let t2 = std::time::Instant::now();
                        output_blocks.clear();

                        if desc.is_way_blob {
                            // Read this blob's coord_payloads payload into a
                            // worker-local buffer before reframe.
                            let t_cpread = std::time::Instant::now();
                            coord_payloads_reader
                                .pread_blob_payload(desc.way_blob_idx, &mut coord_payload_buf)
                                .map_err(|e| crate::error::new_error(
                                    crate::error::ErrorKind::Io(std::io::Error::other(
                                        e.to_string(),
                                    )),
                                ))?;
                            #[allow(clippy::cast_possible_truncation)]
                            s4_coord_payload_pread_ref.fetch_add(
                                t_cpread.elapsed().as_millis() as u64,
                                Relaxed,
                            );
                            s4_coord_payload_bytes_ref.fetch_add(
                                coord_payload_buf.len() as u64,
                                Relaxed,
                            );

                            // Wire-format reframe: splice locations without
                            // full PrimitiveBlock decode or BlockBuilder.
                            let (way_count, _new_slot_pos, min_id, max_id, missing) =
                                reframe_way_blob_with_locations(
                                    &decompress_buf,
                                    &coord_payload_buf,
                                    per_way_rcs.blob_record(desc.way_blob_idx),
                                    desc.way_blob_idx,
                                    desc.slot_start,
                                    &mut reframe_output,
                                    &mut way_reframe_scratch,
                                    way_reframe_cref,
                                ).map_err(|e| crate::error::new_error(
                                    crate::error::ErrorKind::Io(std::io::Error::other(e))
                                ))?;

                            let index = crate::blob_index::BlobIndex {
                                kind: crate::blob_index::ElemKind::Way,
                                min_id,
                                max_id,
                                count: way_count,
                                bbox: None,
                            };
                            let taken = std::mem::take(&mut reframe_output);
                            reframe_output.reserve(taken.len());
                            output_blocks.push((taken, index, None));

                            let block_stats = Stats {
                                ways_written: way_count,
                                missing_locations: missing,
                                blobs_decoded: 1,
                                ..Stats::default()
                            };
                            #[allow(clippy::cast_possible_truncation)]
                            {
                                let elapsed = t2.elapsed().as_millis() as u64;
                                s4_assemble_ref.fetch_add(elapsed, Relaxed);
                                s4_way_reframe_ref.fetch_add(elapsed, Relaxed);
                            }
                            s4_blobs_ref.fetch_add(1, Relaxed);
                            s4_way_blobs_ref.fetch_add(1, Relaxed);
                            return Ok((std::mem::take(&mut output_blocks), block_stats));
                        }

                        // Non-way blobs: full PrimitiveBlock decode + BlockBuilder.
                        let block = PrimitiveBlock::new(
                            bytes::Bytes::from(std::mem::take(&mut decompress_buf))
                        )?;
                        let mut block_stats = assemble_block(
                            &block,
                            &mut bb,
                            &mut output_blocks,
                            keep_untagged_nodes,
                            relation_member_node_ids,
                        ).map_err(|e| crate::error::new_error(
                            crate::error::ErrorKind::Io(std::io::Error::other(e))
                        ))?;
                        block_stats.blobs_decoded = 1;
                        flush_local(&mut bb, &mut output_blocks).map_err(|e| {
                            crate::error::new_error(
                                crate::error::ErrorKind::Io(std::io::Error::other(e))
                            )
                        })?;
                        #[allow(clippy::cast_possible_truncation)]
                        {
                            let elapsed = t2.elapsed().as_millis() as u64;
                            s4_assemble_ref.fetch_add(elapsed, Relaxed);
                            s4_nonway_assemble_ref.fetch_add(elapsed, Relaxed);
                        }
                        s4_nonway_blobs_ref.fetch_add(1, Relaxed);

                        if decompress_buf.capacity() == 0 {
                            decompress_buf = Vec::new();
                        }

                        s4_blobs_ref.fetch_add(1, Relaxed);

                        Ok((std::mem::take(&mut output_blocks), block_stats))
                    })();

                    // Track max live buffer bytes for this worker.
                    {
                        let worker_bytes = read_buf.capacity() as u64
                            + decompress_buf.capacity() as u64
                            + reframe_output.capacity() as u64;
                        let mut current = s4_max_worker_buf_ref.load(std::sync::atomic::Ordering::Relaxed);
                        while worker_bytes > current {
                            match s4_max_worker_buf_ref.compare_exchange_weak(
                                current, worker_bytes,
                                std::sync::atomic::Ordering::Relaxed,
                                std::sync::atomic::Ordering::Relaxed,
                            ) {
                                Ok(_) => break,
                                Err(actual) => current = actual,
                            }
                        }
                    }

                    let t3 = std::time::Instant::now();
                    // Bump channel depth before send and update the
                    // high-water mark with the post-bump value (i.e.
                    // the depth the channel will hold once this send
                    // completes). Bounded `sync_channel` blocks here
                    // when full, so the recorded value still reflects
                    // a real moment of saturation.
                    let depth_after = s4_channel_depth_ref
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    {
                        let mut hw = s4_channel_high_water_ref
                            .load(std::sync::atomic::Ordering::Relaxed);
                        while depth_after > hw {
                            match s4_channel_high_water_ref.compare_exchange_weak(
                                hw, depth_after,
                                std::sync::atomic::Ordering::Relaxed,
                                std::sync::atomic::Ordering::Relaxed,
                            ) {
                                Ok(_) => break,
                                Err(actual) => hw = actual,
                            }
                        }
                    }
                    if tx.send((desc.seq, result)).is_err() {
                        break;
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    s4_send_ref.fetch_add(t3.elapsed().as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
                }
            });
        }
        drop(desc_rx);
        drop(decoded_tx);

        // Consumer: reorder + write to PbfWriter. Decode results arrive
        // from workers; passthrough items are pre-seeded and the consumer
        // preads their raw frames inline when they become the head of the
        // reorder buffer. `write_raw_owned` hands the pre-framed bytes to
        // the writer thread verbatim.
        enum ConsumerItem {
            Decoded(crate::error::Result<(Vec<OwnedBlock>, Stats)>),
            Passthrough {
                frame_offset: u64,
                frame_size: usize,
                count: u64,
                kind: crate::blob_index::ElemKind,
            },
        }

        let mut reorder: crate::reorder_buffer::ReorderBuffer<ConsumerItem> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(32);

        // Pre-seed passthrough items at their global seq positions.
        for desc in &passthrough_items {
            let kind = desc.kind.expect(
                "passthrough eligibility requires a known blob kind",
            );
            reorder.push(desc.seq, ConsumerItem::Passthrough {
                frame_offset: desc.frame_offset,
                frame_size: desc.frame_size,
                count: desc.count,
                kind,
            });
        }

        let mut frame_read_buf: Vec<u8> = Vec::new();

        // Drain helper: pop consecutive ready items. Shared between the
        // main result loop and the final drain for schedules that end
        // on a passthrough tail (no decode result arrives to push it).
        let mut drain = |reorder: &mut crate::reorder_buffer::ReorderBuffer<ConsumerItem>,
                         total_stats: &mut Stats,
                         s4_bytes_written: &mut u64,
                         s4_write_calls: &mut u64,
                         s4_write_ms: &mut u64,
                         s4_passthrough_blobs: &mut u64,
                         s4_passthrough_pread_ms: &mut u64,
                         s4_passthrough_bytes: &mut u64|
            -> Result<()> {
            while let Some(item) = reorder.pop_ready() {
                match item {
                    ConsumerItem::Decoded(result) => {
                        let (blocks, block_stats) = result?;
                        total_stats.merge(&block_stats);
                        for (block_bytes, index, tagdata) in blocks {
                            *s4_bytes_written += block_bytes.len() as u64;
                            *s4_write_calls += 1;
                            let t_w = std::time::Instant::now();
                            writer.write_primitive_block_owned(
                                block_bytes, index, tagdata.as_deref(),
                            )?;
                            #[allow(clippy::cast_possible_truncation)]
                            { *s4_write_ms += t_w.elapsed().as_millis() as u64; }
                        }
                    }
                    ConsumerItem::Passthrough { frame_offset, frame_size, count, kind } => {
                        let t_pread = std::time::Instant::now();
                        frame_read_buf.resize(frame_size, 0);
                        shared_file.read_exact_at(&mut frame_read_buf, frame_offset)
                            .map_err(|e| crate::error::new_error(
                                crate::error::ErrorKind::Io(e),
                            ))?;
                        #[allow(clippy::cast_possible_truncation)]
                        { *s4_passthrough_pread_ms += t_pread.elapsed().as_millis() as u64; }
                        *s4_passthrough_bytes += frame_size as u64;

                        let t_w = std::time::Instant::now();
                        *s4_bytes_written += frame_size as u64;
                        *s4_write_calls += 1;
                        writer.write_raw_owned(std::mem::take(&mut frame_read_buf))?;
                        #[allow(clippy::cast_possible_truncation)]
                        { *s4_write_ms += t_w.elapsed().as_millis() as u64; }

                        *s4_passthrough_blobs += 1;
                        total_stats.blobs_passthrough += 1;
                        match kind {
                            crate::blob_index::ElemKind::Node => {
                                total_stats.nodes_read += count;
                                total_stats.nodes_written += count;
                            }
                            crate::blob_index::ElemKind::Relation => {
                                total_stats.relations_written += count;
                            }
                            crate::blob_index::ElemKind::Way => {
                                // Ways never pass through (they need
                                // coord_payloads splicing). Fall through
                                // without touching stats; unreachable in
                                // practice but we avoid a hard panic here
                                // since this is on the output path.
                            }
                        }
                    }
                }
            }
            Ok(())
        };

        // Drain any passthrough prefix before the first decode result.
        drain(
            &mut reorder, &mut total_stats,
            &mut s4_bytes_written, &mut s4_write_calls, &mut s4_write_ms,
            &mut s4_passthrough_blobs, &mut s4_passthrough_pread_ms,
            &mut s4_passthrough_bytes,
        )?;

        loop {
            let t_recv = std::time::Instant::now();
            let msg = decoded_rx.recv();
            #[allow(clippy::cast_possible_truncation)]
            { s4_recv_ms += t_recv.elapsed().as_millis() as u64; }
            let (seq_num, item) = match msg {
                Ok(v) => {
                    // Decrement channel depth on successful recv.
                    s4_channel_depth.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    v
                }
                Err(_) => break,
            };

            reorder.push(seq_num, ConsumerItem::Decoded(item));
            drain(
                &mut reorder, &mut total_stats,
                &mut s4_bytes_written, &mut s4_write_calls, &mut s4_write_ms,
                &mut s4_passthrough_blobs, &mut s4_passthrough_pread_ms,
                &mut s4_passthrough_bytes,
            )?;
        }

        // Final drain for passthrough tails: if the schedule ends on
        // passthrough items (common - relations sit at EOF in sorted
        // PBFs) there's no trailing decode push to trigger the last
        // pop_ready, so do it here.
        drain(
            &mut reorder, &mut total_stats,
            &mut s4_bytes_written, &mut s4_write_calls, &mut s4_write_ms,
            &mut s4_passthrough_blobs, &mut s4_passthrough_pread_ms,
            &mut s4_passthrough_bytes,
        )?;

        Ok(())
    })?;

    // Time the final writer flush. `write_raw_owned` and
    // `write_primitive_block_owned` hand work to the writer thread
    // (compression + file I/O); when the decoded_tx channel grows,
    // backpressure that used to surface as `s4_send_ms` shifts into
    // the writer pipeline and only materialises here. Isolating
    // flush makes the channel-vs-writer attribution legible.
    let t_flush = std::time::Instant::now();
    writer.flush()?;
    #[allow(clippy::cast_possible_truncation)]
    let s4_flush_ms: u64 = t_flush.elapsed().as_millis() as u64;
    let s4_output_bytes = std::fs::metadata(output)
        .map_err(|e| format!("stat output {}: {e}", output.display()))?
        .len();

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter("s4_pread_ms", s4_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_decompress_ms", s4_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_assemble_ms", s4_assemble_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_way_reframe_ms", s4_way_reframe_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_nonway_assemble_ms", s4_nonway_assemble_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_way_blobs_processed", s4_way_blobs_processed.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_nonway_blobs_processed", s4_nonway_blobs_processed.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_send_ms", s4_send_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_blobs", s4_blobs.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_bytes_read", s4_bytes_read.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_bytes_written", s4_bytes_written as i64);
        crate::debug::emit_counter("s4_pread_calls", s4_pread_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_write_calls", s4_write_calls as i64);
        crate::debug::emit_counter("s4_max_worker_buf_bytes", s4_max_worker_buf_bytes.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_coord_payload_pread_ms", s4_coord_payload_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_coord_payload_bytes", s4_coord_payload_bytes.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_passthrough_blobs", s4_passthrough_blobs as i64);
        crate::debug::emit_counter("s4_passthrough_pread_ms", s4_passthrough_pread_ms as i64);
        crate::debug::emit_counter("s4_passthrough_bytes", s4_passthrough_bytes as i64);
        crate::debug::emit_counter("s4_consumer_recv_ms", s4_recv_ms as i64);
        crate::debug::emit_counter("s4_consumer_write_ms", s4_write_ms as i64);
        crate::debug::emit_counter("s4_flush_ms", s4_flush_ms as i64);
        crate::debug::emit_counter("s4_decode_threads", decode_threads as i64);
        crate::debug::emit_counter("s4_output_bytes", s4_output_bytes as i64);
        crate::debug::emit_counter(
            "s4_channel_high_water",
            s4_channel_high_water.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter("extjoin_skipped_node_blobs", skipped_node_blobs as i64);
        crate::debug::emit_counter("s4_node_blobs_total", s4_node_blobs_total as i64);
        crate::debug::emit_counter("s4_node_blobs_no_tagindex", s4_node_blobs_no_tagindex as i64);
        crate::debug::emit_counter("s4_node_blobs_empty_tags", s4_node_blobs_empty_tags as i64);
        crate::debug::emit_counter("s4_node_blobs_kept_by_tags", s4_node_blobs_kept_by_tags as i64);
        crate::debug::emit_counter("s4_node_blobs_kept_by_members", s4_node_blobs_kept_by_members as i64);
        crate::debug::emit_counter("s4_way_blobs", s4_way_blobs as i64);
        crate::debug::emit_counter("s4_relation_blobs", s4_relation_blobs as i64);
        crate::debug::emit_counter("s4_schedule_scan_ms", t_schedule.elapsed().as_millis() as i64);
    }
    way_reframe_counters.emit();

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
    relation_member_node_ids: Option<&IdSetDense>,
) -> std::result::Result<Stats, String> {
    let mut stats = Stats::default();

    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                stats.nodes_read += 1;
                let has_tags = dn.tags().next().is_some();
                if keep_untagged_nodes
                    || has_tags
                    || relation_member_node_ids.is_some_and(|ids| ids.get(dn.id()))
                {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = dense_node_metadata(dn);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            Element::Node(n) => {
                stats.nodes_read += 1;
                let has_tags = n.tags().next().is_some();
                if keep_untagged_nodes
                    || has_tags
                    || relation_member_node_ids.is_some_and(|ids| ids.get(n.id()))
                {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = element_metadata(&n.info());
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref());
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            Element::Way(w) => {
                // `require_indexdata` guarantees every blob has an index,
                // and sorted PBF confines ways to blobs indexed as Way
                // (routed through reframe_way_blob_with_locations). Reaching
                // here means the input violates one of those invariants
                // (e.g. a way inside a non-Way-indexed blob). The integrated
                // path produces coord_payloads keyed by way-blob index only,
                // so there is no coordinate source for an out-of-band way -
                // error out rather than silently emit (0,0).
                return Err(format!(
                    "way {} appeared in a non-way-indexed blob; ALTW external \
                     index requires sorted + indexed PBF where ways are confined \
                     to Way-indexed blobs",
                    w.id(),
                ));
            }
            Element::Relation(r) => {
                ensure_relation_capacity_local(bb, output)?;
                members_buf.clear();
                members_buf.extend(r.members().map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                }));
                let meta = element_metadata(&r.info());
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                stats.relations_written += 1;
            }
        }
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Wire-format way reframe for stage 4
// ---------------------------------------------------------------------------

/// Sub-phase counters for the way reframe hot path.
///
/// Timing is captured **per-blob** only (parse_block, group_reframe,
/// unknown_field_copy). The earlier per-way timers were stripped after
/// they confirmed the way path is not the wall-critical segment; per-way
/// `Instant::now()` samples were a measurable cost at 1.16B ways/planet
/// and provided no actionable signal beyond the blob-level attribution.
struct WayReframeCounters {
    parse_block_ns: std::sync::atomic::AtomicU64,
    unknown_field_copy_ns: std::sync::atomic::AtomicU64,
    group_reframe_ns: std::sync::atomic::AtomicU64,
    refs_total: std::sync::atomic::AtomicU64,
    refs_present: std::sync::atomic::AtomicU64,
    max_refs_per_way: std::sync::atomic::AtomicU64,
    lat_bytes: std::sync::atomic::AtomicU64,
    lon_bytes: std::sync::atomic::AtomicU64,
    ways_total: std::sync::atomic::AtomicU64,
}

impl WayReframeCounters {
    fn new() -> Self {
        Self {
            parse_block_ns: std::sync::atomic::AtomicU64::new(0),
            unknown_field_copy_ns: std::sync::atomic::AtomicU64::new(0),
            group_reframe_ns: std::sync::atomic::AtomicU64::new(0),
            refs_total: std::sync::atomic::AtomicU64::new(0),
            refs_present: std::sync::atomic::AtomicU64::new(0),
            max_refs_per_way: std::sync::atomic::AtomicU64::new(0),
            lat_bytes: std::sync::atomic::AtomicU64::new(0),
            lon_bytes: std::sync::atomic::AtomicU64::new(0),
            ways_total: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[allow(clippy::cast_possible_wrap)]
    fn emit(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        // Convert nanosecond accumulators to milliseconds at emit time so
        // counter names and downstream comparisons stay on the same unit.
        let ns_to_ms = |ns: u64| (ns / 1_000_000) as i64;
        crate::debug::emit_counter("s4_way_parse_block_ms", ns_to_ms(self.parse_block_ns.load(Relaxed)));
        crate::debug::emit_counter("s4_way_unknown_field_copy_ms", ns_to_ms(self.unknown_field_copy_ns.load(Relaxed)));
        crate::debug::emit_counter("s4_way_group_reframe_ms", ns_to_ms(self.group_reframe_ns.load(Relaxed)));
        crate::debug::emit_counter("s4_way_refs_total", self.refs_total.load(Relaxed) as i64);
        crate::debug::emit_counter("s4_way_refs_present", self.refs_present.load(Relaxed) as i64);
        crate::debug::emit_counter("s4_way_max_refs_per_way", self.max_refs_per_way.load(Relaxed) as i64);
        crate::debug::emit_counter("s4_way_lat_bytes", self.lat_bytes.load(Relaxed) as i64);
        crate::debug::emit_counter("s4_way_lon_bytes", self.lon_bytes.load(Relaxed) as i64);
        crate::debug::emit_counter("s4_way_messages_total", self.ways_total.load(Relaxed) as i64);
        let total_refs = self.refs_total.load(Relaxed);
        let total_ways = self.ways_total.load(Relaxed);
        if let Some(avg) = total_refs.checked_div(total_ways) {
            crate::debug::emit_counter("s4_way_avg_refs_per_way", avg as i64);
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
    group_out: Vec<u8>,
}

impl WayReframeScratch {
    fn new() -> Self {
        Self {
            group_ranges: Vec::new(),
            scalar_fields: Vec::new(),
            reframed_way: Vec::new(),
            packed_lats: Vec::new(),
            packed_lons: Vec::new(),
            group_out: Vec::new(),
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
/// Returns `(way_count, way_slot_pos_after, min_way_id, max_way_id, missing_locations)`.
///
/// `coord_payload` is the per-blob delta-varint payload produced by stage 3.
/// Each way consumes `2*ref_count` varints, interleaved (lat, lon, lat,
/// lon, ...) with deltas reset per way. Because the raw varint encoding
/// matches PBF's packed field 9/10 byte-for-byte, we de-interleave by
/// copying raw varint bytes into `packed_lats` and `packed_lons` without
/// zigzag-decoding or re-encoding.
///
/// `missing_locations` is left at 0 here. The whole-pipeline value is
/// computed once in `external_join` as `total_slots - stage2_resolved_count`
/// so it matches the dense path's per-ref counter without paying a per-ref
/// decode in this hot loop.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn reframe_way_blob_with_locations(
    decompressed: &[u8],
    coord_payload: &[u8],
    per_way_refcount_record: &[u8],
    way_blob_idx: usize,
    mut way_slot_pos: u64,
    output: &mut Vec<u8>,
    scratch: &mut WayReframeScratch,
    counters: &WayReframeCounters,
) -> std::result::Result<(u64, u64, i64, i64, u64), String> {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};
    use std::sync::atomic::Ordering::Relaxed;

    scratch.group_ranges.clear();
    scratch.scalar_fields.clear();
    let mut stringtable_range: Option<(usize, usize)> = None;

    // Level 1: PrimitiveBlock - find string table + groups.
    let t_block = std::time::Instant::now();
    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(|e| format!("reframe block: {e}"))? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| format!("reframe st: {e}"))?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                stringtable_range = Some((offset, data.len()));
            }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| format!("reframe group: {e}"))?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                scratch.group_ranges.push((offset, data.len()));
            }
            (17..=20, WIRE_VARINT) => {
                let raw = cursor.read_raw_field(wire_type).map_err(|e| format!("reframe scalar: {e}"))?;
                protohoggr::encode_tag(&mut scratch.scalar_fields, field, wire_type);
                scratch.scalar_fields.extend_from_slice(raw);
            }
            _ => cursor.skip_field(wire_type).map_err(|e| format!("reframe skip: {e}"))?,
        }
    }

    let (st_offset, st_len) = stringtable_range
        .ok_or("reframe: no StringTable in PrimitiveBlock")?;
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    #[allow(clippy::cast_possible_truncation)]
    counters.parse_block_ns.fetch_add(t_block.elapsed().as_nanos() as u64, Relaxed);

    let mut refcount_cursor = Cursor::new(per_way_refcount_record);
    let expected_ways = refcount_cursor
        .read_varint()
        .map_err(|e| format!("per-way sidecar blob {way_blob_idx} num_ways: {e}"))?;
    #[allow(clippy::cast_possible_truncation)]
    let expected_ways_usize = expected_ways as usize;
    let mut sidecar_way_idx: usize = 0;

    output.clear();
    protohoggr::encode_bytes_field(output, 1, stringtable_bytes);

    let mut total_ways: u64 = 0;
    let mut min_way_id: i64 = i64::MAX;
    let mut max_way_id: i64 = i64::MIN;
    let missing_locations: u64 = 0;
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
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(|e| format!("reframe gfield: {e}"))? {
            if field == 3 && wire_type == WIRE_LEN {
                // Way submessage - splice locations.
                let way_bytes = gr_cursor.read_len_delimited().map_err(|e| format!("reframe way: {e}"))?;

                if sidecar_way_idx >= expected_ways_usize {
                    return Err(format!(
                        "blob {way_blob_idx}: encountered more way messages than the per-way sidecar record declares ({expected_ways_usize})"
                    ));
                }
                let ref_count = refcount_cursor
                    .read_varint()
                    .map_err(|e| format!(
                        "per-way sidecar blob {way_blob_idx} way {sidecar_way_idx}: {e}"
                    ))?;
                sidecar_way_idx += 1;

                let mut way_id: i64 = 0;

                let mut way_cursor = Cursor::new(way_bytes);
                while let Some((wf, wt)) = way_cursor.read_tag().map_err(|e| format!("reframe wfield: {e}"))? {
                    if wf == 1 && wt == WIRE_VARINT {
                        way_id = way_cursor.read_varint_i64().map_err(|e| format!("reframe id: {e}"))?;
                    } else {
                        way_cursor.skip_field(wt).map_err(|e| format!("reframe wskip: {e}"))?;
                    }
                }

                if way_id < min_way_id { min_way_id = way_id; }
                if way_id > max_way_id { max_way_id = way_id; }

                // De-interleave pre-encoded varints from coord_payload into
                // PBF packed fields 9/10. The raw varint bytes match PBF's
                // packed sint32 wire format 1:1, so we copy bytes without
                // zigzag decode + re-encode.
                scratch.packed_lats.clear();
                scratch.packed_lons.clear();
                for _ in 0..ref_count {
                    let lat_start = payload_pos;
                    while payload_pos < coord_payload.len()
                        && (coord_payload[payload_pos] & 0x80) != 0
                    {
                        payload_pos += 1;
                    }
                    if payload_pos >= coord_payload.len() {
                        return Err("coord_payload: truncated lat varint".into());
                    }
                    payload_pos += 1;
                    scratch.packed_lats.extend_from_slice(&coord_payload[lat_start..payload_pos]);
                    let lon_start = payload_pos;
                    while payload_pos < coord_payload.len()
                        && (coord_payload[payload_pos] & 0x80) != 0
                    {
                        payload_pos += 1;
                    }
                    if payload_pos >= coord_payload.len() {
                        return Err("coord_payload: truncated lon varint".into());
                    }
                    payload_pos += 1;
                    scratch.packed_lons.extend_from_slice(&coord_payload[lon_start..payload_pos]);
                }
                way_slot_pos += ref_count;
                blob_refs_present += ref_count;
                if ref_count > blob_max_refs_per_way {
                    blob_max_refs_per_way = ref_count;
                }
                blob_lat_bytes += scratch.packed_lats.len() as u64;
                blob_lon_bytes += scratch.packed_lons.len() as u64;
                blob_refs += ref_count;

                // Build reframed way: original bytes + appended fields 9, 10.
                scratch.reframed_way.clear();
                scratch.reframed_way.extend_from_slice(way_bytes);
                if ref_count > 0 {
                    protohoggr::encode_bytes_field(&mut scratch.reframed_way, 9, &scratch.packed_lats);
                    protohoggr::encode_bytes_field(&mut scratch.reframed_way, 10, &scratch.packed_lons);
                }

                protohoggr::encode_bytes_field(&mut scratch.group_out, 3, &scratch.reframed_way);
                total_ways += 1;
            } else {
                // Non-way field in the group - copy verbatim.
                let t_copy = std::time::Instant::now();
                let raw = gr_cursor.read_raw_field(wire_type).map_err(|e| format!("reframe gskip: {e}"))?;
                protohoggr::encode_tag(&mut scratch.group_out, field, wire_type);
                scratch.group_out.extend_from_slice(raw);
                #[allow(clippy::cast_possible_truncation)]
                counters.unknown_field_copy_ns.fetch_add(t_copy.elapsed().as_nanos() as u64, Relaxed);
            }
        }

        let t_group = std::time::Instant::now();
        protohoggr::encode_bytes_field(output, 2, &scratch.group_out);
        #[allow(clippy::cast_possible_truncation)]
        counters.group_reframe_ns.fetch_add(t_group.elapsed().as_nanos() as u64, Relaxed);
    }

    // Append scalar fields (granularity, etc.).
    output.extend_from_slice(&scratch.scalar_fields);

    if sidecar_way_idx != expected_ways_usize {
        return Err(format!(
            "blob {way_blob_idx}: per-way sidecar record declared {expected_ways_usize} ways but the blob contained {sidecar_way_idx}"
        ));
    }
    if refcount_cursor.remaining() != 0 {
        return Err(format!(
            "per-way sidecar blob {way_blob_idx} has {} trailing bytes",
            refcount_cursor.remaining()
        ));
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
        let mut current = counters.max_refs_per_way.load(Relaxed);
        while blob_max_refs_per_way > current {
            match counters.max_refs_per_way.compare_exchange_weak(
                current, blob_max_refs_per_way, Relaxed, Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    if payload_pos != coord_payload.len() {
        return Err(format!(
            "coord_payload: consumed {payload_pos} of {} bytes (trailing bytes indicate stage 3 over-production or version skew)",
            coord_payload.len()
        ));
    }

    Ok((total_ways, way_slot_pos, min_way_id, max_way_id, missing_locations))
}
