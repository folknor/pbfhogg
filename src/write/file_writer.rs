//! Enum wrapper for file-backed writers, supporting both buffered and
//! O_DIRECT paths through a single concrete type.
//!
//! When the `linux-direct-io` feature is not enabled, this is a single-variant
//! enum and the compiler optimizes away the match.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;

#[cfg(feature = "linux-direct-io")]
use super::direct_writer::DirectWriter;
use super::metrics::WRITER_METRICS;

/// A file writer that is either buffered (normal) or direct I/O (`O_DIRECT`).
///
/// Implements [`Write`] by delegating to the active variant.
// Making these variants private would break callers that construct or match
// them. Keep that API: generic `PbfWriter::new` accepts caller-owned writers,
// while `PbfWriter::to_path_sync` is the separate path-owned API with a fixed
// buffer and explicit sync-on-flush behavior.
pub enum FileWriter {
    /// Buffered output; `FileWriter::buffered` uses a 256 KiB buffer.
    Buffered(BufWriter<File>),

    /// O_DIRECT writer with page-aligned buffering.
    #[cfg(feature = "linux-direct-io")]
    Direct(DirectWriter),
}

impl FileWriter {
    /// Create a buffered file writer (default path).
    pub fn buffered(path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        Ok(FileWriter::Buffered(BufWriter::with_capacity(
            crate::write::IO_BUF_BYTES,
            file,
        )))
    }

    /// Create an `O_DIRECT` file writer.
    #[cfg(feature = "linux-direct-io")]
    pub fn direct(path: &Path) -> io::Result<Self> {
        Ok(FileWriter::Direct(DirectWriter::create(path)?))
    }

    /// Flush buffered bytes and then `fsync` the file: finish *and* make
    /// durable.
    ///
    /// This is the operation the pipeline's end-of-stream finalization wants,
    /// and it used to be what [`Write::flush`] did. That was wrong: `flush` is
    /// a `Write`-trait method with published semantics of "push buffered bytes
    /// onward", and any incidental call - a wrapping `BufWriter`, a generic
    /// helper, a library consumer doing the ordinary thing - silently bought a
    /// full `fsync`. On a planet-scale write that is not a small surprise.
    ///
    /// Durability is now something a caller asks for by name. `flush` flushes.
    ///
    /// One exception, buffered variant only: a target that does not support
    /// synchronization - a FIFO, a socket, or a character device such as
    /// `/dev/null` - makes `fsync` fail with `EINVAL` after every byte has
    /// already been handed over by `write(2)` and every write error already
    /// returned. That is reported as success, not as a failed write. Regular
    /// files and block devices propagate every
    /// `fsync` error, `EINVAL` included.
    pub fn sync(&mut self) -> io::Result<()> {
        self.flush()?;
        let t_sync = std::time::Instant::now();
        let result = match self {
            FileWriter::Buffered(w) => sync_file(w.get_ref()),
            #[cfg(feature = "linux-direct-io")]
            FileWriter::Direct(w) => w.sync_all(),
        };
        WRITER_METRICS.record_sync_all(t_sync);
        result
    }
}

/// `fsync` a buffered output, treating "this target does not support
/// synchronization" as success. Only consulted after `sync_all` has already
/// failed, so the regular-file success path is exactly one `fsync`.
pub(super) fn sync_file(file: &File) -> io::Result<()> {
    match file.sync_all() {
        Err(e) if sync_unsupported(e.raw_os_error(), || file_type(file)) => Ok(()),
        other => other,
    }
}

/// The file's type, or `None` if `fstat` itself failed.
fn file_type(file: &File) -> Option<std::fs::FileType> {
    file.metadata().ok().map(|m| m.file_type())
}

