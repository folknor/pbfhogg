//! SIGINT and SIGTERM: the CLI's interruption policy.
//!
//! The library never touches signals or other process state (ADR-0016); it
//! exposes a [`Invocation`] token. This module is the one caller that trips the
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

use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};

use pbfhogg::host::Invocation;

/// The first signal taken, or 0. Written once, by the waiter, before it
/// trips the token (`Release` there); read by `run` after an `Acquire`
/// observation of the token.
static FIRST_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Decides between the "stopping" notice and `run`'s final report, so a
/// reporter thread scheduled late never prints after the run's final stderr
/// line. (Help and version text goes to stdout, which this cannot order: a
/// notice already printing may land after it.)
static NOTICE: NoticeGate = NoticeGate::new();

/// Who owns the stopping notice: nobody yet, the reporter (printing it), or
/// `run` (finished, so the notice is stale and must not print).
///
/// `finish` is a compare-and-swap and never waits. The ordering it does not
/// enforce itself rides on std's stderr lock, which every stderr write in the
/// binary takes (`output::write_stderr`, clap's `Error::print`): the reporter
/// acquires that lock *before* it claims the notice and keeps it through the
/// print. So if the reporter wins, every later stderr write - the run's final
/// line included - queues behind the notice; if `finish` wins, the reporter
/// prints nothing. `finish` therefore never blocks on the reporter; a final
/// stderr write may wait for the notice, as it would wait on a full pipe
/// anyway; a run that ends
/// with no stderr write at all (stdout's reader gone) exits even while the
/// notice is stuck on a full stderr pipe. An earlier design held a mutex
/// across the print that `finish` also took, which coupled completion to the
/// reporter's write; do not reintroduce a wait in `finish`. Only `run` and the
/// reporter touch the gate, never the waiter.
struct NoticeGate(AtomicU8);

const NOTICE_PENDING: u8 = 0;
const NOTICE_PRINTING: u8 = 1;
const NOTICE_FINISHED: u8 = 2;

impl NoticeGate {
    const fn new() -> Self {
        Self(AtomicU8::new(NOTICE_PENDING))
    }

    /// The reporter's claim; true means print. Call only while holding the
    /// output lock the final report also takes.
    fn claim(&self) -> bool {
        self.0
            .compare_exchange(
                NOTICE_PENDING,
                NOTICE_PRINTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// `run`'s claim. Never waits.
    fn finish(&self) {
        // Losing means the reporter is printing; nothing to do.
        _ = self.0.compare_exchange(
            NOTICE_PENDING,
            NOTICE_FINISHED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// The reporter's protocol, apart from the real stderr so it can be tested:
/// take the output lock through `hold_output`, then claim, then print under
/// the same lock. The claim must not move before the lock.
fn deliver(gate: &NoticeGate, hold_output: impl FnOnce(&mut dyn FnMut()), mut print: impl FnMut()) {
    hold_output(&mut || {
        if gate.claim() {
            print();
        }
    });
}

/// Block SIGINT and SIGTERM in the calling thread and start the waiter that
/// trips `cancel`. Call first thing in `run`, before any other thread is
/// spawned: a thread started earlier keeps the default disposition and would
/// be killed by the signal instead.
pub(crate) fn install(cancel: Invocation) -> std::io::Result<()> {
    let set = signal_set();
    // SAFETY: `set` is an initialised signal set; a null old-set pointer is
    // allowed.
    let rc =
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc));
    }
    #[allow(
        clippy::disallowed_methods,
        reason = "the process's signal waiter, which lives as long as the process"
    )]
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

fn wait_loop(set: &libc::sigset_t, cancel: &Invocation) {
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
#[allow(
    clippy::disallowed_methods,
    reason = "a detached process thread, so the signal waiter never blocks on stderr"
)]
fn report_stopping(signo: libc::c_int) {
    drop(
        std::thread::Builder::new()
            .name("pbfhogg-signal-notice".to_owned())
            .spawn(move || {
                deliver(&NOTICE, crate::output::hold_stderr, || {
                    crate::host::cli_note(
                        "cli.signal_stopping",
                        format_args!(
                            "{} received; stopping and removing this run's staging and scratch \
                             files. Send it again to exit at once and leave them for the next \
                             run to report.",
                            name(signo)
                        ),
                    );
                });
            }),
    );
}

