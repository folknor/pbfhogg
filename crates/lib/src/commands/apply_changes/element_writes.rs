//! Element-write helpers shared between OSC-diff emission and base-PBF
//! passthrough. The `*_local` helpers flush into the rewrite task's own
//! block sink, which frames each block on the worker as it completes;
//! the `add_osc_*` encoders add one element and leave capacity to the
//! caller, as the drain's create runs (`stream_output.rs`) size every block
//! themselves.

use crate::PrimitiveBlock;
use crate::block_builder::{BlockBuilder, MemberData};
use crate::owned::{dense_node_raw_metadata, element_raw_partial_metadata};

use crate::commands::{
    BlockSink, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local,
};

use super::Result;
use super::loc_mode::{LocMap, LocMode};
use super::refresh::{CoordFraming, StaleProbe, evaluate_way, refreshed_locations};

// ---------------------------------------------------------------------------
// Writing OSC elements (from diff, metadata carried from the OSC attributes)
// ---------------------------------------------------------------------------

// Callers own merge-stat accounting and select the capacity/flush operation.
// Keep worker-local output and the drain's create runs separate: a local
// flush frames a block of the rewrite task, whereas a create run cuts
// its own blocks. A common sink must not move flush boundaries, pre-seeding
// or stats across those owners. Share only the post-capacity element encoding.

pub(super) fn add_osc_node(bb: &mut BlockBuilder, node: &crate::osc::CompactNodeRef<'_>) {
    bb.add_node_partial(
        node.id(),
        node.decimicro_lat(),
        node.decimicro_lon(),
        node.tags(),
        node.metadata().as_ref(),
    );
}

/// Whether an OSC node carries no metadata, so that it can be written as a
/// plain `Node` without `Info` claiming exactly what its dense form does.
pub(super) fn osc_node_is_bare(node: &crate::osc::CompactNodeRef<'_>) -> bool {
    node.metadata()
        .is_none_or(|m| m == crate::block_builder::PartialMetadata::default())
}

/// [`add_osc_node`] for a bare node (`osc_node_is_bare`), written plain:
/// the overflow recovery's retry of a lone node that overflowed dense.
///
/// The retry is limited to nodes without metadata on purpose. A node with
/// metadata goes dense with claims a plain `Info` reproduces only through a
/// conversion over every metadata state (a versioned slot's implied
/// visibility, the changeset sentinel, versionless padding columns), and
/// plain is not always smaller then (a negative timestamp is ten bytes in
/// `Info`, one in a dense delta). That trade, a new way to change a claim
/// for a window of a few bytes below the blob limit, was refused.
pub(super) fn add_osc_node_bare_plain(
    bb: &mut BlockBuilder,
    node: &crate::osc::CompactNodeRef<'_>,
) {
    bb.add_node_bare_plain(
        node.id(),
        node.decimicro_lat(),
        node.decimicro_lon(),
        node.tags(),
    );
}

/// Add an OSC way to `bb`, with inline locations when `locs` is present.
///
/// An id absent from `locs` gets `(0, 0)`. That is only reachable when the
/// run allows it: `rewrite::merge` counts unresolved references against the
/// merged map and fails the command unless `--ignore-missing-nodes` asked
/// for this fallback.
///
/// The `refs` and `locations` vectors (and `members` in
/// [`add_osc_relation`]) are kept on purpose: replacing them with iterator
/// inputs to the builder was proposed and refused (2026-11-26). These run
/// once per OSC create or modify, a diff's worth of elements against a
/// planet's worth of base elements, so the allocations cannot show in wall.
pub(super) fn add_osc_way(
    bb: &mut BlockBuilder,
    way: &crate::osc::CompactWayRef<'_>,
    locs: Option<&LocMap>,
) {
    let refs: Vec<i64> = way.refs().collect();
    let meta = way.metadata();
    if let Some(locs) = locs {
        let locations: Vec<(i32, i32)> = refs
            .iter()
            .map(|node_id| locs.get(node_id).copied().unwrap_or((0, 0)))
            .collect();
        bb.add_way_with_locations_partial(way.id(), way.tags(), &refs, &locations, meta.as_ref());
    } else {
        bb.add_way_partial(way.id(), way.tags(), &refs, meta.as_ref());
    }
}

