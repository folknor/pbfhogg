//! Explicit caller-invoked process policy.

/// Ask glibc to return free chunks above the trim threshold to the OS
/// (`malloc_trim(0)`). Returns 1 if memory was released, 0 otherwise.
///
/// Use to fight the documented cross-thread `PrimitiveBlock` retention
/// pattern (`src/read/pipeline.rs:66-89`): decoded blocks are
/// allocated on rayon decode threads and dropped on the consumer
/// thread; glibc retains those pages on the decode-thread arena's
/// free list rather than returning them to the OS, which manifests
/// as steadily-climbing anon RSS at scale. Calling `malloc_trim(0)`
/// at a batch boundary forces a sweep across all arenas.
///
/// Cost: a few ms per call at planet scale (one pass over the
/// free-list). Cheap enough to fire per snapshot batch (~140
/// batches at planet, ~150 ms total).
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
/// Use to fight the cross-thread `PrimitiveBlock` retention pattern
/// when mallinfo2 shows growth concentrated in `arena` (brk) with
/// `hblkhd` (mmap) flat. Default glibc threshold is 128 KB and is
/// also dynamic (climbs as the program runs, up to 32 MB). This
/// pins it to a hard floor.
///
/// Call once early in the run; the setting is process-global. On
/// non-glibc / non-Linux this is a no-op returning 0.
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
/// returned to the OS. At planet scale that is the cross-thread
/// `PrimitiveBlock` retention pattern (`src/read/pipeline.rs`), and it
/// shows as anon RSS climbing into the tens of GB. Two arenas force
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

/// Result of probing + raising `RLIMIT_NOFILE`. `Display` prints a
/// compact one-line summary suitable for stderr narration.
#[derive(Clone, Copy, Debug)]
pub struct FdAllowance {
    /// Effective soft limit after the raise attempt, clamped to a sane
    /// ceiling if the kernel reports `RLIM_INFINITY`.
    pub descriptors: u64,
    pub adjustment: Option<FdAdjustment>,
}

impl std::fmt::Display for FdAllowance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.adjustment {
            None => write!(f, "RLIMIT_NOFILE: {} (not adjusted)", self.descriptors),
            Some(FdAdjustment::ProbeFailed) => write!(
                f,
                "RLIMIT_NOFILE: probe failed; assuming {}",
                self.descriptors
            ),
            Some(FdAdjustment::AlreadyAtCap) => write!(
                f,
                "RLIMIT_NOFILE: {} (already at hard cap; no adjustment)",
                self.descriptors
            ),
            Some(FdAdjustment::Raised { from }) => write!(
                f,
                "RLIMIT_NOFILE: {from} -> {} (soft raised to hard cap)",
                self.descriptors
            ),
            Some(FdAdjustment::RaiseFailed) => write!(
                f,
                "RLIMIT_NOFILE: {} (raise to hard cap rejected; still at soft cap)",
                self.descriptors
            ),
        }
    }
}

/// Self-raise `RLIMIT_NOFILE` soft limit to the current hard cap and
/// report both the effective soft limit and the narrative of what
/// happened.
///
/// External join's stage 1 pass B holds `num_workers * NUM_BUCKETS`
/// (up to ~4096 on a 17-core host) rank-shard files open concurrently.
/// Linux default soft ulimit is 1024 on many distros; some still cap
/// hard at 4096. Without this raise, `add-locations-to-ways
/// --index-type external` fails with EMFILE on a default-ulimit shell
/// even though the hard cap is usually high enough. The unprivileged
/// soft-raise up to hard cap is the standard pattern for servers
/// (PostgreSQL, Redis, nginx).
///
/// Returns 1024 as a conservative fallback `descriptors` if the
/// probe itself fails, so callers can still compute a sensible cap.
pub fn raise_nofile_to_hard_cap() -> FdAllowance {
    // SAFETY: `getrlimit`/`setrlimit` are pure-data FFI calls. The
    // rlimit struct is fully initialised before being passed as
    // `&mut`, and we don't share it across threads.
    unsafe {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            return FdAllowance {
                descriptors: 1024,
                adjustment: Some(FdAdjustment::ProbeFailed),
            };
        }
        let soft_before: u64 = rlim.rlim_cur;
        let hard: u64 = rlim.rlim_max;
        if soft_before >= hard {
            // `rlim_cur` can be `RLIM_INFINITY` (= `u64::MAX` on
            // Linux); clamp so downstream arithmetic doesn't overflow.
            return FdAllowance {
                descriptors: soft_before.min(1 << 30),
                adjustment: Some(FdAdjustment::AlreadyAtCap),
            };
        }
        let target = libc::rlimit {
            rlim_cur: hard,
            rlim_max: hard,
        };
        let raise_ok = libc::setrlimit(libc::RLIMIT_NOFILE, &target) == 0;
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            return FdAllowance {
                descriptors: 1024,
                adjustment: Some(FdAdjustment::ProbeFailed),
            };
        }
        let effective: u64 = rlim.rlim_cur.min(1 << 30);
        if raise_ok {
            FdAllowance {
                descriptors: effective,
                adjustment: Some(FdAdjustment::Raised { from: soft_before }),
            }
        } else {
            FdAllowance {
                descriptors: effective,
                adjustment: Some(FdAdjustment::RaiseFailed),
            }
        }
    }
}
/// Probe the current descriptor allowance without adjusting it.
pub fn current_nofile() -> FdAllowance {
    // SAFETY: the output structure is initialized and exclusively borrowed.
    unsafe {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return FdAllowance {
                descriptors: 1024,
                adjustment: Some(FdAdjustment::ProbeFailed),
            };
        }
        FdAllowance {
            descriptors: limit.rlim_cur.min(1 << 30),
            adjustment: None,
        }
    }
}
