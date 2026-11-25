//! Rewritten blobs whose output cannot travel to the drain as one held item:
//! a block that overflowed the PBF data blob limits, or frames past the
//! retention threshold.
//!
//! An apply-changes output block can outgrow the limit although everything
//! that went into it fits on its own: a rewritten block carries its source
//! block's whole string table, because surviving base elements keep their
//! string indices, plus the strings of every OSC element written into it,
//! and blocks are cut by element count, not by bytes. A 17 MiB tag value
//! modified to another 17 MiB value makes a 34 MiB block from a source block
//! and an OSC element that each fit. And a blob's frames can be many times
//! its input: every block repeats the strings its elements share, so one
//! large role on many relation creates is framed once per block.
//!
//! Nothing is sized up front, so normal input pays nothing beyond a byte
//! count per frame: the first pass ([`RewriteSink`]) records an overflowing
//! block's framing refusal with the range of operations it held, and stops
//! holding frames once they pass `APPLY_CHANGES_REWRITE_RETAIN_BYTES`. A
//! blob that did either is *deferred*:
//!
//! 1. Every overflow is planned ([`plan`]) by trial writes whose frames are
//!    dropped as they are framed, so planning holds no output. The compact
//!    strategy comes first: an attempt's survivors are written with their
//!    string indices translated into a table holding only the source strings
//!    that attempt's survivors use (`BlockBuilder::pre_seed_subset_string_table`),
//!    with the OSC elements' strings beside them. An attempt that overflows
//!    anywhere is dropped and each half tried with its own strings, so a
//!    single element is tried with exactly its own: a survivor that grew
//!    (refreshed `--locations-on-ways` coordinates) beside a large string the
//!    diff removed, or survivors alternating with large OSC values, no
//!    longer carry the dead string into every frame. Where the compact
//!    strategy ends in a framing refusal (a single element), the whole-table
//!    one plans every overflow again: survivors' blocks carry the source
//!    table as in the first pass, an attempt starts unseeded so OSC elements
//!    before the first survivor go into a block without it, and a block of
//!    an attempt that still overflows is planned again on its own, halved
//!    when it was the whole attempt. Compact output is smaller for the same
//!    partition but its cuts and index widths differ (strings are interned
//!    by content, so an index can land wider than its source slot, and a
//!    smaller payload can compress to a larger body), so it is not a superset
//!    of what the whole table recovers; the fallback keeps every recovery
//!    that strategy finds. A plan is a tree of attempts ([`Plan`]): every
//!    attempt is a pure function of its operations and strategy (its subset
//!    is marked over its own operations, its builder starts fresh), so
//!    writing it again yields the same blocks.
//! 2. Then the walk's own refusal, if any, is returned, so a refused blob
//!    sends nothing; otherwise the worker sends the blob's item with a frame
//!    stream ([`RewrittenFrames::Stream`](super::descriptor::RewrittenFrames))
//!    and writes the blob a second time ([`EmitSink`]): the walk again, each
//!    block framed and sent as it completes, and in place of each recorded
//!    overflow its plan, written the same way. A block that differs from the
//!    record (an overflow the plan does not hold, a recorded one that now
//!    frames) is an internal error. At most the stream's few frames are in
//!    flight, so the blob's output is never resident, however large.
//!
//! What this does not cure, and stays a loud refusal before publication: an
//! element that does not fit a block alone with only its own strings,
//! refused with that element's own framing error. Halving is not an
//! exhaustive search either: a block's size depends on the elements beside
//! it (a string another element interned first lands at a narrower index,
//! a compact seed is in source-slot order, dense deltas and the compressed
//! body change with neighbours), so a partition can exist that halving does
//! not find, which needs blocks within bytes of the limit.

use std::cell::RefCell;
use std::ops::Range;
use std::sync::mpsc::SyncSender;

use crate::ElementKind;
use crate::PrimitiveBlock;
use crate::block_builder::BlockBuilder;
use crate::commands::{BlockSink, FramedBlocks, flush_local, is_block_overflow};
use crate::error::Error;
use crate::osc::sorted::SortedDiff;
use crate::tunables::APPLY_CHANGES_REWRITE_RETAIN_BYTES;
use crate::writer::Compression;

