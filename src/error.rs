use std::borrow::Cow;
use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::result;
use std::str;
use std::str::Utf8Error;
use std::sync::Arc;

use crate::read::blob::ByteOffset;

/// The text of a panic payload: the `&str` or `String` that `panic!` with a
/// message produces, or a placeholder for a `panic_any` payload of another
/// type.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "non-string panic payload"
    }
}

/// `"<who> panicked: <payload>"` as an `Internal` error: the one wording
/// every join and `catch_unwind` site uses to surface a caught panic as a
/// failure. The payload text is the one place a message is extracted from a
/// foreign value, because a panic payload is not an error.
#[cold]
pub(crate) fn panic_error(who: &str, payload: &(dyn std::any::Any + Send)) -> Error {
    Error::internal(format!("{who} panicked: {}", panic_message(payload)))
}

// Error data structures are modeled on the `csv` crate by BurntSushi.
// Manual Display/StdError impls are intentional - avoids a thiserror dependency
// for a small, stable enum that rarely changes.

#[cold]
pub(crate) fn new_error(kind: ErrorKind) -> Error {
    Error(Box::new(Inner {
        kind: Arc::new(kind),
        context: Vec::new(),
        location: None,
    }))
}

#[cold]
pub(crate) fn new_blob_error(kind: BlobError) -> Error {
    new_error(ErrorKind::Blob(kind))
}

#[cold]
pub(crate) fn new_wire_error(msg: &'static str) -> Error {
    new_error(ErrorKind::WireFormat { msg })
}

#[cold]
pub(crate) fn new_missing_header() -> Error {
    new_error(ErrorKind::MissingHeader)
}

#[cold]
pub(crate) fn new_output_error(kind: OutputError) -> Error {
    new_error(ErrorKind::Output(kind))
}

#[cold]
pub(crate) fn new_missing_feature(feature: &'static str) -> Error {
    new_error(ErrorKind::MissingFeature(feature))
}

#[cold]
pub(crate) fn new_stringtable_utf8(err: Utf8Error, index: usize) -> Error {
    new_error(ErrorKind::StringtableUtf8 { err, index })
}

#[cold]
pub(crate) fn new_stringtable_index_out_of_bounds(index: usize) -> Error {
    new_error(ErrorKind::StringtableIndexOutOfBounds { index })
}

/// A type alias for `Result<T, pbfhogg::Error>`.
pub type Result<T> = result::Result<T, Error>;

/// Where in the input an error was detected: which input, which blob, which
/// byte. Applied to an error as a unit ([`Error::locate`]); an all-empty
/// `Location` is the same as no location.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Location {
    /// The input this error came from, when the reader was opened from a
    /// path. `BlobReader::new` is handed an arbitrary `Read` and has no
    /// path to record, so it leaves this `None` rather than inventing one.
    pub(crate) file: Option<Arc<Path>>,
    /// 0-based index of the blob being read when the error was detected.
    pub(crate) blob: Option<u64>,
    /// Byte offset into the input stream.
    pub(crate) offset: Option<ByteOffset>,
}

