//! Shared scaffolding for 256-bucket radix-partitioned external joins.
//!
//! Used by the external ALTW backend (coordinate resolution).
//!
//! Provides a managed scratch directory (auto-cleanup on drop) and the
//! shared bucket-count + per-bucket buffer-size constants. Callers do their
//! own bucket-writer bookkeeping - different commands want different
//! ownership shapes (per-worker shards in stage 1, shared per-bucket
//! mutexes in stage 2).
//!
use crate::scratch::{RunScratch, ScratchPath};

use super::Result;

// ---------------------------------------------------------------------------
// Scratch file names
// ---------------------------------------------------------------------------
//
// The external join's scratch lives in the run's `RunScratch` directory. These
// are the only places its file names are spelled, so stage 1 (create), stage 2
// (open) and the reclaiming code cannot drift apart.

/// Worker `worker_id`'s id-shard file for id bucket `bucket_idx`.
pub(crate) fn id_shard_path(
    scratch: &RunScratch,
    worker_id: usize,
    bucket_idx: usize,
) -> Result<ScratchPath> {
    scratch.file(&format!("id-W{worker_id}-{bucket_idx:03}"))
}

/// Slot bucket `index`.
pub(crate) fn slot_bucket_path(scratch: &RunScratch, index: usize) -> Result<ScratchPath> {
    scratch.file(&format!("slot-{index:03}"))
}

/// Stage 3 worker `index`'s payload file.
pub(crate) fn payload_path(scratch: &RunScratch, index: usize) -> Result<ScratchPath> {
    scratch.file(&format!("payloads-W{index}"))
}

// ---------------------------------------------------------------------------
// File-level page-cache eviction helper
// ---------------------------------------------------------------------------

/// Advise the kernel to evict a single file's pages from page cache.
#[cfg(feature = "linux-direct-io")]
pub(crate) fn advise_dontneed_file(file: &std::fs::File) {
    use std::os::unix::io::AsRawFd;
    unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
}
