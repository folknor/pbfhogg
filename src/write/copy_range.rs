//! Kernel-space byte copy helpers (`copy_file_range`) for passthrough writes,
//! and the syscall rules every raw copy loop under `write/` shares.
//!
//! Lets the merge/passthrough write path hand raw blob bytes from an input fd
//! straight into the output fd without a userspace round-trip. On filesystems
//! with reflink support (btrfs, xfs), `copy_file_range` can be a metadata-only
//! operation.
//!
//! When the kernel refuses the copy itself for this pair of files (see
//! [`kernel_copy_refused`] for the errno set and why it is safe), the range
//! falls back to `pread` + `write` in userspace. That fallback is a
//! performance cliff at planet scale, so every range routed to it is counted
//! (`writer_copy_range_fallback_ranges` / `_bytes`). Requires the
//! `linux-direct-io` feature.
//!
//! The shared rules, owned here so the loops cannot drift apart again:
//!
//! - **`EINTR` is retried, immediately, everywhere** ([`retry_eintr`]): a
//!   signal that interrupted a call moved nothing, so the same call is made
//!   again. No backoff. There is no cancellation check on the retry: library
//!   cancellation is a token checked between calls, not a signal, and the CLI
//!   blocks its signals in every thread, so nothing it does produces `EINTR`.
//! - **A short read is progress, not an error.** Each loop advances by what
//!   the call returned; only a read of zero bytes is end of file, which
//!   inside an indexed range means the input is shorter than its index says.
//! - **Lengths convert with [`len_to_usize`]**, saturating. The kernel and
//!   every loop here cap the per-call size anyway, so a saturated request is
//!   only ever a smaller first call.
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
use crate::write::metrics::WRITER_METRICS;

/// A raw `ssize_t` syscall return as a result: the count, or the thread's
/// `errno`. Read `errno` nowhere else - it must be taken before any other
/// call can overwrite it.
#[inline(always)]
pub(crate) fn syscall_result(ret: isize) -> io::Result<usize> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret.cast_unsigned())
    }
}

/// Run one syscall attempt until it is not interrupted. The shared `EINTR`
/// rule for every raw read, write and copy loop under `write/`: retry at
/// once, nothing else. Every other outcome, including a short count, goes
/// back to the caller.
#[inline(always)]
pub(crate) fn retry_eintr(mut call: impl FnMut() -> io::Result<usize>) -> io::Result<usize> {
    loop {
        match call() {
            Err(err) if err.raw_os_error() == Some(libc::EINTR) => {}
            other => return other,
        }
    }
}

/// A byte count as a per-call length, saturating where `usize` is narrower.
/// Every caller clamps the result against its own per-call cap (the kernel's
/// for `copy_file_range`, a buffer's for `pread`).
#[inline(always)]
pub(crate) fn len_to_usize(len: u64) -> usize {
    usize::try_from(len).unwrap_or(usize::MAX)
}

/// `pread(2)` of up to `len` bytes into `dst` at `offset`, under the shared
/// `EINTR` rule. May return fewer bytes than asked; zero is end of file.
///
/// # Safety
/// `dst` must be valid for writes of `len` bytes and `fd` open for reading.
#[allow(clippy::cast_possible_wrap)]
#[inline]
pub(crate) unsafe fn pread_retrying(
    fd: RawFd,
    dst: *mut u8,
    len: usize,
    offset: u64,
) -> io::Result<usize> {
    // An offset past `i64::MAX` wraps negative and the kernel answers EINVAL.
    retry_eintr(|| syscall_result(unsafe { libc::pread(fd, dst.cast(), len, offset as i64) }))
}

