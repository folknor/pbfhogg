//! Owned test children with cancellable, bounded pipe capture.
use pbfhogg::{
    TEST_CHILD_BUDGET, TEST_CHILD_CAPTURE_CHUNK, TEST_CHILD_CAPTURE_EDGE, TEST_CHILD_CLEANUP,
    TEST_CHILD_POLL,
};
use pbfhogg_test_scratch::TestScratch;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Deliberate inherited values only. Hook and instrumentation variables never
/// enter the child unless the caller sets them after this function.
///
/// Also the parent-death signal every test child carries: the kernel
/// `SIGKILL`s the child when the thread that spawned it ends, so a test
/// binary that dies without unwinding (an expired
/// `pbfhogg_test_scratch::HangBound` aborts it) leaves no child running on
/// its scratch. Call this right after `Command::new`, before any other
/// `pre_exec` hook, so the promise guard's filter stays the last hook. The
/// signal is keyed to the spawning thread, which is sound because every test
/// child is spawned, owned and reaped by its test's own thread. It reaches
/// the direct child only: a descendant does not inherit it, which the
/// guarded pbfhogg binary and the harness never start; the osmium oracle is
/// trusted not to leave one.
pub fn isolate(cmd: &mut Command, scratch: &TestScratch) {
    parent_death_kill(cmd);
    cmd.env_clear()
        .current_dir(scratch.path())
        .stdin(Stdio::null());
    for key in [
        "PATH",
        "HOME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "LC_MESSAGES",
        "TZ",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("TMPDIR", scratch.path());
}

/// Register the parent-death `SIGKILL` (see [`isolate`]). The parent PID is
/// read before the fork; a child whose parent already died between the fork
/// and the `prctl` has been reparented, sees another PID and exits at once.
fn parent_death_kill(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: getpid has no preconditions.
    let parent = unsafe { libc::getpid() };
    // SAFETY: the closure runs between fork and exec and calls only
    // async-signal-safe functions (`prctl`, `getppid`, `_exit`).
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                libc::_exit(127);
            }
            Ok(())
        });
    }
}

struct Capture {
    thread: JoinHandle<()>,
    result: std::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
}

fn capture(
    mut pipe: impl Read + AsRawFd + Send + 'static,
    stop: Arc<AtomicBool>,
) -> io::Result<Capture> {
    // SAFETY: fcntl operates on a live owned pipe descriptor. Only this reader
    // uses its file status flags; no borrowed descriptor outlives the pipe.
    unsafe {
        let flags = libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let (send, result) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let output = (|| {
            let mut first = Vec::new();
            let mut last = std::collections::VecDeque::new();
            let mut chunk = [0; TEST_CHILD_CAPTURE_CHUNK];
            let mut total = 0_usize;
            loop {
                if stop.load(Ordering::Acquire) {
                    return Err(io::Error::other("capture cancelled before EOF"));
                }
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        total = total.saturating_add(n);
                        let prefix = n.min(TEST_CHILD_CAPTURE_EDGE - first.len());
                        first.extend_from_slice(&chunk[..prefix]);
                        for byte in &chunk[prefix..n] {
                            if last.len() == TEST_CHILD_CAPTURE_EDGE {
                                last.pop_front();
                            }
                            last.push_back(*byte);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        let mut fd = libc::pollfd {
                            fd: pipe.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        let timeout = libc::c_int::try_from(TEST_CHILD_POLL.as_millis())
                            .expect("capture cancellation interval fits poll");
                        // SAFETY: poll borrows the live pipe for this call only.
                        if unsafe { libc::poll(&mut fd, 1, timeout) } < 0 {
                            let error = io::Error::last_os_error();
                            if error.kind() != io::ErrorKind::Interrupted {
                                return Err(error);
                            }
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            if total > TEST_CHILD_CAPTURE_EDGE * 2 {
                first.extend_from_slice(b"\n[output truncated; first and last MiB retained]\n");
            }
            first.extend(last);
            Ok(first)
        })();
        let _sent = send.send(output);
    });
    Ok(Capture { thread, result })
}

std::thread_local! {
    /// When this thread's children have spent `TEST_CHILD_BUDGET`, anchored
    /// at its first child (see [`children_budget`]).
    static BUDGET: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// The deadline every child spawned on this thread shares, so a test that
/// runs children in sequence fails naming the budget instead of running
/// into the runner's watchdog unnamed. It is anchored at the thread's first
/// child, not at test start, which nothing here can see: setup before that
/// child is outside it, and a test spending more than the margin
/// `TEST_RUNNER_WATCHDOG - TEST_CHILD_BUDGET - 3 * TEST_CHILD_CLEANUP` there
/// still meets the watchdog first, as before. A test whose children span
/// more than the budget fails here even if it would have ended under the
/// watchdog: fitting the budget is part of the contract. libtest runs every test on a
/// thread of its own named after the test, so the anchor is per test, except
/// where it cannot start that thread and runs the test on its runner thread,
/// `main`, shared by later tests: there no budget is anchored, and each child
/// keeps only its own timeout. A helper thread that spawns a child gets an
/// allowance of its own; the owner keeps the deadline it was spawned under
/// wherever it moves.
fn children_budget() -> Option<Instant> {
    if std::thread::current().name() == Some("main") {
        return None;
    }
    BUDGET.with(|budget| {
        let deadline = budget
            .get()
            .unwrap_or_else(|| Instant::now() + TEST_CHILD_BUDGET);
        budget.set(Some(deadline));
        Some(deadline)
    })
}

/// Spend this thread's children budget now, for the test that checks the
/// refusal without waiting the budget out.
#[allow(dead_code)]
pub fn spend_children_budget() {
    BUDGET.with(|budget| budget.set(Some(Instant::now())));
}

fn budget_spent() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "this test's children spent TEST_CHILD_BUDGET ({TEST_CHILD_BUDGET:?} from its first child)"
        ),
    )
}

/// The child and its pipe readers stay owned even if waiting or assertions fail.
pub struct OwnedChild<'a> {
    child: Child,
    _scratch: &'a TestScratch,
    stop: Arc<AtomicBool>,
    stdout: Option<Capture>,
    stderr: Option<Capture>,
    /// The spawning thread's [`children_budget`], kept with the child.
    budget: Option<Instant>,
    /// The exit status once any wait here has reaped the child. Every reap
    /// goes through [`Self::reaped`], and nothing else reaches the `Child`,
    /// so a recorded status is the one proof the PID is no longer ours.
    status: Option<ExitStatus>,
}