pub(super) fn add_osc_relation(bb: &mut BlockBuilder, rel: &crate::osc::CompactRelationRef<'_>) {
    let members: Vec<MemberData<'_>> = rel
        .members()
        .map(|(mt, ref_id, role)| MemberData {
            id: crate::MemberId::from_id_and_type(ref_id, mt),
            role,
        })
        .collect();
    bb.add_relation_partial(rel.id(), rel.tags(), &members, rel.metadata().as_ref());
}

// ---------------------------------------------------------------------------
// Writing base elements for parallel rewrite (local flush, no PbfWriter)
// ---------------------------------------------------------------------------

/// The string table a block holding surviving base elements starts from.
#[derive(Clone, Copy, Debug)]
pub(super) enum SourceSeed<'a> {
    /// The source block's whole table, slot for slot: the rewrite's first
    /// pass, whose survivors copy their indices unchanged.
    Whole,
    /// Only the source slots marked (by source index): the overflow
    /// replay's, whose survivors' indices are translated so a block carries
    /// only the strings its elements use (`overflow.rs`).
    Subset(&'a [bool]),
}

/// Mark in `used` (one entry per source slot) every source string slot the
/// raw writers below copy for surviving base `element`: its tag keys and
/// values, a relation's member roles, and its user, read with the
/// conversions the writers apply (`dense_node_raw_metadata`, which can
/// write a user naming the empty string as 0, and
/// `element_raw_partial_metadata`; the effective `Info` user of a way or
/// relation). An index outside the table is skipped: the raw add refuses
/// it with its own error.
pub(super) fn mark_used_slots(element: &crate::Element<'_>, used: &mut [bool]) -> Result<()> {
    use crate::read::wire::{PackedInt32Iter, PackedUint32Iter, WireInfo};
    let mut mark = |sid: u32| {
        if let Some(slot) = used.get_mut(sid as usize) {
            *slot = true;
        }
    };
    match element {
        crate::Element::DenseNode(dn) => {
            for (k, v) in dn.raw_tags() {
                mark(k.cast_unsigned());
                mark(v.cast_unsigned());
            }
            if let Some(meta) = dense_node_raw_metadata(dn)? {
                mark(meta.user_sid.cast_unsigned());
            }
        }
        crate::Element::Node(n) => {
            for (k, v) in n.raw_tags() {
                mark(k);
                mark(v);
            }
            let meta = element_raw_partial_metadata(crate::ElementKind::Node, n.id(), &n.info())?;
            if let Some(sid) = meta.and_then(|m| m.user_sid) {
                mark(sid.cast_unsigned());
            }
        }
        crate::Element::Way(w) => {
            PackedUint32Iter::new(w.keys_data()).for_each(&mut mark);
            PackedUint32Iter::new(w.vals_data()).for_each(&mut mark);
            if let Some(sid) = w
                .info_data()
                .map(WireInfo::user_sid_of)
                .transpose()?
                .flatten()
            {
                mark(sid.cast_unsigned());
            }
        }
        crate::Element::Relation(r) => {
            PackedUint32Iter::new(r.keys_data()).for_each(&mut mark);
            PackedUint32Iter::new(r.vals_data()).for_each(&mut mark);
            for sid in PackedInt32Iter::new(r.roles_sid_data()) {
                mark(sid.cast_unsigned());
            }
            if let Some(sid) = r
                .info_data()
                .map(WireInfo::user_sid_of)
                .transpose()?
                .flatten()
            {
                mark(sid.cast_unsigned());
            }
        }
    }
    Ok(())
}

