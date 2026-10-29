//! Named inputs: an open file together with the path it was opened from, so
//! every error about it can say which file, and the one set of opens that
//! name the path when they fail.
//!
//! An `io::Error` from `open(2)` or `read(2)` carries no path. Reaching the
//! operator as a bare "Permission denied" or "No such file or directory" on
//! a command with several inputs (a config naming thirty region files, an ID
//! list, an expressions file, a geocode index's dozen data files) it says
//! nothing usable. Every open of a caller-named input goes through here
//! (`clippy.toml` refuses the bare `std` calls across the library and the
//! CLI, which has its own path-naming opens), and
//! every pool that preads blobs a schedule names holds an [`InputFile`], so a
//! failure is located with the file, the blob ordinal and the byte offset.
//!
//! Locating is cold: nothing here runs per blob on success.
//!
//! # One input, one file, however many opens
//!
//! A command that reads its input in several passes opens it several times,
//! and on purpose: `posix_fadvise` advice and readahead state belong to an
//! open file description, so the header walker's `FADV_RANDOM`, a buffered
//! pass's `FADV_SEQUENTIAL` and a worker pool's plain body preads each keep
//! their own. Opening the path each time, though, resolves it each time, so
//! a file renamed over the input mid-run (an atomic replace by a downloader)
//! would be read partly from each file, with no error. A [`PinnedInput`]
//! closes that gap without merging any descriptions: it is taken once, at
//! the command's start, and every later open of that input is an
//! [`InputSource::Pinned`] open derived from it - a fresh description of the
//! pinned file, verified against it before use. See [`PinnedInput`] for what
//! it does and does not catch. A one-shot [`InputSource::Path`] open is what
//! a single-pass reader, a library entry point handed a bare path, and every
//! scratch file use.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Location, Result};

/// `failed to <what> <path>` context on a failed filesystem call, keeping
/// the original `io::Error` (its kind and OS error) as the cause.
#[cold]
fn named(what: &str, path: &Path, err: std::io::Error) -> Error {
    Error::from(err).context(format!("failed to {what} {}", path.display()))
}

/// `File::open`, naming `path` in the error.
#[allow(clippy::disallowed_methods)]
pub(crate) fn open(path: &Path) -> Result<File> {
    File::open(path).map_err(|e| named("open", path, e))
}

/// `fs::read`, naming `path` in the error.
#[allow(clippy::disallowed_methods)]
pub(crate) fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| named("read", path, e))
}

/// `fs::read_to_string`, naming `path` in the error. Content that is not
/// UTF-8 is malformed input, not an I/O failure: every text input read here
/// (an expressions file, an ID list, an extract config or region file) has
/// to be text, so it is reported as invalid data, with the original error as
/// its cause.
#[allow(clippy::disallowed_methods)]
pub(crate) fn read_to_string(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::InvalidData {
            Error::invalid_data_with(format!("failed to read {}", path.display()), e)
        } else {
            named("read", path, e)
        }
    })
}

/// `fs::metadata`, naming `path` in the error.
#[allow(clippy::disallowed_methods)]
pub(crate) fn metadata(path: &Path) -> Result<std::fs::Metadata> {
    std::fs::metadata(path).map_err(|e| named("stat", path, e))
}

/// Where an input is opened from: its path, resolved again by every open, or
/// a [`PinnedInput`] every open is derived from and verified against.
///
/// Deliberately not `AsRef<Path>`: a helper that took the path back out of a
/// pinned source and opened that would drop the pin without a trace.
/// [`Self::path`] is for naming the input, never for opening it.
#[derive(Clone, Copy, Debug)]
pub enum InputSource<'a> {
    /// The path, resolved again by every open: a single-pass reader, a
    /// library entry point handed a bare path.
    Path(&'a Path),
    /// A pinned input; see [`PinnedInput`].
    Pinned(&'a PinnedInput),
}

impl<'a> From<&'a Path> for InputSource<'a> {
    fn from(path: &'a Path) -> Self {
        Self::Path(path)
    }
}

