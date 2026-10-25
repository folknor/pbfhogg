//! The one driver of the block-pair merge over two indexed inputs, for the
//! text diff and derive (`diff --format osc`) alike.
//!
//! Per kind it plans shards ([`super::shard::plan_shards`]) and runs
//! [`block_pair_merge_phase`] once per shard. A phase that plans one shard
//! (always at `-j 1`) runs inline on the caller's thread, recording straight
//! into the command's final sink; a phase with more runs each shard on a
//! scoped thread with its own sink, whose output the command then absorbs in
//! shard order. The command supplies both sides of that through
//! [`ShardedReport`].

use crate::ElementKind;
use crate::error::Result;
use crate::osc::merge_join::{
    BlockPairMergeStats, BlockPairSide, MergeSink, block_pair_merge_phase,
};

use super::shard::{Shard, WalkedFile, WalkedPair, plan_shards};

/// A command's side of a sharded comparison.
pub(super) trait ShardedReport {
    /// A shard's own sink, moved to its worker thread.
    type Shard: MergeSink + Send;
    /// What a finished shard hands back for [`ShardedReport::absorb`].
    type Done: Send;

    /// Names a worker panic in the error it becomes.
    const WORKER: &'static str;

    /// The final sink, which a one-shard phase records into directly.
    fn inline(&mut self) -> &mut impl MergeSink;

    /// A sink for shard `idx` of `kind`'s phase. Called on the caller's
    /// thread before the phase's workers start.
    fn open_shard(&mut self, kind: ElementKind, idx: usize) -> Result<Self::Shard>;

    /// Close a shard's sink on its worker once its merge succeeded (flush
    /// its files).
    fn finish_shard(shard: Self::Shard) -> Result<Self::Done>;

    /// Fold every shard of a phase, in shard order, into the final sink.
    fn absorb(&mut self, done: Vec<Self::Done>, cancel: &crate::host::Cancel) -> Result<()>;

    /// Observe a phase's plan (sidecar counters).
    fn on_plan(&mut self, _kind: ElementKind, _shards: &[Shard]) {}

    /// The fault plan's shard-panic hook for this command.
    #[cfg(feature = "test-hooks")]
    fn panic_hook(faults: &crate::test_hooks::FaultPlan) -> &std::sync::atomic::AtomicUsize;

    /// The panic message of an armed shard.
    #[cfg(feature = "test-hooks")]
    fn panic_message(idx: usize) -> String;
}

/// One phase to run: its kind and its sidecar markers.
pub(super) struct Phase {
    pub(super) kind: ElementKind,
    pub(super) start_marker: &'static str,
    pub(super) end_marker: &'static str,
}

/// Run `phases` over both walked inputs with up to `jobs` shards per kind,
/// and return the merge's shadow counters summed over every shard.
///
/// A phase whose kind neither input holds is skipped, markers included.
/// When the whole run is one shard per kind (`jobs` at most one, or an input
/// holding a negative ID), each side's descriptor is re-advised for
/// sequential reading: the walker opened it `RANDOM` for the header walk,
/// and one shard reads each side's blobs in file order, which readahead
/// serves. A multi-shard run keeps `RANDOM`, the measured shape.
pub(super) fn run<R: ShardedReport>(
    walked: &WalkedPair,
    phases: &[Phase],
    jobs: usize,
    skip_equal_blobs: bool,
    report: &mut R,
    host: &crate::host::Host,
) -> Result<BlockPairMergeStats> {
    if jobs <= 1 || walked.has_negative {
        for file in [&walked.old.file, &walked.new.file] {
            crate::fadvise::advise(file, 0, 0, crate::fadvise::Advice::Sequential);
        }
    }
    let mut total = BlockPairMergeStats::default();
    for phase in phases {
        let kind = phase.kind;
        let (old_descs, new_descs) = (walked.old.kind(kind), walked.new.kind(kind));
        if old_descs.is_empty() && new_descs.is_empty() {
            continue;
        }
        crate::debug::emit_marker(phase.start_marker);
        let shards = plan_shards(old_descs, new_descs, jobs, walked.has_negative);
        report.on_plan(kind, &shards);
        let stats = if let [shard] = shards.as_slice() {
            run_shard(
                *shard,
                &walked.old,
                &walked.new,
                kind,
                skip_equal_blobs,
                report.inline(),
                &host.cancel,
            )?
        } else {
            run_sharded_phase(walked, kind, &shards, skip_equal_blobs, report, host)?
        };
        total.add(&stats);
        crate::debug::emit_marker(phase.end_marker);
    }
    Ok(total)
}

