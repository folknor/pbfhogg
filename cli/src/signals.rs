//! SIGINT and SIGTERM: the CLI's interruption policy.
//!
//! The library never touches signals or other process state (ADR-0016); it
//! exposes a [`Cancel`] token. This module is the one caller that trips the
//! CLI's token, and the policy is:
//!
//! - **The first SIGINT or SIGTERM** requests cancellation. The run stops at
//!   its next checkpoint, its workers wind down, every guard drops (output
//!   staging and the run's scratch directory are removed, nothing is
//!   published), and `run` exits `128 + signo` - `130` for SIGINT, `143` for
//!   SIGTERM - after reporting the interruption.
//! - **A second signal** while that is under way exits at once with
//!   `128 + signo` of the second signal, skipping the cleanup still in
//!   progress. What it leaves is reported by the next run's leftover scan.
//!
//! Mechanism: both signals are blocked in the main thread before any other
//! thread exists, so every thread inherits the mask and no signal handler
//! ever runs; one dedicated thread takes them with `sigwait`. That thread
//! does nothing that can block: it records the signal, trips the token and
//! hands the one-line notice to a throwaway reporter thread (stderr can be a
//! full pipe, or held by another thread's write, and a waiter stuck there
//! could never take the second signal). The second signal is `_exit`, which
//! is safe from any thread and runs no destructor.
//!
//! pbfhogg starts no child processes, so the blocked mask (which `exec`
//! would hand down) reaches no other program.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, PoisonError};

use pbfhogg::host::Cancel;

/// The first signal taken, or 0. Written once, by the waiter, before it
/// trips the token (`Release` there); read by `run` after an `Acquire`
/// observation of the token.
static FIRST_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Set by `run` before it reports the outcome, so a reporter thread
/// scheduled late does not print a "stopping" notice after the run's final
/// line. A lock, not a flag: the reporter holds it across its check and its
/// print, so `finish` either sees the notice already printed or stops it from
/// printing at all. Only `run` and the reporter take it, never the waiter,
/// so a reporter stuck on a full stderr cannot keep the second signal from
/// being taken.
static FINISHED: Mutex<bool> = Mutex::new(false);

/// Block SIGINT and SIGTERM in the calling thread and start the waiter that
/// trips `cancel`. Call first thing in `run`, before any other thread is
/// spawned: a thread started earlier keeps the default disposition and would
/// be killed by the signal instead.
pub(crate) fn install(cancel: Cancel) -> std::io::Result<()> {
    let set = signal_set();
    // SAFETY: `set` is an initialised signal set; a null old-set pointer is
    // allowed.
    let rc =
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc));
    }
    let spawned = std::thread::Builder::new()
        .name("pbfhogg-signals".to_owned())
        .spawn(move || wait_loop(&set, &cancel));
    if let Err(e) = spawned {
        // Nobody would take the blocked signals: restore them so Ctrl-C
        // still ends the process (without cleanup, as before).
        // SAFETY: as above.
        unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const set, std::ptr::null_mut()) };
        return Err(e);
    }
    Ok(())
}

fn signal_set() -> libc::sigset_t {
    // SAFETY: `sigemptyset` initialises the zeroed set; adding two valid
    // signal numbers cannot fail.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, libc::SIGINT);
        libc::sigaddset(&raw mut set, libc::SIGTERM);
        set
    }
}

fn wait_loop(set: &libc::sigset_t, cancel: &Cancel) {
    loop {
        let mut signo: libc::c_int = 0;
        // SAFETY: `set` is initialised and `signo` is a valid out pointer.
        if unsafe { libc::sigwait(set, &raw mut signo) } != 0 {
            continue;
        }
        if FIRST_SIGNAL
            .compare_exchange(0, signo, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            cancel.cancel();
            report_stopping(signo);
        } else {
            // SAFETY: `_exit` ends the process without running destructors
            // or atexit handlers, which is the point: the first signal's
            // cleanup is what the operator asked to skip.
            unsafe { libc::_exit(128 + signo) };
        }
    }
}

