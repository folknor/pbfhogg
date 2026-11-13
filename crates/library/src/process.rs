//! Linux worker lifecycle. The caller is the sole spawner and must stop on
//! `Unestablished`: only ECHILD permits reading artefacts or another launch.
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::tunables;

#[derive(Debug)]
pub(crate) enum Failure {
    /// No judgement or subsequent worker launch is safe.
    Unestablished(Box<Problem>),
    Refused(Box<Problem>),
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unestablished(e) => write!(f, "quiescence unestablished: {}", e.reason),
            Self::Refused(e) => f.write_str(&e.reason),
        }
    }
}

/// Evidence survives capture, wait and cleanup errors too. A verification
/// caller may save these bounded prefixes without reading worker artefacts.
#[derive(Debug)]
pub(crate) struct Problem {
    pub(crate) reason: String,
    pub(crate) captures: Captures,
}
#[derive(Debug, Default)]
pub(crate) struct Captures {
    pub(crate) out: Capture,
    pub(crate) err: Capture,
    pub(crate) markers: Capture,
}
impl Failure {
    fn unestablished(reason: String) -> Self {
        Self::Unestablished(Box::new(Problem {
            reason,
            captures: Captures::default(),
        }))
    }
    fn refused(reason: String) -> Self {
        Self::Refused(Box::new(Problem {
            reason,
            captures: Captures::default(),
        }))
    }
    fn with_captures(mut self, captures: Captures) -> Self {
        match &mut self {
            Self::Unestablished(problem) | Self::Refused(problem) => problem.captures = captures,
        }
        self
    }
}

