//! Sequence admission window between the scanner and the drain.
//!
//! The scanner takes one token before it dispatches a seq (on either route,
//! to the drain directly or to the worker pool); the drain gives it back
//! once it has processed that seq in order. So at most
//! [`APPLY_CHANGES_SEQ_WINDOW`](crate::tunables::APPLY_CHANGES_SEQ_WINDOW)
//! seqs are ever reserved and not yet processed, wherever they sit: in the
//! candidate channel, in a worker, in the drain channel or in the drain's
//! reorder map. Without it the reorder map grows with how long a slow head
//! seq takes, since every later seq keeps completing behind it.
//!
//! This bounds outstanding sequences, not bytes. A rewritten blob carries
//! every output frame its walk produced, and that grows with the OSC
//! creates falling in its range, so a count is not a byte ceiling.
//!
//! Liveness: the head seq always holds a token, and the drain receives
//! every message whatever the window state, so the head arrives and is
//! processed, returning its token. The scanner holds a token while blocked
//! sending a candidate; workers drain the candidate channel without needing
//! another token. The `--locations-on-ways` node boundary takes no token:
//! it is not a seq and nothing returns one for it. The scanner takes no
//! token while it waits at the barrier. The token sender lives only in
//! [`WindowReturn`], owned by the drain, so every drain exit (an error, a
//! cancellation, a published producer failure) wakes a parked scanner with
//! a disconnect. Every reserved seq still owes the drain one item or a
//! published failure, as before: a seq dropped silently would now hang the
//! scanner on an exhausted window rather than surface at channel close.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

use crate::error::{Error, Result};

/// Outstanding-seq accounting shared by both ends: incremented by the
/// scanner before dispatch, decremented by the drain before it returns the
/// token, so `now` never exceeds the window and `high` is exact.
#[derive(Default)]
struct Outstanding {
    now: AtomicU64,
    high: AtomicU64,
}

/// The scanner's end: takes a token per seq.
pub(super) struct WindowAdmit {
    tokens: Receiver<()>,
    outstanding: Arc<Outstanding>,
    /// Time spent parked on an exhausted window (blocking fallback only;
    /// a token available on the first try costs nothing here).
    wait_ns: u64,
    /// Told when an admission takes the blocking branch (test builds only),
    /// so a test knows the scanner is past its non-blocking try.
    #[cfg(test)]
    blocking: Option<std::sync::mpsc::Sender<()>>,
}

/// The drain's end: returns a token per processed seq. The only sender.
pub(super) struct WindowReturn {
    tokens: SyncSender<()>,
    outstanding: Arc<Outstanding>,
}

/// A fresh window of `size` tokens.
pub(super) fn seq_window(size: usize) -> (WindowAdmit, WindowReturn) {
    let (tx, rx) = sync_channel::<()>(size);
    // loop-ends: fills a fresh channel once per token; never waits
    for _ in 0..size {
        // A fresh channel with `size` capacity cannot be full or closed.
        tx.try_send(()).ok();
    }
    let outstanding = Arc::new(Outstanding::default());
    (
        WindowAdmit {
            tokens: rx,
            outstanding: Arc::clone(&outstanding),
            wait_ns: 0,
            #[cfg(test)]
            blocking: None,
        },
        WindowReturn {
            tokens: tx,
            outstanding,
        },
    )
}

impl WindowAdmit {
    /// Reserve one seq. Blocks while the window is exhausted; fails once
    /// the drain has returned (its sender is gone), like a failed send.
    pub(super) fn admit(&mut self) -> Result<()> {
        match self.tokens.try_recv() {
            Ok(()) => {}
            Err(TryRecvError::Disconnected) => return Err(window_closed()),
            Err(TryRecvError::Empty) => {
                #[cfg(test)]
                if let Some(blocking) = &self.blocking {
                    blocking.send(()).ok();
                }
                let start = crate::counters::Stopwatch::start();
                let got = self.tokens.recv();
                self.wait_ns += start.elapsed_ns();
                got.map_err(|_| window_closed())?;
            }
        }
        let now = self.outstanding.now.fetch_add(1, Ordering::Relaxed) + 1;
        self.outstanding.high.fetch_max(now, Ordering::Relaxed);
        Ok(())
    }

    /// Nanoseconds the scanner spent parked on the window.
    pub(super) fn wait_ns(&self) -> u64 {
        self.wait_ns
    }
}

impl WindowReturn {
    /// Give back the token of a seq the drain has just processed.
    pub(super) fn release(&self) {
        self.outstanding.now.fetch_sub(1, Ordering::Relaxed);
        // Tokens in the channel plus outstanding seqs never exceed the
        // window, so this never blocks; a closed receiver (the scanner has
        // returned) needs no token back.
        self.tokens.try_send(()).ok();
    }

    /// Most seqs ever reserved and not yet processed at once.
    pub(super) fn high_water(&self) -> u64 {
        self.outstanding.high.load(Ordering::Relaxed)
    }
}

fn window_closed() -> Error {
    Error::internal("scanner admission window closed by the drain")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::seq_window;

    #[test]
    fn the_window_refills_only_as_seqs_are_processed() {
        let (mut admit, ret) = seq_window(2);
        admit.admit().unwrap();
        admit.admit().unwrap();
        assert!(admit.tokens.try_recv().is_err());
        ret.release();
        admit.admit().unwrap();
        assert_eq!(ret.high_water(), 2);
    }

    /// The drain is dropped only once the scanner has taken the blocking
    /// branch, so the error is the blocking receive's, whether the drop
    /// lands before that receive parks or after: a disconnect wakes it
    /// either way. The drop is the release, on an unwind too.
    #[test]
    fn a_returned_drain_wakes_a_parked_scanner() {
        let (mut admit, ret) = seq_window(1);
        admit.admit().unwrap();
        let (blocking_tx, blocking_rx) = std::sync::mpsc::channel();
        admit.blocking = Some(blocking_tx);
        let ret = std::sync::Mutex::new(Some(ret));
        let release = || drop(ret.lock().unwrap().take());
        let (admitted, ()) = crate::test_support::with_parked_thread(
            "a returned drain did not wake the parked scanner",
            move || admit.admit(),
            release,
            || {
                blocking_rx
                    // wait-deliberate: the scanner reaches the blocking branch at once; expiry fails
                    .recv_timeout(crate::test_support::OBSERVE_BOUND)
                    .expect("the scanner took the blocking branch");
                release();
            },
        );
        assert!(admitted.is_err());
    }
}