/// Whether the kernel refused a `copy_file_range` of `len` bytes from
/// `in_fd` at `src_offset` to `out_fd` (at `out_offset`, or at its file
/// position when `None`) in a way the userspace fallback may take over.
///
/// Called only on the cold error path, and only before any byte of the range
/// has moved: the kernel reports an error only when a call copied nothing,
/// so the offsets are exact at that point, but an errno after progress means
/// the kernel just accepted this very pair of files, and the run fails
/// rather than guess why (the conservative policy coreutils' `copy.c` also
/// takes). That is a failure policy, not a proof that finishing in userspace
/// would be impossible.
///
/// The errnos where the kernel is saying "not here" rather than "wrong":
///
/// - `EXDEV`: the two files are on filesystems the kernel will not copy
///   between.
/// - `ENOSYS`: no `copy_file_range` in this kernel (before 4.5).
/// - `EOPNOTSUPP` (`ENOTSUP` is the same value on Linux): the filesystem,
///   typically FUSE or NFS, does not implement it.
/// - `EPERM`: container seccomp profiles deny the syscall this way. A real
///   permission problem (an immutable output) is not masked: the fallback's
///   own write fails with it.
/// - `EINVAL`: the kernel also uses it for "not a regular file" (an output
///   that is `/dev/null` or a FIFO, which `FileWriter::buffered` accepts),
///   where the fallback is correct. Its other meanings are argument bugs, and
///   none can reach the fallback: flags are always 0, the two range checks
///   below refuse offsets the syscall cannot represent, and the same-file
///   check refuses the overlapping-ranges case.
///
/// The guards apply to every errno in the set, not just `EINVAL`: a kernel
/// that answers `ENOSYS` or a seccomp `EPERM` checked nothing about the
/// arguments. Each refuses the fallback (the caller reports the original
/// error), and each fails closed when it cannot tell:
///
/// - the range must end at or below `i64::MAX` on both sides that carry an
///   explicit offset (computed with `checked_add`);
/// - the two fds must be different files (`fstat` `st_dev`/`st_ino`): a
///   forward chunked copy within one file corrupts overlapping ranges;
/// - `out_fd` must not be `O_APPEND`, where `pwrite` ignores its offset on
///   Linux. No writer here opens one, and a caller that flips the flag on a
///   descriptor it handed over mid-copy is outside what any check can see.
///
/// `EBADF`, `EFBIG`, `EOVERFLOW`, `EIO` and everything else stay hard errors.
#[cold]
pub(crate) fn kernel_copy_refused(
    err: &io::Error,
    in_fd: RawFd,
    out_fd: RawFd,
    src_offset: u64,
    out_offset: Option<u64>,
    len: u64,
) -> bool {
    if !matches!(
        err.raw_os_error(),
        Some(libc::EXDEV | libc::ENOSYS | libc::EOPNOTSUPP | libc::EPERM | libc::EINVAL)
    ) {
        return false;
    }
    let representable = |start: u64| {
        start
            .checked_add(len)
            .is_some_and(|end| end <= i64::MAX.cast_unsigned())
    };
    if !representable(src_offset) || out_offset.is_some_and(|out| !representable(out)) {
        return false;
    }
    let (Some(input), Some(output)) = (file_id(in_fd), file_id(out_fd)) else {
        return false;
    };
    if input == output {
        return false;
    }
    // Safety: F_GETFL takes no argument and only reads the fd's flags.
    let flags = unsafe { libc::fcntl(out_fd, libc::F_GETFL) };
    flags >= 0 && flags & libc::O_APPEND == 0
}

/// `(st_dev, st_ino)` of an open fd, or `None` if `fstat` fails.
fn file_id(fd: RawFd) -> Option<(libc::dev_t, libc::ino_t)> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // Safety: `stat` is a valid out pointer for one `struct stat`.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // Safety: `fstat` returned 0, so it initialised `stat`.
    let stat = unsafe { stat.assume_init() };
    Some((stat.st_dev, stat.st_ino))
}

/// Count one range routed to the userspace fallback and the `len` bytes it
/// was asked to copy (counted at entry: a fallback that then fails partway
/// has still been routed this many). Cold: the kernel path never gets here.
#[cold]
pub(crate) fn record_fallback(len: u64) {
    WRITER_METRICS
        .copy_range_fallback_ranges
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    WRITER_METRICS
        .copy_range_fallback_bytes
        .fetch_add(len, std::sync::atomic::Ordering::Relaxed);
}

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
pub(crate) fn copy_range(
    input: &InputFile,
    out_fd: RawFd,
    offset: u64,
    len: u64,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    let in_fd = input.file().as_raw_fd();
    copy_range_with(input, out_fd, offset, len, cancel, |off_in, chunk| {
        // Safety: fds are valid and open. off_in is explicit (doesn't change
        // in_fd position). off_out is NULL (uses out_fd's current position).
        syscall_result(unsafe {
            libc::copy_file_range(in_fd, off_in, out_fd, std::ptr::null_mut(), chunk, 0)
        })
    })
}

