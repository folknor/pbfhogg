//! Invoke the `pbfhogg` binary from integration tests.
//!
//! Integration tests that drive the CLI (rather than the library
//! directly) go through this helper. It finds the compiled binary
//! produced by the `pbfhogg-cli` workspace member and wraps
//! [`std::process::Command`] with a fluent builder plus assertion
//! helpers.
//!
//! Locating the binary: [`CARGO_TARGET_DIR`] env var if set,
//! otherwise `{CARGO_MANIFEST_DIR}/target`, plus `debug` or `release`
//! picked from `cfg(debug_assertions)`. `brokkr check` and
//! `brokkr test` both build the binary as part of the workspace test
//! run, so it exists by the time a test starts. If somebody runs
//! `cargo test -p pbfhogg` in isolation and the binary is missing,
//! [`CliInvoker::new`] panics with a message pointing at the fix.
//!
//! No external crate dependency - kept deliberately small to match
//! the project's minimal dev-dep posture.
//!
//! ## Timeout
//!
//! Every invocation has a wall-clock timeout (default 60 s,
//! [`CliInvoker::timeout`] overrides). On expiry the child is sent
//! `SIGKILL` and the test panics with a clear "timed out" message
//! instead of wedging the test runner. This is load-bearing for
//! `brokkr check`: a hung CLI test would block every subsequent test
//! in the binary indefinitely.
//!
//! ## Platform skips
//!
//! [`CliOutput::is_o_direct_unsupported`] and
//! [`CliOutput::is_uring_unsupported`] match the CLI's error strings
//! for the two platform-dependent flags pbfhogg exposes. Tests that
//! exercise `--direct-io` or `--io-uring` should consult these
//! before asserting success - tmpfs / overlayfs and hosts with low
//! `RLIMIT_MEMLOCK` are common.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Default wall-clock timeout applied to every [`CliInvoker`] that
/// doesn't override via [`CliInvoker::timeout`]. Sized for Tier 1 /
/// Tier 2 fixtures; tests that exercise real datasets should bump
/// it explicitly.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Polling cadence for [`std::process::Child::try_wait`] inside the
/// timeout loop. 20 ms is fast enough that test wall-time isn't
/// noticeably padded; slow enough that the loop doesn't burn CPU on
/// a long-running command.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Invokes the `pbfhogg` binary with a set of arguments, captures
/// stdout/stderr, returns a [`CliOutput`] that assertions read from.
pub struct CliInvoker {
    cmd: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
}

impl CliInvoker {
    /// Start a new invocation. Default timeout is
    /// [`DEFAULT_TIMEOUT`]; override via [`Self::timeout`].
    pub fn new() -> Self {
        Self {
            cmd: Command::new(pbfhogg_bin()),
            stdin: None,
            timeout: DEFAULT_TIMEOUT,
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

    /// Set bytes to feed on the binary's stdin.
    pub fn stdin_bytes(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    /// Override the wall-clock timeout. On expiry the child is
    /// `SIGKILL`ed and the test panics. Use a generous value for
    /// real-dataset tests; the default is sized for small fixtures.
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Run the binary, capture output, return a [`CliOutput`].
    /// Does NOT assert on exit status - the caller does that.
    /// Panics on timeout (see [`Self::timeout`]).
    pub fn run(mut self) -> CliOutput {
        if self.stdin.is_some() {
            self.cmd.stdin(Stdio::piped());
        }
        self.cmd.stdout(Stdio::piped());
        self.cmd.stderr(Stdio::piped());

        let mut child = self.cmd.spawn().expect("spawn pbfhogg binary");

        if let Some(bytes) = self.stdin {
            use std::io::Write;
            let mut stdin = child.stdin.take().expect("stdin piped");
            stdin.write_all(&bytes).expect("write stdin");
            drop(stdin);
        }

        // Drain stdout/stderr in background threads so a chatty
        // child can't block on a full pipe buffer while we poll
        // `try_wait`. `Child::wait_with_output` does this for us
        // but consumes the child by value, which we can't afford -
        // we need to keep `child` available so the timeout path can
        // call `kill()`.
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        let stdout_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            stdout.read_to_end(&mut buf).ok();
            buf
        });
        let stderr_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            stderr.read_to_end(&mut buf).ok();
            buf
        });

        let start = Instant::now();
        let status: ExitStatus = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {
                    if start.elapsed() > self.timeout {
                        child.kill().ok();
                        child.wait().ok();
                        let stderr_buf = stderr_thread.join().unwrap_or_default();
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

        let stdout_buf = stdout_thread.join().unwrap_or_default();
        let stderr_buf = stderr_thread.join().unwrap_or_default();

        CliOutput {
            status,
            stdout: stdout_buf,
            stderr: stderr_buf,
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

    /// True when the command failed with stderr matching the
    /// O_DIRECT-unsupported pattern (typical on tmpfs / overlayfs /
    /// some CI filesystems that reject `O_DIRECT` with `EINVAL`).
    /// Pure check; the caller logs and returns when it sees true.
    pub fn is_o_direct_unsupported(&self) -> bool {
        if self.status.success() {
            return false;
        }
        let msg = self.stderr_str();
        msg.contains("Invalid argument") || msg.contains("EINVAL")
    }

    /// True when the command failed with stderr matching the
    /// io_uring-unavailable pattern (low `RLIMIT_MEMLOCK`, kernel
    /// without the required submission queue features, etc.).
    /// Pure check; the caller logs and returns when it sees true.
    pub fn is_uring_unsupported(&self) -> bool {
        if self.status.success() {
            return false;
        }
        let msg = self.stderr_str();
        msg.contains("RLIMIT_MEMLOCK")
            || msg.contains("kernel does not support")
            || msg.contains("not supported")
    }
}

/// Locate the compiled `pbfhogg` binary. Panics with a helpful
/// message if the binary is not present.
///
/// Resolution order:
///   1. `BROKKR_TEST_BIN_DIR` env var (set by `brokkr check` and
///      `brokkr test` per sweep, points at `<target>/<profile>`).
///      Authoritative whenever brokkr drives the run, including
///      sweeps with empty `build_packages`.
///   2. `CARGO_TARGET_DIR` (or `CARGO_MANIFEST_DIR/target`) plus
///      `cfg!(debug_assertions)` for plain `cargo test` runs.
///      The cfg heuristic conflates the test crate's profile with
///      the bin target's profile, so it can pick the wrong subdir;
///      brokkr-driven runs avoid this via path 1.
fn pbfhogg_bin() -> PathBuf {
    if let Some(d) = std::env::var_os("BROKKR_TEST_BIN_DIR") {
        let bin = PathBuf::from(d).join("pbfhogg");
        assert!(
            bin.exists(),
            "pbfhogg binary not found at {} (from BROKKR_TEST_BIN_DIR). \
             The brokkr sweep should have built it; check the sweep's \
             build_packages config.",
            bin.display(),
        );
        return bin;
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let bin = target.join(profile).join("pbfhogg");
    assert!(
        bin.exists(),
        "pbfhogg binary not found at {}. \
         Run `brokkr check` (or `cargo build -p pbfhogg-cli`) first. \
         Integration tests that drive the CLI rely on the binary \
         being built as part of the workspace test run.",
        bin.display(),
    );
    bin
}

/// Convenience: convert a path to an OsStr for passing as an arg.
/// Used by test code that builds command lines piecemeal.
pub fn path_arg(p: &Path) -> &OsStr {
    p.as_os_str()
}
