//! Scoped `std` worker threads, every one joined before its scope returns.
//!
//! `std::thread::scope` waits for each thread's main function only: a thread
//! still running its thread-local destructors counts as finished, so a call
//! built on it can return while its threads still hold whatever their
//! thread-locals hold. `JoinHandle::join` waits for the whole thread,
//! destructors included. [`scope`] is `std::thread::scope` with every thread
//! registered at spawn and explicitly joined, in spawn order, before it
//! returns: on success, error and unwind alike. Every library worker set that
//! runs inside a call goes through here; `clippy.toml` disallows the raw
//! `std` spawns outside this module, [`JoinedPool`](crate::joined_pool) (the
//! library's rayon pools, joined the same way) and the session threads that
//! keep and join their own `JoinHandle`.
//!
//! Only the thread that opened a scope can spawn into it, and a [`Handle`]
//! cannot leave that thread or the scope's body (neither is `Send`), so once
//! the body has returned nothing can add a thread or still hold a handle:
//! the join pass sees every thread. A thread that wants workers of its own
//! opens its own scope. Guards that must act before the join (a ledger stop,
//! a channel drop) are dropped by the body, as with `std`'s scope.
//!
//! Panics: [`Handle::join`] returns a thread's panic to its caller. A thread
//! nobody joined that panicked has its payload re-raised once every thread
//! has been joined (the first such payload; the others are forgotten, as a
//! payload's destructor could panic again). If the body itself panicked, its
//! panic is the one that continues and every child payload is forgotten.
//! Neither a join nor the disposal of a result runs under the registry
//! borrow.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::ScopedJoinHandle;

/// A scope's spawning side, lent to its body. Not `Send` and not `Sync`.
pub(crate) struct Scope<'w, 'scope, 'env: 'scope> {
    inner: &'scope std::thread::Scope<'scope, 'env>,
    /// Every thread spawned, in spawn order; `None` once joined.
    registry: RefCell<Vec<Option<ScopedJoinHandle<'scope, ()>>>>,
    _owner: PhantomData<(&'w (), *const ())>,
}

/// A thread spawned into a [`Scope`]. Dropping it unjoined leaves the thread
/// to the scope's join pass; its result is dropped on its own thread.
pub(crate) struct Handle<'w, 'scope, 'env: 'scope, T> {
    scope: &'w Scope<'w, 'scope, 'env>,
    index: usize,
    slot: Arc<Mutex<Option<T>>>,
    thread: std::thread::Thread,
}

impl<'w, 'scope, 'env> Scope<'w, 'scope, 'env> {
    /// Spawn an unnamed thread. Panics if the OS refuses one, as
    /// `std::thread::Scope::spawn` does.
    pub(crate) fn spawn<T, F>(&'w self, f: F) -> Handle<'w, 'scope, 'env, T>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        match self.spawn_with(std::thread::Builder::new(), f) {
            Ok(handle) => handle,
            Err(e) => panic!("failed to spawn thread: {e}"),
        }
    }

    /// Spawn an unnamed thread, returning the OS refusal.
    pub(crate) fn try_spawn<T, F>(&'w self, f: F) -> std::io::Result<Handle<'w, 'scope, 'env, T>>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        self.spawn_with(std::thread::Builder::new(), f)
    }

    /// Spawn a thread named `name`, returning the OS refusal.
    pub(crate) fn spawn_named<T, F>(
        &'w self,
        name: String,
        f: F,
    ) -> std::io::Result<Handle<'w, 'scope, 'env, T>>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        self.spawn_with(std::thread::Builder::new().name(name), f)
    }

    fn spawn_with<T, F>(
        &'w self,
        builder: std::thread::Builder,
        f: F,
    ) -> std::io::Result<Handle<'w, 'scope, 'env, T>>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        let slot = Arc::new(Mutex::new(None));
        let theirs = Arc::clone(&slot);
        #[allow(
            clippy::disallowed_methods,
            reason = "the one sanctioned scoped spawn: the handle is registered and joined"
        )]
        let native = builder.spawn_scoped(self.inner, move || {
            let value = f();
            *theirs.lock().unwrap_or_else(PoisonError::into_inner) = Some(value);
        })?;
        let thread = native.thread().clone();
        let mut registry = self.registry.borrow_mut();
        let index = registry.len();
        registry.push(Some(native));
        drop(registry);
        Ok(Handle {
            scope: self,
            index,
            slot,
            thread,
        })
    }

    /// Join every thread not yet joined, in spawn order. Returns the first
    /// panic payload; later ones are forgotten.
    fn join_all(&self) -> Option<Box<dyn std::any::Any + Send>> {
        let mut first = None;
        let mut index = 0;
        loop {
            let native = {
                let mut registry = self.registry.borrow_mut();
                let Some(entry) = registry.get_mut(index) else {
                    break;
                };
                entry.take()
            };
            index += 1;
            if let Some(native) = native
                && let Err(payload) = native.join()
            {
                if first.is_none() {
                    first = Some(payload);
                } else {
                    std::mem::forget(payload);
                }
            }
        }
        first
    }
}

