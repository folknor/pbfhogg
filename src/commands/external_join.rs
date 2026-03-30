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
use std::path::{Path, PathBuf};

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::Compression;
use crate::{Element, ElementReader, PrimitiveBlock};

use super::add_locations_to_ways::Stats;
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

/// Number of buckets for radix partitioning. 256 = partition by high byte.
const NUM_BUCKETS: usize = 256;

/// Maximum node ID in current OSM data. Used to compute bucket ranges.
/// 14B gives headroom above the current ~13B maximum.
const MAX_NODE_ID: u64 = 14_000_000_000;

/// Size of the write buffer per bucket file (256 KB).
const BUCKET_BUF_SIZE: usize = 256 * 1024;

/// Size of a COO pair on disk: `(node_id: i64, slot_pos: u64)` = 16 bytes.
const COO_PAIR_SIZE: usize = 16;

/// Size of a resolved entry: `(slot_pos: u64, lat: i32, lon: i32)` = 16 bytes.
const RESOLVED_ENTRY_SIZE: usize = 16;

/// Size of a coordinate slot: `(lat: i32, lon: i32)` = 8 bytes.
const COORD_SLOT_SIZE: usize = 8;

// ---------------------------------------------------------------------------
// Scratch directory management
// ---------------------------------------------------------------------------

/// Managed scratch directory for bucket files. Cleaned up on drop.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(parent: &Path) -> Result<Self> {
        let path = parent.join(format!(".pbfhogg-external-join-{}", std::process::id()));
        std::fs::create_dir_all(&path).map_err(|e| {
            format!("failed to create scratch directory {}: {e}", path.display())
        })?;
        Ok(Self { path })
    }

    fn bucket_path(&self, prefix: &str, index: usize) -> PathBuf {
        self.path.join(format!("{prefix}-{index:03}"))
    }

    fn file_path(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Best-effort cleanup. Ignore errors (crash leaves stale dir, user can clean).
        drop(std::fs::remove_dir_all(&self.path));
    }
}

// ---------------------------------------------------------------------------
// Bucket writers
// ---------------------------------------------------------------------------

/// Set of buffered writers for radix bucket files.
struct BucketWriters {
    writers: Vec<Option<BufWriter<std::fs::File>>>,
    paths: Vec<PathBuf>,
    entry_counts: Vec<u64>,
}

impl BucketWriters {
    /// Create bucket files eagerly. Each bucket gets a buffered writer.
    fn create(scratch: &ScratchDir, prefix: &str) -> Result<Self> {
        let mut writers = Vec::with_capacity(NUM_BUCKETS);
        let mut paths = Vec::with_capacity(NUM_BUCKETS);
        let entry_counts = vec![0u64; NUM_BUCKETS];

        for i in 0..NUM_BUCKETS {
            let path = scratch.bucket_path(prefix, i);
            let file = std::fs::File::create(&path)
                .map_err(|e| format!("failed to create bucket {}: {e}", path.display()))?;
            writers.push(Some(BufWriter::with_capacity(BUCKET_BUF_SIZE, file)));
            paths.push(path);
        }

        Ok(Self { writers, paths, entry_counts })
    }

    /// Flush, sync, fadvise(DONTNEED), and close all writers.
    /// sync_data ensures pages are clean so fadvise can evict them.
    fn finish(&mut self) -> Result<Vec<u64>> {
        for writer in &mut self.writers {
            if let Some(w) = writer.as_mut() {
                w.flush()?;
                #[cfg(feature = "linux-direct-io")]
                {
                    use std::os::unix::io::AsRawFd;
                    drop(w.get_ref().sync_data());
                    unsafe {
                        libc::posix_fadvise(
                            w.get_ref().as_raw_fd(),
                            0,
                            0,
                            libc::POSIX_FADV_DONTNEED,
                        )
                    };
                }
            }
            *writer = None;
        }
        Ok(self.entry_counts.clone())
    }

    /// Delete all bucket files.
    fn cleanup(&self) {
        for path in &self.paths {
            drop(std::fs::remove_file(path));
        }
    }
}

// ---------------------------------------------------------------------------
// COO pair: (node_id, slot_pos)
// ---------------------------------------------------------------------------

/// A coordinate-list (COO) pair linking a node ID to a position in the
/// way-ref stream where its coordinates should be placed.
#[derive(Clone, Copy)]
struct CooPair {
    node_id: i64,
    slot_pos: u64,
}