impl<'a> From<&'a PathBuf> for InputSource<'a> {
    fn from(path: &'a PathBuf) -> Self {
        Self::Path(path.as_path())
    }
}

impl<'a> From<&'a Arc<Path>> for InputSource<'a> {
    fn from(path: &'a Arc<Path>) -> Self {
        Self::Path(path)
    }
}

impl<'a> From<&'a PinnedInput> for InputSource<'a> {
    fn from(pin: &'a PinnedInput) -> Self {
        Self::Pinned(pin)
    }
}

impl<'a> InputSource<'a> {
    /// The input's path, for naming it in diagnostics and errors.
    #[must_use]
    pub fn path(&self) -> &'a Path {
        match *self {
            Self::Path(path) => path,
            Self::Pinned(pin) => pin.path(),
        }
    }

    /// A new open file description of the input, for reading, with no
    /// advice applied. The error names the input.
    pub(crate) fn open(&self) -> Result<File> {
        match *self {
            Self::Path(path) => open(path),
            Self::Pinned(pin) => pin.open(),
        }
    }

    /// A new `O_DIRECT` open file description of the input and the
    /// alignment its reads must keep, as `crate::direct_io::open_for_read`
    /// returns them: a platform refusal names the input, an ordinary open
    /// failure is left for the caller to name.
    #[cfg(feature = "linux-direct-io")]
    pub(crate) fn open_direct(&self) -> Result<(File, usize)> {
        match *self {
            Self::Path(path) => crate::direct_io::open_for_read(path),
            Self::Pinned(pin) => pin.open_direct(),
        }
    }

    /// The input's metadata: of the pinned file for a pinned input, so a
    /// size or type read here describes the file the passes read.
    ///
    /// # Errors
    /// The `stat` failed; the error names the input.
    pub fn metadata(&self) -> Result<std::fs::Metadata> {
        match *self {
            Self::Path(path) => metadata(path),
            Self::Pinned(pin) => pin.metadata(),
        }
    }

    /// The input as a pin for a run under `host`: a path is pinned now (see
    /// [`PinnedInput::pin`]), a pinned input is shared as it is. A library
    /// entry point that opens its input more than once takes an
    /// `InputSource` and calls this, so a caller that composes several
    /// steps on one input (a CLI reading other inputs first) can pin it
    /// once, ahead of them, and hand the pin down.
    ///
    /// A shared pin is not taken again: it keeps the identity it was taken
    /// with and the host it was taken under (its fault plan, under
    /// `test-hooks`), whatever `host` says.
    ///
    /// # Errors
    /// As [`PinnedInput::pin`], for a path.
    pub fn pin(self, host: &crate::host::Host) -> Result<PinnedInput> {
        let host = &host.enter();
        match self {
            Self::Path(path) => PinnedInput::pin(path, host),
            Self::Pinned(pin) => Ok(pin.clone()),
        }
    }
}

