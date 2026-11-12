//! Owned test children with cancellable, bounded pipe capture.
use pbfhogg::{
    TEST_CHILD_BUDGET, TEST_CHILD_CAPTURE_CHUNK, TEST_CHILD_CAPTURE_EDGE, TEST_CHILD_CLEANUP,
    TEST_CHILD_POLL,
};
use pbfhogg_test_scratch::TestScratch;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The child starts from an empty environment: nothing of the runner's is
/// inherited (no `PATH`, `HOME`, locale or `TZ`; every child is started by
/// absolute path, and an empty locale is the C locale), and only `TMPDIR`,
/// pointed at the scratch, is set here. Whatever else the child sees - hook,
/// protocol and instrumentation variables - the caller sets after this
/// function, so a child's environment is exactly what its test wrote. The
/// source-policy rules `host_independence` (every constructor clears) and
/// `environment` (no test reads the runner's variables outside its closed
/// inventory) hold the tree to this.
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
    // First, unconditionally: the source-policy rule host_independence
    // requires the clear to be this function's first statement.
    cmd.env_clear()
        .current_dir(scratch.path())
        .stdin(Stdio::null())
        .env("TMPDIR", scratch.path());
    parent_death_kill(cmd);
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

/// A capture's cancellation: an eventfd the capture thread polls beside its
/// pipe. Firing it leaves it readable for good (nobody reads it), so the
/// thread sees it however late it polls. Only cleanup fires it: a normal
/// `finish` lets the capture run to EOF, so no buffered output is lost.
#[derive(Clone)]
struct Stop(Arc<std::os::fd::OwnedFd>);

impl Stop {
    fn new() -> io::Result<Self> {
        // SAFETY: eventfd takes no pointers.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: eventfd returned a fresh descriptor that nothing else owns.
        Ok(Self(Arc::new(unsafe {
            <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
        })))
    }

    fn fire(&self) {
        let one: u64 = 1;
        // SAFETY: writes eight bytes from a live u64 to a live eventfd. A
        // failure can only be a counter overflow, which leaves it readable.
        let _written = unsafe { libc::write(self.0.as_raw_fd(), (&raw const one).cast(), 8) };
    }

    /// Whether the stop has fired; never blocks.
    fn fired(&self) -> bool {
        let mut fd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll borrows the live eventfd for this call only.
        // wait-deliberate: zero timeout, a nonblocking readiness probe
        unsafe { libc::poll(&mut fd, 1, 0) > 0 }
    }
}

struct Capture {
    thread: JoinHandle<()>,
    result: std::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
    stop: Stop,
}

