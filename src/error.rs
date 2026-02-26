use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::result;
use std::str;
use std::str::Utf8Error;

use prost::DecodeError;

// Error data structures are modeled on the `csv` crate by BurntSushi.
// Manual Display/StdError impls are intentional — avoids a thiserror dependency
// for a small, stable enum that rarely changes.

#[cold]
pub(crate) fn new_error(kind: ErrorKind) -> Error {
    Error(Box::new(kind))
}

#[cold]
pub(crate) fn new_blob_error(kind: BlobError) -> Error {
    Error(Box::new(ErrorKind::Blob(kind)))
}

#[cold]
pub(crate) fn new_protobuf_error(err: DecodeError, location: &'static str) -> Error {
    Error(Box::new(ErrorKind::Protobuf { err, location }))
}

/// A type alias for `Result<T, pbfhogg::Error>`.
pub type Result<T> = result::Result<T, Error>;

/// An error that can occur when reading PBF files.
#[derive(Debug)]
pub struct Error(Box<ErrorKind>);

impl Error {
    /// Return the specific type of this error.
    #[inline]
    pub fn kind(&self) -> &ErrorKind {
        &self.0
    }

    /// Unwrap this error into its underlying type.
    #[inline]
    pub fn into_kind(self) -> ErrorKind {
        *self.0
    }
}

/// The specific type of an error.
#[non_exhaustive]
#[derive(Debug)]
pub enum ErrorKind {
    /// An error for I/O operations.
    Io(io::Error),
    /// An error that occurs when decoding a protobuf message.
    Protobuf {
        err: DecodeError,
        location: &'static str,
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
        new_error(ErrorKind::Io(err))
    }
}

impl From<Error> for io::Error {
    fn from(err: Error) -> io::Error {
        io::Error::other(err)
    }
}

// Removed deprecated `description()` (deprecated since Rust 1.42) and `cause()` (deprecated
// since Rust 1.33). Callers should use the `Display` impl (below) for human-readable error
// messages — it covers all ErrorKind variants. The `source()` method is the modern replacement
// for `cause()` with the same semantics. BlobError variants have no underlying source error,
// so they return None.
impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match *self.0 {
            ErrorKind::Io(ref err) => Some(err),
            ErrorKind::Protobuf { ref err, .. } => Some(err),
            ErrorKind::StringtableUtf8 { ref err, .. } => Some(err),
            ErrorKind::StringtableIndexOutOfBounds { .. } => None,
            ErrorKind::Blob(_) => None,
            ErrorKind::WireFormat { .. } => None,
            ErrorKind::MissingHeader => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self.0 {
            ErrorKind::Io(ref err) => err.fmt(f),
            ErrorKind::Protobuf { ref err, location } => {
                write!(f, "protobuf error at '{location}': {err}")
            }
            ErrorKind::StringtableUtf8 { ref err, index } => {
                write!(f, "invalid UTF-8 at string table index {index}: {err}")
            }
            ErrorKind::StringtableIndexOutOfBounds { index } => {
                write!(f, "stringtable index out of bounds: {index}")
            }
            ErrorKind::Blob(BlobError::InvalidHeaderSize) => {
                write!(f, "blob header size could not be decoded")
            }
            ErrorKind::Blob(BlobError::HeaderTooBig { size }) => {
                write!(f, "blob header is too big: {size} bytes")
            }
            ErrorKind::Blob(BlobError::MessageTooBig { size }) => {
                write!(f, "blob message is too big: {size} bytes")
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
        }
    }
}

#[cold]
pub(crate) fn new_wire_error(msg: &'static str) -> Error {
    new_error(ErrorKind::WireFormat { msg })
}
