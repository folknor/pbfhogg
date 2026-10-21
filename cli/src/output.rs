//! The one place this binary acquires its stdout.
//!
//! `print!` / `println!` panic when a write fails, and the commonest write
//! failure is not exotic at all: `pbfhogg inspect x | head -1` closes the
//! pipe after one line, the next write gets `EPIPE`, and the process dies with
//! "failed printing to stdout: Broken pipe" and exit 101. (Rust ignores
//! `SIGPIPE`, so the failure arrives as an `io::Error` rather than as the
//! signal that ends a C tool quietly.) Every stdout write in the binary
//! therefore goes through [`PipeSafeStdout`], which hands the error back as a
//! value, and every write site propagates it with `?` so the command stops
//! producing output it can no longer deliver.
//!
//! The reader going away is recorded here, process-wide, so `main` can tell
//! "stdout's reader left" apart from a failure and end the run silently with
//! success - the convention of `head`-friendly tools, and the same choice
//! `main` already makes for clap's `--help` / `--version` output, whose write
//! result it discards. The record is a flag rather than an inspection of the
//! returned error because the error crosses library boundaries that may wrap
//! or re-type it on the way up; the flag is set at the one place the `EPIPE`
//! is certainly still an `io::Error` from stdout.
//!
//! Any other stdout failure (a full disk behind `> file`, say) is not
//! swallowed: it propagates like any error and fails the command.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once a stdout write has failed with `BrokenPipe`. Never cleared: the
/// reader does not come back.
static READER_GONE: AtomicBool = AtomicBool::new(false);

/// A stdout handle that reports a write failure instead of panicking, and
/// records the reader going away. Unbuffered; use [`buffered`].
pub(crate) struct PipeSafeStdout;

impl PipeSafeStdout {
    /// Pass `result` through, recording a broken pipe on the way.
    fn observe<T>(result: io::Result<T>) -> io::Result<T> {
        if let Err(ref err) = result
            && err.kind() == io::ErrorKind::BrokenPipe
        {
            READER_GONE.store(true, Ordering::Relaxed);
        }
        result
    }

    /// Once the reader is gone every further write fails the same way, so
    /// answer without another syscall.
    fn gone_error() -> io::Error {
        io::Error::from(io::ErrorKind::BrokenPipe)
    }
}

impl Write for PipeSafeStdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if reader_gone() {
            return Err(Self::gone_error());
        }
        // std's stdout is line-buffered internally; flushing after each
        // write makes a failure surface on the call that caused it rather
        // than on some later one, or at process exit where std drops it.
        // Callers write through `buffered`, so this is one syscall per
        // buffer, not per line.
        let mut handle = io::stdout().lock();
        let result = match handle.write(buf) {
            Ok(written) => handle.flush().map(|()| written),
            Err(err) => Err(err),
        };
        Self::observe(result)
    }

    fn flush(&mut self) -> io::Result<()> {
        if reader_gone() {
            return Err(Self::gone_error());
        }
        Self::observe(io::stdout().lock().flush())
    }
}

/// The binary's stdout, buffered. Every stdout write in `pbfhogg` goes
/// through one of these. Call `flush()?` before returning success: a
/// `BufWriter` dropped with data still buffered flushes and discards the
/// error.
pub(crate) fn buffered() -> io::BufWriter<PipeSafeStdout> {
    io::BufWriter::new(PipeSafeStdout)
}

/// Whether a stdout write has failed because the reader went away.
pub(crate) fn reader_gone() -> bool {
    READER_GONE.load(Ordering::Relaxed)
}

/// For a command whose answer is decided before its output is written
/// (`check`, `inspect --indexed`): a reader that stopped reading early must
/// not erase the exit status that answer carries, so the output step's
/// failure is dropped when, and only when, it was the reader going away.
///
/// A streaming command cannot do this. `diff` writes as it compares, so when
/// the reader leaves there is no verdict yet, and the run ends with exit 0 -
/// under `set -o pipefail`, `pbfhogg diff a b | head` therefore reads as
/// "identical". That is deliberate: the alternative is to keep diffing into
/// nowhere to learn the verdict, which at planet scale holds `| head` open
/// for minutes. Pipe to a file, or use the exit status of an unpiped run,
/// when the verdict matters.
pub(crate) fn tolerate_reader_gone<E>(result: Result<(), E>) -> Result<(), E> {
    match result {
        Err(_) if reader_gone() => Ok(()),
        other => other,
    }
}
