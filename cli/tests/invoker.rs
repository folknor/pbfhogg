//! Invoke the `pbfhogg` binary from integration tests.
//!
//! Integration tests that drive the CLI (rather than the library
//! directly) go through this helper. It wraps [`std::process::Command`]
//! with a fluent builder plus assertion helpers.
//!
//! Locating the binary: [`pbfhogg_bin`], cargo's own
//! `CARGO_BIN_EXE_pbfhogg`. Cargo builds the binary for this package's
//! integration tests with the test build's own features, profile and
//! target, and hands its path at compile time, so there is nothing to
//! guess and no binary from another build to pick up. That is why every
//! CLI-driving test lives in this package: the root package's tests get
//! no such path.
//!
//! The subcommand lists the command-table tests check their tables
//! against come from the clap tree production parses
//! ([`pbfhogg_cli::cli_command`]), never from `--help` text.
//!
//! No external crate dependency - kept deliberately small to match
//! the project's minimal dev-dep posture.
//!
//! ## Timeout
//!
//! Every invocation has a wall-clock timeout (default 10 s,
//! [`CliInvoker::timeout`] overrides). On expiry the child is sent
//! `SIGKILL` and the test panics with a clear "timed out" message
//! instead of wedging the test runner. The default leaves time to report diagnostics before the
//! 20 s per-test watchdog in `brokkr check`. Explicit overrides
//! remain subject to that outer watchdog.
//!
//! ## The promise guard
//!
//! On Linux every child runs under `guard` (seccomp): starting a process,
//! or closing, duplicating, passing or reflagging the descriptor it
//! inherits as a lease, kills it with `SIGSYS`, and [`CliInvoker::run`] and
//! [`RunningCli::wait`] fail the test naming the guard. Start the binary
//! only through this type, or the run escapes the guard.
//!
//! ## Platform skips
//!
//! [`CliOutput::is_o_direct_unsupported`] and
//! [`CliOutput::is_uring_unsupported`] require exit code 3 and the typed
//! setup error's feature label. Tests that
//! exercise `--direct-io` or `--io-uring` should consult these
//! before asserting success - tmpfs / overlayfs and hosts with low
//! `RLIMIT_MEMLOCK` are common.

use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Default wall-clock timeout applied to every [`CliInvoker`] that
/// doesn't override via [`CliInvoker::timeout`]. Sized for Tier 1 /
/// Tier 2 fixtures; tests that exercise real datasets should bump
/// it explicitly.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Polling cadence for [`std::process::Child::try_wait`] inside the
/// timeout loop. 20 ms is fast enough that test wall-time isn't
/// noticeably padded; slow enough that the loop doesn't burn CPU on
/// a long-running command.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Retain the first and last MiB while continuing to drain the pipe.
fn capture(mut reader: impl Read) -> Vec<u8> {
    const EDGE: usize = 1024 * 1024;
    let mut first = Vec::with_capacity(EDGE);
    let mut last = std::collections::VecDeque::with_capacity(EDGE);
    let mut total = 0_usize;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        total = total.saturating_add(count);
        let prefix = count.min(EDGE - first.len());
        first.extend_from_slice(&chunk[..prefix]);
        for byte in &chunk[prefix..count] {
            if last.len() == EDGE {
                last.pop_front();
            }
            last.push_back(*byte);
        }
    }
    if total > EDGE * 2 {
        first.extend_from_slice(b"\n[output truncated; first and last MiB retained]\n");
    }
    first.extend(last);
    first
}

/// The exit status base of a [`CliInvoker::bind_mounted`] child whose
/// namespace setup the host refused; the failed step is added.
#[cfg(target_os = "linux")]
pub const NAMESPACE_DENIED: i32 = 100;
/// The exit status base of a [`CliInvoker::bind_mounted`] child whose
/// namespace setup failed for any other reason; the failed step is added.
#[cfg(target_os = "linux")]
pub const NAMESPACE_BROKEN: i32 = 110;

/// Invokes the `pbfhogg` binary with a set of arguments, captures
/// stdout/stderr, returns a [`CliOutput`] that assertions read from.
pub struct CliInvoker {
    cmd: Command,
    timeout: Duration,
    stderr_closed: bool,
}

