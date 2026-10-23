//! Unified file reader that selects between buffered and O_DIRECT I/O.
//!
//! [`FileReader`] wraps either a standard `BufReader<File>` or (when the
//! `linux-direct-io` feature is enabled) a [`DirectReader`] that bypasses the
//! kernel page cache. All read-path code uses `FileReader` as the concrete
//! reader type, so the O_DIRECT path is zero-cost when the feature is off
//! (single-variant enum optimizes away).

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

#[cfg(feature = "linux-direct-io")]
use super::direct_reader::DirectReader;

/// A file reader that selects between buffered and O_DIRECT I/O at runtime.
// Not #[non_exhaustive] - variants are construction-controlled (users don't match on this).
pub enum FileReader {
    /// Standard buffered reader (256 KB buffer).
    Buffered(BufReader<File>),
    /// O_DIRECT reader with page-aligned buffering (Linux only).
    #[cfg(feature = "linux-direct-io")]
    Direct(DirectReader),
}

/// Name the file an open failed on, preserving the original
/// [`io::ErrorKind`] so callers matching on `NotFound` still match.
///
/// A bare `No such file or directory (os error 2)` is what a user saw for
/// a mistyped input path: true, and useless on a command with several
/// inputs. The path is known here and nowhere below here.
pub(crate) fn open_error(path: &Path, err: &io::Error) -> io::Error {
    io::Error::new(
        err.kind(),
        format!("failed to open {}: {err}", path.display()),
    )
}

impl FileReader {
    /// Advise away a consumed prefix using the currently owned file.
    /// Returns whether an advisory syscall was issued; O_DIRECT has no cache.
    /// The fd is borrowed only for this call, never cached by the caller.
    #[cfg(target_os = "linux")]
    pub(crate) fn evict_consumed_prefix(&self, offset: u64) -> bool {
        use std::os::unix::io::AsRawFd;
        match self {
            Self::Buffered(reader) => {
                // SAFETY: the borrow keeps the owning File alive for the call.
                unsafe {
                    libc::posix_fadvise(
                        reader.get_ref().as_raw_fd(),
                        0,
                        offset.try_into().unwrap_or(i64::MAX),
                        libc::POSIX_FADV_DONTNEED,
                    )
                };
                true
            }
            #[cfg(feature = "linux-direct-io")]
            Self::Direct(_) => false,
        }
    }

    /// Open a file for buffered reading (256 KB `BufReader`).
    ///
    /// On Linux, advises the kernel for sequential readahead via
    /// `posix_fadvise(POSIX_FADV_SEQUENTIAL)`.
    pub fn buffered(path: &Path) -> io::Result<Self> {
        let f = File::open(path).map_err(|e| open_error(path, &e))?;
        #[cfg(all(
            target_os = "linux",
            any(feature = "linux-direct-io", feature = "linux-io-uring")
        ))]
        {
            use std::os::unix::io::AsRawFd;
            // Advisory hint for sequential readahead - matches osmium's approach.
            // SAFETY: valid fd, advisory-only, no-op on failure.
            unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
        }
        Ok(Self::Buffered(BufReader::with_capacity(
            crate::write::IO_BUF_BYTES,
            f,
        )))
    }

    /// Open a file for O_DIRECT reading with page-aligned buffers.
    #[cfg(feature = "linux-direct-io")]
    pub fn direct(path: &Path) -> io::Result<Self> {
        // `open_error` rebuilds the error from its message, so name the path
        // on the carried source and keep a platform refusal typed.
        Ok(Self::Direct(DirectReader::open(path).map_err(|e| {
            crate::error::map_io_preserving_platform(e, |e| open_error(path, &e))
        })?))
    }

    /// Open a file, selecting buffered or O_DIRECT based on the `direct` flag.
    ///
    /// Returns an error if `direct` is true but the `linux-direct-io` feature
    /// is not enabled.
    pub fn open(path: &Path, direct: bool) -> io::Result<Self> {
        if direct {
            #[cfg(feature = "linux-direct-io")]
            {
                return Self::direct(path);
            }
            #[cfg(not(feature = "linux-direct-io"))]
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "--direct-io requires the linux-direct-io feature",
                ));
            }
        }
        Self::buffered(path)
    }
}

impl FileReader {
    /// Skip `n` bytes without materializing data into a destination buffer.
    ///
    /// For `BufReader`: skips `n-1` bytes via `seek_relative` (advances
    /// within internal buffer when possible, otherwise seeks the underlying
    /// fd) and then reads exactly one byte to validate the file actually
    /// contains `n` bytes from the current position. Without the post-skip
    /// read, `BufReader::seek_relative` can succeed past EOF on file-backed
    /// readers - the truncation would only surface at the next caller's
    /// read, leaving callers like `has_indexdata` that don't immediately
    /// read again with a silent shape-4 truncation hole. Per
    /// [`reference/truncation-handling.md`](../../reference/truncation-handling.md),
    /// a payload that doesn't deliver the declared `data_size` must
    /// hard-error here, not be deferred.
    ///
    /// For `DirectReader`: consumes buffered bytes, then lseeks the fd.
    /// `DirectReader::skip` already validates EOF and errors on past-end,
    /// so no extra check is needed.
    pub(crate) fn skip(&mut self, n: u64) -> io::Result<()> {
        if n == 0 {
            return Ok(());
        }
        match self {
            Self::Buffered(r) => {
                #[allow(clippy::cast_possible_wrap)]
                let signed = (n - 1) as i64;
                r.seek_relative(signed)?;
                let mut sentinel = [0u8; 1];
                r.read_exact(&mut sentinel)
            }
            #[cfg(feature = "linux-direct-io")]
            Self::Direct(r) => r.skip(n),
        }
    }
}

#[cfg(feature = "linux-direct-io")]
impl FileReader {
    /// Return the raw file descriptor for `copy_file_range`.
    ///
    /// The fd remains valid as long as the `FileReader` is alive. Used with
    /// explicit offsets, so it does not interfere with buffered/direct read
    /// position tracking.
    pub fn raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        match self {
            Self::Buffered(r) => r.get_ref().as_raw_fd(),
            Self::Direct(r) => r.raw_fd(),
        }
    }
}

impl Read for FileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Buffered(r) => r.read(buf),
            #[cfg(feature = "linux-direct-io")]
            Self::Direct(r) => r.read(buf),
        }
    }
}