impl<'a> OwnedChild<'a> {
    /// Start `cmd`, refusing once this thread's children budget is spent: a
    /// test whose earlier children took it all fails here, even when every
    /// child so far ended in time.
    pub fn spawn(cmd: &mut Command, scratch: &'a TestScratch) -> io::Result<Self> {
        let budget = children_budget();
        if budget.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(budget_spent());
        }
        let child = cmd.spawn()?;
        let mut owner = Self {
            child,
            _scratch: scratch,
            stop: Arc::new(AtomicBool::new(false)),
            stdout: None,
            stderr: None,
            budget,
            status: None,
        };
        if let Some(pipe) = owner.child.stdout.take() {
            owner.stdout = Some(capture(pipe, Arc::clone(&owner.stop))?);
        }
        if let Some(pipe) = owner.child.stderr.take() {
            owner.stderr = Some(capture(pipe, Arc::clone(&owner.stop))?);
        }
        Ok(owner)
    }

    /// Check for an exit without blocking, reaping it and recording its status.
    fn reaped(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        Ok(self.status)
    }

    pub fn has_exited(&mut self) -> io::Result<bool> {
        Ok(self.reaped()?.is_some())
    }

    /// The deadline the spawning thread's children share, if one is anchored.
    #[allow(dead_code)]
    pub fn budget(&self) -> Option<Instant> {
        self.budget
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Send `signal`, refused once a wait here has reaped the child: its PID
    /// could by then name another process. An exited child not yet reaped
    /// still holds its PID, so the signal reaches only that zombie. This
    /// does not reap first, which would turn that harmless race into a
    /// refusal.
    pub fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        if self.status.is_some() {
            return Err(io::Error::other(
                "signal refused: the child has already been reaped",
            ));
        }
        let pid = libc::pid_t::try_from(self.child.id())
            .map_err(|_| io::Error::other("pid does not fit pid_t"))?;
        // SAFETY: the child is unreaped (no status recorded), so the PID is ours.
        if unsafe { libc::kill(pid, signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Wait at most `timeout`, and never past the thread's children budget;
    /// on expiry the child is killed and the error names which bound ran
    /// out. An exit already observed is returned whatever the time.
    pub fn wait(&mut self, timeout: Duration) -> io::Result<Output> {
        // A recorded status (has_exited() may already have reaped) must be
        // read before opening a pidfd: a reaped PID could name another process.
        let status = if let Some(status) = self.reaped()? {
            status
        } else {
            let budget = self
                .budget
                .map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let (limit, expired) = match budget {
                Some(left) if left < timeout => (left, budget_spent as fn() -> io::Error),
                _ => (timeout, child_timed_out as fn() -> io::Error),
            };
            let exit = ExitFd::open(&self.child)?;
            let status = wait_for_exit(
                &mut self.child,
                limit,
                |child, remaining| {
                    exit.block(remaining)?;
                    child.try_wait()
                },
                Child::kill,
                expired,
            )?;
            self.status = Some(status);
            status
        };
        Ok(Output {
            status,
            stdout: finish(&mut self.stdout)?,
            stderr: finish(&mut self.stderr)?,
        })
    }
}

fn child_timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "test child timed out")
}

/// Exit readiness for an unreaped child. A pidfd becomes readable the moment
/// the child exits, so completion costs no polling delay; where the kernel
/// lacks pidfd_open (before Linux 5.3) it degrades to sleeping one
/// `TEST_CHILD_POLL` interval per check.
struct ExitFd(Option<std::os::fd::OwnedFd>);

