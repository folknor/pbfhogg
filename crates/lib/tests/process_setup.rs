//! The integration binary's one process step: its soft `RLIMIT_NOFILE` is
//! raised to the hard limit before `main`, so every test starts under the
//! same limit and no fixture changes it. Test hosts never read the limit
//! (their descriptor grants are fixed, `common::FixedFdGrant`); the raise
//! only gives the tests' real descriptors, which nothing in this shared
//! process bounds, the room the hard limit allows. Child processes inherit
//! the raised limit.
//!
//! The step runs from `.init_array`, so it uses raw libc calls, plain
//! atomics and nothing that can panic. `the_descriptor_limit_was_raised_at_start`
//! reports what it found.

use std::sync::atomic::{AtomicI32, AtomicU64, Ordering::Relaxed};

/// Not run yet.
const UNRUN: u64 = u64::MAX;

static SOFT_BEFORE: AtomicU64 = AtomicU64::new(UNRUN);
static SOFT_AFTER: AtomicU64 = AtomicU64::new(UNRUN);
static HARD: AtomicU64 = AtomicU64::new(UNRUN);
/// errno of the failed call, 0 when none failed.
static ERRNO: AtomicI32 = AtomicI32::new(0);

extern "C" fn raise_nofile_at_start() {
    let errno = || std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: an initialised, exclusive output structure.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        ERRNO.store(errno(), Relaxed);
        return;
    }
    SOFT_BEFORE.store(limit.rlim_cur, Relaxed);
    HARD.store(limit.rlim_max, Relaxed);
    if limit.rlim_cur < limit.rlim_max {
        let raised = libc::rlimit {
            rlim_cur: limit.rlim_max,
            rlim_max: limit.rlim_max,
        };
        // SAFETY: a plain-data argument; before main no other thread runs.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const raised) } != 0 {
            ERRNO.store(errno(), Relaxed);
            SOFT_AFTER.store(limit.rlim_cur, Relaxed);
            return;
        }
    }
    // SAFETY: as above.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        ERRNO.store(errno(), Relaxed);
        return;
    }
    SOFT_AFTER.store(limit.rlim_cur, Relaxed);
}

#[used]
#[unsafe(link_section = ".init_array")]
static RAISE_NOFILE_AT_START: extern "C" fn() = raise_nofile_at_start;

/// The step ran before any test and left the soft limit at the hard limit.
#[test]
fn the_descriptor_limit_was_raised_at_start() {
    let (before, after, hard, errno) = (
        SOFT_BEFORE.load(Relaxed),
        SOFT_AFTER.load(Relaxed),
        HARD.load(Relaxed),
        ERRNO.load(Relaxed),
    );
    assert_ne!(
        before, UNRUN,
        "the start-up step did not run (errno {errno})"
    );
    assert_eq!(
        (after, errno),
        (hard, 0),
        "soft RLIMIT_NOFILE {before} was not raised to the hard limit {hard}"
    );
}
