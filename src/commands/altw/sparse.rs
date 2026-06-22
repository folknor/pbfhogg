//! Rank-indexed flat node coordinate store.
//!
//! Pre-allocates a `referenced.total_count() * 8`-byte temp file and
//! mmaps it `MmapMut`. Pass 1 workers write `(lat, lon)` at byte offset
//! `rank_if_set(node_id) << 3` via `AtomicU64::store(Relaxed)` directly
//! into the mmap; pass 2's `get(node_id)` is `rank_if_set(node_id)` plus
//! an `AtomicU64::load(Relaxed)` at the same offset. No chunk format,
//! no `start_pad`, no sentinel padding inside chunks (unwritten slots
//! stay zero, which is the existing sentinel).
//!
//! Trade-offs vs the previous chunk-indexed encoding (Planetiler-style):
//! - Disk shrinks ~2.4x at japan density (5.7 GB -> 2.4 GB; 8 bytes /
//!   referenced node, no chunk-padding overhead).
//! - Pass 1 becomes parallel: workers write directly via mmap with no
//!   serial consumer, so the dispatcher / merge bottleneck disappears.
//! - The strictly-increasing-id precondition is gone. Random insertion
//!   order works because each rank slot is unique and the AtomicU64
//!   stores are race-free per slot.
//! - At the cost of carrying the IdSet (and its rank index) into pass 2:
//!   ~440 MB + ~100 MB at planet, vs the chunk format's ~440 MB
//!   `offsets`+`start_pad`. Net RAM is roughly flat.
//!
//! The sentinel for "node id was referenced but never written" is
//! `(lat, lon) == (0, 0)`. `set_len` zero-fills the file, so unwritten
//! slots return `None` from `get`. This matches the prior chunk-format
//! semantics; the rare collision (a real OSM node at exactly `(0, 0)`
//! decimicrodegrees) was already silently absent under the old code.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::idset::IdSet;

use super::Result;

/// Rank-indexed flat node coordinate store.
pub(super) struct SparseArrayIndex {
    /// Carries the `referenced` IdSet plus its rank index. `get` does
    /// `rank_if_set(node_id) -> Some(rank)` then an `AtomicU64::load`
    /// at `rank * 8`.
    referenced: IdSet,
    mmap: memmap2::MmapMut,
    _file: std::fs::File,
}

impl SparseArrayIndex {
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        let rank = self.referenced.rank_if_set(node_id)?;
        let byte_offset = (rank * 8) as usize;
        if byte_offset + 8 > self.mmap.len() {
            return None;
        }
        // SAFETY: byte_offset + 8 <= mmap.len(), pointer is 8-byte aligned
        // (page-aligned base + 8*rank). Atomic load pairs with atomic
        // stores in `SharedSparseWriter::insert` to eliminate data-race UB.
        let packed = unsafe {
            let ptr = self.mmap.as_ptr().add(byte_offset).cast::<AtomicU64>();
            (*ptr).load(Ordering::Relaxed)
        };
        if packed == 0 {
            return None;
        }
        let lat = packed as i32;
        let lon = (packed >> 32) as i32;
        Some((lat, lon))
    }
}

/// Thread-safe writer for parallel sparse-rank index population.
///
/// Holds a raw pointer into the `SparseArrayIndex` mmap buffer. Each
/// referenced node id maps to a disjoint 8-byte slot
/// (`base + rank_if_set(id) * 8`). All writes use
/// `AtomicU64::store(Relaxed)`, eliminating data-race UB even if
/// duplicate node IDs appear in the input.
struct SharedSparseWriter {
    base: *mut u8,
    capacity_bytes: usize,
}

// SAFETY: All writes use atomic operations (AtomicU64 stores). The raw
// pointer requires manual Send+Sync; lifetime is bounded by the
// synchronous parallel scan in `build_node_index_sparse`.
unsafe impl Send for SharedSparseWriter {}
unsafe impl Sync for SharedSparseWriter {}

