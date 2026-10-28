//! Explicit caller-invoked process policy, and the procfs readers the library,
//! the CLI and the sidecar instrumentation use. The metric readers return
//! `Option`: a failed or unavailable sample is reported as absent, never as
//! zero, and each caller decides what absence means for its output. The one
//! precondition probe here, [`is_mount_point`], returns an I/O error instead,
//! because its caller refuses work on the answer and must not mistake an
//! unreadable mount table for "not a mount point".

use crate::tunables::{NOFILE_PROBE_FAILED_ASSUMED_SOFT, NOFILE_SOFT_LIMIT_CLAMP};

/// Read a file the kernel provides (procfs, cgroupfs). Not a caller-named
/// input, so it bypasses `crate::read::input`: the path is the kernel's, and
/// each caller decides what a failure means - most fold it into an absent
/// sample, [`is_mount_point`] returns it with the path as context.
#[allow(clippy::disallowed_methods)]
pub(crate) fn read_kernel_file(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// [`read_kernel_file`] as raw bytes, for a table whose fields need not be
/// UTF-8 (mountinfo paths).
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)]
fn read_kernel_bytes(path: &str) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// Read cumulative minor and major page faults from `/proc/self/stat`.
///
/// Returns `None` when procfs is unavailable or the process-stat record is
/// missing either counter. Non-Linux targets have no procfs implementation.
#[cfg(target_os = "linux")]
pub(crate) fn read_page_faults() -> Option<(u64, u64)> {
    let stat = read_kernel_file("/proc/self/stat").ok()?;
    parse_page_faults(&stat)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_page_faults() -> Option<(u64, u64)> {
    None
}

#[cfg(target_os = "linux")]
fn parse_page_faults(stat: &str) -> Option<(u64, u64)> {
    // `comm` (field 2) can contain spaces; field 10 and field 12 are
    // indexes 7 and 9 after the closing parenthesis.
    let after_comm = stat.rsplit_once(')')?.1;
    let mut fields = after_comm.split_whitespace();
    let minflt = fields.nth(7)?.parse::<u64>().ok()?;
    // Skip field 11 (cminflt) to get field 12 (majflt).
    let majflt = fields.nth(1)?.parse::<u64>().ok()?;
    Some((minflt, majflt))
}

/// Microseconds elapsed since this process started, using `/proc/self/stat`
/// and `/proc/uptime` to include dynamic-loader and pre-`main` time.
///
/// Returns `None` when procfs data, the clock tick rate, or a valid elapsed
/// time is unavailable. Non-Linux targets return `None`.
#[cfg(target_os = "linux")]
pub(crate) fn micros_since_process_start() -> Option<u64> {
    let stat = read_kernel_file("/proc/self/stat").ok()?;
    // `comm` (field 2) is parenthesised and may contain spaces; fields after
    // the closing paren are space-separated, so field 22 is index 19 there.
    let after_comm = stat.rsplit_once(')')?.1;
    let starttime_ticks: u64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;
    // SAFETY: sysconf is a libc function safe to call from any thread.
    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_sec <= 0 {
        return None;
    }
    #[allow(clippy::cast_sign_loss)]
    let ticks_per_sec = ticks_per_sec as u64;
    let uptime = read_kernel_file("/proc/uptime").ok()?;
    let uptime_secs: f64 = uptime.split_whitespace().next()?.parse().ok()?;
    let start_secs = starttime_ticks as f64 / ticks_per_sec as f64;
    let alive = uptime_secs - start_secs;
    if !alive.is_finite() || alive < 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((alive * 1e6) as u64)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn micros_since_process_start() -> Option<u64> {
    None
}

/// Read peak resident set size (`VmHWM`) from `/proc/self/status` in KiB.
///
/// Returns `None` when procfs is unavailable, the field is absent or the
/// value is malformed. Non-Linux targets return `None`.
#[cfg(target_os = "linux")]
pub fn read_peak_rss_kb() -> Option<u64> {
    let status = read_kernel_file("/proc/self/status").ok()?;
    parse_peak_rss_kb(&status)
}

#[cfg(not(target_os = "linux"))]
pub fn read_peak_rss_kb() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn parse_peak_rss_kb(status: &str) -> Option<u64> {
    let rest = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?;
    let trimmed = rest.trim();
    let kb_str = trimmed.strip_suffix("kB").unwrap_or(trimmed).trim();
    kb_str.parse::<u64>().ok()
}

/// Whether `path` is exactly a mount point in this process's mount namespace,
/// read from `/proc/self/mountinfo`.
///
/// `path` is canonicalized first, so a symlink is judged by its target; a
/// caller that cares about the link itself must check for it separately.
/// Errors from canonicalization or from reading the mount table are returned,
/// not folded into `false`. Non-Linux targets have no portable mount table
/// and return `Ok(false)`; there an operation that cannot cross a mount (a
/// rename) remains the authority.
#[cfg(target_os = "linux")]
#[cfg_attr(not(feature = "commands"), allow(dead_code))]
pub(crate) fn is_mount_point(path: &std::path::Path) -> crate::Result<bool> {
    use std::os::unix::ffi::OsStrExt;

    let canonical = std::fs::canonicalize(path)?;
    let mountinfo = read_kernel_bytes("/proc/self/mountinfo")
        .map_err(|e| crate::Error::from(e).context("cannot read /proc/self/mountinfo"))?;
    for line in mountinfo.split(|byte| *byte == b'\n') {
        // Fields: mount ID, parent ID, major:minor, root, mount point, ...
        // The kernel octal-escapes whitespace inside a path, so splitting
        // on whitespace cannot cut one.
        let mut fields = line
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty());
        let Some(mountpoint) = fields.nth(4) else {
            continue;
        };
        let decoded = decode_mountinfo_path(mountpoint);
        if std::path::Path::new(std::ffi::OsStr::from_bytes(&decoded)) == canonical.as_path() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(not(target_os = "linux"))]
#[cfg_attr(not(feature = "commands"), allow(dead_code))]
pub(crate) fn is_mount_point(_path: &std::path::Path) -> crate::Result<bool> {
    Ok(false)
}

/// Undo the kernel's `\ooo` octal escaping of a mountinfo path field.
#[cfg(target_os = "linux")]
pub(crate) fn decode_mountinfo_path(path: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(path.len());
    let mut offset = 0;
    while offset < path.len() {
        if path[offset] == b'\\'
            && offset + 3 < path.len()
            && path[offset + 1..offset + 4]
                .iter()
                .all(|byte| matches!(*byte, b'0'..=b'7'))
        {
            let high = path[offset + 1] - b'0';
            let middle = path[offset + 2] - b'0';
            let low = path[offset + 3] - b'0';
            decoded.push((high << 6) | (middle << 3) | low);
            offset += 4;
        } else {
            decoded.push(path[offset]);
            offset += 1;
        }
    }
    decoded
}

#[cfg(all(test, target_os = "linux"))]
mod proc_metric_tests {
    #[test]
    fn page_fault_parser_handles_spaces_in_comm() {
        let stat = "123 (program name with spaces) R 1 2 3 4 5 6 101 102 103 104";
        assert_eq!(super::parse_page_faults(stat), Some((101, 103)));
    }

    #[test]
    fn mountinfo_paths_decode_octal_escapes_only() {
        assert_eq!(super::decode_mountinfo_path(br"/mnt/a\040b"), b"/mnt/a b");
        assert_eq!(super::decode_mountinfo_path(br"/x\134y"), br"/x\y");
        assert_eq!(super::decode_mountinfo_path(br"/x\09"), br"/x\09");
        assert_eq!(super::decode_mountinfo_path(br"/x\04"), br"/x\04");
    }
}

/// Ask glibc to return free chunks above the trim threshold to the OS
/// (`malloc_trim(0)`). Returns 1 if memory was released, 0 otherwise.
///
/// Use at a phase boundary when glibc holds eligible freed heap chunks as
/// anonymous RSS and returning them to the OS is useful. A former pipelined
/// PBF reader allocated separate string-table entry arrays on decode workers
/// and freed them on the consumer; glibc retained those pages and this reached
/// 25+ GB anon at scale. The current reader stores entries inline in its pooled
/// decompression buffer, removing that specific cross-thread retention path.
/// Calling `malloc_trim(0)` sweeps all arenas for free chunks above the trim
/// threshold.
///
/// Cost: a few ms per call at planet scale (one pass over the
/// free-list). Cheap enough to fire per snapshot batch (~140
/// batches at planet, ~150 ms total).
///
/// This remains public opt-in API even though the repository has no callers;
/// downstream applications may use the process-wide operation.
///
/// On non-glibc / non-Linux this is a no-op returning 0.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn malloc_trim() -> i32 {
    // SAFETY: malloc_trim is a glibc function safe to call from any thread.
    unsafe { libc::malloc_trim(0) }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn malloc_trim() -> i32 {
    0
}

/// Lower glibc's `M_MMAP_THRESHOLD` so allocations >= `bytes` route
/// through `mmap`/`munmap` instead of brk. mmap-backed chunks are
/// released back to the OS the moment they're freed, while brk-arena
/// chunks accumulate as fragmentation when allocations interleave
/// across threads.
///
/// Use against cross-thread allocate-here, free-there retention (see
/// [`malloc_trim`] for the reader-side instance that has since been
/// designed out) when mallinfo2 shows growth concentrated in `arena` (brk)
/// with `hblkhd` (mmap) flat. Default glibc threshold is 128 KB and is
/// also dynamic (climbs as the program runs, up to 32 MB). This
/// pins it to a hard floor.
///
/// Call once early in the run; the setting is process-global. On
/// non-glibc / non-Linux this is a no-op returning 0.
///
/// This remains public opt-in API even though the repository has no callers;
/// downstream applications may use the process-wide operation.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn set_mmap_threshold(bytes: i32) -> i32 {
    // SAFETY: mallopt is glibc and safe to call from any thread
    // before allocations grow past the desired threshold.
    unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, bytes) }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn set_mmap_threshold(_bytes: i32) -> i32 {
    0
}

