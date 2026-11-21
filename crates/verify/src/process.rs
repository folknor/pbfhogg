//! The one owner of every child the verifier starts.
//!
//! The verifier is single-threaded. [`Lifecycle::install`] runs before any
//! thread could exist: it blocks SIGTERM, SIGINT and SIGHUP and reads them
//! from a signalfd instead, and makes the verifier a child subreaper, so
//! every descendant of a launch is reparented here when its parent exits.
//!
//! Every launch - pbfhogg, a reference tool, a provisioning or probe tool -
//! goes through [`Lifecycle::run_group`] ([`Lifecycle::run`] is a group of
//! one). The members of a group start together, and the group settles
//! before any of its outputs is read or another group starts: every
//! leader's status is collected, then any descendant still alive is killed
//! and reaped until `waitpid` reports ECHILD. A descendant that outlived its
//! leader is an environment outcome for the whole group (its outputs may be
//! partial, and with siblings alive it cannot be attributed to one); one
//! that cannot be reaped is [`Error::Unsettled`], after which nothing may
//! run.
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
        let mut ended = self.run_group(std::slice::from_ref(launch), &mut || Ok(()))?;
        ended
            .pop()
            .ok_or_else(|| Error::tool("a launch group of one ended empty"))
    }

    /// Start every launch of `launches` together, wait for all of them and
    /// settle the whole group once: the barrier is where results become
    /// usable, and nothing of any member is handed back before it. The exit
    /// statuses, in launch order, are the caller's to judge.
    ///
    /// `observe` runs at each leader's exit, the caller's observation of its
    /// inputs that a lone launch gets after it; an `Err` aborts the group.
    ///
    /// A descendant that outlives its leader cannot be attributed to one
    /// member once several are alive, and it may have touched any member's
    /// files, so it makes the whole group an environment outcome. While more
    /// than one member runs, the wait scans for one at every bounded wakeup
    /// (`tunables::GROUP_ADOPTION_POLL`) and aborts the group as soon as it
    /// is seen, so an orphan never runs beside a long sibling. An abort -
    /// a stop signal, a member's stderr overflow, an adoption, an observer
    /// error, a failed start - kills every member, settles, and returns that
    /// error for the group: [`Error::Unsettled`] wins over everything, then
    /// [`Error::Interrupted`], then the abort's own error.
    pub(crate) fn run_group(
        &mut self,
        launches: &[Launch<'_>],
        observe: &mut dyn FnMut() -> Result<(), Error>,
    ) -> Result<Vec<Ended>, Error> {
        self.checkpoint()?;
        let mut members = Vec::with_capacity(launches.len());
        let mut waited = self.spawn_all(launches, &mut members);
        if waited.is_ok() {
            waited = self.wait_group(&mut members, observe);
        }
        // Whatever the wait concluded, nothing of this group may outlive it.
        // On the clean path every leader is already collected; on an abort
        // the sweep kills and reaps the rest and owns their statuses. Every
        // live leader whose pidfd is held is signalled first, so a sweep that
        // fails early (descriptors exhausted) has still stopped them.
        for m in members.iter().filter(|m| m.status.is_none()) {
            // A failed signal here is retried by the sweep, whose own
            // failure is the group's Unsettled.
            if let Some(fd) = &m.pidfd
                && kill(fd).is_err()
            {
                continue;
            }
        }
        let mut leaders: Vec<u32> = members
            .iter()
            .filter(|m| m.status.is_none())
            .map(|m| m.child.id())
            .collect();
        let labels = || {
            members
                .iter()
                .map(|m| m.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let adopted =
            settle(&mut leaders).map_err(|e| Error::Unsettled(format!("{}: {e}", labels())))?;
        // A stop that arrived during the wait's cleanup wins over its error.
        self.checkpoint()?;
        waited?;
        if adopted {
            return Err(Error::Environment(format!(
                "a descendant of {} outlived its launch; its outputs may be partial",
                labels()
            )));
        }
        members
            .into_iter()
            .map(|m| {
                let status = m
                    .status
                    .ok_or_else(|| Error::tool(format!("{}: status lost", m.label)))?;
                Ok(Ended {
                    status,
                    stderr: m.captured.bytes,
                })
            })
            .collect()
    }

    /// Spawn each launch from this, the one forking thread (the parent-death
    /// signal is keyed to it). A member is registered the moment it exists,
    /// before any fallible setup, so a later failure still settles it.
    fn spawn_all(
        &mut self,
        launches: &[Launch<'_>],
        members: &mut Vec<Member>,
    ) -> Result<(), Error> {
        for launch in launches {
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
            let stderr = child.stderr.take();
            members.push(Member {
                child,
                label,
                stderr,
                pidfd: None,
                captured: Capture::default(),
                status: None,
            });
            let member = members
                .last_mut()
                .ok_or_else(|| Error::tool("member lost"))?;
            let err = member
                .stderr
                .as_ref()
                .ok_or_else(|| Error::tool("missing stderr pipe"))?;
            nonblocking(err.as_raw_fd())?;
            member.pidfd = Some(pidfd(member.child.id())?);
        }
        Ok(())
    }

    /// Wait for every leader, draining each stderr, or for a stop signal.
    fn wait_group(
        &mut self,
        members: &mut [Member],
        observe: &mut dyn FnMut() -> Result<(), Error>,
    ) -> Result<(), Error> {
        let bounded = members.len() > 1;
        // loop-ends: every leader exits (its pidfd readable), a stop signal
        // arrives (signalfd readable) or the group aborts; poll sleeps until
        // a descriptor is ready, or the adoption scan's bound with siblings.
        loop {
            if self.signal_pending()? {
                self.cancelled = true;
                return Err(Error::Interrupted);
            }
            // loop-ends: one non-blocking pass over the live members.
            for m in members.iter_mut().filter(|m| m.status.is_none()) {
                m.drain()?;
                if let Some(status) = m.child.try_wait()? {
                    // Reaped: recorded first, so an abort below never counts
                    // this PID, which may be reused, as a live leader.
                    m.status = Some(status);
                    m.pidfd = None;
                    // The leader is gone. What a surviving descendant writes
                    // later is not collected: such a descendant makes the
                    // group an environment outcome anyway.
                    m.drain()?;
                    m.stderr = None;
                    observe()?;
                }
            }
            if members.iter().all(|m| m.status.is_some()) {
                return Ok(());
            }
            if bounded {
                let live: Vec<u32> = members
                    .iter()
                    .filter(|m| m.status.is_none())
                    .map(|m| m.child.id())
                    .collect();
                if children()?.iter().any(|pid| !live.contains(pid)) {
                    return Err(Error::Environment(
                        "a launch's descendant outlived it while its siblings ran".into(),
                    ));
                }
            }
            let mut fds = vec![pollfd(self.signals.as_raw_fd())];
            for m in members.iter().filter(|m| m.status.is_none()) {
                if let Some(err) = &m.stderr {
                    fds.push(pollfd(err.as_raw_fd()));
                }
                if let Some(fd) = &m.pidfd {
                    fds.push(pollfd(fd.as_raw_fd()));
                }
            }
            let timeout = if bounded {
                i32::try_from(tunables::GROUP_ADOPTION_POLL.as_millis())
                    .map_err(io::Error::other)?
            } else {
                -1
            };
            poll(&mut fds, timeout)?;
        }
    }
}

/// One live member of a launch group. `stderr` is dropped at EOF or at the
/// leader's exit and `pidfd` at its exit, so neither keeps the poll ready.
struct Member {
    child: std::process::Child,
    label: String,
    stderr: Option<std::process::ChildStderr>,
    pidfd: Option<OwnedFd>,
    captured: Capture,
    status: Option<ExitStatus>,
}

impl Member {
    fn drain(&mut self) -> Result<(), Error> {
        let Some(err) = self.stderr.as_mut() else {
            return Ok(());
        };
        if self.captured.drain(err)? {
            self.stderr = None;
        }
        if self.captured.overflow {
            return Err(Error::tool(format!(
                "{}: stderr exceeded {} bytes",
                self.label,
                tunables::STDERR_LIMIT
            )));
        }
        Ok(())
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
    /// Read what `reader` holds now; `true` once it is closed.
    fn drain(&mut self, reader: &mut impl Read) -> io::Result<bool> {
        let mut buf = vec![0; tunables::DRAIN_BUFFER];
        // loop-ends: the pipe is empty (WouldBlock), closed (0), or overflows.
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return Ok(true),
                Ok(n) => {
                    let keep = n.min(tunables::STDERR_LIMIT.saturating_sub(self.bytes.len()));
                    self.bytes.extend_from_slice(&buf[..keep]);
                    self.overflow |= keep != n;
                    // A continuous writer must not starve signal handling or
                    // settlement. The caller treats overflow as a launch error.
                    if self.overflow {
                        return Ok(false);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
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

/// Kill and reap every child of this process until ECHILD. `leaders` are
/// the group's leaders not yet reaped (an aborted group's live ones; none on
/// the clean path). Each PID is classified as it is seen, against a set that
/// shrinks as leaders are reaped, so a reused PID is never mistaken for a
/// leader. Any other child, found alive or only as a reaped exit, is
/// adopted, and the result says so.
fn settle(leaders: &mut Vec<u32>) -> io::Result<bool> {
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
            adopted |= !leaders.contains(&pid);
            match pidfd(pid) {
                Ok(fd) => {
                    kill(&fd)?;
                    killed.push(fd);
                }
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => return Err(e),
            }
        }
        let none_left = reap(&mut |pid| match leaders.iter().position(|&l| l == pid) {
            Some(i) => {
                leaders.swap_remove(i);
            }
            None => adopted = true,
        })?;
        if none_left {
            return Ok(adopted);
        }
        let mut fds: Vec<_> = killed.iter().map(|fd| pollfd(fd.as_raw_fd())).collect();
        let wait = i32::try_from(tunables::SETTLE_POLL.as_millis()).map_err(io::Error::other)?;
        poll(&mut fds, wait)?;
    }
}

/// Reap every exited child, handing each PID to `seen` in reap order;
/// `true` once none is left.
fn reap(seen: &mut dyn FnMut(u32)) -> io::Result<bool> {
    // loop-ends: no exited child remains (0) or no child at all (ECHILD).
    loop {
        let mut status = 0;
        // SAFETY: status is writable; -1 waits for any child of this process.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL | libc::WNOHANG) };
        if pid == 0 {
            return Ok(false);
        }
        if let Ok(pid) = u32::try_from(pid) {
            seen(pid);
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
        assert!(!capture.drain(&mut data.as_slice()).unwrap());
        assert_eq!(capture.bytes.len(), tunables::STDERR_LIMIT);
        assert!(capture.overflow);
    }
}
