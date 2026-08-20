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
// Not #[non_exhaustive] - variants are construction-controlled (users don't match on this).
pub enum FileWriter {
    /// Standard 256 KB `BufWriter<File>`.
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
            256 * 1024,
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
    pub fn sync(&mut self) -> io::Result<()> {
        self.flush()?;
        let t_sync = std::time::Instant::now();
        let result = match self {
            FileWriter::Buffered(w) => w.get_ref().sync_all(),
            #[cfg(feature = "linux-direct-io")]
            FileWriter::Direct(w) => w.sync_all(),
        };
        WRITER_METRICS.record_sync_all(t_sync);
        result
    }
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
        let dir = tempfile::TempDir::new().unwrap();
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
}
