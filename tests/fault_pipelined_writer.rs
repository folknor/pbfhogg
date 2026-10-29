//! Fault injection into the pipelined `PbfWriter`'s framing jobs, through the
//! fault plan on the writer's token (`pbfhogg::test_hooks`).
//!
//! The framing work for each block is a detached job on the writer's
//! executor, and the writer thread stops at its first error. Every way the
//! writer then ends - `flush`, `Drop`, or a write that recovers the writer
//! thread's error - must still wait for every framing job it dispatched, or
//! those jobs go on moving the invocation's counters after the writer is
//! gone (`Invocation::for_each_counter` promises them final).
//!
//! The scenario is deterministic: block 0 is malformed, so its framing fails
//! and the writer thread returns that error and drops its receiver; block 1's
//! framing job is parked by `hold_framing_at`, and only a writer that has
//! taken back every other dispatch permit releases it. A writer that walks
//! away from its jobs, or whose wait stops short, returns with the job still
//! parked and block 1 never framed.

#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
use std::time::{Duration, Instant};

use pbfhogg::host::{Executor, Invocation};
use pbfhogg::writer::{Compression, PbfWriter};

/// A minimal but real PrimitiveBlock: one empty PrimitiveGroup (field 2).
const EMPTY_BLOCK: [u8; 2] = [0x12, 0x00];
/// A truncated varint: the framing job's index scan refuses it.
const MALFORMED_BLOCK: [u8; 1] = [0x80];
/// The pool's thread count: one for the parked job, one for the rest.
const POOL_THREADS: usize = 2;

#[derive(Clone, Copy, Debug)]
enum Backend {
    Buffered,
    Parallel,
}

#[derive(Clone, Copy, Debug)]
enum End {
    /// `flush` returns the writer thread's error.
    Flush,
    /// The writer is dropped unflushed.
    Drop,
    /// A raw write finds the writer thread gone and returns its error.
    FailedWrite,
}

fn open(backend: Backend, path: &Path, executor: Executor) -> PbfWriter<impl Write> {
    let header = pbfhogg::block_builder::HeaderBuilder::new()
        .build()
        .unwrap();
    match backend {
        Backend::Buffered => PbfWriter::to_path(path, Compression::None, &header, executor),
        Backend::Parallel => {
            PbfWriter::to_path_parallel(path, Compression::None, &header, executor)
        }
    }
    .unwrap()
}

fn counter(invocation: &Invocation, name: &str) -> u64 {
    let mut found = None;
    invocation.for_each_counter(|n, value| {
        if n == name {
            found = Some(value);
        }
    });
    found.unwrap_or_else(|| panic!("no counter named {name}"))
}

fn wait_for(cond: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

/// What the run looked like the moment the writer's ending operation
/// returned, before anything else touched it.
struct AtReturn {
    held: u64,
    dispatched: u64,
    done: u64,
    framed: u64,
    wait_reached: bool,
}

impl AtReturn {
    fn take(invocation: &Invocation) -> Self {
        let faults = invocation.faults();
        Self {
            held: faults.held_framing_jobs.load(SeqCst),
            dispatched: faults.framing_jobs_dispatched.load(SeqCst),
            done: faults.framing_jobs_done.load(SeqCst),
            framed: counter(invocation, "writer_payload_framed_items"),
            wait_reached: faults.framing_wait_reached.load(SeqCst),
        }
    }
}

/// Releases the parked job and joins the pool on every exit path: the pool
/// is dropped here, and its threads have all exited (so every job on it has
/// ended) before this returns or the bounded wait gives up.
struct Cleanup {
    invocation: Invocation,
    pool: Option<Arc<rayon::ThreadPool>>,
    exited: Arc<AtomicUsize>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.invocation.faults().release_framing.store(true, SeqCst);
        drop(self.pool.take());
        let joined = wait_for(|| self.exited.load(SeqCst) == POOL_THREADS);
        if !std::thread::panicking() {
            assert!(joined, "the writer's pool did not exit 10 s after release");
        }
    }
}