impl CliInvoker {
    /// Start a new invocation. Default timeout is
    /// [`DEFAULT_TIMEOUT`]; override via [`Self::timeout`].
    pub fn new() -> Self {
        Self {
            cmd: Command::new(pbfhogg_bin()),
            timeout: DEFAULT_TIMEOUT,
            stderr_closed: false,
        }
    }

    /// Append a single argument.
    pub fn arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
        self.cmd.arg(arg);
        self
    }

    /// Append a sequence of arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.cmd.args(args);
        self
    }

    /// Set one environment variable for this child process only.
    pub fn env<K: AsRef<OsStr>, V: AsRef<OsStr>>(mut self, key: K, val: V) -> Self {
        self.cmd.env(key, val);
        self
    }

    /// Run the child in `dir`.
    pub fn current_dir(mut self, dir: impl AsRef<std::path::Path>) -> Self {
        self.cmd.current_dir(dir);
        self
    }

    /// Give stderr a pipe whose reader has already gone away.
    pub fn stderr_closed(mut self) -> Self {
        let (reader, writer) = std::io::pipe().expect("stderr pipe");
        drop(reader);
        self.cmd.stderr(writer);
        self.stderr_closed = true;
        self
    }

    /// Cap the size of any file the child writes at `bytes` (`RLIMIT_FSIZE`),
    /// with `SIGXFSZ` ignored, so a write that would extend a file past it
    /// fails with `EFBIG` ("File too large") instead of killing the child: a
    /// forced write failure on any filesystem, for tests of how one is
    /// reported. Pipes (stdout, stderr) are not files and are not capped.
    #[cfg(target_os = "linux")]
    pub fn file_size_limit(mut self, bytes: u64) -> Self {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure runs between fork and exec and calls only
        // async-signal-safe functions (`signal`, `setrlimit`).
        unsafe {
            self.cmd.pre_exec(move || {
                if libc::signal(libc::SIGXFSZ, libc::SIG_IGN) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
                let mut limit: libc::rlimit = std::mem::zeroed();
                if libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The soft limit only: the hard one stays where it is.
                limit.rlim_cur = bytes.min(limit.rlim_max);
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        self
    }

    /// Run the child in a private user and mount namespace in which
    /// `target` is a bind mount of `source`: the same device, another
    /// mount. Nothing is mounted outside the child. Setup that fails ends
    /// the child before `exec` with [`NAMESPACE_DENIED`] plus the step when
    /// the host refuses it (user namespaces disabled or restricted, which a
    /// test skips on), or [`NAMESPACE_BROKEN`] plus the step for any other
    /// error (a setup bug, which a test fails on). Steps: 1 unshare, 2
    /// setgroups, 3 uid_map, 4 gid_map, 5 private propagation, 6 bind mount.
    #[cfg(target_os = "linux")]
    pub fn bind_mounted(mut self, source: &std::path::Path, target: &std::path::Path) -> Self {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::process::CommandExt;
        // Everything the child needs is built here, before the fork: between
        // fork and exec only raw syscalls on these buffers are safe.
        // SAFETY: getuid and getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let uid_map = format!("{uid} {uid} 1");
        let gid_map = format!("{gid} {gid} 1");
        let c = |s: &[u8]| CString::new(s).expect("no NUL in a test path");
        let setgroups_path = c(b"/proc/self/setgroups");
        let uid_map_path = c(b"/proc/self/uid_map");
        let gid_map_path = c(b"/proc/self/gid_map");
        let root = c(b"/");
        let source = c(source.as_os_str().as_bytes());
        let target = c(target.as_os_str().as_bytes());
        // SAFETY: the closure runs between fork and exec and calls only
        // async-signal-safe functions (`unshare`, `open`, `write`, `close`,
        // `mount`, `_exit`) on buffers prepared above; it allocates nothing.
        unsafe {
            self.cmd.pre_exec(move || {
                /// End the child, saying which step failed and whether the
                /// host refused it.
                unsafe fn fail(step: i32) -> ! {
                    // SAFETY: reading this thread's errno is always valid.
                    let errno = unsafe { *libc::__errno_location() };
                    let denied = if step == 1 {
                        // No user namespaces: disabled, unsupported, or the
                        // per-user limit reached.
                        matches!(
                            errno,
                            libc::EPERM
                                | libc::EACCES
                                | libc::EINVAL
                                | libc::ENOSYS
                                | libc::EUSERS
                                | libc::ENOSPC
                        )
                    } else {
                        // A restricted namespace (an LSM policy) refusing
                        // the maps or the mounts.
                        matches!(errno, libc::EPERM | libc::EACCES)
                    };
                    let base = if denied {
                        NAMESPACE_DENIED
                    } else {
                        NAMESPACE_BROKEN
                    };
                    // SAFETY: `_exit` is async-signal-safe and never returns.
                    unsafe { libc::_exit(base + step) }
                }
                unsafe fn write_file(path: &CString, text: &[u8], step: i32) {
                    // SAFETY: a valid NUL-terminated path and a live buffer.
                    unsafe {
                        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                        if fd < 0 {
                            fail(step);
                        }
                        let n = libc::write(fd, text.as_ptr().cast(), text.len());
                        if usize::try_from(n).ok() != Some(text.len()) {
                            fail(step);
                        }
                        libc::close(fd);
                    }
                }
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                    fail(1);
                }
                write_file(&setgroups_path, b"deny", 2);
                write_file(&uid_map_path, uid_map.as_bytes(), 3);
                write_file(&gid_map_path, gid_map.as_bytes(), 4);
                if libc::mount(
                    std::ptr::null(),
                    root.as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ) != 0
                {
                    fail(5);
                }
                if libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                ) != 0
                {
                    fail(6);
                }
                Ok(())
            });
        }
        self
    }

    /// Override the wall-clock timeout. On expiry the child is
    /// `SIGKILL`ed and the test panics. The default is sized for small
    /// fixtures.
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Run the binary, capture output, return a [`CliOutput`].
    /// Does NOT assert on exit status - the caller does that.
    /// Panics on timeout (see [`Self::timeout`]).
    pub fn run(mut self) -> CliOutput {
        self.cmd.stdout(Stdio::piped());
        if !self.stderr_closed {
            self.cmd.stderr(Stdio::piped());
        }

        guard(&mut self.cmd);
        let mut child = self.cmd.spawn().expect("spawn pbfhogg binary");

        // Drain stdout/stderr in background threads so a chatty
        // child can't block on a full pipe buffer while we poll
        // `try_wait`. `Child::wait_with_output` does this for us
        // but consumes the child by value, which we can't afford -
        // we need to keep `child` available so the timeout path can
        // call `kill()`.
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take();

        let stdout_thread = std::thread::spawn(move || capture(stdout));
        let stderr_thread = stderr.map(|stderr| std::thread::spawn(move || capture(stderr)));

        let start = Instant::now();
        let status: ExitStatus = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {
                    if start.elapsed() > self.timeout {
                        child.kill().ok();
                        child.wait().ok();
                        let stderr_buf = stderr_thread
                            .map(|thread| thread.join().unwrap_or_default())
                            .unwrap_or_default();
                        panic!(
                            "pbfhogg timed out after {:?}; stderr:\n{}",
                            self.timeout,
                            String::from_utf8_lossy(&stderr_buf),
                        );
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(e) => panic!("error waiting for pbfhogg child: {e}"),
            }
        };
        assert_not_guard_killed(status);

        let stdout_buf = stdout_thread.join().unwrap_or_default();
        let stderr_buf = stderr_thread
            .map(|thread| thread.join().unwrap_or_default())
            .unwrap_or_default();

        CliOutput {
            status,
            stdout: stdout_buf,
            stderr: stderr_buf,
        }
    }

    #[cfg_attr(
        not(feature = "test-hooks"),
        allow(
            dead_code,
            reason = "only the test-hooks interruption tests start a child without waiting"
        )
    )]
    /// Start the binary without waiting for it, for a test that signals it
    /// mid-run. Stdout and stderr are captured as for [`Self::run`]. The
    /// child is killed and reaped if the handle drops while it still runs, so
    /// a failing assertion never leaves one behind.
    pub fn spawn(mut self) -> RunningCli {
        self.cmd.stdout(Stdio::piped());
        self.cmd.stderr(Stdio::piped());
        guard(&mut self.cmd);
        let mut child = self.cmd.spawn().expect("spawn pbfhogg binary");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        RunningCli {
            child: Some(child),
            stdout_thread: Some(std::thread::spawn(move || capture(stdout))),
            stderr_thread: Some(std::thread::spawn(move || capture(stderr))),
            timeout: self.timeout,
        }
    }

    /// Run and assert exit status is success. Returns captured output
    /// so the test can inspect stdout or stderr.
    pub fn assert_success(self) -> CliOutput {
        let out = self.run();
        assert!(
            out.status.success(),
            "pbfhogg exited non-zero ({}); stderr:\n{}",
            out.status,
            out.stderr_str(),
        );
        out
    }

    /// Run and assert exit status is failure. Returns captured output
    /// so the test can inspect the error message.
    pub fn assert_failure(self) -> CliOutput {
        let out = self.run();
        assert!(
            !out.status.success(),
            "pbfhogg exited successfully but test expected failure; stdout:\n{}\nstderr:\n{}",
            out.stdout_str(),
            out.stderr_str(),
        );
        out
    }
}