/// Cap glibc's malloc arena count at 2 (`mallopt(M_ARENA_MAX, 2)`).
///
/// glibc gives each thread its own arena on contention, up to 8 x cores.
/// When buffers are allocated on one thread and freed on another (decode
/// or pread workers allocate, the merge / compression / consumer thread
/// frees), the freed chunks land back on the allocating thread's arena
/// and accumulate there as fragmentation instead of being reused or
/// returned to the OS, which shows as anon RSS climbing with file size.
/// The decode pipeline's per-block string-table allocations were the
/// worst instance (tens of GB at planet) and are gone (see
/// [`malloc_trim`]); other buffers still cross threads. Two arenas force
/// the worker pool to share free lists, which keeps the peak bounded.
///
/// The cost is lock contention on the shared arenas, which hurts
/// workers that keep allocation-heavy scratch alive across blobs on the
/// same thread. Call sites document why their worker shape is safe.
///
/// The setting is process-global and overrides an arena_max glibc tunable.
/// Once glibc has latched the limit at 2 it cannot be raised again. If glibc
/// already latched its default limit, this call can be silently ineffective.
/// Existing arenas are never destroyed: a cap below the current arena count
/// only prevents creating new arenas. The caller owns this policy decision.
/// Call it once, early, before the worker pool spins up. On non-glibc /
/// non-Linux this is a no-op.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn cap_malloc_arenas() {
    // SAFETY: mallopt is glibc and safe to call from any thread; the
    // return value only reports an invalid parameter, which 2 is not.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn cap_malloc_arenas() {}