/// One input of one run, pinned, so that every open of it during the run
/// reads the same file.
///
/// Taken once, at the start of a command that opens its input more than
/// once, before any other open of it; every later open goes through
/// [`InputSource::Pinned`] and yields a new open file description (its own
/// offset, flags, `posix_fadvise` advice and readahead state, exactly as an
/// open of the path would), of the pinned file. The pin itself holds one
/// descriptor for the run, which nothing reads or advises. Clones share it,
/// and it lives as long as the last clone.
///
/// How a later open reaches the pinned file:
///
/// - On Linux, when `/proc/self/fd` reopens the pin (checked once, with a
///   real open, when the pin is taken), every open goes through the pin's
///   `/proc/self/fd` entry, which names the pinned file itself whatever its
///   path names now. A file renamed over the input, or the input unlinked,
///   mid-run does not affect the run: it reads the file it started on.
/// - Otherwise (no usable procfs, another Unix) every open goes through the
///   path again, and an open that finds a different file there is refused
///   with [`ErrorKind::InputReplaced`](crate::ErrorKind::InputReplaced), so
///   the run stops rather than read some passes from each file. These opens
///   are non-blocking, so a FIFO put in the input's place cannot hang the
///   run, but the open itself still happens: a device put in its place
///   sees the open before it is refused.
///
/// Either way every descriptor is checked against the pin (`fstat`, device
/// and inode) before it is returned, and its `O_NONBLOCK` is cleared again.
///
/// What a pin does not catch: changes to the contents behind the same file
/// (an in-place rewrite or truncation), and a replacement before the pin was taken. A pin
/// identifies a file; it is not a snapshot of one. Where `std` reports no
/// device and inode (not Unix) nothing is verified: every open is a plain
/// open of the path.
///
/// Only a regular file can be pinned: a command that pins reads its input
/// more than once, and a FIFO, socket or character device does not serve
/// the same bytes twice. A block device is refused as well: its `st_size`
/// is zero, which the header walker would read as an empty file. On Linux the pin is an
/// `O_PATH` descriptor, so the pin itself runs no device open and needs no
/// read permission (the one-off procfs probe is a real read-only open of the
/// pinned file); an unreadable input fails at its first real open, naming
/// the path.
///
/// Off Unix a pin holds no descriptor, only the path: its opens and its
/// metadata go through the path, unverified.
#[derive(Clone, Debug)]
pub struct PinnedInput(Arc<Pin>);

#[derive(Debug)]
struct Pin {
    path: Arc<Path>,
    #[cfg(unix)]
    anchor: File,
    #[cfg(unix)]
    identity: Identity,
    #[cfg(unix)]
    reopen: Reopen,
    /// Where the fault plan lives (see `crate::test_hooks`).
    #[cfg(feature = "test-hooks")]
    cancel: crate::host::Invocation,
}

/// `(st_dev, st_ino)`.
#[cfg(unix)]
type Identity = (u64, u64);

#[cfg(unix)]
fn identity_of(meta: &std::fs::Metadata) -> Identity {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// How a pinned input's later opens reach the pinned file.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reopen {
    /// Through `/proc/self/fd/<pin>`: the pinned file itself.
    Procfs,
    /// Through the path, verified.
    Path,
}

/// A read-only, non-blocking open of `path` with `extra` flags; the caller
/// names the input in a failure.
#[cfg(unix)]
fn open_nonblocking(path: &Path, extra: i32) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    // A read-only open: `crate::read::input` is the one owner of these, and
    // every caller names the input in the error.
    #[allow(clippy::disallowed_methods)]
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | extra)
        .open(path)
}

