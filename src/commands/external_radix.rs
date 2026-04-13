//! Shared scaffolding for 256-bucket radix-partitioned external joins.
//!
//! Used by `external_join` (ALTW coordinate resolution).
//!
//! Provides a managed scratch directory (auto-cleanup on drop) and a set of
//! buffered bucket writers with flush-sync-fadvise-close semantics. Payload
//! encoding and the radix-bucket index derivation are left to the caller —
//! different commands use different pair shapes.
//!
//! Moved here from `src/commands/external_join.rs` during the renumber planet
//! refactor (2026-04-11). Behavior identical to the original.

use std::io::{BufWriter, Write as _};
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

    /// Create a scratch dir with a stable name (no PID suffix).
    /// Used by `--keep-scratch` / `--start-stage` so subsequent runs
    /// can find the persisted scratch state.
    pub(crate) fn new_stable(parent: &Path, name: &str) -> Result<Self> {
        let path = parent.join(format!(".pbfhogg-{name}-scratch"));
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
// Bucket writers
// ---------------------------------------------------------------------------

/// Set of buffered writers for radix bucket files. 256 buckets, one file each,
/// buffered at 256 KB.
///
/// Fields are `pub(crate)` so callers can perform direct writes inside hot
/// loops without an extra method-dispatch hop — the original `external_join`
/// code pattern. See the stage-1 loop in `external_join::stage1_way_pass` for
/// the canonical call shape.
pub(crate) struct BucketWriters {
    pub(crate) writers: Vec<Option<BufWriter<std::fs::File>>>,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) entry_counts: Vec<u64>,
}

impl BucketWriters {
    /// Create bucket files eagerly. Each bucket gets a buffered writer.
    pub(crate) fn create(scratch: &ScratchDir, prefix: &str) -> Result<Self> {
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
    pub(crate) fn finish(&mut self) -> Result<Vec<u64>> {
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
    pub(crate) fn cleanup(&self) {
        for path in &self.paths {
            drop(std::fs::remove_file(path));
        }
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