/// Seed `bb` from `block` for a surviving base element, unless the block in
/// progress already is: a block started by diff elements is flushed first
/// under a whole-table seed, which needs an empty builder; a subset seed
/// is set on an empty builder too (after any flush), so the same rule
/// serves both.
fn ensure_seeded<O: BlockSink + ?Sized>(
    bb: &mut BlockBuilder,
    output: &mut O,
    block: &PrimitiveBlock,
    seed: SourceSeed<'_>,
) -> Result<()> {
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        match seed {
            SourceSeed::Whole => bb.pre_seed_string_table(block)?,
            SourceSeed::Subset(used) => bb.pre_seed_subset_string_table(block, used)?,
        }
    }
    Ok(())
}

pub(super) fn write_base_dense_node_local<O: BlockSink + ?Sized>(
    bb: &mut BlockBuilder,
    output: &mut O,
    dn: &crate::DenseNode<'_>,
    block: &PrimitiveBlock,
    seed: SourceSeed<'_>,
) -> Result<()> {
    ensure_node_capacity_local(bb, output)?;
    ensure_seeded(bb, output, block, seed)?;
    let meta = dense_node_raw_metadata(dn)?;
    // Block construction keeps every dense index a nonnegative int32, so the
    // cast is the identity.
    bb.add_node_raw(
        dn.id(),
        dn.decimicro_lat()?,
        dn.decimicro_lon()?,
        dn.raw_tags()
            .map(|(k, v)| (k.cast_unsigned(), v.cast_unsigned())),
        meta.as_ref(),
    )
}

pub(super) fn write_base_node_local<O: BlockSink + ?Sized>(
    bb: &mut BlockBuilder,
    output: &mut O,
    node: &crate::Node<'_>,
    block: &PrimitiveBlock,
    seed: SourceSeed<'_>,
) -> Result<()> {
    // The coordinate accessors scale exactly and refuse a value that does
    // not fit an i32 decimicrodegree, as write_base_dense_node_local does: a
    // re-encoded base node is never written at a wrapped position. A blob
    // spliced through untouched is not decoded and so not judged here.
    ensure_node_capacity_local(bb, output)?;
    ensure_seeded(bb, output, block, seed)?;
    // Partial source metadata keeps its field presence (a plain node out);
    // complete or absent metadata goes dense, as before.
    let meta = element_raw_partial_metadata(crate::ElementKind::Node, node.id(), &node.info())?;
    bb.add_node_raw_partial(
        node.id(),
        node.decimicro_lat()?,
        node.decimicro_lon()?,
        node.raw_tags(),
        meta.as_ref(),
    )
}

/// Write a surviving base node with no metadata as a plain `Node` without
/// `Info`, the overflow recovery's retry of a lone survivor that overflowed
/// dense (a plain-source node the rewrite would otherwise make a few bytes
/// larger than the block it came from). Only for a node
/// [`base_node_is_bare`] accepts; see `add_osc_node_bare_plain` for why a
/// node with metadata is not retried.
pub(super) fn write_base_node_bare_plain_local<O: BlockSink + ?Sized>(
    bb: &mut BlockBuilder,
    output: &mut O,
    element: &crate::Element<'_>,
    block: &PrimitiveBlock,
    seed: SourceSeed<'_>,
) -> Result<()> {
    if !base_node_is_bare(element)? {
        return Err(crate::error::Error::internal(
            "apply-changes: a plain retry was planned for an element that is not a bare node",
        ));
    }
    ensure_node_capacity_local(bb, output)?;
    ensure_seeded(bb, output, block, seed)?;
    match element {
        crate::Element::DenseNode(dn) => bb.add_node_raw_bare_plain(
            dn.id(),
            dn.decimicro_lat()?,
            dn.decimicro_lon()?,
            dn.raw_tags()
                .map(|(k, v)| (k.cast_unsigned(), v.cast_unsigned())),
        ),
        crate::Element::Node(n) => {
            bb.add_node_raw_bare_plain(n.id(), n.decimicro_lat()?, n.decimicro_lon()?, n.raw_tags())
        }
        crate::Element::Way(_) | crate::Element::Relation(_) => Err(crate::error::Error::internal(
            "apply-changes: a plain retry was planned for a way or relation",
        )),
    }
}

