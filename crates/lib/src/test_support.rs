//! Skip support for the library's unit tests, which cannot reach the
//! integration harness in `crates/lib/tests/common`.
//!
//! A unit test that cannot run on this host announces it through the same
//! `skip` the integration tests use (`crates/lib/tests/common/skip.rs`, mounted by
//! `#[path]` as `path_guard` mounts the shared scratch allocator), and skips
//! only on the typed refusal of the feature it needs, decided by [`refused`]:
//! a `PlatformUnsupported` labelled for another feature, or any other error,
//! fails the test, and so does any refusal of a backend the run requires.
//!
//! It also holds [`Parked`], the observation a unit test uses to know that a
//! thread is blocked inside a primitive's condition-variable wait.

/// A count of threads parked in one primitive's condition-variable wait,
/// kept inside the state that primitive's mutex guards (test builds only).
///
/// The waiter calls [`enter`](Self::enter) under its mutex immediately
/// before `Condvar::wait` and [`leave`](Self::leave) after it returns. A test
/// that then sees the count under the same mutex knows the waiter found its
/// condition false and has released the mutex inside `wait` (it cannot have
/// done anything else while the test holds the lock), so whatever the test
/// does next under that mutex wakes it: nothing is inferred from elapsed
/// time.
#[derive(Default)]
pub(crate) struct Parked {
    waiting: usize,
}

impl Parked {
    pub(crate) fn enter(&mut self, observer: &std::sync::Condvar) {
        self.waiting += 1;
        observer.notify_all();
    }

    pub(crate) fn leave(&mut self) {
        self.waiting -= 1;
    }
}

/// Bound on a unit test's wait for a transition that a correct
/// implementation makes promptly; only a broken one reaches it.
pub(crate) const OBSERVE_BOUND: std::time::Duration = crate::tunables::TEST_OBSERVE_BOUND;

/// Wait, under `mutex`, until `parked(state)` reports at least `n` waiters,
/// failing the test once [`OBSERVE_BOUND`] passes. Returns the guard, so the
/// caller's next step runs while the waiters are provably parked.
pub(crate) fn wait_parked<'a, S>(
    mutex: &'a std::sync::Mutex<S>,
    observer: &std::sync::Condvar,
    parked: impl Fn(&S) -> &Parked,
    n: usize,
) -> std::sync::MutexGuard<'a, S> {
    let guard = mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (guard, timeout) = observer
        // wait-deliberate: waits for the parked count; expiry fails below
        .wait_timeout_while(guard, OBSERVE_BOUND, |s| parked(s).waiting < n)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        !timeout.timed_out(),
        "fewer than {n} threads parked within {OBSERVE_BOUND:?}"
    );
    guard
}

/// Run `parked` on a thread of its own while `observe` watches it from the
/// test thread, all under one `HangBound` armed first, and return both
/// results. The thread is joined on every exit: if `observe` unwinds (a
/// failed assertion), `release` runs first, so a thread parked waiting for
/// the release the test never reached is woken before the join, not left
/// detached. On success `observe` releases the thread itself and `release`
/// does not run. A thread that stays hung after release is the bound's.
pub(crate) fn with_parked_thread<T: Send, R>(
    label: &'static str,
    parked: impl FnOnce() -> T + Send,
    release: impl FnOnce(),
    observe: impl FnOnce() -> R,
) -> (T, R) {
    struct OnUnwind<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for OnUnwind<F> {
        fn drop(&mut self) {
            if std::thread::panicking()
                && let Some(release) = self.0.take()
            {
                release();
            }
        }
    }
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        crate::tunables::TEST_HANG_BOUND,
        crate::tunables::TEST_HANG_REPORT_GRACE,
        label,
    );
    // The library's scope, not std's: it joins the thread whole, its
    // thread-local destructors included, on an unwind too, so the thread
    // has ended before the bound disarms.
    crate::threads::scope(|scope| {
        let handle = scope.spawn(parked);
        let observed = {
            let _release = OnUnwind(Some(release));
            observe()
        };
        let joined = handle
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (joined, observed)
    })
}

#[path = "../tests/common/skip.rs"]
mod shared_skip;

#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
pub(crate) use shared_skip::Backend;
pub(crate) use shared_skip::skip;

/// Whether `err` authorizes dropping a cell that needs `cell`: a typed
/// `PlatformUnsupported` refusal labelled with one of `accepted` (the labels
/// that legitimately stop this cell, which the site chooses). A match fails
/// the test where the run requires `cell`; the caller announces the skip.
/// Every unit-test skip on a platform refusal decides through here, so none
/// can bypass the gate's requirement.
#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
pub(crate) fn refused(cell: Backend, err: &crate::Error, accepted: &[&str]) -> bool {
    let refused = matches!(
        err.kind(),
        crate::ErrorKind::PlatformUnsupported { feature, .. } if accepted.contains(feature)
    );
    if refused {
        shared_skip::fail_if_required(cell, err);
    }
    refused
}

/// Whether `err` is the typed O_DIRECT setup refusal, the one error an
/// O_DIRECT test may skip on (see [`refused`]).
#[cfg(feature = "linux-direct-io")]
pub(crate) fn is_direct_io_refusal(err: &crate::Error) -> bool {
    refused(Backend::DirectIo, err, &["direct-io"])
}

/// Whether files in `dir` open `O_DIRECT`. A typed direct-io refusal is the
/// one `false`, announced as a skip of `portion` (the caller drops that
/// portion or returns); any other failure fails the test.
#[cfg(feature = "linux-direct-io")]
pub(crate) fn direct_reads_supported(dir: &std::path::Path, portion: &str) -> bool {
    let probe = dir.join("direct-probe.bin");
    std::fs::write(&probe, [0_u8; 16]).unwrap_or_else(|e| panic!("write probe: {e}"));
    match crate::direct_io::open_for_read(&probe) {
        Ok(_) => true,
        Err(e) if is_direct_io_refusal(&e) => {
            skip(format!("{portion}: O_DIRECT not supported: {e}"));
            false
        }
        Err(e) => panic!("{e}"),
    }
}

/// Build several data blobs without reading a checkout fixture. The returned
/// offset is inside the final payload, recorded while writing rather than
/// discovered through the reader under test.
#[allow(clippy::unwrap_used)]
pub(crate) fn multi_blob_pbf(path: &std::path::Path) -> usize {
    use crate::block_builder::{BlockBuilder, HeaderBuilder};
    use crate::writer::{Compression, PbfWriter};
    let mut bytes = Vec::new();
    let mut final_frame = 0;
    {
        let mut writer = PbfWriter::new(&mut bytes, Compression::None);
        writer
            .write_header(&HeaderBuilder::new().build().unwrap())
            .unwrap();
        writer.flush().unwrap();
    }
    for id in 1..=3 {
        final_frame = bytes.len();
        let mut block = BlockBuilder::new();
        block.add_node(id, 100, 200, std::iter::empty::<(&str, &str)>(), None);
        let payload = block.take().unwrap().unwrap();
        let mut writer = PbfWriter::new(&mut bytes, Compression::None);
        writer.write_primitive_block(payload).unwrap();
        writer.flush().unwrap();
    }
    let header_len =
        u32::from_be_bytes(bytes[final_frame..final_frame + 4].try_into().unwrap()) as usize;
    let cut = final_frame + 4 + header_len + 3;
    assert!(cut < bytes.len());
    std::fs::write(path, bytes).unwrap();
    cut
}
