//! Element-write helpers shared between OSC-diff emission and base-PBF
//! passthrough. Two shapes: `*_local` helpers flush to a local `Vec<OwnedBlock>`
//! for parallel rewrite tasks; the other pair writes through a `PbfWriter`
//! directly for the sequential output path.

use rustc_hash::FxHashMap;

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
use super::stats::MergeStats;

// ---------------------------------------------------------------------------
// Writing OSC elements (from diff, metadata carried from the OSC attributes)
// ---------------------------------------------------------------------------

pub(super) fn write_osc_way(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    way: &crate::osc::CompactWayRef<'_>,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
    _stats: &mut MergeStats,
) -> Result<()> {
    ensure_way_capacity(bb, writer)?;
    let refs: Vec<i64> = way.refs().collect();
    let meta = way.metadata();
    if let Some(locs) = loc_map {
        let mut locations: Vec<(i32, i32)> = Vec::with_capacity(refs.len());
        for &node_id in &refs {
            match locs.get(&node_id) {
                Some(&loc) => locations.push(loc),
                None => locations.push((0, 0)),
            }
        }
        bb.add_way_with_locations(way.id(), way.tags(), &refs, &locations, meta.as_ref());
    } else {
        bb.add_way(way.id(), way.tags(), &refs, meta.as_ref());
    }
    Ok(())
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

/// Write an OSC way with optional LocationsOnWays coordinate lookup.
pub(super) fn write_osc_way_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    way: &crate::osc::CompactWayRef<'_>,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
    _stats: &mut MergeStats,
) -> Result<()> {
    ensure_way_capacity_local(bb, output)?;
    let refs: Vec<i64> = way.refs().collect();
    let meta = way.metadata();

    if let Some(locs) = loc_map {
        let mut locations: Vec<(i32, i32)> = Vec::with_capacity(refs.len());
        for &node_id in &refs {
            match locs.get(&node_id) {
                Some(&loc) => locations.push(loc),
                None => locations.push((0, 0)),
            }
        }
        bb.add_way_with_locations(way.id(), way.tags(), &refs, &locations, meta.as_ref());
    } else {
        bb.add_way(way.id(), way.tags(), &refs, meta.as_ref());
    }
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
