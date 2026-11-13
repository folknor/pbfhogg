//! The one owner of every child the verifier starts.
//!
//! The verifier is single-threaded. [`Lifecycle::install`] runs before any
//! thread could exist: it blocks SIGTERM, SIGINT and SIGHUP and reads them
//! from a signalfd instead, and makes the verifier a child subreaper, so
//! every descendant of a launch is reparented here when its parent exits.
//!
//! Every launch - pbfhogg, a reference tool, a provisioning or probe tool -
//! goes through [`Lifecycle::run`], and settles before the next one starts:
//! the leader's status is collected, then any descendant still alive is
//! killed and reaped until `waitpid` reports ECHILD. Only then is a launch's
//! output read. A descendant that outlived its leader is an environment
//! outcome (its outputs may be partial); one that cannot be reaped is
//! [`Error::Unsettled`], after which nothing may run.
//!
//! Cancellation: `brokkr kill` and Ctrl-C reach this process as a signal,
//! and brokkr follows its SIGTERM with SIGKILL after a short grace. While a
//! child lives, the wait polls the signalfd beside the child, and a signal
//! kills and reaps the whole tree at once. While none lives, the signal is
//! seen at the next [`Lifecycle::checkpoint`]; if SIGKILL comes first there
//! is no descendant to strand. Each child also carries a parent-death
//! SIGKILL as a backstop for the verifier's own death. That backstop is not
//! inherited across fork: a descendant a launcher forks may outlive a
//! verifier killed by SIGKILL, and nothing here contains it.
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

use crate::error::Error;
use crate::tunables;

/// One launch: an absolute program, its arguments, its whole environment
/// (nothing is inherited), its working directory and the file its stdout
/// goes to.
pub(crate) struct Launch<'a> {
    pub(crate) program: &'a Path,
    pub(crate) args: Vec<OsString>,
    pub(crate) env: Vec<(OsString, OsString)>,
    pub(crate) cwd: &'a Path,
    pub(crate) sink: &'a Path,
}

/// A settled launch: the leader's status and its whole stderr.
#[derive(Debug)]
pub(crate) struct Ended {
    pub(crate) status: ExitStatus,
    pub(crate) stderr: Vec<u8>,
}

impl Ended {
    pub(crate) fn code(&self) -> Option<i32> {
        self.status.code()
    }

    pub(crate) fn signal(&self) -> Option<i32> {
        use std::os::unix::process::ExitStatusExt;
        self.status.signal()
    }