impl Location {
    /// A location naming `file`, blob `blob` and byte `offset`, each only
    /// when known. Cold: only built when an error is being reported.
    #[cold]
    pub(crate) fn new(
        file: Option<&Arc<Path>>,
        blob: Option<u64>,
        offset: Option<ByteOffset>,
    ) -> Location {
        Location {
            file: file.map(Arc::clone),
            blob,
            offset,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.file.is_none() && self.blob.is_none() && self.offset.is_none()
    }
}

impl fmt::Display for Location {
    /// `in <file>, blob <n>, at byte offset <n>`, each part only when known.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut sep = "";
        if let Some(path) = self.file.as_deref() {
            write!(f, "in {}", path.display())?;
            sep = ", ";
        }
        if let Some(blob) = self.blob {
            write!(f, "{sep}blob {blob}")?;
            sep = ", ";
        }
        if let Some(at) = self.offset {
            write!(f, "{sep}at byte offset {}", at.0)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct Inner {
    /// Shared so that `Error: Clone` copies a pointer, not the kind (which
    /// holds a non-`Clone` `io::Error`).
    kind: Arc<ErrorKind>,
    /// Context added by [`Error::context`], outermost first.
    context: Vec<Cow<'static, str>>,
    location: Option<Location>,
}

/// An error that can occur when reading or writing PBF files.
///
/// One thin pointer, and `Clone` (the kind is shared).
#[derive(Clone, Debug)]
pub struct Error(Box<Inner>);

impl Error {
    /// Return the specific type of this error.
    #[inline]
    pub fn kind(&self) -> &ErrorKind {
        &self.0.kind
    }

    /// Prefix this error with `msg`. Never changes the kind or the
    /// location; repeated calls nest, the latest call being outermost.
    #[cold]
    #[must_use]
    pub fn context(mut self, msg: impl Into<Cow<'static, str>>) -> Error {
        self.0.context.insert(0, msg.into());
        self
    }

    /// The caller's request is invalid: arguments, configuration or call
    /// order.
    #[cold]
    #[must_use]
    pub(crate) fn invalid_argument(msg: impl Into<Cow<'static, str>>) -> Error {
        new_error(ErrorKind::InvalidArgument {
            msg: msg.into(),
            source: None,
        })
    }

    /// Input content is malformed or out of range beyond PBF framing.
    #[cold]
    #[must_use]
    pub(crate) fn invalid_data(msg: impl Into<Cow<'static, str>>) -> Error {
        new_error(ErrorKind::InvalidData {
            msg: msg.into(),
            source: None,
        })
    }

    /// [`Error::invalid_data`] with a typed cause.
    #[cold]
    #[must_use]
    pub(crate) fn invalid_data_with(
        msg: impl Into<Cow<'static, str>>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Error {
        new_error(ErrorKind::InvalidData {
            msg: msg.into(),
            source: Some(Arc::new(source)),
        })
    }

    /// Classify a crate-owned leaf error (no cause of its own, its `Display`
    /// is the whole message, such as a range check) as an invalid argument.
    /// A leaf with a cause, or a foreign error, keeps that cause reachable
    /// through `source()` by being attached when the kind is built.
    #[cold]
    #[must_use]
    pub(crate) fn invalid_argument_from(leaf: impl StdError) -> Error {
        debug_assert!(leaf.source().is_none(), "a leaf has no cause");
        Error::invalid_argument(leaf.to_string())
    }

    /// [`Error::invalid_argument_from`] for invalid input content.
    #[cold]
    #[must_use]
    pub(crate) fn invalid_data_from(leaf: impl StdError) -> Error {
        debug_assert!(leaf.source().is_none(), "a leaf has no cause");
        Error::invalid_data(leaf.to_string())
    }

    /// A broken internal invariant or a caught panic.
    #[cold]
    #[must_use]
    pub(crate) fn internal(msg: impl Into<Cow<'static, str>>) -> Error {
        new_error(ErrorKind::Internal {
            msg: msg.into(),
            source: None,
        })
    }

    /// [`Error::internal`] with a typed cause.
    #[cold]
    #[must_use]
    pub(crate) fn internal_with(
        msg: impl Into<Cow<'static, str>>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Error {
        new_error(ErrorKind::Internal {
            msg: msg.into(),
            source: Some(Arc::new(source)),
        })
    }

    /// A thread pool that could not be built. The pool error stays the
    /// payload inside the `io::Error`, reached through
    /// `ErrorKind::Io(e).get_ref()`.
    #[cold]
    #[must_use]
    pub(crate) fn pool_build(what: &str, source: rayon::ThreadPoolBuildError) -> Error {
        Error::from(io::Error::other(source)).context(format!("failed to build {what}"))
    }

    /// An `Io` error that has a kind and a message but no OS cause: a sink
    /// or resource failed in a way that has no errno.
    #[cold]
    #[must_use]
    pub(crate) fn io_kind(kind: io::ErrorKind, msg: impl Into<Cow<'static, str>>) -> Error {
        let msg: Cow<'static, str> = msg.into();
        Error::from(io::Error::new(kind, msg.into_owned()))
    }

    /// Wrap an error returned by a caller-supplied callback. The wrapped
    /// error stays reachable through [`std::error::Error::source`].
    #[cold]
    #[must_use]
    pub fn external(source: impl StdError + Send + Sync + 'static) -> Error {
        new_error(ErrorKind::External {
            msg: Cow::Borrowed("callback error"),
            source: Some(Arc::new(source)),
        })
    }

    /// A platform setup refusal. Doc-hidden: for tests of exit-code
    /// handling, which cannot otherwise construct one from outside the crate.
    #[doc(hidden)]
    #[cold]
    #[must_use]
    pub fn platform_unsupported(feature: &'static str, source: io::Error) -> Error {
        new_error(ErrorKind::PlatformUnsupported { feature, source })
    }

    /// The complete human-readable text of this error and its causes.
    ///
    /// [`Display`](fmt::Display) shows only this error's own context,
    /// message and location; this walks the causes. Each error's own text is
    /// joined with `": "`, then every location is appended at the end,
    /// outermost first, a repeated location printed once. An `io::Error`
    /// carrying a `pbfhogg::Error` is transparent. The walk stops at the
    /// first foreign error, whose own text is printed.
    #[cold]
    #[must_use]
    pub fn report(&self) -> String {
        let mut texts: Vec<String> = Vec::new();
        let mut locations: Vec<String> = Vec::new();
        let mut current: &(dyn StdError + 'static) = self;
        loop {
            let Some(err) = current.downcast_ref::<Error>() else {
                if let Some(carried) = current.downcast_ref::<io::Error>().and_then(carried_error) {
                    current = carried;
                    continue;
                }
                texts.push(current.to_string());
                break;
            };
            texts.extend(err.0.context.iter().map(ToString::to_string));
            if let Some(location) = err.0.location.as_ref().filter(|l| !l.is_empty()) {
                let text = location.to_string();
                if !locations.contains(&text) {
                    locations.push(text);
                }
            }
            if let ErrorKind::Io(io) = err.kind() {
                if let Some(carried) = carried_error(io) {
                    current = carried;
                    continue;
                }
                texts.push(io.to_string());
                break;
            }
            texts.push(Message(err).to_string());
            match err.structural_source() {
                Some(next) => current = next,
                None => break,
            }
        }
        let mut out = texts.join(": ");
        for location in &locations {
            out.push_str(" (");
            out.push_str(location);
            out.push(')');
        }
        out
    }

    /// The cause this kind wraps, including an `io::Error` held directly.
    fn structural_source(&self) -> Option<&(dyn StdError + 'static)> {
        match &*self.0.kind {
            ErrorKind::Io(err) => Some(err),
            ErrorKind::PlatformUnsupported { source, .. } => Some(source),
            ErrorKind::StringtableUtf8 { err, .. } => Some(err),
            ErrorKind::InvalidArgument { source, .. }
            | ErrorKind::InvalidData { source, .. }
            | ErrorKind::Internal { source, .. }
            | ErrorKind::External { source, .. } => {
                source.as_deref().map(|s| s as &(dyn StdError + 'static))
            }
            ErrorKind::Output(
                OutputError::ExclusivePublishUnsupported { source, .. }
                | OutputError::Unsynced { source, .. },
            ) => Some(source),
            ErrorKind::Output(OutputError::RollbackIncomplete { source, .. }) => Some(source),
            ErrorKind::Output(_)
            | ErrorKind::StringtableIndexOutOfBounds { .. }
            | ErrorKind::Blob(_)
            | ErrorKind::WireFormat { .. }
            | ErrorKind::MissingHeader
            | ErrorKind::MissingFeature(_) => None,
        }
    }

    /// Attach `location`, unless this error already has one. The location
    /// is applied as a unit, never field by field, and an all-empty one
    /// counts as none.
    #[cold]
    #[must_use]
    pub(crate) fn locate(mut self, location: Location) -> Error {
        if self.0.location.as_ref().is_none_or(Location::is_empty) && !location.is_empty() {
            self.0.location = Some(location);
        }
        self
    }

    /// This error as an `io::Error` for an `io::Result` boundary. A bare
    /// `Io` error (no context, no location, kind not shared) gives back the
    /// original `io::Error`, keeping its OS error; anything else is carried
    /// whole, per `From<Error> for io::Error`.
    pub(crate) fn into_io(self) -> io::Error {
        if self.0.context.is_empty() && self.0.location.is_none() {
            let Inner { kind, .. } = *self.0;
            return match Arc::try_unwrap(kind) {
                Ok(ErrorKind::Io(err)) => err,
                Ok(kind) => io::Error::from(new_error(kind)),
                Err(shared) => io::Error::from(Error(Box::new(Inner {
                    kind: shared,
                    context: Vec::new(),
                    location: None,
                }))),
            };
        }
        io::Error::from(self)
    }

    /// Byte offset into the input stream at which the error was detected,
    /// when the detecting layer knew it.
    ///
    /// A wire-format or blob error carries no useful diagnosis on its own
    /// when the input is a 90 GB planet file; this is the "where". Errors
    /// raised below the framing layer (decompression, element decoding) do
    /// not know their own position and report `None`.
    #[inline]
    pub fn byte_offset(&self) -> Option<ByteOffset> {
        self.0.location.as_ref().and_then(|l| l.offset)
    }

    /// The input file this error came from, when the reader knew one.
    ///
    /// A byte offset alone is ambiguous on a command with several inputs -
    /// `cat a.pbf b.pbf`, `apply-changes`, `merge-changes` all report a
    /// position with no way to tell which stream it indexes. `None` means
    /// the reader was constructed from a bare `Read` and genuinely has no
    /// path, never that the path was unavailable at the reporting site.
    #[inline]
    pub fn source_path(&self) -> Option<&Path> {
        self.0.location.as_ref().and_then(|l| l.file.as_deref())
    }

    /// 0-based index of the blob being read when the error was detected.
    #[inline]
    pub fn blob_index(&self) -> Option<u64> {
        self.0.location.as_ref().and_then(|l| l.blob)
    }
}

/// The specific type of an error.
#[non_exhaustive]
#[derive(Debug)]
pub enum ErrorKind {
    /// An error for I/O operations.
    Io(io::Error),
    /// A requested I/O feature is unavailable during platform setup.
    /// Runtime I/O failures must remain `Io`, even when their errno matches.
    PlatformUnsupported {
        feature: &'static str,
        source: io::Error,
    },
    /// The stringtable contains an entry at `index` that could not be decoded to a valid UTF-8
    /// string.
    StringtableUtf8 { err: Utf8Error, index: usize },
    /// An element contains an out-of-bounds index to the stringtable.
    StringtableIndexOutOfBounds { index: usize },
    /// An error that occurs when decoding `Blob`s.
    Blob(BlobError),
    /// An error that occurs when decoding protobuf wire format.
    WireFormat { msg: &'static str },
    /// The first blob in the PBF file is not an `OsmHeader` blob.
    MissingHeader,
    /// The input header does not declare a feature required by the operation.
    MissingFeature(&'static str),
    /// The caller's request is invalid: arguments, configuration, or call
    /// order (such as writing after a writer completed).
    InvalidArgument {
        msg: Cow<'static, str>,
        source: Option<Arc<dyn StdError + Send + Sync>>,
    },
    /// Input content is malformed or out of range beyond PBF framing.
    InvalidData {
        msg: Cow<'static, str>,
        source: Option<Arc<dyn StdError + Send + Sync>>,
    },
    /// A broken internal invariant or a caught panic.
    Internal {
        msg: Cow<'static, str>,
        source: Option<Arc<dyn StdError + Send + Sync>>,
    },
    /// An error returned by a caller-supplied callback; see
    /// [`Error::external`].
    External {
        msg: Cow<'static, str>,
        source: Option<Arc<dyn StdError + Send + Sync>>,
    },
    /// An output could not be prepared or published; see
    /// [`crate::output`].
    Output(OutputError),
}

/// Why an output was refused or could not be published.
///
/// The texts name no front-end flag: how a caller grants overwrite
/// permission is the caller's business.
#[non_exhaustive]
#[derive(Debug)]
pub enum OutputError {
    /// The destination exists and replacing it was not requested.
    Exists { path: PathBuf },
    /// The destination is a symlink. Refused whatever the overwrite intent.
    Symlink { path: PathBuf, target: PathBuf },
    /// A file output's destination exists and is not a regular file.
    NotAFile { path: PathBuf },
    /// A directory output's destination exists and is not a directory.
    NotADirectory { path: PathBuf },
    /// A directory output's destination is a mount point.
    MountPoint { path: PathBuf },
    /// The path names no file or directory (empty, a root, or ending in
    /// `..`).
    InvalidPath { path: PathBuf },
    /// Two outputs of one run resolve to the same file.
    Collision { first: PathBuf, second: PathBuf },
    /// Publication without replacement needs an exclusive rename (or, for a
    /// file, a hard link), and this filesystem offers neither.
    ExclusivePublishUnsupported { path: PathBuf, source: io::Error },
    /// The output *was* published; syncing its directory failed, so its
    /// survival across a crash is unconfirmed.
    Unsynced { path: PathBuf, source: io::Error },
    /// Publishing failed and undoing it failed too. Each entry says what was
    /// left where; nothing it names was deleted.
    RollbackIncomplete {
        unresolved: Vec<String>,
        source: Error,
    },
}

impl fmt::Display for OutputError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Exists { path } => write!(
                f,
                "output {} already exists and replacing it was not requested",
                path.display()
            ),
            Self::Symlink { path, target } => write!(
                f,
                "output {} is a symlink to {}; an output is never written over a symlink",
                path.display(),
                target.display()
            ),
            Self::NotAFile { path } => write!(
                f,
                "output {} exists and is not a regular file",
                path.display()
            ),
            Self::NotADirectory { path } => {
                write!(f, "output {} exists and is not a directory", path.display())
            }
            Self::MountPoint { path } => write!(
                f,
                "output directory {} is a mount point; publication renames the directory \
                 itself, which a mount point cannot be",
                path.display()
            ),
            Self::InvalidPath { path } => write!(
                f,
                "output path '{}' does not name a file or directory",
                path.display()
            ),
            Self::Collision { first, second } => write!(
                f,
                "outputs {} and {} resolve to the same file",
                first.display(),
                second.display()
            ),
            Self::ExclusivePublishUnsupported { path, .. } => write!(
                f,
                "cannot publish {} without risking replacing an entry that appeared meanwhile: \
                 the filesystem supports neither an exclusive rename nor, for a file, a hard link",
                path.display()
            ),
            Self::Unsynced { path, .. } => write!(
                f,
                "output {} was published, but syncing its directory failed, so it may not \
                 survive a crash",
                path.display()
            ),
            Self::RollbackIncomplete { unresolved, .. } => write!(
                f,
                "publishing the output failed and could not be fully undone: {}",
                unresolved.join("; ")
            ),
        }
    }
}

/// An error that occurs when decoding a blob.
#[non_exhaustive]
#[derive(Debug)]
pub enum BlobError {
    /// Header size could not be decoded to a u32.
    InvalidHeaderSize,
    /// Blob header is bigger than [`MAX_BLOB_HEADER_SIZE`](blob/MAX_BLOB_HEADER_SIZE.v.html).
    HeaderTooBig {
        /// Blob header size in bytes.
        size: u64,
    },
    /// Blob content is bigger than [`MAX_BLOB_MESSAGE_SIZE`](blob/MAX_BLOB_MESSAGE_SIZE.v.html).
    MessageTooBig {
        /// Blob content size in bytes.
        size: u64,
    },
    /// `BlobHeader.datasize` declares a serialized `Blob` message bigger than
    /// [`MAX_BLOB_DATASIZE`](blob/MAX_BLOB_DATASIZE.v.html). Rejected before the
    /// compressed body is allocated, so a hostile declared size cannot force an
    /// outsized pre-decompression allocation.
    DataSizeTooBig {
        /// Declared `datasize` in bytes.
        size: u64,
    },
    /// The blob is empty because the `raw` and `zlib-data` fields are missing.
    Empty,
    /// Blob header declares a negative `datasize`.
    InvalidDataSize {
        /// The negative datasize value.
        size: i32,
    },
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        // Preserve structured setup failures carried through the Write API.
        if err
            .get_ref()
            .is_some_and(<dyn std::error::Error + Send + Sync>::is::<Error>)
        {
            match err
                .into_inner()
                .and_then(|inner| inner.downcast::<Error>().ok())
            {
                Some(inner) => *inner,
                None => unreachable!("checked Error payload"),
            }
        } else {
            new_error(ErrorKind::Io(err))
        }
    }
}

impl From<protohoggr::WireError> for Error {
    fn from(err: protohoggr::WireError) -> Error {
        new_error(ErrorKind::WireFormat { msg: err.msg })
    }
}

impl From<Error> for io::Error {
    fn from(err: Error) -> io::Error {
        // The full error rides inside the io::Error, so it round trips
        // exactly through `From<io::Error> for Error`. Only the outer
        // `io::ErrorKind` is derived, so `Write` layers keep reporting
        // StorageFull, Interrupted and BrokenPipe for an `Io` error. The
        // outer error has no raw OS error: decisions on errno must look at
        // the original syscall error before converting.
        let kind = match err.kind() {
            ErrorKind::Io(inner) => inner.kind(),
            ErrorKind::PlatformUnsupported { .. } => io::ErrorKind::Unsupported,
            ErrorKind::InvalidData { .. } => io::ErrorKind::InvalidData,
            ErrorKind::InvalidArgument { .. } => io::ErrorKind::InvalidInput,
            ErrorKind::Output(OutputError::Exists { .. } | OutputError::Collision { .. }) => {
                io::ErrorKind::AlreadyExists
            }
            ErrorKind::Output(
                OutputError::Symlink { .. }
                | OutputError::NotAFile { .. }
                | OutputError::NotADirectory { .. }
                | OutputError::MountPoint { .. }
                | OutputError::InvalidPath { .. },
            ) => io::ErrorKind::InvalidInput,
            _ => io::ErrorKind::Other,
        };
        io::Error::new(kind, err)
    }
}

/// The `pbfhogg::Error` an `io::Error` carries as its payload, if any, seen
/// through any number of `io::Error` layers wrapped around it.
fn carried_error(err: &io::Error) -> Option<&Error> {
    let mut payload: &(dyn StdError + 'static) = err.get_ref()?;
    loop {
        if let Some(error) = payload.downcast_ref::<Error>() {
            return Some(error);
        }
        payload = payload.downcast_ref::<io::Error>()?.get_ref()?;
    }
}

/// An error's own message, without context, location or causes.
struct Message<'a>(&'a Error);

// Removed deprecated `description()` (deprecated since Rust 1.42) and `cause()` (deprecated
// since Rust 1.33). Callers should use the `Display` impl (below) for human-readable error
// messages - it covers all ErrorKind variants. The `source()` method is the modern replacement
// for `cause()` with the same semantics. BlobError variants have no underlying source error,
// so they return None.
impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match *self.0.kind {
            // Like std's `io::Error`: the payload is not a separate link.
            // The CLI exit-code walker inspects the `io::Error` directly.
            ErrorKind::Io(ref err) => err.source(),
            _ => self.structural_source(),
        }
    }
}

impl fmt::Display for Error {
    /// This error's own context, message and location. The text of its
    /// source is never included; see [`Error::report`].
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for context in &self.0.context {
            write!(f, "{context}: ")?;
        }
        fmt::Display::fmt(&Message(self), f)?;
        // The "where": which input, which blob, which byte.
        if let Some(location) = self.0.location.as_ref().filter(|l| !l.is_empty()) {
            write!(f, " ({location})")?;
        }
        Ok(())
    }
}

impl fmt::Display for Message<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self.0.0.kind {
            ErrorKind::Io(ref err) => fmt::Display::fmt(err, f),
            ErrorKind::PlatformUnsupported { feature, .. } => {
                write!(f, "platform feature {feature} unavailable")
            }
            ErrorKind::InvalidArgument { ref msg, .. }
            | ErrorKind::InvalidData { ref msg, .. }
            | ErrorKind::Internal { ref msg, .. }
            | ErrorKind::External { ref msg, .. } => f.write_str(msg),
            ErrorKind::Output(ref output) => fmt::Display::fmt(output, f),
            ErrorKind::StringtableUtf8 { index, .. } => {
                write!(f, "invalid UTF-8 at string table index {index}")
            }
            ErrorKind::StringtableIndexOutOfBounds { index } => {
                write!(f, "stringtable index out of bounds: {index}")
            }
            ErrorKind::Blob(BlobError::InvalidHeaderSize) => {
                // The only producer is the zero-length frame prefix in
                // `read::frame::check_header_len`. The old wording ("could not
                // be decoded") described a decode failure that never happens:
                // the four bytes decode fine, they just declare nothing.
                write!(
                    f,
                    "blob header size is zero: a frame must describe a blob, so a run of \
                     zero bytes is a corrupt file rather than a stream of empty frames"
                )
            }
            ErrorKind::Blob(BlobError::HeaderTooBig { size }) => {
                write!(f, "blob header is too big: {size} bytes")
            }
            ErrorKind::Blob(BlobError::MessageTooBig { size }) => {
                write!(f, "blob message is too big: {size} bytes")
            }
            ErrorKind::Blob(BlobError::DataSizeTooBig { size }) => {
                write!(f, "blob header declares datasize too big: {size} bytes")
            }
            ErrorKind::Blob(BlobError::Empty) => {
                write!(f, "blob is missing fields 'raw' and 'zlib_data'")
            }
            ErrorKind::Blob(BlobError::InvalidDataSize { size }) => {
                write!(f, "blob header has negative datasize: {size}")
            }
            ErrorKind::WireFormat { msg } => {
                write!(f, "wire format error: {msg}")
            }
            ErrorKind::MissingHeader => {
                write!(f, "PBF file does not start with an OsmHeader blob")
            }
            // No CLI flag names here. This is a library `Display` impl and
            // it has no idea which front end is reading it; naming
            // `--type node` made the library a dependency of the CLI's
            // argument surface. The hint stays, phrased in terms of the
            // file, so any caller can act on it.
            ErrorKind::MissingFeature(feature) => {
                write!(f, "input PBF is missing required feature: {feature}")?;
                if feature == crate::HeaderBlock::LOCATIONS_ON_WAYS {
                    write!(
                        f,
                        "; way geometry needs an input carrying inline way-node coordinates"
                    )?;
                }
                Ok(())
            }
        }
    }
}