/// Outcome of the `RLIMIT_NOFILE` self-raise attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdAdjustment {
    /// `getrlimit` failed; caller is using the conservative fallback.
    ProbeFailed,
    /// Soft limit was already at the hard cap; nothing to raise.
    AlreadyAtCap,
    /// Soft raised from `from` up to the hard cap.
    Raised { from: u64 },
    /// Soft limit is below the hard cap but `setrlimit` rejected the
    /// raise. Rare: unprivileged soft-raise to hard cap should always
    /// succeed on Linux, but seccomp or LSM policies can block it.
    RaiseFailed,
}

/// How many descriptors were already open when an allowance was probed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdCount {
    /// Open descriptors numbered below the soft limit. Includes the one
    /// the count itself held open, so it is conservative by one.
    Counted(u64),
    /// The open descriptors cannot be counted here (no procfs, a non-Linux
    /// target); nothing was subtracted.
    Uncounted,
    /// Enumerating the open descriptors started and then failed; carries
    /// the errno. Nothing can be granted on a partial count.
    Failed(i32),
    /// Counting needed a descriptor and none was free; carries the errno
    /// (`EMFILE` for the process limit, `ENFILE` for the system-wide table).
    Exhausted(i32),
}

/// What a built-in provider derived an [`FdAllowance`] from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdProbe {
    /// Soft `RLIMIT_NOFILE` after any raise attempt, clamped to a sane
    /// ceiling if the kernel reports `RLIM_INFINITY`. An assumption, not an
    /// observation, when `adjustment` is `Some(FdAdjustment::ProbeFailed)`.
    pub soft_limit: u64,
    /// `None` when the provider did not try to adjust the limit.
    pub adjustment: Option<FdAdjustment>,
    pub open: FdCount,
    /// Hard `RLIMIT_NOFILE`, clamped as `soft_limit` is: how far the
    /// process could raise its soft limit without privilege. `None` when
    /// the probe could not read it.
    pub hard_limit: Option<u64>,
}

