//! Per-block rewrite loop: walks a decoded `PrimitiveBlock`, applies the OSC
//! diff overlay element-by-element, interleaves inline upsert creates at
//! their sorted positions, and frames each output block on the worker as it
//! completes.
//!
//! What the rewrite writes, and in which order, is one iterator ([`Steps`]);
//! how it is cut into blocks is the consumer's. The split exists so the
//! output of a block that overflowed the PBF blob limit can be written again
//! in smaller blocks (`overflow.rs`): the steps are a pure function of the
//! block, the diff and the inline slice, so a second walk yields the same
//! operations in the same order.

use std::ops::Range;

use crate::ElementKind;
use crate::blob_meta::BlobIndex;
use crate::block_builder::BlockBuilder;
use crate::osc::sorted::SortedDiff;
use crate::writer::Compression;
use crate::{BlockElementsIter, Element, PrimitiveBlock};

use crate::commands::{
    BlockSink, FramedBlocks, ensure_node_capacity_local, ensure_relation_capacity_local,
    flush_local,
};

use super::Result;
use super::classify::{DiffHit, KindSeeks, blob_walk, element_kind_id};
use super::element_writes::{
    BaseWrites, RefreshScratch, RefreshTally, add_osc_node, add_osc_relation,
    write_base_dense_node_local, write_base_node_local, write_base_relation_local,
    write_base_way_local, write_base_way_local_with_locations, write_osc_way_local,
};
use super::loc_mode::LocMode;
use super::overflow::{
    self, EmitSink, Inputs, OpSink, RewriteSink, StreamTx, collect_ops, end_stream, plan,
};
use super::refresh::StaleProbe;
use super::stats::MergeStats;

/// Output from `rewrite_block_parallel`, from a rewrite whose walk has
/// ended.
pub(super) struct RewriteOutput {
    /// Each output block's frame, with its kind, in output order; `None`
    /// once they have been streamed, the item already sent.
    pub(super) frames: Option<Vec<(ElementKind, Vec<u8>)>>,
    /// Wall time spent framing those blocks, excluding their encoding.
    pub(super) frame_ns: u64,
    /// The blob's local stats, for a held item (a streamed item carried
    /// them).
    pub(super) stats: MergeStats,
    /// Inline-coordinate refresh counts for the block's surviving base ways
    /// (all zero without `--locations-on-ways`).
    pub(super) refresh: RefreshTally,
}

/// One element the rewrite writes, in output order.
#[derive(Clone)]
pub(super) enum Op<'a> {
    /// The diff's record `upserts(kind)[i]`: a create, or the replacement
    /// of a base element.
    Osc(ElementKind, usize),
    /// A base element the diff leaves alone.
    Base(Element<'a>),
}