/// Whether a failed `fsync` means the target cannot be synchronized at all,
/// rather than that synchronization failed.
///
/// Linux returns `EINVAL` when the descriptor "is bound to a special file
/// (e.g., a pipe, FIFO, or socket) which does not support synchronization".
/// The exception is deliberately narrow:
///
/// * only `EINVAL` - `EIO`, `ENOSPC` and every other errno is a real failure;
/// * only FIFOs, sockets and character devices - a block device implements
///   `fsync`, so an `EINVAL` from one is not "unsupported", and a regular file
///   never gets the benefit of the doubt;
/// * if the file type cannot be read, the original error stands;
/// * Linux only, the platform whose semantics this is verified against.
///
/// Before this, `PbfWriter::to_path` onto `/dev/null`, a FIFO or a pipe wrote
/// every byte successfully and then failed `flush()` with `EINVAL`, and
/// `bench-write` hid that by dropping its writer instead of flushing it.
///
/// `file_type` is only called once the errno is `EINVAL`, so every other
/// failure propagates without an extra `fstat`; it returns `None` when `fstat`
/// itself failed.
#[cfg(target_os = "linux")]
fn sync_unsupported(
    errno: Option<i32>,
    file_type: impl FnOnce() -> Option<std::fs::FileType>,
) -> bool {
    use std::os::unix::fs::FileTypeExt;
    errno == Some(libc::EINVAL)
        && file_type().is_some_and(|t| t.is_fifo() || t.is_socket() || t.is_char_device())
}

#[cfg(not(target_os = "linux"))]
fn sync_unsupported(
    _errno: Option<i32>,
    _file_type: impl FnOnce() -> Option<std::fs::FileType>,
) -> bool {
    false
}

