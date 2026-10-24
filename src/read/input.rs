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

use std::fs::File;
use std::path::Path;
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

/// An open input and the path it was opened from, so a failure reading it
/// can name the file. Cloning shares both.
#[derive(Clone, Debug)]
pub struct InputFile {
    file: Arc<File>,
    path: Arc<Path>,
}

impl InputFile {
    /// Open `path` for positional reads, naming it in a failure.
    ///
    /// # Errors
    /// The open failed; the error names `path`.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            file: Arc::new(open(path)?),
            path: Arc::from(path),
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