#[derive(Debug, Default)]
pub(crate) struct Capture {
    pub(crate) bytes: Vec<u8>,
    pub(crate) overflow: bool,
}
impl Capture {
    fn drain(&mut self, reader: &mut impl Read) -> io::Result<bool> {
        let mut buf = [0; tunables::DRAIN_BUFFER];
        let mut read = 0;
        while read < tunables::DRAIN_TURN {
            match reader.read(&mut buf) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    read += n;
                    let keep = n.min(tunables::CAPTURE_LIMIT - self.bytes.len());
                    self.bytes.extend_from_slice(&buf[..keep]);
                    self.overflow |= keep != n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }
}

#[derive(Debug)]
pub(crate) struct Completed {
    pub(crate) elapsed: Duration,
    pub(crate) status: ExitStatus,
    pub(crate) timed_out: bool,
    pub(crate) residual_children: bool,
    pub(crate) out: Capture,
    pub(crate) err: Capture,
    pub(crate) markers: Capture,
}

/// Call once at the executable boundary, before spawning anything.
pub(crate) fn subreaper() -> io::Result<()> {
    // SAFETY: prctl's scalar arguments have no pointer lifetime requirements.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Every worker has an explicitly built environment. In particular no
/// HOTPATH_* or inherited marker FIFO reaches describe or verification.
pub(crate) fn command(executable: &Path) -> Command {
    let mut cmd = Command::new(executable);
    cmd.env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: fcntl operates on a borrowed, live descriptor, with scalar flags.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Drains stdout, stderr and an optional nonblocking marker FIFO throughout
/// execution and cleanup. FIFO creation and evidence retention belong to run.
pub(crate) fn run(
    mut cmd: Command,
    deadline: Duration,
    mut fifo: Option<File>,
) -> Result<Completed, Failure> {
    if let Some(file) = &fifo {
        nonblocking(file).map_err(|e| Failure::refused(e.to_string()))?;
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| Failure::refused(format!("spawn worker: {e}")))?;
    let mut out = child
        .stdout
        .take()
        .ok_or_else(|| Failure::unestablished("missing stdout pipe".into()))?;
    let mut err = child
        .stderr
        .take()
        .ok_or_else(|| Failure::unestablished("missing stderr pipe".into()))?;
    let worker_fd = pidfd(child.id());
    let setup = nonblocking(&out).and_then(|()| nonblocking(&err));
    let mut ready = vec![pollfd(out.as_raw_fd()), pollfd(err.as_raw_fd())];
    if let Some(file) = &fifo {
        ready.push(pollfd(file.as_raw_fd()));
    }
    if let Ok(fd) = &worker_fd {
        ready.push(pollfd(fd.as_raw_fd()));
    }

    let mut captures = Captures::default();
    let mut drain = || -> io::Result<bool> {
        let mut more = captures.out.drain(&mut out)?;
        more |= captures.err.drain(&mut err)?;
        if let Some(file) = &mut fifo {
            more |= captures.markers.drain(file)?;
        }
        Ok(more)
    };
    let mut fault = setup.err().or_else(|| {
        worker_fd
            .as_ref()
            .err()
            .map(|e| io::Error::other(e.to_string()))
    });
    let mut timed_out = false;
    // loop-ends: worker exit or deadline; poll wakes on capture bytes or pidfd exit.
    let status = loop {
        if fault.is_some() || start.elapsed() >= deadline {
            timed_out = start.elapsed() >= deadline;
            break None;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => {
                fault = Some(e);
                break None;
            }
        }
        fault = drain().err();
        if fault.is_none() {
            fault = wait_ready(&mut ready, deadline.saturating_sub(start.elapsed())).err();
        }
    };
    // If setup failed, do not read blocking pipes during cleanup.
    let can_drain = fault.is_none();
    let mut drain_fault = None;
    let cleaned = cleanup(status.is_none().then_some(child.id()), &mut || {
        if can_drain && let Err(e) = drain() {
            drain_fault = Some(e);
        }
    });
    let residual_children = match cleaned {
        Ok(result) => result,
        Err(e) => return Err(Failure::unestablished(e.to_string()).with_captures(captures)),
    };
    // All writers are dead. Capture even a pipe larger than one drain turn.
    if can_drain && let Err(e) = drain_remaining(&mut drain) {
        drain_fault = Some(e);
    }
    if let Some(e) = fault.or(drain_fault) {
        return Err(Failure::refused(format!("worker capture/wait: {e}")).with_captures(captures));
    }
    // cleanup may have reaped the worker through waitpid; its status must be
    // retained there rather than fabricated from Child::wait's ECHILD.
    let Some(status) = status.or(residual_children.worker_status) else {
        return Err(
            Failure::refused("worker status was not observed".into()).with_captures(captures)
        );
    };
    Ok(Completed {
        elapsed: start.elapsed(),
        status,
        timed_out,
        residual_children: residual_children.adopted,
        out: captures.out,
        err: captures.err,
        markers: captures.markers,
    })
}

struct Cleanup {
    adopted: bool,
    worker_status: Option<ExitStatus>,
}
fn children() -> io::Result<Vec<i32>> {
    let mut ids = Vec::new();
    for task in std::fs::read_dir("/proc/self/task")? {
        let path = task?.path().join("children");
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // exited thread
            Err(e) => return Err(e),
        };
        for id in text.split_whitespace() {
            ids.push(
                id.parse()
                    .map_err(|_| io::Error::other("invalid child PID"))?,
            );
        }
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}
fn pidfd(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: syscall takes a numeric PID and flags; returned fd is owned.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw == -1 {
        return Err(io::Error::last_os_error());
    }
    let raw = i32::try_from(raw).map_err(|_| io::Error::other("pidfd outside descriptor range"))?;
    // SAFETY: successful pidfd_open returned a fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}
fn kill_pidfd(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: live pidfd, null siginfo, valid signal and flags. No numeric
    // PID is signalled, even if the enumerated process exited meanwhile.
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } == -1
    {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(e);
        }
    }
    Ok(())
}
fn cleanup(mut worker: Option<u32>, drain: &mut impl FnMut()) -> io::Result<Cleanup> {
    use std::os::unix::process::ExitStatusExt;
    let start = Instant::now();
    let mut result = Cleanup {
        adopted: false,
        worker_status: None,
    };
    // loop-ends: ECHILD proves quiescence; pidfds notify exits; the cleanup deadline refuses a stuck tree.
    loop {
        let mut killed = Vec::new();
        for pid in children()? {
            cleanup_bound(start)?;
            result.adopted |= !worker.is_some_and(|id| i64::from(pid) == i64::from(id));
            match pidfd(u32::try_from(pid).map_err(|_| io::Error::other("negative child PID"))?) {
                Ok(fd) => {
                    kill_pidfd(&fd)?;
                    killed.push(fd);
                }
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => return Err(e),
            }
        }
        drain();
        loop {
            cleanup_bound(start)?;
            let mut status = 0;
            // SAFETY: status is writable; -1 waits for any of our children.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL | libc::WNOHANG) };
            if pid > 0 {
                if worker.is_some_and(|id| i64::from(pid) == i64::from(id)) {
                    result.worker_status = Some(ExitStatus::from_raw(status));
                    // Once reaped, this numeric ID can be reused. Any later
                    // child with it is adopted, never the original worker.
                    worker = None;
                } else {
                    result.adopted = true;
                }
            } else if pid == 0 {
                break;
            } else {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::ECHILD) => return Ok(result),
                    Some(libc::EINTR) => continue,
                    _ => return Err(e),
                }
            }
        }
        cleanup_bound(start)?;
        let mut ready: Vec<_> = killed.iter().map(|fd| pollfd(fd.as_raw_fd())).collect();
        wait_ready(
            &mut ready,
            tunables::CLEANUP_DEADLINE.saturating_sub(start.elapsed()),
        )?;
    }
}

fn cleanup_bound(start: Instant) -> io::Result<()> {
    if start.elapsed() >= tunables::CLEANUP_DEADLINE {
        Err(io::Error::other("child cleanup deadline expired"))
    } else {
        Ok(())
    }
}

fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}
/// Wait on data or child exit, capped so cleanup and all drains get a turn.
fn wait_ready(fds: &mut [libc::pollfd], remaining: Duration) -> io::Result<()> {
    let timeout = remaining.min(tunables::POLL_INTERVAL).as_millis();
    let timeout =
        i32::try_from(timeout).map_err(|_| io::Error::other("poll timeout out of range"))?;
    // SAFETY: the slice owns writable pollfd entries for its exact length.
    // wait-deliberate: fd readiness wakes this wait; expiry only returns to enforce the owning deadline.
    if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    for fd in fds {
        // A closed capture pipe never becomes writable again. Avoid a HUP
        // busy loop while a worker that closed its streams keeps computing.
        if fd.revents & libc::POLLHUP != 0 {
            fd.fd = -1;
        }
    }
    Ok(())
}

fn drain_remaining(drain: &mut impl FnMut() -> io::Result<bool>) -> io::Result<()> {
    let start = Instant::now();
    // loop-ends: captures empty, read error or drain deadline; all writers have exited.
    loop {
        if !drain()? {
            return Ok(());
        }
        if start.elapsed() >= tunables::CLEANUP_DEADLINE {
            return Err(io::Error::other("capture drain deadline expired"));
        }
    }
}
