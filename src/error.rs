use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::result;
use std::str;
use std::str::Utf8Error;

use crate::read::blob::ByteOffset;

// Error data structures are modeled on the `csv` crate by BurntSushi.
// Manual Display/StdError impls are intentional - avoids a thiserror dependency
// for a small, stable enum that rarely changes.

#[cold]
pub(crate) fn new_error(kind: ErrorKind) -> Error {
    Error(Box::new(Inner {
        kind,
        at: None,
        file: None,
        blob: None,
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

/// A type alias for `Result<T, pbfhogg::Error>`.
pub type Result<T> = result::Result<T, Error>;

#[derive(Debug)]
struct Inner {
    kind: ErrorKind,
    at: Option<ByteOffset>,
    /// The input this error came from, when the reader was opened from a
    /// path. `BlobReader::new` is handed an arbitrary `Read` and has no
    /// path to record, so it leaves this `None` rather than inventing one.
    file: Option<std::sync::Arc<std::path::Path>>,
    /// 0-based index of the blob being read when the error was detected.
    blob: Option<u64>,
}

/// An error that can occur when reading or writing PBF files.
#[derive(Debug)]
pub struct Error(Box<Inner>);

impl Error {
    /// Return the specific type of this error.
    #[inline]
    pub fn kind(&self) -> &ErrorKind {
        &self.0.kind
    }

    /// Unwrap this error into its underlying type.
    ///
    /// Note that this discards the byte offset; use [`Error::byte_offset`]
    /// first if the location matters.
    #[inline]
    pub fn into_kind(self) -> ErrorKind {
        self.0.kind
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
        self.0.at
    }

    /// Attach the byte offset at which this error was detected. Called by
    /// the framing layer, which is the lowest layer that knows the position.
    /// An offset already attached by an inner layer wins - the innermost
    /// frame is the more precise one.
    #[cold]
    #[must_use]
    pub(crate) fn at(mut self, offset: ByteOffset) -> Error {
        if self.0.at.is_none() {
            self.0.at = Some(offset);
        }
        self
    }

    /// The input file this error came from, when the reader knew one.
    ///
    /// A byte offset alone is ambiguous on a command with several inputs -
    /// `cat a.pbf b.pbf`, `apply-changes`, `merge-changes` all report a
    /// position with no way to tell which stream it indexes. `None` means
    /// the reader was constructed from a bare `Read` and genuinely has no
    /// path, never that the path was unavailable at the reporting site.
    #[inline]
    pub fn source_path(&self) -> Option<&std::path::Path> {
        self.0.file.as_deref()
    }

    /// 0-based index of the blob being read when the error was detected.
    #[inline]
    pub fn blob_index(&self) -> Option<u64> {
        self.0.blob
    }

    /// Attach the input file and blob index. Same innermost-wins rule as
    /// [`Error::at`]: an inner reader that already named its own input is
    /// the more precise attribution, and an outer one must not overwrite
    /// it with the file *it* happens to be reading.
    #[cold]
    #[must_use]
    pub(crate) fn in_input(
        mut self,
        file: Option<&std::sync::Arc<std::path::Path>>,
        blob: Option<u64>,
    ) -> Error {
        // Stamped as a pair, never field by field: a file from one reader
        // and a blob index from another would read as a precise location
        // and point nowhere. Either field being present means an inner
        // reader already stamped, so the whole pair is left alone - a
        // reader that seeked away from the stream start has a file but no
        // trustworthy blob count, and an outer stamp must not "complete"
        // it with a count from a different stream.
        if self.0.blob.is_none() && self.0.file.is_none() {
            self.0.blob = blob;
            self.0.file = file.map(std::sync::Arc::clone);
        }
        self
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
        io::Error::other(err)
    }
}

// Removed deprecated `description()` (deprecated since Rust 1.42) and `cause()` (deprecated
// since Rust 1.33). Callers should use the `Display` impl (below) for human-readable error
// messages - it covers all ErrorKind variants. The `source()` method is the modern replacement
// for `cause()` with the same semantics. BlobError variants have no underlying source error,
// so they return None.
impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self.0.kind {
            ErrorKind::Io(ref err) => Some(err),
            ErrorKind::PlatformUnsupported { ref source, .. } => Some(source),
            ErrorKind::StringtableUtf8 { ref err, .. } => Some(err),
            ErrorKind::StringtableIndexOutOfBounds { .. } => None,
            ErrorKind::Blob(_) => None,
            ErrorKind::WireFormat { .. } => None,
            ErrorKind::MissingHeader => None,
            ErrorKind::MissingFeature(_) => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.fmt_kind(f)?;
        // The "where": which input, which blob, which byte. Only the
        // framing layer knows any of it, so an error raised below that
        // layer still prints exactly as it did before this was added.
        let mut sep = " (";
        if let Some(path) = self.0.file.as_deref() {
            write!(f, "{sep}in {}", path.display())?;
            sep = ", ";
        }
        if let Some(blob) = self.0.blob {
            write!(f, "{sep}blob {blob}")?;
            sep = ", ";
        }
        if let Some(at) = self.0.at {
            write!(f, "{sep}at byte offset {})", at.0)?;
        } else if sep == ", " {
            write!(f, ")")?;
        }
        Ok(())
    }
}

impl Error {
    fn fmt_kind(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0.kind {
            ErrorKind::Io(ref err) => fmt::Display::fmt(err, f),
            ErrorKind::PlatformUnsupported {
                feature,
                ref source,
            } => {
                write!(f, "platform feature {feature} unavailable: {source}")
            }
            ErrorKind::StringtableUtf8 { ref err, index } => {
                write!(f, "invalid UTF-8 at string table index {index}: {err}")
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
                if feature == "LocationsOnWays" {
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

/// Carry a setup refusal through APIs whose public result is `io::Result`.
#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
pub(crate) fn platform_unsupported(feature: &'static str, source: io::Error) -> io::Error {
    io::Error::other(new_error(ErrorKind::PlatformUnsupported {
        feature,
        source,
    }))
}

/// Classify a failed `open(2)` with `O_DIRECT`. `EINVAL` and `EOPNOTSUPP`
/// at open are the filesystem refusing direct I/O (tmpfs, some overlay and
/// network filesystems); every other errno stays an ordinary I/O failure.
/// Only the open is classified this way: an `EINVAL` from a later read or
/// write is an alignment bug, never a platform refusal.
#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
pub(crate) fn direct_io_open_error(error: io::Error) -> io::Error {
    if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP)) {
        platform_unsupported("direct-io", error)
    } else {
        error
    }
}

/// The feature label of a setup refusal carried by `error`, if it carries one.
#[cfg(feature = "linux-direct-io")]
pub(crate) fn carried_platform_feature(error: &io::Error) -> Option<&'static str> {
    match error.get_ref()?.downcast_ref::<Error>()?.kind() {
        ErrorKind::PlatformUnsupported { feature, .. } => Some(*feature),
        _ => None,
    }
}

/// Apply `map` (typically adding context such as the path) to an I/O error
/// without losing a carried setup refusal: a carrier has `map` applied to its
/// source and is re-carried, anything else is mapped directly. Context that
/// rebuilds the error from its message alone would otherwise turn a platform
/// refusal back into a plain I/O failure.
#[cfg(feature = "linux-direct-io")]
pub(crate) fn map_io_preserving_platform(
    error: io::Error,
    map: impl FnOnce(io::Error) -> io::Error,
) -> io::Error {
    if carried_platform_feature(&error).is_none() {
        return map(error);
    }
    match error
        .into_inner()
        .and_then(|inner| inner.downcast::<Error>().ok())
        .map(|inner| (*inner).into_kind())
    {
        Some(ErrorKind::PlatformUnsupported { feature, source }) => {
            platform_unsupported(feature, map(source))
        }
        _ => unreachable!("checked PlatformUnsupported payload"),
    }
}

#[cfg(all(test, any(feature = "linux-direct-io", feature = "linux-io-uring")))]
mod platform_tests {
    use super::*;

    #[test]
    fn setup_error_survives_io_result_carrier() {
        let error = Error::from(platform_unsupported(
            "io-uring",
            io::Error::from_raw_os_error(libc::EPERM),
        ));
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
    fn context_mapping_keeps_the_platform_refusal() {
        let refused = direct_io_open_error(io::Error::from_raw_os_error(libc::EINVAL));
        let named = map_io_preserving_platform(refused, |source| {
            io::Error::new(source.kind(), format!("failed to open x: {source}"))
        });
        assert_eq!(carried_platform_feature(&named), Some("direct-io"));
        assert!(named.to_string().contains("failed to open x"));

        let plain = direct_io_open_error(io::Error::from_raw_os_error(libc::ENOENT));
        assert_eq!(carried_platform_feature(&plain), None);
        assert_eq!(plain.raw_os_error(), Some(libc::ENOENT));
    }
}
