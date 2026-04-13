//! External join for add-locations-to-ways: bounded-memory coordinate resolution
//! via double radix permutation.
//!
//! Instead of building a giant random-access node index (16 GB mmap at planet
//! scale), this module pre-computes the way-node join using sequential I/O and
//! bounded memory:
//!
//! 1. **Way pass**: stream ways, emit `(node_id, slot_pos)` COO pairs into
//!    256 node buckets partitioned by high bits of node_id.
//! 2. **Node join**: per bucket, sort pairs by node_id in RAM (~500 MB),
//!    merge-join with matching node stream, emit `(slot_pos, lat, lon)` into
//!    256 slot buckets partitioned by high bits of slot_pos.
//! 3. **Slot reorder**: per bucket, sort by slot_pos, write final coord_slots
//!    file sequentially.
//! 4. **Assembly**: stream original PBF + coord_slots, emit enriched ways.
//!
//! Memory at every stage: <1 GB. All I/O sequential. No mmap, no random access.
//! See `notes/altw-partitioned.md` for the full design.

use std::io::{BufWriter, Write as _};
use std::path::Path;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::Compression;
use crate::{Element, ElementReader, PrimitiveBlock};

use super::add_locations_to_ways::Stats;
use super::external_radix::{BucketWriters, ScratchDir, NUM_BUCKETS};
#[cfg(feature = "linux-direct-io")]
use super::external_radix::advise_dontneed_file;
use super::id_set_dense::IdSetDense;
use super::{
    dense_node_metadata, element_metadata,
    ensure_node_capacity_local, ensure_relation_capacity_local, ensure_way_capacity_local,
    flush_local, require_indexdata, writer_from_header,
    HeaderOverrides, Result,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum node ID in current OSM data. Used to compute bucket ranges.
/// 14B gives headroom above the current ~13B maximum.
const MAX_NODE_ID: u64 = 14_000_000_000;

/// Size of a rank-occurrence record: `(local_rank: u32, slot_pos: u64)` = 12 bytes.
const RANK_RECORD_SIZE: usize = 12;

/// Size of a resolved entry: `(slot_pos: u64, lat: i32, lon: i32)` = 16 bytes.
const RESOLVED_ENTRY_SIZE: usize = 16;

/// Size of a coordinate slot: `(lat: i32, lon: i32)` = 8 bytes.
const COORD_SLOT_SIZE: usize = 8;

// ---------------------------------------------------------------------------
// Rank-occurrence record: (rank, slot_pos)
// ---------------------------------------------------------------------------

/// A rank-bucketed occurrence record. `local_rank` is the rank offset
/// within the bucket (`global_rank - bucket_rank_start`), stored as u32
/// (max ~40M entries per bucket at planet, well under u32::MAX).
/// `slot_pos` is the final position in the coord_slots array.
///
/// 12 bytes instead of 16: 25% I/O reduction across stages 1B and 2.
#[derive(Clone, Copy)]
struct RankRecord {
    local_rank: u32,
    slot_pos: u64,
}

impl RankRecord {
    fn write_to(&self, buf: &mut [u8; RANK_RECORD_SIZE]) {
        buf[..4].copy_from_slice(&self.local_rank.to_le_bytes());
        buf[4..12].copy_from_slice(&self.slot_pos.to_le_bytes());
    }
}

// ---------------------------------------------------------------------------
// Resolved entry: (slot_pos, lat, lon)
// ---------------------------------------------------------------------------

/// A resolved coordinate ready to be placed into the final coord_slots file.
#[derive(Clone, Copy)]
struct ResolvedEntry {
    slot_pos: u64,
    lat: i32,
    lon: i32,
}

impl ResolvedEntry {
    fn write_to(&self, buf: &mut [u8; RESOLVED_ENTRY_SIZE]) {
        buf[..8].copy_from_slice(&self.slot_pos.to_le_bytes());
        buf[8..12].copy_from_slice(&self.lat.to_le_bytes());
        buf[12..16].copy_from_slice(&self.lon.to_le_bytes());
    }

    fn read_from(buf: &[u8; RESOLVED_ENTRY_SIZE]) -> Self {
        let slot_pos = u64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        let lat = i32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let lon = i32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        Self { slot_pos, lat, lon }
    }

    /// Bucket index for slot-pos partitioning.
    #[allow(clippy::cast_possible_truncation)]
    fn slot_bucket(&self, total_slots: u64) -> usize {
        let range_size = total_slots.div_ceil(NUM_BUCKETS as u64);
        if range_size == 0 {
            return 0;
        }
        let bucket = self.slot_pos / range_size;
        (bucket as usize).min(NUM_BUCKETS - 1)
    }
}

// ---------------------------------------------------------------------------
// Stage 1: Two-pass way scan
//   Pass A: build IdSetDense of referenced node IDs (parallel)
//   Pass B: emit rank-bucketed (rank, slot_pos) records (parallel)
// ---------------------------------------------------------------------------

/// Way-blob schedule entry for the parallel way scans.
struct WayBlobTask {
    seq: u32,
    data_offset: u64,
    data_size: usize,
}

/// Build the way-blob schedule via header-only scan.
fn build_way_schedule(input: &Path) -> Result<Vec<WayBlobTask>> {
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut schedule: Vec<WayBlobTask> = Vec::new();
    let mut seq: u32 = 0;
    while let Some(result) = scanner.next_header_with_data_offset() {
        let (hdr, _, data_offset, data_size) = result?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(idx) = hdr.index() {
            if !matches!(idx.kind, crate::blob_index::ElemKind::Way) { continue; }
        }
        schedule.push(WayBlobTask { seq, data_offset, data_size });
        seq += 1;
    }
    Ok(schedule)
}

/// Two-pass stage 1.
///
/// **Pass A**: parallel way scan to build `IdSetDense` of all referenced
/// node IDs + write sidecar ref counts.
///
/// **Pass B**: rescan ways with rank index available. Emit `(rank, slot_pos)`
/// records into rank-bucketed per-worker shard files.
///
/// Returns `(total_refs, unique_nodes, rank_bucket_entry_counts, num_workers, node_id_set)`.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
fn stage1_way_pass(
    input: &Path,
    _direct_io: bool,
    scratch: &ScratchDir,
    ref_count_sidecar: &Path,
) -> Result<(u64, u64, Vec<u64>, usize, IdSetDense)> {
    use std::os::unix::fs::FileExt as _;

    let schedule = build_way_schedule(input)?;
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    // ---- Pass A: build IdSetDense + sidecar ----
    crate::debug::emit_marker("EXTJOIN_S1_PASS_A_START");

    let mut node_id_set = super::id_set_dense::IdSetDense::new();
    // Pre-allocate for planet-scale node IDs (~13B max).
    #[allow(clippy::cast_possible_wrap)]
    node_id_set.pre_allocate(MAX_NODE_ID as i64);

    let s1a_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s1a_decompress_ms = std::sync::atomic::AtomicU64::new(0);
    let s1a_scan_ms = std::sync::atomic::AtomicU64::new(0);

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let mut total_refs: u64 = 0;

    // Workers scan way refs and set bits in the shared IdSetDense.
    // Consumer writes sidecar ref counts in blob order.
    {
        type PassAItem = (u32, std::result::Result<u64, String>);
        let (tx, rx) = std::sync::mpsc::sync_channel::<PassAItem>(32);
        let schedule_ref = &schedule;
        let next_ref = &next_idx;
        let node_id_set_ref = &node_id_set;
        let s1a_pread_ref = &s1a_pread_ms;
        let s1a_decompress_ref = &s1a_decompress_ms;
        let s1a_scan_ref = &s1a_scan_ms;

        std::thread::scope(|scope| -> Result<()> {
            for _ in 0..num_workers {
                let file = std::sync::Arc::clone(&shared_file);
                let tx = tx.clone();
                scope.spawn(move || {
                    use std::sync::atomic::Ordering::Relaxed;
                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();

                    loop {
                        let idx = next_ref.fetch_add(1, Relaxed);
                        if idx >= schedule_ref.len() { break; }
                        let task = &schedule_ref[idx];

                        let result: std::result::Result<u64, String> = (|| {
                            let t0 = std::time::Instant::now();
                            read_buf.resize(task.data_size, 0);
                            file.read_exact_at(&mut read_buf, task.data_offset)
                                .map_err(|e| format!("pass A pread: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_pread_ref.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);

                            let t1 = std::time::Instant::now();
                            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                                .map_err(|e| format!("pass A decompress: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_decompress_ref.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                            let t2 = std::time::Instant::now();
                            let mut ref_count: u64 = 0;
                            super::way_scanner::scan_way_refs(
                                &decompress_buf, &mut refs_buf, &mut group_starts,
                                |_way_id, refs| {
                                    for &node_id in refs {
                                        node_id_set_ref.set_atomic(node_id);
                                        ref_count += 1;
                                    }
                                },
                            ).map_err(|e| e.to_string())?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_scan_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

                            Ok(ref_count)
                        })();

                        if tx.send((task.seq, result)).is_err() { break; }
                    }
                });
            }
            drop(tx);

            // Consumer: write sidecar in blob order.
            let mut sidecar_writer = BufWriter::with_capacity(
                64 * 1024,
                std::fs::File::create(ref_count_sidecar)
                    .map_err(|e| format!("create sidecar: {e}"))?,
            );
            let mut reorder: crate::reorder_buffer::ReorderBuffer<
                std::result::Result<u64, String>,
            > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

            for (seq_num, item) in rx {
                reorder.push(seq_num as usize, item);
                while let Some(result) = reorder.pop_ready() {
                    let ref_count = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                    sidecar_writer.write_all(&ref_count.to_le_bytes())?;
                    total_refs += ref_count;
                }
            }
            sidecar_writer.write_all(&total_refs.to_le_bytes())?;
            sidecar_writer.flush()?;
            Ok(())
        })?;
    }

    // Build rank index.
    node_id_set.build_rank_index();
    let unique_nodes = node_id_set.total_count();
    let unique_nodes_u64 = unique_nodes;

    // Load sidecar prefix sums for slot_pos computation in pass B.
    let slot_starts = load_ref_count_sidecar(ref_count_sidecar, total_refs)?;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s1a_pread_ms", s1a_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_decompress_ms", s1a_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_scan_ms", s1a_scan_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_unique_nodes", unique_nodes as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S1_PASS_A_END");

    // ---- Pass B: emit rank-bucketed (rank, slot_pos) records ----
    crate::debug::emit_marker("EXTJOIN_S1_PASS_B_START");

    let s1b_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s1b_decompress_ms = std::sync::atomic::AtomicU64::new(0);
    let s1b_scan_ms = std::sync::atomic::AtomicU64::new(0);

    next_idx.store(0, std::sync::atomic::Ordering::Relaxed);

    let worker_counts: std::sync::Mutex<Vec<Vec<u64>>> = std::sync::Mutex::new(Vec::new());
    let actual_num_workers: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    let pass_b_error: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    {
        let schedule_ref = &schedule;
        let next_ref = &next_idx;
        let node_id_set_ref = &node_id_set;
        let slot_starts_ref = &slot_starts;
        let worker_counts_ref = &worker_counts;
        let actual_ref = &actual_num_workers;
        let s1b_pread_ref = &s1b_pread_ms;
        let s1b_decompress_ref = &s1b_decompress_ms;
        let s1b_scan_ref = &s1b_scan_ms;
        let err_ref = &pass_b_error;

        std::thread::scope(|scope| -> Result<()> {
            for worker_id in 0..num_workers {
                let file = std::sync::Arc::clone(&shared_file);
                scope.spawn(move || {
                    use std::sync::atomic::Ordering::Relaxed;

                    // Per-worker rank-bucket shard files.
                    let mut shard_writers: Vec<Option<BufWriter<std::fs::File>>> = Vec::with_capacity(NUM_BUCKETS);
                    let mut entry_counts = vec![0u64; NUM_BUCKETS];
                    for bucket_idx in 0..NUM_BUCKETS {
                        let path = scratch.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
                        let f = match std::fs::File::create(&path) {
                            Ok(f) => f,
                            Err(e) => {
                                *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(format!("create rank shard {}: {e}", path.display()));
                                return;
                            }
                        };
                        shard_writers.push(Some(BufWriter::with_capacity(
                            super::external_radix::BUCKET_BUF_SIZE, f,
                        )));
                    }

                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();
                    let mut rec_buf = [0u8; RANK_RECORD_SIZE];

                    loop {
                        // Stop if another worker hit an error.
                        if err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
                            break;
                        }
                        let idx = next_ref.fetch_add(1, Relaxed);
                        if idx >= schedule_ref.len() { break; }
                        let task = &schedule_ref[idx];

                        let blob_result: std::result::Result<(), String> = (|| {
                            let t0 = std::time::Instant::now();
                            read_buf.resize(task.data_size, 0);
                            file.read_exact_at(&mut read_buf, task.data_offset)
                                .map_err(|e| format!("pass B pread: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1b_pread_ref.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);

                            let t1 = std::time::Instant::now();
                            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                                .map_err(|e| format!("pass B decompress: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1b_decompress_ref.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                            let t2 = std::time::Instant::now();
                            let slot_start = slot_starts_ref[task.seq as usize];
                            let mut local_ref_idx: u64 = 0;
                            let mut write_err: Option<String> = None;
                            super::way_scanner::scan_way_refs(
                                &decompress_buf, &mut refs_buf, &mut group_starts,
                                |_way_id, refs| {
                                    if write_err.is_some() { return; }
                                    for &node_id in refs {
                                        let global_rank = node_id_set_ref.rank(node_id);
                                        let rank_range = unique_nodes_u64.div_ceil(NUM_BUCKETS as u64);
                                        #[allow(clippy::cast_possible_truncation)]
                                        let bucket = if rank_range == 0 { 0 } else {
                                            (global_rank / rank_range) as usize
                                        }.min(NUM_BUCKETS - 1);
                                        let bucket_rank_start = bucket as u64 * rank_range;
                                        #[allow(clippy::cast_possible_truncation)]
                                        let local_rank = (global_rank - bucket_rank_start) as u32;
                                        let slot_pos = slot_start + local_ref_idx;
                                        let rec = RankRecord { local_rank, slot_pos };
                                        rec.write_to(&mut rec_buf);
                                        if let Some(w) = shard_writers[bucket].as_mut() {
                                            if let Err(e) = w.write_all(&rec_buf) {
                                                write_err = Some(format!("write rank shard: {e}"));
                                                return;
                                            }
                                        }
                                        entry_counts[bucket] += 1;
                                        local_ref_idx += 1;
                                    }
                                },
                            ).map_err(|e| e.to_string())?;
                            if let Some(e) = write_err { return Err(e); }
                            #[allow(clippy::cast_possible_truncation)]
                            s1b_scan_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);
                            Ok(())
                        })();

                        if let Err(e) = blob_result {
                            *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                            break;
                        }
                    }

                    // Flush shard writers.
                    for w in &mut shard_writers {
                        if let Some(writer) = w.as_mut() {
                            if let Err(e) = writer.flush() {
                                *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(format!("flush rank shard: {e}"));
                            }
                        }
                        *w = None;
                    }

                    actual_ref.fetch_add(1, Relaxed);
                    worker_counts_ref.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(entry_counts);
                });
            }
            Ok(())
        })?;
    }

    if let Some(e) = pass_b_error.into_inner().unwrap_or(None) {
        return Err(e.into());
    }

    let num_actual_workers = actual_num_workers.load(std::sync::atomic::Ordering::Relaxed);

    // Merge per-worker entry counts.
    let all_counts = worker_counts.into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut merged_counts = vec![0u64; NUM_BUCKETS];
    for counts in &all_counts {
        for (i, &c) in counts.iter().enumerate() {
            merged_counts[i] += c;
        }
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s1b_pread_ms", s1b_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_decompress_ms", s1b_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_scan_emit_ms", s1b_scan_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_blobs", schedule.len() as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S1_PASS_B_END");

    let num_actual = num_actual_workers;
    Ok((total_refs, unique_nodes_u64, merged_counts, num_actual, node_id_set))
}

// ---------------------------------------------------------------------------
// Stage 2: Node join — counting-sort per rank bucket, single-pass node merge
// ---------------------------------------------------------------------------

/// For each rank bucket: load records, counting-sort by rank, single-pass
/// node merge using rank order (= node-ID order by construction).
///
/// Uses a pipelined loader thread: one thread loads and counting-sorts
/// the next bucket while the consumer merges the current bucket against
/// the node stream. Queue depth 2 to hide load latency.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn stage2_node_join(
    input: &Path,
    _direct_io: bool,
    scratch: &ScratchDir,
    rank_bucket_counts: &[u64],
    num_shard_workers: usize,
    slot_buckets: &mut BucketWriters,
    total_slots: u64,
    unique_nodes: u64,
    node_id_set: &IdSetDense,
) -> Result<u64> {
    let mut resolved_count: u64 = 0;
    let rank_range_size = u64::from(unique_nodes).div_ceil(NUM_BUCKETS as u64);

    // A prepared rank bucket: loaded from disk, counting-sorted, ready
    // for the consumer to merge against the node stream.
    struct PreparedBucket {
        bucket_idx: usize,
        grouped_slot_pos: Vec<u64>,
        group_offsets: Vec<u64>,
        max_rank: u64,
        bucket_rank_start: u64,
    }

    /// Reusable scratch buffers for the bucket loader thread.
    struct LoaderScratch {
        data_buf: Vec<u8>,
        counts: Vec<u64>,
        write_pos: Vec<u64>,
    }

    /// Load shard files for one rank bucket, counting-sort by local rank
    /// directly from raw bytes (no intermediate RankRecord Vec), return
    /// a ready-to-consume PreparedBucket.
    #[allow(clippy::cast_possible_truncation)]
    fn prepare_bucket(
        bucket_idx: usize,
        scratch: &ScratchDir,
        num_shard_workers: usize,
        unique_nodes: u64,
        rank_range_size: u64,
        loader: &mut LoaderScratch,
    ) -> std::result::Result<PreparedBucket, String> {
        let bucket_rank_start = bucket_idx as u64 * rank_range_size;
        let bucket_rank_end = if bucket_idx == NUM_BUCKETS - 1 {
            unique_nodes
        } else {
            ((bucket_idx as u64 + 1) * rank_range_size).min(unique_nodes)
        };
        let local_range = (bucket_rank_end - bucket_rank_start) as usize;

        // Pass 1: load shard bytes and count occurrences per local rank
        // directly from raw bytes — no RankRecord materialization.
        loader.counts.clear();
        loader.counts.resize(local_range, 0);

        // Collect all shard bytes into data_buf (concatenated).
        loader.data_buf.clear();
        for worker_id in 0..num_shard_workers {
            let path = scratch.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
            let file = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("open rank shard: {e}")),
            };
            let len = file.metadata()
                .map_err(|e| format!("stat rank shard: {e}"))?
                .len() as usize;
            if len == 0 { continue; }
            let start = loader.data_buf.len();
            loader.data_buf.resize(start + len, 0);
            std::io::Read::read_exact(&mut &file, &mut loader.data_buf[start..])
                .map_err(|e| format!("read rank shard: {e}"))?;
            #[cfg(feature = "linux-direct-io")]
            advise_dontneed_file(&file);
        }

        // Count directly from raw bytes (12-byte records: u32 local_rank + u64 slot_pos).
        for chunk in loader.data_buf.chunks_exact(RANK_RECORD_SIZE) {
            let local_rank = u32::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3],
            ]) as usize;
            loader.counts[local_rank] += 1;
        }

        // Prefix sum → offsets.
        let mut group_offsets = vec![0u64; local_range + 1];
        for (i, count) in loader.counts.iter().enumerate() {
            group_offsets[i + 1] = group_offsets[i] + count;
        }

        // Pass 2: scatter slot_pos values directly from raw bytes.
        let total = group_offsets[local_range] as usize;
        let mut grouped_slot_pos = vec![0u64; total];
        loader.write_pos.clear();
        loader.write_pos.extend_from_slice(&group_offsets[..local_range]);
        for chunk in loader.data_buf.chunks_exact(RANK_RECORD_SIZE) {
            let local_rank = u32::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3],
            ]) as usize;
            let slot_pos = u64::from_le_bytes([
                chunk[4], chunk[5], chunk[6], chunk[7],
                chunk[8], chunk[9], chunk[10], chunk[11],
            ]);
            let pos = loader.write_pos[local_rank] as usize;
            grouped_slot_pos[pos] = slot_pos;
            loader.write_pos[local_rank] += 1;
        }

        Ok(PreparedBucket {
            bucket_idx,
            grouped_slot_pos,
            group_offsets,
            max_rank: bucket_rank_end,
            bucket_rank_start,
        })
    }

    // Pipelined bucket loader: one thread prepares buckets ahead of the
    // consumer. Queue depth 2 to hide load latency behind merge work.
    let (bucket_tx, bucket_rx) = std::sync::mpsc::sync_channel::<
        std::result::Result<PreparedBucket, String>
    >(2);

    // Parallel node scan (same P2b-v2 pattern as before).
    use super::node_scanner::{NodeTuple, extract_node_tuples};
    use std::os::unix::fs::FileExt;

    let mut blob_reader = crate::blob::BlobReader::seekable_from_path(input)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?
    );

    let mut entry_buf = [0u8; RESOLVED_ENTRY_SIZE];

    type Descriptor = (usize, u64, usize);
    type DecodedItem = (usize, crate::error::Result<Vec<NodeTuple>>);

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<Descriptor>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);

    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    let io_error: std::sync::Mutex<Option<crate::error::Error>> = std::sync::Mutex::new(None);

    let s2_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_decompress_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_extract_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_blobs = std::sync::atomic::AtomicU64::new(0);
    let s2_pread_ref = &s2_pread_ms;
    let s2_decompress_ref = &s2_decompress_ms;
    let s2_extract_ref = &s2_extract_ms;
    let s2_blobs_ref = &s2_blobs;

    let mut s2_recv_ms: u64 = 0;
    let mut s2_merge_ms: u64 = 0;
    let mut s2_bucket_load_ms: u64 = 0;
    let mut s2_bucket_loads: u64 = 0;

    let s2_stop = std::sync::atomic::AtomicBool::new(false);
    let s2_stop_ref = &s2_stop;

    std::thread::scope(|scope| -> Result<()> {
        // Bucket loader thread: prepares buckets ahead of the consumer.
        {
            let tx = bucket_tx;
            scope.spawn(move || {
                let mut loader = LoaderScratch {
                    data_buf: Vec::new(),
                    counts: Vec::new(),
                    write_pos: Vec::new(),
                };
                for bucket_idx in 0..NUM_BUCKETS {
                    if s2_stop_ref.load(std::sync::atomic::Ordering::Relaxed) { break; }
                    if rank_bucket_counts[bucket_idx] == 0 { continue; }
                    let result = prepare_bucket(
                        bucket_idx, scratch, num_shard_workers,
                        unique_nodes, rank_range_size,
                        &mut loader,
                    );
                    let is_err = result.is_err();
                    if tx.send(result).is_err() { break; }
                    if is_err { break; }
                }
            });
        }

        let io_error_ref = &io_error;
        scope.spawn(move || {
            let mut seq: usize = 0;
            while let Some(result) = blob_reader.next_header_with_data_offset() {
                match result {
                    Ok((header, _, data_offset, data_size)) => {
                        if !matches!(header.blob_type(), crate::blob::BlobType::OsmData) { continue; }
                        if let Some(idx) = header.index() {
                            if !matches!(idx.kind, crate::blob_index::ElemKind::Node) { continue; }
                        }
                        if desc_tx.send((seq, data_offset, data_size)).is_err() { break; }
                        seq += 1;
                    }
                    Err(e) => {
                        if let Ok(mut guard) = io_error_ref.lock() { *guard = Some(e); }
                        break;
                    }
                }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = decoded_tx.clone();
            let file = std::sync::Arc::clone(&shared_file);
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;
                let mut read_buf: Vec<u8> = Vec::new();
                let mut decompress_buf: Vec<u8> = Vec::new();
                let mut tuples: Vec<NodeTuple> = Vec::new();
                let mut group_starts: Vec<(usize, usize)> = Vec::new();

                loop {
                    let (seq, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(item) => item,
                            Err(_) => break,
                        }
                    };
                    let result = (|| -> crate::error::Result<Vec<NodeTuple>> {
                        let t0 = std::time::Instant::now();
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        #[allow(clippy::cast_possible_truncation)]
                        s2_pread_ref.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);

                        #[cfg(target_os = "linux")]
                        {
                            use std::os::unix::io::AsRawFd;
                            #[allow(clippy::cast_possible_wrap)]
                            unsafe {
                                libc::posix_fadvise(file.as_raw_fd(), data_offset as i64, data_size as i64, libc::POSIX_FADV_DONTNEED);
                            }
                        }

                        let t1 = std::time::Instant::now();
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;
                        #[allow(clippy::cast_possible_truncation)]
                        s2_decompress_ref.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                        let t2 = std::time::Instant::now();
                        tuples.clear();
                        extract_node_tuples(&decompress_buf, &mut tuples, &mut group_starts)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(e.to_string()))))?;
                        #[allow(clippy::cast_possible_truncation)]
                        s2_extract_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

                        s2_blobs_ref.fetch_add(1, Relaxed);
                        Ok(std::mem::take(&mut tuples))
                    })();
                    if tx.send((seq, result)).is_err() { break; }
                }
            });
        }
        drop(desc_rx);
        drop(decoded_tx);

        // Consumer: reorder node tuples + merge against pipelined prepared buckets.
        // The bucket loader thread feeds PreparedBuckets via bucket_rx,
        // overlapping load+sort with merge work.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<crate::error::Result<Vec<NodeTuple>>> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(32);

        // Receive the first prepared bucket.
        let mut current_bucket: Option<PreparedBucket> = match bucket_rx.recv() {
            Ok(Ok(b)) => Some(b),
            Ok(Err(e)) => {
                s2_stop_ref.store(true, std::sync::atomic::Ordering::Relaxed);
                return Err(e.into());
            }
            Err(_) => None,
        };

        loop {
            let t_recv = std::time::Instant::now();
            let msg = decoded_rx.recv();
            #[allow(clippy::cast_possible_truncation)]
            { s2_recv_ms += t_recv.elapsed().as_millis() as u64; }
            let (seq, item) = match msg {
                Ok(v) => v,
                Err(_) => break,
            };

            reorder.push(seq, item);

            while let Some(result) = reorder.pop_ready() {
                let tuples = result?;

                let t_merge = std::time::Instant::now();
                for &NodeTuple { id, lat, lon } in &tuples {
                    let rank = match node_id_set.rank_if_set(id) {
                        Some(r) => r,
                        None => continue,
                    };

                    // Advance to the rank bucket covering this rank.
                    while current_bucket.as_ref().is_none_or(|b| rank >= b.max_rank) {
                        let t_load = std::time::Instant::now();
                        current_bucket = match bucket_rx.recv() {
                            Ok(Ok(b)) => Some(b),
                            Ok(Err(e)) => {
                                s2_stop_ref.store(true, std::sync::atomic::Ordering::Relaxed);
                                return Err(e.into());
                            }
                            Err(_) => None,
                        };
                        #[allow(clippy::cast_possible_truncation)]
                        { s2_bucket_load_ms += t_load.elapsed().as_millis() as u64; }
                        s2_bucket_loads += 1;
                        if current_bucket.is_none() {
                            return Ok(());
                        }
                    }

                    let bkt = current_bucket.as_ref()
                        .ok_or("no bucket available for rank merge")?;

                    #[allow(clippy::cast_possible_truncation)]
                    let local_rank = (rank - bkt.bucket_rank_start) as usize;
                    #[allow(clippy::cast_possible_truncation)]
                    let start = bkt.group_offsets[local_rank] as usize;
                    #[allow(clippy::cast_possible_truncation)]
                    let end = bkt.group_offsets[local_rank + 1] as usize;

                    for &slot_pos in &bkt.grouped_slot_pos[start..end] {
                        let entry = ResolvedEntry { slot_pos, lat, lon };
                        let bucket = entry.slot_bucket(total_slots);
                        entry.write_to(&mut entry_buf);
                        if let Some(writer) = slot_buckets.writers[bucket].as_mut() {
                            writer.write_all(&entry_buf)?;
                        }
                        slot_buckets.entry_counts[bucket] += 1;
                        resolved_count += 1;
                    }
                }
                #[allow(clippy::cast_possible_truncation)]
                { s2_merge_ms += t_merge.elapsed().as_millis() as u64; }

                drop(tuples);
            }
        }

        Ok(())
    })?;

    if let Some(e) = io_error.into_inner().unwrap_or(None) {
        return Err(Box::new(e));
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s2_pread_ms", s2_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_decompress_ms", s2_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_extract_ms", s2_extract_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_blobs", s2_blobs.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_consumer_recv_ms", s2_recv_ms as i64);
        crate::debug::emit_counter("s2_consumer_merge_ms", s2_merge_ms as i64);
        crate::debug::emit_counter("s2_bucket_load_ms", s2_bucket_load_ms as i64);
        crate::debug::emit_counter("s2_bucket_loads", s2_bucket_loads as i64);
    }

    Ok(resolved_count)
}

