//! Recovery from an output block that overflowed the PBF data blob limits.
//!
//! An apply-changes output block can outgrow the limit although everything
//! that went into it fits on its own: a rewritten block carries its source
//! block's whole string table, because surviving base elements keep their
//! string indices, plus the strings of every OSC element written into it,
//! and blocks are cut by element count, not by bytes. A 17 MiB tag value
//! modified to another 17 MiB value makes a 34 MiB block from a source block
//! and an OSC element that each fit.
//!
//! Nothing is sized up front, so normal input pays nothing: an overflowing
//! block's framing refusal is recorded with the range of operations it held
//! ([`RewriteSink`]), and [`Replay`] writes that range again into fresh
//! blocks. Its first attempt cuts where the ordinary rules cut, with one
//! difference that is the whole point: the attempt starts unseeded, so OSC
//! elements before the first survivor go into a block without the source
//! table. A range that still overflows as one block is halved, and a single
//! element that overflows alone is refused with its framing error.
//!
//! What this does not cure, and stays a loud refusal before publication: an
//! element that does not fit a block alone in its output form (a surviving
//! element always carries its source block's whole table, so one whose
//! refreshed coordinates grew it, beside a table near the limit, can fail
//! where it alone with only its own strings would fit), and an input whose
//! recovery would produce more than `APPLY_CHANGES_OVERFLOW_REPLAY_BYTES` of
//! frames for one blob. Halving finds a fitting partition whenever every
//! element fits alone; it can miss one that exists only because string
//! index widths shrink when two elements share a table, which needs blocks
//! within bytes of the limit.

use std::ops::Range;

use crate::ElementKind;
use crate::PrimitiveBlock;
use crate::block_builder::BlockBuilder;
use crate::commands::{BlockSink, FramedBlocks, flush_local};
use crate::error::Error;
use crate::osc::sorted::SortedDiff;
use crate::tunables::APPLY_CHANGES_OVERFLOW_REPLAY_BYTES;
use crate::writer::Compression;

use super::Result;
use super::element_writes::{BaseWrites, RefreshScratch};
use super::loc_mode::LocMode;
use super::refresh::StaleProbe;
use super::rewrite_block::{Op, OpContext, Step, Steps, write_op};
use super::stats::MergeStats;

type Frame = (ElementKind, Vec<u8>);

/// An output block that overflowed: the operations it held and where its
/// frame would have gone.
pub(super) struct Overflow {
    /// Its operations, numbered in write order.
    ops: Range<usize>,
    /// The number of frames before it.
    at: usize,
    /// Its framing refusal.
    error: Error,
}

/// A rewrite's block sink: frames like [`FramedBlocks`], and records a block
/// that overflows the blob limits, with the operations it held, rather than
/// failing. The writer numbers operations by calling
/// [`op_written`](Self::op_written) after each; every flush happens before
/// the next operation is added (capacity, re-seeding and the final flush
/// alike), so a flushed block holds exactly the operations since the last.
pub(super) struct RewriteSink<'a> {
    pub(super) framed: FramedBlocks<'a>,
    next_op: usize,
    block_start: usize,
    pub(super) overflows: Vec<Overflow>,
    /// A recovery attempt's byte allowance for the frames it holds, and the
    /// error refusing past it; `None` for a rewrite's first pass.
    cap: Option<(usize, Error)>,
    held: usize,
}

impl<'a> RewriteSink<'a> {
    pub(super) fn new(framed: FramedBlocks<'a>) -> Self {
        Self {
            framed,
            next_op: 0,
            block_start: 0,
            overflows: Vec::new(),
            cap: None,
            held: 0,
        }
    }

    /// Count one operation written into the block in progress.
    #[inline]
    pub(super) fn op_written(&mut self) {
        self.next_op += 1;
    }
}

impl BlockSink for RewriteSink<'_> {
    fn flush(&mut self, bb: &mut BlockBuilder) -> Result<()> {
        let ops = self.block_start..self.next_op;
        let before = self.framed.frames.len();
        if let Some(error) = self.framed.flush_or_overflow(bb)? {
            self.overflows.push(Overflow {
                ops,
                at: self.framed.frames.len(),
                error,
            });
        }
        self.block_start = self.next_op;
        // Every frame is charged: a block carrying the source table repeats
        // it, and an OSC-only block repeats whatever strings its elements
        // share (one interned role on thousands of relations).
        if let Some((cap, refusal)) = &self.cap {
            for frame in &self.framed.frames[before..] {
                self.held = self.held.saturating_add(frame.1.len());
            }
            if self.held > *cap {
                return Err(refusal.clone());
            }
        }
        Ok(())
    }
}