/// End the writer as `end` says: the error it reported, if any, and the
/// state at the moment the ending operation returned.
fn end_writer<W: Write>(
    mut writer: PbfWriter<W>,
    end: End,
    invocation: &Invocation,
) -> (Option<pbfhogg::Error>, AtReturn) {
    match end {
        End::Flush => {
            let err = writer.flush().err();
            let at = AtReturn::take(invocation);
            (err, at)
        }
        End::Drop => {
            drop(writer);
            (None, AtReturn::take(invocation))
        }
        End::FailedWrite => {
            // Raw sends succeed while the writer thread still holds its
            // receiver; the first one after it exits fails and recovers its
            // error.
            let deadline = Instant::now() + Duration::from_secs(10);
            let err = loop {
                match writer.write_raw(b"raw") {
                    Err(e) => break e,
                    Ok(()) => assert!(
                        Instant::now() < deadline,
                        "the writer thread never exited after its framing error"
                    ),
                }
            };
            let at = AtReturn::take(invocation);
            // The writer is still alive: a later framed write must be
            // refused without dispatching anything.
            assert!(
                writer.write_primitive_block(&EMPTY_BLOCK).is_err(),
                "a framed write after the writer thread's error was recovered must fail"
            );
            assert_eq!(
                invocation.faults().framing_jobs_dispatched.load(SeqCst),
                at.dispatched,
                "a refused write must not dispatch a framing job"
            );
            drop(writer.flush());
            (Some(err), at)
        }
    }
}

fn run(backend: Backend, end: End) {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("out.osm.pbf");
    let exited = Arc::new(AtomicUsize::new(0));
    let on_exit = Arc::clone(&exited);
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(POOL_THREADS)
            .exit_handler(move |_| {
                on_exit.fetch_add(1, SeqCst);
            })
            .build()
            .unwrap(),
    );
    let mut writer = open(backend, &path, Executor::Pool(Arc::clone(&pool)));
    let invocation = writer.invocation().clone();
    let cleanup = Cleanup {
        invocation: invocation.clone(),
        pool: Some(pool),
        exited,
    };
    let faults = invocation.faults();
    faults.hold_framing_at.store(1, SeqCst);

    writer.write_primitive_block(&MALFORMED_BLOCK).unwrap();
    writer.write_primitive_block(&EMPTY_BLOCK).unwrap();
    assert!(
        wait_for(|| faults.held_framing_jobs.load(SeqCst) == 1),
        "{backend:?}/{end:?}: block 1's framing job never parked"
    );

    let (err, at) = end_writer(writer, end, &invocation);
    drop(cleanup);
    assert!(
        !faults.framing_hook_timed_out.load(SeqCst),
        "{backend:?}/{end:?}: the framing hook timed out; the check proved nothing"
    );

    match (end, &err) {
        (End::Drop, _) => {}
        (_, Some(err)) => assert!(
            matches!(err.kind(), pbfhogg::ErrorKind::WireFormat { .. }),
            "{backend:?}/{end:?}: the writer thread's own framing error must surface, got: {err}"
        ),
        (_, None) => panic!("{backend:?}/{end:?}: the writer thread's error was not reported"),
    }
    assert_eq!(
        at.held, 0,
        "{backend:?}/{end:?}: the writer returned while a framing job was still parked"
    );
    assert_eq!(
        at.dispatched, 2,
        "{backend:?}/{end:?}: the scenario dispatches exactly two framing jobs"
    );
    assert_eq!(
        at.done, at.dispatched,
        "{backend:?}/{end:?}: the writer returned before every framing job it dispatched ended"
    );
    assert_eq!(
        at.framed, 1,
        "{backend:?}/{end:?}: writer_payload_framed_items moved after the writer returned"
    );
    assert!(
        at.wait_reached,
        "{backend:?}/{end:?}: the parked job was not released by a writer holding every other permit"
    );
}

#[test]
fn fault_injection_framing_jobs_end_before_a_failed_writer_does() {
    for backend in [Backend::Buffered, Backend::Parallel] {
        for end in [End::Flush, End::Drop, End::FailedWrite] {
            run(backend, end);
        }
    }
}
