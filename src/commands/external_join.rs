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

/// Size of a rank-occurrence record: `(rank: u32, slot_pos: u64)` = 12 bytes.
const RANK_RECORD_SIZE: usize = 12;

/// Size of a resolved entry: `(slot_pos: u64, lat: i32, lon: i32)` = 16 bytes.
const RESOLVED_ENTRY_SIZE: usize = 16;

/// Size of a coordinate slot: `(lat: i32, lon: i32)` = 8 bytes.
const COORD_SLOT_SIZE: usize = 8;

// ---------------------------------------------------------------------------
// Rank-occurrence record: (rank, slot_pos)
// ---------------------------------------------------------------------------

/// A rank-bucketed occurrence record. `rank` is the dense ordinal of the
/// node_id in the `IdSetDense` bitset (computed via `rank(node_id)`).
/// `slot_pos` is the final position in the coord_slots array.
///
/// Rank order matches node-ID order by construction (IdSetDense::rank()
/// counts set bits below the ID), so stage 2 can process rank buckets
/// in ascending order with a single-pass node scan.
#[derive(Clone, Copy)]
struct RankRecord {
    rank: u32,
    slot_pos: u64,
}

impl RankRecord {
    fn write_to(&self, buf: &mut [u8; RANK_RECORD_SIZE]) {
        buf[..4].copy_from_slice(&self.rank.to_le_bytes());
        buf[4..12].copy_from_slice(&self.slot_pos.to_le_bytes());
    }

    fn read_from(buf: &[u8; RANK_RECORD_SIZE]) -> Self {
        let rank = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let slot_pos = u64::from_le_bytes([
            buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
        ]);
        Self { rank, slot_pos }
    }

