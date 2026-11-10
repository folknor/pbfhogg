//! Parallel per-frame work over one sequential frame walk.
//!
//! [`map_frames_ordered`] keeps the input a single front-to-back
//! [`Frames`] walk on the calling thread - so it reads a stream (a FIFO,
//! `/dev/stdin`) as well as a file, with the walk's own framing, header
//! position and partial-index handling - and hands each frame it admits to
//! a worker pool for the expensive part (decompression and a payload scan).
//! The caller folds the workers' results on the calling thread in frame
//! order, so a report or an output built from them is the one a sequential
//! loop would build.
//!
//! It runs on [`run_ordered_streams_from`] with terminal-only tasks: the walk
//! is the source, so a failure to read frame `k` surfaces only after every
//! earlier frame's result has been folded, and a worker's failure at frame
//! `k` outranks any later read failure or worker failure - the order a
//! sequential loop reports them in. The source issues up to a window of
//! frames ahead of the fold, so on a stream a failure at one frame can wait
//! for the stream's writer to deliver the next frames (or close the stream)
//! before it is reported; cancellation is observed between frames and at
//! the pool's checkpoints, never inside a blocking read.
//!
//! Memory: at most `workers + FRAME_POOL_WINDOW_SLACK` raw frames are
//! issued and not yet folded, and each worker holds its own scratch (one
//! decompressed block, for the callers here).

use std::convert::Infallible;

use crate::error::{Location, Result};
use crate::ordered_pool::{Emitter, StreamConfig, StreamConsumer, run_ordered_streams_from};
use crate::read::frame::{FrameSource, Frames, RawBlobFrame};
use crate::tunables::{
    DECODE_WORKERS_FALLBACK, DECODE_WORKERS_MIN, DECODE_WORKERS_RESERVED, FRAME_POOL_WINDOW_SLACK,
};

/// Run `work` on every frame of `frames` that `admit` keeps, on a pool of
/// workers, and hand each result to `fold` on the calling thread in frame
/// order.
///
/// `admit` runs on the calling thread as each frame is read: `None` skips
/// the frame (it is never worked or folded, though its payload has already
/// been read with it, so this cannot skip reading one; `sort`'s pass 1,
/// which must, runs its own header-first walk), `Some(payload)` issues it.
/// `local` builds each worker's scratch on its own thread. A `work` failure
/// is located at its frame, as `Frames::locate` locates one; a `fold`
/// failure is the caller's own and is returned as it is.
pub(crate) fn map_frames_ordered<S, P, L, E>(
    frames: &mut Frames<S>,
    host: &crate::host::Entered<'_>,
    mut admit: impl FnMut(RawBlobFrame) -> Option<P>,
    local: impl Fn() -> L + Sync,
    work: impl Fn(&mut L, P) -> Result<E> + Sync,
    fold: impl FnMut(E) -> Result<()>,
) -> Result<()>
where
    S: FrameSource,
    P: Send,
    E: Send,
{
    let workers = crate::host::sized_workers(
        &*host.resources,
        DECODE_WORKERS_RESERVED,
        DECODE_WORKERS_MIN,
        DECODE_WORKERS_FALLBACK,
    );
    let config = StreamConfig {
        window: std::num::NonZeroUsize::new(workers.saturating_add(FRAME_POOL_WINDOW_SLACK))
            .unwrap_or(std::num::NonZeroUsize::MIN),
        // Terminal-only tasks: one slot lets a worker deposit its result and
        // take the next frame without waiting for the fold.
        depth: 1,
        cancel: &host.invocation,
    };
    let mut source = || -> Result<Option<(P, Location)>> {
        while let Some(frame) = frames.next_raw()? {
            let location = frames.location(frame.id);
            if let Some(payload) = admit(frame) {
                return Ok(Some((payload, location)));
            }
        }
        Ok(None)
    };
    let task = |local: &mut L,
                _task: usize,
                (payload, location): (P, Location),
                _emitter: &Emitter<'_, Infallible, E>|
     -> Result<E> { work(local, payload).map_err(|e| e.locate_blob(location)) };
    run_ordered_streams_from(
        &mut source,
        workers,
        config,
        |_| local(),
        task,
        &mut Fold(fold),
    )
}

/// The calling thread's side: every task ends in exactly one result and
/// emits no items.
struct Fold<F>(F);

impl<E, F: FnMut(E) -> Result<()>> StreamConsumer<Infallible, E> for Fold<F> {
    fn item(&mut self, _task: usize, item: Infallible) -> Result<()> {
        match item {}
    }

    fn finish_task(&mut self, _task: usize, end: E) -> Result<()> {
        (self.0)(end)
    }
}
