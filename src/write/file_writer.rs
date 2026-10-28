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
use super::metrics::WriterMetrics;

/// A file writer that is either buffered (normal) or direct I/O (`O_DIRECT`).
///
/// Implements [`Write`] by delegating to the active variant. Its write and
/// sync counters (`writer_buffered_write_*`, `writer_direct_write_*`,
/// `writer_sync_all_*`) count into the invocation it was opened under.
///
/// Public only as the output type of the path constructors'
/// `PbfWriter<FileWriter>`; there is no public constructor. Only the
/// `PbfWriter` that owns it opens one, under the writer's own invocation,
/// so the file's counters and the writer's are always one invocation's. A
/// `FileWriter` a caller could build and hand to
/// [`PbfWriter::new_in`](crate::writer::PbfWriter::new_in) would hold a
/// token of its own and split the run's writer totals across two
/// invocations without any error. A synchronous writer over a file is
/// [`PbfWriter::to_path_sync`](crate::writer::PbfWriter::to_path_sync).
pub struct FileWriter {
    inner: Inner,
    invocation: crate::host::Invocation,
}

enum Inner {
    /// Buffered output; `FileWriter::buffered_in` uses a 256 KiB buffer.
    Buffered(BufWriter<File>),

    /// O_DIRECT writer with page-aligned buffering.
    #[cfg(feature = "linux-direct-io")]
    Direct(DirectWriter),
}

impl FileWriter {
    /// A buffered (256 KiB) file writer under `invocation`, the invocation
    /// of the `PbfWriter` that will own it: its write and sync counters
    /// record there.
    pub(crate) fn buffered_in(
        path: &Path,
        invocation: &crate::host::Invocation,
    ) -> crate::Result<Self> {
        let file = super::create_backend_file(path)?;
        Ok(Self {
            inner: Inner::Buffered(BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file)),
            invocation: invocation.clone(),
        })
    }

    /// An `O_DIRECT` file writer under `invocation` (see
    /// [`FileWriter::buffered_in`]).
    #[cfg(feature = "linux-direct-io")]
    pub(crate) fn direct_in(
        path: &Path,
        invocation: &crate::host::Invocation,
    ) -> crate::Result<Self> {
        Ok(Self {
            inner: Inner::Direct(DirectWriter::create(path)?),
            invocation: invocation.clone(),
        })
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
    /// One exception, buffered variant only (the direct variant never opens
    /// anything but a regular file): a target that does not support
    /// synchronization - a FIFO, a socket, or a character device such as
    /// `/dev/null` - makes `fsync` fail with `EINVAL` after every byte has
    /// already been handed over by `write(2)` and every write error already
    /// returned. That is reported as success, not as a failed write. Regular
    /// files and block devices propagate every
    /// `fsync` error, `EINVAL` included.
    pub(crate) fn sync(&mut self) -> crate::Result<()> {
        self.flush()?;
        let metrics = &self.invocation.metrics().writer;
        let result = match &mut self.inner {
            Inner::Buffered(w) => sync_file(w.get_ref(), metrics),
            #[cfg(feature = "linux-direct-io")]
            Inner::Direct(w) => w.sync_all(metrics),
        };
        Ok(result?)
    }
}

/// `fsync` a buffered output through the writer-metrics funnel, treating
/// "this target does not support synchronization" as success. The file type
/// is only consulted after the `fsync` has already failed, so the
/// regular-file success path is exactly one `fsync`.
pub(super) fn sync_file(file: &File, metrics: &WriterMetrics) -> io::Result<()> {
    let synced = metrics.sync_recorded(file);
    match synced {
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

impl FileWriter {
    /// Flush internal buffers and return the output file for a cursor copy
    /// (`copy_range::CopyOut::Cursor`), if available.
    ///
    /// Returns `None` for O_DIRECT writers (`copy_file_range` is incompatible
    /// with `DirectWriter`'s page-aligned buffering). Returns the file for
    /// buffered writers after flushing the `BufWriter`, so the file position
    /// matches the logical write position; the empty `BufWriter` resumes at
    /// wherever the copy leaves it. The returned borrow keeps this writer
    /// from being written meanwhile.
    pub(crate) fn flush_and_file(&mut self) -> io::Result<Option<&std::fs::File>> {
        match &mut self.inner {
            Inner::Buffered(w) => {
                w.flush()?;
                Ok(Some(w.get_ref()))
            }
            #[cfg(feature = "linux-direct-io")]
            Inner::Direct(_) => Ok(None),
        }
    }
}

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let metrics = &self.invocation.metrics().writer;
        match &mut self.inner {
            Inner::Buffered(w) => {
                let n = w.write(buf)?;
                metrics.buffered_write_calls.fetch_add(1, Relaxed);
                metrics.buffered_write_bytes.fetch_add(n as u64, Relaxed);
                Ok(n)
            }
            #[cfg(feature = "linux-direct-io")]
            Inner::Direct(w) => {
                let n = w.write(buf)?;
                metrics.direct_write_calls.fetch_add(1, Relaxed);
                metrics.direct_write_bytes.fetch_add(n as u64, Relaxed);
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
        match &mut self.inner {
            Inner::Buffered(w) => w.flush(),
            #[cfg(feature = "linux-direct-io")]
            Inner::Direct(w) => w.flush(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::FileWriter;
    use std::io::Write;
    use std::sync::atomic::Ordering::Relaxed;

    /// `Write::flush` must push bytes onward, and must not `fsync`.
    ///
    /// `FileWriter::flush` used to call `sync_all()`, so every incidental
    /// flush - a wrapping `BufWriter`, a generic helper, a library consumer
    /// following `Write` semantics - bought a full fsync. The loop below is
    /// exactly that kind of caller. The fsyncs are counted on this writer's
    /// own invocation, so the counts are exact.
    #[test]
    fn flush_pushes_bytes_onward_without_fsyncing() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("flush-is-not-fsync.bin");
        let token = crate::host::Invocation::new();
        let mut w = FileWriter::buffered_in(&path, &token).unwrap();
        let syncs = || token.metrics().writer.sync_all_calls.load(Relaxed);

        const FLUSHES: usize = 64;
        for i in 0..FLUSHES {
            w.write_all(&[u8::try_from(i % 256).unwrap()]).unwrap();
            w.flush().unwrap();
        }
        assert_eq!(
            syncs(),
            0,
            "Write::flush must not fsync; {FLUSHES} flushes produced {} fsync calls",
            syncs()
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
        assert_eq!(syncs(), 1, "sync() must fsync exactly once");
    }

    /// `/dev/null` cannot be synchronized; the write to it succeeded, so
    /// `sync()` must not report a failure. This is the target `bench-write`
    /// writes to.
    #[cfg(target_os = "linux")]
    #[test]
    fn sync_on_dev_null_succeeds() {
        let mut w = FileWriter::buffered_in(
            std::path::Path::new("/dev/null"),
            &crate::host::Invocation::new(),
        )
        .unwrap();
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
