//! File-backed mmap dense node location index.
//!
//! Direct indexing: `mmap[node_id * 8 .. node_id * 8 + 8]` stores
//! `(lat: i32, lon: i32)` packed as 8 bytes (little-endian). Backed by a
//! temporary file (created and immediately unlinked); the OS manages physical
//! memory via page cache so the index can exceed physical RAM without OOM.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::idset::IdSet;
use crate::ElementReader;

use super::ENTRY_SIZE;
use super::Result;

/// Default dense index capacity: 16 billion entries (128 GB virtual).
/// Covers current OSM max node ID (~12.5B) with headroom for growth.
pub(super) const DENSE_INDEX_DEFAULT_CAPACITY: usize = 16_000_000_000;

// Require 64-bit platform for dense index (32-bit cannot address 128 GB).
const _: () = assert!(std::mem::size_of::<usize>() >= 8);

/// File-backed mmap node location index.
///
/// Sentinel: `(0, 0)` means unset. ~116 nodes at exactly null island (0°N, 0°E)
/// will appear as missing - acceptable ambiguity for diagnostic counters.
pub(crate) struct DenseMmapIndex {
    mmap: memmap2::MmapMut,
    _file: std::fs::File,
    capacity: usize,
}

impl DenseMmapIndex {
    /// Look up a node's coordinates by ID. Returns `None` for unset entries.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub(crate) fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        if node_id < 0 {
            return None;
        }
        let idx = node_id as usize;
        if idx >= self.capacity {
            return None;
        }
        let offset = idx * ENTRY_SIZE;
        // SAFETY: offset + 8 <= capacity * ENTRY_SIZE = mmap length.
        // Pointer is 8-byte aligned (page-aligned base + 8*idx).
        // Atomic load pairs with atomic stores in SharedDenseWriter::insert.
        let packed = unsafe {
            let ptr = self.mmap.as_ptr().add(offset).cast::<AtomicU64>();
            (*ptr).load(Ordering::Relaxed)
        };
        if packed == 0 {
            return None;
        }
        let lat = packed as i32;
        let lon = (packed >> 32) as i32;
        Some((lat, lon))
    }

    pub(crate) fn new(capacity: usize, scratch_dir: &Path) -> Result<Self> {
        let byte_len = capacity
            .checked_mul(ENTRY_SIZE)
            .ok_or("dense index capacity overflow")?;
        let temp_path = scratch_dir.join(format!(
            ".pbfhogg-node-index-{}",
            std::process::id()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|e| {
                format!(
                    "failed to create index temp file at {}: {e}",
                    temp_path.display()
                )
            })?;
        // Unlink immediately - fd keeps the file alive, OS cleans up on close/crash.
        // Ignore errors: unlink failure is non-fatal (file just won't auto-clean).
        drop(std::fs::remove_file(&temp_path));
        file.set_len(byte_len as u64).map_err(|e| {
            format!(
                "failed to set index file size ({} GB): {e}",
                byte_len / 1_000_000_000
            )
        })?;
        // SAFETY: file is exclusively owned, opened read+write, and sized to byte_len.
        let mmap = unsafe {
            memmap2::MmapMut::map_mut(&file).map_err(|e| {
                format!(
                    "failed to mmap index file ({} GB): {e}",
                    byte_len / 1_000_000_000
                )
            })?
        };
        Ok(Self { mmap, _file: file, capacity })
    }
}

/// Thread-safe writer for parallel dense index population.
///
/// Holds a raw pointer into the `DenseMmapIndex` mmap buffer. Each node ID
/// maps to a disjoint 8-byte slot (`base + node_id * 8`). All writes use
/// `AtomicU64::store(Relaxed)`, eliminating data-race UB even if duplicate
/// node IDs appear in the input (e.g. from corrupt or non-canonical PBFs).
///
/// The caller must ensure the `DenseMmapIndex` outlives all uses of this
/// writer. In practice, both live in `build_node_index_dense` and `par_iter`
/// is synchronous (blocks until complete), so the pointer cannot escape.
struct SharedDenseWriter {
    base: *mut u8,
    capacity: usize,
}

// SAFETY: All writes use atomic operations (AtomicU64 stores), eliminating
// data-race UB. The raw pointer requires manual Send+Sync; lifetime is
// bounded by the synchronous par_iter in build_node_index_dense.
unsafe impl Send for SharedDenseWriter {}
unsafe impl Sync for SharedDenseWriter {}

impl SharedDenseWriter {
    /// Insert a node's coordinates. Silently ignores negative IDs and IDs
    /// beyond capacity (same semantics as `DenseMmapIndex::get`).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn insert(&self, node_id: i64, lat: i32, lon: i32) {
        if node_id < 0 {
            return;
        }
        let idx = node_id as usize;
        if idx >= self.capacity {
            return;
        }
        let offset = idx * ENTRY_SIZE;
        let packed = (lat as u32 as u64) | ((lon as u32 as u64) << 32);
        // SAFETY: offset + 8 <= capacity * ENTRY_SIZE = mmap length.
        // Pointer is 8-byte aligned (page-aligned base + 8*idx).
        // Atomic store eliminates data-race UB even with duplicate node IDs.
        unsafe {
            let ptr = self.base.add(offset).cast::<AtomicU64>();
            (*ptr).store(packed, Ordering::Relaxed);
        }
    }
}

/// Build the dense mmap index in parallel. Each rayon task writes directly
/// to disjoint mmap slots via `SharedDenseWriter`.
///
/// Only nodes present in `referenced` are inserted - at planet scale this
/// reduces touched pages from ~80 GB (all 10.4B nodes) to ~16 GB (~2B
/// way-referenced nodes).
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn build_node_index_dense(
    input: &Path,
    direct_io: bool,
    scratch_dir: &Path,
    referenced: &IdSet,
) -> Result<DenseMmapIndex> {
    let mut index = DenseMmapIndex::new(DENSE_INDEX_DEFAULT_CAPACITY, scratch_dir)?;
    let writer = SharedDenseWriter {
        base: index.mmap.as_mut_ptr(),
        capacity: index.capacity,
    };

    // Check for existing LocationsOnWays before consuming the reader.
    {
        let reader = ElementReader::open(input, direct_io)?;
        if reader.header().has_locations_on_ways() {
            eprintln!(
                "Warning: input PBF already declares LocationsOnWays. \
                 Existing way-node coordinates will be overwritten."
            );
        }
    }

    // Node-only sequential scanner: bypasses PrimitiveBlock construction to avoid
    // cross-thread alloc/free retention (25+ GB at Europe/planet scale).
    // See notes/cross-pipeline-optimization-plan.md.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut tuples: Vec<crate::scan::node::NodeTuple> = Vec::new();
    let mut group_starts: Vec<(usize, usize)> = Vec::new();

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        // Skip non-node blobs using indexdata.
        if let Some(idx) = blob.index() {
            if !matches!(idx.kind, crate::blob_meta::ElemKind::Node) {
                continue;
            }
        }

        blob.decompress_into(&mut decompress_buf)?;
        tuples.clear();
        crate::scan::node::extract_node_tuples(&decompress_buf, &mut tuples, &mut group_starts)?;

        // Insert into mmap index. SharedDenseWriter is safe for concurrent access
        // (direct mmap slot writes to disjoint positions).
        tuples.par_iter().for_each(|t| {
            if referenced.get(t.id) {
                writer.insert(t.id, t.lat, t.lon);
            }
        });

    }

    Ok(index)
}
