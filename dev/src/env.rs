use std::path::Path;
use std::process::Command;

use crate::config::{DevConfig, ResolvedPaths};

/// Environment information for the `dev env` subcommand.
pub struct EnvInfo {
    pub hostname: String,
    pub kernel: String,
    pub governor: String,
    pub memory_total_mb: u64,
    pub memory_available_mb: u64,
    pub io_uring_status: String,
    pub drives: Vec<(String, String)>,
    pub tools: Vec<(String, String)>,
    pub datasets: Vec<(String, DatasetStatus)>,
}

/// Whether a dataset PBF file exists on disk.
pub enum DatasetStatus {
    Present,
    Missing,
}

/// Collect all environment information.
pub fn collect(config: &DevConfig, paths: &ResolvedPaths) -> EnvInfo {
    let (mem_total, mem_avail) = read_memory();

    EnvInfo {
        hostname: paths.hostname.clone(),
        kernel: read_kernel(),
        governor: read_governor(),
        memory_total_mb: mem_total,
        memory_available_mb: mem_avail,
        io_uring_status: read_io_uring_status(),
        drives: collect_drives(paths),
        tools: collect_tools(),
        datasets: check_datasets(config, &paths.data_dir),
    }
}

/// Print environment info in formatted output.
pub fn print(info: &EnvInfo) {
    print_header(info);
    print_drives(info);
    print_tools(info);
    print_datasets(info);
}

fn print_header(info: &EnvInfo) {
    println!("{:<12} {}", "hostname:", info.hostname);
    println!("{:<12} {}", "kernel:", info.kernel);
    println!("{:<12} {}", "governor:", info.governor);
    println!(
        "{:<12} {} GB ({} GB available)",
        "memory:",
        info.memory_total_mb / 1024,
        info.memory_available_mb / 1024,
    );
    println!("{:<12} {}", "io_uring:", info.io_uring_status);
}

fn print_drives(info: &EnvInfo) {
    let parts: Vec<String> = info
        .drives
        .iter()
        .map(|(label, dtype)| format!("{label}={dtype}"))
        .collect();
    println!("{:<12} {}", "drives:", parts.join("  "));
}

fn print_tools(info: &EnvInfo) {
    let parts: Vec<String> = info
        .tools
        .iter()
        .map(|(name, ver)| format!("{name} {ver}"))
        .collect();
    println!("{:<12} {}", "tools:", parts.join("  "));
}

fn print_datasets(info: &EnvInfo) {
    let parts: Vec<String> = info
        .datasets
        .iter()
        .map(|(name, status)| format_dataset(name, status))
        .collect();
    println!("{:<12} {}", "datasets:", parts.join("  "));
}

fn format_dataset(name: &str, status: &DatasetStatus) -> String {
    match status {
        DatasetStatus::Present => format!("{name} \u{2713}"),
        DatasetStatus::Missing => format!("{name} \u{2717} (missing)"),
    }
}

// ---------------------------------------------------------------------------
// System readers
// ---------------------------------------------------------------------------

/// Read the kernel version from `/proc/version`.
fn read_kernel() -> String {
    let content = match std::fs::read_to_string("/proc/version") {
        Ok(s) => s,
        Err(_) => return "unknown".to_owned(),
    };

    // Format: "Linux version 6.18.0-9-generic ..."
    // We want the third whitespace-delimited word (the version number).
    extract_kernel_version(&content)
}

fn extract_kernel_version(content: &str) -> String {
    content
        .split_whitespace()
        .nth(2)
        .unwrap_or("unknown")
        .to_owned()
}

/// Read the CPU frequency governor.
fn read_governor() -> String {
    read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
}

/// Read total and available memory from `/proc/meminfo`, returning MB values.
fn read_memory() -> (u64, u64) {
    let content = match std::fs::read_to_string("/proc/meminfo") {
        Ok(s) => s,
        Err(_) => return (0, 0),
    };

    let total = parse_meminfo_field(&content, "MemTotal:");
    let avail = parse_meminfo_field(&content, "MemAvailable:");
    (total, avail)
}

/// Find a line starting with `prefix` in meminfo content and parse the kB
/// value, returning megabytes.
fn parse_meminfo_field(content: &str, prefix: &str) -> u64 {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            return parse_kb_to_mb(rest);
        }
    }
    0
}