fn capture(mut pipe: impl Read + AsRawFd + Send + 'static) -> io::Result<Capture> {
    // SAFETY: fcntl operates on a live owned pipe descriptor. Only this reader
    // uses its file status flags; no borrowed descriptor outlives the pipe.
    unsafe {
        let flags = libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let stop = Stop::new()?;
    let watched = stop.clone();
    let (send, result) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let output = (|| {
            let mut first = Vec::new();
            let mut last = std::collections::VecDeque::new();
            let mut chunk = [0; TEST_CHILD_CAPTURE_CHUNK];
            let mut total = 0_usize;
            loop {
                if watched.fired() {
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
                        let mut fds = [
                            libc::pollfd {
                                fd: pipe.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                            libc::pollfd {
                                fd: watched.0.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                        ];
                        // No timeout: the pipe's data or EOF, or the stop
                        // firing, ends the wait.
                        // SAFETY: poll borrows the live pipe and eventfd for this call only.
                        // wait-deliberate: untimed; ends on the pipe or the stop event
                        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
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
    Ok(Capture {
        thread,
        result,
        stop,
    })
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
    /// The child's pidfd, opened right after the spawn while the child is
    /// unreaped, so it names this child for good; see [`ExitFd`].
    exit: ExitFd,
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
        // Owned before anything else can fail: an error below returns
        // through this owner, whose Drop kills and reaps the child.
        let mut owner = Self {
            child,
            _scratch: scratch,
            exit: ExitFd(None),
            stdout: None,
            stderr: None,
            budget,
            status: None,
        };
        owner.exit = ExitFd::open(&owner.child)?;
        if let Some(pipe) = owner.child.stdout.take() {
            owner.stdout = Some(capture(pipe)?);
        }
        if let Some(pipe) = owner.child.stderr.take() {
            owner.stderr = Some(capture(pipe)?);
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

    /// Whether the child has exited, without reaping it, so a signal sent
    /// after a `false` still reaches at worst its zombie.
    fn exited_unreaped(&self) -> io::Result<bool> {
        if self.status.is_some() {
            return Ok(true);
        }
        self.exit.exited(&self.child)
    }

    /// Wait, without reaping, until the child exits or `deadline` passes;
    /// whether it exited.
    #[allow(dead_code)]
    pub fn wait_exited(&self, deadline: Instant) -> io::Result<bool> {
        loop {
            if self.exited_unreaped()? {
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            self.exit.poll_with_none(remaining)?;
        }
    }

    /// Wait until `fd` is readable, the child exits, or `deadline` passes,
    /// whichever comes first; the child is never reaped here. Readable wins
    /// when both are ready, so a caller draining an announcement pipe sees
    /// what the child wrote before it exited.
    #[allow(dead_code)]
    pub fn wait_readable(
        &self,
        fd: std::os::fd::BorrowedFd<'_>,
        deadline: Instant,
    ) -> io::Result<Readiness> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let ready = self.exit.poll_with(fd, remaining)?;
            if ready.fd {
                return Ok(Readiness::Readable);
            }
            if ready.exited || self.exited_unreaped()? {
                return Ok(Readiness::Exited);
            }
            if Instant::now() >= deadline {
                return Ok(Readiness::Expired);
            }
        }
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
        // An exit already reaped returns its recorded status.
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
            let exit = &self.exit;
            let status = wait_for_exit(
                &mut self.child,
                limit,
                |child, remaining| {
                    exit.poll_with_none(remaining)?;
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

/// How [`OwnedChild::wait_readable`] ended.
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub enum Readiness {
    /// The descriptor is readable.
    Readable,
    /// The child exited (unreaped) with nothing left to read.
    Exited,
    /// The deadline passed.
    Expired,
}

/// What one [`ExitFd::poll_with`] saw.
struct Ready {
    fd: bool,
    exited: bool,
}

/// Poll timeouts are whole milliseconds, rounded up so a wait never ends
/// before its bound.
fn poll_millis(limit: Duration) -> libc::c_int {
    libc::c_int::try_from(limit.as_nanos().div_ceil(1_000_000)).unwrap_or(libc::c_int::MAX)
}

/// Exit readiness for an unreaped child. A pidfd becomes readable the moment
/// the child exits, so completion costs no polling delay. Where the kernel
/// lacks pidfd_open (before Linux 5.3) it degrades to re-checking every
/// `TEST_CHILD_POLL`: the one surviving polled child wait, kept until a
/// sound exit observer for that case exists (a blocking `waitid` helper
/// needs an ownership and cancellation protocol nobody has written).
struct ExitFd(Option<std::os::fd::OwnedFd>);

impl ExitFd {
    /// Open the pidfd. Call only while the child is unreaped, which spawn
    /// guarantees: nothing can have reaped it yet.
    fn open(child: &Child) -> io::Result<Self> {
        let pid = libc::pid_t::try_from(child.id())
            .map_err(|_| io::Error::other("pid does not fit pid_t"))?;
        // SAFETY: the child is unreaped, so the PID still names it;
        // pidfd_open takes no pointers.
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
    fn poll_with_none(&self, limit: Duration) -> io::Result<()> {
        self.poll(None, limit).map(|_| ())
    }

    /// Return when `fd` is readable, the child has exited, or `limit` has
    /// elapsed, saying which were seen.
    fn poll_with(&self, fd: std::os::fd::BorrowedFd<'_>, limit: Duration) -> io::Result<Ready> {
        self.poll(Some(fd), limit)
    }

    fn poll(&self, fd: Option<std::os::fd::BorrowedFd<'_>>, limit: Duration) -> io::Result<Ready> {
        let mut fds = Vec::with_capacity(2);
        if let Some(fd) = fd {
            fds.push(libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let timeout = match &self.0 {
            Some(pidfd) => {
                fds.push(libc::pollfd {
                    fd: pidfd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                poll_millis(limit)
            }
            // No exit notification without a pidfd: re-check at this pace.
            None => poll_millis(TEST_CHILD_POLL.min(limit)),
        };
        let count = libc::nfds_t::try_from(fds.len())
            .map_err(|_| io::Error::other("descriptor count does not fit nfds_t"))?;
        // SAFETY: poll borrows the live descriptors for this call only.
        // wait-deliberate: bounded by the caller's deadline; ends on readiness or exit
        if unsafe { libc::poll(fds.as_mut_ptr(), count, timeout) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        if fds.iter().any(|p| p.revents & libc::POLLNVAL != 0) {
            return Err(io::Error::other("poll on a closed descriptor"));
        }
        let ready =
            |p: &libc::pollfd| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
        Ok(Ready {
            fd: fd.is_some() && ready(&fds[0]),
            exited: self.0.is_some() && fds.last().is_some_and(ready),
        })
    }

    /// Whether the child has exited, without reaping it.
    fn exited(&self, child: &Child) -> io::Result<bool> {
        if self.0.is_some() {
            return Ok(self.poll(None, Duration::ZERO)?.exited);
        }
        let pid = libc::id_t::try_from(child.id())
            .map_err(|_| io::Error::other("pid does not fit id_t"))?;
        // SAFETY: a zeroed siginfo is a valid out buffer; WNOWAIT leaves the
        // child unreaped, so its PID stays ours.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: waitid filled `info`; si_pid is zero when nothing has exited.
        Ok(unsafe { info.si_pid() } != 0)
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
        for capture in [&self.stdout, &self.stderr].into_iter().flatten() {
            capture.stop.fire();
        }
        // std's `Child` keeps a status it has reaped and never signals then.
        let _kill = self.child.kill();
        let deadline = Instant::now() + TEST_CHILD_CLEANUP;
        let reaped = loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if Instant::now() < deadline => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if self.exit.poll_with_none(remaining).is_err() {
                        break false;
                    }
                }
                _ => break false,
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
        let mut thread = Some(capture(reader).expect("capture"));
        // The pipe never reaches EOF; only the stop ends the capture.
        thread.as_ref().expect("capture").stop.fire();
        let error = finish(&mut thread).expect_err("cancellation is explicit");
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(thread.is_none(), "reader joined");
    }

    #[test]
    fn capture_read_errors_are_not_empty_success() {
        let scratch = pbfhogg_test_scratch::scratch();
        let directory = std::fs::File::open(scratch.path()).expect("directory");
        let mut thread = Some(capture(directory).expect("capture"));
        assert!(finish(&mut thread).is_err());
        assert!(thread.is_none());
    }
}