// ---------------------------------------------------------------------------
// Stage 3: Slot reorder — build final coord_slots file
// ---------------------------------------------------------------------------

/// Parallel slot reorder: workers claim buckets via AtomicUsize, load
/// slot bucket file, scatter entries into a local buffer, pwrite to
/// the pre-sized coord_slots file at each bucket's disjoint byte range.
///
/// Lightweight reference to slot bucket paths + counts for stage 3.
/// Used when resuming from `--start-stage 3` without a live `BucketWriters`.
struct SlotBucketRef {
    paths: Vec<std::path::PathBuf>,
    entry_counts: Vec<u64>,
}

/// Stage 3 from a `BucketWriters` (normal path).
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
fn stage3_slot_reorder(
    slot_buckets: &BucketWriters,
    coord_slots_path: &Path,
    total_slots: u64,
) -> Result<()> {
    let r = SlotBucketRef {
        paths: slot_buckets.paths.clone(),
        entry_counts: slot_buckets.entry_counts.clone(),
    };
    stage3_slot_reorder_from_ref(&r, coord_slots_path, total_slots)
}

/// Pre-allocates the output file to `total_slots * 8` bytes (zero-filled
/// by the OS). Empty buckets need no explicit zero-write — the file is
/// already zeroed.
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
fn stage3_slot_reorder_from_ref(
    slot_buckets: &SlotBucketRef,
    coord_slots_path: &Path,
    total_slots: u64,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(coord_slots_path)
        .map_err(|e| format!("create coord_slots: {e}"))?;
    let total_bytes = total_slots * COORD_SLOT_SIZE as u64;
    file.set_len(total_bytes)
        .map_err(|e| format!("ftruncate coord_slots to {total_bytes}: {e}"))?;
    let shared_file = std::sync::Arc::new(file);

    let range_size = total_slots.div_ceil(NUM_BUCKETS as u64);

    // Cap workers for I/O-heavy stage — too many workers thrash the disk.
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6);

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let s3_load_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_scatter_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_write_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_buckets_loaded = std::sync::atomic::AtomicU64::new(0);
    let s3_error: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    let next_ref = &next_idx;
    let s3_load_ref = &s3_load_ms;
    let s3_scatter_ref = &s3_scatter_ms;
    let s3_write_ref = &s3_write_ms;
    let s3_loaded_ref = &s3_buckets_loaded;
    let err_ref = &s3_error;
    let entry_counts = &slot_buckets.entry_counts;
    let paths = &slot_buckets.paths;

    std::thread::scope(|scope| {
        for _ in 0..num_workers {
            let file = std::sync::Arc::clone(&shared_file);
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;
                let mut data_buf: Vec<u8> = Vec::new();
                let mut scatter_buf: Vec<u8> = Vec::new();
                let mut buf = [0u8; RESOLVED_ENTRY_SIZE];

                loop {
                    if err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
                        break;
                    }
                    let bucket_idx = next_ref.fetch_add(1, Relaxed);
                    if bucket_idx >= NUM_BUCKETS { break; }

                    let bucket_start = bucket_idx as u64 * range_size;
                    let bucket_end = if bucket_idx == NUM_BUCKETS - 1 {
                        total_slots
                    } else {
                        ((bucket_idx as u64 + 1) * range_size).min(total_slots)
                    };
                    let bucket_slots = bucket_end - bucket_start;

                    if entry_counts[bucket_idx] == 0 {
                        // Pre-sized file is already zeroed — nothing to write.
                        continue;
                    }

                    let result: std::result::Result<(), String> = (|| {
                        let bucket_bytes = bucket_slots as usize * COORD_SLOT_SIZE;
                        scatter_buf.clear();
                        scatter_buf.resize(bucket_bytes, 0);

                        let t_load = std::time::Instant::now();
                        data_buf.clear();
                        let bucket_file = std::fs::File::open(&paths[bucket_idx])
                            .map_err(|e| format!("open slot bucket: {e}"))?;
                        std::io::Read::read_to_end(&mut &bucket_file, &mut data_buf)
                            .map_err(|e| format!("read slot bucket: {e}"))?;
                        #[cfg(feature = "linux-direct-io")]
                        advise_dontneed_file(&bucket_file);
                        s3_load_ref.fetch_add(t_load.elapsed().as_millis() as u64, Relaxed);

                        let t_scatter = std::time::Instant::now();
                        for chunk in data_buf.chunks_exact(RESOLVED_ENTRY_SIZE) {
                            buf.copy_from_slice(chunk);
                            let entry = ResolvedEntry::read_from(&buf);
                            let local_pos = (entry.slot_pos - bucket_start) as usize;
                            let offset = local_pos * COORD_SLOT_SIZE;
                            scatter_buf[offset..offset + 4].copy_from_slice(&entry.lat.to_le_bytes());
                            scatter_buf[offset + 4..offset + 8].copy_from_slice(&entry.lon.to_le_bytes());
                        }
                        s3_scatter_ref.fetch_add(t_scatter.elapsed().as_millis() as u64, Relaxed);

                        let t_write = std::time::Instant::now();
                        let file_offset = bucket_start * COORD_SLOT_SIZE as u64;
                        file.write_all_at(&scatter_buf, file_offset)
                            .map_err(|e| format!("pwrite coord_slots: {e}"))?;
                        s3_write_ref.fetch_add(t_write.elapsed().as_millis() as u64, Relaxed);

                        s3_loaded_ref.fetch_add(1, Relaxed);
                        Ok(())
                    })();

                    if let Err(e) = result {
                        *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                        break;
                    }
                }
            });
        }
    });

    if let Some(e) = s3_error.into_inner().unwrap_or(None) {
        return Err(e.into());
    }

    // Sync to ensure all pwrite data is flushed before mmap in stage 4.
    shared_file.sync_data()
        .map_err(|e| format!("sync coord_slots: {e}"))?;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s3_load_ms", s3_load_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s3_scatter_ms", s3_scatter_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s3_write_ms", s3_write_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s3_buckets_loaded", s3_buckets_loaded.load(std::sync::atomic::Ordering::Relaxed) as i64);
    }

    Ok(())
}