/// Print the "stopping" notice from a thread of its own, so the waiter never
/// blocks on stderr. A failure to start it costs only the notice.
fn report_stopping(signo: libc::c_int) {
    drop(
        std::thread::Builder::new()
            .name("pbfhogg-signal-notice".to_owned())
            .spawn(move || {
                let finished = FINISHED.lock().unwrap_or_else(PoisonError::into_inner);
                if *finished {
                    return;
                }
                crate::output::cli_eprintln!(
                    "Note: {} received; stopping and removing this run's staging and scratch \
                     files. Send it again to exit at once and leave them for the next run to \
                     report.",
                    name(signo)
                );
            }),
    );
}

/// The first signal taken, once the caller has observed the token tripped.
pub(crate) fn first_signal(cancel: &Cancel) -> Option<libc::c_int> {
    if !cancel.is_cancelled() {
        return None;
    }
    match FIRST_SIGNAL.load(Ordering::Acquire) {
        0 => None,
        signo => Some(signo),
    }
}

/// `run` is about to report the outcome; a notice still queued is stale.
pub(crate) fn finish() {
    *FINISHED.lock().unwrap_or_else(PoisonError::into_inner) = true;
}

/// The exit status for a run cancelled by `signo`: `128 + signo`. A token
/// tripped by anything else (nothing else trips the CLI's) maps to SIGINT's.
pub(crate) fn exit_status(signo: Option<libc::c_int>) -> u8 {
    u8::try_from(128 + signo.unwrap_or(libc::SIGINT)).unwrap_or(130)
}

pub(crate) fn name(signo: libc::c_int) -> &'static str {
    match signo {
        libc::SIGINT => "SIGINT",
        libc::SIGTERM => "SIGTERM",
        _ => "a signal",
    }
}

/// Test hooks driven by the environment, compiled only with `test-hooks`,
/// which no product build enables. They park a run at a chosen checkpoint so
/// a test can signal it while it is provably mid-run.
#[cfg(feature = "test-hooks")]
pub(crate) mod test_hooks {
    use pbfhogg::host::Cancel;

    /// `PBFHOGG_TEST_CANCEL_PAUSE=<k>:<ready file>[:stuck]` parks the thread
    /// reaching the `k`-th checkpoint: it writes `<ready file>` and polls the
    /// token. Normally it then returns, and the checkpoint reports the
    /// cancellation. With `stuck` it writes `<ready file>.ack` once it has
    /// seen the token tripped and never returns, so the run cannot finish
    /// its cleanup and only a second signal ends it.
    ///
    /// `PBFHOGG_TEST_CANCEL_COUNT=<file>` counts checkpoints; [`write_count`]
    /// writes the total when the run ends.
    #[allow(
        clippy::disallowed_methods,
        reason = "test-only hook files, outside every output and scratch API"
    )]
    pub(crate) fn arm(cancel: &Cancel) {
        if std::env::var_os("PBFHOGG_TEST_CANCEL_COUNT").is_some() {
            cancel.on_check(|_, _| {});
            return;
        }
        let Some(spec) = std::env::var_os("PBFHOGG_TEST_CANCEL_PAUSE") else {
            return;
        };
        let spec = spec.to_string_lossy().into_owned();
        let mut parts = spec.splitn(3, ':');
        let (Some(k), Some(ready)) = (parts.next(), parts.next()) else {
            return;
        };
        let Ok(k) = k.parse::<u64>() else { return };
        let stuck = parts.next() == Some("stuck");
        let ready = std::path::PathBuf::from(ready);
        cancel.on_check(move |ordinal, token| {
            if ordinal != k {
                return;
            }
            drop(std::fs::write(&ready, ordinal.to_string()));
            while !token.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if stuck {
                let mut ack = ready.clone().into_os_string();
                ack.push(".ack");
                drop(std::fs::write(ack, b"cancelled"));
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        });
    }

    /// Write the checkpoint count for `PBFHOGG_TEST_CANCEL_COUNT`.
    #[allow(
        clippy::disallowed_methods,
        reason = "test-only hook file, outside every output and scratch API"
    )]
    pub(crate) fn write_count(cancel: &Cancel) {
        if let Some(path) = std::env::var_os("PBFHOGG_TEST_CANCEL_COUNT") {
            drop(std::fs::write(path, cancel.checks_passed().to_string()));
        }
    }
}