/// Descriptors a run may hold, as answered by
/// [`ProcessActions::fd_allowance`](crate::host::ProcessActions::fd_allowance).
/// `Display` prints a compact one-line summary suitable for narration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdAllowance {
    /// Descriptors the run may open on top of those already open.
    pub descriptors: u64,
    /// The limit and open count this was derived from; `None` for a
    /// host-defined grant.
    pub probe: Option<FdProbe>,
}

impl FdAllowance {
    /// A host-defined grant of `descriptors`, for example a share the host
    /// reserves for the duration of one call.
    pub fn host_grant(descriptors: u64) -> Self {
        Self {
            descriptors,
            probe: None,
        }
    }

    /// The allowance a probe yields: the soft limit less the descriptors
    /// already open, the whole soft limit where they cannot be counted, and
    /// nothing when counting found none free or failed partway.
    pub fn from_probe(probe: FdProbe) -> Self {
        let descriptors = match probe.open {
            FdCount::Counted(open) => probe.soft_limit.saturating_sub(open),
            FdCount::Uncounted => probe.soft_limit,
            FdCount::Exhausted(_) | FdCount::Failed(_) => 0,
        };
        Self {
            descriptors,
            probe: Some(probe),
        }
    }
}

impl std::fmt::Display for FdAllowance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Some(probe) = self.probe else {
            return write!(f, "descriptor allowance: {} (host grant)", self.descriptors);
        };
        let soft = probe.soft_limit;
        match probe.adjustment {
            None => match probe.hard_limit {
                Some(hard) if hard > soft => {
                    write!(f, "RLIMIT_NOFILE: {soft} (not adjusted; hard limit {hard})")?;
                }
                _ => write!(f, "RLIMIT_NOFILE: {soft} (not adjusted)")?,
            },
            Some(FdAdjustment::ProbeFailed) => {
                write!(f, "RLIMIT_NOFILE: probe failed; assuming {soft}")?;
            }
            Some(FdAdjustment::AlreadyAtCap) => write!(
                f,
                "RLIMIT_NOFILE: {soft} (already at hard cap; no adjustment)"
            )?,
            Some(FdAdjustment::Raised { from }) => write!(
                f,
                "RLIMIT_NOFILE: {from} -> {soft} (soft raised to hard cap)"
            )?,
            Some(FdAdjustment::RaiseFailed) => write!(
                f,
                "RLIMIT_NOFILE: {soft} (raise to hard cap rejected; still at soft cap)"
            )?,
        }
        match probe.open {
            FdCount::Counted(open) => {
                write!(f, "; {open} open, {} available", self.descriptors)
            }
            FdCount::Uncounted => f.write_str("; open descriptors not counted"),
            FdCount::Exhausted(errno) => write!(
                f,
                "; no descriptor free to count open ones ({})",
                std::io::Error::from_raw_os_error(errno)
            ),
            FdCount::Failed(errno) => write!(
                f,
                "; counting open descriptors failed ({})",
                std::io::Error::from_raw_os_error(errno)
            ),
        }
    }
}

/// Count the process's open descriptors numbered below `limit`.
///
/// Only numbers below the soft limit compete for new descriptors: after a
/// limit is lowered, descriptors already open above it do not take a slot
/// below it.
pub fn count_open_descriptors(limit: u64) -> FdCount {
    #[cfg(target_os = "linux")]
    {
        let dir = match std::fs::read_dir("/proc/self/fd") {
            Ok(dir) => dir,
            Err(e) => {
                return match e.raw_os_error() {
                    Some(errno) if errno == libc::EMFILE || errno == libc::ENFILE => {
                        FdCount::Exhausted(errno)
                    }
                    _ => FdCount::Uncounted,
                };
            }
        };
        let mut open: u64 = 0;
        for entry in dir {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => return FdCount::Failed(e.raw_os_error().unwrap_or(libc::EIO)),
            };
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u64>().ok())
            else {
                return FdCount::Failed(libc::EINVAL);
            };
            if fd < limit {
                open += 1;
            }
        }
        FdCount::Counted(open)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = limit;
        FdCount::Uncounted
    }
}