use super::Result;
use super::descriptor::StreamMsg;
use super::element_writes::{BaseWrites, RefreshScratch, SourceSeed, mark_used_slots};
use super::failure::FailureSlot;
use super::loc_mode::LocMode;
use super::refresh::StaleProbe;
use super::rewrite_block::{Op, OpContext, Step, Steps, write_op};
use super::stats::MergeStats;

/// A block sink that numbers the operations written into its blocks: the
/// writer calls [`op_written`](Self::op_written) after each, and every
/// flush happens before the next operation is added (capacity, re-seeding
/// and the final flush alike), so a flushed block holds exactly the
/// operations since the last.
pub(super) trait OpSink: BlockSink {
    fn op_written(&mut self);
}

/// An output block that overflowed: the operations it held, numbered in
/// write order, and its framing refusal.
pub(super) struct Overflow {
    ops: Range<usize>,
    error: Error,
}

impl Overflow {
    pub(super) fn ops(&self) -> Range<usize> {
        self.ops.clone()
    }
}

/// The first pass's sink, and a planning trial's: frames like
/// [`FramedBlocks`], and records a block that overflows the blob limits,
/// with the operations it held, rather than failing. The first pass holds
/// its frames until they pass `APPLY_CHANGES_REWRITE_RETAIN_BYTES`, then
/// releases them and holds none; a trial holds none.
pub(super) struct RewriteSink<'a> {
    pub(super) framed: FramedBlocks<'a>,
    next_op: usize,
    block_start: usize,
    pub(super) overflows: Vec<Overflow>,
    /// The bytes of frames held before they are released; `None` for a
    /// trial.
    retain: Option<usize>,
    held: usize,
    released: bool,
}

impl<'a> RewriteSink<'a> {
    /// The first pass's sink.
    pub(super) fn first_pass(framed: FramedBlocks<'a>) -> Self {
        Self::new(framed, Some(APPLY_CHANGES_REWRITE_RETAIN_BYTES), 0)
    }

    /// A trial's sink, numbering operations from `start`.
    fn trial(framed: FramedBlocks<'a>, start: usize) -> Self {
        Self::new(framed, None, start)
    }

    fn new(framed: FramedBlocks<'a>, retain: Option<usize>, start: usize) -> Self {
        Self {
            framed,
            next_op: start,
            block_start: start,
            overflows: Vec::new(),
            retain,
            held: 0,
            released: false,
        }
    }

    /// Whether the blob cannot be sent as held frames: a block overflowed,
    /// or the frames were released.
    pub(super) fn deferred(&self) -> bool {
        self.released || !self.overflows.is_empty()
    }
}

impl OpSink for RewriteSink<'_> {
    #[inline]
    fn op_written(&mut self) {
        self.next_op += 1;
    }
}

impl BlockSink for RewriteSink<'_> {
    fn flush(&mut self, bb: &mut BlockBuilder) -> Result<()> {
        let ops = self.block_start..self.next_op;
        let before = self.framed.frames.len();
        if let Some(error) = self.framed.flush_or_overflow(bb)? {
            self.overflows.push(Overflow { ops, error });
        }
        self.block_start = self.next_op;
        match self.retain {
            Some(limit) if !self.released => {
                if let Some((_, frame)) = self.framed.frames.get(before) {
                    self.held = self.held.saturating_add(frame.len());
                }
                if self.held > limit {
                    self.framed.frames = Vec::new();
                    self.released = true;
                }
            }
            _ => self.framed.frames.clear(),
        }
        Ok(())
    }
}

/// The worker's slot for the sender of the stream it is writing, kept
/// outside the worker's unwind boundary (`run_workers`) so the sender is
/// dropped only after a failure has been published: the drain then never
/// sees a stream close without a cause it can take.
pub(super) type StreamSlot = RefCell<Option<SyncSender<StreamMsg>>>;

/// Where a deferred blob's frames go, and what stops writing them: the
/// invocation's token, and a failure published by any producer, which the
/// drain reading this stream would otherwise learn of only when the stream
/// ends. Checked before every operation, trial and frame.
pub(super) struct StreamTx<'a> {
    pub(super) slot: &'a StreamSlot,
    pub(super) failure: &'a FailureSlot,
    pub(super) cancel: &'a crate::host::Recorder,
}

