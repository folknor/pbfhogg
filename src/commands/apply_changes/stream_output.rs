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

use rayon::prelude::*;

use crate::ElementKind;
use crate::block_builder::{BlockBuilder, DEFAULT_ELEMENT_CAP};
use crate::file_writer::FileWriter;
use crate::osc::CompactDiffOverlay;
use crate::tunables::APPLY_CHANGES_CREATE_BATCH_BLOCKS;
use crate::writer::{Compression, PbfWriter, frame_blob_pipelined};

use super::Result;
use super::diff_ranges::{DiffRanges, UpsertCursors};
use super::element_writes::{add_osc_node, add_osc_relation, add_osc_way};
use super::loc_mode::LocMode;
use super::stats::MergeStats;

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

    /// Write the creates `ids` of `kind`, in order, as blocks of
    /// `DEFAULT_ELEMENT_CAP`. Creates counted into `stats` per kind. The
    /// first failure in output order is returned, after every block before
    /// it has been written; nothing after it is.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit(
        &mut self,
        kind: ElementKind,
        ids: &[i64],
        diff: &CompactDiffOverlay,
        loc: LocMode<'_>,
        writer: &mut PbfWriter<FileWriter>,
        stats: &mut MergeStats,
    ) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let cap = DEFAULT_ELEMENT_CAP.get();
        let invocation = writer.cancel_token().permit();
        let metrics = &invocation.metrics().writer;
        let compression = self.compression;
        let mut added = 0u64;
        match self.executor {
            Some(executor) if ids.len() > cap => {
                for batch in ids.chunks(cap * APPLY_CHANGES_CREATE_BATCH_BLOCKS) {
                    invocation.check()?;
                    // Ordered results, so the successful prefix is written and
                    // the first failure in output order is the one returned.
                    let framed: Vec<Result<(Option<Vec<u8>>, u64)>> = executor.install(|| {
                        batch
                            .par_chunks(cap)
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
                for chunk in ids.chunks(cap) {
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

/// Encode the creates `ids` (at most one block's worth) into the empty
/// builder `bb` and frame the block, returning the frame (`None` when no id
/// had a record) and the number of elements added. The builder's poison, if
/// an addition was refused, surfaces through `take_encoded`.
fn encode_and_frame(
    bb: &mut BlockBuilder,
    kind: ElementKind,
    ids: &[i64],
    diff: &CompactDiffOverlay,
    loc: LocMode<'_>,
    compression: &Compression,
    metrics: &crate::write::metrics::WriterMetrics,
) -> Result<(Option<Vec<u8>>, u64)> {
    debug_assert!(bb.is_empty() && ids.len() <= DEFAULT_ELEMENT_CAP.get());
    let mut added = 0u64;
    for &id in ids {
        match kind {
            ElementKind::Node => {
                if let Some(osc) = diff.get_node(id) {
                    add_osc_node(bb, &osc);
                    added += 1;
                }
            }
            ElementKind::Way => {
                if let Some(osc) = diff.get_way(id) {
                    let locs = loc.for_way(id)?;
                    add_osc_way(bb, &osc, locs);
                    added += 1;
                }
            }
            ElementKind::Relation => {
                if let Some(osc) = diff.get_relation(id) {
                    add_osc_relation(bb, &osc);
                    added += 1;
                }
            }
        }
    }
    let Some(block) = bb.take_encoded()? else {
        return Ok((None, added));
    };
    let indexdata = block.index.serialize();
    // No WayMembers, as `take_owned` gives none: the same frame the writer's
    // own framing job made of these blocks.
    let frame = frame_blob_pipelined(
        block.bytes,
        compression,
        Some(&indexdata),
        block.tagdata,
        None,
        metrics,
    )?;
    Ok((Some(frame), added))
}

/// Flush remaining upserts for the previous element type during a type
/// transition. Also handles skipped types (e.g., Node -> Relation flushes
/// all Way upserts).
#[allow(clippy::too_many_arguments)]
pub(super) fn flush_remaining_upserts(
    prev: ElementKind,
    next: ElementKind,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    cursors: &mut UpsertCursors,
    creates: &mut CreateEncoder<'_>,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<u64> {
    // Flush remaining creates of the previous type
    let mut emitted = emit_rest(prev, ranges, diff, cursors, creates, writer, stats, loc)?;

    // Handle skipped type: Node -> Relation (flush all Way upserts)
    if prev == ElementKind::Node && next == ElementKind::Relation {
        emitted += emit_rest(
            ElementKind::Way,
            ranges,
            diff,
            cursors,
            creates,
            writer,
            stats,
            loc,
        )?;
    }

    Ok(emitted)
}

/// Write every upsert of `kind` from the cursor on and advance the cursor
/// to the end; returns how many ids that was.
#[allow(clippy::too_many_arguments)]
fn emit_rest(
    kind: ElementKind,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    cursors: &mut UpsertCursors,
    creates: &mut CreateEncoder<'_>,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<u64> {
    let (cursor, upserts) = cursors.get_mut(kind, ranges);
    let run = &upserts[*cursor..];
    *cursor = upserts.len();
    creates.emit(kind, run, diff, loc, writer, stats)?;
    Ok(run.len() as u64)
}

/// Emit gap creates: upsert IDs of the current type that fall before a blob's min_id.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_gap_creates(
    blob_kind: ElementKind,
    min_id: i64,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    cursors: &mut UpsertCursors,
    creates: &mut CreateEncoder<'_>,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<()> {
    let (cursor, upserts) = cursors.get_mut(blob_kind, ranges);
    let start = *cursor;
    let len = upserts[start..].partition_point(|&id| crate::osm_id::osm_id_cmp(id, min_id).is_lt());
    *cursor = start + len;
    creates.emit(
        blob_kind,
        &upserts[start..start + len],
        diff,
        loc,
        writer,
        stats,
    )
}

/// Check whether there are gap creates to emit before min_id (without mutating cursors).
pub(super) fn has_gap_creates(
    blob_kind: ElementKind,
    min_id: i64,
    ranges: &DiffRanges,
    cursors: &UpsertCursors,
) -> bool {
    let (cursor, upserts) = cursors.get(blob_kind, ranges);
    cursor < upserts.len() && crate::osm_id::osm_id_cmp(upserts[cursor], min_id).is_lt()
}