/// Classify a failed `open(2)` with `O_DIRECT`. `EINVAL` and `EOPNOTSUPP`
/// at open are the filesystem refusing direct I/O (tmpfs, some overlay and
/// network filesystems); every other errno stays an ordinary I/O failure.
/// Only the open is classified this way: an `EINVAL` from a later read or
/// write is an alignment bug, never a platform refusal.
#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
pub(crate) fn direct_io_open_error(error: io::Error) -> Error {
    if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP)) {
        Error::platform_unsupported("direct-io", error)
    } else {
        Error::from(error)
    }
}

/// A failed open of `path`, rendered as the file it was about. The original
/// `io::Error` stays the cause.
#[cfg(feature = "linux-direct-io")]
#[derive(Debug)]
struct OpenFailed {
    path: String,
    source: io::Error,
}

#[cfg(feature = "linux-direct-io")]
impl fmt::Display for OpenFailed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "failed to open {}: {}", self.path, self.source)
    }
}

#[cfg(feature = "linux-direct-io")]
impl StdError for OpenFailed {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&self.source)
    }
}

/// [`direct_io_open_error`] for a read-side open of `path`. A platform
/// refusal renders as `platform feature direct-io unavailable: failed to
/// open <path>: <os text>`: the path rides inside the refusal's source, so
/// the refusal stays the outermost text. Any other failure is an ordinary
/// `Io` error without the path; the caller adds it as context.
#[cfg(feature = "linux-direct-io")]
pub(crate) fn direct_io_open_error_at(path: &Path, error: io::Error) -> Error {
    if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP)) {
        let kind = error.kind();
        let named = io::Error::new(
            kind,
            OpenFailed {
                path: path.display().to_string(),
                source: error,
            },
        );
        Error::platform_unsupported("direct-io", named)
    } else {
        Error::from(error)
    }
}

