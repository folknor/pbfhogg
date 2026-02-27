use std::path::Path;
use std::process::Command;

use crate::error::DevError;

/// Structured git state for the benchmark harness.
pub struct GitInfo {
    /// Short hash from `git rev-parse --short HEAD`.
    pub commit: String,
    /// First line of the commit message.
    pub subject: String,
    /// True when the working tree has no staged or unstaged changes.
    pub is_clean: bool,
}

/// Collect git information from the working directory.
pub fn collect(workspace_root: &Path) -> Result<GitInfo, DevError> {
    let commit = read_commit_hash(workspace_root)?;
    let subject = read_commit_subject(workspace_root)?;
    let is_clean = check_clean(workspace_root);

    Ok(GitInfo {
        commit,
        subject,
        is_clean,
    })
}

fn read_commit_hash(workspace_root: &Path) -> Result<String, DevError> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(workspace_root)
        .output()
        .map_err(DevError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DevError::Build(format!(
            "git rev-parse failed: {stderr}"
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn read_commit_subject(workspace_root: &Path) -> Result<String, DevError> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%s"])
        .current_dir(workspace_root)
        .output()
        .map_err(DevError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DevError::Build(format!("git log failed: {stderr}")));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn check_clean(workspace_root: &Path) -> bool {
    let unstaged = Command::new("git")
        .args(["diff", "--quiet", "HEAD"])
        .current_dir(workspace_root)
        .output();

    let staged = Command::new("git")
        .args(["diff", "--quiet", "--cached", "HEAD"])
        .current_dir(workspace_root)
        .output();

    let unstaged_ok = unstaged
        .as_ref()
        .ok()
        .map_or(false, |o| o.status.success());

    let staged_ok = staged
        .as_ref()
        .ok()
        .map_or(false, |o| o.status.success());

    unstaged_ok && staged_ok
}
