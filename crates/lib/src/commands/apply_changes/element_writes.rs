//! Element-write helpers shared between OSC-diff emission and base-PBF
//! passthrough. The `*_local` helpers flush into the rewrite task's own
//! [`FramedBlocks`], which frames each block on the worker as it completes;
//! the `add_osc_*` encoders add one element and leave capacity to the
//! caller, as the drain's create runs (`stream_output.rs`) size every block
//! themselves.

use crate::PrimitiveBlock;
use crate::block_builder::{BlockBuilder, MemberData};
use crate::owned::{dense_node_raw_metadata, element_raw_partial_metadata};

use crate::commands::{
    FramedBlocks, ensure_node_capacity_local, ensure_relation_capacity_local,
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

/// Add an OSC way to `bb`, with inline locations when `locs` is present.
///
/// An id absent from `locs` gets `(0, 0)`. That is only reachable when the
/// run allows it: `rewrite::merge` counts unresolved references against the
/// merged map and fails the command unless `--ignore-missing-nodes` asked
/// for this fallback.
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

pub(super) fn write_base_dense_node_local(
    bb: &mut BlockBuilder,
    output: &mut FramedBlocks<'_>,
    dn: &crate::DenseNode<'_>,
    block: &PrimitiveBlock,
) -> Result<()> {
    ensure_node_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block)?;
    }
    let meta = dense_node_raw_metadata(dn);
    // Block construction keeps every dense index a nonnegative int32, so the
    // cast is the identity.
    bb.add_node_raw(
        dn.id(),
        dn.decimicro_lat(),
        dn.decimicro_lon(),
        dn.raw_tags()
            .map(|(k, v)| (k.cast_unsigned(), v.cast_unsigned())),
        meta.as_ref(),
    )
}

pub(super) fn write_base_node_local(
    bb: &mut BlockBuilder,
    output: &mut FramedBlocks<'_>,
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
        bb.pre_seed_string_table(block)?;
    }
    // Partial source metadata keeps its field presence (a plain node out);
    // complete or absent metadata goes dense, as before.
    let meta = element_raw_partial_metadata(&node.info());
    bb.add_node_raw_partial(
        node.id(),
        node.decimicro_lat(),
        node.decimicro_lon(),
        node.raw_tags(),
        meta.as_ref(),
    )
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
    /// the retention limit (`commands::release_scratch`). Each holds one way's
    /// worth, so on real data the limit never engages; it bounds a hostile
    /// way with a huge ref list.
    pub(super) fn release_outliers(&mut self) {
        use crate::commands::release_scratch;
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
        }
    }
}

/// The source block's `date_granularity` that `BlockBuilder`'s output blocks
/// declare (by omitting the field).
const DEFAULT_DATE_GRANULARITY: i32 = 1000;

/// A raw `Info` sub-message as the default-framed output block must carry it.
///
/// `Info.timestamp` is in units of the block's `date_granularity`
/// milliseconds, and an output block declares the default 1000. Under the
/// default the bytes are copied as they are. Otherwise every timestamp field
/// is rewritten as `raw * date_granularity / 1000` seconds (wrapping, as the
/// read accessor scales), the conversion the base nodes of the same rewrite
/// get, so one rewritten blob keeps one whole-second contract. Every other
/// field keeps its bytes, unknown fields included; a timestamp field that is
/// absent stays absent, and one not varint-typed is copied like an unknown
/// field, as the parser treats it.
fn output_info<'b>(
    info: Option<&'b [u8]>,
    date_granularity: i32,
    buf: &'b mut Vec<u8>,
) -> Result<Option<&'b [u8]>> {
    use crate::read::wire::{Cursor, WIRE_VARINT};
    use protohoggr::{encode_tag, encode_varint};

    let Some(info) = info else {
        return Ok(None);
    };
    if date_granularity == DEFAULT_DATE_GRANULARITY {
        return Ok(Some(info));
    }
    buf.clear();
    let mut cursor = Cursor::new(info);
    while let Some((field, wire_type)) = cursor.read_tag()? {
        if field == 2 && wire_type == WIRE_VARINT {
            let raw = cursor.read_varint_i64()?;
            let seconds = raw.wrapping_mul(i64::from(date_granularity)) / 1000;
            encode_tag(buf, 2, WIRE_VARINT);
            encode_varint(buf, seconds.cast_unsigned());
        } else {
            let bytes = cursor.read_raw_field(wire_type)?;
            encode_tag(buf, field, wire_type);
            buf.extend_from_slice(bytes);
        }
    }
    Ok(Some(buf))
}

pub(super) fn write_base_way_local(
    bb: &mut BlockBuilder,
    output: &mut FramedBlocks<'_>,
    way: &crate::Way<'_>,
    block: &PrimitiveBlock,
    ctx: &mut BaseWrites<'_>,
) -> Result<()> {
    ensure_way_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block)?;
    }
    let info = output_info(way.info_data(), ctx.date_granularity, &mut ctx.scratch.info)?;
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
pub(super) fn write_base_way_local_with_locations(
    bb: &mut BlockBuilder,
    output: &mut FramedBlocks<'_>,
    way: &crate::Way<'_>,
    block: &PrimitiveBlock,
    ctx: &mut BaseWrites<'_>,
) -> Result<()> {
    ensure_way_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block)?;
    }
    let BaseWrites {
        framing,
        date_granularity,
        probe,
        scratch,
        tally,
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
    let info = output_info(way.info_data(), *date_granularity, info)?;
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
pub(super) fn write_osc_way_local(
    bb: &mut BlockBuilder,
    output: &mut FramedBlocks<'_>,
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
    output: &mut FramedBlocks<'_>,
    rel: &crate::Relation<'_>,
    block: &PrimitiveBlock,
    ctx: &mut BaseWrites<'_>,
) -> Result<()> {
    ensure_relation_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block)?;
    }
    let info = output_info(rel.info_data(), ctx.date_granularity, &mut ctx.scratch.info)?;
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
