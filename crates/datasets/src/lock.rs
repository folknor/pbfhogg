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
//! The lock name takes the same protection as every other entry the tool
//! changes: it is refused when a registration (or the input being promoted)
//! resolves through it, since creating it would fill that name. The open
//! never follows a symlink and never waits (a FIFO, or a lease held on the
//! file, refuses), and only a regular file with a single link is a lock: a
//! hard link would couple this key's exclusion to whatever file shares the
//! inode. A refused entry is left in place; an orphan of an older tool may
//! still hold the inode it reached. Every invocation that shares the
//! dataset's staging entries must see the same lock inode for the key, and
//! distinct keys distinct inodes; a mount layered over the lock name alone
//! breaks that, and is not detected.
//!
//! What this tool published and may adopt is recorded per file in the
//! dataset's recovery journal (`journal`), not here.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use pbfhogg_devkit::Config;

use crate::layout;

pub(crate) struct DatasetLock {
    file: File,
    path: PathBuf,
}

impl DatasetLock {
    /// Take the lock of dataset `key` in `data_dir`. `protect` names paths
    /// besides the host's registrations whose resolution must not pass
    /// through the lock name (promotion's input).
    pub(crate) fn acquire(
        config: &Config,
        host: &str,
        data_dir: &Path,
        key: &str,
        protect: &[&Path],
    ) -> Result<Self, String> {
        let path = data_dir.join(format!(".{key}.download.lock"));
        layout::reserve_lock(config, host, data_dir, &path, protect)?;
        let at = |e: std::io::Error| format!("{}: {e}", path.display());
        // O_NONBLOCK stays set on the descriptor the children inherit;
        // regular-file I/O ignores it.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| match e.raw_os_error() {
                Some(libc::ELOOP) => format!(
                    "{} is a symlink, not a lock file; remove it once no download \
                     of '{key}' is running, and retry",
                    path.display()
                ),
                Some(libc::EWOULDBLOCK) => format!(
                    "{}: open would block (a lease on the file?): {e}",
                    path.display()
                ),
                _ => at(e),
            })?;
        let meta = file.metadata().map_err(at)?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err(format!(
                "{} is not a regular file with a single link, so it cannot be this \
                 dataset's lock; remove it once no download of '{key}' is running, \
                 and retry",
                path.display()
            ));
        }
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
