//! Owned test children with cancellable, bounded pipe capture.
use pbfhogg::{
    TEST_CHILD_CAPTURE_CHUNK, TEST_CHILD_CAPTURE_EDGE, TEST_CHILD_CLEANUP, TEST_CHILD_POLL,
};
use pbfhogg_test_scratch::TestScratch;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Deliberate inherited values only. Hook and instrumentation variables never
/// enter the child unless the caller sets them after this function.
pub fn isolate(cmd: &mut Command, scratch: &TestScratch) {
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

/// The child and its pipe readers stay owned even if waiting or assertions fail.
pub struct OwnedChild<'a> {
    child: Child,
    _scratch: &'a TestScratch,
    stop: Arc<AtomicBool>,
    stdout: Option<Capture>,
    stderr: Option<Capture>,
}

impl<'a> OwnedChild<'a> {
    pub fn spawn(cmd: &mut Command, scratch: &'a TestScratch) -> io::Result<Self> {
        let child = cmd.spawn()?;
        let mut owner = Self {
            child,
            _scratch: scratch,
            stop: Arc::new(AtomicBool::new(false)),
            stdout: None,
            stderr: None,
        };
        if let Some(pipe) = owner.child.stdout.take() {
            owner.stdout = Some(capture(pipe, Arc::clone(&owner.stop))?);
        }
        if let Some(pipe) = owner.child.stderr.take() {
            owner.stderr = Some(capture(pipe, Arc::clone(&owner.stop))?);
        }
        Ok(owner)
    }

    pub fn has_exited(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn signal(&self, signal: libc::c_int) {
        let pid = libc::pid_t::try_from(self.child.id()).expect("pid fits");
        // SAFETY: this owner has not relinquished its child.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "signal child");
    }

    pub fn wait(&mut self, timeout: Duration) -> io::Result<Output> {
        // A cached status (has_exited() may already have reaped) must be read
        // before opening a pidfd: a reaped PID could name another process.
        let status = if let Some(status) = self.child.try_wait()? {
            status
        } else {
            let exit = ExitFd::open(&self.child)?;
            wait_for_exit(
                &mut self.child,
                timeout,
                |child, remaining| {
                    exit.block(remaining)?;
                    child.try_wait()
                },
                Child::kill,
            )?
        };
        Ok(Output {
            status,
            stdout: finish(&mut self.stdout)?,
            stderr: finish(&mut self.stderr)?,
        })
    }

    /// Exercise ownership on a wait failure without changing normal waiting.
    pub fn wait_with_check(
        &mut self,
        timeout: Duration,
        check: impl FnOnce(&mut Child) -> io::Result<()>,
    ) -> io::Result<Output> {
        check(&mut self.child)?;
        self.wait(timeout)
    }
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
) -> io::Result<T> {
    let start = Instant::now();
    loop {
        if let Some(status) = block(state, timeout.saturating_sub(start.elapsed()))? {
            return Ok(status);
        }
        if start.elapsed() >= timeout {
            kill(state)?;
            // OwnedChild::drop performs bounded reaping and pipe cleanup.
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "test child timed out",
            ));
        }
    }
}

fn finish(slot: &mut Option<Capture>) -> io::Result<Vec<u8>> {
    let Some(capture) = slot.as_ref() else {
        return Ok(Vec::new());
    };
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
                _ => std::thread::sleep(TEST_CHILD_POLL),
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
