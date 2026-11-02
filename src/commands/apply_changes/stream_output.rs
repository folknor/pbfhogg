//! Output helpers for gap creates, trailing creates at type transitions,
//! and passthrough coalescing. The drain calls these while processing items
//! in file order.
//!
//! ## Create runs
//!
//! Every create the drain writes (gap creates before a blob, the creates of
//! a kind left over at a type transition, the trailing creates at end of
//! input) leaves as part of a run: a slice of the sorted upsert vector the
//! drain has chosen, written as consecutive blocks of `DEFAULT_ELEMENT_CAP`
//! elements and a shorter last one. [`CreateEncoder`] decides where the
//! blocks are cut (every upsert id has its overlay record, so each create is
//! one element and the cuts are those a single builder filling to its cap
//! would make), encodes and frames each block the way the workers frame
//! theirs (`frame_blob_pipelined`), and writes the frames raw in order.
//!
//! A run of one block is encoded on the drain thread. A longer one is
//! encoded on the invocation's executor, `APPLY_CHANGES_CREATE_BATCH_BLOCKS`
//! blocks per call, except under an explicit `-j 1`, which runs
//! sequentially and encodes every block on the drain thread. At planet the OSC's node creates past the last base
//! node blob are about 300 blocks, written at the node-to-way transition:
//! encoded serially that kept the drain busy for a third of a second while
//! the workers kept producing way output behind it, a backlog of framed
//! output the allocator then kept as free memory in the workers' arenas for
//! the rest of the run (reference/performance.md, "apply-changes' memory
//! step at the node-to-way transition").

use std::ops::Range;

use rayon::prelude::*;

use crate::ElementKind;
use crate::block_builder::{BlockBuilder, DEFAULT_ELEMENT_CAP};
use crate::file_writer::FileWriter;
use crate::osc::sorted::SortedDiff;
use crate::tunables::APPLY_CHANGES_CREATE_BATCH_BLOCKS;
use crate::writer::{Compression, PbfWriter, frame_blob_pipelined};

use super::Result;
use super::element_writes::{add_osc_node, add_osc_relation, add_osc_way};
use super::loc_mode::LocMode;
use super::stats::MergeStats;
use super::upsert_cursors::UpsertCursors;

/// Encodes, frames and writes the drain's create runs; see the module doc.
pub(super) struct CreateEncoder<'a> {
    /// Where runs longer than one block are encoded; `None` encodes every
    /// block on the drain thread (an explicit `-j 1`, which runs sequentially).
    executor: Option<&'a crate::host::Executor>,
    compression: Compression,
    /// The builder for blocks encoded on the drain thread. It builds nothing
    /// but creates and is empty at every block's start: each block is taken
    /// before the next is begun.
    bb: BlockBuilder,
}

impl<'a> CreateEncoder<'a> {
    pub(super) fn new(
        executor: Option<&'a crate::host::Executor>,
        compression: Compression,
    ) -> Self {
        Self {
            executor,
            compression,
            bb: BlockBuilder::new(),
        }
    }

    /// Write the creates `run` of `kind` (an index range into the kind's
    /// sorted upserts), in order, as blocks of `DEFAULT_ELEMENT_CAP`.
    /// Creates counted into `stats` per kind. The first failure in output
    /// order is returned, after every block before it has been written;
    /// nothing after it is.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit(
        &mut self,
        kind: ElementKind,
        run: Range<usize>,
        diff: &SortedDiff,
        loc: LocMode<'_>,
        writer: &mut PbfWriter<FileWriter>,
        stats: &mut MergeStats,
    ) -> Result<()> {
        if run.is_empty() {
            return Ok(());
        }
        let cap = DEFAULT_ELEMENT_CAP.get();
        let invocation = writer.cancel_token().permit();
        let metrics = &invocation.metrics().writer;
        let compression = self.compression;
        let mut added = 0u64;
        match self.executor {
            Some(executor) if run.len() > cap => {
                for batch in chunks(run, cap * APPLY_CHANGES_CREATE_BATCH_BLOCKS) {
                    invocation.check()?;
                    let blocks: Vec<Range<usize>> = chunks(batch, cap).collect();
                    // Ordered results, so the successful prefix is written and
                    // the first failure in output order is the one returned.
                    let framed: Vec<Result<(Option<Vec<u8>>, u64)>> = executor.install(|| {
                        blocks
                            .into_par_iter()
                            .map(|chunk| {
                                let mut bb = BlockBuilder::new();
                                encode_and_frame(
                                    &mut bb,
                                    kind,
                                    chunk,
                                    diff,
                                    loc,
                                    &compression,
                                    metrics,
                                )
                            })
                            .collect()
                    });
                    for result in framed {
                        let (frame, n) = result?;
                        added += n;
                        if let Some(frame) = frame {
                            writer.write_raw_owned(frame)?;
                        }
                    }
                }
            }
            _ => {
                for chunk in chunks(run, cap) {
                    invocation.check()?;
                    let (frame, n) = encode_and_frame(
                        &mut self.bb,
                        kind,
                        chunk,
                        diff,
                        loc,
                        &compression,
                        metrics,
                    )?;
                    added += n;
                    if let Some(frame) = frame {
                        writer.write_raw_owned(frame)?;
                    }
                }
            }
        }
        match kind {
            ElementKind::Node => stats.diff_nodes += added,
            ElementKind::Way => stats.diff_ways += added,
            ElementKind::Relation => stats.diff_relations += added,
        }
        Ok(())
    }
}

