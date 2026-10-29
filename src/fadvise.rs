//! The crate's one `posix_fadvise`: page-cache advice on a descriptor the
//! library opened itself.
//!
//! Advice is a hint. The kernel may ignore it, and a failure (`ESPIPE` on a
//! FIFO, `EINVAL` on a file with no page cache behind it) changes how fast a
//! run is, never what it produces, so no caller handles one. Failures are
//! counted instead: [`advise`] keeps the invocation's total and emits it as
//! the `fadvise_failures` sidecar counter on every failure, so a run whose
//! hints did not take effect can be told apart from one whose hints did.
//!
//! Why the library may advise at all: ADR-0016 (`decisions/`) gives process
//! state and shared-host policy to the caller, and input page-cache eviction
//! with it (`host::PageCachePolicy`). Every other piece of advice here touches
//! only the library's own open file descriptions - their readahead window,
//! their access pattern, or pages of the library's own scratch files - and
//! changes nothing another descriptor or process sees beyond what the read
//! itself would. Input eviction still goes through this helper; whether it
//! happens at all is the caller's policy, decided before it gets here.
//!
//! The behaviour is the same in every Linux build. It used to compile only
//! under the `linux-direct-io` or `linux-io-uring` features, a leftover from
//! when `libc` was optional; those features now mean only `O_DIRECT` and
//! `io_uring`. Off Linux [`advise`] does nothing.
//!
//! `clippy.toml` bans `libc::posix_fadvise` everywhere else in the library.

use std::fs::File;

/// The advice [`advise`] can give.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Advice {
    /// `POSIX_FADV_SEQUENTIAL`: a front-to-back read; Linux doubles the
    /// readahead window for the open file description.
    Sequential,
    /// `POSIX_FADV_RANDOM`: no readahead for this open file description.
    Random,
    /// `POSIX_FADV_WILLNEED`: start reading the range into the page cache.
    WillNeed,
    /// `POSIX_FADV_DONTNEED`: drop the range's clean cached pages. Dirty pages
    /// stay until written back, so a caller that wants its own writes dropped
    /// syncs first.
    DontNeed,
}

/// Advise the kernel about `len` bytes of `file` from `offset`, where `len`
/// zero means through the end of the file (as in `posix_fadvise`).
///
/// Returns whether the advice was given, that is whether the syscall was
/// made, whatever it returned: `false` off Linux, and when the range cannot
/// be expressed in `off_t` (an `offset` past its maximum, or a nonzero `len`
/// that clamps to nothing there). A nonzero `len` reaching past the largest
/// offset is clamped to it rather than passed as zero, which would widen a
/// finite request to the whole tail of the file. Neither `true` nor a
/// successful return means any page was read or dropped.
#[cfg(target_os = "linux")]
pub(crate) fn advise(
    file: &File,
    offset: u64,
    len: u64,
    advice: Advice,
    invocation: &crate::host::Invocation,
) -> bool {
    use std::os::unix::io::AsRawFd as _;

    let max = off_t_max();
    let Some(room) = max.checked_sub(offset) else {
        return false;
    };
    let len = if len == 0 {
        0
    } else {
        match len.min(room) {
            0 => return false,
            clamped => clamped,
        }
    };
    let (Ok(offset), Ok(len)) = (libc::off_t::try_from(offset), libc::off_t::try_from(len)) else {
        return false;
    };
    let flag = match advice {
        Advice::Sequential => libc::POSIX_FADV_SEQUENTIAL,
        Advice::Random => libc::POSIX_FADV_RANDOM,
        Advice::WillNeed => libc::POSIX_FADV_WILLNEED,
        Advice::DontNeed => libc::POSIX_FADV_DONTNEED,
    };
    #[cfg(test)]
    tests::record_attempt(advice);
    // The crate's one `posix_fadvise`.
    #[allow(clippy::disallowed_methods)]
    // Safety: the borrow keeps the descriptor open for the call, and the
    // call only reads its arguments.
    let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), offset, len, flag) };
    // `posix_fadvise` returns the error number itself, not -1 and `errno`.
    if rc != 0 {
        // Cold: a nested entry under whatever admits the caller (a
        // command's call, a reader method's, a pipeline thread's permit).
        let call = invocation.call();
        call.metrics().fadvise.record_failure(&call);
    }
    true
}

/// The largest offset `off_t` holds, which is narrower than `u64` everywhere
/// and narrower than `i64` on some 32-bit targets.
#[cfg(target_os = "linux")]
fn off_t_max() -> u64 {
    u64::try_from(libc::off_t::MAX).unwrap_or(u64::MAX)
}

/// Off Linux there is no advice to give.
#[cfg(not(target_os = "linux"))]
pub(crate) fn advise(
    _file: &File,
    _offset: u64,
    _len: u64,
    _advice: Advice,
    _invocation: &crate::host::Invocation,
) -> bool {
    false
}

crate::counters::counter_set! {
    /// Failed advice of one invocation.
    pub(crate) struct FadviseMetrics {
        /// `posix_fadvise` calls that returned an error.
        failures => "fadvise_failures",
    }
    extra {
        /// Serializes a failure's increment with its emission (see
        /// [`FadviseMetrics::record_failure`]).
        emit_order: std::sync::Mutex<()> = std::sync::Mutex::new(()),
    }
}

impl FadviseMetrics {
    /// Count one failed advice and emit the invocation's total. Cold: a hint
    /// that failed once on a descriptor usually fails on every call, but a
    /// run that sees it at all is the exception.
    ///
    /// The increment and the emission happen under one lock, so the totals
    /// reach the FIFO in increasing order; an atomic increment emitted after
    /// the fact could emit a smaller total after a larger one. The total
    /// itself is an atomic, so reading it never takes the lock. Best effort,
    /// like every sidecar counter: a sample the FIFO drops is not repaired.
    #[cfg(target_os = "linux")]
    #[cold]
    fn record_failure(&self, invocation: &crate::host::Recorder) {
        use std::sync::atomic::Ordering::Relaxed;
        let _order = self
            .emit_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let total = self.failures.fetch_add(1, Relaxed).saturating_add(1);
        invocation.counter("fadvise_failures", total);
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::{Advice, advise, off_t_max};
    use std::cell::RefCell;

    thread_local! {
        /// Advice attempted on this thread, in order. Thread-local so tests
        /// running concurrently cannot see each other's calls.
        static ATTEMPTS: RefCell<Vec<Advice>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record_attempt(advice: Advice) {
        ATTEMPTS.with(|a| a.borrow_mut().push(advice));
    }

    /// How many times `advice` has been attempted on this thread so far.
    pub(crate) fn attempts(advice: Advice) -> usize {
        ATTEMPTS.with(|a| a.borrow().iter().filter(|&&x| x == advice).count())
    }

    #[test]
    fn ranges_off_t_cannot_express_are_not_advised() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        let file = crate::read::input::open(&path).unwrap();
        let max = off_t_max();
        let before = attempts(Advice::WillNeed);
        let inv = crate::host::Invocation::new();
        assert!(!advise(&file, max + 1, 0, Advice::WillNeed, &inv));
        // A finite request at the last offset has no room left: advising it
        // would mean "through EOF".
        assert!(!advise(&file, max, 1, Advice::WillNeed, &inv));
        assert_eq!(attempts(Advice::WillNeed), before, "nothing was attempted");
        assert!(advise(&file, 0, 0, Advice::WillNeed, &inv));
        assert!(advise(&file, 1, u64::MAX, Advice::WillNeed, &inv));
        assert_eq!(attempts(Advice::WillNeed), before + 2);
    }
}
