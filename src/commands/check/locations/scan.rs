//! Wire-level scanners for `check --locations`.
//!
//! Both scanners walk a decompressed `PrimitiveBlock` without building a
//! [`PrimitiveBlock`](crate::PrimitiveBlock): the check decodes every way
//! blob once per partition, so the parse is on its hot path. Each blob is
//! also validated once, the way the reader validates it, by
//! [`validate_block`]; the scanners themselves read only what the check
//! compares. They differ from the crate's other raw scanners
//! (`crate::scan::node`, `crate::scan::way`) in ways the check needs:
//!
//! - **Exact coordinates.** A coordinate is `offset + granularity * raw`
//!   nanodegrees, computed in `i128`, where the reader computes it in `i64`
//!   with wrapping arithmetic. A multiplication that overflows `i64` can be
//!   cancelled by the offset into a legal coordinate; wrapping reaches the
//!   exact value whenever it fits `i64` but turns a value past `i64` into
//!   another, so wrap alone cannot judge it. `crate::scan::node` is exact
//!   too (checked `i64`, `i128` on overflow) but bounds only the e7 range,
//!   not the geographic domain checked here. Delta accumulation of the raw
//!   values still wraps in `i64`, exactly as the reader's does, so the raw
//!   value is the one every reader decodes. Granularity is read as the reader
//!   reads it (any `int32`).
//! - **Domain, then e7.** The exact value is judged against the geographic
//!   domain in nanodegrees (latitude within +-90 degrees, longitude within
//!   +-180) and only then truncated to the decimicrodegree (e7) value the
//!   rest of the crate carries, with the same truncation toward zero as the
//!   reader's `decimicro_lat` / `decimicro_lon`. See [`Coord`].
//! - **Streaming.** A way's references and coordinates are decoded in
//!   lockstep and handed out one at a time, so a worker's memory does not grow
//!   with the length of a way, and a coordinate is only scaled when the
//!   caller asks for it.
//! - **Kinds.** The node scanner reports which element kinds the blob holds
//!   (the crate's `KindSet` rule: a dense message counts only with an ID), so
//!   the check routes blobs on what it decoded rather than on indexdata. The
//!   way scanner raises a kind contradiction for a blob claiming ways that
//!   holds elements of another kind: that is the one claim the check acts on
//!   before decoding, and the check stops on it.

use crate::ElementKind;
use crate::error::{Error, Result, new_wire_error};
use crate::read::wire::{Cursor, WIRE_LEN, WIRE_VARINT, WireDenseNodes, WireNode, WireWay};

/// Latitude bound in nanodegrees: 90 degrees.
const LAT_LIMIT_NANO: i128 = 90_000_000_000;
/// Longitude bound in nanodegrees: 180 degrees.
const LON_LIMIT_NANO: i128 = 180_000_000_000;

/// One decoded coordinate.
///
/// `Valid` carries the e7 value the comparison uses; `Invalid` the exact
/// nanodegree value, for the report. The comparison contract is e7 equality:
/// two coordinates that agree to the decimicrodegree compare equal even if
/// their exact nanodegree values differ, which only a block with a
/// granularity below 100 or an offset that is not a multiple of 100 can
/// produce. Every coordinate store in this crate, and the OSM data model,
/// is e7.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Coord {
    Valid { lat: i32, lon: i32 },
    Invalid { lat_nano: i128, lon_nano: i128 },
}

/// The framing a block's coordinates are decoded with.
#[derive(Clone, Copy, Debug)]
pub(super) struct Framing {
    granularity: i128,
    lat_offset: i128,
    lon_offset: i128,
}

impl Framing {
    pub(super) fn coord(self, raw_lat: i64, raw_lon: i64) -> Coord {
        let lat_nano = self.lat_offset + self.granularity * i128::from(raw_lat);
        let lon_nano = self.lon_offset + self.granularity * i128::from(raw_lon);
        if (-LAT_LIMIT_NANO..=LAT_LIMIT_NANO).contains(&lat_nano)
            && (-LON_LIMIT_NANO..=LON_LIMIT_NANO).contains(&lon_nano)
        {
            // In domain, so both quotients fit i32 (at most 1.8e9).
            #[allow(clippy::cast_possible_truncation)]
            Coord::Valid {
                lat: (lat_nano / 100) as i32,
                lon: (lon_nano / 100) as i32,
            }
        } else {
            Coord::Invalid { lat_nano, lon_nano }
        }
    }
}