/// Whether surviving base `element` is a node the rewrite writes with no
/// metadata, so that a plain `Node` without `Info` claims what its dense
/// form does.
pub(super) fn base_node_is_bare(element: &crate::Element<'_>) -> Result<bool> {
    Ok(match element {
        crate::Element::DenseNode(dn) => dense_node_raw_metadata(dn)?.is_none(),
        crate::Element::Node(n) => {
            element_raw_partial_metadata(crate::ElementKind::Node, n.id(), &n.info())?
                .is_none_or(|m| m == crate::block_builder::RawPartialMetadata::default())
        }
        crate::Element::Way(_) | crate::Element::Relation(_) => false,
    })
}

/// What the raw writers of one source block's surviving base ways and
/// relations need beyond the element itself: the block's framing, which the
/// output block (always default-framed, see `BlockBuilder`) does not carry,
/// the OSC probe for refreshing inline coordinates, the worker's scratch,
/// and the refresh counts for the block.
pub(super) struct BaseWrites<'a> {
    framing: CoordFraming,
    date_granularity: i32,
    probe: &'a StaleProbe,
    scratch: &'a mut RefreshScratch,
    pub(super) tally: RefreshTally,
    /// The table a block of survivors starts from.
    pub(super) seed: SourceSeed<'a>,
}

/// Buffers the surviving-base-way writers and staleness detection reuse,
/// one set per worker.
#[derive(Default)]
pub(super) struct RefreshScratch {
    info: Vec<u8>,
    lats: Vec<u8>,
    lons: Vec<u8>,
    pub(super) hits: Vec<(usize, (i32, i32))>,
}

impl RefreshScratch {
    /// Clear every buffer for the next blob, releasing one a blob grew past
    /// the retention limit (`retention::release_scratch`). Each holds one way's
    /// worth, so on real data the limit never engages; it bounds a hostile
    /// way with a huge ref list.
    pub(super) fn release_outliers(&mut self) {
        use crate::retention::release_scratch;
        release_scratch(&mut self.info);
        release_scratch(&mut self.lats);
        release_scratch(&mut self.lons);
        release_scratch(&mut self.hits);
    }

    /// Bytes of capacity the buffers hold.
    pub(super) fn capacity_bytes(&self) -> usize {
        self.info.capacity()
            + self.lats.capacity()
            + self.lons.capacity()
            + self.hits.capacity() * std::mem::size_of::<(usize, (i32, i32))>()
    }
}

/// Coordinate-refresh counts for surviving base ways.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RefreshTally {
    /// Ways written with at least one coordinate changed to the OSC's.
    pub(super) ways: u64,
    /// Positions whose coordinate changed to the OSC's.
    pub(super) refs: u64,
    /// Positions naming a node the OSC deletes (coordinate kept).
    pub(super) deleted_refs: u64,
}

impl<'a> BaseWrites<'a> {
    pub(super) fn new(
        block: &PrimitiveBlock,
        probe: &'a StaleProbe,
        scratch: &'a mut RefreshScratch,
    ) -> Self {
        Self {
            framing: CoordFraming::of_block(block),
            date_granularity: block.date_granularity(),
            probe,
            scratch,
            tally: RefreshTally::default(),
            seed: SourceSeed::Whole,
        }
    }
}

