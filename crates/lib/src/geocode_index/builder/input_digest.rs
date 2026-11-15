//! The input's whole-file XXH3-128 and size, recorded in the index header so
//! a consumer can check which PBF an index was built from.
//!
//! The build never reads its input as one sequential stream (a header walk,
//! then per-kind blob preads in kind order), so the digest is a read of its
//! own: a fourth open of the pinned input, advised `SEQUENTIAL` on its own
//! description so the passes' `RANDOM` description is left alone, read on a
//! background thread beside passes 1 to 3, and joined only for the header
//! write: pass 3 reads no input, so its work hides what the hash's read
//! does not finish inside the reading passes. The digest is of every byte of
//! the file, frame headers, compressed payloads and frames of unknown type
//! included, and so equals a plain whole-file XXH3-128 (seed zero) of the
//! same file.
//!
//! What it proves rests on the pin: it names the file the passes read, but a
//! pin is not a snapshot (`PinnedInput`). The hashed size is checked against
//! the size the schedule walk framed, and the file's size is checked again
//! once the last pass has read it, so a truncation or growth that persists
//! is refused; a same-size rewrite during the build is not caught, and the
//! input must stay unchanged for the build's duration.
//!
//! A failed hash read is reported when the header is due, not the moment it
//! happens: the build is refused before anything is published, but the
//! passes running at the time are not interrupted. A build that fails first
//! raises the stop flag, which the hash sees between chunks.

use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, Result};
use crate::read::input::PinnedInput;
use crate::tunables::GEOCODE_INPUT_HASH_CHUNK_BYTES;

/// The recorded identity of an input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InputDigest {
    pub(super) xxh3_128: u128,
    pub(super) bytes: u64,
}

/// Open the input for the hash: on the calling thread, through the pin, so
/// it is counted and verified like every other open of the build.
pub(super) fn open(input: &PinnedInput, invocation: &crate::host::Invocation) -> Result<File> {
    let file = input.open()?;
    crate::fadvise::advise(&file, 0, 0, crate::fadvise::Hint::Sequential, invocation);
    Ok(file)
}

/// Hash `file` from its start to its end. Stops with a cancellation when the
/// invocation is cancelled, and returns `Ok(None)` when `stop` is raised (the
/// build failed and no longer wants the answer).
pub(super) fn hash(
    mut file: File,
    input: &PinnedInput,
    stop: &AtomicBool,
    invocation: &crate::host::Invocation,
) -> Result<Option<InputDigest>> {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; GEOCODE_INPUT_HASH_CHUNK_BYTES];
    let mut bytes: u64 = 0;
    // loop-ends: each pass reads one chunk; the loop ends at end of file, on
    // a read error, a cancellation, or the stop flag the failed passes raise.
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        invocation.check()?;
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(Error::from(e).context(format!(
                    "failed to read {} for its digest",
                    input.path().display()
                )));
            }
        };
        hasher.update(&buf[..n]);
        bytes += n as u64;
    }
    Ok(Some(InputDigest {
        xxh3_128: hasher.digest128(),
        bytes,
    }))
}

/// Refuse a digest whose extent is not the file the build framed: the size
/// the schedule walk saw, and the size the pinned file has now, after the
/// last pass read it.
pub(super) fn check_extent(
    digest: InputDigest,
    walked_size: u64,
    input: &PinnedInput,
) -> Result<InputDigest> {
    let now = input.metadata()?.len();
    if digest.bytes != walked_size || now != walked_size {
        return Err(Error::invalid_data(format!(
            "{} changed size during the geocode build (framed {walked_size} bytes, \
             hashed {}, {now} now); the input must stay unchanged while it is built",
            input.path().display(),
            digest.bytes,
        )));
    }
    Ok(digest)
}

/// Raises the hash worker's stop flag when dropped, so a pass that fails or
/// unwinds inside the scope ends the hash before the scope joins it.
pub(super) struct StopOnDrop<'a>(pub(super) &'a AtomicBool);

impl StopOnDrop<'_> {
    /// The passes succeeded: let the hash run to its end.
    pub(super) fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
