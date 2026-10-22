//! Element-write helpers shared between OSC-diff emission and base-PBF
//! passthrough. Two shapes: `*_local` helpers flush to a local `Vec<OwnedBlock>`
//! for parallel rewrite tasks; the other pair writes through a `PbfWriter`
//! directly for the sequential output path.

use crate::PrimitiveBlock;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::file_writer::FileWriter;
use crate::owned::{dense_node_raw_metadata, element_raw_metadata};
use crate::writer::PbfWriter;

use crate::commands::{
    ensure_node_capacity_local, ensure_relation_capacity, ensure_relation_capacity_local,
    ensure_way_capacity, ensure_way_capacity_local, flush_local,
};

use super::Result;
use super::loc_mode::{LocMap, LocMode};

// ---------------------------------------------------------------------------
// Writing OSC elements (from diff, metadata carried from the OSC attributes)
// ---------------------------------------------------------------------------

// Callers own merge-stat accounting; these helpers only encode elements.

pub(super) fn write_osc_way(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    way: &crate::osc::CompactWayRef<'_>,
    loc: LocMode<'_>,
) -> Result<()> {
    let locs = loc.for_way(way.id())?;
    ensure_way_capacity(bb, writer)?;
    add_osc_way(bb, way, locs);
    Ok(())
}

/// Add an OSC way to `bb`, with inline locations when `locs` is present.
///
/// An id absent from `locs` gets `(0, 0)`. That is only reachable when the
/// run allows it: `rewrite::merge` counts unresolved references against the
/// merged map and fails the command unless `--ignore-missing-nodes` asked
/// for this fallback.
fn add_osc_way(bb: &mut BlockBuilder, way: &crate::osc::CompactWayRef<'_>, locs: Option<&LocMap>) {
    let refs: Vec<i64> = way.refs().collect();
    let meta = way.metadata();
    if let Some(locs) = locs {
        let locations: Vec<(i32, i32)> = refs
            .iter()
            .map(|node_id| locs.get(node_id).copied().unwrap_or((0, 0)))
            .collect();
        bb.add_way_with_locations(way.id(), way.tags(), &refs, &locations, meta.as_ref());
    } else {
        bb.add_way(way.id(), way.tags(), &refs, meta.as_ref());
    }
}

pub(super) fn write_osc_relation(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    rel: &crate::osc::CompactRelationRef<'_>,
) -> Result<()> {
    ensure_relation_capacity(bb, writer)?;
    let members: Vec<MemberData<'_>> = rel
        .members()
        .map(|(mt, ref_id, role)| MemberData {
            id: crate::MemberId::from_id_and_type(ref_id, mt),
            role,
        })
        .collect();
    bb.add_relation(rel.id(), rel.tags(), &members, rel.metadata().as_ref());
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing base elements for parallel rewrite (local flush, no PbfWriter)
// ---------------------------------------------------------------------------

pub(super) fn write_base_dense_node_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    dn: &crate::DenseNode<'_>,
    block: &PrimitiveBlock,
) -> Result<()> {
    ensure_node_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block);
    }
    let meta = dense_node_raw_metadata(dn);
    bb.add_node_raw(
        dn.id(),
        dn.decimicro_lat(),
        dn.decimicro_lon(),
        dn.raw_tags(),
        meta.as_ref(),
    );
    Ok(())
}

pub(super) fn write_base_node_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    node: &crate::Node<'_>,
    block: &PrimitiveBlock,
) -> Result<()> {
    // Preserve the block API's documented wrapping coordinate scaling and
    // narrowing, just as write_base_dense_node_local does. This rewrite
    // helper is not a coordinate validator. Under --locations-on-ways the
    // worker's node_coords extractor checks scaling before rewriting a blob
    // routed as Node; outside that mode base-node rewrites retain the block
    // API policy. Adding checked scaling here alone would make plain nodes
    // differ from dense nodes and would not validate passthrough blobs.
    ensure_node_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block);
    }
    let meta = element_raw_metadata(&node.info());
    bb.add_node_raw(
        node.id(),
        node.decimicro_lat(),
        node.decimicro_lon(),
        node.raw_tags()
            .map(|(k, v)| (k.cast_signed(), v.cast_signed())),
        meta.as_ref(),
    );
    Ok(())
}

pub(super) fn write_base_way_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    way: &crate::Way<'_>,
    block: &PrimitiveBlock,
) -> Result<()> {
    ensure_way_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block);
    }
    bb.add_way_raw_bytes(
        way.id(),
        way.keys_data(),
        way.vals_data(),
        way.refs_data(),
        way.info_data(),
    );
    Ok(())
}

/// Write a surviving base way with LocationsOnWays data preserved.
///
/// Like `write_base_way_local` but also forwards raw `lat_data`/`lon_data` bytes.
pub(super) fn write_base_way_local_with_locations(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    way: &crate::Way<'_>,
    block: &PrimitiveBlock,
) -> Result<()> {
    ensure_way_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block);
    }
    bb.add_way_raw_bytes_with_locations(
        way.id(),
        way.keys_data(),
        way.vals_data(),
        way.refs_data(),
        way.lat_data(),
        way.lon_data(),
        way.info_data(),
    );
    Ok(())
}

/// Write an OSC way, with LocationsOnWays coordinates when `loc` requires them.
pub(super) fn write_osc_way_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    way: &crate::osc::CompactWayRef<'_>,
    loc: LocMode<'_>,
) -> Result<()> {
    let locs = loc.for_way(way.id())?;
    ensure_way_capacity_local(bb, output)?;
    add_osc_way(bb, way, locs);
    Ok(())
}

pub(super) fn write_base_relation_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    rel: &crate::Relation<'_>,
    block: &PrimitiveBlock,
) -> Result<()> {
    ensure_relation_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block);
    }
    bb.add_relation_raw_bytes(
        rel.id(),
        rel.keys_data(),
        rel.vals_data(),
        rel.roles_sid_data(),
        rel.memids_data(),
        rel.types_data(),
        rel.info_data(),
    );
    Ok(())
}
