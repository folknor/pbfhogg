//! The per-dataset download lock and its recorded source identity.
//!
//! `DATA/.KEY.download.lock` is never renamed or unlinked. The tool takes an
//! exclusive `flock` on it before looking at any file of the dataset and holds
//! it until after the registry commit. Every child it starts (curl, pbfhogg)
//! gets the lock file as its stdin, so the lock lives as long as the longest
//! of them: if this process dies with a writer still running, a retry finds
//! the lock held and refuses rather than racing the orphan for its staged
//! file. This rests on those children keeping fd 0 open until they exit,
//! which neither closes. The lock is never released explicitly (`LOCK_UN`
//! would release it for an orphan sharing the open file description too); it
//! goes when the last holder exits.
//!
//! The file's content is the source identity (the registry `origin` value)
//! of the files this tool may leave unregistered at its target names. A retry
//! adopts such a file only when its request has the same identity, which
//! keeps a crash between publishing a download and registering it from
//! letting another source claim the file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
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

    /// The recorded identity, `None` when none has been written. A torn or
    /// unreadable record reads as `Some("")`, which matches no source.
    pub(crate) fn identity(&self) -> Result<Option<String>, String> {
        let mut text = String::new();
        let mut file = &self.file;
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.read_to_string(&mut text))
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        if text.is_empty() {
            return Ok(None);
        }
        Ok(Some(text.strip_suffix('\n').unwrap_or("").to_owned()))
    }

    /// Record `identity` durably. Only called when no file this tool could
    /// adopt exists, so a crash mid-write strands nothing.
    pub(crate) fn set_identity(&self, identity: &str) -> Result<(), String> {
        let at = |e: std::io::Error| format!("{}: {e}", self.path.display());
        let mut file = &self.file;
        self.file.set_len(0).map_err(at)?;
        file.seek(SeekFrom::Start(0)).map_err(at)?;
        file.write_all(format!("{identity}\n").as_bytes())
            .map_err(at)?;
        self.file.sync_all().map_err(at)?;
        crate::layout::sync_parent(&self.path)
    }

    /// A descriptor for a child's stdin, sharing the lock.
    pub(crate) fn for_child(&self) -> Result<File, String> {
        self.file
            .try_clone()
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }
}