// ---------------------------------------------------------------------------
// Stage 4: Assembly — emit enriched PBF
// ---------------------------------------------------------------------------

/// Memory-mapped coord_slots file for zero-syscall coordinate lookup.
/// Access is sequential (slot_pos advances monotonically during assembly),
/// so MADV_SEQUENTIAL enables kernel readahead. Replaces the previous
/// per-ref pread approach (8B syscalls at planet scale).
struct CoordSlots {
    mmap: memmap2::Mmap,
    total_slots: u64,
}

impl CoordSlots {
    fn open(path: &Path, total_slots: u64) -> Result<Self> {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("failed to open coord_slots: {e}"))?;
        let len = file.metadata()
            .map_err(|e| format!("failed to stat coord_slots: {e}"))?
            .len();
        if len == 0 {
            return Ok(Self {
                mmap: memmap2::MmapOptions::new().map_anon()?.make_read_only()?,
                total_slots: 0,
            });
        }
        let mmap = unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| format!("failed to mmap coord_slots: {e}"))?;
        #[cfg(unix)]
        {
            mmap.advise(memmap2::Advice::Sequential).ok();
        }
        Ok(Self { mmap, total_slots })
    }

    /// Read a coordinate at the given slot position. Zero syscalls — direct
    /// mmap byte access.
    #[allow(clippy::cast_possible_truncation)]
    fn get(&self, slot_pos: u64) -> Option<(i32, i32)> {
        if slot_pos >= self.total_slots {
            return None;
        }
        let offset = slot_pos as usize * COORD_SLOT_SIZE;
        let bytes = self.mmap.get(offset..offset + COORD_SLOT_SIZE)?;
        let lat = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let lon = i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if lat == 0 && lon == 0 {
            return None; // sentinel
        }
        Some((lat, lon))
    }
}