/// One step of the rewrite's walk.
pub(super) enum Step<'a> {
    /// A base element was reached that inline creates sort before: its ID
    /// for the blob walk, observed before those creates are written. The
    /// element itself follows as a [`Step::Element`] whose ID is already
    /// observed.
    Observe(i64),
    /// An inline create.
    Create(Op<'a>),
    /// A base element and what the diff does to it: the op to write, or
    /// `None` where the diff deletes it. Observed here unless a
    /// [`Step::Observe`] of it came first.
    Element { id: i64, op: Option<Op<'a>> },
}

/// The rewrite's steps over one base block: for each base element, the
/// inline creates sorting before it (behind a [`Step::Observe`] of it when
/// there are any), then the element with what the diff does to it; then the
/// trailing creates. An element with no create before it is one step, moved
/// once: this is the rewrite's per-element hot path, and parking every
/// element behind its own observe step measured 3 % slower on planet
/// (`reference/performance/apply-changes.md`, "Overflow recovery").
///
/// The `osm_id_cmp` here is coherent with the `osm_id_key` bounds used to
/// slice `inline` in `streaming::upsert_slice` ONLY if the base block
/// iterates in canonical OSM order. The header's `is_sorted()` precondition
/// (`rewrite.rs::build_header_bytes`) claims it and the blob walk refuses
/// the block where it is false; any future caller that bypasses both must
/// re-establish the invariant or the cursor may skip past upserts and drop
/// creates.
pub(super) struct Steps<'a> {
    elements: BlockElementsIter<'a>,
    diff: &'a SortedDiff,
    kind: ElementKind,
    /// The inline upserts not yet passed, as indexes into `upserts(kind)`.
    inline: Range<usize>,
    // Creates come from `inline` alone; what the diff does to each base
    // element is found by its own per-kind cursors, so a base element the
    // upsert cursor has already passed (an ID the block holds twice) is
    // still replaced.
    seeks: KindSeeks,
    /// A base element observed and waiting behind its inline creates.
    pending: Option<(ElementKind, i64, Element<'a>)>,
}

impl<'a> Steps<'a> {
    pub(super) fn new(
        block: &'a PrimitiveBlock,
        diff: &'a SortedDiff,
        kind: ElementKind,
        inline: Range<usize>,
    ) -> Self {
        Self {
            elements: block.elements(),
            diff,
            kind,
            inline,
            seeks: KindSeeks::default(),
            pending: None,
        }
    }

    /// Whether an inline create sorts before base element `id`.
    #[inline]
    fn create_due(&self, id: i64) -> bool {
        !self.inline.is_empty()
            && self
                .diff
                .upserts(self.kind)
                .get(self.inline.start)
                .is_some_and(|&next| crate::osm_id::osm_id_cmp(next, id).is_lt())
    }

    #[inline]
    fn next_create(&mut self) -> Option<Result<Step<'a>>> {
        let i = self.inline.next()?;
        Some(Ok(Step::Create(Op::Osc(self.kind, i))))
    }

    /// The element's step, once no create sorts before it. Refused where
    /// the diff would replace or delete a base element holding a higher
    /// version than the changes claimed for it
    /// ([`SortedDiff::check_base_version`]).
    #[inline]
    fn resolve(
        &mut self,
        elem_kind: ElementKind,
        id: i64,
        element: Element<'a>,
    ) -> Result<Step<'a>> {
        // Skip a modification ID (written below as the element's own hit).
        if !self.inline.is_empty()
            && self.diff.upserts(self.kind).get(self.inline.start) == Some(&id)
        {
            self.inline.next();
        }
        let op = match self.seeks.find(self.diff, elem_kind, id) {
            DiffHit::Deleted(i) => {
                self.diff
                    .check_base_version(elem_kind, id, Err(i), base_claim(&element))?;
                None
            }
            DiffHit::Upserted(i) => {
                self.diff
                    .check_base_version(elem_kind, id, Ok(i), base_claim(&element))?;
                Some(Op::Osc(elem_kind, i))
            }
            DiffHit::None => Some(Op::Base(element)),
        };
        Ok(Step::Element { id, op })
    }
}

/// The version a base element claims (`claimed_version`: 0 and the
/// Osmosis -1 sentinel are none).
#[inline]
fn base_claim(element: &Element<'_>) -> Option<i32> {
    match element {
        Element::DenseNode(dn) => dn.info().and_then(crate::DenseNodeInfo::claimed_version),
        Element::Node(n) => n.info().claimed_version(),
        Element::Way(w) => w.info().claimed_version(),
        Element::Relation(r) => r.info().claimed_version(),
    }
}

impl<'a> Iterator for Steps<'a> {
    type Item = Result<Step<'a>>;

    #[inline]
    fn next(&mut self) -> Option<Result<Step<'a>>> {
        if let Some((elem_kind, id, element)) = self.pending.take() {
            if self.create_due(id) {
                self.pending = Some((elem_kind, id, element));
                return self.next_create();
            }
            return Some(self.resolve(elem_kind, id, element));
        }
        let Some(element) = self.elements.next() else {
            return self.next_create();
        };
        let (elem_kind, id) = element_kind_id(&element);
        if self.create_due(id) {
            self.pending = Some((elem_kind, id, element));
            return Some(Ok(Step::Observe(id)));
        }
        Some(self.resolve(elem_kind, id, element))
    }
}

