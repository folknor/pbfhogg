//! The crate's one `posix_fadvise`: page-cache advice on a descriptor the
//! library opened itself.
//!
//! Advice is a hint. The kernel may ignore it, and a failure (`ESPIPE` on a
//! FIFO, `EINVAL` on a file with no page cache behind it) changes how fast a
//! run is, never what it produces, so no caller handles one. Failures are
//! counted instead: every entry here keeps the invocation's total and emits
//! it as the `fadvise_failures` sidecar counter on every failure, so a run
//! whose hints did not take effect can be told apart from one whose hints
//! did.
//!
//! Why the library may advise at all: ADR-0016 (`decisions/`) gives process
//! state and shared-host policy to the caller, and input page-cache eviction
//! with it (`host::PageCachePolicy`). Every other piece of advice here touches
//! only the library's own open file descriptions - their readahead window,
//! their access pattern, or pages of the library's own scratch files - and
//! changes nothing another descriptor or process sees beyond what the read
//! itself would.
//!
//! # Three entries, each eviction counted by its kind
//!
//! The `host.page-cache-keep.v1` library capability is proven by the
//! invocation counter `fadvise_input_evictions` staying 0 under
//! `PageCachePolicy::Keep`. That proof holds only if no input page can be
//! dropped without the count, so the split is in the types, not in a
//! convention:
//!
//! - [`advise`] takes a [`Hint`], which has no variant that drops pages,
//!   and [`will_need`] only ever asks for pages to be read.
//! - [`evict_input`] is the only way to drop pages of an arbitrary `File`,
//!   and it always counts. It fails closed: it holds a call on the invocation
//!   across the syscall, and issues nothing when the invocation refuses one
//!   (sealed), so no eviction can land after the count was snapshotted.
//! - [`evict_scratch`] takes only a
//!   [`ScratchFile`](crate::scratch::ScratchFile), which `crate::scratch`
//!   alone mints, from a path inside the run's own scratch directory. It
//!   never counts as an input eviction; it counts apart, as
//!   `fadvise_scratch_evictions`, so a run that stopped evicting its scratch
//!   can be told from one that evicts it. That count proves no absence, so it
//!   needs no fail-closed hold: the caller's capability already keeps the
//!   invocation from sealing across the call.
//!
//! Whether an input is evicted at all is the caller's policy, decided before
//! it gets here; `brokkr.toml` text rules confine the entry points that arm
//! input eviction, and [`evict_input`]'s callers, to the read path that
//! implements the policy.
//!
//! The behaviour is the same in every Linux build. It used to compile only
//! under the `linux-direct-io` or `linux-io-uring` features, a leftover from
//! when `libc` was optional; those features now mean only `O_DIRECT` and
//! `io_uring`. Off Linux nothing here gives advice.
//!
//! `clippy.toml` bans `libc::posix_fadvise` everywhere else in the library,
//! and `libc::madvise` / `libc::posix_madvise` everywhere, so page reclaim
//! advice on a mapping cannot route around the count either.

use std::fs::File;

/// Advice [`advise`] can give: none of it drops a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hint {
    /// `POSIX_FADV_SEQUENTIAL`: a front-to-back read; Linux doubles the
    /// readahead window for the open file description.
    Sequential,
    /// `POSIX_FADV_RANDOM`: no readahead for this open file description.
    Random,
}

/// Every advice the syscall below is given, private so that the two
/// `POSIX_FADV_DONTNEED` variants are reachable only through their entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Advice {
    Hint(Hint),
    /// `POSIX_FADV_WILLNEED`, issued only by [`will_need`] in calls no
    /// longer than [`WILLNEED_CALL_BYTES`](crate::tunables::WILLNEED_CALL_BYTES).
    WillNeed,
    /// `POSIX_FADV_DONTNEED` on an input, counted ([`evict_input`]).
    EvictInput,
    /// `POSIX_FADV_DONTNEED` on library scratch, counted apart from input
    /// ([`evict_scratch`]).
    EvictScratch,
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
/// successful return means any page was read.
#[cfg(target_os = "linux")]
pub(crate) fn advise(
    file: &File,
    offset: u64,
    len: u64,
    hint: Hint,
    invocation: &crate::host::Invocation,
) -> bool {
    let Some(range) = off_t_range(offset, len) else {
        return false;
    };
    let rc = issue(file, range, Advice::Hint(hint));
    if rc != 0 {
        // Cold: a nested entry under whatever admits the caller (a
        // command's call, a reader method's, a pipeline thread's permit), so
        // never refused there; a sealed invocation records nothing.
        if let Ok(call) = invocation.call() {
            call.metrics().fadvise.record_failure(&call);
        }
    }
    true
}