/// Blob descriptor for the stage 4 pre-scan schedule.
struct BlobDescriptor {
    seq: usize,
    data_offset: u64,
    data_size: usize,
    slot_start: u64,
    is_way_blob: bool,
}

/// Load the ref-count sidecar and compute prefix sums for slot_start values.
fn load_ref_count_sidecar(path: &Path, total_slots: u64) -> Result<Vec<u64>> {
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

/// Assembly pass: re-read the PBF, attach coordinates from coord_slots to ways.
/// P2c: pread-from-workers with pre-scan schedule for parallel decompress + assembly.
/// See notes/p2c-parallel-assembly-spec.md.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn stage4_assembly(
    input: &Path,
    output: &Path,
    coord_slots: &CoordSlots,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSetDense>,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
    ref_count_sidecar: &Path,
    total_slots: u64,
) -> Result<Stats> {
    use std::os::unix::fs::FileExt;

    // Load sidecar and compute slot_start prefix sums.
    let way_slot_starts = load_ref_count_sidecar(ref_count_sidecar, total_slots)?;
    // Header-only pre-scan: build the blob schedule.
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.set_parse_tagdata(true);
    // Skip OsmHeader.
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

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

    while let Some(result) = scanner.next_header_with_data_offset() {
        let (header_entry, _, data_offset, data_size) = result?;
        if !matches!(header_entry.blob_type(), crate::blob::BlobType::OsmData) {
            continue;
        }

        // Count blob types for diagnostics.
        if let Some(idx) = header_entry.index() {
            match idx.kind {
                crate::blob_index::ElemKind::Node => s4_node_blobs_total += 1,
                crate::blob_index::ElemKind::Way => s4_way_blobs += 1,
                crate::blob_index::ElemKind::Relation => s4_relation_blobs += 1,
            }
        }

        // P1b: skip node blobs with only untagged non-member nodes.
        if !keep_untagged_nodes {
            if let Some(idx) = header_entry.index() {
                if matches!(idx.kind, crate::blob_index::ElemKind::Node) {
                    let tag_index = header_entry.tag_index();
                    let has_tagindex = tag_index.is_some();
                    let has_tags = tag_index.is_none_or(|ti| !ti.keys_empty());
                    if !has_tagindex {
                        s4_node_blobs_no_tagindex += 1;
                    } else if !has_tags {
                        s4_node_blobs_empty_tags += 1;
                    }
                    if has_tags {
                        s4_node_blobs_kept_by_tags += 1;
                    } else {
                        let has_members = relation_member_node_ids
                            .is_some_and(|ids| ids.any_in_range(idx.min_id, idx.max_id));
                        if has_members {
                            s4_node_blobs_kept_by_members += 1;
                        }
                        if !has_members {
                            skipped_node_blobs += 1;
                            continue;
                        }
                    }
                }
            }
        }

        // Way blobs consume sidecar entries for slot_start.
        let slot_start = if let Some(idx) = header_entry.index() {
            if matches!(idx.kind, crate::blob_index::ElemKind::Way) {
                if way_sidecar_idx >= way_slot_starts.len() {
                    return Err("ref count sidecar has fewer entries than way blobs in PBF".into());
                }
                let start = way_slot_starts[way_sidecar_idx];
                way_sidecar_idx += 1;
                start
            } else {
                0
            }
        } else {
            0
        };

        let is_way_blob = header_entry.index()
            .is_some_and(|idx| matches!(idx.kind, crate::blob_index::ElemKind::Way));
        schedule.push(BlobDescriptor { seq, data_offset, data_size, slot_start, is_way_blob });
        seq += 1;
    }

    // Verify all sidecar entries were consumed.
    if way_sidecar_idx != way_slot_starts.len() {
        return Err(format!(
            "ref count sidecar has {} entries but only {} way blobs seen in PBF",
            way_slot_starts.len(), way_sidecar_idx,
        ).into());
    }

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

    type DecodedItem = (usize, crate::error::Result<(Vec<OwnedBlock>, Stats)>);
    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<BlobDescriptor>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
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
    let s4_pread_ref = &s4_pread_ms;
    let s4_decompress_ref = &s4_decompress_ms;
    let s4_assemble_ref = &s4_assemble_ms;
    let s4_way_reframe_ref = &s4_way_reframe_ms;
    let s4_nonway_assemble_ref = &s4_nonway_assemble_ms;
    let s4_way_blobs_ref = &s4_way_blobs_processed;
    let s4_nonway_blobs_ref = &s4_nonway_blobs_processed;
    let s4_send_ref = &s4_send_ms;
    let s4_blobs_ref = &s4_blobs;
    let way_reframe_counters = WayReframeCounters::new();
    let way_reframe_cref = &way_reframe_counters;

    // Consumer-side counters.
    let mut s4_recv_ms: u64 = 0;
    let mut s4_write_ms: u64 = 0;

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher: feed schedule into descriptor channel.
        scope.spawn(move || {
            for desc in schedule {
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
                let mut refs_buf: Vec<i64> = Vec::new();
                let mut locations_buf: Vec<(i32, i32)> = Vec::new();
                let mut way_reframe_scratch = WayReframeScratch::new();
                let mut reframe_output: Vec<u8> = Vec::new();

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

                        let t1 = std::time::Instant::now();
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;
                        #[allow(clippy::cast_possible_truncation)]
                        s4_decompress_ref.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                        let t2 = std::time::Instant::now();
                        output_blocks.clear();

                        if desc.is_way_blob {
                            // Wire-format reframe: splice locations without
                            // full PrimitiveBlock decode or BlockBuilder.
                            let (way_count, _new_slot_pos, min_id, max_id, missing) =
                                reframe_way_blob_with_locations(
                                    &decompress_buf,
                                    coord_slots,
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

                            let mut block_stats = Stats::default();
                            block_stats.ways_written = way_count;
                            block_stats.missing_locations = missing;
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
                        let block_stats = assemble_block(
                            &block,
                            &mut bb,
                            &mut output_blocks,
                            coord_slots,
                            desc.slot_start,
                            keep_untagged_nodes,
                            relation_member_node_ids,
                            &mut refs_buf,
                            &mut locations_buf,
                        ).map_err(|e| crate::error::new_error(
                            crate::error::ErrorKind::Io(std::io::Error::other(e))
                        ))?;
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

                    let t3 = std::time::Instant::now();
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

        // Consumer: reorder + write to PbfWriter.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            crate::error::Result<(Vec<OwnedBlock>, Stats)>
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(32);

        loop {
            let t_recv = std::time::Instant::now();
            let msg = decoded_rx.recv();
            #[allow(clippy::cast_possible_truncation)]
            { s4_recv_ms += t_recv.elapsed().as_millis() as u64; }
            let (seq_num, item) = match msg {
                Ok(v) => v,
                Err(_) => break,
            };

            reorder.push(seq_num, item);

            while let Some(result) = reorder.pop_ready() {
                let (blocks, block_stats) = result?;
                total_stats.merge(&block_stats);

                for (block_bytes, index, tagdata) in blocks {
                    let t_w = std::time::Instant::now();
                    writer.write_primitive_block_owned(
                        block_bytes, index, tagdata.as_deref(),
                    )?;
                    #[allow(clippy::cast_possible_truncation)]
                    { s4_write_ms += t_w.elapsed().as_millis() as u64; }
                }
            }
        }

        Ok(())
    })?;

    writer.flush()?;

    #[allow(clippy::cast_possible_wrap)]
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
        crate::debug::emit_counter("s4_consumer_recv_ms", s4_recv_ms as i64);
        crate::debug::emit_counter("s4_consumer_write_ms", s4_write_ms as i64);
        crate::debug::emit_counter("extjoin_skipped_node_blobs", skipped_node_blobs as i64);
        crate::debug::emit_counter("s4_node_blobs_total", s4_node_blobs_total as i64);
        crate::debug::emit_counter("s4_node_blobs_no_tagindex", s4_node_blobs_no_tagindex as i64);
        crate::debug::emit_counter("s4_node_blobs_empty_tags", s4_node_blobs_empty_tags as i64);
        crate::debug::emit_counter("s4_node_blobs_kept_by_tags", s4_node_blobs_kept_by_tags as i64);
        crate::debug::emit_counter("s4_node_blobs_kept_by_members", s4_node_blobs_kept_by_members as i64);
        crate::debug::emit_counter("s4_way_blobs", s4_way_blobs as i64);
        crate::debug::emit_counter("s4_relation_blobs", s4_relation_blobs as i64);
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
    coord_slots: &CoordSlots,
    mut way_slot_pos: u64,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSetDense>,
    refs_buf: &mut Vec<i64>,
    locations_buf: &mut Vec<(i32, i32)>,
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
                ensure_way_capacity_local(bb, output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                locations_buf.clear();
                for _node_id in refs_buf.iter() {
                    match coord_slots.get(way_slot_pos) {
                        Some(loc) => locations_buf.push(loc),
                        None => {
                            stats.missing_locations += 1;
                            locations_buf.push((0, 0));
                        }
                    }
                    way_slot_pos += 1;
                }
                let meta = element_metadata(&w.info());
                bb.add_way_with_locations(w.id(), w.tags(), refs_buf, locations_buf, meta.as_ref());
                stats.ways_written += 1;
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
struct WayReframeCounters {
    parse_block_ms: std::sync::atomic::AtomicU64,
    parse_way_ms: std::sync::atomic::AtomicU64,
    coord_lookup_ms: std::sync::atomic::AtomicU64,
    reassemble_ms: std::sync::atomic::AtomicU64,
    refs_total: std::sync::atomic::AtomicU64,
    ways_total: std::sync::atomic::AtomicU64,
}

impl WayReframeCounters {
    fn new() -> Self {
        Self {
            parse_block_ms: std::sync::atomic::AtomicU64::new(0),
            parse_way_ms: std::sync::atomic::AtomicU64::new(0),
            coord_lookup_ms: std::sync::atomic::AtomicU64::new(0),
            reassemble_ms: std::sync::atomic::AtomicU64::new(0),
            refs_total: std::sync::atomic::AtomicU64::new(0),
            ways_total: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[allow(clippy::cast_possible_wrap)]
    fn emit(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        crate::debug::emit_counter("s4_way_parse_block_ms", self.parse_block_ms.load(Relaxed) as i64);
        crate::debug::emit_counter("s4_way_parse_way_ms", self.parse_way_ms.load(Relaxed) as i64);
        crate::debug::emit_counter("s4_way_coord_lookup_ms", self.coord_lookup_ms.load(Relaxed) as i64);
        crate::debug::emit_counter("s4_way_reassemble_ms", self.reassemble_ms.load(Relaxed) as i64);
        crate::debug::emit_counter("s4_way_refs_total", self.refs_total.load(Relaxed) as i64);
        crate::debug::emit_counter("s4_way_messages_total", self.ways_total.load(Relaxed) as i64);
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
/// groups, relation groups, and all non-ref way fields verbatim. Only
/// decodes way refs (field 8) to count them and look up coords.
///
/// Returns `(way_count, way_slot_pos_after, min_way_id, max_way_id, missing_locations)`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn reframe_way_blob_with_locations(
    decompressed: &[u8],
    coord_slots: &CoordSlots,
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

    // Level 1: PrimitiveBlock — find string table + groups.
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
    counters.parse_block_ms.fetch_add(t_block.elapsed().as_millis() as u64, Relaxed);

    output.clear();
    protohoggr::encode_bytes_field(output, 1, stringtable_bytes);

    let mut total_ways: u64 = 0;
    let mut min_way_id: i64 = i64::MAX;
    let mut max_way_id: i64 = i64::MIN;
    let mut missing_locations: u64 = 0;
    let mut blob_refs: u64 = 0;

    // Level 2: process each PrimitiveGroup.
    for &(gr_offset, gr_len) in &scratch.group_ranges {
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];
        scratch.group_out.clear();

        let mut gr_cursor = Cursor::new(group_bytes);
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(|e| format!("reframe gfield: {e}"))? {
            if field == 3 && wire_type == WIRE_LEN {
                // Way submessage — splice locations.
                let way_bytes = gr_cursor.read_len_delimited().map_err(|e| format!("reframe way: {e}"))?;

                let t_way = std::time::Instant::now();
                let mut way_id: i64 = 0;
                let mut refs_data: &[u8] = &[];
                let mut refs_range: Option<(usize, usize)> = None;

                let mut way_cursor = Cursor::new(way_bytes);
                while let Some((wf, wt)) = way_cursor.read_tag().map_err(|e| format!("reframe wfield: {e}"))? {
                    if wf == 1 && wt == WIRE_VARINT {
                        way_id = way_cursor.read_varint_i64().map_err(|e| format!("reframe id: {e}"))?;
                    } else if wf == 8 && wt == WIRE_LEN {
                        let val_start = way_bytes.len() - way_cursor.remaining();
                        let tag_start = val_start - 1; // field 8 tag = 1 byte
                        refs_data = way_cursor.read_len_delimited().map_err(|e| format!("reframe refs: {e}"))?;
                        let val_end = way_bytes.len() - way_cursor.remaining();
                        refs_range = Some((tag_start, val_end));
                    } else {
                        way_cursor.skip_field(wt).map_err(|e| format!("reframe wskip: {e}"))?;
                    }
                }

                #[allow(clippy::cast_possible_truncation)]
                counters.parse_way_ms.fetch_add(t_way.elapsed().as_millis() as u64, Relaxed);

                if way_id < min_way_id { min_way_id = way_id; }
                if way_id > max_way_id { max_way_id = way_id; }

                // Count refs and look up locations.
                let t_coord = std::time::Instant::now();
                scratch.packed_lats.clear();
                scratch.packed_lons.clear();
                let mut last_lat: i64 = 0;
                let mut last_lon: i64 = 0;
                let mut ref_count: u64 = 0;

                if !refs_data.is_empty() {
                    let mut ref_cursor = Cursor::new(refs_data);
                    while ref_cursor.remaining() > 0 {
                        // Skip the ref delta — we don't need the node ID,
                        // just need to count refs for slot_pos advancement.
                        ref_cursor.read_varint().map_err(|e| format!("reframe ref varint: {e}"))?;

                        let (lat, lon) = match coord_slots.get(way_slot_pos) {
                            Some(loc) => loc,
                            None => {
                                missing_locations += 1;
                                (0, 0)
                            }
                        };
                        way_slot_pos += 1;
                        ref_count += 1;

                        let lat_i64 = i64::from(lat);
                        let lon_i64 = i64::from(lon);
                        protohoggr::encode_varint(
                            &mut scratch.packed_lats,
                            protohoggr::zigzag_encode_64(lat_i64 - last_lat),
                        );
                        protohoggr::encode_varint(
                            &mut scratch.packed_lons,
                            protohoggr::zigzag_encode_64(lon_i64 - last_lon),
                        );
                        last_lat = lat_i64;
                        last_lon = lon_i64;
                    }
                }

                #[allow(clippy::cast_possible_truncation)]
                counters.coord_lookup_ms.fetch_add(t_coord.elapsed().as_millis() as u64, Relaxed);
                blob_refs += ref_count;

                // Build reframed way: original bytes + appended fields 9, 10.
                let t_reassemble = std::time::Instant::now();
                scratch.reframed_way.clear();
                if let Some((refs_start, refs_end)) = refs_range {
                    // Copy everything before refs field.
                    scratch.reframed_way.extend_from_slice(&way_bytes[..refs_start]);
                    // Copy refs field verbatim.
                    scratch.reframed_way.extend_from_slice(&way_bytes[refs_start..refs_end]);
                    // Copy everything after refs field (other fields like keys, vals, info
                    // that appeared after refs — field order is not guaranteed).
                    scratch.reframed_way.extend_from_slice(&way_bytes[refs_end..]);
                } else {
                    // No refs field — copy way bytes verbatim.
                    scratch.reframed_way.extend_from_slice(way_bytes);
                }
                // Append location fields.
                if ref_count > 0 {
                    protohoggr::encode_bytes_field(&mut scratch.reframed_way, 9, &scratch.packed_lats);
                    protohoggr::encode_bytes_field(&mut scratch.reframed_way, 10, &scratch.packed_lons);
                }

                protohoggr::encode_bytes_field(&mut scratch.group_out, 3, &scratch.reframed_way);
                #[allow(clippy::cast_possible_truncation)]
                counters.reassemble_ms.fetch_add(t_reassemble.elapsed().as_millis() as u64, Relaxed);
                total_ways += 1;
            } else {
                // Non-way field in the group — copy verbatim.
                let raw = gr_cursor.read_raw_field(wire_type).map_err(|e| format!("reframe gskip: {e}"))?;
                protohoggr::encode_tag(&mut scratch.group_out, field, wire_type);
                scratch.group_out.extend_from_slice(raw);
            }
        }

        protohoggr::encode_bytes_field(output, 2, &scratch.group_out);
    }

    // Append scalar fields (granularity, etc.).
    output.extend_from_slice(&scratch.scalar_fields);

    counters.refs_total.fetch_add(blob_refs, Relaxed);
    counters.ways_total.fetch_add(total_ways, Relaxed);

    Ok((total_ways, way_slot_pos, min_way_id, max_way_id, missing_locations))
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the full external join pipeline for add-locations-to-ways.
///
/// Bounded memory (<1 GB), all sequential I/O. Uses ~224 GB temp disk at
/// planet scale. See module docs for the algorithm.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn external_join(
    input: &Path,
    output: &Path,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
    keep_scratch: bool,
    start_stage: Option<u8>,
) -> Result<Stats> {
    require_indexdata(
        input,
        direct_io,
        force,
        "external join requires indexdata for efficient blob filtering",
    )?;

    {
        let reader = ElementReader::open(input, direct_io)?;
        if !reader.header().is_sorted() {
            return Err("external join requires a sorted PBF (Sort.Type_then_ID). \
                        The single-pass node merge depends on ascending node ID order."
                .into());
        }
    }

    // Stable scratch dir name when keep_scratch or start_stage is set,
    // so subsequent runs can find the persisted state.
    let scratch_dir = if keep_scratch || start_stage.is_some() {
        ScratchDir::new_stable(output.parent().unwrap_or(Path::new(".")), "external-join")?
    } else {
        ScratchDir::new(output.parent().unwrap_or(Path::new(".")), "external-join")?
    };

    let manifest_path = scratch_dir.file_path("manifest");
    let ref_count_sidecar = scratch_dir.file_path("way-ref-counts");
    let coord_slots_path = scratch_dir.file_path("coord_slots");
    let start = start_stage.unwrap_or(1);

    // --- Read manifest if resuming ---
    let (total_slots, unique_nodes, rank_bucket_counts, num_shard_workers, node_id_set) =
        if start >= 2 {
            // Re-run pass A to rebuild IdSetDense (~7s on Europe).
            // Read total_slots from manifest.
            let manifest = std::fs::read(&manifest_path)
                .map_err(|e| format!("read manifest for --start-stage: {e}. Run with --keep-scratch first."))?;
            if manifest.len() < 8 {
                return Err("manifest too small".into());
            }
            let total_slots = u64::from_le_bytes(manifest[..8].try_into()
                .map_err(|_| "manifest read failed")?);

            // Rebuild IdSetDense via pass A (cheap: 7s Europe, 21s planet).
            let schedule = build_way_schedule(input)?;
            let shared_file = std::sync::Arc::new(
                std::fs::File::open(input)
                    .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
            );
            let num_workers = std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(2).max(1))
                .unwrap_or(4);

            let mut node_id_set = super::id_set_dense::IdSetDense::new();
            #[allow(clippy::cast_possible_wrap)]
            node_id_set.pre_allocate(MAX_NODE_ID as i64);

            let next_idx = std::sync::atomic::AtomicUsize::new(0);
            {
                let schedule_ref = &schedule;
                let next_ref = &next_idx;
                let node_id_set_ref = &node_id_set;

                std::thread::scope(|scope| {
                    for _ in 0..num_workers {
                        let file = std::sync::Arc::clone(&shared_file);
                        scope.spawn(move || {
                            use std::os::unix::fs::FileExt as _;
                            let mut read_buf: Vec<u8> = Vec::new();
                            let mut decompress_buf: Vec<u8> = Vec::new();
                            let mut refs_buf: Vec<i64> = Vec::new();
                            let mut group_starts: Vec<(usize, usize)> = Vec::new();

                            loop {
                                let idx = next_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if idx >= schedule_ref.len() { break; }
                                let task = &schedule_ref[idx];
                                read_buf.resize(task.data_size, 0);
                                if file.read_exact_at(&mut read_buf, task.data_offset).is_err() { break; }
                                if crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf).is_err() { break; }
                                drop(super::way_scanner::scan_way_refs(
                                    &decompress_buf, &mut refs_buf, &mut group_starts,
                                    |_way_id, refs| {
                                        for &node_id in refs {
                                            node_id_set_ref.set_atomic(node_id);
                                        }
                                    },
                                ));
                            }
                        });
                    }
                });
            }
            node_id_set.build_rank_index();
            let unique_nodes = node_id_set.total_count();

            // Rank bucket counts: read from shard files on disk.
            let mut rank_bucket_counts = vec![0u64; NUM_BUCKETS];
            let mut num_shard_workers = 0usize;
            loop {
                let path = scratch_dir.path.join(format!("rank-W{num_shard_workers}-000"));
                if path.exists() {
                    num_shard_workers += 1;
                } else {
                    break;
                }
            }
            num_shard_workers = num_shard_workers.max(1);
            for bucket_idx in 0..NUM_BUCKETS {
                for worker_id in 0..num_shard_workers {
                    let path = scratch_dir.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
                    if let Ok(meta) = std::fs::metadata(&path) {
                        rank_bucket_counts[bucket_idx] += meta.len() / RANK_RECORD_SIZE as u64;
                    }
                }
            }

            (total_slots, unique_nodes, rank_bucket_counts, num_shard_workers, node_id_set)
        } else {
            // --- Stage 1: Two-pass way scan ---
            crate::debug::emit_marker("EXTJOIN_STAGE1_START");
            let (total_slots, unique_nodes, rank_bucket_counts, num_shard_workers, node_id_set) =
                stage1_way_pass(input, direct_io, &scratch_dir, &ref_count_sidecar)?;
            let total_coo: u64 = rank_bucket_counts.iter().sum();
            #[allow(clippy::cast_possible_wrap)]
            {
                crate::debug::emit_counter("extjoin_total_slots", total_slots as i64);
                crate::debug::emit_counter("extjoin_total_coo", total_coo as i64);
                crate::debug::emit_counter("extjoin_unique_nodes", unique_nodes as i64);
            }
            crate::debug::emit_marker("EXTJOIN_STAGE1_END");

            // Write manifest for future --start-stage runs.
            if keep_scratch {
                std::fs::write(&manifest_path, total_slots.to_le_bytes())
                    .map_err(|e| format!("write manifest: {e}"))?;
            }

            (total_slots, unique_nodes, rank_bucket_counts, num_shard_workers, node_id_set)
        };

    if start <= 2 {
        // --- Stage 2: Node join ---
        crate::debug::emit_marker("EXTJOIN_STAGE2_START");
        let mut slot_buckets = BucketWriters::create(&scratch_dir, "slot")?;
        let resolved_count =
            stage2_node_join(input, direct_io, &scratch_dir, &rank_bucket_counts, num_shard_workers, &mut slot_buckets, total_slots, unique_nodes, &node_id_set)?;
        slot_buckets.finish()?;
        if !keep_scratch {
            for worker_id in 0..num_shard_workers {
                for bucket_idx in 0..NUM_BUCKETS {
                    let path = scratch_dir.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
                    drop(std::fs::remove_file(&path));
                }
            }
        }
        #[allow(clippy::cast_possible_wrap)]
        crate::debug::emit_counter("extjoin_resolved_count", resolved_count as i64);
        crate::debug::emit_marker("EXTJOIN_STAGE2_END");
    }

    if start <= 3 {
        // --- Stage 3: Slot reorder ---
        crate::debug::emit_marker("EXTJOIN_STAGE3_START");
        // Re-read slot bucket entry counts from disk if resuming.
        let slot_entry_counts: Vec<u64> = if start >= 3 {
            (0..NUM_BUCKETS).map(|i| {
                let path = scratch_dir.bucket_path("slot", i);
                std::fs::metadata(&path).map(|m| m.len() / RESOLVED_ENTRY_SIZE as u64).unwrap_or(0)
            }).collect()
        } else {
            // Slot buckets still in memory from stage 2 — read counts from files.
            (0..NUM_BUCKETS).map(|i| {
                let path = scratch_dir.bucket_path("slot", i);
                std::fs::metadata(&path).map(|m| m.len() / RESOLVED_ENTRY_SIZE as u64).unwrap_or(0)
            }).collect()
        };
        // Build a minimal BucketWriters-like struct for stage 3 (it only reads paths + counts).
        let slot_paths: Vec<std::path::PathBuf> = (0..NUM_BUCKETS)
            .map(|i| scratch_dir.bucket_path("slot", i))
            .collect();
        let slot_bucket_ref = SlotBucketRef { paths: slot_paths, entry_counts: slot_entry_counts };
        stage3_slot_reorder_from_ref(&slot_bucket_ref, &coord_slots_path, total_slots)?;
        if !keep_scratch {
            for i in 0..NUM_BUCKETS {
                drop(std::fs::remove_file(&scratch_dir.bucket_path("slot", i)));
            }
        }
        crate::debug::emit_marker("EXTJOIN_STAGE3_END");
    }

    // Collect relation member node IDs (for node filtering in stage 4).
    let relation_member_node_ids = if keep_untagged_nodes {
        None
    } else {
        Some(super::add_locations_to_ways::collect_relation_member_node_ids(
            input, direct_io,
        )?)
    };

    // --- Stage 4: Assembly ---
    crate::debug::emit_marker("EXTJOIN_STAGE4_START");
    let coord_slots = CoordSlots::open(&coord_slots_path, total_slots)?;
    let stats = stage4_assembly(
        input,
        output,
        &coord_slots,
        keep_untagged_nodes,
        relation_member_node_ids.as_ref(),
        compression,
        direct_io,
        overrides,
        &ref_count_sidecar,
        total_slots,
    )?;
    crate::debug::emit_marker("EXTJOIN_STAGE4_END");

    if !keep_scratch {
        drop(scratch_dir); // cleanup temp files
    } else {
        std::mem::forget(scratch_dir); // prevent Drop from cleaning up
    }

    Ok(stats)
}
