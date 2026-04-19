//! Shared scaffolding for 256-bucket radix-partitioned external joins.
//!
//! Used by `external_join` (ALTW coordinate resolution).
//!
//! Provides a managed scratch directory (auto-cleanup on drop) and the
//! shared bucket-count + per-bucket buffer-size constants. Callers do their
//! own bucket-writer bookkeeping - different commands want different
//! ownership shapes (per-worker shards in stage 1, shared per-bucket
//! mutexes in stage 2).
//!
//! Moved here from `src/commands/external_join.rs` during the renumber planet
//! refactor (2026-04-11).

use std::path::{Path, PathBuf};

use super::Result;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of buckets for radix partitioning. 256 = partition by high byte.
pub(crate) const NUM_BUCKETS: usize = 256;

/// Size of the write buffer per bucket file (256 KB).
pub(crate) const BUCKET_BUF_SIZE: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Scratch directory management
// ---------------------------------------------------------------------------

/// Managed scratch directory for bucket files. Cleaned up on drop.
///
/// The directory name is `<parent>/.pbfhogg-<name>-<pid>`. `name` identifies
/// the command that owns the scratch dir so concurrent runs of different
/// commands don't collide, and so stale directories left behind by a crash
/// are attributable.
pub(crate) struct ScratchDir {
    pub(crate) path: PathBuf,
}

impl ScratchDir {
    pub(crate) fn new(parent: &Path, name: &str) -> Result<Self> {
        let path = parent.join(format!(".pbfhogg-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&path).map_err(|e| {
            format!("failed to create scratch directory {}: {e}", path.display())
        })?;
        Ok(Self { path })
    }

    pub(crate) fn bucket_path(&self, prefix: &str, index: usize) -> PathBuf {
        self.path.join(format!("{prefix}-{index:03}"))
    }

    pub(crate) fn file_path(&self, name: &str) -> PathBuf {
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
// File-level page-cache eviction helper
// ---------------------------------------------------------------------------

/// Advise the kernel to evict a single file's pages from page cache.
#[cfg(feature = "linux-direct-io")]
pub(crate) fn advise_dontneed_file(file: &std::fs::File) {
    use std::os::unix::io::AsRawFd;
    unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
}
