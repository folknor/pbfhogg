use std::path::{Path, PathBuf};

use crate::error::DevError;

/// A single requirement that must be satisfied before a subcommand runs.
pub enum Check {
    /// Binary must exist in PATH.
    Binary {
        name: &'static str,
        help: &'static str,
    },
    /// File must exist at path.
    File {
        path: PathBuf,
        description: String,
    },
    /// Minimum free disk space in bytes.
    DiskSpace {
        path: PathBuf,
        min_bytes: u64,
    },
    /// Read a /proc or /sys file and check it contains expected value.
    KernelParam {
        path: &'static str,
        expected: &'static str,
        description: &'static str,
    },
}

/// Run all checks, collecting failures. If any fail, return `DevError::Preflight`
/// with all failure messages (not just the first).
pub fn run_preflight(checks: &[Check]) -> Result<(), DevError> {
    let mut failures = Vec::new();

    for check in checks {
        if let Some(msg) = run_single(check) {
            failures.push(msg);
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(DevError::Preflight(failures))
    }
}

/// Run a single check. Returns `Some(message)` on failure, `None` on success.
fn run_single(check: &Check) -> Option<String> {
    match check {
        Check::Binary { name, help } => check_binary(name, help),
        Check::File { path, description } => check_file(path, description),
        Check::DiskSpace { path, min_bytes } => check_disk_space(path, *min_bytes),
        Check::KernelParam {
            path,
            expected,
            description,
        } => check_kernel_param(path, expected, description),
    }
}

fn check_binary(name: &str, help: &str) -> Option<String> {
    let result = std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => None,
        _ => Some(format!("'{name}' not found in PATH ({help})")),
    }
}

fn check_file(path: &Path, description: &str) -> Option<String> {
    if path.exists() {
        None
    } else {
        Some(format!("{description}: {}", path.display()))
    }
}

fn check_disk_space(path: &Path, min_bytes: u64) -> Option<String> {
    match available_bytes(path) {
        Some(avail) if avail >= min_bytes => None,
        Some(avail) => Some(format!(
            "insufficient disk space at {}: {} MB available, {} MB required",
            path.display(),
            avail / (1024 * 1024),
            min_bytes / (1024 * 1024),
        )),
        None => Some(format!(
            "could not check disk space at {}",
            path.display()
        )),
    }
}

/// Query available disk space via `libc::statvfs`.
fn available_bytes(path: &Path) -> Option<u64> {
    let c_path = path_to_cstring(path)?;

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };

    if ret != 0 {
        return None;
    }

    // f_bavail and f_frsize are both c_ulong (u64 on 64-bit Linux).
    Some(stat.f_bavail * stat.f_frsize)
}

fn check_kernel_param(path: &str, expected: &str, description: &str) -> Option<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => {
            return Some(format!(
                "{description}: could not read {path}"
            ));
        }
    };

    let trimmed = content.trim();
    if trimmed == expected {
        None
    } else {
        Some(format!(
            "{description}: expected '{expected}', got '{trimmed}' (in {path})"
        ))
    }
}

/// Convert a `PathBuf` to a `CString`, returning `None` if the path contains
/// interior nul bytes.
fn path_to_cstring(path: &Path) -> Option<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).ok()
}