/// Ask the kernel to start reading `len` (nonzero) bytes of `file` from
/// `offset` into the page cache, clipped to the file's current size.
///
/// Linux honours one `POSIX_FADV_WILLNEED` call only up to the larger of the
/// device's optimal I/O size and the description's readahead window, and
/// drops the rest of a longer range silently with a successful return. So the
/// range is issued as consecutive calls, split at absolute multiples of
/// [`WILLNEED_CALL_BYTES`](crate::tunables::WILLNEED_CALL_BYTES): the kernel
/// rounds each call outward to whole pages, and a call inside one aligned
/// block of that size rounds to at most the block (every page size up to it
/// divides it), so no call reaches past the cap a chunk boundary assumes.
/// `FADV_RANDOM` on the same description does not stop this advice; it only
/// turns off the kernel's own readahead on demand reads.
///
/// Returns the bytes passed to advice calls, a contiguous prefix from
/// `offset` (every call is made, whatever an earlier one returned): 0 when
/// nothing was issued (off Linux, `len` zero, `offset` at or past the file's
/// size, a size the descriptor cannot report, or a range `off_t` cannot
/// express). Neither a nonzero return nor a successful call means any page
/// was read. Each call's elapsed time is summed into the invocation's
/// `fadvise_willneed_wait_ns`, and each failed call counts into
/// `fadvise_failures`.
#[cfg(target_os = "linux")]
pub(crate) fn will_need(
    file: &File,
    offset: u64,
    len: u64,
    invocation: &crate::host::Invocation,
) -> u64 {
    use crate::tunables::WILLNEED_CALL_BYTES;
    if len == 0 {
        return 0;
    }
    let Ok(meta) = file.metadata() else {
        return 0;
    };
    let Some(room) = meta.len().checked_sub(offset) else {
        return 0;
    };
    let Some(off_t_room) = off_t_max().checked_sub(offset) else {
        return 0;
    };
    let total = len.min(room).min(off_t_room);
    let mut issued = 0_u64;
    let mut wait_ns = 0_u64;
    let mut failures = 0_u32;
    while issued < total {
        let at = offset + issued;
        let chunk = (total - issued).min(WILLNEED_CALL_BYTES - at % WILLNEED_CALL_BYTES);
        // In range by the clip above; a conversion failure ends the advice.
        let (Ok(at_t), Ok(chunk_t)) = (libc::off_t::try_from(at), libc::off_t::try_from(chunk))
        else {
            break;
        };
        let timed = crate::counters::Stopwatch::start();
        let rc = issue(file, (at_t, chunk_t), Advice::WillNeed);
        wait_ns = wait_ns.saturating_add(timed.elapsed_ns());
        if rc != 0 {
            failures += 1;
        }
        issued += chunk;
    }
    // A sealed invocation records nothing.
    if issued > 0
        && let Ok(call) = invocation.call()
    {
        let fadvise = &call.metrics().fadvise;
        fadvise
            .willneed_wait_ns
            .fetch_add(wait_ns, std::sync::atomic::Ordering::Relaxed);
        for _ in 0..failures {
            fadvise.record_failure(&call);
        }
    }
    issued
}

/// Drop `len` bytes of input `file` from `offset` from the page cache (`len`
/// zero through the end of the file), counted into `invocation`'s
/// `fadvise_input_evictions`. The one way the library drops pages of a file
/// it did not create as scratch.
///
/// Counted as issued, before the syscall and whatever it returns: a failed
/// eviction under `Keep` must not read as none. A range `off_t` cannot
/// express is refused as in [`advise`] and counts nothing, since nothing is
/// issued. Fails closed: the count and the syscall run under one call on
/// `invocation`, held until the syscall has returned, so the invocation
/// cannot seal between them; when it refuses the call (it has sealed),
/// nothing is issued and this returns `false`. Input eviction normally runs
/// under the reader's admission (a call or a pipeline permit), so the nested
/// call is not refused there.
#[cfg(target_os = "linux")]
pub(crate) fn evict_input(
    file: &File,
    offset: u64,
    len: u64,
    invocation: &crate::host::Invocation,
) -> bool {
    let Some(range) = off_t_range(offset, len) else {
        return false;
    };
    let Ok(call) = invocation.call() else {
        return false;
    };
    call.metrics()
        .fadvise
        .input_evictions
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if issue(file, range, Advice::EvictInput) != 0 {
        call.metrics().fadvise.record_failure(&call);
    }
    drop(call);
    true
}