impl CooPair {
    fn write_to(&self, buf: &mut [u8; COO_PAIR_SIZE]) {
        buf[..8].copy_from_slice(&self.node_id.to_le_bytes());
        buf[8..].copy_from_slice(&self.slot_pos.to_le_bytes());
    }

    fn read_from(buf: &[u8; COO_PAIR_SIZE]) -> Self {
        let node_id = i64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        let slot_pos = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        Self { node_id, slot_pos }
    }

    /// Bucket index for node-id partitioning.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn node_bucket(&self) -> usize {
        let id = if self.node_id < 0 { 0u64 } else { self.node_id as u64 };
        let range_size = MAX_NODE_ID.div_ceil(NUM_BUCKETS as u64);
        let bucket = id / range_size;
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
// Stage 1: Way pass — emit COO pairs into node buckets
// ---------------------------------------------------------------------------

/// Scan all way blobs and emit `(node_id, slot_pos)` pairs into node buckets.
///
/// Returns the total number of way-node refs (= total coord slots needed).
#[hotpath::measure]
fn stage1_way_pass(
    input: &Path,
    direct_io: bool,
    node_buckets: &mut BucketWriters,
    ref_count_sidecar: &Path,
) -> Result<u64> {
    // Sequential reader to avoid PrimitiveBlock cross-thread retention.
    // Pipelined reader retains ~11 GB anon at Europe scale (360K way blobs),
    // which carries into stage 2 and pushes peak to 20 GB.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let decompress_pool = crate::blob::DecompressPool::new();
    let mut slot_pos: u64 = 0;
    let mut pair_buf = [0u8; COO_PAIR_SIZE];

    // P2c sidecar: per-way-blob ref counts for stage 4 slot_pos pre-computation.
    // Stage 1 and stage 4 must see way blobs in the same file order (both filter
    // by ElemKind::Way from the same indexdata). Changing blob ordering without
    // updating both stages would silently corrupt the sidecar alignment.
    let mut sidecar_writer = BufWriter::with_capacity(
        64 * 1024,
        std::fs::File::create(ref_count_sidecar)
            .map_err(|e| format!("failed to create ref count sidecar: {e}"))?,
    );

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        if let Some(idx) = blob.index() {
            if !matches!(idx.kind, crate::blob_index::ElemKind::Way) {
                continue;
            }
        }

        let decompressed = blob.decompress_pooled(&decompress_pool)?;
        let block = PrimitiveBlock::new(decompressed)?;
        let blob_start_pos = slot_pos;
        for element in block.elements_skip_metadata() {
            if let Element::Way(w) = element {
                for node_id in w.refs() {
                    let pair = CooPair { node_id, slot_pos };
                    let bucket = pair.node_bucket();
                    pair.write_to(&mut pair_buf);
                    if let Some(writer) = node_buckets.writers[bucket].as_mut() {
                        writer.write_all(&pair_buf)?;
                    }
                    node_buckets.entry_counts[bucket] += 1;
                    slot_pos += 1;
                }
            }
        }
        // Write per-blob ref count to sidecar.
        let blob_ref_count = slot_pos - blob_start_pos;
        sidecar_writer.write_all(&blob_ref_count.to_le_bytes())?;
    }

    // Trailer: total ref count for alignment verification in stage 4.
    sidecar_writer.write_all(&slot_pos.to_le_bytes())?;
    sidecar_writer.flush()?;

    Ok(slot_pos)
}

// ---------------------------------------------------------------------------
// Stage 2: Node join — merge-join per bucket, emit resolved entries
// ---------------------------------------------------------------------------