/// The element kinds a block holds: the crate's decoded-kind set, recorded
/// here by the raw scanners under the same rule.
pub(super) type Kinds = crate::element_kind::KindSet;

/// Reusable string-table and group-range scratch for [`validate_block`].
pub(super) type ValidateScratch = (Vec<(u32, u32)>, Vec<(u32, u32)>);

/// Validate a block exactly as the reader does when it builds a
/// [`PrimitiveBlock`](crate::PrimitiveBlock): group and element framing,
/// tag, Info and DenseInfo arrays, dense `keys_vals`, way geometry and
/// relation member arrays, string table UTF-8. The check calls it once per
/// blob, so a file the reader refuses is refused here too.
pub(super) fn validate_block(block: &[u8], scratch: &mut ValidateScratch) -> Result<()> {
    let (st, gr) = scratch;
    crate::PrimitiveBlock::from_vec_with_scratch(block.to_vec(), st, gr).map(drop)
}

/// Caller-owned scratch for a block's group positions, `(offset, len)` in
/// the block, reused across blobs.
pub(super) type Groups = Vec<(usize, usize)>;

/// Parse the block's top level: its groups and its coordinate framing.
fn block_top(block: &[u8], groups: &mut Groups) -> Result<Framing> {
    let mut cursor = Cursor::new(block);
    let mut granularity: i32 = 100;
    let mut lat_offset: i64 = 0;
    let mut lon_offset: i64 = 0;
    groups.clear();
    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                groups.push((data.as_ptr() as usize - block.as_ptr() as usize, data.len()));
            }
            (17, WIRE_VARINT) => {
                granularity = crate::read::wire::read_int32(
                    &mut cursor,
                    "PrimitiveBlock.granularity exceeds int32",
                )?;
            }
            (19, WIRE_VARINT) => lat_offset = cursor.read_varint_i64()?,
            (20, WIRE_VARINT) => lon_offset = cursor.read_varint_i64()?,
            _ => cursor.skip_field(wire_type)?,
        }
    }
    Ok(Framing {
        granularity: i128::from(granularity),
        lat_offset: i128::from(lat_offset),
        lon_offset: i128::from(lon_offset),
    })
}

/// Walk one group's fields to the end, propagating every framing error,
/// and call `element` for each element field with its kind and bytes (`None`
/// marks the dense message, refused when repeated as the decoder refuses it).
fn walk_group<'g>(
    group: &'g [u8],
    mut element: impl FnMut(ElementKind, Option<&'g [u8]>, &'g [u8]) -> Result<()>,
) -> Result<Kinds> {
    let mut kinds = Kinds::default();
    let mut seen_dense = false;
    let mut cursor = Cursor::new(group);
    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                kinds.add(ElementKind::Node);
                element(ElementKind::Node, Some(data), data)?;
            }
            (2, WIRE_LEN) => {
                if seen_dense {
                    return Err(new_wire_error(crate::read::wire::DUP_GROUP_DENSE));
                }
                seen_dense = true;
                let data = cursor.read_len_delimited()?;
                // A dense message holds nodes only when it carries an ID, as
                // the decoder's kind set counts it: an empty one is no node.
                if !WireDenseNodes::parse(data)?.id_data.is_empty() {
                    kinds.add(ElementKind::Node);
                }
                element(ElementKind::Node, None, data)?;
            }
            (3, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                kinds.add(ElementKind::Way);
                element(ElementKind::Way, Some(data), data)?;
            }
            (4, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                kinds.add(ElementKind::Relation);
                element(ElementKind::Relation, Some(data), data)?;
            }
            _ => cursor.skip_field(wire_type)?,
        }
    }
    Ok(kinds)
}

#[cold]
#[inline(never)]
fn dense_err() -> Error {
    new_wire_error(
        "dense group id/lat/lon arrays hold a truncated or overlong varint, \
         or are of unequal length",
    )
}