impl Default for CliInvoker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(
    not(feature = "test-hooks"),
    allow(
        dead_code,
        reason = "only the test-hooks interruption tests start a child without waiting"
    )
)]
/// A `pbfhogg` child started by [`CliInvoker::spawn`].
pub struct RunningCli {
    child: Option<std::process::Child>,
    stdout_thread: Option<std::thread::JoinHandle<Vec<u8>>>,
    stderr_thread: Option<std::thread::JoinHandle<Vec<u8>>>,
    timeout: Duration,
}

#[cfg_attr(
    not(feature = "test-hooks"),
    allow(
        dead_code,
        reason = "only the test-hooks interruption tests start a child without waiting"
    )
)]
impl RunningCli {
    /// Send `signal` (a `libc::SIG*` number) to the child.
    pub fn signal(&self, signal: libc::c_int) {
        let child = self.child.as_ref().expect("child is running");
        let pid = libc::pid_t::try_from(child.id()).expect("pid fits pid_t");
        // SAFETY: plain syscall on a child this handle owns and has not
        // reaped, so the pid cannot have been reused.
        let rc = unsafe { libc::kill(pid, signal) };
        assert_eq!(rc, 0, "kill({pid}, {signal}) failed");
    }

    /// Whether the child has exited (it is reaped if so).
    pub fn has_exited(&mut self) -> bool {
        let child = self.child.as_mut().expect("child is running");
        matches!(child.try_wait(), Ok(Some(_)))
    }

