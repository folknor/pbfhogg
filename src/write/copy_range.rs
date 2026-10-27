//! The crate's range copies: the one `copy_file_range` loop
//! ([`copy_range`]) and the one positioned-read loop ([`read_range`]) that
//! every passthrough write goes through.
//!
//! Lets the merge/passthrough write path hand raw blob bytes from an input fd
//! straight into the output fd without a userspace round-trip. On filesystems
//! with reflink support (btrfs, xfs), `copy_file_range` can be a metadata-only
//! operation.
//!
//! Every backend that splices uses these two loops and nothing else:
//!
//! - the buffered sink and the unpipelined `PbfWriter::write_raw_copy` call
//!   [`copy_range`] with [`CopyOut::Cursor`] (the output fd's position,
//!   which the copy advances);
//! - the `pwrite` pool calls it with [`CopyOut::At`] (an explicit offset; the
//!   fd's position is never touched, so concurrent workers cannot race on
//!   it);
//! - the io_uring writer reads a range into its registered buffers through
//!   [`read_range`] with itself as the [`ReadSink`], with no bounce buffer.
//!
//! When the kernel refuses the copy itself for this pair of files (see
//! [`kernel_copy_refused`] for the errno set and why it is safe), the range
//! falls back to [`read_range`] with a bounce buffer that writes the output
//! the same way the kernel copy would have (`write_all` at the cursor, or
//! `write_all_at` the explicit offset). That fallback is a performance cliff
//! at planet scale, so every range routed to it is counted
//! (`writer_copy_range_fallback_ranges` / `_bytes`), the first refusing errno
//! is kept, and the CLI turns them into one note per run
//! (`report_writer_degradations`).
//!
//! The module compiles in every build, and every backend that can splice
//! does so in every build: splicing used to need the `linux-direct-io`
//! feature, a leftover from when `libc` was optional, though it has nothing
//! to do with `O_DIRECT`. The one platform split is inside [`copy_range`]:
//! Linux asks the kernel first; elsewhere there is no `copy_file_range` to
//! ask, so the range goes straight to the userspace copy. That is the
//! platform's copy, not a refusal, so it is not counted or noted - but it is
//! only taken when [`bounce_safe`] holds, and otherwise refused, since there
//! is no kernel to have checked the pair first. Off Linux is not built or
//! tested by any sweep here.
//!
//! The shared rules, owned here so no second loop can drift from them:
//!
//! - **`EINTR` is retried, immediately, everywhere** ([`retry_eintr`]): a
//!   signal that interrupted a call moved nothing, so the same call is made
//!   again. No backoff. There is no cancellation check on the retry: library
//!   cancellation is a token checked between calls, not a signal, and the CLI
//!   blocks its signals in every thread, so nothing it does produces `EINTR`.
//! - **A short read or copy is progress, not an error.** Each loop advances
//!   by what the call returned; only zero bytes is end of file, which inside
//!   an indexed range means the input is shorter than its index says.
//! - **Cancellation is checked before every call**: a coalesced passthrough
//!   run can span the whole input (sort of an already-sorted planet is one
//!   range), so this is where a cancelled run stops splicing.
//! - **Lengths convert with [`len_to_usize`]**, saturating. The kernel and
//!   every sink cap the per-call size anyway, so a saturated request is only
//!   ever a smaller first call. The kernel copy asks for the whole remaining
//!   range every call (the kernel caps it at about 2 GiB); it is never
//!   chunked here.
//!
//! `clippy.toml` bans the raw byte-moving libc calls (`copy_file_range`,
//! `pread`, `pwrite`, `read`, `write` and their kin) everywhere in the
//! library; the one `copy_file_range` below is the only exemption. That
//! enforces who makes the syscalls, not that no other byte loop can be
//! written: std's `FileExt::read_at` stays allowed, because the read path
//! uses it legitimately. A new passthrough copy belongs here.
//!
//! Errors name what they can: a failed `copy_file_range` names the input and
//! the byte range (the syscall does not say which end failed; the writer adds
//! the output's name outside it), a zero-byte copy inside the range and a
//! failed or short-ended read are located in the input at the offset where
//! they stopped, and a failed fallback write is left to the writer, which
//! names the output.

use std::fs::File;
use std::io::{self, Write as _};
use std::os::unix::fs::FileExt as _;
use std::os::unix::io::{AsRawFd as _, RawFd};

