//! Sequential output-path helpers for `merge()`: gap creates, trailing
//! creates at type transitions, and passthrough coalescing. These run on the
//! main thread as slots are drained in file order.

use rustc_hash::FxHashMap;

use crate::blob_meta::ElemKind;
use crate::block_builder::BlockBuilder;
use crate::file_writer::FileWriter;
use crate::osc::CompactDiffOverlay;
use crate::writer::PbfWriter;

use crate::commands::{ensure_node_capacity, flush_block};

use super::Result;
use super::diff_ranges::{DiffRanges, UpsertCursors};
use super::element_writes::{write_osc_relation, write_osc_way};
use super::stats::MergeStats;

/// Emit a single create element via PbfWriter (for gap creates and trailing creates).
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_create_for_output(
    id: i64,
    kind: ElemKind,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    match kind {
        ElemKind::Node => {
            if let Some(osc) = diff.get_node(id) {
                ensure_node_capacity(bb, writer)?;
                bb.add_node(
                    osc.id(),
                    osc.decimicro_lat(),
                    osc.decimicro_lon(),
                    osc.tags(),
                    None,
                );
                stats.diff_nodes += 1;
            }
        }
        ElemKind::Way => {
            if let Some(osc) = diff.get_way(id) {
                write_osc_way(bb, writer, &osc, loc_map, stats)?;
                stats.diff_ways += 1;
            }
        }
        ElemKind::Relation => {
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
    prev: ElemKind,
    next: ElemKind,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    cursors: &mut UpsertCursors,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    // Flush remaining creates of the previous type
    let (cursor, upserts) = cursors.get_mut(prev, ranges);
    while *cursor < upserts.len() {
        emit_create_for_output(upserts[*cursor], prev, diff, bb, writer, stats, loc_map)?;
        *cursor += 1;
    }
    flush_block(bb, writer)?;

    // Handle skipped type: Node -> Relation (flush all Way upserts)
    if prev == ElemKind::Node && next == ElemKind::Relation {
        let (cursor, upserts) = cursors.get_mut(ElemKind::Way, ranges);
        while *cursor < upserts.len() {
            emit_create_for_output(
                upserts[*cursor],
                ElemKind::Way,
                diff,
                bb,
                writer,
                stats,
                loc_map,
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
    blob_kind: ElemKind,
    min_id: i64,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    cursors: &mut UpsertCursors,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    let (cursor, upserts) = cursors.get_mut(blob_kind, ranges);
    while *cursor < upserts.len() && crate::osm_id::osm_id_cmp(upserts[*cursor], min_id).is_lt() {
        emit_create_for_output(
            upserts[*cursor],
            blob_kind,
            diff,
            bb,
            writer,
            stats,
            loc_map,
        )?;
        *cursor += 1;
    }
    Ok(())
}

/// Check whether there are gap creates to emit before min_id (without mutating cursors).
pub(super) fn has_gap_creates(
    blob_kind: ElemKind,
    min_id: i64,
    ranges: &DiffRanges,
    cursors: &UpsertCursors,
) -> bool {
    let (cursor, upserts) = cursors.get(blob_kind, ranges);
    cursor < upserts.len() && crate::osm_id::osm_id_cmp(upserts[cursor], min_id).is_lt()
}
