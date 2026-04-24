//! Kernel-space byte copy helpers (`copy_file_range`) for passthrough writes.
//!
//! Lets the merge/passthrough write path hand raw blob bytes from an input fd
//! straight into the output fd without a userspace round-trip. On filesystems
//! with reflink support (btrfs, xfs), `copy_file_range` can be a metadata-only
//! operation.
//!
//! Cross-device copies (EXDEV) fall back to `pread` + `write` in userspace.
//! Requires the `linux-direct-io` feature.

use std::io;
use std::os::unix::io::RawFd;

/// Copy `len` bytes between file descriptors using `copy_file_range(2)`.
///
/// Uses an explicit input offset (does not change `in_fd`'s file position),
/// safe when `in_fd` is wrapped in a `BufReader` or `DirectReader`.
/// Output uses the fd's current position (sequential write).
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn copy_range(
    in_fd: RawFd,
    out_fd: RawFd,
    mut offset: u64,
    mut len: u64,
) -> io::Result<()> {
    while len > 0 {
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
                return copy_range_fallback(in_fd, out_fd, offset, len);
            }
            return Err(err);
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "copy_file_range returned 0",
            ));
        }
        let n = n.cast_unsigned() as u64;
        offset += n;
        len -= n;
    }
    Ok(())
}

/// Fallback for cross-device copies: pread from `in_fd` at `offset`, write to `out_fd`.
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
    in_fd: RawFd,
    out_fd: RawFd,
    mut offset: u64,
    mut len: u64,
) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::io::FromRawFd;

    let mut buf = vec![0u8; 256 * 1024];
    // Wrap in ManuallyDrop so we don't close the fd when done - caller owns it.
    let mut out = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(out_fd) });
    // Read from in_fd using pread (doesn't change file position).
    while len > 0 {
        let chunk = buf.len().min(len as usize);
        let n = unsafe {
            libc::pread(in_fd, buf.as_mut_ptr().cast(), chunk, offset as i64)
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "pread returned 0 during cross-device copy",
            ));
        }
        let n = n.cast_unsigned();
        out.write_all(&buf[..n])?;
        offset += n as u64;
        len -= n as u64;
    }
    Ok(())
}
