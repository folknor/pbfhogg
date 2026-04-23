//! Per-block rewrite loop: walks a decoded `PrimitiveBlock`, applies the OSC
//! diff overlay element-by-element, interleaves inline upsert creates at
//! their sorted positions, and flushes the results into owned blocks for the
//! main thread to hand to the pipelined writer.

use rustc_hash::FxHashMap;

use crate::blob_meta::ElemKind;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::osc::CompactDiffOverlay;
use crate::{Element, PrimitiveBlock};

use crate::commands::{
    ensure_node_capacity_local, ensure_relation_capacity_local, flush_local,
};

use super::element_writes::{
    write_base_dense_node_local, write_base_node_local, write_base_relation_local,
    write_base_way_local, write_base_way_local_with_locations, write_osc_way_local,
};
use super::stats::MergeStats;
use super::Result;

/// Output from `rewrite_block_parallel`: serialized blocks + local stats.
pub(super) struct RewriteOutput {
    pub(super) blocks: Vec<OwnedBlock>,
    pub(super) stats: MergeStats,
}

/// Emit a single create element into the local BlockBuilder.
#[allow(clippy::too_many_arguments)]
fn emit_create_local(
    id: i64,
    kind: ElemKind,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    match kind {
        ElemKind::Node => {
            if let Some(osc) = diff.get_node(id) {
                ensure_node_capacity_local(bb, output)?;
                bb.add_node(osc.id(), osc.decimicro_lat(), osc.decimicro_lon(), osc.tags(), None);
                stats.diff_nodes += 1;
            }
        }
        ElemKind::Way => {
            if let Some(osc) = diff.get_way(id) {
                write_osc_way_local(bb, output, &osc, loc_map, stats)?;
                stats.diff_ways += 1;
            }
        }
        ElemKind::Relation => {
            if let Some(osc) = diff.get_relation(id) {
                ensure_relation_capacity_local(bb, output)?;
                let members: Vec<MemberData<'_>> = osc
                    .members()
                    .map(|(mt, ref_id, role)| MemberData {
                        id: crate::MemberId::from_id_and_type(ref_id, mt),
                        role,
                    })
                    .collect();
                bb.add_relation(osc.id(), osc.tags(), &members, None);
                stats.diff_relations += 1;
            }
        }
    }
    Ok(())
}

/// Rewrite a block in parallel: same element-by-element logic as `rewrite_block`,
/// but flushes to local `Vec<Vec<u8>>` instead of `PbfWriter`. Interleaves
/// upserts at their sorted positions within the block - IDs that match base
/// elements are modifications (handled by normal element processing); IDs that
/// don't match are creates (emitted by the cursor).
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn rewrite_block_parallel(
    block: &PrimitiveBlock,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    inline_upserts: &[i64],
    kind: ElemKind,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<RewriteOutput> {
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut stats = MergeStats::new();
    let mut upsert_cursor: usize = 0;

    bb.pre_seed_string_table(block);

    for element in block.elements() {
        let elem_id = match &element {
            Element::DenseNode(dn) => dn.id(),
            Element::Node(n) => n.id(),
            Element::Way(w) => w.id(),
            Element::Relation(r) => r.id(),
        };

        // Emit creates (upsert IDs not in base block) before this element.
        // The `osm_id_cmp` here is coherent with the `osm_id_key` bounds used
        // to slice `inline_upserts` in `streaming::upsert_slice` ONLY if the
        // base block iterates in canonical OSM order. Apply-changes enforces
        // this via the `is_sorted()` precondition in `rewrite.rs`; any future
        // caller that bypasses the sort gate must re-establish the invariant
        // or the cursor may skip past upserts and drop creates.
        while upsert_cursor < inline_upserts.len() && crate::osm_id::osm_id_cmp(inline_upserts[upsert_cursor], elem_id).is_lt() {
            let cid = inline_upserts[upsert_cursor];
            upsert_cursor += 1;
            emit_create_local(cid, kind, diff, bb, &mut output, &mut stats, loc_map)?;
        }
        // Skip modification IDs (handled below by normal element processing)
        if upsert_cursor < inline_upserts.len() && inline_upserts[upsert_cursor] == elem_id {
            upsert_cursor += 1;
        }

        match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                if diff.deleted_nodes.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_node(id) {
                    ensure_node_capacity_local(bb, &mut output)?;
                    bb.add_node(osc.id(), osc.decimicro_lat(), osc.decimicro_lon(), osc.tags(), None);

                    stats.diff_nodes += 1;
                } else {
                    write_base_dense_node_local(bb, &mut output, dn, block)?;
                    stats.base_nodes += 1;
                }
            }
            Element::Node(n) => {
                let id = n.id();
                if diff.deleted_nodes.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_node(id) {
                    ensure_node_capacity_local(bb, &mut output)?;
                    bb.add_node(osc.id(), osc.decimicro_lat(), osc.decimicro_lon(), osc.tags(), None);

                    stats.diff_nodes += 1;
                } else {
                    write_base_node_local(bb, &mut output, n, block)?;
                    stats.base_nodes += 1;
                }
            }
            Element::Way(w) => {
                let id = w.id();
                if diff.deleted_ways.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_way(id) {
                    write_osc_way_local(bb, &mut output, &osc, loc_map, &mut stats)?;
                    stats.diff_ways += 1;
                } else if loc_map.is_some() {
                    // Forward existing raw lat/lon data for LocationsOnWays
                    write_base_way_local_with_locations(bb, &mut output, w, block)?;
                    stats.base_ways += 1;
                } else {
                    write_base_way_local(bb, &mut output, w, block)?;
                    stats.base_ways += 1;
                }
            }
            Element::Relation(r) => {
                let id = r.id();
                if diff.deleted_relations.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_relation(id) {
                    ensure_relation_capacity_local(bb, &mut output)?;
                    let members: Vec<MemberData<'_>> = osc
                        .members()
                        .map(|(mt, ref_id, role)| MemberData {
                            id: crate::MemberId::from_id_and_type(ref_id, mt),
                            role,
                        })
                        .collect();
                    bb.add_relation(osc.id(), osc.tags(), &members, None);

                    stats.diff_relations += 1;
                } else {
                    write_base_relation_local(bb, &mut output, r, block)?;
                    stats.base_relations += 1;
                }
            }
        }
    }

    // Emit remaining upserts after the last element (trailing creates)
    while upsert_cursor < inline_upserts.len() {
        let cid = inline_upserts[upsert_cursor];
        upsert_cursor += 1;
        emit_create_local(cid, kind, diff, bb, &mut output, &mut stats, loc_map)?;
    }

    // Flush remaining elements in the BlockBuilder
    flush_local(bb, &mut output)?;

    Ok(RewriteOutput {
        blocks: output,
        stats,
    })
}