/// The [`copy_range`] loop over one `copy_file_range` attempt, `cfr(off_in,
/// len)`. Production passes the syscall; tests pass a fake that scripts the
/// kernel's answers.
#[allow(clippy::cast_possible_wrap)]
#[inline(always)]
fn copy_range_with(
    input: &InputFile,
    out_fd: RawFd,
    mut offset: u64,
    mut len: u64,
    cancel: &crate::host::Cancel,
    mut cfr: impl FnMut(&mut i64, usize) -> io::Result<usize>,
) -> Result<()> {
    let (start, end) = (offset, offset.saturating_add(len));
    while len > 0 {
        cancel.check()?;
        let mut off_in = offset as i64;
        let chunk = len_to_usize(len);
        let n = match retry_eintr(|| cfr(&mut off_in, chunk)) {
            Ok(n) => n,
            Err(err) => {
                let in_fd = input.file().as_raw_fd();
                if offset == start && kernel_copy_refused(&err, in_fd, out_fd, offset, None, len) {
                    record_fallback(len);
                    return copy_range_fallback(input, out_fd, offset, len, cancel);
                }
                return Err(copy_error(err, input, start, end));
            }
        };
        if n == 0 {
            return Err(copy_error(
                io::Error::new(io::ErrorKind::UnexpectedEof, "copy_file_range returned 0"),
                input,
                start,
                end,
            ));
        }
        let n = n as u64;
        offset += n;
        len -= n;
    }
    Ok(())
}

/// Userspace fallback when the kernel refuses the copy: pread from `input`
/// at `offset`, write to `out_fd`.
///
/// **Single-threaded output only.** Uses position-based `write_all`, which
/// advances `out_fd`'s file position. Parallel writers that call
/// `pwrite`/`write_at` on the same `out_fd` concurrently with this helper
/// would race on the implicit position and produce a torn output.
/// `parallel_writer::copy_range_fallback_pwrite` is the pwrite-based
/// sibling for parallel contexts; new parallel callers should use that
/// primitive rather than reaching for this one.
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
    // Read from in_fd using pread (doesn't change file position). `write_all`
    // retries `Interrupted` itself, which is the same rule.
    while len > 0 {
        cancel.check()?;
        let chunk = buf.len().min(len_to_usize(len));
        // Safety: `buf` is valid for `chunk <= buf.len()` bytes.
        let n = unsafe { pread_retrying(in_fd, buf.as_mut_ptr(), chunk, offset) }
            .map_err(|err| read_error(err, input, offset))?;
        if n == 0 {
            return Err(read_error(
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "pread returned 0 during userspace copy fallback",
                ),
                input,
                offset,
            ));
        }
        out.write_all(&buf[..n])?;
        offset += n as u64;
        len -= n as u64;
    }
    Ok(())
}

