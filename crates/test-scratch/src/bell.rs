//! A doorbell for state a test owns or a test build observes: a ring count
//! under a mutex, and the condition variable every waiter sleeps on.
//!
//! The writer stores its state (an atomic, typically), then rings. A waiter
//! snapshots the ring count, evaluates its condition without the bell's
//! lock, and sleeps only while the count is unchanged, so a ring between
//! the evaluation and the sleep is never lost, and the condition may take
//! any other lock. This is the library fault plan's bell protocol, for
//! state outside a fault plan.
//!
//! The bell is a notification, not a snapshot: a condition over several
//! values reads them one by one, and a transient state that comes and goes
//! between two evaluations can be missed. Monotonic flags, counts and maxima
//! fit it; a handshake that needs more keeps its state under one lock.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

/// See the module docs.
#[derive(Debug, Default)]
pub struct Bell {
    rings: Mutex<u64>,
    rung: Condvar,
}

impl Bell {
    /// A bell nobody has rung.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rings: Mutex::new(0),
            rung: Condvar::new(),
        }
    }

    fn rings(&self) -> MutexGuard<'_, u64> {
        self.rings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wake every waiter: call after the store it announces.
    pub fn ring(&self) {
        let mut rings = self.rings();
        *rings = rings.wrapping_add(1);
        self.rung.notify_all();
    }

    /// Wait until `cond` holds, at most until `deadline`: whether it held.
    /// `cond` is evaluated without the bell's lock, after every ring.
    pub fn wait(&self, deadline: Instant, mut cond: impl FnMut() -> bool) -> bool {
        loop {
            let seen = *self.rings();
            if cond() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let rings = self.rings();
            drop(
                self.rung
                    // wait-deliberate: the bell's park; the caller's deadline fails it
                    .wait_timeout_while(rings, deadline - now, |rings| *rings == seen)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Bell;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::time::{Duration, Instant};

    #[test]
    fn a_ring_after_the_store_wakes_the_waiter() {
        let bell = Bell::new();
        let flag = AtomicBool::new(false);
        // fixture-time: a failure bound for this crate's own test
        let deadline = Instant::now() + Duration::from_secs(10);
        std::thread::scope(|s| {
            let waiter = s.spawn(|| bell.wait(deadline, || flag.load(SeqCst)));
            flag.store(true, SeqCst);
            bell.ring();
            assert!(waiter.join().expect("waiter"));
        });
    }

    #[test]
    fn a_condition_that_never_holds_ends_at_the_deadline() {
        let bell = Bell::new();
        assert!(!bell.wait(Instant::now(), || false));
    }
}