use crate::error::{Error, Location, Result};
use crate::read::input::InputFile;
use crate::write::metrics::WRITER_METRICS;

/// Where [`copy_range`] writes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CopyOut<'a> {
    /// At the file's position, which the copy advances. The caller must own
    /// that position for the whole copy: a buffered writer is flushed first
    /// (`FileWriter::flush_and_file`), and nothing else writes through the
    /// descriptor meanwhile.
    Cursor(&'a File),
    /// At this explicit offset. The file's position is never read or moved,
    /// so concurrent copies into one file at disjoint offsets are safe.
    At(&'a File, u64),
}

impl CopyOut<'_> {
    fn file(&self) -> &File {
        match self {
            CopyOut::Cursor(file) | CopyOut::At(file, _) => file,
        }
    }

    fn offset(&self) -> Option<u64> {
        match self {
            CopyOut::Cursor(_) => None,
            CopyOut::At(_, offset) => Some(*offset),
        }
    }
}

/// A raw `ssize_t` syscall return as a result: the count, or the thread's
/// `errno`. Read `errno` nowhere else - it must be taken before any other
/// call can overwrite it.
#[cfg(target_os = "linux")]
#[inline(always)]
fn syscall_result(ret: isize) -> io::Result<usize> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret.cast_unsigned())
    }
}

/// Run one syscall attempt until it is not interrupted. The shared `EINTR`
/// rule: retry at once, nothing else. Every other outcome, including a short
/// count, goes back to the caller.
#[inline(always)]
fn retry_eintr(mut call: impl FnMut() -> io::Result<usize>) -> io::Result<usize> {
    loop {
        match call() {
            Err(err) if err.raw_os_error() == Some(libc::EINTR) => {}
            other => return other,
        }
    }
}

/// A byte count as a per-call length, saturating where `usize` is narrower.
/// Every caller clamps the result against its own per-call cap (the kernel's
/// for `copy_file_range`, a sink's for a read).
#[inline(always)]
fn len_to_usize(len: u64) -> usize {
    usize::try_from(len).unwrap_or(usize::MAX)
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
/// arguments. They are [`bounce_safe`]'s; each refuses the fallback (the
/// caller reports the original error).
///
/// `EBADF`, `EFBIG`, `EOVERFLOW`, `EIO` and everything else stay hard errors.
#[cfg(target_os = "linux")]
#[cold]
fn kernel_copy_refused(
    err: &io::Error,
    in_fd: RawFd,
    out_fd: RawFd,
    src_offset: u64,
    out_offset: Option<u64>,
    len: u64,
) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EXDEV | libc::ENOSYS | libc::EOPNOTSUPP | libc::EPERM | libc::EINVAL)
    ) && bounce_safe(in_fd, out_fd, src_offset, out_offset, len)
}

/// Whether the userspace copy may move `len` bytes from `in_fd` at
/// `src_offset` to `out_fd` (at `out_offset`, or at its file position when
/// `None`). Each guard fails closed when it cannot tell:
///
/// - the range must end at or below `i64::MAX` on both sides that carry an
///   explicit offset (computed with `checked_add`);
/// - the two fds must be different files (`fstat` `st_dev`/`st_ino`): a
///   forward chunked copy within one file corrupts overlapping ranges;
/// - `out_fd` must not be `O_APPEND`, where `pwrite` ignores its offset on
///   Linux. No writer here opens one, and a caller that flips the flag on a
///   descriptor it handed over mid-copy is outside what any check can see.
///
/// Stricter than the kernel, which accepts disjoint ranges of one file; the
/// fallback has no such check of its own, so it takes none of them.
fn bounce_safe(
    in_fd: RawFd,
    out_fd: RawFd,
    src_offset: u64,
    out_offset: Option<u64>,
    len: u64,
) -> bool {
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
/// has still been routed this many), and keep the first refusing errno for
/// the run's note. Cold: the kernel path never gets here.
#[cfg(target_os = "linux")]
#[cold]
fn record_fallback(len: u64, err: &io::Error) {
    use std::sync::atomic::Ordering::Relaxed;
    WRITER_METRICS
        .copy_range_fallback_ranges
        .fetch_add(1, Relaxed);
    WRITER_METRICS
        .copy_range_fallback_bytes
        .fetch_add(len, Relaxed);
    if let Some(errno) = err.raw_os_error() {
        // First refusal wins; a later one (another worker, another output)
        // may have a different errno, which the note does not claim to cover.
        // `Err` is "an earlier refusal is already kept", which is the rule.
        WRITER_METRICS
            .copy_range_fallback_errno
            .compare_exchange(0, errno, Relaxed, Relaxed)
            .ok();
    }
}

/// The symbolic name of an errno [`kernel_copy_refused`] accepts, for the
/// run's note; `None` for anything else.
pub(crate) fn refusal_name(errno: i32) -> Option<&'static str> {
    Some(match errno {
        libc::EXDEV => "EXDEV",
        libc::ENOSYS => "ENOSYS",
        libc::EOPNOTSUPP => "EOPNOTSUPP",
        libc::EPERM => "EPERM",
        libc::EINVAL => "EINVAL",
        _ => return None,
    })
}