impl ExitFd {
    fn open(child: &Child) -> io::Result<Self> {
        let pid = libc::pid_t::try_from(child.id())
            .map_err(|_| io::Error::other("pid does not fit pid_t"))?;
        // SAFETY: the child is unreaped (the caller checked try_wait), so the
        // PID still names it; pidfd_open takes no pointers.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOSYS) {
                return Ok(Self(None));
            }
            return Err(error);
        }
        let fd = std::os::fd::RawFd::try_from(fd)
            .map_err(|_| io::Error::other("pidfd does not fit RawFd"))?;
        // SAFETY: pidfd_open returned a fresh descriptor that nothing else owns.
        Ok(Self(Some(unsafe {
            <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
        })))
    }

    /// Return when the child has exited or `limit` has elapsed.
    fn block(&self, limit: Duration) -> io::Result<()> {
        let Some(fd) = &self.0 else {
            // wait-deliberate: exit poll pace without a pidfd; the caller bounds it
            std::thread::sleep(TEST_CHILD_POLL.min(limit));
            return Ok(());
        };
        let millis = limit.as_nanos().div_ceil(1_000_000);
        let millis = libc::c_int::try_from(millis).unwrap_or(libc::c_int::MAX);
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll borrows the live pidfd for this call only.
        if unsafe { libc::poll(&mut poll, 1, millis) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(())
    }
}

/// Wait and terminate on the owning thread: `block` returns as soon as the
/// child exits (no polling delay), and the kill runs here rather than on a
/// watchdog, so a failed kill is returned immediately instead of leaving a
/// waiter blocked forever, and no other thread can race reaping and PID reuse.
fn wait_for_exit<S, T>(
    state: &mut S,
    timeout: Duration,
    mut block: impl FnMut(&mut S, Duration) -> io::Result<Option<T>>,
    kill: impl FnOnce(&mut S) -> io::Result<()>,
    expired: fn() -> io::Error,
) -> io::Result<T> {
    let start = Instant::now();
    loop {
        if let Some(status) = block(state, timeout.saturating_sub(start.elapsed()))? {
            return Ok(status);
        }
        if start.elapsed() >= timeout {
            kill(state)?;
            // OwnedChild::drop performs bounded reaping and pipe cleanup.
            return Err(expired());
        }
    }
}

fn finish(slot: &mut Option<Capture>) -> io::Result<Vec<u8>> {
    let Some(capture) = slot.as_ref() else {
        return Ok(Vec::new());
    };
    // wait-deliberate: expiry is returned as an error, never taken as the answer
    let output = match capture.result.recv_timeout(TEST_CHILD_CLEANUP) {
        Ok(output) => output,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "child pipe did not reach EOF",
            ));
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(io::Error::other("capture thread panicked"))
        }
    };
    if let Some(capture) = slot.take() {
        capture
            .thread
            .join()
            .map_err(|_| io::Error::other("capture thread panicked"))?;
    }
    output
}

impl Drop for OwnedChild<'_> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _kill = self.child.kill();
        let start = Instant::now();
        let reaped = loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break true,
                _ if start.elapsed() >= TEST_CHILD_CLEANUP => break false,
                _ => std::thread::sleep(TEST_CHILD_POLL), // wait-deliberate: exit poll pace under a bound
            }
        };
        let mut joined = true;
        for slot in [&mut self.stdout, &mut self.stderr] {
            // Cancellation intentionally returns a capture error. Only failure
            // to finish the thread matters during cleanup.
            let _result = finish(slot);
            joined &= slot.is_none();
        }
        if !reaped || !joined {
            if std::thread::panicking() {
                eprintln!("test child cleanup incomplete: reaped={reaped}, joined={joined}");
            } else {
                panic!("test child cleanup incomplete: reaped={reaped}, joined={joined}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_timeout_kill_returns_without_waiting_for_exit() {
        let mut polls = 0;
        let error = wait_for_exit(
            &mut polls,
            Duration::ZERO,
            |polls, _remaining| {
                *polls += 1;
                Ok(None::<()>)
            },
            |_| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "kill refused",
                ))
            },
            child_timed_out,
        )
        .expect_err("kill failure must reach the owner");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(polls, 1, "no exit observation after a failed kill");
    }

    #[test]
    fn capture_cancels_when_another_writer_keeps_the_pipe_open() {
        let (reader, _writer) = std::os::unix::net::UnixStream::pair().expect("pipe");
        let stop = Arc::new(AtomicBool::new(false));
        let mut thread = Some(capture(reader, Arc::clone(&stop)).expect("capture"));
        assert_eq!(
            finish(&mut thread).expect_err("no EOF").kind(),
            io::ErrorKind::TimedOut
        );
        stop.store(true, Ordering::Release);
        assert!(finish(&mut thread).is_err(), "cancellation is explicit");
        assert!(thread.is_none(), "reader joined");
    }

    #[test]
    fn capture_read_errors_are_not_empty_success() {
        let scratch = pbfhogg_test_scratch::scratch();
        let directory = std::fs::File::open(scratch.path()).expect("directory");
        let mut thread =
            Some(capture(directory, Arc::new(AtomicBool::new(false))).expect("capture"));
        assert!(finish(&mut thread).is_err());
        assert!(thread.is_none());
    }
}
