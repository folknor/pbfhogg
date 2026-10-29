//! Rayon pools the library builds for one call, joined before the call ends.
//!
//! rayon's `ThreadPool::drop` only asks the pool's workers to stop; they exit
//! on their own schedule, so a pool dropped at the end of a call leaves its
//! threads, and whatever their thread-locals hold, running after the call has
//! returned. [`JoinedPool`] spawns every worker as a plain `std` thread through
//! rayon's `spawn_handler`, keeps each `JoinHandle`, and joins them all when it
//! drops. `JoinHandle::join` waits for the thread's thread-local destructors
//! too. Neither alternative does: an `exit_handler` runs before them, and
//! `std::thread::scope` waits only for a thread's main function (so rayon's
//! `build_scoped` does not wait for them either).
//!
//! Every library-built pool goes through here: `clippy.toml` disallows rayon's
//! pool constructors in the shipped library, and [`JoinedPool::build`] is the
//! one exemption made for them (`output.rs` and `scratch.rs` allow the lint
//! module-wide for their file calls and build no pool).
//!
//! What the join relies on:
//!
//! - rayon completing its termination request: `ThreadPool::drop`, or the
//!   `Terminator` guard in rayon's `Registry::new` when a build fails part
//!   way. A panic inside rayon's own termination is not repaired, and a second
//!   panic during an unwind aborts the process.
//! - Nothing detached outliving the call on the pool. A worker exits only
//!   once the pool holds no detached work (`spawn` and `spawn_broadcast` raise
//!   rayon's termination count), so the library submits none to these pools;
//!   a caller callback that runs on one of their workers is bound by the
//!   contract on [`InstrumentSink`](crate::debug::InstrumentSink) and
//!   [`DiagnosticSink`](crate::host::DiagnosticSink).

use std::thread::JoinHandle;

/// A rayon pool built for one call. Once it has dropped, every one of its
/// worker threads has exited, thread-local destructors included: on success,
/// on an error return, during an unwind, and after a build that failed part
/// way.
///
/// It is lent, never shared: callers borrow [`pool`](Self::pool) (as
/// [`ExecutorRef::Pool`](crate::host::ExecutorRef::Pool) where an executor
/// is needed), and `rayon::ThreadPool` is not `Clone`, so nothing can keep the
/// pool alive past this value. Drop it on the thread that built it, never on
/// one of its own workers.
pub(crate) struct JoinedPool {
    // Field order is the shutdown order: `pool` drops first, which asks the
    // workers to stop, then `workers` joins them.
    pool: rayon::ThreadPool,
    #[expect(dead_code, reason = "held only to be joined when it drops")]
    workers: Workers,
}

impl JoinedPool {
    /// Build a pool of `threads` workers. `what` names it in a build failure
    /// ([`Error::pool_build`](crate::Error)); a failure has already joined
    /// the threads spawned before it.
    pub(crate) fn build(
        what: &str,
        threads: usize,
        invocation: &crate::host::Invocation,
    ) -> crate::Result<Self> {
        #[cfg(not(feature = "test-hooks"))]
        let _ = invocation;
        // Exists before the first spawn, so a build that fails or unwinds
        // part way still joins what it started (rayon has told those threads
        // to stop by then).
        let mut workers = Workers(Vec::with_capacity(threads));
        #[allow(
            clippy::disallowed_methods,
            reason = "the one sanctioned rayon pool construction: its workers are joined"
        )]
        let built = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .spawn_handler(|thread| {
                #[cfg(feature = "test-hooks")]
                invocation.faults().on_built_pool_spawn(thread.index())?;
                #[cfg(feature = "test-hooks")]
                let hooks = invocation.clone();
                let handle = std::thread::Builder::new().spawn(move || {
                    thread.run();
                    #[cfg(feature = "test-hooks")]
                    hooks.faults().on_built_pool_thread_exit();
                })?;
                workers.0.push(handle);
                Ok(())
            })
            .build();
        match built {
            Ok(pool) => Ok(Self { pool, workers }),
            Err(e) => Err(crate::Error::pool_build(what, e)),
        }
    }

    /// The pool, lent for the call.
    pub(crate) fn pool(&self) -> &rayon::ThreadPool {
        &self.pool
    }
}

/// The join guard: joins every handle it holds when it drops.
struct Workers(Vec<JoinHandle<()>>);

impl Drop for Workers {
    fn drop(&mut self) {
        for handle in self.0.drain(..) {
            // A worker that panicked does not stop the join of the rest. Its
            // payload is forgotten, not dropped: the destructor could panic
            // again, here or during an unwind (`Invocation::deliver` does the
            // same).
            if let Err(payload) = handle.join() {
                std::mem::forget(payload);
            }
        }
    }
}

#[cfg(all(test, feature = "test-hooks"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Releases the held pool threads and joins the owner on every exit
    /// path, a failed assertion included.
    struct Release(crate::host::Invocation, Option<std::thread::JoinHandle<()>>);

    impl Drop for Release {
        fn drop(&mut self) {
            self.0
                .faults()
                .release_built_pool_threads
                .store(true, SeqCst);
            if let Some(handle) = self.1.take() {
                drop(handle.join());
            }
        }
    }

    /// An owner that unwinds with its pool still running does not finish
    /// unwinding until every worker has exited.
    #[test]
    fn an_unwinding_owner_waits_for_its_workers() {
        let invocation = crate::host::Invocation::new();
        invocation
            .faults()
            .hold_built_pool_threads
            .store(true, SeqCst);
        let owner = invocation.clone();
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let pool = JoinedPool::build("test pool", 2, &owner).unwrap();
                pool.pool().install(|| ());
                panic!("owner unwinds");
            }));
            let done = owner.faults().built_pool_exit_hooks_done.load(SeqCst);
            done_tx.send((unwound.is_err(), done)).unwrap();
        });
        let _release = Release(invocation.clone(), Some(handle));
        let faults = invocation.faults();

        let start = Instant::now();
        while faults.built_pool_threads_held.load(SeqCst) < 2 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the pool threads did not reach their exit hook"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            matches!(
                done_rx.recv_timeout(Duration::from_millis(200)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "the owner finished unwinding while its workers were still running"
        );
        faults.release_built_pool_threads.store(true, SeqCst);
        let (unwound, done) = done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(unwound);
        assert_eq!(done, 2, "both workers finished before the unwind did");
        assert!(!faults.built_pool_hook_timed_out.load(SeqCst));
    }
}