/// A failed `copy_file_range` of `start..end` from `input`.
#[cold]
fn copy_error(err: io::Error, input: &InputFile, start: u64, end: u64) -> Error {
    Error::from(err).context(format!(
        "failed to copy bytes {start}..{end} of {}",
        input.path().display()
    ))
}

/// Copy `len` bytes from `input` at `offset` to `out` using
/// `copy_file_range(2)`, falling back to userspace where the kernel refuses
/// (see the module docs).
///
/// Uses an explicit input offset (does not change the input's file
/// position), safe when the input is also read through a `BufReader` or
/// `DirectReader`. The output is written as [`CopyOut`] says. A cancelled
/// run stops between kernel calls with `ErrorKind::Cancelled`.
///
/// Off Linux there is no kernel copy: the range is copied in userspace when
/// [`bounce_safe`] holds and refused otherwise (see the module docs).
#[cfg(target_os = "linux")]
pub(crate) fn copy_range(
    input: &InputFile,
    out: CopyOut<'_>,
    offset: u64,
    len: u64,
    cancel: &crate::host::Invocation,
) -> Result<()> {
    let in_fd = input.file().as_raw_fd();
    let out_fd = out.file().as_raw_fd();
    copy_range_with(input, out, offset, len, cancel, |off_in, off_out, chunk| {
        let off_out = off_out.map_or(std::ptr::null_mut(), std::ptr::from_mut);
        // The crate's one `copy_file_range`: std has no wrapper for it.
        #[allow(clippy::disallowed_methods)]
        // Safety: both fds are open for the borrow of `input` and `out`.
        // `off_in` is explicit, so the input's position never moves;
        // `off_out` is explicit for `CopyOut::At` (the output's position
        // never moves either) and NULL for `CopyOut::Cursor` (the kernel
        // writes at, and advances, the output's position).
        let ret = unsafe { libc::copy_file_range(in_fd, off_in, out_fd, off_out, chunk, 0) };
        syscall_result(ret)
    })
}

/// [`copy_range`] off Linux: the userspace copy, when it is safe.
#[cfg(not(target_os = "linux"))]
pub(crate) fn copy_range(
    input: &InputFile,
    out: CopyOut<'_>,
    offset: u64,
    len: u64,
    cancel: &crate::host::Invocation,
) -> Result<()> {
    let in_fd = input.file().as_raw_fd();
    let out_fd = out.file().as_raw_fd();
    if !bounce_safe(in_fd, out_fd, offset, out.offset(), len) {
        return Err(copy_error(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot establish that a userspace range copy is safe for this pair of files",
            ),
            input,
            offset,
            offset.saturating_add(len),
        ));
    }
    copy_range_fallback(input, out, offset, len, cancel)
}

