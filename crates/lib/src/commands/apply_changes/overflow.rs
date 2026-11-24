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
//! blocks, by one of two strategies.
//!
//! The compact one comes first: an attempt's survivors are written with
//! their string indices translated into a table holding only the source
//! strings that attempt's survivors use
//! (`BlockBuilder::pre_seed_subset_string_table`), with the OSC elements'
//! strings beside them. An attempt that overflows anywhere is dropped and
//! each half tried with its own strings, so a single element is tried with
//! exactly its own: a survivor that grew (refreshed `--locations-on-ways`
//! coordinates) beside a large string the diff removed, or survivors
//! alternating with large OSC values, no longer carry the dead string into
//! every frame.
//!
//! Where the compact strategy ends in a framing refusal (a single element)
//! or passes its budget, the whole-table one runs from scratch: survivors' blocks
//! carry the source table as in the first pass, the attempt starts
//! unseeded so OSC elements before the first survivor go into a block
//! without it, and a block that still overflows is halved. Compact output
//! is smaller for the same partition but its cuts and index widths differ
//! (strings are interned by content, so an index can land wider than its
//! source slot, and a smaller payload can compress to a larger body), so
//! it is not a superset of what the whole table recovers; the fallback
//! keeps every recovery that strategy finds.
//!
//! What this does not cure, and stays a loud refusal before publication: an
//! element that does not fit a block alone with only its own strings, and
//! an input whose recovery under both strategies needs more than
//! `APPLY_CHANGES_OVERFLOW_REPLAY_BYTES` of frames for one blob (as when its
//! changes carry that many bytes of large values between them, or one large
//! string is shared by many changed elements that must be cut apart: the
//! worker holds a blob's frames until its rewrite ends). The first is
//! refused with that element's own framing error; the second with
//! `ErrorKind::OverflowReplayBudget`, which names the allowance and carries
//! the overflowed block's framing refusal as its source. Halving finds a
//! fitting partition whenever every element fits alone; it can miss one
//! that exists only because string index widths shrink when two elements
//! share a table, which needs blocks within bytes of the limit.

use std::ops::Range;

use crate::ElementKind;
use crate::PrimitiveBlock;
use crate::block_builder::BlockBuilder;
use crate::commands::{BlockSink, FramedBlocks, flush_local, is_block_overflow};
use crate::error::Error;
use crate::osc::sorted::SortedDiff;
use crate::tunables::APPLY_CHANGES_OVERFLOW_REPLAY_BYTES;
use crate::writer::Compression;

