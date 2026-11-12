//! A fail-stop bound on a test whose defect would be a hang.
//!
//! A hung thread cannot be joined, and a test that panics past one leaves it
//! running under later tests, on a fixture it may already have lost. So on
//! expiry this ends the whole test binary instead: one line naming the test
//! goes to stderr, then `abort`. Nothing else survives it: every test child
//! carries a parent-death `SIGKILL` (`crates/lib/tests/common/process.rs`,
//! `isolate`), so the kernel ends the children whose owners `abort` skips.
//!
//! The bound covers the guard's lifetime and nothing else. Declare it first in
//! the test, before the resources and release guards of the scenario, so it is
//! dropped last and also bounds their cleanup joins, during unwinding too. Run
//! the operation on the test thread itself: the library joins the threads it
//! starts inside the operation, thread-local teardown included, so the bound
//! covers them; a thread the test itself detaches is not covered (the source
//! policy refuses a discarded `thread::spawn` handle). An explicit early
//! `drop`, `mem::forget` and the test thread's own teardown after the test
//! returns are outside it.

use std::io::Write;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Armed until dropped; see the module documentation.
#[must_use = "the bound lasts only while the guard is alive; bind it to a named variable"]
pub struct HangBound {
    cancel: Option<mpsc::Sender<()>>,
    watchdog: Option<JoinHandle<()>>,
}

impl HangBound {
    /// End the process unless this guard is dropped within `bound`. `grace`
    /// is how long the report may take to reach stderr before the process
    /// ends without it, so a blocked stderr cannot keep a hung binary alive.
    pub fn arm(bound: Duration, grace: Duration, label: &str) -> Self {
        let deadline = Instant::now() + bound;
        let test = std::thread::current()
            .name()
            .unwrap_or("<unnamed test thread>")
            .to_owned();
        let report =
            format!("\ntest {test}: {label}: no return within {bound:?}; ending the test binary\n");
        let (cancel, armed) = mpsc::channel::<()>();
        let watchdog = std::thread::Builder::new()
            .name(format!("hang-bound {test}"))
            .spawn(move || {
                let remaining = deadline.saturating_duration_since(Instant::now());
                // wait-deliberate: expiry is the hang this watchdog exists to end
                if let Err(RecvTimeoutError::Timeout) = armed.recv_timeout(remaining) {
                    expire(report, grace);
                }
            });
        let watchdog = match watchdog {
            Ok(handle) => handle,
            Err(error) => panic!("start the hang bound watchdog: {error}"),
        };
        Self {
            cancel: Some(cancel),
            watchdog: Some(watchdog),
        }
    }
}

/// Report on a thread of its own, so a stderr held by a hung thread or a full
/// pipe delays the end by at most `grace`, then abort.
fn expire(report: String, grace: Duration) -> ! {
    let (written, wait) = mpsc::channel::<()>();
    let reporter = std::thread::Builder::new()
        .name("hang-bound report".to_owned())
        .spawn(move || {
            let _written = std::io::stderr().write_all(report.as_bytes());
            let _sent = written.send(());
        });
    if reporter.is_ok() {
        let _reported = wait.recv_timeout(grace); // wait-deliberate: report grace before abort
    }
    std::process::abort()
}

impl Drop for HangBound {
    fn drop(&mut self) {
        // A send, or the sender dropping, both wake the watchdog before its
        // deadline; it then returns without acting.
        if let Some(cancel) = self.cancel.take() {
            let _sent = cancel.send(());
        }
        if let Some(watchdog) = self.watchdog.take()
            && watchdog.join().is_err()
            && !std::thread::panicking()
        {
            panic!("the hang bound watchdog panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HangBound;
    use std::time::Duration;

    const PROBE: &str = "PBFHOGG_HANG_BOUND_PROBE";

    /// The child side of the expiry test: a bound that expires over a test
    /// that never returns. Without the variable this is an empty test.
    #[test]
    fn expiry_probe() {
        if std::env::var_os(PROBE).is_none() {
            return;
        }
        let _bound = HangBound::arm(Duration::ZERO, Duration::from_secs(1), "probe label");
        loop {
            std::thread::park();
        }
    }

    /// Expiry ends the binary by abort, with the test and label on stderr.
    /// The child's own bound is what ends it, so the wait is bounded.
    #[test]
    fn an_expired_bound_aborts_the_binary_with_a_named_report() {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["hang::tests::expiry_probe", "--exact", "--nocapture"])
            .env(PROBE, "1")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run the probe");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(output.status.signal(), Some(6), "{:?}", output.status);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("hang::tests::expiry_probe: probe label: no return within"),
            "{stderr}"
        );
    }

    /// The bound the disarm tests arm: long enough that the drop always
    /// comes first. Nothing waits it out: `Drop` joins the watchdog, so a
    /// drop that returns has proved the bound can never fire.
    const DISARMED_BOUND: Duration = Duration::from_secs(10);

    #[test]
    fn a_dropped_bound_never_fires() {
        let bound = HangBound::arm(DISARMED_BOUND, Duration::ZERO, "quick");
        drop(bound);
    }

    #[test]
    fn a_bound_disarms_while_its_test_unwinds() {
        let outcome = std::panic::catch_unwind(|| {
            let _bound = HangBound::arm(DISARMED_BOUND, Duration::ZERO, "unwind");
            panic!("the operation failed");
        });
        assert!(outcome.is_err());
    }
}