/// Scan every node of a blob, calling `sink(element_ordinal, id, coord)`,
/// and return the element kinds its groups hold.
///
/// Within a group the dense nodes come first, as in the element iterator's
/// order; element ordinals count nodes within the blob from 0.
pub(super) fn scan_nodes(
    block: &[u8],
    groups: &mut Groups,
    mut sink: impl FnMut(u64, i64, Coord) -> Result<()>,
) -> Result<Kinds> {
    use protohoggr::zigzag_decode_64;

    let framing = block_top(block, groups)?;
    let mut ordinal: u64 = 0;
    let mut kinds = Kinds::default();
    for &(off, len) in groups.iter() {
        let group = &block[off..off + len];
        // Two walks of the group, so the dense nodes come first without
        // buffering the plain ones: the first finds the dense message and
        // the kinds, the second streams the plain nodes.
        let mut dense: Option<&[u8]> = None;
        let group_kinds = walk_group(group, |kind, plain_node, data| {
            if kind == ElementKind::Node && plain_node.is_none() {
                dense = Some(data);
            }
            Ok(())
        })?;
        kinds = kinds.union(group_kinds);
        if let Some(dense) = dense {
            let dense = WireDenseNodes::parse(dense)?;
            let mut ids = Cursor::new(dense.id_data);
            let mut lats = Cursor::new(dense.lat_data);
            let mut lons = Cursor::new(dense.lon_data);
            let (mut id, mut lat, mut lon) = (0_i64, 0_i64, 0_i64);
            while !ids.is_empty() {
                let did = ids.read_varint().map_err(|_| dense_err())?;
                let dlat = lats.read_varint().map_err(|_| dense_err())?;
                let dlon = lons.read_varint().map_err(|_| dense_err())?;
                id = id.wrapping_add(zigzag_decode_64(did));
                lat = lat.wrapping_add(zigzag_decode_64(dlat));
                lon = lon.wrapping_add(zigzag_decode_64(dlon));
                sink(ordinal, id, framing.coord(lat, lon))?;
                ordinal += 1;
            }
            if !lats.is_empty() || !lons.is_empty() {
                return Err(dense_err());
            }
        }
        walk_group(group, |kind, plain_node, _| {
            if let (ElementKind::Node, Some(data)) = (kind, plain_node) {
                let node = WireNode::parse(data)?;
                sink(ordinal, node.id, framing.coord(node.lat, node.lon))?;
                ordinal += 1;
            }
            Ok(())
        })?;
    }
    Ok(kinds)
}

/// What [`scan_ways`] hands its sink.
pub(super) enum WayEvent {
    /// A way begins. `has_refs` and `has_coords` say whether its `refs` and
    /// coordinate arrays are non-empty; its references follow as `Ref`s.
    Way {
        /// Ordinal of the way within its blob, from 0.
        ordinal: u64,
        id: i64,
        has_refs: bool,
        has_coords: bool,
    },
    /// One reference of the current way.
    Ref {
        position: u64,
        node_id: i64,
        /// The raw (unscaled) inline coordinate; `None` when the way carries
        /// no coordinate arrays.
        raw: Option<(i64, i64)>,
        framing: Framing,
    },
}

impl WayEvent {
    /// A `Ref`'s inline coordinate, scaled; `None` for a way without
    /// coordinate arrays (and for a `Way` event).
    pub(super) fn coord(&self) -> Option<Coord> {
        match *self {
            WayEvent::Ref {
                raw: Some((lat, lon)),
                framing,
                ..
            } => Some(framing.coord(lat, lon)),
            _ => None,
        }
    }
}

#[cold]
#[inline(never)]
fn refs_err() -> Error {
    new_wire_error("way refs array holds a truncated or overlong varint")
}

#[cold]
#[inline(never)]
fn way_coords_err() -> Error {
    new_wire_error(
        "way lat/lon arrays hold a truncated or overlong varint, or disagree \
         with the node-reference count",
    )
}

/// Read one delta and fold it into `acc` with `i64` wrapping, as the reader
/// accumulates.
fn next_delta(cursor: &mut Cursor<'_>, acc: &mut i64, err: fn() -> Error) -> Result<i64> {
    let raw = cursor.read_varint().map_err(|_| err())?;
    *acc = acc.wrapping_add(protohoggr::zigzag_decode_64(raw));
    Ok(*acc)
}