/// `range` cut into consecutive ranges of `size` elements, the last one
/// shorter: the index-range form of `slice::chunks`.
fn chunks(range: Range<usize>, size: usize) -> impl Iterator<Item = Range<usize>> {
    let end = range.end;
    range
        .step_by(size)
        .map(move |start| start..end.min(start + size))
}

/// Encode the creates `run` (an index range into the kind's sorted upserts,
/// at most one block's worth) into the empty builder `bb` and frame the
/// block, returning the frame (`None` for an empty run) and the number of
/// elements added. The builder's poison, if an addition was refused,
/// surfaces through `lend_encoded`.
fn encode_and_frame(
    bb: &mut BlockBuilder,
    kind: ElementKind,
    run: Range<usize>,
    diff: &SortedDiff,
    loc: LocMode<'_>,
    compression: &Compression,
    metrics: &crate::write::metrics::WriterMetrics,
) -> Result<(Option<Vec<u8>>, u64)> {
    debug_assert!(bb.is_empty() && run.len() <= DEFAULT_ELEMENT_CAP.get());
    let added = run.len() as u64;
    for i in run {
        match kind {
            ElementKind::Node => add_osc_node(bb, &diff.node(i)),
            ElementKind::Way => {
                let osc = diff.way(i);
                let locs = loc.for_way(osc.id())?;
                add_osc_way(bb, &osc, locs);
            }
            ElementKind::Relation => add_osc_relation(bb, &diff.relation(i)),
        }
    }
    let frame = bb.lend_encoded(|block| {
        let indexdata = block.index.serialize();
        // No WayMembers, as `take_owned` gives none: the same frame the
        // writer's own framing job made of these blocks.
        frame_blob_pipelined(
            block.bytes,
            compression,
            Some(&indexdata),
            block.tagdata,
            None,
            metrics,
        )
    })?;
    Ok((frame, added))
}

/// Flush remaining upserts for the previous element type during a type
/// transition. Also handles skipped types (e.g., Node -> Relation flushes
/// all Way upserts).
#[allow(clippy::too_many_arguments)]
pub(super) fn flush_remaining_upserts(
    prev: ElementKind,
    next: ElementKind,
    diff: &SortedDiff,
    cursors: &mut UpsertCursors,
    creates: &mut CreateEncoder<'_>,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<u64> {
    // Flush remaining creates of the previous type
    let mut emitted = emit_rest(prev, diff, cursors, creates, writer, stats, loc)?;

    // Handle skipped type: Node -> Relation (flush all Way upserts)
    if prev == ElementKind::Node && next == ElementKind::Relation {
        emitted += emit_rest(ElementKind::Way, diff, cursors, creates, writer, stats, loc)?;
    }

    Ok(emitted)
}

/// Write every upsert of `kind` from the cursor on and advance the cursor
/// to the end; returns how many ids that was.
fn emit_rest(
    kind: ElementKind,
    diff: &SortedDiff,
    cursors: &mut UpsertCursors,
    creates: &mut CreateEncoder<'_>,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<u64> {
    let (cursor, upserts) = cursors.get_mut(kind, diff);
    let run = *cursor..upserts.len();
    *cursor = upserts.len();
    let emitted = run.len() as u64;
    creates.emit(kind, run, diff, loc, writer, stats)?;
    Ok(emitted)
}

/// Emit gap creates: upsert IDs of the current type that fall before a blob's min_id.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_gap_creates(
    blob_kind: ElementKind,
    min_id: i64,
    diff: &SortedDiff,
    cursors: &mut UpsertCursors,
    creates: &mut CreateEncoder<'_>,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<()> {
    let (cursor, upserts) = cursors.get_mut(blob_kind, diff);
    let start = *cursor;
    let len = upserts[start..].partition_point(|&id| crate::osm_id::osm_id_cmp(id, min_id).is_lt());
    *cursor = start + len;
    creates.emit(blob_kind, start..start + len, diff, loc, writer, stats)
}

/// Check whether there are gap creates to emit before min_id (without mutating cursors).
pub(super) fn has_gap_creates(
    blob_kind: ElementKind,
    min_id: i64,
    diff: &SortedDiff,
    cursors: &UpsertCursors,
) -> bool {
    let (cursor, upserts) = cursors.get(blob_kind, diff);
    cursor < upserts.len() && crate::osm_id::osm_id_cmp(upserts[cursor], min_id).is_lt()
}
