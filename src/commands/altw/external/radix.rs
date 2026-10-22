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
use std::path::{Path, PathBuf};

use super::Result;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of buckets for radix partitioning. 256 = partition by high byte.
pub(crate) const NUM_BUCKETS: usize = 256;

/// Size of the write buffer per ALTW bucket file (256 KB).
///
/// This is an ALTW-specific batching tunable, separate from
/// `crate::write::IO_BUF_BYTES`. Bucket files have many independent buffered
/// writers, so this capacity also determines ALTW's aggregate buffering.
pub(crate) const BUCKET_BUF_SIZE: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Scratch directory management
// ---------------------------------------------------------------------------

/// Managed scratch directory for bucket files. Cleaned up on drop.
///
/// The directory name is `<parent>/.pbfhogg-<name>-<pid><suffix>`, the suffix
/// coming from the run's [`RunToken`](crate::scratch::RunToken). `name`
/// identifies the command that owns the scratch dir so concurrent runs of
/// different commands don't collide, and so stale directories left behind by
/// a crash are attributable; the pid and token separate concurrent runs of the
/// same command across and within processes.
///
/// `parent` must exist: scratch goes into a directory, it does not create one.
/// A directory already at the path (a crashed run whose pid was reused) is
/// reused, as before.
pub(crate) struct ScratchDir {
    pub(crate) path: PathBuf,
}

impl ScratchDir {
    pub(crate) fn new(parent: &Path, name: &str, token: &crate::scratch::RunToken) -> Result<Self> {
        let path = parent.join(format!(
            ".pbfhogg-{name}-{}{}",
            std::process::id(),
            token.suffix()
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(e) => {
                return Err(
                    format!("failed to create scratch directory {}: {e}", path.display()).into(),
                );
            }
        }
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