/// The [`copy_range`] loop over one `copy_file_range` attempt,
/// `cfr(off_in, off_out, len)` (`off_out` is `None` for a cursor copy).
/// Production passes the syscall; tests pass a fake that scripts the
/// kernel's answers.
#[cfg(target_os = "linux")]
#[allow(clippy::cast_possible_wrap)]
#[inline(always)]
fn copy_range_with(
    input: &InputFile,
    out: CopyOut<'_>,
    mut offset: u64,
    mut len: u64,
    cancel: &crate::host::Invocation,
    mut cfr: impl FnMut(&mut i64, Option<&mut i64>, usize) -> io::Result<usize>,
) -> Result<()> {
    let (start, end) = (offset, offset.saturating_add(len));
    let mut out_at = out.offset();
    while len > 0 {
        cancel.check()?;
        let mut off_in = offset as i64;
        let mut off_out = out_at.map(|at| at as i64);
        let chunk = len_to_usize(len);
        let n = match retry_eintr(|| cfr(&mut off_in, off_out.as_mut(), chunk)) {
            Ok(n) => n,
            Err(err) => {
                // Decide on the original syscall error, before any
                // conversion, and only while nothing of the range has moved
                // (so `out` still names where the range starts).
                if offset == start
                    && kernel_copy_refused(
                        &err,
                        input.file().as_raw_fd(),
                        out.file().as_raw_fd(),
                        offset,
                        out_at,
                        len,
                    )
                {
                    record_fallback(len, &err);
                    return copy_range_fallback(input, out, offset, len, cancel);
                }
                return Err(copy_error(err, input, start, end));
            }
        };
        if n == 0 {
            // Zero bytes inside the range is the input ending early - the
            // one kernel answer that does say which end failed - so it is
            // located in the input at the first byte not copied, as
            // `read_range` locates a zero-byte read. Other syscall errors
            // stay unlocated: they do not establish which file failed.
            return Err(copy_error(
                io::Error::new(io::ErrorKind::UnexpectedEof, "copy_file_range returned 0"),
                input,
                start,
                end,
            )
            .locate(Location::at_offset(input.path(), offset)));
        }
        let n = n as u64;
        offset += n;
        len -= n;
        if let Some(at) = &mut out_at {
            *at += n;
        }
    }
    Ok(())
}

/// Where [`read_range`] puts the bytes it reads.
///
/// The division of checks: [`read_range`] enforces the reservation protocol
/// (it refuses an empty or oversized reservation before reading, and only
/// ever reports a fill no larger than the reservation it just got); an
/// implementation that hands out memory it does not own as a slice (the
/// io_uring writer's registered buffers) still bounds-checks `filled`
/// against its own capacity, so a protocol slip cannot corrupt state that
/// later feeds pointer arithmetic.
pub(crate) trait ReadSink {
    /// Writable space for the next read: between 1 and `want` bytes
    /// (`want` is never zero).
    fn space(&mut self, want: usize) -> Result<&mut [u8]>;
    /// The first `n` bytes of the space last returned now hold input bytes.
    fn filled(&mut self, n: usize) -> Result<()>;
}

/// Read `len` bytes of `input` at `offset` into `sink`, one positioned read
/// per reservation.
///
/// A short read is progress; a read of zero bytes inside the range is the
/// input ending early, located in the input at that offset. The sink decides
/// what happens to the bytes and when (the io_uring writer submits only full
/// buffers; nothing here flushes a partial one).
pub(crate) fn read_range(
    input: &InputFile,
    mut offset: u64,
    mut len: u64,
    cancel: &crate::host::Invocation,
    sink: &mut impl ReadSink,
) -> Result<()> {
    let file = input.file();
    while len > 0 {
        cancel.check()?;
        let want = len_to_usize(len);
        let dst = sink.space(want)?;
        if dst.is_empty() || dst.len() > want {
            return Err(Error::internal(
                "range read sink reserved an empty or oversized buffer",
            ));
        }
        // std's `read_at` is one `pread`; the retry is the shared rule.
        let n = retry_eintr(|| file.read_at(dst, offset))
            .map_err(|err| read_error(err, input, offset))?;
        if n == 0 {
            return Err(read_error(
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "input ended inside the requested copy range",
                ),
                input,
                offset,
            ));
        }
        sink.filled(n)?;
        offset += n as u64;
        len -= n as u64;
    }
    Ok(())
}

/// The userspace fallback's sink: a bounce buffer written to the output the
/// way the kernel copy would have.
struct Bounce<'a> {
    buf: Vec<u8>,
    out: CopyOut<'a>,
}

impl ReadSink for Bounce<'_> {
    fn space(&mut self, want: usize) -> Result<&mut [u8]> {
        let n = self.buf.len().min(want);
        Ok(&mut self.buf[..n])
    }

    fn filled(&mut self, n: usize) -> Result<()> {
        let bytes = self.buf.get(..n).ok_or_else(|| {
            Error::internal("range read filled more than the bounce buffer holds")
        })?;
        // Both retry `Interrupted` themselves, which is the shared rule.
        match &mut self.out {
            CopyOut::Cursor(file) => file.write_all(bytes)?,
            CopyOut::At(file, at) => {
                file.write_all_at(bytes, *at)?;
                *at += n as u64;
            }
        }
        Ok(())
    }
}