    /// Wait for the child to exit within the invoker's timeout and return
    /// its output. On timeout the child is killed and the test panics.
    pub fn wait(mut self) -> CliOutput {
        let mut child = self.child.take().expect("child is running");
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if start.elapsed() > self.timeout => {
                    child.kill().ok();
                    child.wait().ok();
                    let stderr = self
                        .stderr_thread
                        .take()
                        .map(|t| t.join().unwrap_or_default())
                        .unwrap_or_default();
                    panic!(
                        "pbfhogg did not exit within {:?} of being waited for; stderr:\n{}",
                        self.timeout,
                        String::from_utf8_lossy(&stderr)
                    );
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(e) => panic!("error waiting for pbfhogg child: {e}"),
            }
        };
        assert_not_guard_killed(status);
        let stdout = self
            .stdout_thread
            .take()
            .map(|t| t.join().unwrap_or_default())
            .unwrap_or_default();
        let stderr = self
            .stderr_thread
            .take()
            .map(|t| t.join().unwrap_or_default())
            .unwrap_or_default();
        CliOutput {
            status,
            stdout,
            stderr,
        }
    }
}

impl Drop for RunningCli {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().ok();
            child.wait().ok();
        }
    }
}

/// Captured stdout/stderr/exit status from a [`CliInvoker::run`].
pub struct CliOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CliOutput {
    /// Stderr as a UTF-8 string (lossy).
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Stdout as a UTF-8 string (lossy).
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Assert stderr contains the given substring.
    pub fn assert_stderr_contains(&self, needle: &str) -> &Self {
        let haystack = self.stderr_str();
        assert!(
            haystack.contains(needle),
            "stderr did not contain {needle:?}; stderr was:\n{haystack}",
        );
        self
    }

