//! Kernel-space byte copy helpers (`copy_file_range`) for passthrough writes.
//!
//! Lets the merge/passthrough write path hand raw blob bytes from an input fd
//! straight into the output fd without a userspace round-trip. On filesystems
//! with reflink support (btrfs, xfs), `copy_file_range` can be a metadata-only
//! operation.
//!
//! Cross-device copies (EXDEV) fall back to `pread` + `write` in userspace.
//! Requires the `linux-direct-io` feature.
//!
//! Errors name what they can: a failed `copy_file_range` names the input and
//! the byte range (the syscall does not say which end failed; the writer adds
//! the output's name outside it), a failed fallback read is located in the
//! input at its offset, and a failed fallback write is left to the writer,
//! which names the output.

use std::io;
use std::os::unix::io::{AsRawFd as _, RawFd};

use crate::error::{Error, Location, Result};
use crate::read::input::InputFile;

/// A failed `copy_file_range` of `start..end` from `input`.
#[cold]
fn copy_error(err: io::Error, input: &InputFile, start: u64, end: u64) -> Error {
    Error::from(err).context(format!(
        "failed to copy bytes {start}..{end} of {}",
        input.path().display()
    ))
}

/// Copy `len` bytes from `input` at `offset` to `out_fd` using
/// `copy_file_range(2)`.
///
/// Uses an explicit input offset (does not change the input's file
/// position), safe when the input is also read through a `BufReader` or
/// `DirectReader`. Output uses the fd's current position (sequential write).
///
/// `cancel` is checked before every `copy_file_range` call: a coalesced
/// passthrough run can span the whole input (sort of an already-sorted
/// planet is one range), and the kernel copies at most about 2 GiB per call,
/// so this is the point where a cancelled run stops splicing. The error is
/// `ErrorKind::Cancelled`.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn copy_range(
    input: &InputFile,
    out_fd: RawFd,
    mut offset: u64,
    mut len: u64,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    let in_fd = input.file().as_raw_fd();
    let (start, end) = (offset, offset.saturating_add(len));
    while len > 0 {
        cancel.check()?;
        let mut off_in = offset as i64;
        // Safety: fds are valid and open. off_in is explicit (doesn't change
        // in_fd position). off_out is NULL (uses out_fd's current position).
        let n = unsafe {
            libc::copy_file_range(
                in_fd,
                &mut off_in,
                out_fd,
                std::ptr::null_mut(),
                len as usize,
                0,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EXDEV) {
                // Cross-device: fall back to pread+write.
                return copy_range_fallback(input, out_fd, offset, len, cancel);
            }
            return Err(copy_error(err, input, start, end));
        }
        if n == 0 {
            return Err(copy_error(
                io::Error::new(io::ErrorKind::UnexpectedEof, "copy_file_range returned 0"),
                input,
                start,
                end,
            ));
        }
        let n = n.cast_unsigned() as u64;
        offset += n;
        len -= n;
    }
    Ok(())
}

/// Fallback for cross-device copies: pread from `input` at `offset`, write
/// to `out_fd`.
///
/// **Single-threaded output only.** Uses position-based `write_all`, which
/// advances `out_fd`'s file position. Parallel writers that call
/// `pwrite`/`write_at` on the same `out_fd` concurrently with this helper
/// would race on the implicit position and produce a torn output.
/// `parallel_writer::copy_range_fallback_pwrite` is the pwrite-based
/// sibling for parallel contexts; new parallel callers should use that
/// primitive rather than reaching for this one.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn copy_range_fallback(
    input: &InputFile,
    out_fd: RawFd,
    mut offset: u64,
    mut len: u64,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::io::FromRawFd;

    let in_fd = input.file().as_raw_fd();
    let mut buf = vec![0u8; crate::write::IO_BUF_BYTES];
    // Wrap in ManuallyDrop so we don't close the fd when done - caller owns it.
    let mut out = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(out_fd) });
    // Read from in_fd using pread (doesn't change file position).
    while len > 0 {
        cancel.check()?;
        let chunk = buf.len().min(len as usize);
        let n = unsafe { libc::pread(in_fd, buf.as_mut_ptr().cast(), chunk, offset as i64) };
        if n < 0 {
            return Err(read_error(io::Error::last_os_error(), input, offset));
        }
        if n == 0 {
            return Err(read_error(
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "pread returned 0 during cross-device copy",
                ),
                input,
                offset,
            ));
        }
        let n = n.cast_unsigned();
        out.write_all(&buf[..n])?;
        offset += n as u64;
        len -= n as u64;
    }
    Ok(())
}

/// A failed read of `input` at `offset` during a cross-device copy.
#[cold]
pub(crate) fn read_error(err: io::Error, input: &InputFile, offset: u64) -> Error {
    Error::from(err).locate(Location::at_offset(input.path(), offset))
}