/// Parse a meminfo value like "  32637372 kB" into megabytes.
fn parse_kb_to_mb(rest: &str) -> u64 {
    let kb: u64 = rest
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    kb / 1024
}

/// Check io_uring support and memlock limit.
fn read_io_uring_status() -> String {
    let disabled = check_uring_disabled();
    let memlock = read_memlock_limit();

    if disabled {
        return format!("disabled ({memlock})");
    }

    format!("supported ({memlock})")
}

/// Check if io_uring is disabled via the kernel parameter.
fn check_uring_disabled() -> bool {
    match std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled") {
        Ok(content) => content.trim() != "0",
        // File doesn't exist means io_uring is not restricted.
        Err(_) => false,
    }
}

/// Read RLIMIT_MEMLOCK and format it.
fn read_memlock_limit() -> String {
    let mut rlim: libc::rlimit = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut rlim) };

    if ret != 0 {
        return "memlock=unknown".to_owned();
    }

    format_memlock(rlim.rlim_cur)
}

fn format_memlock(cur: u64) -> String {
    if cur == libc::RLIM_INFINITY {
        "memlock=unlimited".to_owned()
    } else {
        format!("memlock={} KB", cur / 1024)
    }
}

// ---------------------------------------------------------------------------
// Drives
// ---------------------------------------------------------------------------

fn collect_drives(paths: &ResolvedPaths) -> Vec<(String, String)> {
    match &paths.drives {
        Some(d) => {
            let mut out = Vec::with_capacity(4);
            push_drive(&mut out, "source", d.source.as_deref());
            push_drive(&mut out, "data", d.data.as_deref());
            push_drive(&mut out, "scratch", d.scratch.as_deref());
            push_drive(&mut out, "target", d.target.as_deref());
            out
        }
        None => vec![("all".to_owned(), "unknown".to_owned())],
    }
}

fn push_drive(out: &mut Vec<(String, String)>, label: &str, value: Option<&str>) {
    let dtype = value.unwrap_or("unknown");
    out.push((label.to_owned(), dtype.to_owned()));
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn collect_tools() -> Vec<(String, String)> {
    vec![
        ("cargo".to_owned(), read_tool_version("cargo", &["--version"])),
        ("osmium".to_owned(), read_tool_version("osmium", &["--version"])),
        ("pbfhogg".to_owned(), read_git_rev()),
    ]
}

/// Run a command and extract the version from its first line of stdout.
fn read_tool_version(name: &str, args: &[&str]) -> String {
    let output = match Command::new(name).args(args).output() {
        Ok(o) => o,
        Err(_) => return "not found".to_owned(),
    };

    if !output.status.success() {
        return "not found".to_owned();
    }

    extract_version_from_stdout(&output.stdout)
}

fn extract_version_from_stdout(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let first_line = text.lines().next().unwrap_or("unknown");
    // Find the first word that starts with a digit (the version number).
    // Handles "cargo 1.95.0-nightly (...)" and "osmium version 1.19.0".
    first_line
        .split_whitespace()
        .find(|w| w.as_bytes().first().is_some_and(u8::is_ascii_digit))
        .unwrap_or("unknown")
        .to_owned()
}

/// Get the current git short rev for pbfhogg.
fn read_git_rev() -> String {
    let output = match Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return "unknown".to_owned(),
    };

    if !output.status.success() {
        return "unknown".to_owned();
    }

    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

// ---------------------------------------------------------------------------
// Datasets
// ---------------------------------------------------------------------------

fn check_datasets(config: &DevConfig, data_dir: &Path) -> Vec<(String, DatasetStatus)> {
    let mut out: Vec<(String, DatasetStatus)> = config
        .datasets
        .iter()
        .map(|(name, ds)| {
            let status = if data_dir.join(&ds.pbf).exists() {
                DatasetStatus::Present
            } else {
                DatasetStatus::Missing
            };
            (name.clone(), status)
        })
        .collect();

    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read a file and return its trimmed contents, or "unknown" on error.
fn read_trimmed(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim().to_owned(),
        Err(_) => "unknown".to_owned(),
    }
}