#[cfg(feature = "linux-direct-io")]
impl FileWriter {
    /// Flush internal buffers and return the raw output fd, if available.
    ///
    /// Returns `None` for O_DIRECT writers (`copy_file_range` is incompatible
    /// with `DirectWriter`'s page-aligned buffering). Returns `Some(fd)` for
    /// buffered writers after flushing the `BufWriter` so the fd position
    /// matches the logical write position.
    pub(crate) fn flush_and_raw_fd(&mut self) -> io::Result<Option<std::os::unix::io::RawFd>> {
        use std::os::unix::io::AsRawFd;
        match self {
            FileWriter::Buffered(w) => {
                w.flush()?;
                Ok(Some(w.get_ref().as_raw_fd()))
            }
            FileWriter::Direct(_) => Ok(None),
        }
    }
}

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            FileWriter::Buffered(w) => {
                let n = w.write(buf)?;
                WRITER_METRICS.buffered_write_calls.fetch_add(1, Relaxed);
                WRITER_METRICS
                    .buffered_write_bytes
                    .fetch_add(n as u64, Relaxed);
                Ok(n)
            }
            #[cfg(feature = "linux-direct-io")]
            FileWriter::Direct(w) => {
                let n = w.write(buf)?;
                WRITER_METRICS.direct_write_calls.fetch_add(1, Relaxed);
                WRITER_METRICS
                    .direct_write_bytes
                    .fetch_add(n as u64, Relaxed);
                Ok(n)
            }
        }
    }

    /// Push buffered bytes onward - and nothing more.
    ///
    /// Explicitly *not* an `fsync`; see [`FileWriter::sync`] for that. Note
    /// that the `Direct` variant's own `flush` still finalizes its page-
    /// aligned tail and `set_len`s the file, because for an `O_DIRECT` writer
    /// that padding removal is part of pushing the bytes onward, not part of
    /// durability.
    fn flush(&mut self) -> io::Result<()> {
        match self {
            FileWriter::Buffered(w) => w.flush(),
            #[cfg(feature = "linux-direct-io")]
            FileWriter::Direct(w) => w.flush(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{FileWriter, WRITER_METRICS};
    use std::io::Write;
    use std::sync::atomic::Ordering::Relaxed;

    /// `Write::flush` must push bytes onward, and must not `fsync`.
    ///
    /// `FileWriter::flush` used to call `sync_all()`, so every incidental
    /// flush - a wrapping `BufWriter`, a generic helper, a library consumer
    /// following `Write` semantics - bought a full fsync. The loop below is
    /// exactly that kind of caller. The bound is deliberately loose enough to
    /// tolerate an unrelated `sync()` from a concurrent test and still bite:
    /// if `flush` synced, the delta would be at least one per iteration.
    #[test]
    fn flush_pushes_bytes_onward_without_fsyncing() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("flush-is-not-fsync.bin");
        let mut w = FileWriter::buffered(&path).unwrap();

        const FLUSHES: usize = 64;
        let before = WRITER_METRICS.sync_all_calls.load(Relaxed);
        for i in 0..FLUSHES {
            w.write_all(&[u8::try_from(i % 256).unwrap()]).unwrap();
            w.flush().unwrap();
        }
        let after_flushes = WRITER_METRICS.sync_all_calls.load(Relaxed);
        assert!(
            after_flushes - before < FLUSHES as u64,
            "Write::flush must not fsync; {FLUSHES} flushes produced \
             {} fsync calls",
            after_flushes - before
        );

        // ...and flush must actually have pushed the bytes out, or "does not
        // fsync" would be trivially satisfied by doing nothing at all.
        assert_eq!(
            std::fs::read(&path).unwrap().len(),
            FLUSHES,
            "flush must make buffered bytes visible in the file"
        );

        // The durability that `flush` gave up is available by name.
        w.sync().unwrap();
        assert!(
            WRITER_METRICS.sync_all_calls.load(Relaxed) > after_flushes,
            "sync() must fsync"
        );
    }

    /// `/dev/null` cannot be synchronized; the write to it succeeded, so
    /// `sync()` must not report a failure. This is the target `bench-write`
    /// writes to.
    #[cfg(target_os = "linux")]
    #[test]
    fn sync_on_dev_null_succeeds() {
        let mut w = FileWriter::buffered(std::path::Path::new("/dev/null")).unwrap();
        w.write_all(b"bytes").unwrap();
        w.sync().unwrap();
    }

    #[cfg(target_os = "linux")]
    mod unsupported {
        use super::super::sync_unsupported;
        use std::fs::File;
        use std::os::fd::OwnedFd;

        fn file_type_of(file: &File) -> Option<std::fs::FileType> {
            Some(file.metadata().unwrap().file_type())
        }

        fn char_device() -> File {
            File::create("/dev/null").unwrap()
        }

        fn fifo() -> File {
            let (_reader, writer) = std::io::pipe().unwrap();
            File::from(OwnedFd::from(writer))
        }

        fn socket() -> File {
            let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
            File::from(OwnedFd::from(a))
        }

        #[test]
        fn einval_on_targets_without_fsync_is_unsupported() {
            for (name, file) in [
                ("char device", char_device()),
                ("fifo", fifo()),
                ("socket", socket()),
            ] {
                assert!(
                    sync_unsupported(Some(libc::EINVAL), || file_type_of(&file)),
                    "{name}: EINVAL means fsync is unsupported here"
                );
            }
        }

        /// Any other errno is a real failure, and is decided without an
        /// `fstat`: the file type is never asked for.
        #[test]
        fn other_errnos_stay_errors_without_an_fstat() {
            for errno in [Some(libc::EIO), Some(libc::ENOSPC), Some(libc::EROFS), None] {
                assert!(
                    !sync_unsupported(errno, || panic!("fstat for errno {errno:?}")),
                    "errno {errno:?} is a real failure"
                );
            }
        }

        #[test]
        fn einval_on_a_regular_file_stays_an_error() {
            // Only the file type is consulted; any regular file will do.
            let file = File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
            assert!(!sync_unsupported(Some(libc::EINVAL), || file_type_of(
                &file
            )));
        }

        #[test]
        fn unreadable_file_type_keeps_the_original_error() {
            assert!(!sync_unsupported(Some(libc::EINVAL), || None));
        }
    }
}