/// Probe with the open descriptors counted against `soft_limit`.
fn probed(
    soft_limit: u64,
    hard_limit: Option<u64>,
    adjustment: Option<FdAdjustment>,
) -> FdAllowance {
    FdAllowance::from_probe(FdProbe {
        soft_limit,
        adjustment,
        open: count_open_descriptors(soft_limit),
        hard_limit,
    })
}

/// Self-raise `RLIMIT_NOFILE` soft limit to the current hard cap and
/// report the descriptors a run may open on top of those already open,
/// with the narrative of what happened.
///
/// External join's stage 1 holds `num_workers * NUM_BUCKETS` id-shard
/// files open concurrently, which on a many-core host is several
/// thousand. Linux default soft ulimit is 1024 on many distros; some
/// still cap hard at 4096. Without this raise, `add-locations-to-ways
/// --index-type external` on a default-ulimit shell runs with fewer shard
/// workers than the host has cores, or refuses to start, even though the
/// hard cap is usually high enough. The unprivileged
/// soft-raise up to hard cap is the standard pattern for servers
/// (PostgreSQL, Redis, nginx).
///
/// The open count is a snapshot, not a reservation: it is right while no
/// other thread of the process opens descriptors concurrently.
///
/// Assumes a soft limit of 1024 if the probe itself fails, so callers can
/// still compute a sensible cap.
pub fn raise_nofile_to_hard_cap() -> FdAllowance {
    // SAFETY: `getrlimit`/`setrlimit` are pure-data FFI calls. The
    // rlimit struct is fully initialised before being passed as
    // `&mut`, and we don't share it across threads.
    let (soft, hard, adjustment) = unsafe {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            (
                NOFILE_PROBE_FAILED_ASSUMED_SOFT,
                None,
                FdAdjustment::ProbeFailed,
            )
        } else {
            let soft_before: u64 = rlim.rlim_cur;
            let hard: u64 = rlim.rlim_max;
            let hard_seen = Some(hard.min(NOFILE_SOFT_LIMIT_CLAMP));
            if soft_before >= hard {
                // `rlim_cur` can be `RLIM_INFINITY` (= `u64::MAX` on
                // Linux); clamp so downstream arithmetic doesn't overflow.
                (
                    soft_before.min(NOFILE_SOFT_LIMIT_CLAMP),
                    hard_seen,
                    FdAdjustment::AlreadyAtCap,
                )
            } else {
                let target = libc::rlimit {
                    rlim_cur: hard,
                    rlim_max: hard,
                };
                let raise_ok = libc::setrlimit(libc::RLIMIT_NOFILE, &target) == 0;
                if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
                    // A successful setrlimit established the target even if
                    // the follow-up observation failed. Do not replace that
                    // known limit with the initial-probe fallback.
                    if raise_ok {
                        (
                            hard.min(NOFILE_SOFT_LIMIT_CLAMP),
                            hard_seen,
                            FdAdjustment::Raised { from: soft_before },
                        )
                    } else {
                        (
                            soft_before.min(NOFILE_SOFT_LIMIT_CLAMP),
                            hard_seen,
                            FdAdjustment::RaiseFailed,
                        )
                    }
                } else if raise_ok {
                    (
                        rlim.rlim_cur.min(NOFILE_SOFT_LIMIT_CLAMP),
                        Some(rlim.rlim_max.min(NOFILE_SOFT_LIMIT_CLAMP)),
                        FdAdjustment::Raised { from: soft_before },
                    )
                } else {
                    (
                        rlim.rlim_cur.min(NOFILE_SOFT_LIMIT_CLAMP),
                        Some(rlim.rlim_max.min(NOFILE_SOFT_LIMIT_CLAMP)),
                        FdAdjustment::RaiseFailed,
                    )
                }
            }
        }
    };
    probed(soft, hard, Some(adjustment))
}
/// Probe the current descriptor allowance without adjusting the limit:
/// the soft limit less the descriptors already open. Same snapshot caveat
/// as [`raise_nofile_to_hard_cap`].
pub fn current_nofile() -> FdAllowance {
    // SAFETY: the output structure is initialized and exclusively borrowed.
    let limits = unsafe {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            None
        } else {
            Some((
                limit.rlim_cur.min(NOFILE_SOFT_LIMIT_CLAMP),
                limit.rlim_max.min(NOFILE_SOFT_LIMIT_CLAMP),
            ))
        }
    };
    match limits {
        Some((soft, hard)) => probed(soft, Some(hard), None),
        None => probed(
            NOFILE_PROBE_FAILED_ASSUMED_SOFT,
            None,
            Some(FdAdjustment::ProbeFailed),
        ),
    }
}