/// Drop a scratch file's clean pages from the page cache, once its bytes
/// have been read into memory (or, for a file this run wrote, synced: dirty
/// pages stay until written back). Scratch is the library's own, so this is
/// library scratch management, not the caller's page-cache policy
/// (ADR-0016), and it is not counted as an input eviction; the
/// [`ScratchFile`](crate::scratch::ScratchFile) argument is what keeps an
/// input out of it. Counted into `recorder`'s `fadvise_scratch_evictions` as
/// issued, before the syscall and whatever it returns, like an input
/// eviction; a failure also counts into its `fadvise_failures`. Returns
/// whether the advice was given (`false` off Linux, where nothing counts).
#[cfg(target_os = "linux")]
pub(crate) fn evict_scratch(
    file: &crate::scratch::ScratchFile,
    recorder: &crate::host::Recorder,
) -> bool {
    recorder
        .metrics()
        .fadvise
        .scratch_evictions
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The whole file: offset 0, length 0, always expressible.
    if issue(file.as_file(), (0, 0), Advice::EvictScratch) != 0 {
        recorder.metrics().fadvise.record_failure(recorder);
    }
    true
}

/// `offset` and `len` as `off_t`, clamped as [`advise`] describes, or `None`
/// when the range cannot be expressed.
#[cfg(target_os = "linux")]
fn off_t_range(offset: u64, len: u64) -> Option<(libc::off_t, libc::off_t)> {
    let room = off_t_max().checked_sub(offset)?;
    let len = if len == 0 {
        0
    } else {
        match len.min(room) {
            0 => return None,
            clamped => clamped,
        }
    };
    Some((
        libc::off_t::try_from(offset).ok()?,
        libc::off_t::try_from(len).ok()?,
    ))
}

/// The syscall, returning its result (the error number itself, not -1 and
/// `errno`).
#[cfg(target_os = "linux")]
fn issue(file: &File, (offset, len): (libc::off_t, libc::off_t), advice: Advice) -> libc::c_int {
    use std::os::unix::io::AsRawFd as _;

    let flag = match advice {
        Advice::Hint(Hint::Sequential) => libc::POSIX_FADV_SEQUENTIAL,
        Advice::Hint(Hint::Random) => libc::POSIX_FADV_RANDOM,
        Advice::WillNeed => libc::POSIX_FADV_WILLNEED,
        Advice::EvictInput | Advice::EvictScratch => libc::POSIX_FADV_DONTNEED,
    };
    #[cfg(test)]
    tests::record_attempt(advice, (offset, len));
    // The crate's one `posix_fadvise`.
    #[allow(clippy::disallowed_methods)]
    // Safety: the borrow keeps the descriptor open for the call, and the
    // call only reads its arguments.
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), offset, len, flag)
    }
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
    _hint: Hint,
    _invocation: &crate::host::Invocation,
) -> bool {
    false
}

/// Off Linux there is no advice to give.
#[cfg(not(target_os = "linux"))]
pub(crate) fn will_need(
    _file: &File,
    _offset: u64,
    _len: u64,
    _invocation: &crate::host::Invocation,
) -> u64 {
    0
}

/// Off Linux there is no advice to give.
#[cfg(not(target_os = "linux"))]
pub(crate) fn evict_input(
    _file: &File,
    _offset: u64,
    _len: u64,
    _invocation: &crate::host::Invocation,
) -> bool {
    false
}

/// Off Linux there is no advice to give.
#[cfg(not(target_os = "linux"))]
pub(crate) fn evict_scratch(
    _file: &crate::scratch::ScratchFile,
    _recorder: &crate::host::Recorder,
) -> bool {
    false
}