#[cfg(all(test, any(feature = "linux-direct-io", feature = "linux-io-uring")))]
mod platform_tests {
    use super::*;

    #[cfg(feature = "linux-direct-io")]
    #[test]
    fn read_side_refusal_renders_in_the_documented_order() {
        let refused = direct_io_open_error_at(
            Path::new("/some/in.osm.pbf"),
            io::Error::from_raw_os_error(libc::EINVAL),
        );
        assert!(matches!(
            refused.kind(),
            ErrorKind::PlatformUnsupported {
                feature: "direct-io",
                ..
            }
        ));
        let os_text = io::Error::from_raw_os_error(libc::EINVAL).to_string();
        assert_eq!(
            refused.report(),
            format!(
                "platform feature direct-io unavailable: failed to open /some/in.osm.pbf: {os_text}"
            )
        );
    }

    #[test]
    fn setup_error_survives_io_result_carrier() {
        let carried = io::Error::from(Error::platform_unsupported(
            "io-uring",
            io::Error::from_raw_os_error(libc::EPERM),
        ));
        let error = Error::from(carried);
        assert!(
            matches!(error.kind(), ErrorKind::PlatformUnsupported { feature: "io-uring", source }
            if source.raw_os_error() == Some(libc::EPERM))
        );
        assert!(error.source().is_some());
    }