impl StreamTx<'_> {
    /// `Err` once the token is tripped or a failure is published. The
    /// latter's error is never reported: the published one is first.
    pub(super) fn check(&self) -> Result<()> {
        self.cancel.check()?;
        if self.failure.has_failed() {
            return Err(Error::internal(
                "apply-changes: stopped writing a deferred blob after a failure was published",
            ));
        }
        Ok(())
    }

    fn send(&self, msg: StreamMsg) -> Result<()> {
        self.check()?;
        let slot = self.slot.borrow();
        let tx = slot
            .as_ref()
            .ok_or_else(|| Error::internal("apply-changes: a deferred blob has no open stream"))?;
        tx.send(msg)
            .map_err(|_| Error::internal("apply-changes: the drain closed a blob's frame stream"))
    }

    /// Send the end of the stream and close it. Only a successful send
    /// clears the slot; a failed one leaves the sender for the worker's
    /// failure publication to outlive.
    fn end(&self) -> Result<()> {
        self.send(StreamMsg::End)?;
        drop(self.slot.borrow_mut().take());
        Ok(())
    }
}

/// What writing a rewrite's operations again needs: the same inputs the
/// rewrite had.
#[derive(Clone, Copy)]
pub(super) struct Inputs<'a> {
    pub(super) block: &'a PrimitiveBlock,
    pub(super) diff: &'a SortedDiff,
    pub(super) kind: ElementKind,
    pub(super) loc: LocMode<'a>,
    pub(super) probe: &'a StaleProbe,
    pub(super) compression: &'a Compression,
    pub(super) metrics: &'a crate::write::metrics::WriterMetrics,
}

/// The operations the rewrite wrote, in order, up to `end`. The steps are a
/// pure function of the block, the diff and the inline slice, so this walk
/// yields what the first one wrote (no walk, no stats).
pub(super) fn collect_ops<'a>(
    inputs: Inputs<'a>,
    inline: Range<usize>,
    end: usize,
    tx: &StreamTx<'_>,
) -> Result<Vec<Op<'a>>> {
    let mut ops = Vec::new();
    for step in Steps::new(inputs.block, inputs.diff, inputs.kind, inline) {
        if ops.len() == end {
            break;
        }
        tx.check()?;
        match step? {
            Step::Create(op) | Step::Element { op: Some(op), .. } => ops.push(op),
            Step::Observe(_) | Step::Element { op: None, .. } => {}
        }
    }
    if ops.len() < end {
        return Err(Error::internal(format!(
            "apply-changes overflow replay found {} operations where the rewrite wrote {end}",
            ops.len()
        )));
    }
    Ok(ops)
}

/// How an attempt writes survivors' blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Strategy {
    /// Blocks of survivors carry only the source strings the attempt's
    /// survivors use.
    Compact,
    /// Blocks of survivors carry the source block's whole table, as the
    /// first pass's do.
    WholeTable,
}

/// How a recorded overflow's operations are written: a tree of attempts.
pub(super) enum Plan {
    /// One attempt at `ops` under `strategy`, with, for each block of it
    /// that overflowed, in order, the plan of that block's operations (a
    /// compact attempt is kept only when none did).
    Attempt {
        ops: Range<usize>,
        strategy: Strategy,
        children: Vec<Plan>,
    },
    /// Consecutive pieces, together covering a range.
    Seq(Vec<Plan>),
}

impl Plan {
    fn range(&self) -> Range<usize> {
        match self {
            Plan::Attempt { ops, .. } => ops.clone(),
            Plan::Seq(parts) => {
                let start = parts.first().map_or(0, |p| p.range().start);
                let end = parts.last().map_or(0, |p| p.range().end);
                start..end
            }
        }
    }

    /// How many attempts deep the plan nests, each needing scratch of its
    /// own while the ones above it are mid-write.
    pub(super) fn depth(&self) -> usize {
        match self {
            Plan::Attempt { children, .. } => {
                1 + children.iter().map(Plan::depth).max().unwrap_or(0)
            }
            Plan::Seq(parts) => parts.iter().map(Plan::depth).max().unwrap_or(0),
        }
    }
}