crate::counters::counter_set! {
    /// Failed advice, and evictions by kind, of one invocation.
    pub(crate) struct FadviseMetrics {
        /// `posix_fadvise` calls that returned an error.
        failures => "fadvise_failures",
        /// `POSIX_FADV_DONTNEED` advice issued on an input file
        /// ([`evict_input`]), whatever the call returned.
        input_evictions => "fadvise_input_evictions",
        /// `POSIX_FADV_DONTNEED` advice issued on a library scratch file
        /// ([`evict_scratch`]), whatever the call returned. Every advice
        /// issued, on a failed or cancelled run too: it is what the run
        /// asked for, not what a command expected to ask for.
        scratch_evictions => "fadvise_scratch_evictions",
        /// Nanoseconds spent inside `POSIX_FADV_WILLNEED` calls
        /// ([`will_need`]), summed over every thread that issued
        /// one. Syscall elapsed time on the advising thread, which includes
        /// any reclaim or scheduling delay the kernel spent there; it is not
        /// the read-ahead I/O itself, which the call only queues, and not a
        /// measure of what the advice cost or saved elsewhere.
        willneed_wait_ns => "fadvise_willneed_wait_ns",
    }
    extra {
        /// Serializes a failure's increment with its emission (see
        /// [`FadviseMetrics::record_failure`]).
        emit_order: std::sync::Mutex<()> = std::sync::Mutex::new(()),
    }
}

impl FadviseMetrics {
    /// Emit the invocation's `fadvise_willneed_wait_ns` total to its sidecar
    /// stream. Called once per advising pass, not per call: the advice runs
    /// once per consumed blob, and a line per call would flood the FIFO.
    pub(crate) fn emit_willneed(&self, invocation: &crate::host::Recorder) {
        invocation.counter(
            "fadvise_willneed_wait_ns",
            self.willneed_wait_ns
                .load(std::sync::atomic::Ordering::Relaxed),
        );
    }

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
    use super::{Advice, Hint, advise, evict_input, evict_scratch, off_t_max, will_need};
    use crate::tunables::WILLNEED_CALL_BYTES;
    use std::cell::RefCell;

    type Range = (libc::off_t, libc::off_t);

    thread_local! {
        /// Advice attempted on this thread, in order, with its range.
        /// Thread-local so tests running concurrently cannot see each other's
        /// calls.
        static ATTEMPTS: RefCell<Vec<(Advice, Range)>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record_attempt(advice: Advice, range: Range) {
        ATTEMPTS.with(|a| a.borrow_mut().push((advice, range)));
    }

    fn attempts(advice: Advice) -> usize {
        ATTEMPTS.with(|a| a.borrow().iter().filter(|x| x.0 == advice).count())
    }

    /// How many times `hint` has been attempted on this thread so far.
    pub(crate) fn hint_attempts(hint: Hint) -> usize {
        attempts(Advice::Hint(hint))
    }

    fn attempt_count() -> usize {
        ATTEMPTS.with(|a| a.borrow().len())
    }

    /// The `WILLNEED` ranges attempted on this thread after the first `from`
    /// attempts of any kind, as `(offset, len)`.
    fn willneed_since(from: usize) -> Vec<(u64, u64)> {
        ATTEMPTS.with(|a| {
            a.borrow()[from..]
                .iter()
                .filter(|x| x.0 == Advice::WillNeed)
                .map(|&(_, (at, len))| (u64::try_from(at).unwrap(), u64::try_from(len).unwrap()))
                .collect()
        })
    }

    /// Every `WILLNEED` call stays inside one aligned block of
    /// `WILLNEED_CALL_BYTES` (so its page-rounded extent does too), the calls
    /// tile the requested range clipped to the file without gap or overlap,
    /// and the return is the bytes they covered.
    #[test]
    fn will_need_splits_at_aligned_blocks_and_clips_to_the_file() {
        let c = WILLNEED_CALL_BYTES;
        let size = 5 * c + 123;
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("f");
        std::fs::write(&path, vec![0_u8; usize::try_from(size).unwrap()]).unwrap();
        let file = crate::read::input::open(&path).unwrap();
        let inv = crate::host::Invocation::new();

        let check = |offset: u64, len: u64, want: u64| {
            let from = attempt_count();
            assert_eq!(will_need(&file, offset, len, &inv), want, "{offset}+{len}");
            let calls = willneed_since(from);
            let mut at = offset;
            for &(o, l) in &calls {
                assert_eq!(o, at, "contiguous");
                assert!(l > 0);
                assert_eq!(o / c, (o + l - 1) / c, "one aligned block: {o}+{l}");
                at += l;
            }
            assert_eq!(at - offset, want);
            calls.len()
        };

        // An unaligned range exactly one block long straddles two blocks.
        assert_eq!(check(1, c, c), 2);
        // A short range crossing one boundary.
        assert_eq!(check(c - 10, 20, 20), 2);
        // An aligned whole block is one call.
        assert_eq!(check(c, c, c), 1);
        // Clipped to the file's size; an unbounded length is the tail.
        assert_eq!(check(4 * c, 10 * c, c + 123), 2);
        assert_eq!(check(7, u64::MAX, size - 7), 6);
        // Nothing to advise: zero length, at or past the end.
        assert_eq!(check(0, 0, 0), 0);
        assert_eq!(check(size, 1, 0), 0);
        assert_eq!(check(off_t_max() + 1, 1, 0), 0);
    }

    fn counter(inv: &crate::host::Invocation, wanted: &str) -> u64 {
        let mut n = None;
        inv.for_each_counter(|name, value| {
            if name == wanted {
                n = Some(value);
            }
        });
        n.unwrap()
    }

    fn input_evictions(inv: &crate::host::Invocation) -> u64 {
        counter(inv, "fadvise_input_evictions")
    }

    #[test]
    fn ranges_off_t_cannot_express_are_not_advised() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        let file = crate::read::input::open(&path).unwrap();
        let max = off_t_max();
        let before = hint_attempts(Hint::Sequential);
        let inv = crate::host::Invocation::new();
        assert!(!advise(&file, max + 1, 0, Hint::Sequential, &inv));
        // A finite request at the last offset has no room left: advising it
        // would mean "through EOF".
        assert!(!advise(&file, max, 1, Hint::Sequential, &inv));
        assert_eq!(
            hint_attempts(Hint::Sequential),
            before,
            "nothing was attempted"
        );
        assert!(advise(&file, 0, 0, Hint::Sequential, &inv));
        assert!(advise(&file, 1, u64::MAX, Hint::Sequential, &inv));
        assert_eq!(hint_attempts(Hint::Sequential), before + 2);
    }

