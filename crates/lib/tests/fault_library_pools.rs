//! The rayon pool the library builds for a call (the pipelined reader's
//! decode pool) is joined before
//! the call ends: on success, after a build that failed part way, and during
//! an unwind. Each test parks every pool thread after its rayon main loop
//! returns (`FaultPlan::hold_built_pool_threads`), waits until exactly the
//! expected threads have parked and the call has reached the pool's join,
//! then releases them and checks the call ended only after every one
//! finished its exit hook.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::mpsc;
use std::thread::JoinHandle;

use pbfhogg::ElementReader;
use pbfhogg::host::Invocation;

use crate::common::write_many_block_pbf;

/// Releases the parked pool threads and joins the calling thread on every
/// exit path, a failed assertion included.
struct Release {
    invocation: Invocation,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Release {
    fn drop(&mut self) {
        self.invocation.faults().release_built_pool_threads();
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            eprintln!("library-pool driver thread panicked");
        }
    }
}

/// Runs `call` on its own thread under `invocation`, whose pool threads are
/// held, and returns its result once `expected` pool threads have parked,
/// the call has reached its pool's join, and they have been released.
fn run_held<T: Send + 'static>(
    invocation: &Invocation,
    expected: u64,
    call: impl FnOnce() -> T + Send + 'static,
) -> T {
    // Declared first, so it also bounds the release guard's join.
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        pbfhogg::TEST_HANG_BOUND,
        pbfhogg::TEST_HANG_REPORT_GRACE,
        "the held library pool call did not end",
    );
    let faults = invocation.faults();
    faults.hold_built_pool_threads.store(true, SeqCst);
    let observer = invocation.clone();
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let _release = Release {
        invocation: invocation.clone(),
        handle: Some(std::thread::spawn(move || {
            let result = call();
            let hooks_done = observer.faults().built_pool_exit_hooks_done.load(SeqCst);
            done_tx.send((result, hooks_done)).unwrap();
        })),
    };

    // Every thread is parked and cannot exit, and the call has reached its
    // pool's join: from here the call ends only after the release below.
    assert!(
        faults.wait_for(|| faults.built_pool_threads_held.load(SeqCst) >= expected
            && faults.built_pool_joins_started.load(SeqCst) >= 1),
        "only {} of {expected} pool threads reached their exit hook, \
         and {} joins started",
        faults.built_pool_threads_held.load(SeqCst),
        faults.built_pool_joins_started.load(SeqCst)
    );
    faults.release_built_pool_threads();
    let (result, hooks_done) = done_rx.recv().unwrap();
    assert_eq!(
        faults.built_pool_threads_held.load(SeqCst),
        expected,
        "pool threads parked"
    );
    assert_eq!(
        hooks_done, expected,
        "every pool thread finished before the call ended"
    );
    assert!(!faults.built_pool_hook_timed_out.load(SeqCst));
    result
}

fn many_block_input() -> (pbfhogg_test_scratch::TestScratch, std::path::PathBuf) {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("many.osm.pbf");
    write_many_block_pbf(&path, 8);
    (dir, path)
}

fn read_all(path: &Path, invocation: Invocation, threads: usize) -> pbfhogg::Result<()> {
    ElementReader::from_path(path)?
        .invocation(invocation)
        .decode_threads(threads)
        .for_each_block_pipelined(|_| Ok(()))
}

#[test]
fn decode_pool_is_joined_before_the_read_returns() {
    let (_dir, path) = many_block_input();
    let invocation = Invocation::new();
    let reader_invocation = invocation.clone();
    run_held(&invocation, 2, move || {
        read_all(&path, reader_invocation, 2)
    })
    .unwrap();
}

#[test]
fn decode_pool_partial_build_failure_joins_the_spawned_threads() {
    let (_dir, path) = many_block_input();
    let invocation = Invocation::new();
    invocation
        .faults()
        .built_pool_spawn_fail_at
        .store(2, SeqCst);
    let reader_invocation = invocation.clone();
    let err = run_held(&invocation, 2, move || {
        read_all(&path, reader_invocation, 4)
    })
    .unwrap_err();
    assert!(
        err.to_string().contains("failed to build decode pool"),
        "{err}"
    );
}

#[test]
fn decode_pool_is_joined_before_a_panicking_consumer_unwinds() {
    let (_dir, path) = many_block_input();
    let invocation = Invocation::new();
    let reader_invocation = invocation.clone();
    let unwound = run_held(&invocation, 2, move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ElementReader::from_path(&path)
                .unwrap()
                .invocation(reader_invocation)
                .decode_threads(2)
                .for_each_block_pipelined(|_| panic!("consumer panics"))
        }))
        .is_err()
    });
    assert!(unwound);
}