    /// Assert stdout contains the given substring.
    pub fn assert_stdout_contains(&self, needle: &str) -> &Self {
        let haystack = self.stdout_str();
        assert!(
            haystack.contains(needle),
            "stdout did not contain {needle:?}; stdout was:\n{haystack}",
        );
        self
    }

    /// A structured platform setup refusal (CLI exit code 3).
    pub fn is_o_direct_unsupported(&self) -> bool {
        self.platform_unsupported("direct-io")
    }

    /// A structured io_uring setup refusal (CLI exit code 3).
    pub fn is_uring_unsupported(&self) -> bool {
        // This writer also requires an O_DIRECT output file.
        self.platform_unsupported("io-uring") || self.platform_unsupported("direct-io")
    }

    fn platform_unsupported(&self, feature: &str) -> bool {
        self.status.code() == Some(3)
            && self
                .stderr_str()
                .contains(&format!("platform feature {feature} unavailable:"))
    }
}

/// Put `cmd` under the promise guard (see `guard`), registered last so that
/// no other `pre_exec` hook runs after its filter.
fn guard(cmd: &mut Command) {
    #[cfg(target_os = "linux")]
    crate::guard::apply(cmd);
    #[cfg(not(target_os = "linux"))]
    let _ = cmd;
}

/// Fail the test when the promise guard killed the child: it started a
/// process, or touched the descriptor it inherited.
fn assert_not_guard_killed(status: ExitStatus) {
    #[cfg(target_os = "linux")]
    assert!(
        !crate::guard::killed_by_guard(status),
        "pbfhogg died of SIGSYS under the promise guard: it started a process, \
         or closed, duplicated, passed or reflagged the descriptor it inherited \
         (reference/cli-reference.md, Names pbfhogg leaves behind)"
    );
    #[cfg(not(target_os = "linux"))]
    let _ = status;
}

/// The `pbfhogg` binary cargo built for this test build.
pub fn pbfhogg_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_pbfhogg"))
}

/// The top-level subcommands production parses, in declaration order, as
/// `pbfhogg --help` lists them: hidden ones and clap's generated `help`
/// left out.
pub fn subcommands() -> Vec<String> {
    listed(&pbfhogg_cli::cli_command())
        .map(|sub| sub.get_name().to_owned())
        .collect()
}

/// Every subcommand path production parses, nested ones (`inspect tags`)
/// included, each parent listed after its own children; hidden ones and
/// `help` are left out at every level.
pub fn subcommand_paths() -> Vec<Vec<String>> {
    fn walk(cmd: &clap::Command, prefix: &[String], out: &mut Vec<Vec<String>>) {
        for sub in listed(cmd) {
            let mut path = prefix.to_vec();
            path.push(sub.get_name().to_owned());
            walk(sub, &path, out);
            out.push(path);
        }
    }
    let mut paths = Vec::new();
    walk(&pbfhogg_cli::cli_command(), &[], &mut paths);
    assert!(paths.len() >= 20, "the clap tree lists only {paths:?}");
    paths
}

/// The subcommands of `cmd` that its `--help` lists.
fn listed(cmd: &clap::Command) -> impl Iterator<Item = &clap::Command> {
    cmd.get_subcommands()
        .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
}

mod tests {
    use super::{CliInvoker, CliOutput};

    /// The binary cargo handed this build runs, and stdout capture and the
    /// assertion helpers work on a trivial invocation.
    #[test]
    fn cli_invoker_runs_version_command() {
        let out = CliInvoker::new().arg("--version").assert_success();
        out.assert_stdout_contains("pbfhogg");
    }

    #[cfg(unix)]
    #[test]
    fn unrelated_failures_are_never_platform_skips() {
        use std::os::unix::process::ExitStatusExt;
        for code in [1, 2, 3] {
            let output = CliOutput {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: Vec::new(),
                stderr: b"Invalid argument EINVAL: not supported; io_uring creation failed"
                    .to_vec(),
            };
            assert!(!output.is_o_direct_unsupported());
            assert!(!output.is_uring_unsupported());
        }
    }
}
