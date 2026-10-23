//! First-failure record shared by the threads of one writer pool.
//!
//! Both thread pools in this module tree (`parallel_writer`'s `pwrite`
//! workers and `parallel_gzip`'s compressors + writer thread) need the same
//! three things when one participant fails: the *first* failure is the one
//! that surfaces, every other participant stops doing work as soon as it
//! can, and the owner joins every thread it started before returning. They
//! used to get none of it: `parallel_writer` returned on the first panicked
//! join and detached the rest of its pool, which went on writing into an
//! output the caller had already been told was a failure, and both pools
//! reported a panic as a fixed string with its payload thrown away.
//!
//! [`FirstFailure`] is the record. The contract every user of it keeps:
//!
//! - **Record before disconnect.** A participant that fails records the
//!   failure *before* it drops any channel end another participant can
//!   observe. A peer that then sees the disconnect and records the
//!   consequence ("channel closed") loses to the cause, because the record
//!   is first-wins under one mutex and the cause got there first.
//! - **`stopped()` implies a recorded failure.** The stop flag is published
//!   while the slot lock is held, after the slot is filled, so a thread that
//!   observes `stopped()` and then reads the slot never finds it empty.
//! - **`take()` only after every participant has been joined.** Taking
//!   earlier would empty the slot while `stopped()` still reads true.
//!
//! Stopping is cooperative: participants poll [`FirstFailure::stopped`]
//! between units of work (one `Acquire` load - a plain load on x86 - per
//! unit, where a unit is a multi-hundred-KB `pwrite` or a 2 MB gzip chunk),
//! so an operation already in progress finishes, and one more can start if
//! its stop check raced the publication. What the owner guarantees is that
//! no thread it started outlives the call, not that work halts instantly.

use crate::error::{Error, panic_error};

use std::any::Any;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// First failure recorded by any participant of one pool, plus the stop flag
/// the participants poll. See the module doc for the ordering contract.
pub(crate) struct FirstFailure {
    stop: AtomicBool,
    first: Mutex<Option<Error>>,
}

impl FirstFailure {
    pub(crate) const fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            first: Mutex::new(None),
        }
    }

    /// Nothing panics while this lock is held, so poisoning cannot happen
    /// through this type; recovering the guard keeps that true even if a
    /// future edit introduces a panic.
    fn slot(&self) -> MutexGuard<'_, Option<Error>> {
        self.first.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record `e` unless a failure is already recorded, and publish `stop`.
    /// A later failure is dropped: it is either a consequence of the first
    /// or an independent fault that does not outrank it. An `io::Error`
    /// carrying a `pbfhogg::Error` is recorded as that error.
    pub(crate) fn record(&self, e: impl Into<Error>) {
        let mut slot = self.slot();
        if slot.is_none() {
            *slot = Some(e.into());
        }
        // Published under the lock, so `stopped()` implies a filled slot.
        self.stop.store(true, Ordering::Release);
    }

    /// Record a caught panic, keeping its message. `who` names the thread
    /// role (`"parallel writer pool worker"`), so the surfaced error reads
    /// `"<who> panicked: <payload>"`.
    pub(crate) fn record_panic(&self, who: &str, payload: &(dyn Any + Send)) {
        self.record(panic_error(who, payload));
    }

    /// True once any participant has recorded a failure.
    #[inline]
    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// The recorded failure, removed. Call only after every participant has
    /// been joined.
    pub(crate) fn take(&self) -> Option<io::Error> {
        self.slot().take().map(Error::into_io)
    }

    /// A clone of the recorded failure, leaving the original in place for
    /// [`take`](Self::take). For the places that must answer *now* while the
    /// owner still has to surface the original later: flush-barrier acks, a
    /// producer whose send failed. Both views carry the same error (it
    /// shares one kind), inside an `io::Error` whose kind follows the
    /// `From<Error> for io::Error` table.
    pub(crate) fn restate(&self) -> Option<io::Error> {
        self.slot().clone().map(io::Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_record_wins_and_publishes_stop() {
        let f = FirstFailure::new();
        assert!(!f.stopped());
        f.record(io::Error::new(io::ErrorKind::StorageFull, "cause"));
        assert!(f.stopped());
        f.record(io::Error::new(io::ErrorKind::BrokenPipe, "consequence"));
        let restated = f.restate().expect("restate after record");
        assert_eq!(restated.kind(), io::ErrorKind::StorageFull);
        assert_eq!(restated.to_string(), "cause");
        let taken = f.take().expect("take after record");
        assert_eq!(taken.to_string(), "cause");
    }

    #[test]
    fn panic_payload_text_survives() {
        let n = 7;
        let payload = std::panic::catch_unwind(|| {
            if n > 0 {
                panic!("boom {n}");
            }
        })
        .expect_err("panicked");
        let f = FirstFailure::new();
        f.record_panic("pool worker", payload.as_ref());
        let msg = f.take().expect("recorded").to_string();
        assert_eq!(msg, "pool worker panicked: boom 7");
    }
}