// The create dispatch mirrors stream_output, but capacity checks below must
// flush into this worker's local blocks before stats advance. The shared
// add_osc_* helpers own only encoding, not ordering or output submission.
/// Write the diff's record `upserts(kind)[i]` into the local BlockBuilder:
/// a create, or the replacement of a base element.
fn write_osc_local<O: BlockSink + ?Sized>(
    i: usize,
    kind: ElementKind,
    diff: &SortedDiff,
    bb: &mut BlockBuilder,
    output: &mut O,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<()> {
    // A block whose content is already past the blob limit cannot be
    // framed whatever comes next: end it here, so the overflow is found
    // (and recovered) at one limit's worth, not after every remaining OSC
    // element has been copied in.
    if bb.content_over_payload_limit() {
        flush_local(bb, output)?;
    }
    match kind {
        ElementKind::Node => {
            ensure_node_capacity_local(bb, output)?;
            add_osc_node(bb, &diff.node(i));
            stats.diff_nodes += 1;
        }
        ElementKind::Way => {
            write_osc_way_local(bb, output, &diff.way(i), loc)?;
            stats.diff_ways += 1;
        }
        ElementKind::Relation => {
            ensure_relation_capacity_local(bb, output)?;
            add_osc_relation(bb, &diff.relation(i));
            stats.diff_relations += 1;
        }
    }
    Ok(())
}

/// What writing one [`Op`] needs besides the builder and the sink.
pub(super) struct OpContext<'c, 'a> {
    pub(super) block: &'a PrimitiveBlock,
    pub(super) diff: &'a SortedDiff,
    pub(super) loc: LocMode<'a>,
    pub(super) stats: &'c mut MergeStats,
    pub(super) base_writes: &'c mut BaseWrites<'a>,
}

/// Write one op into `bb`, flushing into `output` as capacity and the
/// source string table require.
pub(super) fn write_op<O: BlockSink + ?Sized>(
    op: &Op<'_>,
    bb: &mut BlockBuilder,
    output: &mut O,
    cx: &mut OpContext<'_, '_>,
) -> Result<()> {
    let block = cx.block;
    match op {
        Op::Osc(kind, i) => write_osc_local(*i, *kind, cx.diff, bb, output, cx.stats, cx.loc),
        Op::Base(Element::DenseNode(dn)) => {
            write_base_dense_node_local(bb, output, dn, block, cx.base_writes.seed)?;
            cx.stats.base_nodes += 1;
            Ok(())
        }
        Op::Base(Element::Node(n)) => {
            write_base_node_local(bb, output, n, block, cx.base_writes.seed)?;
            cx.stats.base_nodes += 1;
            Ok(())
        }
        Op::Base(Element::Way(w)) => {
            if cx.loc.for_way(w.id())?.is_some() {
                write_base_way_local_with_locations(bb, output, w, block, cx.base_writes)?;
            } else {
                write_base_way_local(bb, output, w, block, cx.base_writes)?;
            }
            cx.stats.base_ways += 1;
            Ok(())
        }
        Op::Base(Element::Relation(r)) => {
            write_base_relation_local(bb, output, r, block, cx.base_writes)?;
            cx.stats.base_relations += 1;
            Ok(())
        }
    }
}

/// Rewrite one decoded base block on a worker, framing each completed output
/// block there rather than handing it to a `PbfWriter`. Interleaves
/// upserts at their sorted positions within the block - IDs that match base
/// elements are modifications (handled by normal element processing); IDs that
/// don't match are creates (emitted by the cursor).
///
/// `loc` governs every way written, base or OSC: see [`LocMode`]. Writing
/// a way under `LocMode::Pending` is an error, never a silent refs-only
/// write. Under `LocMode::Required` a surviving base way's inline
/// coordinates are refreshed against `probe` (see `refresh.rs`); `probe` is
/// empty without `--locations-on-ways`.
///
/// Every base element is observed by a walk of this rewrite's own, built
/// from the blob's indexdata `claim` (`None` for an unindexed blob), so the
/// block's order and claim are checked over all of it; the walk is ended
/// here, after the last block is framed, so a framing error outranks a
/// refusal only the walk's end can make. Frames come back only in an `Ok`
/// result, so nothing of a rewrite that its walk refuses can leave the
/// worker.
///
/// Each block is framed on the worker from the builder's own buffer the
/// moment it completes (`FramedBlocks`), so a blob's raw output is never
/// resident all at once and no encode buffer changes hands: a planet input
/// blob holds some 200k elements and becomes ~27 output blocks, and
/// accumulating them all before framing held each worker's whole raw output
/// beside its framed copy.
///
/// An output block is the source block's whole string table (survivors keep
/// their string indices) plus what the OSC elements add, so it can outgrow
/// the PBF blob limit although the source block and every changed element
/// fit: a 17 MiB tag value modified to another 17 MiB value. Such a block's
/// framing refusal is recorded, not returned, and so is the moment the
/// held frames pass `APPLY_CHANGES_REWRITE_RETAIN_BYTES`. Either defers the
/// blob (`overflow.rs`): once the walk has ended each overflow is planned
/// into smaller blocks that carry only the strings they use, the walk's own
/// refusal is returned, and then `open` sends the blob's item with a frame
/// stream and the blob is written again, each frame sent as it is framed.
/// A plan fails the rewrite, with a single element's framing error, where
/// that element overflows alone with its own strings in every form tried
/// (a bare node is retried plain), or where halving finds no fitting cut
/// although one exists; see the `overflow.rs` module doc for what the plan
/// does not search. A recorded overflow outranks
/// any later error of the walk, as it would have had it been returned at
/// its flush. Nothing of a blob refused before `open` leaves the worker; a
/// failure while streaming leaves part of the blob in the output, which is
/// then not published.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn rewrite_block_parallel(
    block: &PrimitiveBlock,
    diff: &SortedDiff,
    bb: &mut BlockBuilder,
    inline_upserts: Range<usize>,
    kind: ElementKind,
    loc: LocMode<'_>,
    probe: &StaleProbe,
    scratch: &mut RefreshScratch,
    claim: Option<&BlobIndex>,
    compression: &Compression,
    metrics: &crate::write::metrics::WriterMetrics,
    stream: &StreamTx<'_>,
    open: impl FnOnce(MergeStats) -> Result<()>,
) -> Result<RewriteOutput> {
    let mut sink = RewriteSink::first_pass(FramedBlocks::new(*compression, metrics));
    let mut stats = MergeStats::new();
    let mut base_writes = BaseWrites::new(block, probe, scratch);
    let walked = {
        let mut cx = OpContext {
            block,
            diff,
            loc,
            stats: &mut stats,
            base_writes: &mut base_writes,
        };
        walk_and_write(
            block,
            diff,
            bb,
            inline_upserts.clone(),
            kind,
            claim,
            &mut sink,
            &mut cx,
        )
    };
    // The last use of `base_writes`, which releases `scratch` for a replay.
    let refresh = base_writes.tally;
    if !sink.deferred() {
        walked?;
        return Ok(RewriteOutput {
            frames: Some(sink.framed.frames),
            frame_ns: sink.framed.frame_ns,
            stats,
            refresh,
        });
    }
    let RewriteSink {
        framed, overflows, ..
    } = sink;
    let mut frame_ns = framed.frame_ns;
    drop(framed.frames);
    let inputs = Inputs {
        block,
        diff,
        kind,
        loc,
        probe,
        compression,
        metrics,
    };
    let end = overflows.last().map_or(0, |o| o.ops().end);
    let ops = collect_ops(inputs, inline_upserts.clone(), end, stream)?;
    let plans = plan(inputs, &ops, &overflows, scratch, stream)?;
    walked?;
    open(stats)?;
    // The second writing: the walk again (fresh steps, claim walk and
    // seeded builder, so it writes what the first wrote), each block sent
    // as it is framed and each overflow replaced by its plan.
    let depth = plans.iter().map(overflow::Plan::depth).max().unwrap_or(0);
    let mut scratches: Vec<RefreshScratch> = std::iter::repeat_with(RefreshScratch::default)
        .take(depth)
        .collect();
    let mut emit = EmitSink::new(inputs, &ops, &plans, &mut scratches, stream, 0);
    let mut again = MergeStats::new();
    let mut base_writes = BaseWrites::new(block, probe, scratch);
    let mut cx = OpContext {
        block,
        diff,
        loc,
        stats: &mut again,
        base_writes: &mut base_writes,
    };
    walk_and_write(
        block,
        diff,
        bb,
        inline_upserts,
        kind,
        claim,
        &mut emit,
        &mut cx,
    )?;
    frame_ns += emit.finish()?;
    end_stream(stream)?;
    Ok(RewriteOutput {
        frames: None,
        frame_ns,
        stats: MergeStats::new(),
        refresh,
    })
}