/// Plan every recorded overflow, in order: the compact strategy for all,
/// or, where it ends in a framing refusal, the whole-table one for all,
/// which refuses only what the whole-table recovery refused before the
/// compact one existed.
pub(super) fn plan(
    inputs: Inputs<'_>,
    ops: &[Op<'_>],
    overflows: &[Overflow],
    scratch: &mut RefreshScratch,
    tx: &StreamTx<'_>,
) -> Result<Vec<Plan>> {
    let mut planner = Planner {
        inputs,
        ops,
        scratch,
        tx,
    };
    let compact = overflows
        .iter()
        .map(|o| planner.compact(o.ops.clone()))
        .collect::<Result<Vec<_>>>();
    match compact {
        Ok(plans) => Ok(plans),
        Err(e) if is_block_overflow(&e) => overflows
            .iter()
            .map(|o| planner.whole(o.ops.clone()))
            .collect(),
        Err(e) => Err(e),
    }
}

struct Planner<'p, 'a> {
    inputs: Inputs<'a>,
    ops: &'p [Op<'a>],
    scratch: &'p mut RefreshScratch,
    tx: &'p StreamTx<'p>,
}

impl Planner<'_, '_> {
    /// One trial of `range`: the blocks that overflowed.
    fn trial(&mut self, range: Range<usize>, strategy: Strategy) -> Result<Vec<Overflow>> {
        let mut sink = RewriteSink::trial(
            FramedBlocks::new(*self.inputs.compression, self.inputs.metrics),
            range.start,
        );
        write_attempt(
            self.inputs,
            self.ops,
            range,
            strategy,
            self.scratch,
            &mut sink,
            self.tx,
        )?;
        Ok(sink.overflows)
    }

    /// The compact plan of `range`: kept whole if nothing in it overflowed,
    /// otherwise each half planned the same way with its own strings, so a
    /// single element is tried with exactly its own. A single element that
    /// still overflows is refused with its framing error.
    fn compact(&mut self, range: Range<usize>) -> Result<Plan> {
        let overflows = self.trial(range.clone(), Strategy::Compact)?;
        let Some(first) = overflows.into_iter().next() else {
            return Ok(Plan::Attempt {
                ops: range,
                strategy: Strategy::Compact,
                children: Vec::new(),
            });
        };
        if range.len() == 1 {
            return Err(first.error);
        }
        let mid = range.start + range.len() / 2;
        Ok(Plan::Seq(vec![
            self.compact(range.start..mid)?,
            self.compact(mid..range.end)?,
        ]))
    }

    /// The whole-table plan of `range`: one attempt with the ordinary cuts,
    /// then each block of it that overflowed planned again on its own, or
    /// halved when it was the whole attempt.
    fn whole(&mut self, range: Range<usize>) -> Result<Plan> {
        let overflows = self.trial(range.clone(), Strategy::WholeTable)?;
        if let [only] = overflows.as_slice()
            && only.ops == range
        {
            if range.len() == 1 {
                return Err(overflows.into_iter().next().map_or_else(
                    || Error::internal("apply-changes overflow plan lost its refusal"),
                    |o| o.error,
                ));
            }
            let mid = range.start + range.len() / 2;
            return Ok(Plan::Seq(vec![
                self.whole(range.start..mid)?,
                self.whole(mid..range.end)?,
            ]));
        }
        let children = overflows
            .into_iter()
            .map(|o| self.whole(o.ops))
            .collect::<Result<Vec<_>>>()?;
        Ok(Plan::Attempt {
            ops: range,
            strategy: Strategy::WholeTable,
            children,
        })
    }
}