    pub(crate) fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

pub(crate) struct Lifecycle {
    signals: OwnedFd,
    pid: libc::pid_t,
    cancelled: bool,
}

fn os_error<T>(what: &str) -> io::Result<T> {
    let e = io::Error::last_os_error();
    Err(io::Error::new(e.kind(), format!("{what}: {e}")))
}

impl Lifecycle {
    /// Call once, first thing in `main`, before anything could start a thread.
    pub(crate) fn install() -> io::Result<Self> {
        // SAFETY: sigset_t is plain data; the libc calls initialise and fill
        // it before it is read, and every pointer refers to a live local.
        let signals = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                libc::sigaddset(&mut set, signal);
            }
            if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
                return os_error("block signals");
            }
            let fd = libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
            if fd == -1 {
                return os_error("signalfd");
            }
            OwnedFd::from_raw_fd(fd)
        };
        // SAFETY: prctl's scalar arguments carry no pointer lifetimes.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == -1 {
            return os_error("subreaper");
        }
        // SAFETY: getpid cannot fail.
        let pid = unsafe { libc::getpid() };
        Ok(Self {
            signals,
            pid,
            cancelled: false,
        })
    }

    /// `Err(Interrupted)` once a stop signal has arrived. Called before every
    /// launch and before anything that commits a result: a cache
    /// publication, a check's verdict, the run directory's removal, the exit.
    pub(crate) fn checkpoint(&mut self) -> Result<(), Error> {
        if !self.cancelled && self.signal_pending()? {
            self.cancelled = true;
        }
        if self.cancelled {
            Err(Error::Interrupted)
        } else {
            Ok(())
        }
    }

    fn signal_pending(&self) -> io::Result<bool> {
        let mut info = [0_u8; std::mem::size_of::<libc::signalfd_siginfo>()];
        // SAFETY: the buffer is writable for its whole length.
        let n = unsafe {
            libc::read(
                self.signals.as_raw_fd(),
                info.as_mut_ptr().cast(),
                info.len(),
            )
        };
        if n > 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::WouldBlock => Ok(false),
            io::ErrorKind::Interrupted => self.signal_pending(),
            _ => Err(e),
        }
    }

    /// Start `launch`, wait for it and settle its tree. The exit status is
    /// the caller's to judge.
    pub(crate) fn run(&mut self, launch: &Launch<'_>) -> Result<Ended, Error> {
        self.checkpoint()?;
        let sink = File::create(launch.sink)
            .map_err(|e| Error::tool(format!("create {}: {e}", launch.sink.display())))?;
        let mut cmd = Command::new(launch.program);
        cmd.env_clear()
            .args(&launch.args)
            .envs(launch.env.iter().map(|(k, v)| (k, v)))
            .current_dir(launch.cwd)
            .stdin(Stdio::null())
            .stdout(sink)
            .stderr(Stdio::piped());
        let parent = self.pid;
        // SAFETY: the closure runs between fork and exec and calls only
        // async-signal-safe functions on locals.
        unsafe {
            cmd.pre_exec(move || {
                prepare_child(parent);
                Ok(())
            });
        }
        let label = launch.program.display().to_string();
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::tool(format!("start {label}: {e}")))?;
        let waited = self.wait(&mut child);
        // Whatever the wait concluded, nothing of this launch may outlive it.
        let adopted =
            settle(Some(child.id())).map_err(|e| Error::Unsettled(format!("{label}: {e}")))?;
        let (status, stderr) = waited?;
        if adopted {
            return Err(Error::Environment(format!(
                "a descendant of {label} outlived it and was killed; its outputs may be partial"
            )));
        }
        Ok(Ended { status, stderr })
    }

    /// Wait for the leader, draining its stderr, or for a stop signal.
    fn wait(&mut self, child: &mut std::process::Child) -> Result<(ExitStatus, Vec<u8>), Error> {
        let mut err = child
            .stderr
            .take()
            .ok_or_else(|| Error::tool("missing stderr pipe"))?;
        nonblocking(err.as_raw_fd())?;
        let leader = pidfd(child.id())?;
        let mut captured = Capture::default();
        let mut fds = [
            pollfd(self.signals.as_raw_fd()),
            pollfd(err.as_raw_fd()),
            pollfd(leader.as_raw_fd()),
        ];
        // loop-ends: the leader exits (pidfd readable) or a stop signal
        // arrives (signalfd readable); poll sleeps until one is ready.
        let status = loop {
            if self.signal_pending()? {
                self.cancelled = true;
                return Err(Error::Interrupted);
            }
            captured.drain(&mut err)?;
            if captured.overflow {
                return Err(Error::tool(format!(
                    "stderr exceeded {} bytes",
                    tunables::STDERR_LIMIT
                )));
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            poll(&mut fds, -1)?;
        };
        // The leader is gone. What a surviving descendant writes later is not
        // collected: such a descendant makes the launch an environment
        // outcome anyway, once the caller settles the tree.
        captured.drain(&mut err)?;
        if captured.overflow {
            return Err(Error::tool(format!(
                "stderr exceeded {} bytes",
                tunables::STDERR_LIMIT
            )));
        }
        Ok((status, captured.bytes))
    }
}

/// Arm the child backstop, including the race where the parent already died.
///
/// # Safety
/// Called only between fork and exec, or in the single-threaded test worker.
pub(crate) unsafe fn prepare_child(parent: libc::pid_t) {
    // SAFETY: only async-signal-safe operations on a live local signal set.
    unsafe {
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
        if libc::getppid() != parent {
            libc::_exit(127);
        }
    }
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    overflow: bool,
}