/// A raw `Info` sub-message as the default-framed output block must carry it.
///
/// `Info.timestamp` is in units of the block's `date_granularity`
/// milliseconds, and an output block declares the default 1000. Under the
/// default the bytes are copied as they are. Otherwise the timestamp is
/// rewritten by `owned::output_timestamp_seconds`, the conversion the base
/// nodes of the same rewrite get (rounded down to whole seconds, an
/// overflowing scaling refused), so one rewritten blob keeps one
/// whole-second contract. Only the effective timestamp is converted: the
/// parser keeps the last varint field 2, so earlier ones are shadowed and
/// dropped rather than judged, and the converted value is written after the
/// other fields. Every other field keeps its bytes, unknown fields
/// included; a timestamp field that is absent stays absent, and one not
/// varint-typed is copied like an unknown field, as the parser treats it.
fn output_info<'b>(
    kind: crate::ElementKind,
    id: i64,
    info: Option<&'b [u8]>,
    date_granularity: i32,
    buf: &'b mut Vec<u8>,
) -> Result<Option<&'b [u8]>> {
    use crate::read::wire::{Cursor, WIRE_VARINT};
    use protohoggr::{encode_tag, encode_varint};

    let Some(info) = info else {
        return Ok(None);
    };
    if i64::from(date_granularity) == crate::owned::OUTPUT_DATE_GRANULARITY {
        return Ok(Some(info));
    }
    buf.clear();
    let mut timestamp = None;
    let mut cursor = Cursor::new(info);
    while let Some((field, wire_type)) = cursor.read_tag()? {
        if field == 2 && wire_type == WIRE_VARINT {
            timestamp = Some(cursor.read_varint_i64()?);
        } else {
            let bytes = cursor.read_raw_field(wire_type)?;
            encode_tag(buf, field, wire_type);
            buf.extend_from_slice(bytes);
        }
    }
    if let Some(raw) = timestamp {
        let seconds = crate::owned::output_timestamp_seconds(raw, i64::from(date_granularity))
            .map_err(|e| e.context(format!("{} {id}: timestamp", kind.as_str())))?;
        encode_tag(buf, 2, WIRE_VARINT);
        encode_varint(buf, seconds.cast_unsigned());
    }
    Ok(Some(buf))
}

pub(super) fn write_base_way_local<O: BlockSink + ?Sized>(
    bb: &mut BlockBuilder,
    output: &mut O,
    way: &crate::Way<'_>,
    block: &PrimitiveBlock,
    ctx: &mut BaseWrites<'_>,
) -> Result<()> {
    ensure_way_capacity_local(bb, output)?;
    ensure_seeded(bb, output, block, ctx.seed)?;
    let info = output_info(
        crate::ElementKind::Way,
        way.id(),
        way.info_data(),
        ctx.date_granularity,
        &mut ctx.scratch.info,
    )?;
    bb.add_way_raw_bytes(
        way.id(),
        way.keys_data(),
        way.vals_data(),
        way.refs_data(),
        info,
    )
}