impl<T> Handle<'_, '_, '_, T> {
    /// Wait for the thread to exit, thread-local destructors included, and
    /// return its result, or its panic payload.
    pub(crate) fn join(self) -> std::thread::Result<T> {
        let native = self.scope.registry.borrow_mut()[self.index].take();
        if let Some(native) = native {
            native.join()?;
        }
        let value = self
            .slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        value.ok_or_else(|| Box::new("scoped thread produced no result") as _)
    }

    /// The thread's `std` handle.
    pub(crate) fn thread(&self) -> &std::thread::Thread {
        &self.thread
    }
}

/// `std::thread::scope`, with every spawned thread joined before it returns.
pub(crate) fn scope<'env, F, T>(body: F) -> T
where
    F: for<'w, 'scope> FnOnce(&'w Scope<'w, 'scope, 'env>) -> T,
{
    #[allow(
        clippy::disallowed_methods,
        reason = "the one sanctioned std scope: every thread in it is joined"
    )]
    std::thread::scope(|inner| {
        let scope = Scope {
            inner,
            registry: RefCell::new(Vec::new()),
            _owner: PhantomData,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&scope)));
        let child_panic = scope.join_all();
        match result {
            Ok(value) => {
                if let Some(payload) = child_panic {
                    std::panic::resume_unwind(payload);
                }
                value
            }
            Err(payload) => {
                if let Some(child) = child_panic {
                    std::mem::forget(child);
                }
                std::panic::resume_unwind(payload)
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::RefCell as Cell;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

    /// Set when the scope body has returned or unwound: a thread that waits
    /// for it is provably still running when the scope reaches its join.
    #[derive(Default)]
    struct BodyEnded {
        ended: AtomicBool,
        bell: pbfhogg_test_scratch::Bell,
    }

    impl BodyEnded {
        /// Wait (bounded) until the body has ended.
        fn wait(&self) {
            let deadline = std::time::Instant::now() + crate::test_support::OBSERVE_BOUND;
            assert!(
                self.bell.wait(deadline, || self.ended.load(SeqCst)),
                "the scope body never ended"
            );
        }
    }

    /// Marks the body ended when dropped, on a return or an unwind alike.
    struct EndBody<'a>(&'a BodyEnded);

    impl Drop for EndBody<'_> {
        fn drop(&mut self) {
            self.0.ended.store(true, SeqCst);
            self.0.bell.ring();
        }
    }

    /// Parks in its destructor until the scope body has ended (when given
    /// one to wait for), then records that it finished.
    struct SlowTeardown {
        finished: Arc<AtomicUsize>,
        body: Option<Arc<BodyEnded>>,
    }

    impl Drop for SlowTeardown {
        fn drop(&mut self) {
            if let Some(body) = &self.body {
                body.wait();
            }
            self.finished.fetch_add(1, SeqCst);
        }
    }

    /// The scope does not return until every thread's thread-local
    /// destructors have run, joined or not, borrowed results included. The
    /// unjoined threads' destructors wait for the body to end, so they are
    /// still running when the scope reaches its join; the joined one needs
    /// no wait, since the body itself joins it.
    #[test]
    fn scope_waits_for_thread_local_destructors() {
        thread_local! {
            static HELD: Cell<Option<SlowTeardown>> = const { Cell::new(None) };
        }
        let finished = Arc::new(AtomicUsize::new(0));
        let body = Arc::new(BodyEnded::default());
        let borrowed = [1, 2, 3];
        let total = scope(|s| {
            let _end = EndBody(&body);
            let mine = Arc::clone(&finished);
            let joined = s.spawn(move || {
                HELD.with(|h| {
                    *h.borrow_mut() = Some(SlowTeardown {
                        finished: mine,
                        body: None,
                    });
                });
                borrowed.iter().sum::<i32>()
            });
            for _ in 0..3 {
                let (mine, body) = (Arc::clone(&finished), Arc::clone(&body));
                drop(s.spawn(move || {
                    HELD.with(|h| {
                        *h.borrow_mut() = Some(SlowTeardown {
                            finished: mine,
                            body: Some(body),
                        });
                    });
                }));
            }
            joined.join().unwrap()
        });
        assert_eq!(total, 6);
        assert_eq!(finished.load(SeqCst), 4);
    }

    /// A joined panic is the joiner's; an unjoined one is re-raised after
    /// every thread has exited.
    #[test]
    fn panics_follow_join_ownership() {
        let caught = scope(|s| s.spawn(|| panic!("joined")).join().is_err());
        assert!(caught);

        let late = AtomicBool::new(false);
        let body = BodyEnded::default();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scope(|s| {
                let _end = EndBody(&body);
                drop(s.spawn(|| panic!("unjoined")));
                drop(s.spawn(|| {
                    body.wait();
                    late.store(true, SeqCst);
                }));
            });
        }));
        assert!(unwound.is_err());
        assert!(
            late.load(SeqCst),
            "re-raised only after every thread exited"
        );
    }

    /// A panicking body keeps its own payload and still joins every thread.
    #[test]
    fn a_panicking_body_joins_and_keeps_its_panic() {
        let late = AtomicBool::new(false);
        let body = BodyEnded::default();
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scope(|s| {
                let _end = EndBody(&body);
                drop(s.spawn(|| panic!("child")));
                drop(s.spawn(|| {
                    body.wait();
                    late.store(true, SeqCst);
                }));
                panic!("body");
            })
        }))
        .unwrap_err();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"body"));
        assert!(late.load(SeqCst));
    }
}