/// For each node bucket: load into RAM, sort by node_id, merge-join with
/// the matching node stream, emit resolved `(slot_pos, lat, lon)` entries
/// into slot buckets.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
fn stage2_node_join(
    input: &Path,
    _direct_io: bool,
    node_buckets: &BucketWriters,
    slot_buckets: &mut BucketWriters,
    total_slots: u64,
) -> Result<u64> {
    let mut resolved_count: u64 = 0;
    let range_size = MAX_NODE_ID.div_ceil(NUM_BUCKETS as u64);

    // Single-pass node merge: read PBF nodes exactly once, advancing through
    // buckets as node IDs increase. Since PBF nodes are sorted by ID and
    // buckets partition the ID space into ascending ranges, each node falls
    // into exactly one bucket. We load one bucket at a time (~500 MB peak).
    //
    // Previous implementation: 256 separate PBF reads (one per bucket),
    // each decompressing ALL node blobs. That was 256× the I/O cost.

    // Pre-load all non-empty buckets sorted by node_id.
    // We advance through them as the node stream progresses.
    let mut bucket_idx: usize = 0;
    let mut sorted_pairs: Vec<CooPair> = Vec::new();
    let mut data_buf: Vec<u8> = Vec::new();
    let mut pair_cursor: usize = 0;
    let mut bucket_max_id: i64 = 0;

    // Advance to first non-empty bucket. Reuses data_buf and sorted_pairs
    // allocations across bucket loads to prevent heap accumulation — at Europe
    // scale, 256 buckets × ~290 MB each would otherwise accumulate 27+ GB of
    // unreturned heap memory from the allocator.
    fn load_next_bucket(
        bucket_idx: &mut usize,
        sorted_pairs: &mut Vec<CooPair>,
        data_buf: &mut Vec<u8>,
        pair_cursor: &mut usize,
        bucket_max_id: &mut i64,
        node_buckets: &BucketWriters,
        range_size: u64,
    ) -> Result<bool> {
        while *bucket_idx < NUM_BUCKETS {
            if node_buckets.entry_counts[*bucket_idx] > 0 {
                load_coo_bucket_into(
                    &node_buckets.paths[*bucket_idx],
                    data_buf,
                    sorted_pairs,
                )?;
                sorted_pairs.sort_unstable_by_key(|p| p.node_id);
                *pair_cursor = 0;
                // Last bucket covers everything above its lower bound —
                // prevents silent data loss if node IDs exceed MAX_NODE_ID.
                *bucket_max_id = if *bucket_idx == NUM_BUCKETS - 1 {
                    i64::MAX
                } else {
                    #[allow(clippy::cast_possible_truncation)]
                    { ((*bucket_idx as u64 + 1) * range_size).cast_signed() }
                };
                return Ok(true);
            }
            *bucket_idx += 1;
        }
        Ok(false)
    }

    let has_bucket = load_next_bucket(
        &mut bucket_idx, &mut sorted_pairs, &mut data_buf, &mut pair_cursor,
        &mut bucket_max_id, node_buckets, range_size,
    )?;

    if !has_bucket {
        return Ok(0); // No COO pairs at all
    }

    // P2b-v2: Parallel node-only scan with worker-side pread. IO thread reads
    // only headers (~50 bytes), filters by indexdata, sends lightweight descriptors.
    // Workers pread blob data from shared file, decompress + extract tuples with
    // all alloc/free thread-local. Eliminates cross-thread Blob ownership that
    // caused 20 GB retention in P2b-v1. See notes/p2b-parallel-tuples-spec.md.
    use super::node_scanner::{NodeTuple, extract_node_tuples};
    use std::os::unix::fs::FileExt;

    // Seekable reader for header-only iteration.
    let mut blob_reader = crate::blob::BlobReader::seekable_from_path(input)?;
    blob_reader.set_parse_indexdata(true);
    // Skip the OsmHeader blob.
    blob_reader.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    // Shared file for worker pread access.
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?
    );

    let mut entry_buf = [0u8; RESOLVED_ENTRY_SIZE];

    // Descriptor: (seq, data_offset, data_size)
    type Descriptor = (usize, u64, usize);
    type DecodedItem = (usize, crate::error::Result<Vec<NodeTuple>>);

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<Descriptor>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);

    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    std::thread::scope(|scope| -> Result<()> {
        // IO thread: read headers only, filter to node blobs, send descriptors.
        scope.spawn(move || {
            let mut seq: usize = 0;
            while let Some(result) = blob_reader.next_header_with_data_offset() {
                match result {
                    Ok((header, _, data_offset, data_size)) => {
                        if !matches!(header.blob_type(), crate::blob::BlobType::OsmData) {
                            continue;
                        }
                        if let Some(idx) = header.index() {
                            if !matches!(idx.kind, crate::blob_index::ElemKind::Node) {
                                continue;
                            }
                        }
                        if desc_tx.send((seq, data_offset, data_size)).is_err() {
                            break;
                        }
                        seq += 1;
                    }
                    Err(_) => break,
                }
            }
        });

        // Worker threads: pread blob data, decompress, extract tuples.
        // Each worker owns all its buffers — zero cross-thread alloc/free.
        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = decoded_tx.clone();
            let file = std::sync::Arc::clone(&shared_file);
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let mut decompress_buf: Vec<u8> = Vec::new();
                let mut tuples: Vec<NodeTuple> = Vec::new();

                loop {
                    let (seq, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(item) => item,
                            Err(_) => break, // channel closed
                        }
                    };
                    let result = (|| -> crate::error::Result<Vec<NodeTuple>> {
                        // pread blob data — no shared file offset mutation.
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(
                                crate::error::ErrorKind::Io(e)
                            ))?;

                        // Evict pages we just read (worker-side, safe from races).
                        #[cfg(target_os = "linux")]
                        {
                            use std::os::unix::io::AsRawFd;
                            #[allow(clippy::cast_possible_wrap)]
                            unsafe {
                                libc::posix_fadvise(
                                    file.as_raw_fd(),
                                    data_offset as i64,
                                    data_size as i64,
                                    libc::POSIX_FADV_DONTNEED,
                                );
                            }
                        }

                        // Parse wire blob + decompress into thread-local buffer.
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;

                        // Extract node tuples into thread-local Vec.
                        tuples.clear();
                        extract_node_tuples(&decompress_buf, &mut tuples)
                            .map_err(|e| crate::error::new_error(
                                crate::error::ErrorKind::Io(std::io::Error::other(e.to_string()))
                            ))?;

                        // Move tuples out, replace with empty Vec. The consumer
                        // drops the Vec — but only ~32 are in flight (channel
                        // capacity), so this is bounded cross-thread churn.
                        Ok(std::mem::take(&mut tuples))
                    })();
                    if tx.send((seq, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(desc_rx); // allow workers to see channel close
        drop(decoded_tx);

        // Consumer: reorder + merge-join.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<crate::error::Result<Vec<NodeTuple>>> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(32);

        for (seq, item) in decoded_rx {
            reorder.push(seq, item);

            while let Some(result) = reorder.pop_ready() {
                let tuples = result?;

                for &NodeTuple { id, lat, lon } in &tuples {
                    // Advance to the bucket that covers this node ID.
                    while id >= bucket_max_id {
                        bucket_idx += 1;
                        let has = load_next_bucket(
                            &mut bucket_idx, &mut sorted_pairs, &mut data_buf, &mut pair_cursor,
                            &mut bucket_max_id, node_buckets, range_size,
                        )?;
                        if !has {
                            return Ok(());
                        }
                    }

                    while pair_cursor < sorted_pairs.len()
                        && sorted_pairs[pair_cursor].node_id < id
                    {
                        pair_cursor += 1;
                    }

                    while pair_cursor < sorted_pairs.len()
                        && sorted_pairs[pair_cursor].node_id == id
                    {
                        let entry = ResolvedEntry {
                            slot_pos: sorted_pairs[pair_cursor].slot_pos,
                            lat,
                            lon,
                        };
                        let bucket = entry.slot_bucket(total_slots);
                        entry.write_to(&mut entry_buf);
                        if let Some(writer) = slot_buckets.writers[bucket].as_mut() {
                            writer.write_all(&entry_buf)?;
                        }
                        slot_buckets.entry_counts[bucket] += 1;
                        resolved_count += 1;
                        pair_cursor += 1;
                    }
                }

                drop(tuples);
            }
        }

        Ok(())
    })?;

    Ok(resolved_count)
}