/// A failed read of `input` at `offset` during a userspace copy.
#[cold]
pub(crate) fn read_error(err: io::Error, input: &InputFile, offset: u64) -> Error {
    Error::from(err).locate(Location::at_offset(input.path(), offset))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
pub(crate) mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use std::collections::VecDeque;
    use std::os::unix::fs::FileExt as _;
    use std::path::Path;
    use std::sync::atomic::Ordering::Relaxed;

    /// One scripted answer from a fake `copy_file_range`.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Step {
        /// Copy up to this many bytes for real, like the kernel would.
        Copy(usize),
        /// Fail with this errno, copying nothing.
        Errno(i32),
    }

    /// An input of `len` patterned bytes in `dir`.
    pub(crate) fn patterned_input(dir: &Path, len: usize) -> (InputFile, Vec<u8>) {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
        let path = dir.join("in.bin");
        std::fs::write(&path, &bytes).unwrap();
        (InputFile::open(&path).unwrap(), bytes)
    }

    /// A fake copy with the kernel's effect: read `min(asked, cap)` bytes at
    /// `off_in` and hand them to `write`, advancing `off_in`.
    pub(crate) fn real_copy(
        input: &InputFile,
        off_in: &mut i64,
        asked: usize,
        cap: usize,
        mut write: impl FnMut(&[u8]),
    ) -> usize {
        let mut buf = vec![0u8; asked.min(cap)];
        let off = u64::try_from(*off_in).unwrap();
        let n = input.file().read_at(&mut buf, off).unwrap();
        write(&buf[..n]);
        *off_in += i64::try_from(n).unwrap();
        n
    }

    /// Run the production [`copy_range`] loop against `script`, copying
    /// `offset..offset + len` of a fresh input into a fresh output.
    fn run(
        script: &[Step],
        input_len: usize,
        offset: u64,
        len: u64,
    ) -> (Result<()>, Vec<u8>, Vec<u8>, usize) {
        let dir = crate::path_guard::tests::scratch();
        let (input, bytes) = patterned_input(dir.path(), input_len);
        let out_path = dir.path().join("out.bin");
        let out = std::fs::File::create(&out_path).unwrap();
        let out_fd = out.as_raw_fd();
        let mut script: VecDeque<Step> = script.iter().copied().collect();
        let mut calls = 0;
        let result = copy_range_with(
            &input,
            out_fd,
            offset,
            len,
            &crate::host::Cancel::new(),
            |off_in, asked| {
                calls += 1;
                match script.pop_front().unwrap_or(Step::Copy(usize::MAX)) {
                    Step::Errno(errno) => Err(io::Error::from_raw_os_error(errno)),
                    // The implicit output position, as the kernel uses it.
                    Step::Copy(cap) => Ok(real_copy(&input, off_in, asked, cap, |b| {
                        std::io::Write::write_all(&mut &out, b).unwrap();
                    })),
                }
            },
        );
        drop(out);
        let written = std::fs::read(&out_path).unwrap();
        (result, written, bytes, calls)
    }

    fn errno_of(err: &Error) -> Option<i32> {
        match err.kind() {
            ErrorKind::Io(e) => e.raw_os_error(),
            _ => None,
        }
    }

    #[test]
    fn eintr_is_retried_and_short_copies_advance() {
        let script = [
            Step::Errno(libc::EINTR),
            Step::Copy(1000),
            Step::Errno(libc::EINTR),
            Step::Errno(libc::EINTR),
            Step::Copy(3),
        ];
        let (result, written, bytes, calls) = run(&script, 10_000, 100, 9_000);
        result.unwrap();
        assert_eq!(written, bytes[100..9_100]);
        assert_eq!(
            calls, 6,
            "four copies or retries, then the rest in one call"
        );
    }

    #[test]
    fn every_refusal_errno_falls_back_on_the_first_call() {
        for errno in [
            libc::EXDEV,
            libc::ENOSYS,
            libc::EOPNOTSUPP,
            libc::EPERM,
            libc::EINVAL,
        ] {
            let before = WRITER_METRICS.copy_range_fallback_ranges.load(Relaxed);
            // EINTR first: the decision is on the first answer that is not an
            // interruption. The range is larger than the fallback buffer.
            let len = crate::write::IO_BUF_BYTES * 2 + 17;
            let script = [Step::Errno(libc::EINTR), Step::Errno(errno)];
            let (result, written, bytes, calls) = run(&script, len + 50, 50, len as u64);
            result.unwrap_or_else(|e| panic!("errno {errno}: {e}"));
            assert_eq!(written, bytes[50..], "errno {errno}");
            assert_eq!(calls, 2, "errno {errno}: no kernel call after the fallback");
            assert!(
                WRITER_METRICS.copy_range_fallback_ranges.load(Relaxed) > before,
                "errno {errno}: the fallback must be counted"
            );
        }
    }

    #[test]
    fn a_refusal_after_progress_is_a_hard_error() {
        let script = [Step::Copy(10), Step::Errno(libc::EXDEV)];
        let (result, written, _, _) = run(&script, 1_000, 0, 1_000);
        let err = result.unwrap_err();
        assert_eq!(errno_of(&err), Some(libc::EXDEV), "{err}");
        assert_eq!(written.len(), 10);
    }

    #[test]
    fn other_errnos_are_hard_errors() {
        for errno in [libc::EBADF, libc::EIO, libc::EFBIG, libc::EOVERFLOW] {
            let (result, written, _, _) = run(&[Step::Errno(errno)], 1_000, 0, 1_000);
            let err = result.unwrap_err();
            assert_eq!(errno_of(&err), Some(errno), "{err}");
            assert!(written.is_empty());
        }
    }

    #[test]
    fn a_zero_copy_is_end_of_file() {
        let (result, _, _, _) = run(&[Step::Copy(0)], 1_000, 0, 1_000);
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof),
            "{err}"
        );
    }

    #[test]
    fn a_fallback_past_the_end_of_input_is_end_of_file() {
        // The index claims more bytes than the input has: the fallback reads
        // what there is, then reports the truncation.
        let (result, written, bytes, _) = run(&[Step::Errno(libc::EXDEV)], 1_000, 0, 5_000);
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof),
            "{err}"
        );
        assert_eq!(written, bytes);
    }

    #[test]
    fn the_guard_refuses_one_file_unrepresentable_ranges_and_append() {
        let dir = crate::path_guard::tests::scratch();
        let (input, _) = patterned_input(dir.path(), 64);
        let in_fd = input.file().as_raw_fd();
        let out = std::fs::File::create(dir.path().join("out.bin")).unwrap();
        let out_fd = out.as_raw_fd();
        let einval = io::Error::from_raw_os_error(libc::EINVAL);
        let enosys = io::Error::from_raw_os_error(libc::ENOSYS);

        assert!(kernel_copy_refused(&einval, in_fd, out_fd, 0, None, 64));
        assert!(kernel_copy_refused(&einval, in_fd, out_fd, 0, Some(8), 64));
        // The same file on both ends, whatever the errno.
        let again = std::fs::File::open(input.path()).unwrap();
        assert!(!kernel_copy_refused(
            &enosys,
            in_fd,
            again.as_raw_fd(),
            0,
            None,
            64
        ));
        // Ranges the syscall cannot represent.
        let max = i64::MAX.cast_unsigned();
        assert!(!kernel_copy_refused(&einval, in_fd, out_fd, max, None, 1));
        assert!(!kernel_copy_refused(
            &einval,
            in_fd,
            out_fd,
            u64::MAX,
            None,
            1
        ));
        assert!(!kernel_copy_refused(
            &einval,
            in_fd,
            out_fd,
            0,
            Some(max),
            1
        ));
        assert!(kernel_copy_refused(
            &einval,
            in_fd,
            out_fd,
            max - 1,
            None,
            1
        ));
        // An appending output.
        let appending = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("out.bin"))
            .unwrap();
        assert!(!kernel_copy_refused(
            &enosys,
            in_fd,
            appending.as_raw_fd(),
            0,
            None,
            64
        ));
        // A closed descriptor fails closed.
        assert!(!kernel_copy_refused(&enosys, in_fd, -1, 0, None, 64));
        // Errnos outside the set.
        let ebadf = io::Error::from_raw_os_error(libc::EBADF);
        assert!(!kernel_copy_refused(&ebadf, in_fd, out_fd, 0, None, 64));
    }

    #[test]
    fn retry_eintr_returns_short_counts_and_other_errors() {
        let mut answers = VecDeque::from([
            Err(io::Error::from_raw_os_error(libc::EINTR)),
            Ok(3),
            Err(io::Error::from_raw_os_error(libc::EIO)),
        ]);
        let mut next = || answers.pop_front().unwrap();
        assert_eq!(retry_eintr(&mut next).unwrap(), 3);
        assert_eq!(
            retry_eintr(&mut next).unwrap_err().raw_os_error(),
            Some(libc::EIO)
        );
    }
}
