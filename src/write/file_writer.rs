//! Enum wrapper for file-backed writers, supporting both buffered and
//! O_DIRECT paths through a single concrete type.
//!
//! When the `linux-direct-io` feature is not enabled, this is a single-variant
//! enum and the compiler optimizes away the match.

use std::fs::File;
use std::io::{self, BufWriter, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;

#[cfg(feature = "linux-direct-io")]
use super::direct_writer::DirectWriter;
use super::metrics::WRITER_METRICS;
use super::{fallocate_hint_bytes, should_sync_all};

fn elapsed_ns_u64(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// A file writer that is either buffered (normal) or direct I/O (`O_DIRECT`).
///
/// Implements [`Write`] by delegating to the active variant.
// Not #[non_exhaustive] — variants are construction-controlled (users don't match on this).
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
        maybe_fallocate(&file)?;
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
}

#[cfg(target_os = "linux")]
fn maybe_fallocate(file: &File) -> io::Result<()> {
    let Some(len) = fallocate_hint_bytes() else {
        return Ok(());
    };
    let len_off_t: libc::off_t = len
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "fallocate hint too large"))?;
    let t = std::time::Instant::now();
    // Safety: valid fd from an owned File; KEEP_SIZE reserves extents without
    // changing the visible file length if the final output is smaller.
    let rc = unsafe {
        libc::fallocate(file.as_raw_fd(), libc::FALLOC_FL_KEEP_SIZE, 0, len_off_t)
    };
    WRITER_METRICS
        .fallocate_ns
        .fetch_add(elapsed_ns_u64(t), Relaxed);
    if rc == 0 {
        WRITER_METRICS.fallocate_bytes.fetch_add(len, Relaxed);
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn maybe_fallocate(_file: &File) -> io::Result<()> {
    let _ = fallocate_hint_bytes();
    Ok(())
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

    fn flush(&mut self) -> io::Result<()> {
        match self {
            FileWriter::Buffered(w) => {
                w.flush()?;
                if should_sync_all() {
                    let t_sync = std::time::Instant::now();
                    let result = w.get_ref().sync_all();
                    WRITER_METRICS
                        .sync_all_ns
                        .fetch_add(elapsed_ns_u64(t_sync), Relaxed);
                    result
                } else {
                    Ok(())
                }
            }
            #[cfg(feature = "linux-direct-io")]
            FileWriter::Direct(w) => {
                w.flush()?;
                if should_sync_all() {
                    let t_sync = std::time::Instant::now();
                    let result = w.sync_all();
                    WRITER_METRICS
                        .sync_all_ns
                        .fetch_add(elapsed_ns_u64(t_sync), Relaxed);
                    result
                } else {
                    Ok(())
                }
            }
        }
    }
}