    #[test]
    fn raw_io_errors_are_not_platform_refusals() {
        for errno in [libc::EINVAL, libc::ENOSPC, libc::EIO, libc::ENOMEM] {
            let error = Error::from(io::Error::from_raw_os_error(errno));
            assert!(matches!(error.kind(), ErrorKind::Io(source)
                if source.raw_os_error() == Some(errno)));
        }
    }

    #[cfg(feature = "linux-direct-io")]
    #[test]
    fn context_keeps_the_platform_refusal() {
        let refused = direct_io_open_error(io::Error::from_raw_os_error(libc::EINVAL));
        let named = refused.context("failed to open x");
        assert!(matches!(
            named.kind(),
            ErrorKind::PlatformUnsupported {
                feature: "direct-io",
                ..
            }
        ));
        // Display no longer includes the source's text; `report()` does.
        assert!(named.report().contains("failed to open x"));

        let plain = direct_io_open_error(io::Error::from_raw_os_error(libc::ENOENT));
        assert!(matches!(plain.kind(), ErrorKind::Io(e)
            if e.raw_os_error() == Some(libc::ENOENT)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid_data(msg: &'static str) -> Error {
        new_error(ErrorKind::InvalidData {
            msg: Cow::Borrowed(msg),
            source: None,
        })
    }

    fn location(file: &str, blob: u64, offset: u64) -> Location {
        Location {
            file: Some(Arc::from(Path::new(file))),
            blob: Some(blob),
            offset: Some(ByteOffset(offset)),
        }
    }

    #[test]
    fn display_is_own_context_message_and_location_only() {
        let err = Error::from(io::Error::other("disk gone"))
            .context("writing")
            .context("pass 2")
            .locate(Location {
                file: Some(Arc::from(Path::new("a.pbf"))),
                blob: Some(3),
                offset: None,
            });
        assert_eq!(
            err.to_string(),
            "pass 2: writing: disk gone (in a.pbf, blob 3)"
        );

        let wrapped = new_error(ErrorKind::InvalidArgument {
            msg: Cow::Borrowed("bad bbox"),
            source: Some(Arc::new(invalid_data("lat out of range"))),
        });
        assert_eq!(wrapped.to_string(), "bad bbox");
        assert_eq!(wrapped.report(), "bad bbox: lat out of range");
    }

    #[test]
    fn report_places_location_last_and_prints_duplicates_once() {
        let inner = invalid_data("bad tag").locate(location("a.pbf", 7, 99));
        let outer = new_error(ErrorKind::Internal {
            msg: Cow::Borrowed("worker failed"),
            source: Some(Arc::new(inner)),
        })
        .context("decode")
        .locate(location("a.pbf", 7, 99));
        assert_eq!(
            outer.report(),
            "decode: worker failed: bad tag (in a.pbf, blob 7, at byte offset 99)"
        );

        let other = new_error(ErrorKind::Internal {
            msg: Cow::Borrowed("outer"),
            source: Some(Arc::new(
                invalid_data("inner").locate(location("b.pbf", 1, 2)),
            )),
        })
        .locate(location("a.pbf", 7, 99));
        assert_eq!(
            other.report(),
            "outer: inner (in a.pbf, blob 7, at byte offset 99) (in b.pbf, blob 1, at byte offset 2)"
        );
    }

    #[test]
    fn report_reproduces_todays_text() {
        let utf8 = String::from_utf8(vec![0xff])
            .expect_err("invalid")
            .utf8_error();
        let err = new_error(ErrorKind::StringtableUtf8 {
            err: utf8,
            index: 4,
        })
        .locate(Location {
            offset: Some(ByteOffset(10)),
            ..Location::default()
        });
        assert_eq!(
            err.report(),
            format!("invalid UTF-8 at string table index 4: {utf8} (at byte offset 10)")
        );
        let platform = Error::platform_unsupported("direct-io", io::Error::other("nope"));
        assert_eq!(
            platform.report(),
            "platform feature direct-io unavailable: nope"
        );
        assert_eq!(
            platform.to_string(),
            "platform feature direct-io unavailable"
        );
    }

    #[test]
    fn report_sees_through_an_io_carrier_and_stops_at_foreign_errors() {
        let inner = invalid_data("bad")
            .context("ctx")
            .locate(location("a.pbf", 1, 2));
        let carried = Error::from(io::Error::from(inner));
        assert_eq!(
            carried.report(),
            "ctx: bad (in a.pbf, blob 1, at byte offset 2)"
        );

        let foreign = io::Error::other(ForeignWithSource);
        assert_eq!(Error::from(foreign).report(), "foreign");
        let external = Error::external(ForeignWithSource);
        assert_eq!(external.report(), "callback error: foreign");
    }

    #[test]
    fn report_sees_through_nested_io_carriers() {
        let inner = invalid_data("bad").context("ctx");
        let double = io::Error::other(io::Error::from(inner));
        assert_eq!(Error::from(double).report(), "ctx: bad");
    }

    #[derive(Debug)]
    struct ForeignWithSource;
    impl fmt::Display for ForeignWithSource {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("foreign")
        }
    }
    impl StdError for ForeignWithSource {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            Some(&FOREIGN_ROOT)
        }
    }
    #[derive(Debug)]
    struct ForeignRoot;
    impl fmt::Display for ForeignRoot {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("must not be printed")
        }
    }
    impl StdError for ForeignRoot {}
    static FOREIGN_ROOT: ForeignRoot = ForeignRoot;