impl SharedSparseWriter {
    /// Insert a referenced node's coordinates at byte offset `rank * 8`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn insert(&self, rank: u64, lat: i32, lon: i32) {
        let byte_offset = (rank * 8) as usize;
        if byte_offset + 8 > self.capacity_bytes {
            return;
        }
        let packed = u64::from(lat as u32) | (u64::from(lon as u32) << 32);
        // SAFETY: byte_offset + 8 <= capacity_bytes = mmap length.
        // Pointer is 8-byte aligned (page-aligned base + 8*rank).
        // Atomic store eliminates data-race UB even with duplicate ids.
        unsafe {
            let ptr = self.base.add(byte_offset).cast::<AtomicU64>();
            (*ptr).store(packed, Ordering::Relaxed);
        }
    }
}

/// Build the rank-indexed flat sparse array index from node blobs.
///
/// Steps:
///   1. `referenced.build_rank_index()` (~100 MB at planet).
///   2. Pre-allocate a `total * 8`-byte temp file via `set_len`,
///      mmap it `MmapMut`.
///   3. `parallel_scan_blobs_raw` over node blobs: workers extract
///      `(id, lat, lon)` tuples via `scan::node::extract_node_tuples`
///      (wire-only, no `PrimitiveBlock`). Each worker stores 8 bytes
///      into the mmap at `referenced.rank_if_set(id) << 3` for every
///      referenced id, via `AtomicU64::store(Relaxed)`. Workers do
///      not coordinate beyond the shared `&IdSet` and `SharedSparseWriter` -
///      each rank slot is touched at most once, atomic stores make
///      race-free even on duplicates.
///   4. Wrap the mmap into `SparseArrayIndex`, return.
///
/// `direct_io` is intentionally dropped: blob bodies are pread'd from
/// the shared input file handle on worker threads, incompatible with
/// `O_DIRECT` alignment requirements.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn build_node_index_sparse(
    input: &Path,
    _direct_io: bool,
    scratch_dir: &Path,
    mut referenced: IdSet,
) -> Result<SparseArrayIndex> {
    referenced.build_rank_index();
    let total_bytes = referenced.total_count().saturating_mul(8);

    let temp_path = scratch_dir.join(format!(".pbfhogg-sparse-index-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|e| format!("failed to create sparse index temp file: {e}"))?;
    drop(std::fs::remove_file(&temp_path));

    file.set_len(total_bytes)
        .map_err(|e| format!("failed to size sparse index file ({total_bytes} bytes): {e}"))?;

    // SAFETY: file is exclusively owned, opened read+write, sized to total_bytes.
    let mut mmap = unsafe {
        memmap2::MmapMut::map_mut(&file)
            .map_err(|e| format!("failed to mmap sparse index values: {e}"))?
    };

    let capacity_bytes = usize::try_from(total_bytes)
        .map_err(|_| "sparse index total_bytes does not fit in usize")?;
    let writer = SharedSparseWriter {
        base: mmap.as_mut_ptr(),
        capacity_bytes,
    };

    let (schedule, shared_input) = crate::scan::classify::build_classify_schedule(
        input,
        Some(crate::blob_meta::ElemKind::Node),
    )?;

    let referenced_ref = &referenced;
    let writer_ref = &writer;
    type Scratch = (Vec<crate::scan::node::NodeTuple>, Vec<(usize, usize)>);
    crate::scan::classify::parallel_scan_blobs_raw(
        &shared_input,
        &schedule,
        None,
        || -> Scratch { (Vec::new(), Vec::new()) },
        |decompressed, (tuples, group_starts)| -> crate::error::Result<()> {
            tuples.clear();
            crate::scan::node::extract_node_tuples(decompressed, tuples, group_starts)?;
            for tup in tuples.iter() {
                if tup.id < 0 {
                    continue;
                }
                if let Some(rank) = referenced_ref.rank_if_set(tup.id) {
                    writer_ref.insert(rank, tup.lat, tup.lon);
                }
            }
            Ok(())
        },
        |_seq, ()| {},
    )?;

    Ok(SparseArrayIndex {
        referenced,
        mmap,
        _file: file,
    })
}