/// Write `ops[range]` as one attempt into `sink`: a fresh builder, the
/// rewrite's own write rules with survivors' blocks seeded by `strategy`.
/// Under the whole table the builder starts unseeded, so OSC elements
/// before the first survivor go into a block without the source table;
/// under a subset (marked over these operations' survivors) it starts
/// seeded when any survivor is among them, so OSC elements and survivors
/// share blocks. Stats and refresh counts are discarded: the first pass
/// counted these elements.
fn write_attempt<S: OpSink>(
    inputs: Inputs<'_>,
    ops: &[Op<'_>],
    range: Range<usize>,
    strategy: Strategy,
    scratch: &mut RefreshScratch,
    sink: &mut S,
    tx: &StreamTx<'_>,
) -> Result<()> {
    let ops = ops
        .get(range)
        .ok_or_else(|| Error::internal("apply-changes overflow plan outside its operations"))?;
    let mut used = match strategy {
        Strategy::WholeTable => Vec::new(),
        Strategy::Compact => vec![false; inputs.block.string_table_len()],
    };
    let mut bb = BlockBuilder::new();
    let seed = match strategy {
        Strategy::WholeTable => SourceSeed::Whole,
        Strategy::Compact => {
            let mut any_base = false;
            for op in ops {
                if let Op::Base(element) = op {
                    tx.check()?;
                    mark_used_slots(element, &mut used)?;
                    any_base = true;
                }
            }
            if any_base {
                tx.check()?;
                bb.pre_seed_subset_string_table(inputs.block, &used)?;
            }
            SourceSeed::Subset(&used)
        }
    };
    let mut stats = MergeStats::new();
    let mut base_writes = BaseWrites::new(inputs.block, inputs.probe, scratch);
    base_writes.seed = seed;
    let mut cx = OpContext {
        block: inputs.block,
        diff: inputs.diff,
        loc: inputs.loc,
        stats: &mut stats,
        base_writes: &mut base_writes,
    };
    for op in ops {
        tx.check()?;
        write_op(op, &mut bb, sink, &mut cx)?;
        sink.op_written();
    }
    flush_local(&mut bb, sink)
}

/// The second writing's sink: each block framed and sent as it completes,
/// and each recorded overflow replaced by its plan, written the same way.
pub(super) struct EmitSink<'e, 'a> {
    framed: FramedBlocks<'a>,
    next_op: usize,
    block_start: usize,
    inputs: Inputs<'a>,
    ops: &'e [Op<'a>],
    children: std::slice::Iter<'e, Plan>,
    next: Option<&'e Plan>,
    /// Scratch for the plans below this writing, one per nesting level.
    scratches: &'e mut [RefreshScratch],
    tx: &'e StreamTx<'e>,
    /// Framing time of the plans written in place of overflows.
    nested_ns: u64,
}

impl<'e, 'a> EmitSink<'e, 'a> {
    /// The sink of a writing numbering operations from `start` whose
    /// overflows the plans `children` replace, in order.
    pub(super) fn new(
        inputs: Inputs<'a>,
        ops: &'e [Op<'a>],
        children: &'e [Plan],
        scratches: &'e mut [RefreshScratch],
        tx: &'e StreamTx<'e>,
        start: usize,
    ) -> Self {
        let mut children = children.iter();
        let next = children.next();
        Self {
            framed: FramedBlocks::new(*inputs.compression, inputs.metrics),
            next_op: start,
            block_start: start,
            inputs,
            ops,
            children,
            next,
            scratches,
            tx,
            nested_ns: 0,
        }
    }

    /// Every plan was written; the framing time spent.
    pub(super) fn finish(self) -> Result<u64> {
        if self.next.is_some() {
            return Err(Error::internal(
                "apply-changes: a deferred blob's second writing did not reach a recorded overflow",
            ));
        }
        Ok(self.framed.frame_ns + self.nested_ns)
    }
}

impl OpSink for EmitSink<'_, '_> {
    #[inline]
    fn op_written(&mut self) {
        self.next_op += 1;
    }
}

impl BlockSink for EmitSink<'_, '_> {
    fn flush(&mut self, bb: &mut BlockBuilder) -> Result<()> {
        let ops = self.block_start..self.next_op;
        self.block_start = self.next_op;
        // The framing borrow ends here, so a plan below can frame.
        let overflow = self.framed.flush_or_overflow(bb)?;
        if overflow.is_some() {
            let child = self
                .next
                .take()
                .filter(|plan| plan.range() == ops)
                .ok_or_else(|| {
                    Error::internal(
                        "apply-changes: a deferred blob overflowed where its plan recorded no overflow",
                    )
                })?;
            self.next = self.children.next();
            return emit(
                child,
                self.inputs,
                self.ops,
                self.scratches,
                self.tx,
                &mut self.nested_ns,
            );
        }
        if !ops.is_empty()
            && let Some(plan) = self.next
            && ops.end > plan.range().start
        {
            return Err(Error::internal(
                "apply-changes: a deferred blob framed a block its plan recorded as an overflow",
            ));
        }
        for (_, frame) in self.framed.frames.drain(..) {
            self.tx.send(StreamMsg::Frame(frame))?;
        }
        Ok(())
    }
}