    #[test]
    fn locate_applies_as_a_unit_and_empty_counts_as_none() {
        let err = invalid_data("x").locate(Location::default());
        assert!(err.0.location.is_none());
        let err = err.locate(location("a.pbf", 1, 2));
        let err = err.locate(Location {
            file: Some(Arc::from(Path::new("b.pbf"))),
            ..Location::default()
        });
        assert_eq!(err.source_path(), Some(Path::new("a.pbf")));
        assert_eq!(err.blob_index(), Some(1));
        assert_eq!(err.byte_offset().map(|o| o.0), Some(2));
    }

    #[test]
    fn clone_shares_the_kind() {
        let err = invalid_data("x").context("c");
        let copy = err.clone();
        assert!(Arc::ptr_eq(&err.0.kind, &copy.0.kind));
        assert_eq!(err.report(), copy.report());
    }

    #[test]
    fn io_round_trip_is_exact_and_maps_kinds() {
        let cases = [
            (
                Error::from(io::Error::from(io::ErrorKind::StorageFull)),
                io::ErrorKind::StorageFull,
            ),
            (
                Error::platform_unsupported("x", io::Error::other("e")),
                io::ErrorKind::Unsupported,
            ),
            (invalid_data("d"), io::ErrorKind::InvalidData),
            (
                new_error(ErrorKind::InvalidArgument {
                    msg: Cow::Borrowed("a"),
                    source: None,
                }),
                io::ErrorKind::InvalidInput,
            ),
            (
                new_error(ErrorKind::Internal {
                    msg: Cow::Borrowed("i"),
                    source: None,
                }),
                io::ErrorKind::Other,
            ),
            (Error::external(ForeignRoot), io::ErrorKind::Other),
        ];
        for (err, expected) in cases {
            let original = err.clone();
            let carrier = io::Error::from(err);
            assert_eq!(carrier.kind(), expected);
            assert_eq!(carrier.raw_os_error(), None);
            let back = Error::from(carrier);
            assert!(Arc::ptr_eq(&original.0.kind, &back.0.kind));
        }
    }
}

#[cfg(test)]
mod panic_message_tests {
    #[test]
    fn preserves_both_string_payloads_and_identifies_other_payloads() {
        assert_eq!(super::panic_message(&"borrowed panic"), "borrowed panic");
        assert_eq!(
            super::panic_message(&String::from("owned panic")),
            "owned panic"
        );
        assert_eq!(super::panic_message(&42_u32), "non-string panic payload");
    }
}
