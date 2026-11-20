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
//! The reader going away is marked on the error itself: a `BrokenPipe` from
//! stdout comes back as a `BrokenPipe` carrying a private [`StdoutGone`]
//! payload, so `run` can tell "the command stopped because stdout's reader
//! left" apart from a failure and end that run silently with success - the
//! convention of `head`-friendly tools. Only the error the command returned
//! is classified ([`crate::exit_code::is_stdout_gone`]), never the fact that
//! some stdout write failed at some point: a command that detects a truncated
//! input and then drops a buffered stdout writer flushes into the closed pipe
//! on the way out, and that cleanup failure must not erase the input error
//! the command actually stopped on. The marker survives the library's error
//! boundaries: `pbfhogg::Error` keeps a foreign `io::Error` whole in
//! `ErrorKind::Io`, and context does not change the kind.
//!
//! Any other stdout failure (a full disk behind `> file`, say) is not
//! swallowed: it propagates like any error and fails the command.
//!
//! It is also the one place the binary writes stderr, through
//! [`stderr_line`] and [`stderr_fragment`] (the `cli_eprintln!` and
//! `cli_eprint!` macros), so that a closed stderr stops the diagnostics
//! without ending the run. Its flag is kept apart from stdout's.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once a stdout write has failed with `BrokenPipe`. Never cleared: the
/// reader does not come back. Only the fast path reads it; whether a run
/// ended because the reader left is decided on the returned error's
/// [`StdoutGone`] marker, not on this flag.
static READER_GONE: AtomicBool = AtomicBool::new(false);

/// The payload of every `BrokenPipe` a stdout write returns: the proof that
/// this broken pipe is stdout's, not an input FIFO's or a child's.
#[derive(Debug)]
pub(crate) struct StdoutGone;

impl std::fmt::Display for StdoutGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stdout's reader went away")
    }
}

impl std::error::Error for StdoutGone {}

/// Whether `err` is a stdout write failing because its reader went away.
pub(crate) fn is_marked_gone(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::BrokenPipe
        && err.get_ref().is_some_and(<dyn std::error::Error + Send + Sync>::is::<StdoutGone>)
}

/// A stdout handle that reports a write failure instead of panicking, and
/// records the reader going away. Unbuffered; use [`buffered`].
pub(crate) struct PipeSafeStdout;

impl PipeSafeStdout {
    /// Pass `result` through, recording a broken pipe and marking its error
    /// as stdout's.
    fn observe<T>(result: io::Result<T>) -> io::Result<T> {
        match result {
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => {
                READER_GONE.store(true, Ordering::Relaxed);
                Err(Self::gone_error())
            }
            other => other,
        }
    }

    /// The marked error; once the reader is gone every further write fails
    /// this way without another syscall.
    fn gone_error() -> io::Error {
        io::Error::new(io::ErrorKind::BrokenPipe, StdoutGone)
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

fn reader_gone() -> bool {
    READER_GONE.load(Ordering::Relaxed)
}

/// For a command whose answer is decided before its output is written
/// (`check`, `inspect --indexed`, the final flush of a stdout `diff`): a
/// reader that stopped reading early must not erase the exit status that
/// answer carries, so the output step's failure is dropped when, and only
/// when, the error it returned is the reader going away. Any other error -
/// a value the report could not produce, with a buffered line flushed into
/// the closed pipe as the buffer dropped - keeps failing the command.
///
/// A streaming command cannot do this. `diff` writes as it compares, so when
/// the reader leaves there is no verdict yet, and the run ends with exit 0 -
/// under `set -o pipefail`, `pbfhogg diff a b | head` therefore reads as
/// "identical". That is deliberate: the alternative is to keep diffing into
/// nowhere to learn the verdict, which at planet scale holds `| head` open
/// for minutes. Pipe to a file, or use the exit status of an unpiped run,
/// when the verdict matters.
pub(crate) fn tolerate_reader_gone<E: Into<Box<dyn std::error::Error>>>(
    result: Result<(), E>,
) -> Result<(), Box<dyn std::error::Error>> {
    match result.map_err(Into::into) {
        Err(err) if crate::exit_code::is_stdout_gone(err.as_ref()) => Ok(()),
        other => other,
    }
}

/// Set once a stderr write has failed with `BrokenPipe`. Never cleared, and
/// deliberately separate from [`READER_GONE`]: losing the diagnostics reader
/// (`pbfhogg sort ... 2>&1 | head`) must not change how stdout or the exit
/// status are handled, so it never feeds [`reader_gone`] or
/// [`tolerate_reader_gone`].
static STDERR_READER_GONE: AtomicBool = AtomicBool::new(false);

/// Write one diagnostic line to stderr: the `eprintln!` equivalent.
///
/// `eprintln!` panics when stderr's reader has gone away, which used to abort
/// the whole command with status 101 halfway through its work. Here a closed
/// stderr stops the diagnostics and nothing else: the command completes and
/// its exit status is its own. Any other stderr failure still panics with
/// std's own message, as `eprintln!` would.
pub(crate) fn stderr_line(args: std::fmt::Arguments<'_>) {
    write_stderr(args, true);
}

/// The `eprint!` equivalent of [`stderr_line`]: no line terminator.
pub(crate) fn stderr_fragment(args: std::fmt::Arguments<'_>) {
    write_stderr(args, false);
}

/// Run `f` holding std's stderr lock, which every stderr write in the binary
/// takes. The lock is reentrant, so `f` may itself write stderr; every other
/// thread's stderr write waits until `f` returns. The signal reporter relies
/// on this to order its notice before the run's final line.
pub(crate) fn hold_stderr(f: &mut dyn FnMut()) {
    let _held = io::stderr().lock();
    f();
}

fn write_stderr(args: std::fmt::Arguments<'_>, newline: bool) {
    let mut handle = io::stderr().lock();
    if STDERR_READER_GONE.load(Ordering::Relaxed) {
        return;
    }
    let result = if newline {
        handle.write_fmt(format_args!("{args}\n"))
    } else {
        handle.write_fmt(args)
    };
    match result {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
            STDERR_READER_GONE.store(true, Ordering::Relaxed);
        }
        Err(e) => panic!("failed printing to stderr: {e}"),
    }
}

/// `eprintln!` that survives a closed stderr; see [`stderr_line`].
macro_rules! cli_eprintln {
    () => {
        $crate::output::stderr_line(format_args!(""))
    };
    ($($arg:tt)+) => {
        $crate::output::stderr_line(format_args!($($arg)+))
    };
}

/// `eprint!` that survives a closed stderr; see [`stderr_fragment`].
macro_rules! cli_eprint {
    ($($arg:tt)+) => {
        $crate::output::stderr_fragment(format_args!($($arg)+))
    };
}

pub(crate) use {cli_eprint, cli_eprintln};