    /// Bucket index for rank partitioning.
    #[allow(clippy::cast_possible_truncation)]
    fn rank_bucket(&self, total_unique_nodes: u32) -> usize {
        let range_size = (total_unique_nodes as u64).div_ceil(NUM_BUCKETS as u64);
        if range_size == 0 { return 0; }
        let bucket = u64::from(self.rank) / range_size;
        (bucket as usize).min(NUM_BUCKETS - 1)
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
/// Returns `(total_refs, unique_nodes, rank_bucket_entry_counts, node_id_set)`.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
fn stage1_way_pass(
    input: &Path,
    _direct_io: bool,
    scratch: &ScratchDir,
    ref_count_sidecar: &Path,
) -> Result<(u64, u32, Vec<u64>, IdSetDense)> {
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
    let unique_nodes_u32 = u32::try_from(unique_nodes)
        .map_err(|_| format!("unique referenced nodes ({unique_nodes}) exceeds u32::MAX"))?;

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
                                        #[allow(clippy::cast_possible_truncation)]
                                        let rank = node_id_set_ref.rank(node_id) as u32;
                                        let slot_pos = slot_start + local_ref_idx;
                                        let rec = RankRecord { rank, slot_pos };
                                        let bucket = rec.rank_bucket(unique_nodes_u32);
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

    // Consolidate per-worker shards into one file per rank bucket.
    // Reduces stage 2 from N×256 file opens to 256.
    let t_consolidate = std::time::Instant::now();
    let num_actual = num_actual_workers;
    for bucket_idx in 0..NUM_BUCKETS {
        if merged_counts[bucket_idx] == 0 { continue; }
        let consolidated_path = scratch.path.join(format!("rank-{bucket_idx:03}"));
        let mut out = std::fs::File::create(&consolidated_path)
            .map_err(|e| format!("create consolidated rank bucket: {e}"))?;
        for worker_id in 0..num_actual {
            let shard_path = scratch.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
            let mut shard = match std::fs::File::open(&shard_path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("open shard for consolidation: {e}").into()),
            };
            std::io::copy(&mut shard, &mut out)
                .map_err(|e| format!("copy shard to consolidated: {e}"))?;
            drop(std::fs::remove_file(&shard_path));
        }
    }
    // Remove any remaining empty shard files.
    for worker_id in 0..num_actual {
        for bucket_idx in 0..NUM_BUCKETS {
            let path = scratch.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
            drop(std::fs::remove_file(&path));
        }
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    crate::debug::emit_counter("s1_consolidate_ms", t_consolidate.elapsed().as_millis() as i64);

    Ok((total_refs, unique_nodes_u32, merged_counts, node_id_set))
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
    slot_buckets: &mut BucketWriters,
    total_slots: u64,
    unique_nodes: u32,
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
        max_rank: u32,
        bucket_rank_start: u32,
    }

    /// Load shard files for one rank bucket, counting-sort by local rank,
    /// return a ready-to-consume PreparedBucket.
    #[allow(clippy::cast_possible_truncation)]
    fn prepare_bucket(
        bucket_idx: usize,
        scratch: &ScratchDir,
        unique_nodes: u32,
        rank_range_size: u64,
    ) -> std::result::Result<PreparedBucket, String> {
        let bucket_rank_start = (bucket_idx as u64 * rank_range_size) as u32;
        let bucket_rank_end = if bucket_idx == NUM_BUCKETS - 1 {
            unique_nodes
        } else {
            (((bucket_idx as u64 + 1) * rank_range_size) as u32).min(unique_nodes)
        };
        let local_range = (bucket_rank_end - bucket_rank_start) as usize;

        // Load records from consolidated rank bucket file.
        let path = scratch.path.join(format!("rank-{bucket_idx:03}"));
        let mut data_buf: Vec<u8> = Vec::new();
        let file = std::fs::File::open(&path)
            .map_err(|e| format!("open rank bucket {bucket_idx}: {e}"))?;
        let len = file.metadata()
            .map_err(|e| format!("stat rank bucket: {e}"))?
            .len() as usize;
        data_buf.resize(len, 0);
        std::io::Read::read_exact(&mut &file, &mut data_buf)
            .map_err(|e| format!("read rank bucket: {e}"))?;
        #[cfg(feature = "linux-direct-io")]
        advise_dontneed_file(&file);

        let mut records: Vec<RankRecord> = Vec::with_capacity(len / RANK_RECORD_SIZE);
        let mut buf = [0u8; RANK_RECORD_SIZE];
        for chunk in data_buf.chunks_exact(RANK_RECORD_SIZE) {
            buf.copy_from_slice(chunk);
            records.push(RankRecord::read_from(&buf));
        }

        // Counting sort by local rank.
        let mut counts = vec![0u64; local_range];
        for rec in &records {
            let local = (rec.rank - bucket_rank_start) as usize;
            counts[local] += 1;
        }
        let mut group_offsets = vec![0u64; local_range + 1];
        for (i, count) in counts.iter().enumerate() {
            group_offsets[i + 1] = group_offsets[i] + count;
        }
        let total = group_offsets[local_range] as usize;
        let mut grouped_slot_pos = vec![0u64; total];
        let mut write_pos = group_offsets[..local_range].to_vec();
        for rec in &records {
            let local = (rec.rank - bucket_rank_start) as usize;
            let pos = write_pos[local] as usize;
            grouped_slot_pos[pos] = rec.slot_pos;
            write_pos[local] += 1;
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
                for bucket_idx in 0..NUM_BUCKETS {
                    if s2_stop_ref.load(std::sync::atomic::Ordering::Relaxed) { break; }
                    if rank_bucket_counts[bucket_idx] == 0 { continue; }
                    let result = prepare_bucket(
                        bucket_idx, scratch,
                        unique_nodes, rank_range_size,
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
                    if !node_id_set.get(id) {
                        continue;
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    let rank = node_id_set.rank(id) as u32;

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
/// Pre-allocates the output file to `total_slots * 8` bytes (zero-filled
/// by the OS). Empty buckets need no explicit zero-write — the file is
/// already zeroed.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
fn stage3_slot_reorder(
    slot_buckets: &BucketWriters,
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

    while let Some(result) = scanner.next_header_with_data_offset() {
        let (header_entry, _, data_offset, data_size) = result?;
        if !matches!(header_entry.blob_type(), crate::blob::BlobType::OsmData) {
            continue;
        }

        // P1b: skip node blobs with only untagged non-member nodes.
        if !keep_untagged_nodes {
            if let Some(idx) = header_entry.index() {
                if matches!(idx.kind, crate::blob_index::ElemKind::Node) {
                    let has_tags = header_entry.tag_index()
                        .is_none_or(|ti| !ti.keys_empty());
                    if !has_tags {
                        let has_members = relation_member_node_ids
                            .is_some_and(|ids| ids.any_in_range(idx.min_id, idx.max_id));
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

        schedule.push(BlobDescriptor { seq, data_offset, data_size, slot_start });
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
    let s4_send_ms = std::sync::atomic::AtomicU64::new(0);
    let s4_blobs = std::sync::atomic::AtomicU64::new(0);
    let s4_pread_ref = &s4_pread_ms;
    let s4_decompress_ref = &s4_decompress_ms;
    let s4_assemble_ref = &s4_assemble_ms;
    let s4_send_ref = &s4_send_ms;
    let s4_blobs_ref = &s4_blobs;

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
                        let block = PrimitiveBlock::new(
                            bytes::Bytes::from(std::mem::take(&mut decompress_buf))
                        )?;
                        #[allow(clippy::cast_possible_truncation)]
                        s4_decompress_ref.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                        let t2 = std::time::Instant::now();
                        output_blocks.clear();
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
                        s4_assemble_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

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
        crate::debug::emit_counter("s4_send_ms", s4_send_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_blobs", s4_blobs.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s4_consumer_recv_ms", s4_recv_ms as i64);
        crate::debug::emit_counter("s4_consumer_write_ms", s4_write_ms as i64);
        crate::debug::emit_counter("extjoin_skipped_node_blobs", skipped_node_blobs as i64);
    }

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
// Public entry point
// ---------------------------------------------------------------------------

/// Run the full external join pipeline for add-locations-to-ways.
///
/// Bounded memory (<1 GB), all sequential I/O. Uses ~224 GB temp disk at
/// planet scale. See module docs for the algorithm.
#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub fn external_join(
    input: &Path,
    output: &Path,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    require_indexdata(
        input,
        direct_io,
        force,
        "external join requires indexdata for efficient blob filtering",
    )?;

    // The single-pass node merge in stage 2 requires sorted PBF input
    // (nodes in ascending ID order). Verify the header declares Sort.Type_then_ID.
    {
        let reader = ElementReader::open(input, direct_io)?;
        if !reader.header().is_sorted() {
            return Err("external join requires a sorted PBF (Sort.Type_then_ID). \
                        The single-pass node merge depends on ascending node ID order."
                .into());
        }
    }

    let scratch_dir = ScratchDir::new(output.parent().unwrap_or(Path::new(".")), "external-join")?;

    // --- Stage 1: Two-pass way scan ---
    crate::debug::emit_marker("EXTJOIN_STAGE1_START");
    let ref_count_sidecar = scratch_dir.file_path("way-ref-counts");
    let (total_slots, unique_nodes, rank_bucket_counts, node_id_set) =
        stage1_way_pass(input, direct_io, &scratch_dir, &ref_count_sidecar)?;
    let total_coo: u64 = rank_bucket_counts.iter().sum();
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extjoin_total_slots", total_slots as i64);
        crate::debug::emit_counter("extjoin_total_coo", total_coo as i64);
        crate::debug::emit_counter("extjoin_unique_nodes", i64::from(unique_nodes));
    }

    // --- Stage 2: Node join (rank-bucketed, counting sort) ---
    crate::debug::emit_marker("EXTJOIN_STAGE1_END");
    crate::debug::emit_marker("EXTJOIN_STAGE2_START");
    let mut slot_buckets = BucketWriters::create(&scratch_dir, "slot")?;
    let resolved_count =
        stage2_node_join(input, direct_io, &scratch_dir, &rank_bucket_counts, &mut slot_buckets, total_slots, unique_nodes, &node_id_set)?;
    slot_buckets.finish()?;
    // Clean up consolidated rank bucket files.
    for bucket_idx in 0..NUM_BUCKETS {
        let path = scratch_dir.path.join(format!("rank-{bucket_idx:03}"));
        drop(std::fs::remove_file(&path));
    }
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("extjoin_resolved_count", resolved_count as i64);

    // --- Stage 3: Slot reorder ---
    crate::debug::emit_marker("EXTJOIN_STAGE2_END");
    crate::debug::emit_marker("EXTJOIN_STAGE3_START");
    let coord_slots_path = scratch_dir.file_path("coord_slots");
    stage3_slot_reorder(&slot_buckets, &coord_slots_path, total_slots)?;
    slot_buckets.cleanup();

    // Collect relation member node IDs (for node filtering in stage 4).
    // Deferred to here to avoid holding ~1.4 GB (Europe) during stages 1-3.
    let relation_member_node_ids = if keep_untagged_nodes {
        None
    } else {
        Some(super::add_locations_to_ways::collect_relation_member_node_ids(
            input, direct_io,
        )?)
    };
    // --- Stage 4: Assembly ---
    crate::debug::emit_marker("EXTJOIN_STAGE3_END");
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

    // scratch_dir dropped here → cleanup all temp files.
    Ok(stats)
}