/// Scan every way of a blob, streaming one `Way` event and then one `Ref`
/// event per reference.
///
/// Coordinate arrays are optional as a pair, as the decoder has them
/// (`validate_way_geometry_arrays` in `read/block.rs`): absent together they
/// mean the way carries no locations; present, both must run parallel to
/// `refs`, or the blob is a decode error. Lengths are judged while
/// streaming, so events before the fault have already been delivered when
/// the error returns; every caller abandons the run on it. A blob claiming
/// ways that holds an element of another kind is a kind contradiction,
/// raised once the whole block is walked.
pub(super) fn scan_ways(
    block: &[u8],
    claim: Option<ElementKind>,
    groups: &mut Groups,
    mut sink: impl FnMut(&WayEvent) -> Result<()>,
) -> Result<()> {
    let framing = block_top(block, groups)?;
    let mut ordinal: u64 = 0;
    let mut block_kinds = Kinds::default();
    for &(off, len) in groups.iter() {
        let group = &block[off..off + len];
        let kinds = walk_group(group, |kind, way, _| {
            let (ElementKind::Way, Some(data)) = (kind, way) else {
                return Ok(());
            };
            let way = WireWay::parse(data)?;
            let has_coords = !way.lat_data.is_empty() || !way.lon_data.is_empty();
            sink(&WayEvent::Way {
                ordinal,
                id: way.id,
                has_refs: !way.refs_data.is_empty(),
                has_coords,
            })?;
            let mut refs = Cursor::new(way.refs_data);
            let mut lats = Cursor::new(way.lat_data);
            let mut lons = Cursor::new(way.lon_data);
            let (mut node_id, mut lat, mut lon) = (0_i64, 0_i64, 0_i64);
            let mut position: u64 = 0;
            while !refs.is_empty() {
                let node = next_delta(&mut refs, &mut node_id, refs_err)?;
                let raw = if has_coords {
                    Some((
                        next_delta(&mut lats, &mut lat, way_coords_err)?,
                        next_delta(&mut lons, &mut lon, way_coords_err)?,
                    ))
                } else {
                    None
                };
                sink(&WayEvent::Ref {
                    position,
                    node_id: node,
                    raw,
                    framing,
                })?;
                position += 1;
            }
            if !lats.is_empty() || !lons.is_empty() {
                return Err(way_coords_err());
            }
            ordinal += 1;
            Ok(())
        })?;
        block_kinds = block_kinds.union(kinds);
    }
    // The location check reads a blob claiming ways only in its way passes,
    // so another kind in it is one it never reads. Judged on the whole
    // block's kinds, so the witness follows the fixed kind priority rather
    // than group order, as every decoding check names it.
    if claim == Some(ElementKind::Way)
        && let Some(found) = block_kinds.foreign_to(ElementKind::Way)
    {
        return Err(crate::error::new_kind_contradiction(
            ElementKind::Way,
            found,
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Coord, Framing};

    fn framing(granularity: i128, lat_offset: i128, lon_offset: i128) -> Framing {
        Framing {
            granularity,
            lat_offset,
            lon_offset,
        }
    }

    #[test]
    fn default_framing_yields_the_raw_value_as_e7() {
        assert_eq!(
            framing(100, 0, 0).coord(550_000_000, -120_000_000),
            Coord::Valid {
                lat: 550_000_000,
                lon: -120_000_000
            }
        );
    }

    /// The case `i64` arithmetic gets wrong: the product overflows `i64`,
    /// the offset brings it back into the domain, and checked `i64`
    /// arithmetic would refuse a legal coordinate.
    #[test]
    fn an_offset_that_cancels_an_overflowing_product_is_a_valid_coordinate() {
        let f = framing(100, i128::from(i64::MIN), 0);
        let raw = 92_233_720_368_547_759_i64;
        assert!(raw.checked_mul(100).is_none(), "the product overflows i64");
        assert_eq!(f.coord(raw, 0), Coord::Valid { lat: 0, lon: 0 });
    }

    /// The domain is judged on the exact nanodegree value: 90 degrees plus
    /// one nanodegree is outside it even though its e7 truncation is not.
    #[test]
    fn the_domain_is_judged_before_truncation() {
        let f = framing(1, 0, 0);
        assert_eq!(
            f.coord(90_000_000_000, 0),
            Coord::Valid {
                lat: 900_000_000,
                lon: 0
            }
        );
        assert!(matches!(f.coord(90_000_000_001, 0), Coord::Invalid { .. }));
        // osmium's undefined-location sentinel decodes outside the domain.
        assert!(matches!(
            framing(100, 0, 0).coord(i64::from(i32::MAX), i64::from(i32::MAX)),
            Coord::Invalid { .. }
        ));
    }

    /// Sub-e7 differences truncate toward zero, as the reader's
    /// `decimicro_lat` does.
    #[test]
    fn e7_truncates_toward_zero() {
        let f = framing(1, 0, 0);
        assert_eq!(f.coord(199, -199), Coord::Valid { lat: 1, lon: -1 });
    }
}