    /// Every input eviction issued is counted, an unexpressible range is
    /// neither issued nor counted, and a sealed invocation gets no eviction
    /// at all rather than an uncounted one.
    #[test]
    fn input_evictions_are_counted_or_not_issued() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        let file = crate::read::input::open(&path).unwrap();
        let inv = crate::host::Invocation::new();
        let before = attempts(Advice::EvictInput);
        assert!(evict_input(&file, 0, 1, &inv));
        assert!(!evict_input(&file, off_t_max() + 1, 0, &inv));
        assert_eq!(attempts(Advice::EvictInput), before + 1);
        assert_eq!(input_evictions(&inv), 1);

        assert!(matches!(inv.seal(), crate::host::SealOutcome::Sealed(_)));
        assert!(!evict_input(&file, 0, 1, &inv), "sealed: refused");
        assert_eq!(attempts(Advice::EvictInput), before + 1, "nothing issued");
    }

    /// A scratch eviction issues exactly one `DONTNEED` of the scratch kind
    /// and counts exactly one `fadvise_scratch_evictions`, never an input
    /// eviction. The count and the syscall are pinned to each other here, on
    /// the calling thread; the altw external integration test pins which
    /// scratch a run evicts.
    #[test]
    fn scratch_evictions_are_issued_and_counted_apart_from_input() {
        let parent = crate::path_guard::tests::scratch();
        let run =
            crate::scratch::RunScratch::create(parent.path(), &crate::host::Host::for_tests())
                .unwrap();
        let file = crate::scratch::create(&run.file("bucket").unwrap()).unwrap();
        std::io::Write::write_all(&mut file.as_file(), b"x").unwrap();
        let inv = crate::host::Invocation::new();
        let scratch_before = attempts(Advice::EvictScratch);
        let input_before = attempts(Advice::EvictInput);
        let call = inv.call().unwrap();
        assert!(evict_scratch(&file, &call));
        drop(call);
        assert_eq!(attempts(Advice::EvictScratch), scratch_before + 1);
        assert_eq!(attempts(Advice::EvictInput), input_before);
        assert_eq!(counter(&inv, "fadvise_scratch_evictions"), 1);
        assert_eq!(input_evictions(&inv), 0);
    }
}
