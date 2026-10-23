//! Output helpers for gap creates, trailing creates at type transitions,
//! and passthrough coalescing. The drain calls these while processing items
//! in file order.

use crate::ElementKind;
use crate::block_builder::BlockBuilder;
use crate::file_writer::FileWriter;
use crate::osc::CompactDiffOverlay;
use crate::writer::PbfWriter;

use crate::commands::{ensure_node_capacity, flush_block};

use super::Result;
use super::diff_ranges::{DiffRanges, UpsertCursors};
use super::element_writes::{add_osc_node, write_osc_relation, write_osc_way};
use super::loc_mode::LocMode;
use super::stats::MergeStats;

// This dispatch submits capacity flushes directly to PbfWriter. Its local
// counterpart in rewrite_block returns blocks to the ordered drain instead;
// both share encoding without changing either path's flush/stat ownership.
/// Emit a single create element via PbfWriter (for gap creates and trailing creates).
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_create_for_output(
    id: i64,
    kind: ElementKind,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<()> {
    match kind {
        ElementKind::Node => {
            if let Some(osc) = diff.get_node(id) {
                ensure_node_capacity(bb, writer)?;
                add_osc_node(bb, &osc);
                stats.diff_nodes += 1;
            }
        }
        ElementKind::Way => {
            if let Some(osc) = diff.get_way(id) {
                write_osc_way(bb, writer, &osc, loc)?;
                stats.diff_ways += 1;
            }
        }
        ElementKind::Relation => {
            if let Some(osc) = diff.get_relation(id) {
                write_osc_relation(bb, writer, &osc)?;
                stats.diff_relations += 1;
            }
        }
    }
    Ok(())
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
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<()> {
    // Flush remaining creates of the previous type
    let (cursor, upserts) = cursors.get_mut(prev, ranges);
    while *cursor < upserts.len() {
        emit_create_for_output(upserts[*cursor], prev, diff, bb, writer, stats, loc)?;
        *cursor += 1;
    }
    flush_block(bb, writer)?;

    // Handle skipped type: Node -> Relation (flush all Way upserts)
    if prev == ElementKind::Node && next == ElementKind::Relation {
        let (cursor, upserts) = cursors.get_mut(ElementKind::Way, ranges);
        while *cursor < upserts.len() {
            emit_create_for_output(
                upserts[*cursor],
                ElementKind::Way,
                diff,
                bb,
                writer,
                stats,
                loc,
            )?;
            *cursor += 1;
        }
        flush_block(bb, writer)?;
    }

    Ok(())
}

/// Emit gap creates: upsert IDs of the current type that fall before a blob's min_id.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_gap_creates(
    blob_kind: ElementKind,
    min_id: i64,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    cursors: &mut UpsertCursors,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<()> {
    let (cursor, upserts) = cursors.get_mut(blob_kind, ranges);
    while *cursor < upserts.len() && crate::osm_id::osm_id_cmp(upserts[*cursor], min_id).is_lt() {
        emit_create_for_output(upserts[*cursor], blob_kind, diff, bb, writer, stats, loc)?;
        *cursor += 1;
    }
    Ok(())
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