/// Userspace fallback when the kernel refuses the copy: [`read_range`] into
/// a bounce buffer that writes `out` at its cursor or its explicit offset,
/// as the kernel copy would have. An `At` fallback is as safe under
/// concurrent pool workers as the kernel copy it replaces.
fn copy_range_fallback(
    input: &InputFile,
    out: CopyOut<'_>,
    offset: u64,
    len: u64,
    cancel: &crate::host::Invocation,
) -> Result<()> {
    let mut sink = Bounce {
        buf: vec![0u8; crate::write::IO_BUF_BYTES],
        out,
    };
    read_range(input, offset, len, cancel, &mut sink)
}

/// A failed read of `input` at `offset` during a range copy.
#[cold]
fn read_error(err: io::Error, input: &InputFile, offset: u64) -> Error {
    Error::from(err).locate(Location::at_offset(input.path(), offset))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use std::collections::VecDeque;
    use std::path::Path;
    #[cfg(target_os = "linux")]
    use std::sync::atomic::Ordering::Relaxed;

    // The fake-kernel tests below drive `copy_range_with`, the Linux loop;
    // the read loop, the bounce guards and `retry_eintr` are tested on every
    // platform.

    /// One scripted answer from a fake `copy_file_range`.
    #[cfg(target_os = "linux")]
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// Copy up to this many bytes for real, like the kernel would.
        Copy(usize),
        /// Fail with this errno, copying nothing.
        Errno(i32),
    }

    /// An input of `len` patterned bytes in `dir`.
    fn patterned_input(dir: &Path, len: usize) -> (InputFile, Vec<u8>) {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
        let path = dir.join("in.bin");
        std::fs::write(&path, &bytes).unwrap();
        (InputFile::open(&path).unwrap(), bytes)
    }

    /// Run the production [`copy_range`] loop against `script`, copying
    /// `offset..offset + len` of a fresh input into a fresh output that
    /// already holds `out_at` bytes of 0xAA. `explicit` copies with
    /// [`CopyOut::At`] at `out_at`; otherwise with [`CopyOut::Cursor`] after
    /// seeking there. The fake has the kernel's effect on both offsets.
    #[cfg(target_os = "linux")]
    fn run(
        script: &[Step],
        input_len: usize,
        offset: u64,
        len: u64,
        out_at: u64,
        explicit: bool,
    ) -> (Result<()>, Vec<u8>, Vec<u8>, usize) {
        use std::io::{Seek as _, SeekFrom};
        let dir = crate::path_guard::tests::scratch();
        let (input, bytes) = patterned_input(dir.path(), input_len);
        let out_path = dir.path().join("out.bin");
        std::fs::write(&out_path, vec![0xAA; usize::try_from(out_at).unwrap()]).unwrap();
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .open(&out_path)
            .unwrap();
        if !explicit {
            out.seek(SeekFrom::Start(out_at)).unwrap();
        }
        let mut script: VecDeque<Step> = script.iter().copied().collect();
        let mut calls = 0;
        let target = if explicit {
            CopyOut::At(&out, out_at)
        } else {
            CopyOut::Cursor(&out)
        };
        let result = copy_range_with(
            &input,
            target,
            offset,
            len,
            &crate::host::Invocation::new(),
            |off_in, off_out, asked| {
                calls += 1;
                match script.pop_front().unwrap_or(Step::Copy(usize::MAX)) {
                    Step::Errno(errno) => Err(io::Error::from_raw_os_error(errno)),
                    Step::Copy(cap) => {
                        let mut buf = vec![0u8; asked.min(cap)];
                        let src = u64::try_from(*off_in).unwrap();
                        let n = input.file().read_at(&mut buf, src).unwrap();
                        *off_in += i64::try_from(n).unwrap();
                        match off_out {
                            Some(at) => {
                                let pos = u64::try_from(*at).unwrap();
                                out.write_all_at(&buf[..n], pos).unwrap();
                                *at += i64::try_from(n).unwrap();
                            }
                            // The implicit output position, as the kernel
                            // uses it.
                            None => (&out).write_all(&buf[..n]).unwrap(),
                        }
                        Ok(n)
                    }
                }
            },
        );
        if explicit {
            // Neither the kernel copy nor the pwrite fallback may touch the
            // cursor the pool's other workers share.
            assert_eq!(
                (&out).stream_position().unwrap(),
                0,
                "an explicit-offset copy moved the output cursor"
            );
        }
        drop(out);
        let written = std::fs::read(&out_path).unwrap();
        (result, written, bytes, calls)
    }

    #[cfg(target_os = "linux")]
    fn errno_of(err: &Error) -> Option<i32> {
        match err.kind() {
            ErrorKind::Io(e) => e.raw_os_error(),
            _ => None,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn eintr_is_retried_and_short_copies_advance() {
        let script = [
            Step::Errno(libc::EINTR),
            Step::Copy(1000),
            Step::Errno(libc::EINTR),
            Step::Errno(libc::EINTR),
            Step::Copy(3),
        ];
        for explicit in [false, true] {
            let (result, written, bytes, calls) = run(&script, 10_000, 100, 9_000, 7, explicit);
            result.unwrap();
            assert_eq!(&written[..7], &[0xAA; 7], "explicit {explicit}");
            assert_eq!(&written[7..], &bytes[100..9_100], "explicit {explicit}");
            assert_eq!(
                calls, 6,
                "four copies or retries, then the rest in one call"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn every_refusal_errno_falls_back_on_the_first_call() {
        for explicit in [false, true] {
            for errno in [
                libc::EXDEV,
                libc::ENOSYS,
                libc::EOPNOTSUPP,
                libc::EPERM,
                libc::EINVAL,
            ] {
                let before = WRITER_METRICS.copy_range_fallback_ranges.load(Relaxed);
                // EINTR first: the decision is on the first answer that is
                // not an interruption. The range is larger than the fallback
                // buffer, and lands after 5 bytes already in the output.
                let len = crate::write::IO_BUF_BYTES * 2 + 17;
                let script = [Step::Errno(libc::EINTR), Step::Errno(errno)];
                let (result, written, bytes, calls) =
                    run(&script, len + 50, 50, len as u64, 5, explicit);
                result.unwrap_or_else(|e| panic!("errno {errno}: {e}"));
                assert_eq!(&written[..5], &[0xAA; 5], "errno {errno}");
                assert_eq!(&written[5..], &bytes[50..], "errno {errno}");
                assert_eq!(calls, 2, "errno {errno}: no kernel call after the fallback");
                assert!(
                    WRITER_METRICS.copy_range_fallback_ranges.load(Relaxed) > before,
                    "errno {errno}: the fallback must be counted"
                );
                assert_ne!(
                    WRITER_METRICS.copy_range_fallback_errno.load(Relaxed),
                    0,
                    "errno {errno}: a refusing errno must be kept"
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_refusal_after_progress_is_a_hard_error() {
        for explicit in [false, true] {
            let script = [Step::Copy(10), Step::Errno(libc::EXDEV)];
            let (result, written, _, _) = run(&script, 1_000, 0, 1_000, 0, explicit);
            let err = result.unwrap_err();
            assert_eq!(errno_of(&err), Some(libc::EXDEV), "{err}");
            assert_eq!(written.len(), 10);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn other_errnos_are_hard_errors() {
        for errno in [libc::EBADF, libc::EIO, libc::EFBIG, libc::EOVERFLOW] {
            let (result, written, _, _) = run(&[Step::Errno(errno)], 1_000, 0, 1_000, 0, false);
            let err = result.unwrap_err();
            assert_eq!(errno_of(&err), Some(errno), "{err}");
            assert!(written.is_empty());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_zero_copy_is_end_of_file() {
        let (result, _, _, _) = run(&[Step::Copy(0)], 1_000, 0, 1_000, 0, true);
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof),
            "{err}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_fallback_past_the_end_of_input_is_end_of_file() {
        // The index claims more bytes than the input has: the fallback reads
        // what there is, then reports the truncation.
        let (result, written, bytes, _) =
            run(&[Step::Errno(libc::EXDEV)], 1_000, 0, 5_000, 0, false);
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof),
            "{err}"
        );
        assert_eq!(written, bytes);
    }

    /// A sink that reserves a scripted number of bytes per read and records
    /// what it was given.
    struct Scripted {
        sizes: VecDeque<usize>,
        buf: Vec<u8>,
        got: Vec<u8>,
    }

    impl ReadSink for Scripted {
        fn space(&mut self, want: usize) -> Result<&mut [u8]> {
            let n = self.sizes.pop_front().unwrap_or(want);
            self.buf.resize(n, 0);
            Ok(&mut self.buf[..n])
        }
        fn filled(&mut self, n: usize) -> Result<()> {
            self.got.extend_from_slice(&self.buf[..n]);
            Ok(())
        }
    }

    #[test]
    fn read_range_fills_small_reservations_in_order() {
        let dir = crate::path_guard::tests::scratch();
        let (input, bytes) = patterned_input(dir.path(), 1_000);
        let mut sink = Scripted {
            sizes: VecDeque::from([1, 7, 300]),
            buf: Vec::new(),
            got: Vec::new(),
        };
        read_range(&input, 10, 900, &crate::host::Invocation::new(), &mut sink).unwrap();
        assert_eq!(sink.got, bytes[10..910]);
    }

    #[test]
    fn read_range_refuses_an_empty_or_oversized_reservation() {
        let dir = crate::path_guard::tests::scratch();
        let (input, _) = patterned_input(dir.path(), 100);
        for size in [0, 51] {
            let mut sink = Scripted {
                sizes: VecDeque::from([size]),
                buf: Vec::new(),
                got: Vec::new(),
            };
            let err =
                read_range(&input, 0, 50, &crate::host::Invocation::new(), &mut sink).unwrap_err();
            assert!(
                matches!(err.kind(), ErrorKind::Internal { .. }),
                "size {size}: {err}"
            );
            assert!(sink.got.is_empty(), "size {size}: nothing may be read");
        }
    }

    #[test]
    fn the_guard_refuses_one_file_unrepresentable_ranges_and_append() {
        let dir = crate::path_guard::tests::scratch();
        let (input, _) = patterned_input(dir.path(), 64);
        let in_fd = input.file().as_raw_fd();
        let out = std::fs::File::create(dir.path().join("out.bin")).unwrap();
        let out_fd = out.as_raw_fd();

        assert!(bounce_safe(in_fd, out_fd, 0, None, 64));
        assert!(bounce_safe(in_fd, out_fd, 0, Some(8), 64));
        // The same file on both ends.
        let again = std::fs::File::open(input.path()).unwrap();
        assert!(!bounce_safe(in_fd, again.as_raw_fd(), 0, None, 64));
        // Ranges the syscall cannot represent.
        let max = i64::MAX.cast_unsigned();
        assert!(!bounce_safe(in_fd, out_fd, max, None, 1));
        assert!(!bounce_safe(in_fd, out_fd, u64::MAX, None, 1));
        assert!(!bounce_safe(in_fd, out_fd, 0, Some(max), 1));
        assert!(bounce_safe(in_fd, out_fd, max - 1, None, 1));
        // An appending output.
        let appending = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("out.bin"))
            .unwrap();
        assert!(!bounce_safe(in_fd, appending.as_raw_fd(), 0, None, 64));
        // A closed descriptor fails closed.
        assert!(!bounce_safe(in_fd, -1, 0, None, 64));
    }

    /// The kernel refusal is the errno set and the bounce guards together.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_kernel_refusal_needs_both_the_errno_and_the_guards() {
        let dir = crate::path_guard::tests::scratch();
        let (input, _) = patterned_input(dir.path(), 64);
        let in_fd = input.file().as_raw_fd();
        let out = std::fs::File::create(dir.path().join("out.bin")).unwrap();
        let out_fd = out.as_raw_fd();
        let einval = io::Error::from_raw_os_error(libc::EINVAL);
        let enosys = io::Error::from_raw_os_error(libc::ENOSYS);

        assert!(kernel_copy_refused(&einval, in_fd, out_fd, 0, None, 64));
        assert!(kernel_copy_refused(&enosys, in_fd, out_fd, 0, Some(8), 64));
        // The guards hold whatever the errno.
        let again = std::fs::File::open(input.path()).unwrap();
        assert!(!kernel_copy_refused(
            &enosys,
            in_fd,
            again.as_raw_fd(),
            0,
            None,
            64
        ));
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
