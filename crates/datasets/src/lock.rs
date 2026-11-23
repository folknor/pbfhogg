//! The per-dataset download lock.
//!
//! `DATA/.KEY.download.lock` is never renamed or unlinked, and its content
//! is never written. The tool takes an exclusive `flock` on it before
//! looking at any file of the dataset and holds it until after the registry
//! commit. Every child it starts (curl, pbfhogg) gets the lock file as its
//! stdin, so the lock lives as long as the longest of them: if this process
//! dies with a writer still running, a retry finds the lock held and refuses
//! rather than racing the orphan for its staged file. This rests on those
//! children keeping fd 0 open until they exit, which neither closes. The
//! lock is never released explicitly (`LOCK_UN` would release it for an
//! orphan sharing the open file description too); it goes when the last
//! holder exits.
//!
//! What this tool published and may adopt is recorded per file in the
//! dataset's recovery journal (`journal`), not here.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub(crate) struct DatasetLock {
    file: File,
    path: PathBuf,
}

impl DatasetLock {
    pub(crate) fn acquire(data_dir: &Path, key: &str) -> Result<Self, String> {
        let path = data_dir.join(format!(".{key}.download.lock"));
        let at = |e: std::io::Error| format!("{}: {e}", path.display());
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(at)?;
        // SAFETY: flock on a live descriptor this function owns, scalar flags.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(format!(
                    "{} is held: an earlier download of '{key}' or one of its writers \
                     (curl, pbfhogg) is still running",
                    path.display()
                ));
            }
            return Err(at(e));
        }
        Ok(Self { file, path })
    }

    /// A descriptor for a child's stdin, sharing the lock.
    pub(crate) fn for_child(&self) -> Result<File, String> {
        self.file
            .try_clone()
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }
}
