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

use crate::error::{Error, Result};
use crate::read::input::InputSource;

#[cfg(feature = "linux-direct-io")]
use super::direct_reader::DirectReader;

/// A file reader that selects between buffered and O_DIRECT I/O at runtime.
// Not #[non_exhaustive] - variants are construction-controlled (users don't match on this).
pub enum FileReader {
    /// Standard buffered reader (256 KB buffer).
    Buffered(BufReader<File>),
    /// O_DIRECT reader with aligned buffering (Linux only).
    #[cfg(feature = "linux-direct-io")]
    Direct(DirectReader),
}

/// Name the file an open failed on, keeping the original error (and so its
/// [`io::ErrorKind`] and OS error) as the cause, so callers matching on
/// `NotFound` still match.
///
/// A bare `No such file or directory (os error 2)` is what a user saw for
/// a mistyped input path: true, and useless on a command with several
/// inputs. The path is known here and nowhere below here. The same text as
/// [`crate::read::input::open`], which the buffered open goes through.
#[cfg(feature = "linux-direct-io")]
fn open_error(path: &Path, err: impl Into<Error>) -> Error {
    err.into()
        .context(format!("failed to open {}", path.display()))
}

/// The refusal of an `O_DIRECT` read in a build without `linux-direct-io`,
/// shared by every read path that can be asked for one.
#[cfg(not(feature = "linux-direct-io"))]
pub(crate) fn direct_reads_unsupported() -> Error {
    Error::invalid_argument(
        "O_DIRECT reads need the linux-direct-io feature, which this build \
         was compiled without",
    )
}

impl FileReader {
    /// Advise away a consumed prefix using the currently owned file.
    /// Returns whether an advisory syscall was issued; O_DIRECT has no cache.
    /// The fd is borrowed only for this call, never cached by the caller. A
    /// failed advice counts into `invocation`.
    #[cfg(target_os = "linux")]
    pub(crate) fn evict_consumed_prefix(
        &self,
        offset: u64,
        invocation: &crate::host::Invocation,
    ) -> bool {
        match self {
            // An empty prefix is not advised: length zero would mean the
            // whole file.
            Self::Buffered(reader) => {
                offset != 0
                    && crate::fadvise::advise(
                        reader.get_ref(),
                        0,
                        offset,
                        crate::fadvise::Advice::DontNeed,
                        invocation,
                    )
            }
            #[cfg(feature = "linux-direct-io")]
            Self::Direct(_) => false,
        }
    }

    /// Open a file for buffered reading (256 KB `BufReader`) under
    /// `invocation`.
    ///
    /// On Linux, advises the kernel for sequential readahead via
    /// `posix_fadvise(POSIX_FADV_SEQUENTIAL)`, as osmium does, in every
    /// build: the hint is not tied to the `O_DIRECT` or `io_uring` features.
    /// A failed advice counts into `invocation`'s `fadvise_failures`: pass
    /// the invocation the reader built over this one will run under
    /// ([`BlobReader::invocation`](crate::BlobReader::invocation)), or the
    /// count lands where nobody reads it. The reader holds no token itself.
    pub fn buffered_in(path: &Path, invocation: &crate::host::Invocation) -> Result<Self> {
        Self::buffered_from(InputSource::Path(path), invocation)
    }

    fn buffered_from(
        source: InputSource<'_>,
        invocation: &crate::host::Invocation,
    ) -> Result<Self> {
        let f = source.open()?;
        crate::fadvise::advise(&f, 0, 0, crate::fadvise::Advice::Sequential, invocation);
        Ok(Self::Buffered(BufReader::with_capacity(
            crate::write::IO_BUF_BYTES,
            f,
        )))
    }

    /// Open a file for O_DIRECT reading with buffers aligned to the file's
    /// direct I/O alignment.
    #[cfg(feature = "linux-direct-io")]
    pub fn direct(path: &Path) -> Result<Self> {
        Self::direct_from(InputSource::Path(path))
    }

    #[cfg(feature = "linux-direct-io")]
    fn direct_from(source: InputSource<'_>) -> Result<Self> {
        let path = source.path();
        // A platform refusal already names the path inside its source, so
        // it stays the outermost text, and a replaced pinned input is its
        // own refusal; any other failure gets the context.
        let opened = source
            .open_direct()
            .and_then(|(file, align)| DirectReader::from_opened(file, align, path));
        Ok(Self::Direct(opened.map_err(|e| {
            if matches!(
                e.kind(),
                crate::ErrorKind::PlatformUnsupported { .. } | crate::ErrorKind::InputReplaced
            ) {
                e
            } else {
                open_error(path, e)
            }
        })?))
    }

    /// Open an input under `invocation`, selecting buffered or O_DIRECT based
    /// on the `direct` flag. A path (`&Path`, `&PathBuf`) is opened as
    /// given; a [`PinnedInput`](crate::read::input::PinnedInput) is opened as
    /// the file it pinned. A buffered open's failed readahead advice counts
    /// into `invocation` (see [`FileReader::buffered_in`]).
    ///
    /// Returns an error if `direct` is true but the `linux-direct-io` feature
    /// is not enabled.
    pub fn open_in<'a>(
        source: impl Into<InputSource<'a>>,
        direct: bool,
        invocation: &crate::host::Invocation,
    ) -> Result<Self> {
        let source = source.into();
        if direct {
            #[cfg(feature = "linux-direct-io")]
            {
                return Self::direct_from(source);
            }
            #[cfg(not(feature = "linux-direct-io"))]
            {
                return Err(direct_reads_unsupported());
            }
        }
        Self::buffered_from(source, invocation)
    }
}

impl FileReader {
    /// The metadata of the file this reader opened (an `fstat` of its own
    /// descriptor), so a size read here describes the bytes this reader
    /// serves, whatever the input's path names by now.
    pub(crate) fn metadata(&self) -> io::Result<std::fs::Metadata> {
        match self {
            Self::Buffered(r) => r.get_ref().metadata(),
            #[cfg(feature = "linux-direct-io")]
            Self::Direct(r) => r.metadata(),
        }
    }

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
    /// For `DirectReader`: consumes buffered bytes, then moves its read
    /// position. `DirectReader::skip` already validates EOF and errors on
    /// past-end, so no extra check is needed.
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

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::fadvise::{Advice, tests::attempts};

    /// Every Linux build hints sequential readahead on a buffered open. The
    /// hint used to compile only with the `O_DIRECT` or `io_uring` feature,
    /// so a default build read without it while every published number was
    /// measured with it; this runs in the no-feature build too.
    #[test]
    fn a_buffered_open_advises_sequential_readahead() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("in.bin");
        std::fs::write(&path, b"bytes").unwrap();
        let before = attempts(Advice::Sequential);
        let _reader =
            super::FileReader::buffered_in(&path, &crate::host::Invocation::new()).unwrap();
        assert_eq!(
            attempts(Advice::Sequential),
            before + 1,
            "FileReader::buffered_in must advise POSIX_FADV_SEQUENTIAL exactly once"
        );
    }
}
