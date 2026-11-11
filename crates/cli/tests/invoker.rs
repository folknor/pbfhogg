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
//! CLI-driving test lives in this package: the library package's tests get
//! no such path.
//!
//! The subcommand lists the command-table tests check their tables
//! against come from the clap tree production parses
//! ([`pbfhogg_cli::cli_command`]), never from `--help` text.
//!
//! Scratch ownership comes from the dev-only test-scratch leaf; child and
//! capture ownership is shared with the bounded reference-tool runner.
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

use crate::common::process::OwnedChild;
use pbfhogg_test_scratch::TestScratch;
use std::ffi::OsStr;
use std::num::NonZeroU64;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

/// The exit status base of a [`CliInvoker::bind_mounted`] child whose
/// namespace setup the host refused; the failed step is added.
#[cfg(target_os = "linux")]
pub const NAMESPACE_DENIED: i32 = 100;

/// The exit status base of a [`CliInvoker::bind_mounted`] child whose
/// namespace setup failed for any other reason; the failed step is added.
#[cfg(target_os = "linux")]
pub const NAMESPACE_BROKEN: i32 = 110;

/// The CLI's runtime-failure status (`docs/cli/commands.md`, Exit codes).
pub const EXIT_FAILED: i32 = 2;

/// The CLI's usage-error status, `EX_USAGE`: the command line was refused
/// by clap or by the CLI's own option validation.
pub const EXIT_USAGE: i32 = 64;

/// Invokes the `pbfhogg` binary with a set of arguments, captures
/// stdout/stderr, returns a [`CliOutput`] that assertions read from.
pub struct CliInvoker<'a> {
    scratch: &'a TestScratch,
    cmd: Command,
    timeout: Duration,
    stderr_closed: bool,
}