/// The rewrite's first pass: every step written into `bb`, blocks flushed
/// into `sink` (which records an overflowing block instead of failing), the
/// last block flushed, then the walk ended.
#[allow(clippy::too_many_arguments)]
fn walk_and_write<S: OpSink>(
    block: &PrimitiveBlock,
    diff: &SortedDiff,
    bb: &mut BlockBuilder,
    inline_upserts: Range<usize>,
    kind: ElementKind,
    claim: Option<&BlobIndex>,
    sink: &mut S,
    cx: &mut OpContext<'_, '_>,
) -> Result<()> {
    let mut walk = blob_walk(claim);
    bb.pre_seed_string_table(block)?;
    let mut observed = false;
    for step in Steps::new(block, diff, kind, inline_upserts) {
        match step? {
            Step::Observe(id) => {
                walk.observe(id)?;
                observed = true;
            }
            Step::Create(op) => {
                write_op(&op, bb, sink, cx)?;
                sink.op_written();
            }
            Step::Element { id, op } => {
                if !std::mem::take(&mut observed) {
                    walk.observe(id)?;
                }
                match op {
                    Some(op) => {
                        write_op(&op, bb, sink, cx)?;
                        sink.op_written();
                    }
                    None => cx.stats.deleted += 1,
                }
            }
        }
    }
    // Flush remaining elements in the BlockBuilder, then end the walk.
    flush_local(bb, sink)?;
    walk.finish()?;
    Ok(())
}
