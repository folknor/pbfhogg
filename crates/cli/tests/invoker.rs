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

/// The cores a child sizes from by default, matching the library's test hosts.
pub const CHILD_CORES: NonZeroU64 = NonZeroU64::MIN.saturating_add(3);
/// The memory a child routes on by default, matching the library's test hosts.
pub const CHILD_MEMORY: u64 = 8 << 30;

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
        .resources(CHILD_CORES, Some(CHILD_MEMORY))
    }

    /// The sizing inputs the child resolves instead of probing the runner:
    /// `cores` for every resource-sized pool and the width of rayon's global
    /// pool (`RAYON_NUM_THREADS`, honoured by any build), `memory` (`None`
    /// for unknown) for memory-routed choices such as altw's auto backend.
    /// The resource override is a `test-hooks` hook
    /// (`PBFHOGG_TEST_RESOURCES`); a build without the feature ignores it and
    /// sizes and routes from the machine. Fixed sizing inputs, not a thread
    /// cap: `-j` and concurrent stage threads can exceed `cores`.
    pub fn resources(mut self, cores: NonZeroU64, memory: Option<u64>) -> Self {
        let memory = memory.map_or_else(|| "none".to_owned(), |bytes| bytes.to_string());
        self.cmd
            .env("PBFHOGG_TEST_RESOURCES", format!("{cores}:{memory}"))
            .env("RAYON_NUM_THREADS", cores.to_string());
        self
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

    /// Arm only faults represented by the library's publication protocol.
    #[cfg(pbfhogg_test_hooks)]
    #[allow(dead_code)]
    pub fn publish_faults(mut self, faults: &[pbfhogg::test_hooks::PublishFault]) -> Self {
        use pbfhogg::test_hooks::PublishFault;
        let names: Vec<String> = faults
            .iter()
            .map(|fault| match fault {
                PublishFault::BeforeInstall => "before-install".to_owned(),
                PublishFault::MoveAside => "move-aside".to_owned(),
                PublishFault::AfterBackup => "after-backup".to_owned(),
                PublishFault::ForceLinkFallback => "force-link-fallback".to_owned(),
                PublishFault::AfterLink => "after-link".to_owned(),
                PublishFault::AfterInstalled(k) => format!("after-installed:{k}"),
                PublishFault::CrossMount(k) => format!("cross-mount:{k}"),
                PublishFault::Restore => "restore".to_owned(),
                PublishFault::RestoreRename => "restore-rename".to_owned(),
                PublishFault::DirSync => "dir-sync".to_owned(),
                PublishFault::BackupSync => "backup-sync".to_owned(),
                PublishFault::BackupLinkSync => "backup-link-sync".to_owned(),
                PublishFault::MoveAsideCrossMount => "move-aside-cross-mount".to_owned(),
                PublishFault::BackupRemoval => "backup-removal".to_owned(),
            })
            .collect();
        if names.is_empty() {
            self.cmd.env_remove("PBFHOGG_TEST_PUBLISH_FAULTS");
        } else {
            self.cmd.env("PBFHOGG_TEST_PUBLISH_FAULTS", names.join(","));
        }
        self
    }

    #[allow(dead_code)]
    pub fn publish_steps(mut self, path: &Path) -> Self {
        self.cmd.env("PBFHOGG_TEST_PUBLISH_STEPS", path);
        self
    }

    /// Pause at a typed publication step and announce it at `ready`.
    #[cfg(pbfhogg_test_hooks)]
    #[allow(dead_code)]
    pub fn publish_pause(mut self, step: pbfhogg::test_hooks::PublishStep, ready: &Path) -> Self {
        self.cmd
            .env("PBFHOGG_TEST_PUBLISH_PAUSE", step.to_string())
            .env("PBFHOGG_TEST_PUBLISH_READY", ready);
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

    /// A structured direct-io setup refusal (CLI exit code 3), the one result
    /// that lets an O_DIRECT cell skip. A match fails the test where the run
    /// requires direct-io (`common::fail_if_required`).
    pub fn is_o_direct_unsupported(&self) -> bool {
        self.refused(crate::common::Backend::DirectIo, &["direct-io"])
    }

    /// A structured io_uring setup refusal (CLI exit code 3), the one result
    /// that lets an io_uring cell skip. The ring writer also requires an
    /// O_DIRECT output file, so a direct-io refusal counts. A match fails the
    /// test where the run requires io-uring.
    pub fn is_uring_unsupported(&self) -> bool {
        self.refused(crate::common::Backend::IoUring, &["io-uring", "direct-io"])
    }

    fn refused(&self, cell: crate::common::Backend, accepted: &[&str]) -> bool {
        let stderr = self.stderr_str();
        let refused = self.status.code() == Some(3)
            && accepted.iter().any(|feature| {
                stderr.contains(&format!("platform feature {feature} unavailable:"))
            });
        if refused {
            crate::common::fail_if_required(cell, &stderr.trim_end());
        }
        refused
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

    #[cfg(pbfhogg_test_hooks)]
    #[test]
    fn typed_publication_faults_round_trip_through_the_child_protocol() {
        use pbfhogg::test_hooks::PublishFault::{self, *};
        let scratch = pbfhogg_test_scratch::scratch();
        let faults = [
            BeforeInstall,
            MoveAside,
            AfterBackup,
            ForceLinkFallback,
            AfterLink,
            AfterInstalled(2),
            CrossMount(3),
            Restore,
            RestoreRename,
            DirSync,
            BackupSync,
            BackupLinkSync,
            MoveAsideCrossMount,
            BackupRemoval,
        ];
        let invoker = CliInvoker::new(&scratch).publish_faults(&faults);
        let (_, value) = invoker
            .cmd
            .get_envs()
            .find(|(key, _)| *key == "PBFHOGG_TEST_PUBLISH_FAULTS")
            .expect("fault environment");
        let decoded: Vec<PublishFault> = value
            .expect("armed faults")
            .to_str()
            .expect("UTF-8 fault names")
            .split(',')
            .map(|name| name.parse().expect("publication fault spelling"))
            .collect();
        assert_eq!(decoded, faults);
        let cleared = invoker.publish_faults(&[]);
        // A cleared environment drops the entry; an inheriting one records a
        // removal. Either way no value may remain armed.
        assert!(
            !cleared
                .cmd
                .get_envs()
                .any(|(key, value)| { key == "PBFHOGG_TEST_PUBLISH_FAULTS" && value.is_some() })
        );
    }

    /// Malformed protocol values must fail before even a version-only run.
    /// Raw environment access stays private to this refusal test; callers
    /// configure valid publication hooks through the typed setters.
    #[cfg(pbfhogg_test_hooks)]
    #[test]
    fn malformed_publication_hooks_fail_explicitly() {
        let scratch = pbfhogg_test_scratch::scratch();
        for (key, value, message) in [
            (
                "PBFHOGG_TEST_PUBLISH_FAULTS",
                "unknown",
                "not a publication fault",
            ),
            ("PBFHOGG_TEST_PUBLISH_FAULTS", "", "not a publication fault"),
            (
                "PBFHOGG_TEST_PUBLISH_FAULTS",
                "after-link,",
                "not a publication fault",
            ),
            (
                "PBFHOGG_TEST_PUBLISH_FAULTS",
                "after-installed:no",
                "not a member index",
            ),
            (
                "PBFHOGG_TEST_PUBLISH_PAUSE",
                "unknown",
                "not a publication step",
            ),
            (
                "PBFHOGG_TEST_PUBLISH_PAUSE",
                "installed:no",
                "not a member index",
            ),
            (
                "PBFHOGG_TEST_PUBLISH_PAUSE",
                "installed:0",
                "publication pause needs",
            ),
            (
                "PBFHOGG_TEST_PUBLISH_READY",
                "ready",
                "publication pause needs",
            ),
        ] {
            let mut invoker = CliInvoker::new(&scratch).arg("--version");
            invoker.cmd.env(key, value);
            let out = invoker.run();
            assert_eq!(
                out.status.code(),
                Some(101),
                "{key}={value:?}: {}",
                out.stderr_str()
            );
            assert!(
                out.stderr_str().contains(message),
                "{key}={value:?}: {}",
                out.stderr_str()
            );
            assert!(out.stdout.is_empty());
        }
    }

    #[cfg(pbfhogg_test_hooks)]
    #[test]
    fn malformed_resources_hook_fails_explicitly() {
        let scratch = pbfhogg_test_scratch::scratch();
        for value in ["", "4", "0:none", "x:none", "4:", "4:lots", "4:none:1"] {
            let mut invoker = CliInvoker::new(&scratch).arg("--version");
            invoker.cmd.env("PBFHOGG_TEST_RESOURCES", value);
            let out = invoker.run();
            assert_eq!(
                out.status.code(),
                Some(101),
                "{value:?}: {}",
                out.stderr_str()
            );
            assert!(
                out.stderr_str()
                    .contains("is not <cores>:<memory bytes or none>"),
                "{value:?}: {}",
                out.stderr_str()
            );
            assert!(out.stdout.is_empty());
        }
    }

    #[cfg(pbfhogg_test_hooks)]
    #[test]
    fn malformed_checkpoint_and_replacement_hooks_fail_explicitly() {
        let scratch = pbfhogg_test_scratch::scratch();
        for (key, value, message) in [
            (
                "PBFHOGG_TEST_CANCEL_PAUSE",
                "0:ready",
                "positive checkpoint",
            ),
            ("PBFHOGG_TEST_CANCEL_PAUSE", "1:", "ready path"),
            (
                "PBFHOGG_TEST_CANCEL_PAUSE",
                "1:ready:other",
                "suffix must be stuck",
            ),
            ("PBFHOGG_TEST_CANCEL_COUNT", "", "cancel count needs a path"),
            (
                "PBFHOGG_TEST_REPLACE_INPUT",
                "input",
                "replacement needs input",
            ),
            (
                "PBFHOGG_TEST_REPLACE_REPORT",
                "report",
                "report requires an armed replacement",
            ),
            (
                "PBFHOGG_TEST_FORCE_PIN_PATH_REOPEN",
                "yes",
                "must be 0 or 1",
            ),
            (
                "PBFHOGG_TEST_MERGE_CHANGES_SPILL_FIRST",
                "no",
                "is not a count",
            ),
        ] {
            let mut invoker = CliInvoker::new(&scratch).arg("--version");
            invoker.cmd.env(key, value);
            let out = invoker.run();
            assert_eq!(
                out.status.code(),
                Some(101),
                "{key}={value:?}: {}",
                out.stderr_str()
            );
            assert!(
                out.stderr_str().contains(message),
                "{key}={value:?}: {}",
                out.stderr_str()
            );
            assert!(out.stdout.is_empty());
        }
    }

    #[cfg(all(pbfhogg_test_hooks, unix))]
    #[test]
    fn publication_hook_names_require_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let scratch = pbfhogg_test_scratch::scratch();
        for key in ["PBFHOGG_TEST_PUBLISH_FAULTS", "PBFHOGG_TEST_PUBLISH_PAUSE"] {
            let mut invoker = CliInvoker::new(&scratch).arg("--version");
            invoker.cmd.env(key, std::ffi::OsStr::from_bytes(b"\xff"));
            let out = invoker.run();
            assert_eq!(out.status.code(), Some(101), "{}", out.stderr_str());
            assert!(
                out.stderr_str().contains("must be UTF-8"),
                "{}",
                out.stderr_str()
            );
            assert!(out.stdout.is_empty());
        }
    }

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

    /// Every test child dies with the thread that spawned it, which is what
    /// lets an expired hang bound abort the binary without orphaning one:
    /// a child blocked forever on its input is killed when its spawning
    /// thread ends, before its owner (moved out of that thread) kills it.
    #[test]
    fn a_test_child_is_killed_when_its_spawning_thread_ends() {
        use super::OwnedChild;
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::ExitStatusExt;
        use std::process::{Command, Stdio};

        let scratch = pbfhogg_test_scratch::scratch();
        let (reader, _gate) = UnixStream::pair().expect("input gate");
        let mut child = std::thread::scope(|s| {
            s.spawn(|| {
                let mut command = Command::new("/bin/sh");
                crate::common::process::isolate(&mut command, &scratch);
                command
                    .args(["-c", "read value"])
                    .stdin(Stdio::from(OwnedFd::from(reader)));
                OwnedChild::spawn(&mut command, &scratch).expect("gated child")
            })
            .join()
            .expect("spawning thread")
        });
        let output = child
            .wait(pbfhogg::TEST_CHILD_TIMEOUT)
            .expect("the parent-death signal ends the child");
        assert_eq!(output.status.signal(), Some(libc::SIGKILL));
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
                        | "PBFHOGG_TEST_RESOURCES"
                        | "RAYON_NUM_THREADS"
                )
            ));
        }
        // The child sizes and routes from fixed inputs, not the runner.
        assert_eq!(
            env.get(std::ffi::OsStr::new("PBFHOGG_TEST_RESOURCES")),
            Some(&Some(std::ffi::OsStr::new("4:8589934592")))
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("RAYON_NUM_THREADS")),
            Some(&Some(std::ffi::OsStr::new("4")))
        );
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
