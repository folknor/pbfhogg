//! First-failure slot shared by the worker pool and the drain.
//!
//! A worker that fails never delivers its blob's seq to the drain. Without
//! a side channel the drain cannot tell "slow" from "never": it keeps
//! waiting for the missing seq, and under `--locations-on-ways` a missing
//! node seq also means the node-to-way barrier never fires, so the scanner
//! blocks on the barrier, the surviving workers block on the empty
//! candidate channel, and the run hangs. When it did not hang, the drain's
//! "producer dropped a seq" error was returned in place of the real cause.
//!
//! Protocol: every worker-pool error is published here BEFORE the
//! publishing thread releases its drain sender. The drain polls the slot
//! on every loop iteration and once more after it observes the drain
//! channel disconnect; because publication precedes sender release, a
//! disconnected channel with an empty slot proves no worker failed.
//! Returning the error from the drain drops the drain receiver and the
//! barrier sender, which unblocks the scanner and every worker.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::Error;

pub(super) struct FailureSlot {
    /// Set once by the first publication and never cleared, so "a worker
    /// failed" stays observable after the error itself has been taken.
    failed: AtomicBool,
    error: Mutex<Option<Error>>,
}

impl FailureSlot {
    pub(super) fn new() -> Self {
        Self {
            failed: AtomicBool::new(false),
            error: Mutex::new(None),
        }
    }

    /// Record `error` if it is the first failure; later ones are
    /// consequences of the shutdown it triggers and are dropped.
    pub(super) fn publish(&self, error: Error) {
        let mut slot = self
            .error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.failed.swap(true, Ordering::AcqRel) {
            *slot = Some(error);
        }
    }

    /// True once any failure has been published.
    pub(super) fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// Take the first failure, if one was published and not yet taken.
    pub(super) fn take(&self) -> Option<Error> {
        if !self.has_failed() {
            return None;
        }
        self.error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}
