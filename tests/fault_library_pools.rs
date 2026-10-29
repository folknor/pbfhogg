//! The rayon pools the library builds for a call (the pipelined reader's
//! decode pool, merge-changes' `Parallelism::Fixed` pool) are joined before
//! the call ends: on success, after a build that failed part way, and during
//! an unwind. Each test parks every pool thread after its rayon main loop
//! returns (`FaultPlan::hold_built_pool_threads`), waits until exactly the
//! expected threads have parked, checks the call has not ended while they
//! are parked, then releases them and checks the call ended only after every
//! one finished its exit hook.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
        self.invocation
            .faults()
            .release_built_pool_threads
            .store(true, SeqCst);
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            eprintln!("library-pool driver thread panicked");
        }
    }
}

/// Runs `call` on its own thread under `invocation`, whose pool threads are
/// held, and returns its result once `expected` pool threads have parked,
/// the call has been seen not to end while they are parked, and they have
/// been released.
fn run_held<T: Send + 'static>(
    invocation: &Invocation,
    expected: u64,
    call: impl FnOnce() -> T + Send + 'static,
) -> T {
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

    let start = Instant::now();
    while faults.built_pool_threads_held.load(SeqCst) < expected {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "only {} of {expected} pool threads reached their exit hook",
            faults.built_pool_threads_held.load(SeqCst)
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    // Positive sync above: every thread is parked and cannot exit. A call
    // that does not join them returns within this window.
    assert!(
        matches!(
            done_rx.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "the call ended while its pool threads were still running"
    );
    faults.release_built_pool_threads.store(true, SeqCst);
    let (result, hooks_done) = done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
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

fn many_block_input() -> (tempfile::TempDir, std::path::PathBuf) {
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

fn merge_inputs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = crate::common::test_scratch();
    let first = dir.path().join("first.osc");
    let second = dir.path().join("second.osc");
    for (path, id) in [(&first, 1000), (&second, 1001)] {
        std::fs::write(
            path,
            format!(
                "<osmChange version=\"0.6\"><create>\
                 <node id=\"{id}\" version=\"1\" lat=\"0.001\" lon=\"0.002\"/>\
                 </create></osmChange>"
            ),
        )
        .unwrap();
    }
    (dir, first, second)
}

#[test]
fn merge_changes_fixed_pool_is_joined_before_it_returns() {
    let (dir, first, second) = merge_inputs();
    let host = crate::common::test_host();
    let invocation = host.invocation.clone();
    let out = dir.path().join("merged.osc.gz");
    let stats = run_held(&invocation, 2, move || {
        pbfhogg::merge_changes::merge_changes(
            &[&first, &second],
            crate::common::out(out),
            false,
            pbfhogg::Parallelism::from_jobs(2),
            false,
            &host,
        )
    })
    .unwrap();
    assert_eq!(stats.changes_out, 2);
}

#[test]
fn merge_changes_partial_build_failure_joins_the_spawned_threads() {
    let (dir, first, second) = merge_inputs();
    let host = crate::common::test_host();
    let invocation = host.invocation.clone();
    invocation
        .faults()
        .built_pool_spawn_fail_at
        .store(1, SeqCst);
    let out = dir.path().join("merged.osc.gz");
    let err = run_held(&invocation, 1, move || {
        pbfhogg::merge_changes::merge_changes(
            &[&first, &second],
            crate::common::out(out),
            false,
            pbfhogg::Parallelism::from_jobs(3),
            false,
            &host,
        )
    })
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("failed to build merge-changes worker pool"),
        "{err}"
    );
}
