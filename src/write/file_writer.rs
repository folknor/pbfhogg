//! Enum wrapper for file-backed writers, supporting both buffered and
//! O_DIRECT paths through a single concrete type.
//!
//! When the `linux-direct-io` feature is not enabled, this is a single-variant
//! enum and the compiler optimizes away the match.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

#[cfg(feature = "linux-direct-io")]
use super::direct_writer::DirectWriter;

/// A file writer that is either buffered (normal) or direct I/O (`O_DIRECT`).
///
/// Implements [`Write`] by delegating to the active variant.
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
}

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            FileWriter::Buffered(w) => w.write(buf),
            #[cfg(feature = "linux-direct-io")]
            FileWriter::Direct(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            FileWriter::Buffered(w) => w.flush(),
            #[cfg(feature = "linux-direct-io")]
            FileWriter::Direct(w) => w.flush(),
        }
    }
}
