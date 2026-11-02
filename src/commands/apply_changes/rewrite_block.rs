//! Per-block rewrite loop: walks a decoded `PrimitiveBlock`, applies the OSC
//! diff overlay element-by-element, interleaves inline upsert creates at
//! their sorted positions, and frames each output block on the worker as it
//! completes.

use crate::ElementKind;
use crate::blob_meta::BlobIndex;
use crate::block_builder::BlockBuilder;
use crate::osc::CompactDiffOverlay;
use crate::writer::Compression;
use crate::{Element, PrimitiveBlock};

use crate::commands::{
    FramedBlocks, ensure_node_capacity_local, ensure_relation_capacity_local, flush_local,
};

use super::Result;
use super::classify::blob_walk;
use super::element_writes::{
    BaseWrites, RefreshScratch, RefreshTally, add_osc_node, add_osc_relation,
    write_base_dense_node_local, write_base_node_local, write_base_relation_local,
    write_base_way_local, write_base_way_local_with_locations, write_osc_way_local,
};
use super::loc_mode::LocMode;
use super::refresh::StaleProbe;
use super::stats::MergeStats;

/// Output from `rewrite_block_parallel`: the blob's frames, from a rewrite
/// whose walk has ended, and its local stats.
pub(super) struct RewriteOutput {
    /// Each output block's frame, with its kind, in output order.
    pub(super) frames: Vec<(ElementKind, Vec<u8>)>,
    /// Wall time spent framing those blocks, excluding their encoding.
    pub(super) frame_ns: u64,
    pub(super) stats: MergeStats,
    /// Inline-coordinate refresh counts for the block's surviving base ways
    /// (all zero without `--locations-on-ways`).
    pub(super) refresh: RefreshTally,
}

// The create dispatch mirrors stream_output, but capacity checks below must
// flush into this worker's local blocks before stats advance. The shared
// add_osc_* helpers own only encoding, not ordering or output submission.
/// Emit a single create element into the local BlockBuilder.
#[allow(clippy::too_many_arguments)]
fn emit_create_local(
    id: i64,
    kind: ElementKind,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    output: &mut FramedBlocks<'_>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<()> {
    match kind {
        ElementKind::Node => {
            if let Some(osc) = diff.get_node(id) {
                ensure_node_capacity_local(bb, output)?;
                add_osc_node(bb, &osc);
                stats.diff_nodes += 1;
            }
        }
        ElementKind::Way => {
            if let Some(osc) = diff.get_way(id) {
                write_osc_way_local(bb, output, &osc, loc)?;
                stats.diff_ways += 1;
            }
        }
        ElementKind::Relation => {
            if let Some(osc) = diff.get_relation(id) {
                ensure_relation_capacity_local(bb, output)?;
                add_osc_relation(bb, &osc);
                stats.diff_relations += 1;
            }
        }
    }
    Ok(())
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
/// beside its framed copy. A completed block's framing error therefore
/// surfaces at the flush that completes it, ahead of whatever fallible work
/// the element's write helper does after that flush; it promises no other
/// ordering against the element's errors.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn rewrite_block_parallel(
    block: &PrimitiveBlock,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    inline_upserts: &[i64],
    kind: ElementKind,
    loc: LocMode<'_>,
    probe: &StaleProbe,
    scratch: &mut RefreshScratch,
    claim: Option<&BlobIndex>,
    compression: &Compression,
    metrics: &crate::write::metrics::WriterMetrics,
) -> Result<RewriteOutput> {
    let mut walk = blob_walk(claim);
    let mut output = FramedBlocks::new(*compression, metrics);
    let mut stats = MergeStats::new();
    let mut upsert_cursor: usize = 0;
    let mut base_writes = BaseWrites::new(block, probe, scratch);

    bb.pre_seed_string_table(block);

    for element in block.elements() {
        let elem_id = match &element {
            Element::DenseNode(dn) => dn.id(),
            Element::Node(n) => n.id(),
            Element::Way(w) => w.id(),
            Element::Relation(r) => r.id(),
        };
        walk.observe(elem_id)?;

        // Emit creates (upsert IDs not in base block) before this element.
        // The `osm_id_cmp` here is coherent with the `osm_id_key` bounds used
        // to slice `inline_upserts` in `streaming::upsert_slice` ONLY if the
        // base block iterates in canonical OSM order. The header's
        // `is_sorted()` precondition (`rewrite.rs::build_header_bytes`)
        // claims it and `walk` refuses the block where it is false; any
        // future caller that bypasses both must re-establish the invariant
        // or the cursor may skip past upserts and drop creates.
        while upsert_cursor < inline_upserts.len()
            && crate::osm_id::osm_id_cmp(inline_upserts[upsert_cursor], elem_id).is_lt()
        {
            let cid = inline_upserts[upsert_cursor];
            upsert_cursor += 1;
            emit_create_local(cid, kind, diff, bb, &mut output, &mut stats, loc)?;
        }
        // Skip modification IDs (handled below by normal element processing)
        if upsert_cursor < inline_upserts.len() && inline_upserts[upsert_cursor] == elem_id {
            upsert_cursor += 1;
        }

        match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                if diff.deleted_ids(crate::ElementKind::Node).contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_node(id) {
                    ensure_node_capacity_local(bb, &mut output)?;
                    add_osc_node(bb, &osc);

                    stats.diff_nodes += 1;
                } else {
                    write_base_dense_node_local(bb, &mut output, dn, block)?;
                    stats.base_nodes += 1;
                }
            }
            Element::Node(n) => {
                let id = n.id();
                if diff.deleted_ids(crate::ElementKind::Node).contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_node(id) {
                    ensure_node_capacity_local(bb, &mut output)?;
                    add_osc_node(bb, &osc);

                    stats.diff_nodes += 1;
                } else {
                    write_base_node_local(bb, &mut output, n, block)?;
                    stats.base_nodes += 1;
                }
            }
            Element::Way(w) => {
                let id = w.id();
                if diff.deleted_ids(crate::ElementKind::Way).contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_way(id) {
                    write_osc_way_local(bb, &mut output, &osc, loc)?;
                    stats.diff_ways += 1;
                } else if loc.for_way(id)?.is_some() {
                    write_base_way_local_with_locations(
                        bb,
                        &mut output,
                        w,
                        block,
                        &mut base_writes,
                    )?;
                    stats.base_ways += 1;
                } else {
                    write_base_way_local(bb, &mut output, w, block, &mut base_writes)?;
                    stats.base_ways += 1;
                }
            }
            Element::Relation(r) => {
                let id = r.id();
                if diff.deleted_ids(crate::ElementKind::Relation).contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_relation(id) {
                    ensure_relation_capacity_local(bb, &mut output)?;
                    add_osc_relation(bb, &osc);

                    stats.diff_relations += 1;
                } else {
                    write_base_relation_local(bb, &mut output, r, block, &mut base_writes)?;
                    stats.base_relations += 1;
                }
            }
        }
    }

    // Emit remaining upserts after the last element (trailing creates)
    while upsert_cursor < inline_upserts.len() {
        let cid = inline_upserts[upsert_cursor];
        upsert_cursor += 1;
        emit_create_local(cid, kind, diff, bb, &mut output, &mut stats, loc)?;
    }

    // Flush remaining elements in the BlockBuilder, then end the walk.
    flush_local(bb, &mut output)?;
    walk.finish()?;

    Ok(RewriteOutput {
        frames: output.frames,
        frame_ns: output.frame_ns,
        stats,
        refresh: base_writes.tally,
    })
}