impl<'a> CliInvoker<'a> {
    /// Start a new invocation. Default timeout is
    /// [`pbfhogg::TEST_CHILD_TIMEOUT`]; override via [`Self::timeout`].
    pub fn new(scratch: &'a TestScratch) -> Self {
        let mut cmd = Command::new(pbfhogg_bin());
        crate::common::process::isolate(&mut cmd, scratch);
        Self {
            scratch,
            cmd,
            timeout: pbfhogg::TEST_CHILD_TIMEOUT,
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

    /// Park the thread reaching checkpoint `ordinal`: the child writes
    /// `ready` and waits for cancellation; `stuck` keeps it parked after
    /// cancellation (writing `ready` plus `.ack`). The hook's wire form is
    /// colon-separated, so `ready` must be UTF-8 without a colon.
    #[allow(dead_code)]
    pub fn cancel_pause(mut self, ordinal: NonZeroU64, ready: &Path, stuck: bool) -> Self {
        let ready = ready
            .to_str()
            .filter(|r| !r.is_empty() && !r.contains(':'))
            .expect("cancel pause ready path is nonempty UTF-8 without a colon");
        let suffix = if stuck { ":stuck" } else { "" };
        self.cmd.env(
            "PBFHOGG_TEST_CANCEL_PAUSE",
            format!("{ordinal}:{ready}{suffix}"),
        );
        self
    }

    /// Count checkpoints passed and write the count to `path` at exit.
    #[allow(dead_code)]
    pub fn cancel_count(mut self, path: &Path) -> Self {
        self.cmd.env("PBFHOGG_TEST_CANCEL_COUNT", path);
        self
    }

    #[allow(dead_code)]
    pub fn publish_faults(mut self, value: impl AsRef<OsStr>) -> Self {
        self.cmd.env("PBFHOGG_TEST_PUBLISH_FAULTS", value);
        self
    }

    #[allow(dead_code)]
    pub fn publish_steps(mut self, path: &Path) -> Self {
        self.cmd.env("PBFHOGG_TEST_PUBLISH_STEPS", path);
        self
    }

    #[allow(dead_code)]
    pub fn publish_pause(mut self, value: impl AsRef<OsStr>) -> Self {
        self.cmd.env("PBFHOGG_TEST_PUBLISH_PAUSE", value);
        self
    }

    #[allow(dead_code)]
    pub fn publish_ready(mut self, path: &Path) -> Self {
        self.cmd.env("PBFHOGG_TEST_PUBLISH_READY", path);
        self
    }

    /// Replace `input` with `replacement` after its `after`-th open. The
    /// CLI refuses a partial arming, so the three travel together.
    #[allow(dead_code)]
    pub fn replace_input(mut self, input: &Path, replacement: &Path, after: NonZeroU64) -> Self {
        self.cmd
            .env("PBFHOGG_TEST_REPLACE_INPUT", input)
            .env("PBFHOGG_TEST_REPLACE_WITH", replacement)
            .env("PBFHOGG_TEST_REPLACE_AFTER", after.to_string());
        self
    }

    #[allow(dead_code)]
    pub fn force_pin_path_reopen(mut self, value: bool) -> Self {
        self.cmd.env(
            "PBFHOGG_TEST_FORCE_PIN_PATH_REOPEN",
            if value { "1" } else { "0" },
        );
        self
    }

    #[allow(dead_code)]
    pub fn merge_changes_spill_first(mut self, value: u64) -> Self {
        self.cmd
            .env("PBFHOGG_TEST_MERGE_CHANGES_SPILL_FIRST", value.to_string());
        self
    }

    #[allow(dead_code)]
    pub fn replace_report(mut self, path: &Path) -> Self {
        self.cmd.env("PBFHOGG_TEST_REPLACE_REPORT", path);
        self
    }

    #[allow(dead_code)]
    pub fn marker_fifo(mut self, value: impl AsRef<OsStr>) -> Self {
        self.cmd.env("BROKKR_MARKER_FIFO", value);
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

    /// Cap the child's open-file limit at `limit` (`RLIMIT_NOFILE`, soft and
    /// hard both, never above the current hard limit), so its own raise to
    /// the hard limit cannot lift it: a fixed descriptor budget for tests of
    /// up-front descriptor admission. Lowering a limit needs no privilege
    /// and reaches only the child, so the test binary's own limit is
    /// untouched.
    #[cfg(target_os = "linux")]
    pub fn open_file_limit(mut self, limit: u64) -> Self {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure runs between fork and exec and calls only
        // async-signal-safe functions (`getrlimit`, `setrlimit`).
        unsafe {
            self.cmd.pre_exec(move || {
                let mut current: libc::rlimit = std::mem::zeroed();
                if libc::getrlimit(libc::RLIMIT_NOFILE, &mut current) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let capped = limit.min(current.rlim_max);
                current.rlim_cur = capped;
                current.rlim_max = capped;
                if libc::setrlimit(libc::RLIMIT_NOFILE, &current) != 0 {
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
    pub fn run(self) -> CliOutput {
        self.spawn().wait()
    }

    /// Start a child owned by the borrowed scratch scope.
    pub fn spawn(mut self) -> RunningCli<'a> {
        self.cmd.stdout(Stdio::piped());
        if !self.stderr_closed {
            self.cmd.stderr(Stdio::piped());
        }
        guard(&mut self.cmd);
        RunningCli {
            owner: OwnedChild::spawn(&mut self.cmd, self.scratch).expect("spawn pbfhogg"),
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

/// A child whose lifetime cannot exceed its scratch guard.
pub struct RunningCli<'a> {
    owner: OwnedChild<'a>,
    timeout: Duration,
}

#[allow(dead_code)]
impl RunningCli<'_> {
    pub fn signal(&self, signal: libc::c_int) {
        self.owner.signal(signal);
    }

    pub fn has_exited(&mut self) -> bool {
        self.owner.has_exited().expect("inspect child")
    }

    pub fn wait(mut self) -> CliOutput {
        let out = self.owner.wait(self.timeout).expect("wait for pbfhogg");
        assert_not_guard_killed(out.status);
        CliOutput {
            status: out.status,
            stdout: out.stdout,
            stderr: out.stderr,
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
         (docs/cli/commands.md, Names pbfhogg leaves behind)"
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
        let invocation_scratch = pbfhogg_test_scratch::scratch();
        let out = CliInvoker::new(&invocation_scratch)
            .arg("--version")
            .assert_success();
        out.assert_stdout_contains("pbfhogg");
    }

    #[test]
    fn cli_invoker_reaps_on_wait_error_and_assertion_unwind() {
        let scratch = pbfhogg_test_scratch::scratch();
        let mut running = CliInvoker::new(&scratch).arg("--version").spawn();
        let pid = libc::pid_t::try_from(running.owner.id()).expect("pid");
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            running
                .owner
                .wait_with_check(pbfhogg::TEST_CHILD_TIMEOUT, |_| {
                    Err(std::io::Error::other("injected wait error"))
                })
                .expect("injected failure");
        }));
        assert!(failure.is_err());
        let mut status = 0;
        // SAFETY: WNOHANG never blocks; the owner should already have reaped.
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    /// The input gate controls completion independently of host speed. Keeping
    /// it open without writing also exercises the independent timeout path.
    #[test]
    fn owned_child_blocks_for_completion_and_times_out_a_gated_child() {
        use super::OwnedChild;
        use std::io::Write;
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;
        use std::process::{Command, Stdio};

        let scratch = pbfhogg_test_scratch::scratch();
        for release in [true, false] {
            let (reader, mut gate) = UnixStream::pair().expect("input gate");
            let mut command = Command::new("/bin/sh");
            crate::common::process::isolate(&mut command, &scratch);
            command
                .args(["-c", "read value; printf '%s' \"$value\""])
                .stdin(Stdio::from(OwnedFd::from(reader)))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = OwnedChild::spawn(&mut command, &scratch).expect("gated child");
            if release {
                gate.write_all(b"released\n").expect("release input gate");
                let output = child.wait(pbfhogg::TEST_CHILD_TIMEOUT).expect("completion");
                assert!(output.status.success());
                assert_eq!(output.stdout, b"released");
                assert!(output.stderr.is_empty());
                // A status cached by an earlier wait must not call waitid on
                // a PID that has already been reaped.
                let cached = child
                    .wait(pbfhogg::TEST_CHILD_TIMEOUT)
                    .expect("cached exit");
                assert!(cached.status.success());
            } else {
                let error = child.wait(std::time::Duration::ZERO).expect_err("timeout");
                assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
                // The timeout only signals the kill; reaping is a later wait.
                let killed = child
                    .wait(pbfhogg::TEST_CHILD_TIMEOUT)
                    .expect("killed child exits");
                assert!(!killed.status.success());
            }
        }
    }

    #[test]
    fn cli_invoker_has_an_explicit_execution_policy() {
        let scratch = pbfhogg_test_scratch::scratch();
        let invoker = CliInvoker::new(&scratch);
        assert_eq!(invoker.cmd.get_current_dir(), Some(scratch.path()));
        let env: std::collections::BTreeMap<_, _> = invoker.cmd.get_envs().collect();
        for key in env.keys() {
            assert!(matches!(
                key.to_str(),
                Some(
                    "PATH"
                        | "HOME"
                        | "LANG"
                        | "LC_ALL"
                        | "LC_CTYPE"
                        | "LC_MESSAGES"
                        | "TZ"
                        | "TMPDIR"
                )
            ));
        }
        assert_eq!(
            env.get(std::ffi::OsStr::new("TMPDIR")),
            Some(&Some(scratch.path().as_os_str()))
        );
    }

    #[test]
    fn cli_invoker_passes_non_utf8_paths_without_conversion() {
        use std::os::unix::ffi::OsStringExt;
        let scratch = pbfhogg_test_scratch::scratch();
        let input = scratch.child(std::ffi::OsString::from_vec(b"input-\xff.pbf".to_vec()));
        crate::common::write_base_pbf(&input);
        CliInvoker::new(&scratch)
            .arg("check")
            .arg(&input)
            .assert_success();
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
