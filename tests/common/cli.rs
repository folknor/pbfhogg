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

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// Invokes the `pbfhogg` binary with a set of arguments, captures
/// stdout/stderr, returns a [`CliOutput`] that assertions read from.
pub struct CliInvoker {
    cmd: Command,
    stdin: Option<Vec<u8>>,
}

impl CliInvoker {
    /// Start a new invocation.
    pub fn new() -> Self {
        Self {
            cmd: Command::new(pbfhogg_bin()),
            stdin: None,
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

    /// Run the binary, capture output, return a [`CliOutput`].
    /// Does NOT assert on exit status - the caller does that.
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

        let out = child.wait_with_output().expect("wait for pbfhogg binary");
        CliOutput {
            status: out.status,
            stdout: out.stdout,
            stderr: out.stderr,
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
}

/// Locate the compiled `pbfhogg` binary. Panics with a helpful
/// message if the binary is not present.
fn pbfhogg_bin() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
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