/// Load a COO bucket file into reusable buffers. Both `data_buf` and `pairs`
/// are cleared and refilled — their backing allocations are retained across
/// bucket loads, preventing heap accumulation from the allocator holding
/// freed blocks.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
fn load_coo_bucket_into(path: &Path, data_buf: &mut Vec<u8>, pairs: &mut Vec<CooPair>) -> Result<()> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("failed to open bucket {}: {e}", path.display()))?;
    let len = file.metadata()
        .map_err(|e| format!("failed to stat bucket {}: {e}", path.display()))?
        .len() as usize;
    data_buf.clear();
    data_buf.resize(len, 0);
    std::io::Read::read_exact(&mut &file, data_buf)
        .map_err(|e| format!("failed to read bucket {}: {e}", path.display()))?;
    #[cfg(feature = "linux-direct-io")]
    advise_dontneed_file(&file);

    pairs.clear();
    let count = data_buf.len() / COO_PAIR_SIZE;
    if count > pairs.capacity() {
        pairs.reserve(count - pairs.capacity());
    }
    let mut buf = [0u8; COO_PAIR_SIZE];
    for chunk in data_buf.chunks_exact(COO_PAIR_SIZE) {
        buf.copy_from_slice(chunk);
        pairs.push(CooPair::read_from(&buf));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Stage 3: Slot reorder — build final coord_slots file
// ---------------------------------------------------------------------------

/// Read slot buckets in order, scatter entries into a dense buffer per bucket,
/// write the coord_slots file sequentially.
///
/// Each bucket covers a contiguous range of slot positions. Instead of sorting
/// entries and issuing 4.69B individual pwrite calls (which was 72% of total
/// time at Europe scale), we scatter entries by position into a zeroed buffer
/// and write the entire buffer once per bucket.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
fn stage3_slot_reorder(
    slot_buckets: &BucketWriters,
    coord_slots_path: &Path,
    total_slots: u64,
) -> Result<()> {
    let file = std::fs::File::create(coord_slots_path)
        .map_err(|e| format!("failed to create coord_slots file {}: {e}", coord_slots_path.display()))?;
    let mut out = BufWriter::with_capacity(256 * 1024, file);

    let range_size = total_slots.div_ceil(NUM_BUCKETS as u64);
    let mut data_buf: Vec<u8> = Vec::new();
    let mut scatter_buf: Vec<u8> = Vec::new();
    let mut next_slot: u64 = 0;

    for bucket_idx in 0..NUM_BUCKETS {
        // Compute this bucket's slot range.
        let bucket_start = bucket_idx as u64 * range_size;
        let bucket_end = if bucket_idx == NUM_BUCKETS - 1 {
            total_slots
        } else {
            ((bucket_idx as u64 + 1) * range_size).min(total_slots)
        };
        let bucket_slots = bucket_end - bucket_start;

        if slot_buckets.entry_counts[bucket_idx] == 0 {
            // Empty bucket — write zero sentinels for its entire range.
            let zero_bytes = bucket_slots as usize * COORD_SLOT_SIZE;
            scatter_buf.clear();
            scatter_buf.resize(zero_bytes, 0);
            out.write_all(&scatter_buf)?;
            next_slot = bucket_end;
            continue;
        }

        // Load entries and scatter into position-indexed buffer.
        // No sort needed — position is computed directly from slot_pos.
        let bucket_bytes = bucket_slots as usize * COORD_SLOT_SIZE;
        scatter_buf.clear();
        scatter_buf.resize(bucket_bytes, 0);

        data_buf.clear();
        let file = std::fs::File::open(&slot_buckets.paths[bucket_idx])
            .map_err(|e| format!("failed to open slot bucket {}: {e}", slot_buckets.paths[bucket_idx].display()))?;
        std::io::Read::read_to_end(&mut &file, &mut data_buf)
            .map_err(|e| format!("failed to read slot bucket {}: {e}", slot_buckets.paths[bucket_idx].display()))?;
        #[cfg(feature = "linux-direct-io")]
        advise_dontneed_file(&file);

        let mut buf = [0u8; RESOLVED_ENTRY_SIZE];
        for chunk in data_buf.chunks_exact(RESOLVED_ENTRY_SIZE) {
            buf.copy_from_slice(chunk);
            let entry = ResolvedEntry::read_from(&buf);
            let local_pos = (entry.slot_pos - bucket_start) as usize;
            let offset = local_pos * COORD_SLOT_SIZE;
            scatter_buf[offset..offset + 4].copy_from_slice(&entry.lat.to_le_bytes());
            scatter_buf[offset + 4..offset + 8].copy_from_slice(&entry.lon.to_le_bytes());
        }

        out.write_all(&scatter_buf)?;
        next_slot = bucket_end;

    }

    // Write any trailing slots if total_slots doesn't align to bucket boundaries.
    if next_slot < total_slots {
        let remaining = (total_slots - next_slot) as usize * COORD_SLOT_SIZE;
        scatter_buf.clear();
        scatter_buf.resize(remaining, 0);
        out.write_all(&scatter_buf)?;
    }

    out.flush()?;
    Ok(())
}


// ---------------------------------------------------------------------------
// Stage 4: Assembly — emit enriched PBF
// ---------------------------------------------------------------------------

/// Advise the kernel to evict a single file's pages from page cache.
#[cfg(feature = "linux-direct-io")]
fn advise_dontneed_file(file: &std::fs::File) {
    use std::os::unix::io::AsRawFd;
    unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
}

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

    let mut total_stats = Stats {
        nodes_read: 0, nodes_written: 0, nodes_dropped: 0,
        ways_written: 0, relations_written: 0, missing_locations: 0,
        blobs_passthrough: 0, blobs_decoded: 0,
    };

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
                        // Pread blob data.
                        read_buf.resize(desc.data_size, 0);
                        file.read_exact_at(&mut read_buf, desc.data_offset)
                            .map_err(|e| crate::error::new_error(
                                crate::error::ErrorKind::Io(e)
                            ))?;

                        // Decompress + parse into PrimitiveBlock.
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;
                        let block = PrimitiveBlock::new(
                            bytes::Bytes::from(std::mem::take(&mut decompress_buf))
                        )?;

                        // Assemble.
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

                        // Reclaim the decompress buffer for reuse.
                        // PrimitiveBlock took ownership via Bytes::from(take()),
                        // but it's dropped now so we just need a fresh Vec.
                        if decompress_buf.capacity() == 0 {
                            decompress_buf = Vec::new();
                        }

                        Ok((std::mem::take(&mut output_blocks), block_stats))
                    })();

                    if tx.send((desc.seq, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(desc_rx);
        drop(decoded_tx);

        // Consumer: reorder + write to PbfWriter.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            crate::error::Result<(Vec<OwnedBlock>, Stats)>
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(32);

        for (seq_num, item) in decoded_rx {
            reorder.push(seq_num, item);

            while let Some(result) = reorder.pop_ready() {
                let (blocks, block_stats) = result?;
                merge_stats(&mut total_stats, &block_stats);

                for (block_bytes, index, tagdata) in blocks {
                    writer.write_primitive_block_owned(
                        block_bytes, index, tagdata.as_deref(),
                    )?;
                }
            }
        }

        Ok(())
    })?;

    writer.flush()?;
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("extjoin_skipped_node_blobs", skipped_node_blobs as i64);
    Ok(total_stats)
}

