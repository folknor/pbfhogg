//! Unified file reader that selects between buffered and O_DIRECT I/O.
//!
//! [`FileReader`] wraps either a standard `BufReader<File>` or (when the
//! `linux-direct-io` feature is enabled) a [`DirectReader`] that bypasses the
//! kernel page cache. All read-path code uses `FileReader` as the concrete
//! reader type, so the O_DIRECT path is zero-cost when the feature is off
//! (single-variant enum optimizes away).

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
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
    /// The fd is borrowed only for this call, never cached by the caller.
    /// This is the library's one eviction of an input, through
    /// `fadvise::evict_input`: the advice counts into `invocation`'s
    /// `fadvise_input_evictions` (and a failure into its `fadvise_failures`),
    /// and none is issued under a sealed invocation.
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
                offset != 0 && crate::fadvise::evict_input(reader.get_ref(), 0, offset, invocation)
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
    ///
    /// # Errors
    /// The open failure, or [`ErrorKind::Sealed`](crate::ErrorKind::Sealed)
    /// under a sealed invocation.
    pub fn buffered_in(path: &Path, invocation: &crate::host::Invocation) -> Result<Self> {
        let _call = invocation.call()?;
        Self::buffered_from(InputSource::Path(path), invocation)
    }

    fn buffered_from(
        source: InputSource<'_>,
        invocation: &crate::host::Invocation,
    ) -> Result<Self> {
        let f = source.open()?;
        crate::fadvise::advise(&f, 0, 0, crate::fadvise::Hint::Sequential, invocation);
        Ok(Self::Buffered(BufReader::with_capacity(
            crate::write::IO_BUF_BYTES,
            f,
        )))
    }

    /// Open a file for O_DIRECT reading with buffers aligned to the file's
    /// direct I/O alignment.
    #[cfg(feature = "linux-direct-io")]
    pub fn direct(path: &Path) -> Result<Self> {
        Self::direct_from(InputSource::Path(path), None)
    }

    #[cfg(feature = "linux-direct-io")]
    fn direct_from(
        source: InputSource<'_>,
        invocation: Option<&crate::host::Recorder>,
    ) -> Result<Self> {
        let path = source.path();
        // A platform refusal already names the path inside its source, so
        // it stays the outermost text, and a replaced pinned input is its
        // own refusal; any other failure gets the context.
        let opened = source
            .open_direct(invocation)
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
        let _call = invocation.call()?;
        let source = source.into();
        if direct {
            #[cfg(feature = "linux-direct-io")]
            {
                return Self::direct_from(source, Some(&_call));
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
    /// `reference/truncation-handling.md`,
    /// a payload that doesn't deliver the declared `data_size` must
    /// hard-error here, not be deferred.
    ///
    /// For `DirectReader`: consumes buffered bytes, then moves its read
    /// position. `DirectReader::skip` already validates EOF and errors on
    /// past-end, so no extra check is needed.
    ///
    /// Forward only, and it must stay so: [`Self::append_to`] reads past a
    /// drained `BufReader` straight from the `File`, which leaves the
    /// consumed buffer behind the real position, so a backward
    /// `seek_relative` would land in stale bytes.
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

impl FileReader {
    /// Append up to `n` bytes to `out` without zero-filling them, stopping
    /// short only where the input ends; returns how many were appended (not
    /// counting what `out` already held). The `Frames` append for this
    /// reader (`Frames::over_file`).
    ///
    /// The generic route, a bounded `read_to_end` over this `Read`, goes
    /// through std's default `read_buf`, which zero-fills every byte and
    /// keeps reads at 8 KiB-and-doubling sizes. Here the buffered bytes are
    /// copied out of the `BufReader`, a remainder of at least its capacity
    /// is read by a bounded `read_to_end` on the `File` itself (whose native
    /// `read_buf` reads into uninitialised capacity), and a smaller one is
    /// refilled through the buffer. O_DIRECT copies from its aligned buffer
    /// ([`DirectReader::append_to`]).
    pub(crate) fn append_to(&mut self, n: usize, out: &mut Vec<u8>) -> io::Result<usize> {
        out.reserve(n);
        match self {
            Self::Buffered(r) => append_buffered(r, n, out),
            #[cfg(feature = "linux-direct-io")]
            Self::Direct(r) => r.append_to(n, out),
        }
    }
}

/// [`FileReader::append_to`] for a buffered reader. Reading the `File`
/// directly is sound only with the buffer drained, which the loop
/// guarantees; it leaves the consumed buffer behind the file position,
/// harmless because the reader never seeks backward ([`FileReader::skip`]).
fn append_buffered(r: &mut BufReader<File>, n: usize, out: &mut Vec<u8>) -> io::Result<usize> {
    let mut appended = 0;
    while appended < n {
        let want = n - appended;
        let buffered = r.buffer();
        if !buffered.is_empty() {
            let take = want.min(buffered.len());
            out.extend_from_slice(&buffered[..take]);
            r.consume(take);
            appended += take;
            continue;
        }
        if want >= r.capacity() {
            let got = r.get_mut().take(want as u64).read_to_end(out)?;
            return Ok(appended + got);
        }
        match r.fill_buf() {
            Ok([]) => break,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(appended)
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
    use crate::fadvise::{Hint, tests::hint_attempts};
    use std::io::{BufReader, Read};

    /// A buffered reader over `len` patterned bytes with a 16-byte buffer, so
    /// every append transition is reachable with small sizes.
    fn small_buffered(len: usize) -> (super::FileReader, Vec<u8>, tempfile::TempDir) {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("in.bin");
        #[allow(clippy::cast_possible_truncation)]
        let bytes: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let reader = super::FileReader::Buffered(BufReader::with_capacity(16, file));
        (reader, bytes, dir)
    }

    /// Appends below, at and above the buffer capacity, after a partial
    /// drain, into a non-empty vector, a zero-length append, a skip after a
    /// direct read, and a short count at the end: each lands the right bytes
    /// and leaves the reader at the right position.
    #[test]
    fn append_to_delivers_exact_bytes_across_every_transition() {
        let (mut r, bytes, _dir) = small_buffered(200);
        let mut out = vec![0xAA];
        // 5 bytes: a fill, then a partial drain of the buffer.
        assert_eq!(r.append_to(5, &mut out).unwrap(), 5);
        // 0 bytes: nothing read, nothing appended.
        assert_eq!(r.append_to(0, &mut out).unwrap(), 0);
        // 11 left in the buffer, then 40 more past capacity read directly.
        assert_eq!(r.append_to(51, &mut out).unwrap(), 51);
        // Exactly the capacity, with the buffer empty: the direct read.
        assert_eq!(r.append_to(16, &mut out).unwrap(), 16);
        // Below the capacity: a refill through the buffer.
        assert_eq!(r.append_to(15, &mut out).unwrap(), 15);
        assert_eq!(out[0], 0xAA);
        assert_eq!(&out[1..], &bytes[..87]);
        // A forward skip after the direct reads, then the next bytes.
        r.skip(13).unwrap();
        let mut next = Vec::new();
        assert_eq!(r.append_to(20, &mut next).unwrap(), 20);
        assert_eq!(next, bytes[100..120]);
        // A request past the end is a short count with every byte there.
        let mut tail = Vec::new();
        assert_eq!(r.append_to(500, &mut tail).unwrap(), 80);
        assert_eq!(tail, bytes[120..]);
        assert_eq!(r.append_to(4, &mut tail).unwrap(), 0);
    }

    /// The zero-free append agrees with the generic `Read` route it replaces.
    #[test]
    fn append_to_matches_the_read_route() {
        let (mut a, bytes, _da) = small_buffered(300);
        let (mut b, _, _db) = small_buffered(300);
        for n in [1usize, 15, 16, 17, 64, 3, 100] {
            let mut x = Vec::new();
            let mut y = Vec::new();
            let got_a = a.append_to(n, &mut x).unwrap();
            let got_b = b.by_ref().take(n as u64).read_to_end(&mut y).unwrap();
            assert_eq!((got_a, &x), (got_b, &y), "append of {n}");
        }
        assert!(!bytes.is_empty());
    }

    /// Every Linux build hints sequential readahead on a buffered open. The
    /// hint used to compile only with the `O_DIRECT` or `io_uring` feature,
    /// so a default build read without it while every published number was
    /// measured with it; this runs in the no-feature build too.
    #[test]
    fn a_buffered_open_advises_sequential_readahead() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("in.bin");
        std::fs::write(&path, b"bytes").unwrap();
        let before = hint_attempts(Hint::Sequential);
        let _reader =
            super::FileReader::buffered_in(&path, &crate::host::Call::detached()).unwrap();
        assert_eq!(
            hint_attempts(Hint::Sequential),
            before + 1,
            "FileReader::buffered_in must advise POSIX_FADV_SEQUENTIAL exactly once"
        );
    }
}