/// Write `plan`, sending its frames as they are framed.
fn emit(
    plan: &Plan,
    inputs: Inputs<'_>,
    ops: &[Op<'_>],
    scratches: &mut [RefreshScratch],
    tx: &StreamTx<'_>,
    frame_ns: &mut u64,
) -> Result<()> {
    match plan {
        Plan::Seq(parts) => {
            for part in parts {
                emit(part, inputs, ops, scratches, tx, frame_ns)?;
            }
            Ok(())
        }
        Plan::Attempt {
            ops: range,
            strategy,
            children,
        } => {
            let (mine, below) = scratches.split_first_mut().ok_or_else(|| {
                Error::internal("apply-changes: a deferred blob's plan outran its scratch")
            })?;
            let mut sink = EmitSink::new(inputs, ops, children, below, tx, range.start);
            write_attempt(inputs, ops, range.clone(), *strategy, mine, &mut sink, tx)?;
            *frame_ns += sink.finish()?;
            Ok(())
        }
    }
}

/// End a deferred blob's stream once its second writing has ended.
pub(super) fn end_stream(tx: &StreamTx<'_>) -> Result<()> {
    tx.end()
}

#[cfg(test)]
mod tests {
    use super::{RewriteSink, StreamMsg, StreamSlot, StreamTx};
    use crate::block_builder::BlockBuilder;
    use crate::commands::{BlockSink, FramedBlocks};
    use crate::write::metrics::WriterMetrics;
    use crate::writer::Compression;

    /// The first pass holds its frames up to the retention threshold and
    /// releases them past it, which defers the blob; a trial holds none.
    #[test]
    fn frames_past_the_retention_threshold_are_released() {
        let metrics = WriterMetrics::new();
        let mut sink = RewriteSink::new(FramedBlocks::new(Compression::None, &metrics), Some(1), 0);
        let mut bb = BlockBuilder::new();
        bb.add_node(1, 10, 20, [("name", "a")], None);
        sink.flush(&mut bb).expect("flush");
        assert!(sink.framed.frames.is_empty());
        assert!(sink.deferred());

        let mut held = RewriteSink::first_pass(FramedBlocks::new(Compression::None, &metrics));
        bb.add_node(1, 10, 20, [("name", "a")], None);
        held.flush(&mut bb).expect("flush");
        assert_eq!(held.framed.frames.len(), 1);
        assert!(!held.deferred());

        let mut trial = RewriteSink::trial(FramedBlocks::new(Compression::None, &metrics), 0);
        bb.add_node(1, 10, 20, [("name", "a")], None);
        trial.flush(&mut bb).expect("flush");
        assert!(trial.framed.frames.is_empty());
        assert!(!trial.deferred());
    }

    /// A stream ends with `End` and closes; a failed end send keeps the
    /// sender in the slot, for the failure publication to outlive.
    #[test]
    fn a_stream_closes_only_on_a_sent_end() {
        let cancel = crate::host::Call::detached();
        let (drain_tx, _drain_rx) = std::sync::mpsc::sync_channel(1);
        let drain_tx = std::sync::Arc::new(drain_tx);
        let failure = super::FailureSlot::new(super::super::descriptor::DrainWake::new(&drain_tx));
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let slot: StreamSlot = std::cell::RefCell::new(Some(tx));
        let stream = StreamTx {
            slot: &slot,
            failure: &failure,
            cancel: &cancel,
        };
        stream.end().expect("end");
        assert!(slot.borrow().is_none());
        assert!(matches!(rx.recv(), Ok(StreamMsg::End)));

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        drop(rx);
        *slot.borrow_mut() = Some(tx);
        assert!(stream.end().is_err());
        assert!(slot.borrow().is_some());
    }
}