use super::Result;
use super::element_writes::{BaseWrites, RefreshScratch, SourceSeed, mark_used_slots};
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
    /// A recovery attempt's byte allowance for the frames it holds (what the
    /// blob's budget has left), and the block refusal that started the
    /// recovery, carried by the budget error raised past it; `None` for a
    /// rewrite's first pass.
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
                return Err(crate::error::new_overflow_replay_budget(
                    APPLY_CHANGES_OVERFLOW_REPLAY_BYTES as u64,
                    refusal.clone(),
                ));
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
        overflows: &[Overflow],
        frames: Vec<Frame>,
    ) -> Result<(Vec<Frame>, u64)> {
        let end = overflows.last().map_or(0, |o| o.ops.end);
        // The steps are a pure function of the block, the diff and the
        // inline slice, so this walk writes what the first one wrote, in
        // the same order; only the writing is replayed (no walk, no stats).
        let ops: Vec<Op<'_>> = Steps::new(self.block, self.diff, self.kind, self.inline.clone())
            .filter_map(|step| match step {
                Ok(Step::Create(op) | Step::Element { op: Some(op), .. }) => Some(Ok(op)),
                Ok(Step::Observe(_) | Step::Element { op: None, .. }) => None,
                Err(e) => Some(Err(e)),
            })
            .take(end)
            .collect::<Result<_>>()?;
        if ops.len() < end {
            return Err(Error::internal(format!(
                "apply-changes overflow replay found {} operations where the rewrite wrote {end}",
                ops.len()
            )));
        }
        let mut frame_ns = 0;
        // The compact strategy first; where it ends in a framing refusal
        // (a single element) or its budget, the whole-table one, which
        // refuses only what this replay refused before the compact one
        // existed. Neither touches `frames` until one has succeeded, so a
        // failed strategy leaves nothing behind.
        let pieces = match self.recover(&ops, overflows, Strategy::Compact, &mut frame_ns) {
            Ok(pieces) => pieces,
            Err(e) if is_block_overflow(&e) || is_budget_refusal(&e) => {
                self.recover(&ops, overflows, Strategy::WholeTable, &mut frame_ns)?
            }
            Err(e) => return Err(e),
        };
        let mut out = Vec::with_capacity(frames.len() + pieces.iter().map(Vec::len).sum::<usize>());
        let mut kept = frames.into_iter();
        let mut taken = 0;
        for (overflow, piece) in overflows.iter().zip(pieces) {
            out.extend(kept.by_ref().take(overflow.at - taken));
            taken = overflow.at;
            out.extend(piece);
        }
        out.extend(kept);
        Ok((out, frame_ns))
    }

    /// Every overflowed block's replacement frames under `strategy`, in
    /// order, with a budget of its own.
    fn recover(
        &mut self,
        ops: &[Op<'_>],
        overflows: &[Overflow],
        strategy: Strategy,
        frame_ns: &mut u64,
    ) -> Result<Vec<Vec<Frame>>> {
        let mut budget = Budget {
            left: APPLY_CHANGES_OVERFLOW_REPLAY_BYTES,
            frame_ns: 0,
        };
        let mut pieces = Vec::with_capacity(overflows.len());
        let mut result = Ok(());
        for overflow in overflows {
            let ops = &ops[overflow.ops.clone()];
            let mut out = Vec::new();
            result = match strategy {
                Strategy::Compact => {
                    self.encode_compact(ops, &overflow.error, &mut budget, &mut out)
                }
                Strategy::WholeTable => {
                    self.encode_range(ops, &overflow.error, &mut budget, &mut out)
                }
            };
            if result.is_err() {
                break;
            }
            pieces.push(out);
        }
        *frame_ns += budget.frame_ns;
        result.map(|()| pieces)
    }

    /// The compact strategy for `ops`: one attempt whose survivors' blocks
    /// carry only the source strings `ops`' survivors use; kept whole if
    /// nothing in it overflowed, otherwise dropped and each half tried the
    /// same way with its own strings, so a single element is tried with
    /// exactly its own. A single element that still overflows is refused
    /// with its framing error.
    fn encode_compact(
        &mut self,
        ops: &[Op<'_>],
        original: &Error,
        budget: &mut Budget,
        out: &mut Vec<Frame>,
    ) -> Result<()> {
        let mut used = vec![false; self.block.string_table_len()];
        for op in ops {
            if let Op::Base(element) = op {
                mark_used_slots(element, &mut used)?;
            }
        }
        let seed = SourceSeed::Subset(&used);
        let (frames, overflows, held) = self.attempt(ops, original, budget, seed)?;
        if overflows.is_empty() {
            // Only kept frames are charged: a dropped attempt's are freed.
            budget.left = budget.left.saturating_sub(held);
            out.extend(frames);
            return Ok(());
        }
        drop(frames);
        if let [_] = ops {
            return Err(overflows
                .into_iter()
                .next()
                .map_or_else(|| original.clone(), |o| o.error));
        }
        let (left, right) = ops.split_at(ops.len() / 2);
        self.encode_compact(left, original, budget, out)?;
        self.encode_compact(right, original, budget, out)
    }

    /// The whole-table strategy for `ops`, appending their frames to `out`:
    /// one attempt with the ordinary cuts, then each block of it that
    /// overflowed again on its own, halved when it was the whole attempt.
    fn encode_range(
        &mut self,
        ops: &[Op<'_>],
        original: &Error,
        budget: &mut Budget,
        out: &mut Vec<Frame>,
    ) -> Result<()> {
        let (frames, overflows, held) = self.attempt(ops, original, budget, SourceSeed::Whole)?;
        // Charged now, before any recursion: these frames are held until
        // the recovery ends. The cap kept the charge within what was left.
        budget.left = budget.left.saturating_sub(held);
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

    /// One attempt at `ops`: a fresh builder, the rewrite's own write rules
    /// with survivors' blocks seeded by `seed`, and the frames, overflows
    /// and framed bytes that came of it (the caller charges the bytes it
    /// keeps). Under the whole table the builder starts unseeded, so OSC
    /// elements before the first survivor go into a block without the
    /// source table; under a subset it starts seeded when any survivor is
    /// among `ops`, so OSC elements and survivors share blocks. Stats and
    /// refresh counts are discarded: the first pass counted these elements.
    fn attempt(
        &mut self,
        ops: &[Op<'_>],
        original: &Error,
        budget: &mut Budget,
        seed: SourceSeed<'_>,
    ) -> Result<(Vec<Frame>, Vec<Overflow>, usize)> {
        let mut bb = BlockBuilder::new();
        if let SourceSeed::Subset(used) = seed
            && ops.iter().any(|op| matches!(op, Op::Base(_)))
        {
            bb.pre_seed_subset_string_table(self.block, used)?;
        }
        let mut sink = RewriteSink::new(FramedBlocks::new(*self.compression, self.metrics));
        // The attempt holds its frames until it returns, so the budget
        // stops it as it goes, not only once they are charged.
        sink.cap = Some((budget.left, original.clone()));
        let mut stats = MergeStats::new();
        let mut base_writes = BaseWrites::new(self.block, self.probe, self.scratch);
        base_writes.seed = seed;
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
        budget.frame_ns += sink.framed.frame_ns;
        Ok((sink.framed.frames, sink.overflows, sink.held))
    }
}

/// Whether `error` is a strategy passing its replay budget
/// ([`ErrorKind::OverflowReplayBudget`](crate::ErrorKind::OverflowReplayBudget)).
fn is_budget_refusal(error: &Error) -> bool {
    matches!(error.kind(), crate::ErrorKind::OverflowReplayBudget { .. })
}

/// How a replay writes an overflowed block's elements again.
#[derive(Clone, Copy)]
enum Strategy {
    /// Blocks of survivors carry only the source strings their slice uses
    /// ([`Replay::encode_compact`]).
    Compact,
    /// Blocks of survivors carry the source block's whole table, as the
    /// first pass's do ([`Replay::encode_range`]).
    WholeTable,
}

/// The bytes of frames a blob's recovery may still produce, and the
/// framing time it has spent.
struct Budget {
    left: usize,
    frame_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::{RewriteSink, is_budget_refusal};
    use crate::block_builder::BlockBuilder;
    use crate::commands::{BlockSink, FramedBlocks, is_block_overflow};
    use crate::write::metrics::WriterMetrics;
    use crate::writer::Compression;
    use crate::{ErrorKind, OversizePart};

    /// A recovery attempt past what its blob's budget has left is refused
    /// with the budget's own error, naming the whole allowance and carrying
    /// the block refusal that started the recovery as its source; it is not
    /// a block overflow, so no sink records it as one.
    #[test]
    fn an_attempt_past_its_allowance_is_refused_with_the_budget_error() {
        let trigger = crate::error::new_blob_too_large(OversizePart::Payload, 40, 32);
        let metrics = WriterMetrics::new();
        let mut sink = RewriteSink::new(FramedBlocks::new(Compression::None, &metrics));
        sink.cap = Some((1, trigger));
        let mut bb = BlockBuilder::new();
        bb.add_node(1, 10, 20, [("name", "a")], None);
        let Err(err) = sink.flush(&mut bb) else {
            panic!("a frame past a one-byte allowance must be refused");
        };
        assert!(is_budget_refusal(&err), "{err}");
        assert!(!is_block_overflow(&err), "{err}");
        let ErrorKind::OverflowReplayBudget { limit, trigger } = err.kind() else {
            panic!("expected the budget error, got {err}");
        };
        assert_eq!(
            *limit,
            super::APPLY_CHANGES_OVERFLOW_REPLAY_BYTES as u64,
            "the whole allowance, not what was left of it"
        );
        assert!(matches!(
            trigger.kind(),
            ErrorKind::BlobTooLarge {
                part: OversizePart::Payload,
                ..
            }
        ));
        let source = std::error::Error::source(&err)
            .and_then(|s| s.downcast_ref::<crate::Error>())
            .map(crate::Error::kind);
        assert!(matches!(source, Some(ErrorKind::BlobTooLarge { .. })));
        let report = err.report();
        assert!(report.contains("allowance"), "{report}");
        assert!(report.contains("must be at most 32"), "{report}");
    }
}