fn merge_stats(dst: &mut Stats, src: &Stats) {
    dst.nodes_read += src.nodes_read;
    dst.nodes_written += src.nodes_written;
    dst.nodes_dropped += src.nodes_dropped;
    dst.ways_written += src.ways_written;
    dst.relations_written += src.relations_written;
    dst.missing_locations += src.missing_locations;
    dst.blobs_passthrough += src.blobs_passthrough;
    dst.blobs_decoded += src.blobs_decoded;
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
    let mut stats = Stats {
        nodes_read: 0,
        nodes_written: 0,
        nodes_dropped: 0,
        ways_written: 0,
        relations_written: 0,
        missing_locations: 0,
        blobs_passthrough: 0,
        blobs_decoded: 0,
    };

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

    let scratch_dir = ScratchDir::new(output.parent().unwrap_or(Path::new(".")))?;

    // --- Stage 1: Way pass ---
    crate::debug::emit_marker("EXTJOIN_STAGE1_START");
    let mut node_buckets = BucketWriters::create(&scratch_dir, "node")?;
    let ref_count_sidecar = scratch_dir.file_path("way-ref-counts");
    let total_slots = stage1_way_pass(input, direct_io, &mut node_buckets, &ref_count_sidecar)?;
    let node_counts = node_buckets.finish()?;
    let total_coo: u64 = node_counts.iter().sum();
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extjoin_total_slots", total_slots as i64);
        crate::debug::emit_counter("extjoin_total_coo", total_coo as i64);
    }

    // --- Stage 2: Node join ---
    crate::debug::emit_marker("EXTJOIN_STAGE1_END");
    crate::debug::emit_marker("EXTJOIN_STAGE2_START");
    let mut slot_buckets = BucketWriters::create(&scratch_dir, "slot")?;
    let resolved_count =
        stage2_node_join(input, direct_io, &node_buckets, &mut slot_buckets, total_slots)?;
    slot_buckets.finish()?;
    node_buckets.cleanup();
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