/// What writing a rewrite's operations again needs: the same inputs the
/// rewrite had.
pub(super) struct Replay<'a, 's> {
    pub(super) block: &'a PrimitiveBlock,
    pub(super) diff: &'a SortedDiff,
    pub(super) kind: ElementKind,
    pub(super) inline: Range<usize>,
    pub(super) loc: LocMode<'a>,
    pub(super) probe: &'a StaleProbe,
    pub(super) scratch: &'s mut RefreshScratch,
    pub(super) compression: &'a Compression,
    pub(super) metrics: &'a crate::write::metrics::WriterMetrics,
}

impl Replay<'_, '_> {
    /// Re-encode every block in `overflows` (in write order) and splice its
    /// frames into `frames` where the block's frame would have gone.
    /// Returns the frames and the framing time the recovery spent.
    pub(super) fn run(
        mut self,
        overflows: Vec<Overflow>,
        frames: Vec<Frame>,
    ) -> Result<(Vec<Frame>, u64)> {
        let end = overflows.last().map_or(0, |o| o.ops.end);
        // The steps are a pure function of the block, the diff and the
        // inline slice, so this walk writes what the first one wrote, in
        // the same order; only the writing is replayed (no walk, no stats).
        let ops: Vec<Op<'_>> = Steps::new(self.block, self.diff, self.kind, self.inline.clone())
            .filter_map(|step| match step {
                Step::Create(op) | Step::Element { op: Some(op), .. } => Some(op),
                Step::Observe(_) | Step::Element { op: None, .. } => None,
            })
            .take(end)
            .collect();
        if ops.len() < end {
            return Err(Error::internal(format!(
                "apply-changes overflow replay found {} operations where the rewrite wrote {end}",
                ops.len()
            )));
        }
        let mut out = Vec::with_capacity(frames.len() + overflows.len());
        let mut budget = Budget {
            left: APPLY_CHANGES_OVERFLOW_REPLAY_BYTES,
            frame_ns: 0,
        };
        let mut kept = frames.into_iter();
        let mut taken = 0;
        for overflow in overflows {
            out.extend(kept.by_ref().take(overflow.at - taken));
            taken = overflow.at;
            self.encode_range(&ops[overflow.ops], &overflow.error, &mut budget, &mut out)?;
        }
        out.extend(kept);
        Ok((out, budget.frame_ns))
    }

    /// Write `ops` into fresh blocks, appending their frames to `out`: one
    /// attempt with the ordinary cuts, then each block of it that overflowed
    /// again on its own, halved when it was the whole attempt.
    fn encode_range(
        &mut self,
        ops: &[Op<'_>],
        original: &Error,
        budget: &mut Budget,
        out: &mut Vec<Frame>,
    ) -> Result<()> {
        let (frames, overflows) = self.attempt(ops, original, budget)?;
        let mut kept = frames.into_iter();
        let mut taken = 0;
        for overflow in overflows {
            out.extend(kept.by_ref().take(overflow.at - taken));
            taken = overflow.at;
            let sub = &ops[overflow.ops];
            if sub.len() < ops.len() {
                self.encode_range(sub, original, budget, out)?;
            } else if sub.len() == 1 {
                return Err(overflow.error);
            } else {
                let (left, right) = sub.split_at(sub.len() / 2);
                self.encode_range(left, original, budget, out)?;
                self.encode_range(right, original, budget, out)?;
            }
        }
        out.extend(kept);
        Ok(())
    }

    /// One attempt at `ops`: a fresh, unseeded builder, the rewrite's own
    /// write rules, and the frames and overflows that came of it. Stats and
    /// refresh counts are discarded: the first pass counted these elements.
    fn attempt(
        &mut self,
        ops: &[Op<'_>],
        original: &Error,
        budget: &mut Budget,
    ) -> Result<(Vec<Frame>, Vec<Overflow>)> {
        let mut bb = BlockBuilder::new();
        let mut sink = RewriteSink::new(FramedBlocks::new(*self.compression, self.metrics));
        // The attempt holds its frames until it returns, so the budget
        // stops it as it goes, not only once they are charged.
        sink.cap = Some((budget.left, original.clone()));
        let mut stats = MergeStats::new();
        let mut base_writes = BaseWrites::new(self.block, self.probe, self.scratch);
        let mut cx = OpContext {
            block: self.block,
            diff: self.diff,
            loc: self.loc,
            stats: &mut stats,
            base_writes: &mut base_writes,
        };
        for op in ops {
            write_op(op, &mut bb, &mut sink, &mut cx)?;
            sink.op_written();
        }
        flush_local(&mut bb, &mut sink)?;
        // Charged now, before any recursion: these frames are held until
        // the recovery ends. The cap kept the charge within what was left.
        budget.left = budget.left.saturating_sub(sink.held);
        budget.frame_ns += sink.framed.frame_ns;
        Ok((sink.framed.frames, sink.overflows))
    }
}

/// The bytes of frames a blob's recovery may still produce, and the
/// framing time it has spent.
struct Budget {
    left: usize,
    frame_ns: u64,
}