/// Clear `O_NONBLOCK` on `file`'s description again, keeping every other
/// status flag (`O_DIRECT` among them) as the open set it.
#[cfg(unix)]
fn clear_nonblock(file: &File, path: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` stays open for the call (borrowed from `file`); F_GETFL
    // takes no argument.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(named(
            "read the status flags of",
            path,
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: as above; F_SETFL takes the new status flags as an int.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(named(
            "set the status flags of",
            path,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

impl PinnedInput {
    /// Pin `path` for a run under `host`.
    ///
    /// # Errors
    /// The path cannot be opened or `stat`ed, or names something other than
    /// a regular file; the error names it.
    pub fn pin(path: &Path, host: &crate::host::Host) -> Result<Self> {
        let host = &host.enter();
        #[cfg(feature = "test-hooks")]
        let force_path = host.invocation.faults().forces_pin_path_reopen();
        #[cfg(not(feature = "test-hooks"))]
        let force_path = false;
        Self::pin_with(path, host, force_path)
    }

    #[cfg(unix)]
    fn pin_with(path: &Path, host: &crate::host::Entered<'_>, force_path: bool) -> Result<Self> {
        #[cfg(not(feature = "test-hooks"))]
        let _ = host;
        #[cfg(target_os = "linux")]
        let flags = libc::O_PATH;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        let anchor = open_nonblocking(path, flags).map_err(|e| named("open", path, e))?;
        let meta = anchor.metadata().map_err(|e| named("stat", path, e))?;
        let kind = meta.file_type();
        if !kind.is_file() {
            return Err(Error::invalid_argument(format!(
                "{} is not a regular file: this command needs a regular file as input \
                 (a FIFO, /dev/stdin, a socket or a device will not do); write the \
                 stream to a file first and pass that",
                path.display()
            )));
        }
        let identity = identity_of(&meta);
        let reopen = if !force_path && procfs_reopens(&anchor, identity) {
            Reopen::Procfs
        } else {
            Reopen::Path
        };
        Ok(Self(Arc::new(Pin {
            path: Arc::from(path),
            anchor,
            identity,
            reopen,
            #[cfg(feature = "test-hooks")]
            cancel: host.invocation.clone(),
        })))
    }

    #[cfg(not(unix))]
    fn pin_with(path: &Path, host: &crate::host::Entered<'_>, _force_path: bool) -> Result<Self> {
        #[cfg(not(feature = "test-hooks"))]
        let _ = host;
        let meta = metadata(path)?;
        if !meta.is_file() {
            return Err(Error::invalid_argument(format!(
                "{} is not a regular file: this command needs a regular file as input \
                 (a FIFO, /dev/stdin, a socket or a device will not do); write the \
                 stream to a file first and pass that",
                path.display()
            )));
        }
        Ok(Self(Arc::new(Pin {
            path: Arc::from(path),
            #[cfg(feature = "test-hooks")]
            cancel: host.invocation.clone(),
        })))
    }

    /// The pinned input's path, as given.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0.path
    }

    /// The pinned file's metadata (on Unix a live `fstat` of the pin; off
    /// Unix a `stat` of the path).
    ///
    /// # Errors
    /// The `stat` failed; the error names the input.
    pub fn metadata(&self) -> Result<std::fs::Metadata> {
        #[cfg(unix)]
        {
            self.0
                .anchor
                .metadata()
                .map_err(|e| named("stat", &self.0.path, e))
        }
        #[cfg(not(unix))]
        {
            metadata(&self.0.path)
        }
    }

    /// See [`InputSource::open`].
    pub(crate) fn open(&self) -> Result<File> {
        self.hooked(|| self.open_verified())
    }

    /// See [`InputSource::open_direct`].
    #[cfg(feature = "linux-direct-io")]
    pub(crate) fn open_direct(&self) -> Result<(File, usize)> {
        self.hooked(|| {
            crate::direct_io::open_for_read_verified(
                &self.open_path(),
                &self.0.path,
                libc::O_NONBLOCK,
                |file| self.verify(file),
            )
        })
    }

    #[cfg(unix)]
    fn open_verified(&self) -> Result<File> {
        let file =
            open_nonblocking(&self.open_path(), 0).map_err(|e| named("open", &self.0.path, e))?;
        self.verify(&file)?;
        Ok(file)
    }

    #[cfg(not(unix))]
    fn open_verified(&self) -> Result<File> {
        open(&self.0.path)
    }

    /// What a later open opens: the pin's `/proc/self/fd` entry, or the path.
    #[cfg(unix)]
    fn open_path(&self) -> std::borrow::Cow<'_, Path> {
        match self.0.reopen {
            Reopen::Procfs => std::borrow::Cow::Owned(procfs_entry(&self.0.anchor)),
            Reopen::Path => std::borrow::Cow::Borrowed(&self.0.path),
        }
    }

    /// Refuse `file` unless it is the pinned file, then clear the
    /// `O_NONBLOCK` the open added.
    #[cfg(unix)]
    fn verify(&self, file: &File) -> Result<()> {
        let meta = file
            .metadata()
            .map_err(|e| named("stat", &self.0.path, e))?;
        if identity_of(&meta) != self.0.identity {
            return Err(crate::error::new_input_replaced(&self.0.path));
        }
        clear_nonblock(file, &self.0.path)
    }

    #[cfg(feature = "test-hooks")]
    fn hooked<T>(&self, open: impl FnOnce() -> Result<T>) -> Result<T> {
        self.0.cancel.faults().pinned_open(&self.0.path, open)
    }

    #[cfg(not(feature = "test-hooks"))]
    #[allow(clippy::unused_self)]
    fn hooked<T>(&self, open: impl FnOnce() -> Result<T>) -> Result<T> {
        open()
    }

    /// Whether this pin's later opens go through `/proc/self/fd` (a replaced
    /// input does not affect the run) rather than the path (a replaced input
    /// fails the run with `InputReplaced`), so a replacement test can tell
    /// which outcome this host owes.
    #[cfg(any(test, feature = "test-hooks"))]
    #[must_use]
    pub fn reopens_through_procfs(&self) -> bool {
        #[cfg(unix)]
        {
            self.0.reopen == Reopen::Procfs
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

/// `/proc/self/fd/<fd>` of `file`.
#[cfg(unix)]
fn procfs_entry(file: &File) -> PathBuf {
    use std::os::unix::io::AsRawFd;
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// Whether a real read-only open of the pin's `/proc/self/fd` entry yields
/// the pinned file. Any failure, or any other file, means later opens go
/// through the path instead, which verifies every open just the same: the
/// fallback gives up tolerating a replaced input, never consistency.
#[cfg(target_os = "linux")]
fn procfs_reopens(anchor: &File, identity: Identity) -> bool {
    open_nonblocking(&procfs_entry(anchor), 0)
        .and_then(|probe| probe.metadata())
        .is_ok_and(|meta| identity_of(&meta) == identity)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn procfs_reopens(_anchor: &File, _identity: Identity) -> bool {
    false
}

/// Whether a command-line argument repeats an earlier one, for pinning:
/// the exact spelling, not `Path` equality (which folds separators and `.`
/// components) and not the file (no canonicalisation, which would resolve
/// the filesystem again). Two spellings of one file get a pin each.
fn repeats(earlier: &Path, path: &Path) -> bool {
    earlier.as_os_str() == path.as_os_str()
}

/// How many pins [`pin_sources`] takes for `sources`, each a new descriptor:
/// one per distinct spelling among the paths. A caller's pin is shared as
/// it is (a clone, no descriptor), and its anchor is already open, so a
/// probed allowance has already counted it.
pub(crate) fn new_pins(sources: &[InputSource<'_>]) -> usize {
    sources
        .iter()
        .enumerate()
        .filter(|&(i, source)| match *source {
            InputSource::Path(path) => !sources[..i].iter().any(
                |earlier| matches!(*earlier, InputSource::Path(earlier) if repeats(earlier, path)),
            ),
            InputSource::Pinned(_) => false,
        })
        .count()
}

/// Pin every input of a command that takes several, in argument order,
/// before any of them is opened otherwise: one [`PinnedInput`] per argument
/// position, an argument spelled exactly as an earlier one sharing that
/// argument's pin. `cat a a` names one file twice, so it reads one file
/// twice; two pins of one path taken one after the other would pin two
/// files if the path were replaced between them. Every other open of each
/// argument stays its own, derived from its pin.
///
/// A caller may hand some inputs over already pinned: a
/// [`InputSource::Pinned`] source is used as it is (see
/// [`InputSource::pin`]), and only [`InputSource::Path`] sources take part in
/// the exact-spelling rule, sharing the pin of an earlier `Path` source
/// spelled the same. A caller's pin never stands in for a path, nor a path
/// for a caller's pin, whatever their spellings: two pins of one path may
/// identify different files on purpose.
///
/// Holds one new descriptor per distinct path spelling ([`new_pins`]) until
/// the pins are dropped; a caller that may be handed many inputs asks the
/// host for them first (`ProcessActions::fd_allowance`).
///
/// # Errors
/// As [`PinnedInput::pin`], for the first path that fails.
pub(crate) fn pin_sources(
    sources: &[InputSource<'_>],
    host: &crate::host::Entered<'_>,
) -> Result<Vec<PinnedInput>> {
    let mut pins: Vec<PinnedInput> = Vec::with_capacity(sources.len());
    for (i, source) in sources.iter().enumerate() {
        let shared = match *source {
            InputSource::Path(path) => sources[..i].iter().position(
                |earlier| matches!(*earlier, InputSource::Path(earlier) if repeats(earlier, path)),
            ),
            InputSource::Pinned(_) => None,
        };
        let pin = match shared {
            Some(first) => pins[first].clone(),
            None => source.pin(host)?,
        };
        pins.push(pin);
    }
    Ok(pins)
}

/// An open input and the path it was opened from, so a failure reading it
/// can name the file. Cloning shares both.
#[derive(Clone, Debug)]
pub struct InputFile {
    file: Arc<File>,
    path: Arc<Path>,
}

impl InputFile {
    /// Open `source` for positional reads, naming it in a failure.
    ///
    /// # Errors
    /// The open failed, or a pinned input's path now names another file;
    /// the error names the input.
    pub fn open<'a>(source: impl Into<InputSource<'a>>) -> Result<Self> {
        let source = source.into();
        Ok(Self {
            file: Arc::new(source.open()?),
            path: Arc::from(source.path()),
        })
    }

    /// An input already opened from `path`.
    pub fn from_parts(file: Arc<File>, path: Arc<Path>) -> Self {
        Self { file, path }
    }

    /// The open file.
    #[must_use]
    pub fn file(&self) -> &Arc<File> {
        &self.file
    }

    /// The path the file was opened from.
    #[must_use]
    pub fn path(&self) -> &Arc<Path> {
        &self.path
    }

    /// Locate a failure of the work on `blob` with this input, the blob's
    /// ordinal and its frame start. A cancellation stays unlocated and an
    /// existing location (another file a callback read) is kept.
    #[cold]
    pub(crate) fn locate_blob(
        &self,
        blob: &crate::scan::classify::ScheduledBlob,
        err: Error,
    ) -> Error {
        err.locate_blob(blob.location(&self.path))
    }

    /// Locate a failure at byte `offset` of this input, for reads that have
    /// no blob ordinal at hand.
    #[cold]
    pub(crate) fn locate_at(&self, offset: u64, err: Error) -> Error {
        err.locate_blob(Location::new(
            Some(&self.path),
            None,
            Some(crate::ByteOffset(offset)),
        ))
    }
}

/// A pinned input against a path renamed over, unlinked, or naming a stream.
#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod pin_tests {
    use std::io::Read as _;
    use std::os::unix::fs::FileExt as _;
    use std::path::Path;

    use super::{InputSource, PinnedInput};
    use crate::ErrorKind;
    use crate::host::Host;

    fn read_all(mut file: std::fs::File) -> Vec<u8> {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        bytes
    }

    /// `original` at `input`, `replacement` beside it.
    fn fixture(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let input = dir.join("in.osm.pbf");
        let replacement = dir.join("replacement.osm.pbf");
        std::fs::write(&input, b"original").unwrap();
        std::fs::write(&replacement, b"the replacement").unwrap();
        (input, replacement)
    }

    /// On Linux the pin reopens through procfs: a file renamed over the
    /// input, or the input unlinked, leaves every later open on the pinned
    /// file.
    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_reopen_reads_the_pinned_file_after_a_replace_and_an_unlink() {
        let dir = crate::path_guard::tests::scratch();
        let (input, replacement) = fixture(dir.path());
        let pin = PinnedInput::pin_with(&input, &Host::entered_for_tests(), false).unwrap();
        if !pin.reopens_through_procfs() {
            // No usable procfs here: the path mode is covered by its own test.
            return;
        }
        assert_eq!(read_all(pin.open().unwrap()), b"original");
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&input)
            .unwrap();

        std::fs::rename(&replacement, &input).unwrap();
        assert_eq!(read_all(pin.open().unwrap()), b"original");
        assert_eq!(pin.metadata().unwrap().len(), 8);
        // The pin's metadata is a live fstat of the pinned file.
        std::io::Write::write_all(&mut writer, b"!").unwrap();
        assert_eq!(pin.metadata().unwrap().len(), 9);
        // The path itself now names the replacement.
        assert_eq!(read_all(super::open(&input).unwrap()), b"the replacement");

        std::fs::remove_file(&input).unwrap();
        assert_eq!(
            read_all(InputSource::from(&pin).open().unwrap()),
            b"original!"
        );
    }

    /// Reopening through the path refuses a file renamed over the input,
    /// with the typed error naming it, and keeps working while the path
    /// names the pinned file.
    #[test]
    fn path_reopen_refuses_a_replaced_input() {
        let dir = crate::path_guard::tests::scratch();
        let (input, replacement) = fixture(dir.path());
        let pin = PinnedInput::pin_with(&input, &Host::entered_for_tests(), true).unwrap();
        assert!(!pin.reopens_through_procfs());
        assert_eq!(read_all(pin.open().unwrap()), b"original");

        std::fs::rename(&replacement, &input).unwrap();
        let err = pin.open().unwrap_err();
        assert!(matches!(err.kind(), ErrorKind::InputReplaced), "{err:?}");
        assert_eq!(err.source_path(), Some(input.as_path()));
        let err = crate::file_reader::FileReader::open_in(
            &pin,
            false,
            &crate::host::Invocation::new().call(),
        )
        .err()
        .unwrap();
        assert!(matches!(err.kind(), ErrorKind::InputReplaced), "{err:?}");
    }

    /// Every later open is a description of its own (own offset), and none
    /// is left non-blocking.
    #[test]
    fn derived_opens_are_separate_blocking_descriptions() {
        use std::os::unix::io::AsRawFd as _;
        let dir = crate::path_guard::tests::scratch();
        let (input, _) = fixture(dir.path());
        for force_path in [false, true] {
            let pin =
                PinnedInput::pin_with(&input, &Host::entered_for_tests(), force_path).unwrap();
            let mut first = pin.open().unwrap();
            let mut second = pin.open().unwrap();
            let mut byte = [0u8; 1];
            first.read_exact(&mut byte).unwrap();
            first.read_exact(&mut byte).unwrap();
            second.read_exact(&mut byte).unwrap();
            assert_eq!(byte, *b"o", "the second description has its own offset");
            let mut at = [0u8; 2];
            second.read_exact_at(&mut at, 6).unwrap();
            assert_eq!(at, *b"al");
            // SAFETY: the descriptor is open for the call.
            let flags = unsafe { libc::fcntl(first.as_raw_fd(), libc::F_GETFL) };
            assert!(flags >= 0);
            assert_eq!(flags & libc::O_NONBLOCK, 0);
        }
    }

    /// An argument spelled exactly as an earlier one shares its pin; any
    /// other spelling, even of the same file, gets its own, and the count
    /// of pins taken agrees with `new_pins`.
    #[test]
    fn pin_sources_shares_a_pin_per_exact_spelling() {
        let dir = crate::path_guard::tests::scratch();
        let (input, other) = fixture(dir.path());
        let dotted = dir.path().join(".").join("in.osm.pbf");
        assert_eq!(input, dotted, "Path equality folds the `.`");
        let paths: [&Path; 5] = [&input, &other, &input, &dotted, &other];
        let sources = paths.map(InputSource::Path);
        let pins = super::pin_sources(&sources, &Host::entered_for_tests()).unwrap();
        let same = |i: usize, j: usize| std::sync::Arc::ptr_eq(&pins[i].0, &pins[j].0);
        assert!(same(0, 2) && same(1, 4));
        assert!(!same(0, 1) && !same(0, 3) && !same(1, 3));
        assert_eq!(super::new_pins(&sources), 3);
        for (pin, path) in pins.iter().zip(paths) {
            assert_eq!(pin.path().as_os_str(), path.as_os_str());
        }
    }

    /// A caller's pin is used as it is and never shared with a path, nor two
    /// caller pins of one path with each other; paths still share by exact
    /// spelling among themselves.
    #[test]
    fn pin_sources_keeps_caller_pins_apart_from_paths() {
        let dir = crate::path_guard::tests::scratch();
        let (input, _) = fixture(dir.path());
        let host = Host::entered_for_tests();
        let first = PinnedInput::pin(&input, &host).unwrap();
        let second = PinnedInput::pin(&input, &host).unwrap();
        let sources = [
            InputSource::Pinned(&first),
            InputSource::Path(&input),
            InputSource::Pinned(&second),
            InputSource::Path(&input),
            InputSource::Pinned(&first),
        ];
        let pins = super::pin_sources(&sources, &host).unwrap();
        let same = |i: usize, j: usize| std::sync::Arc::ptr_eq(&pins[i].0, &pins[j].0);
        assert!(std::sync::Arc::ptr_eq(&pins[0].0, &first.0));
        assert!(std::sync::Arc::ptr_eq(&pins[2].0, &second.0));
        assert!(same(0, 4) && same(1, 3));
        assert!(!same(0, 1) && !same(0, 2) && !same(1, 2));
        // Only the path takes a new pin: the caller's pins are open already.
        assert_eq!(super::new_pins(&sources), 1);
    }

    /// A FIFO is refused at pin time, without blocking on a writer.
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = crate::path_guard::tests::scratch();
        let fifo = dir.path().join("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let err = PinnedInput::pin(&fifo, &Host::entered_for_tests()).unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::InvalidArgument { .. }),
            "{err:?}"
        );
        assert!(err.report().contains(&fifo.display().to_string()));
    }

    /// The test-hooks replacement fires after the armed open and the later
    /// opens see it: refused through the path, unaffected through procfs.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn armed_replacement_fires_between_opens() {
        let dir = crate::path_guard::tests::scratch();
        for force_path in [true, false] {
            let (input, replacement) = fixture(dir.path());
            let host = Host::entered_for_tests();
            host.invocation
                .faults()
                .force_pin_path_reopen
                .store(force_path, std::sync::atomic::Ordering::Relaxed);
            host.invocation
                .faults()
                .arm_input_replacement(&input, &replacement, 1);
            let pin = PinnedInput::pin(&input, &host).unwrap();
            assert_eq!(read_all(pin.open().unwrap()), b"original");
            let later = pin.open();
            let report = host.invocation.faults().input_replacement().unwrap();
            assert!(report.fired);
            assert_eq!(report.attempts, 2);
            assert_eq!(report.attempts_after_fired, 1);
            if force_path || !pin.reopens_through_procfs() {
                let err = later.unwrap_err();
                assert!(matches!(err.kind(), ErrorKind::InputReplaced), "{err:?}");
                assert_eq!(report.refused, 1);
            } else {
                assert_eq!(read_all(later.unwrap()), b"original");
                assert_eq!(report.verified, 2);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ErrorKind;

    /// A text input that is not UTF-8 is malformed input, named, with the
    /// decoder's error kept as the cause; a missing one stays an I/O error.
    #[test]
    fn read_to_string_classifies_non_utf8_as_invalid_data() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("ids.txt");
        std::fs::write(&path, [b'n', b'1', b'\n', 0xff, 0xfe]).expect("write fixture");
        let err = super::read_to_string(&path).expect_err("not UTF-8");
        assert!(
            matches!(
                err.kind(),
                ErrorKind::InvalidData {
                    source: Some(_),
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(
            err.report().contains(&path.display().to_string()),
            "{}",
            err.report()
        );

        let missing = dir.path().join("missing.txt");
        let err = super::read_to_string(&missing).expect_err("missing");
        assert!(matches!(err.kind(), ErrorKind::Io(_)), "{err:?}");
        assert!(
            err.report().contains(&missing.display().to_string()),
            "{}",
            err.report()
        );
    }
}