/// Write a surviving base way under LocationsOnWays.
///
/// Its inline coordinates are refreshed by the rule in `refresh.rs`: each
/// position naming a node the OSC creates or modifies takes the OSC's
/// coordinate where it differs. Keys, values, refs and Info keep their raw
/// bytes (Info's timestamps rescaled by [`output_info`] under a non-default
/// `date_granularity`). The coordinate bytes are copied as they are when the
/// block's framing is the default and nothing changed; otherwise every
/// coordinate is re-encoded in decimicrodegrees under the default framing the
/// output block declares, since a raw copy would be read back rescaled.
pub(super) fn write_base_way_local_with_locations<O: BlockSink + ?Sized>(
    bb: &mut BlockBuilder,
    output: &mut O,
    way: &crate::Way<'_>,
    block: &PrimitiveBlock,
    ctx: &mut BaseWrites<'_>,
) -> Result<()> {
    ensure_way_capacity_local(bb, output)?;
    ensure_seeded(bb, output, block, ctx.seed)?;
    let BaseWrites {
        framing,
        date_granularity,
        probe,
        scratch,
        tally,
        ..
    } = ctx;
    let RefreshScratch {
        info,
        lats,
        lons,
        hits,
    } = &mut **scratch;
    let has_coords = !(way.lat_data().is_empty() && way.lon_data().is_empty());
    let mut reencode = has_coords && !framing.is_default();
    if !probe.is_empty() {
        let eval = evaluate_way(
            way.refs_data(),
            way.lat_data(),
            way.lon_data(),
            *framing,
            probe,
            hits,
        )?;
        tally.deleted_refs += eval.deleted_refs;
        reencode |= eval.stale_refs > 0;
    }
    let info = output_info(
        crate::ElementKind::Way,
        way.id(),
        way.info_data(),
        *date_granularity,
        info,
    )?;
    if reencode {
        let changed = refreshed_locations(
            way.refs_data(),
            way.lat_data(),
            way.lon_data(),
            *framing,
            probe,
            lats,
            lons,
        )?;
        if changed > 0 {
            tally.ways += 1;
            tally.refs += changed;
        }
        bb.add_way_raw_bytes_with_locations(
            way.id(),
            way.keys_data(),
            way.vals_data(),
            way.refs_data(),
            lats,
            lons,
            info,
        )
    } else {
        bb.add_way_raw_bytes_with_locations(
            way.id(),
            way.keys_data(),
            way.vals_data(),
            way.refs_data(),
            way.lat_data(),
            way.lon_data(),
            info,
        )
    }
}

/// Write an OSC way, with LocationsOnWays coordinates when `loc` requires them.
pub(super) fn write_osc_way_local<O: BlockSink + ?Sized>(
    bb: &mut BlockBuilder,
    output: &mut O,
    way: &crate::osc::CompactWayRef<'_>,
    loc: LocMode<'_>,
) -> Result<()> {
    let locs = loc.for_way(way.id())?;
    ensure_way_capacity_local(bb, output)?;
    add_osc_way(bb, way, locs);
    Ok(())
}

pub(super) fn write_base_relation_local<O: BlockSink + ?Sized>(
    bb: &mut BlockBuilder,
    output: &mut O,
    rel: &crate::Relation<'_>,
    block: &PrimitiveBlock,
    ctx: &mut BaseWrites<'_>,
) -> Result<()> {
    ensure_relation_capacity_local(bb, output)?;
    ensure_seeded(bb, output, block, ctx.seed)?;
    let info = output_info(
        crate::ElementKind::Relation,
        rel.id(),
        rel.info_data(),
        ctx.date_granularity,
        &mut ctx.scratch.info,
    )?;
    bb.add_relation_raw_bytes(
        rel.id(),
        rel.keys_data(),
        rel.vals_data(),
        rel.roles_sid_data(),
        rel.memids_data(),
        rel.types_data(),
        info,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::output_info;

    /// Only the effective (last) timestamp is converted: a shadowed one
    /// whose scaling would overflow is dropped, not judged, as the parser
    /// never reads it. An effective one that overflows is refused.
    #[test]
    fn output_info_converts_only_the_last_timestamp() {
        let max = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F];
        let mut info = vec![0x10];
        info.extend_from_slice(&max);
        info.extend_from_slice(&[0x08, 0x03, 0x10, 0x14]);
        let mut buf = Vec::new();
        let out = output_info(crate::ElementKind::Way, 9, Some(&info), 100, &mut buf)
            .unwrap()
            .unwrap();
        assert_eq!(out, [0x08, 0x03, 0x10, 0x02], "20 x 100 ms is 2 s");

        let mut effective = vec![0x08, 0x03, 0x10];
        effective.extend_from_slice(&max);
        let mut buf = Vec::new();
        let err =
            output_info(crate::ElementKind::Way, 9, Some(&effective), 100, &mut buf).unwrap_err();
        assert!(
            err.report().contains("way 9: timestamp"),
            "{}",
            err.report()
        );
    }
}
