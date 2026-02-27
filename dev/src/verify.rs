//! Verify harness: cross-validate pbfhogg output against reference tools.
//!
//! Provides [`VerifyHarness`] — a shared context for verify subcommands that
//! handles locking, building the CLI binary, and common operations like
//! running pbfhogg/external tools, diffing PBFs, and checking sort order.

use std::fs;
use std::path::{Path, PathBuf};

use crate::build;
use crate::config::ResolvedPaths;
use crate::error::DevError;
use crate::output;
use crate::output::CapturedOutput;

// ---------------------------------------------------------------------------
// VerifyHarness
// ---------------------------------------------------------------------------

/// Shared context for verify subcommands.
///
/// Holds the exclusive lock (preventing concurrent bench/verify runs),
/// the path to the freshly-built CLI binary, and the output directory
/// under `target/verify/`.
pub struct VerifyHarness {
    /// RAII lock — released on drop.
    _lock: crate::lockfile::LockGuard,
    /// Path to the built `pbfhogg` release binary.
    pub binary: PathBuf,
    /// Root output directory: `{target_dir}/verify`.
    pub output_dir: PathBuf,
    /// Workspace root (used as cwd for subprocess invocations).
    pub workspace_root: PathBuf,
}

impl VerifyHarness {
    /// Build the CLI binary and prepare the verify output directory.
    ///
    /// Acquires an exclusive lock via [`crate::lockfile::acquire`] so that
    /// no other dev/bench/verify process runs concurrently.
    pub fn new(
        paths: &ResolvedPaths,
        workspace_root: &Path,
        target_dir: &Path,
    ) -> Result<Self, DevError> {
        let lock = crate::lockfile::acquire(&paths.scratch_dir)?;
        let binary = build::cargo_build(&build::BuildConfig::release_cli(), workspace_root)?;
        let output_dir = target_dir.join("verify");

        Ok(Self {
            _lock: lock,
            binary,
            output_dir,
            workspace_root: workspace_root.to_path_buf(),
        })
    }

    // -- Subprocess runners ------------------------------------------------

    /// Run the pbfhogg CLI with the given arguments.
    ///
    /// Does **not** check the exit status — the caller decides whether
    /// non-zero is an error (some commands exit non-zero normally).
    pub fn run_pbfhogg(&self, args: &[&str]) -> Result<CapturedOutput, DevError> {
        output::run_captured(
            &self.binary.display().to_string(),
            args,
            &self.workspace_root,
        )
    }

    /// Run an external tool (e.g. `osmium`, `osmconvert`) with the given arguments.
    ///
    /// Does **not** check the exit status.
    pub fn run_tool(&self, program: &str, args: &[&str]) -> Result<CapturedOutput, DevError> {
        output::run_captured(program, args, &self.workspace_root)
    }

    // -- Common verify operations ------------------------------------------

    /// Print extended fileinfo for a PBF, prefixed with `label`.
    ///
    /// Runs `pbfhogg fileinfo --extended <pbf>`. On failure, prints the
    /// error but does **not** propagate it (informational only).
    pub fn print_fileinfo(&self, label: &str, pbf: &Path) -> Result<(), DevError> {
        let pbf_str = pbf.display().to_string();
        let captured = self.run_pbfhogg(&["fileinfo", "--extended", &pbf_str])?;

        if captured.status.success() {
            let stdout = String::from_utf8_lossy(&captured.stdout);
            for line in stdout.lines() {
                output::verify_msg(&format!("  {label}: {line}"));
            }
        } else {
            let stderr = String::from_utf8_lossy(&captured.stderr);
            output::error(&format!("fileinfo failed for {label}: {stderr}"));
        }

        Ok(())
    }

    /// Diff two PBF files using `pbfhogg diff --suppress-common`.
    ///
    /// Returns `Ok(true)` if the files are identical (empty diff output),
    /// `Ok(false)` if differences were found (prints the diff), or an error
    /// only if the subprocess fails to spawn.
    pub fn diff_pbfs(&self, a: &Path, b: &Path) -> Result<bool, DevError> {
        let a_str = a.display().to_string();
        let b_str = b.display().to_string();
        let captured = self.run_pbfhogg(&["diff", "--suppress-common", &a_str, &b_str])?;

        let stdout = String::from_utf8_lossy(&captured.stdout);
        if stdout.trim().is_empty() {
            Ok(true)
        } else {
            for line in stdout.lines() {
                output::verify_msg(line);
            }
            Ok(false)
        }
    }

    /// Check whether a PBF is marked as sorted (Sort.Type_then_ID).
    ///
    /// Runs `pbfhogg fileinfo <pbf>` and searches for the sort marker in
    /// stdout. Prints a PASS/FAIL message and returns the result.
    pub fn check_sorted(&self, label: &str, pbf: &Path) -> Result<bool, DevError> {
        let pbf_str = pbf.display().to_string();
        let captured = self.run_pbfhogg(&["fileinfo", &pbf_str])?;

        let stdout = String::from_utf8_lossy(&captured.stdout);
        let sorted = stdout.contains("Sort.Type_then_ID");

        if sorted {
            output::verify_msg(&format!("  {label}: sorted (Sort.Type_then_ID) PASS"));
        } else {
            output::verify_msg(&format!("  {label}: NOT sorted FAIL"));
        }

        Ok(sorted)
    }

    /// Compare the sort flag between a pbfhogg-produced PBF and a reference PBF.
    ///
    /// Returns `false` (fail) only if the reference has Sort.Type_then_ID but
    /// the pbfhogg output does not. All other combinations pass.
    pub fn compare_sort_feature(
        &self,
        pbfhogg_pbf: &Path,
        other_pbf: &Path,
    ) -> Result<bool, DevError> {
        let ours = self.has_sort_flag(pbfhogg_pbf)?;
        let theirs = self.has_sort_flag(other_pbf)?;

        output::verify_msg(&format!(
            "  pbfhogg sorted={ours}, reference sorted={theirs}"
        ));

        // Fail only if reference is sorted but we are not.
        if theirs && !ours {
            output::verify_msg("  FAIL: reference is sorted but pbfhogg output is not");
            Ok(false)
        } else {
            Ok(true)
        }
    }

    // -- Directory helpers -------------------------------------------------

    /// Create (if needed) a subdirectory under `output_dir` and return its path.
    pub fn subdir(&self, name: &str) -> Result<PathBuf, DevError> {
        let dir = self.output_dir.join(name);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    // -- Exit-status helper ------------------------------------------------

    /// Assert that a captured subprocess exited successfully.
    ///
    /// Returns `DevError::Subprocess` with the program name and stderr if the
    /// exit code was non-zero (or the process was killed by a signal).
    pub fn check_exit(&self, captured: &CapturedOutput, program: &str) -> Result<(), DevError> {
        if captured.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&captured.stderr);
        Err(DevError::Subprocess {
            program: program.to_owned(),
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        })
    }

    // -- Internal helpers --------------------------------------------------

    /// Run `fileinfo` and return whether stdout contains "Sort.Type_then_ID".
    fn has_sort_flag(&self, pbf: &Path) -> Result<bool, DevError> {
        let pbf_str = pbf.display().to_string();
        let captured = self.run_pbfhogg(&["fileinfo", &pbf_str])?;
        let stdout = String::from_utf8_lossy(&captured.stdout);
        Ok(stdout.contains("Sort.Type_then_ID"))
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Check whether an executable exists on `PATH`.
pub fn which_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(false, |s| s.success())
}