/// The first signal taken, once the caller has observed the token tripped.
pub(crate) fn first_signal(cancel: &Invocation) -> Option<libc::c_int> {
    if !cancel.is_cancelled() {
        return None;
    }
    match FIRST_SIGNAL.load(Ordering::Acquire) {
        0 => None,
        signo => Some(signo),
    }
}

/// `run` is about to report the outcome; a notice still queued is stale.
/// Never waits, even for a notice stuck on a full stderr (see [`NoticeGate`]).
pub(crate) fn finish() {
    NOTICE.finish();
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[allow(
    clippy::disallowed_methods,
    reason = "std::thread::scope joins its two threads before each test returns"
)]
mod tests {
    use super::{NoticeGate, deliver};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    const BOUND: Duration = Duration::from_secs(10);

    /// The reporter's output lock in these tests: a log every writer appends to.
    fn hold<'a>(log: &'a Mutex<Vec<&'static str>>) -> impl FnOnce(&mut dyn FnMut()) + 'a {
        move |f: &mut dyn FnMut()| {
            let _held = log.lock().unwrap();
            f();
        }
    }

    /// The reporter is parked inside `hold_output`, just before it takes the
    /// lock, while the run finishes. Claiming only under the lock, it then
    /// sees the run finished and prints nothing; a claim moved ahead of the
    /// lock would already have won and would print.
    #[test]
    fn a_finished_run_suppresses_a_reporter_that_had_not_yet_taken_the_lock() {
        let gate = &NoticeGate::new();
        let log = &Mutex::new(Vec::new());
        let printed = &AtomicBool::new(false);
        let (arrived_tx, arrived_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        std::thread::scope(|s| {
            let reporter = s.spawn(move || {
                let hold_output = move |f: &mut dyn FnMut()| {
                    arrived_tx.send(()).unwrap();
                    go_rx.recv_timeout(BOUND).unwrap();
                    hold(log)(f);
                };
                deliver(gate, hold_output, || printed.store(true, Ordering::SeqCst));
            });
            arrived_rx.recv_timeout(BOUND).unwrap();
            gate.finish();
            go_tx.send(()).unwrap();
            reporter.join().unwrap();
        });
        assert!(
            !printed.load(Ordering::SeqCst),
            "the notice printed after the run finished"
        );
    }

    #[test]
    fn finish_does_not_wait_for_a_printing_notice_and_the_final_line_follows_it() {
        let gate = &NoticeGate::new();
        let log = &Mutex::new(Vec::new());
        let notice_printed = &AtomicBool::new(false);
        let (claimed_tx, claimed_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        std::thread::scope(|s| {
            let reporter = s.spawn(move || {
                deliver(gate, hold(log), || {
                    claimed_tx.send(()).unwrap();
                    release_rx.recv_timeout(BOUND).unwrap();
                    // The print, made while `hold` keeps the output lock.
                    notice_printed.store(true, Ordering::SeqCst);
                });
            });
            claimed_rx.recv_timeout(BOUND).unwrap();
            // The reporter holds the output lock and is mid-print: finish
            // must return now, not after the print.
            gate.finish();
            let writer = s.spawn(move || {
                let mut out = log.lock().unwrap();
                out.push(if notice_printed.load(Ordering::SeqCst) {
                    "final after notice"
                } else {
                    "final before notice"
                });
            });
            release_tx.send(()).unwrap();
            reporter.join().unwrap();
            writer.join().unwrap();
        });
        assert_eq!(*log.lock().unwrap(), ["final after notice"]);
    }
}

/// Test hooks driven by the environment, compiled only with `test-hooks`,
/// which no product build enables. They park a run at a chosen checkpoint so
/// a test can signal it while it is provably mid-run.
#[cfg(feature = "test-hooks")]
pub(crate) mod test_hooks {
    use pbfhogg::host::Invocation;

    fn fail(message: impl std::fmt::Display) -> ! {
        crate::host::cli_note("cli.test_hooks", format_args!("{message}"));
        std::process::exit(101);
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "test-only hook file, outside every output and scratch API"
    )]
    fn write_hook(path: &std::path::Path, contents: impl AsRef<[u8]>) {
        if let Err(error) = std::fs::write(path, contents) {
            fail(format_args!(
                "cannot write hook file {}: {error}",
                path.display()
            ));
        }
    }

    struct CancelPause {
        ordinal: u64,
        ready: std::path::PathBuf,
        stuck: bool,
    }

    impl CancelPause {
        fn parse(spec: &std::ffi::OsStr) -> Result<Self, &'static str> {
            let spec = spec.to_str().ok_or("cancel pause must be UTF-8")?;
            let mut parts = spec.splitn(3, ':');
            let ordinal = parts
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .ok_or("cancel pause needs a positive checkpoint")?;
            let ready = parts
                .next()
                .filter(|v| !v.is_empty())
                .ok_or("cancel pause needs a ready path")?;
            let stuck = match parts.next() {
                None => false,
                Some("stuck") => true,
                Some(_) => return Err("cancel pause suffix must be stuck"),
            };
            Ok(Self {
                ordinal,
                ready: ready.into(),
                stuck,
            })
        }
    }

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
    pub(crate) fn arm(cancel: &Invocation) {
        let count = std::env::var_os("PBFHOGG_TEST_CANCEL_COUNT");
        let pause = std::env::var_os("PBFHOGG_TEST_CANCEL_PAUSE");
        if count.is_some() && pause.is_some() {
            fail("cancel count and pause cannot be armed together");
        }
        if let Some(path) = count {
            if path.is_empty() {
                fail("cancel count needs a path");
            }
            cancel.on_check(|_, _| {});
            return;
        }
        let Some(spec) = pause else { return };
        let CancelPause {
            ordinal: k,
            ready,
            stuck,
        } = CancelPause::parse(&spec).unwrap_or_else(|error| fail(error));
        cancel.on_check(move |ordinal, token| {
            if ordinal != k {
                return;
            }
            write_hook(&ready, ordinal.to_string());
            while !token.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if stuck {
                let mut ack = ready.clone().into_os_string();
                ack.push(".ack");
                write_hook(std::path::Path::new(&ack), b"cancelled");
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        });
    }

    /// Arm publication's test hooks on the run's token. Publication is not
    /// cancellable, so the checkpoint pause above can never land inside it;
    /// these let a test stop a run at every state a publication passes
    /// through and kill it there:
    ///
    /// - `PBFHOGG_TEST_PUBLISH_FAULTS=<fault>[,<fault>...]` arms publication
    ///   faults, spelled as `pbfhogg::test_hooks::PublishFault` parses them
    ///   (`force-link-fallback`, `after-installed:1`, ...). A spelling that
    ///   does not parse ends the run with a message, so a test cannot
    ///   silently run unarmed.
    /// - `PBFHOGG_TEST_PUBLISH_STEPS=<file>` appends every publication step
    ///   to the file as it happens, one per line (`installed:0`), so a run
    ///   killed partway still leaves its trace. A trace or ready file that
    ///   cannot be written ends the run, so a short trace never passes for a
    ///   whole one.
    /// - `PBFHOGG_TEST_PUBLISH_PAUSE=<step>` with
    ///   `PBFHOGG_TEST_PUBLISH_READY=<file>` parks the publishing thread
    ///   after that step: it writes the ready file and never returns, so
    ///   only a kill ends the run.
    #[allow(
        clippy::disallowed_methods,
        reason = "test-only hook files, outside every output and scratch API"
    )]
    pub(crate) fn arm_publication(cancel: &Invocation) {
        // A trace or ready file that cannot be written ends the run: a test
        // reading a short trace would silently skip kill points, and one
        // waiting for a ready file would report an unreachable step.
        fn fail(what: &str, path: &std::path::Path, e: &std::io::Error) -> ! {
            crate::host::cli_note(
                "cli.test_hooks",
                format_args!("cannot write the {what} {}: {e}", path.display()),
            );
            std::process::exit(101);
        }
        use pbfhogg::test_hooks::{PublishFault, PublishStep};
        use std::io::Write;
        let faults = cancel.faults();
        if let Some(spec) = std::env::var_os("PBFHOGG_TEST_PUBLISH_FAULTS") {
            for name in spec.to_string_lossy().split(',').filter(|n| !n.is_empty()) {
                match name.parse::<PublishFault>() {
                    Ok(fault) => faults.arm_publish_fault(fault),
                    Err(e) => {
                        crate::host::cli_note("cli.test_hooks", format_args!("{e}"));
                        std::process::exit(101);
                    }
                }
            }
        }
        let steps = std::env::var_os("PBFHOGG_TEST_PUBLISH_STEPS").map(std::path::PathBuf::from);
        let pause = match std::env::var_os("PBFHOGG_TEST_PUBLISH_PAUSE") {
            Some(spec) => match spec.to_string_lossy().parse::<PublishStep>() {
                Ok(step) => Some(step),
                Err(e) => {
                    crate::host::cli_note("cli.test_hooks", format_args!("{e}"));
                    std::process::exit(101);
                }
            },
            None => None,
        };
        let ready = std::env::var_os("PBFHOGG_TEST_PUBLISH_READY").map(std::path::PathBuf::from);
        if steps.is_none() && pause.is_none() {
            return;
        }
        faults.on_publish_step(move |step| {
            if let Some(path) = &steps {
                let appended = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .and_then(|mut file| writeln!(file, "{step}"));
                if let Err(e) = appended {
                    fail("publication step trace", path, &e);
                }
            }
            if pause == Some(step) {
                if let Some(ready) = &ready
                    && let Err(e) = std::fs::write(ready, step.to_string())
                {
                    fail("publication pause ready file", ready, &e);
                }
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        });
    }

    /// Arm the library's input replacement fault on the run's token, so a
    /// test can rename a file over a pinned input between two opens of a CLI
    /// run that composes several library calls (see
    /// `pbfhogg::test_hooks::FaultPlan::arm_input_replacement`):
    /// `PBFHOGG_TEST_REPLACE_INPUT=<input as given>`,
    /// `PBFHOGG_TEST_REPLACE_WITH=<replacement>` and
    /// `PBFHOGG_TEST_REPLACE_AFTER=<n>` rename the replacement over the input
    /// after the `n`-th verified pinned open of it.
    /// `PBFHOGG_TEST_FORCE_PIN_PATH_REOPEN=1` makes every pin taken in the
    /// run reopen through the path. [`write_replacement_report`] writes what
    /// the replacement observed.
    pub(crate) fn arm_input_replacement(cancel: &Invocation) {
        let faults = cancel.faults();
        if let Some(value) = std::env::var_os("PBFHOGG_TEST_FORCE_PIN_PATH_REOPEN") {
            match value.to_str() {
                Some("1") => faults
                    .force_pin_path_reopen
                    .store(true, std::sync::atomic::Ordering::Relaxed),
                Some("0") => {}
                _ => fail("force pin path reopen must be 0 or 1"),
            }
        }
        let config = (
            std::env::var_os("PBFHOGG_TEST_REPLACE_INPUT"),
            std::env::var_os("PBFHOGG_TEST_REPLACE_WITH"),
            std::env::var_os("PBFHOGG_TEST_REPLACE_AFTER"),
        );
        let (input, with, after) = match config {
            (None, None, None) => {
                if std::env::var_os("PBFHOGG_TEST_REPLACE_REPORT").is_some() {
                    fail("replacement report requires an armed replacement");
                }
                return;
            }
            (Some(input), Some(with), Some(after)) if !input.is_empty() && !with.is_empty() => {
                (input, with, after)
            }
            _ => fail("replacement needs input, replacement path and positive open count"),
        };
        let after = after
            .to_str()
            .and_then(|a| a.parse::<u64>().ok())
            .filter(|a| *a > 0)
            .unwrap_or_else(|| fail("replacement open count must be positive"));
        faults.arm_input_replacement(
            std::path::Path::new(&input),
            std::path::Path::new(&with),
            after,
        );
    }

    /// `PBFHOGG_TEST_MERGE_CHANGES_SPILL_FIRST=<n>` arms the library's
    /// merge-changes spill-first handshake on the run's token
    /// (`pbfhogg::test_hooks::FaultPlan::merge_changes_spill_first`): the
    /// second input spills its first `n` members before the first input is
    /// read. A value that does not parse ends the run with a message, so a
    /// test cannot silently run unarmed.
    pub(crate) fn arm_merge_changes(cancel: &Invocation) {
        let Some(spec) = std::env::var_os("PBFHOGG_TEST_MERGE_CHANGES_SPILL_FIRST") else {
            return;
        };
        let Some(n) = spec.to_str().and_then(|n| n.parse::<usize>().ok()) else {
            crate::host::cli_note(
                "cli.test_hooks",
                format_args!("PBFHOGG_TEST_MERGE_CHANGES_SPILL_FIRST={spec:?} is not a count"),
            );
            std::process::exit(101);
        };
        cancel
            .faults()
            .merge_changes_spill_first
            .store(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// `PBFHOGG_TEST_REPLACE_REPORT=<file>`: write the armed replacement's
    /// observations as one line, `fired attempts verified refused
    /// attempts_after_fired` (`fired` as 0 or 1).
    #[allow(
        clippy::disallowed_methods,
        reason = "test-only hook file, outside every output and scratch API"
    )]
    pub(crate) fn write_replacement_report(cancel: &Invocation) {
        let Some(path) = std::env::var_os("PBFHOGG_TEST_REPLACE_REPORT") else {
            return;
        };
        let Some(r) = cancel.faults().input_replacement() else {
            fail("replacement report requested without an armed replacement");
        };
        write_hook(
            std::path::Path::new(&path),
            format!(
                "{} {} {} {} {}",
                u8::from(r.fired),
                r.attempts,
                r.verified,
                r.refused,
                r.attempts_after_fired
            ),
        );
    }

    /// Write the checkpoint count for `PBFHOGG_TEST_CANCEL_COUNT`.
    #[allow(
        clippy::disallowed_methods,
        reason = "test-only hook file, outside every output and scratch API"
    )]
    pub(crate) fn write_count(cancel: &Invocation) {
        if let Some(path) = std::env::var_os("PBFHOGG_TEST_CANCEL_COUNT") {
            write_hook(
                std::path::Path::new(&path),
                cancel.checks_passed().to_string(),
            );
        }
    }

    #[cfg(test)]
    #[allow(clippy::expect_used)]
    mod configuration_tests {
        use super::CancelPause;

        #[test]
        fn pause_requires_a_reachable_checkpoint_and_a_complete_protocol() {
            for spec in [
                "",
                "1",
                "no:ready",
                "0:ready",
                "1:",
                "1:ready:other",
                "1:ready:stuck:extra",
            ] {
                assert!(
                    CancelPause::parse(std::ffi::OsStr::new(spec)).is_err(),
                    "{spec}"
                );
            }
            let pause =
                CancelPause::parse(std::ffi::OsStr::new("2:ready:stuck")).expect("valid pause");
            assert_eq!(pause.ordinal, 2);
            assert_eq!(pause.ready, std::path::Path::new("ready"));
            assert!(pause.stuck);
            assert!(
                !CancelPause::parse(std::ffi::OsStr::new("1:ready"))
                    .expect("valid pause")
                    .stuck
            );
        }
    }
}
