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
pub enum FileReader {
    /// Standard buffered reader (256 KB buffer).
    Buffered(BufReader<File>),
    /// O_DIRECT reader with page-aligned buffering (Linux only).
    #[cfg(feature = "linux-direct-io")]
    Direct(DirectReader),
}

impl FileReader {
    /// Open a file for buffered reading (256 KB `BufReader`).
    pub fn buffered(path: &Path) -> io::Result<Self> {
        let f = File::open(path)?;
        Ok(Self::Buffered(BufReader::with_capacity(256 * 1024, f)))
    }

    /// Open a file for O_DIRECT reading with page-aligned buffers.
    #[cfg(feature = "linux-direct-io")]
    pub fn direct(path: &Path) -> io::Result<Self> {
        Ok(Self::Direct(DirectReader::open(path)?))
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

impl Read for FileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Buffered(r) => r.read(buf),
            #[cfg(feature = "linux-direct-io")]
            Self::Direct(r) => r.read(buf),
        }
    }
}