impl Capture {
    fn drain(&mut self, reader: &mut impl Read) -> io::Result<()> {
        let mut buf = vec![0; tunables::DRAIN_BUFFER];
        // loop-ends: the pipe is empty (WouldBlock), closed (0), or overflows.
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    let keep = n.min(tunables::STDERR_LIMIT.saturating_sub(self.bytes.len()));
                    self.bytes.extend_from_slice(&buf[..keep]);
                    self.overflow |= keep != n;
                    // A continuous writer must not starve signal handling or
                    // settlement. The caller treats overflow as a launch error.
                    if self.overflow {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a live descriptor with scalar flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: as above.
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return os_error("O_NONBLOCK");
    }
    Ok(())
}

fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Sleep until a descriptor is ready, `timeout_ms` at most (-1: no bound).
fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<()> {
    let count = libc::nfds_t::try_from(fds.len()).map_err(io::Error::other)?;
    // SAFETY: the slice owns writable pollfd entries for its exact length.
    // wait-deliberate: readiness of a child descriptor or the signalfd wakes it.
    if unsafe { libc::poll(fds.as_mut_ptr(), count, timeout_ms) } == -1 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    for fd in fds {
        // A closed pipe stays readable forever; stop polling it.
        if fd.revents & libc::POLLHUP != 0 && fd.revents & libc::POLLIN == 0 {
            fd.fd = -1;
        }
    }
    Ok(())
}

fn pidfd(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: numeric PID and flags; a returned descriptor is owned here.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw == -1 {
        return os_error("pidfd_open");
    }
    let raw = i32::try_from(raw).map_err(io::Error::other)?;
    // SAFETY: pidfd_open returned a fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn kill(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: live pidfd, null siginfo; no numeric PID is signalled.
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if sent == -1 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(e);
        }
    }
    Ok(())
}

fn children() -> io::Result<Vec<u32>> {
    let mut ids = Vec::new();
    for task in std::fs::read_dir("/proc/self/task")? {
        let text = match std::fs::read_to_string(task?.path().join("children")) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for id in text.split_whitespace() {
            ids.push(id.parse().map_err(io::Error::other)?);
        }
    }
    Ok(ids)
}

/// Kill and reap every child of this process until ECHILD. `leader` is the
/// launch's own process, which may still be alive (interrupted) or already
/// reaped; any other child found is adopted, and the result says so.
fn settle(leader: Option<u32>) -> io::Result<bool> {
    let start = Instant::now();
    let mut adopted = false;
    // loop-ends: waitpid reports ECHILD; each pass kills what it finds and
    // the deadline refuses a tree that will not die.
    loop {
        if start.elapsed() >= tunables::SETTLE_DEADLINE {
            return Err(io::Error::other("descendants survived SIGKILL"));
        }
        let mut killed = Vec::new();
        for pid in children()? {
            adopted |= Some(pid) != leader;
            match pidfd(pid) {
                Ok(fd) => {
                    kill(&fd)?;
                    killed.push(fd);
                }
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => return Err(e),
            }
        }
        if reap()? {
            return Ok(adopted);
        }
        let mut fds: Vec<_> = killed.iter().map(|fd| pollfd(fd.as_raw_fd())).collect();
        let wait = i32::try_from(tunables::SETTLE_POLL.as_millis()).map_err(io::Error::other)?;
        poll(&mut fds, wait)?;
    }
}

/// Reap every exited child; `true` once none is left.
fn reap() -> io::Result<bool> {
    // loop-ends: no exited child remains (0) or no child at all (ECHILD).
    loop {
        let mut status = 0;
        // SAFETY: status is writable; -1 waits for any child of this process.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL | libc::WNOHANG) };
        if pid == 0 {
            return Ok(false);
        }
        if pid < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ECHILD) => return Ok(true),
                Some(libc::EINTR) => {}
                _ => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn capture_keeps_a_bounded_prefix_and_flags_overflow() {
        let mut capture = Capture::default();
        let data = vec![b'x'; tunables::STDERR_LIMIT + 3];
        capture.drain(&mut data.as_slice()).unwrap();
        assert_eq!(capture.bytes.len(), tunables::STDERR_LIMIT);
        assert!(capture.overflow);
    }
}