/// Run one phase's shards on scoped threads, then absorb their outputs in
/// shard order.
fn run_sharded_phase<R: ShardedReport>(
    walked: &WalkedPair,
    kind: ElementKind,
    shards: &[Shard],
    skip_equal_blobs: bool,
    report: &mut R,
    host: &crate::host::Host,
) -> Result<BlockPairMergeStats> {
    // Opened on this thread: a failure here stops the phase before any
    // worker starts, and files already created are left for the run
    // directory's guard.
    let mut sinks = Vec::with_capacity(shards.len());
    for idx in 0..shards.len() {
        sinks.push(report.open_shard(kind, idx)?);
    }

    let results: Vec<Result<(R::Done, BlockPairMergeStats)>> = std::thread::scope(|s| {
        let handles: Vec<_> = shards
            .iter()
            .copied()
            .zip(sinks)
            .enumerate()
            .map(|(idx, (shard, mut sink))| {
                s.spawn(move || {
                    // Test-only: the fault plan's shard-panic hook simulates
                    // a mid-phase shard crash. The join below turns the
                    // panic into an error and the run directory's guard
                    // sweeps every shard file. Release builds compile this
                    // out entirely.
                    #[cfg(feature = "test-hooks")]
                    if R::panic_hook(host.cancel.faults())
                        .load(std::sync::atomic::Ordering::Relaxed)
                        == idx
                    {
                        panic!("{}", R::panic_message(idx));
                    }
                    #[cfg(not(feature = "test-hooks"))]
                    let _ = idx;
                    let stats = run_shard(
                        shard,
                        &walked.old,
                        &walked.new,
                        kind,
                        skip_equal_blobs,
                        &mut sink,
                        &host.cancel,
                    )?;
                    Ok((R::finish_shard(sink)?, stats))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|payload| {
                    Err(crate::error::panic_error(R::WORKER, payload.as_ref()))
                })
            })
            .collect()
    });

    // Partition into (successful, first error). On an error every shard file
    // is left for the run directory's guard. Shards run independently, so
    // slot order is not time order: a real failure in any shard outranks the
    // cancellation the others observed, which a signal racing that failure
    // would otherwise mask.
    let mut done = Vec::with_capacity(results.len());
    let mut stats = BlockPairMergeStats::default();
    let mut first_err: Option<crate::error::Error> = None;
    for result in results {
        match result {
            Ok((d, s)) => {
                done.push(d);
                stats.add(&s);
            }
            Err(e) => {
                if first_err
                    .as_ref()
                    .is_none_or(|kept| kept.is_cancelled() && !e.is_cancelled())
                {
                    first_err = Some(e);
                }
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    report.absorb(done, &host.cancel)?;
    Ok(stats)
}

/// Merge one shard of one kind into `sink`.
fn run_shard<S: MergeSink>(
    shard: Shard,
    old: &WalkedFile,
    new: &WalkedFile,
    kind: ElementKind,
    skip_equal_blobs: bool,
    sink: &mut S,
    cancel: &crate::host::Cancel,
) -> Result<BlockPairMergeStats> {
    let mut old_side = BlockPairSide::new(
        &old.file,
        &old.path,
        &old.kind(kind)[shard.old_start..shard.old_end],
        shard.window,
    );
    let mut new_side = BlockPairSide::new(
        &new.file,
        &new.path,
        &new.kind(kind)[shard.new_start..shard.new_end],
        shard.window,
    );
    let mut stats = BlockPairMergeStats::default();
    block_pair_merge_phase(
        &mut old_side,
        &mut new_side,
        kind,
        skip_equal_blobs,
        sink,
        &mut stats,
        cancel,
    )?;
    Ok(stats)
}
