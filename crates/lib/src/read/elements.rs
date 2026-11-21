//! Nodes, ways and relations

use super::block::{resolved_tag, str_from_stringtable};
use super::dense::DenseNode;
use super::wire::{
    PackedInt32Iter, PackedSint64Iter, PackedUint32Iter, WireBlock, WireInfo, WireNode,
    WireRelation, WireWay,
};
use crate::error::Result;

/// Generates the coordinate accessors of `Node`, `DenseNode` and
/// `WayNodeLocation` from each type's stored parts.
///
/// A PBF coordinate is `offset + granularity * raw` nanodegrees, where `raw`
/// is the stored value (for dense nodes and way locations, the delta sum,
/// which wraps modulo 2^64 like every reader's; see "What is deliberately not
/// checked" on block construction). The ordinary accessors scale that raw
/// value exactly and refuse a result their type cannot hold, so corrupt
/// framing is an error rather than a plausible coordinate elsewhere on Earth:
///   - `nano_lat()` / `nano_lon()` -> `Result<i64>` nanodegrees
///   - `decimicro_lat()` / `decimicro_lon()` -> `Result<i32>` decimicrodegrees,
///     truncated toward zero
///   - `nano_lat_exact()` / `nano_lon_exact()` -> `i128`, never fails
///   - `lat()` / `lon()` -> `f64` degrees from the exact value, never wraps
///
/// Only the representation is bounded, not the geographic domain.
///
/// The crate-private `*_wrapping` forms keep the old modular scaling and
/// `as i32` narrowing for the one consumer documented to report what that
/// walk reports (`inspect --nodes` and its extended bbox).
///
/// Each type defines `lat_parts(&self)` / `lon_parts(&self)` returning
/// `(offset, granularity, raw)`. `$e7_err` and `$nano_err` are the refusals.
///
/// This macro is an internal implementation detail. It is `#[macro_export]`ed only
/// because `macro_rules!` requires this for cross-module use within the crate
/// (`dense.rs` uses it for `DenseNode`). It is not intended as part of the public API.
#[doc(hidden)]
#[macro_export]
macro_rules! impl_coordinate_conversions {
    ($e7_err:path, $nano_err:path) => {
        /// The latitude in nanodegrees (10^-9), scaled exactly; an error when
        /// it does not fit `i64` (corrupt granularity, offset or deltas).
        #[inline]
        pub fn nano_lat(&self) -> $crate::Result<i64> {
            let (offset, granularity, raw) = self.lat_parts();
            $crate::scan::node::scale_to_nano(offset, granularity, raw).ok_or_else($nano_err)
        }

        /// The longitude counterpart of [`nano_lat`](Self::nano_lat).
        #[inline]
        pub fn nano_lon(&self) -> $crate::Result<i64> {
            let (offset, granularity, raw) = self.lon_parts();
            $crate::scan::node::scale_to_nano(offset, granularity, raw).ok_or_else($nano_err)
        }

        /// The exact latitude in nanodegrees (10^-9), computed in `i128` so it
        /// never wraps.
        #[inline]
        pub fn nano_lat_exact(&self) -> i128 {
            let (offset, granularity, raw) = self.lat_parts();
            $crate::scan::node::exact_nano(offset, granularity, raw)
        }

        /// The exact longitude in nanodegrees (10^-9), computed in `i128`.
        #[inline]
        pub fn nano_lon_exact(&self) -> i128 {
            let (offset, granularity, raw) = self.lon_parts();
            $crate::scan::node::exact_nano(offset, granularity, raw)
        }

        /// The latitude in decimicrodegrees (10^-7), truncated toward zero
        /// from the exact value; an error when it does not fit `i32`.
        #[inline]
        pub fn decimicro_lat(&self) -> $crate::Result<i32> {
            let (offset, granularity, raw) = self.lat_parts();
            $crate::scan::node::scale_to_decimicro(offset, granularity, raw).ok_or_else($e7_err)
        }

        /// The longitude counterpart of [`decimicro_lat`](Self::decimicro_lat).
        #[inline]
        pub fn decimicro_lon(&self) -> $crate::Result<i32> {
            let (offset, granularity, raw) = self.lon_parts();
            $crate::scan::node::scale_to_decimicro(offset, granularity, raw).ok_or_else($e7_err)
        }

        /// The latitude in degrees, from the exact value. Never wraps; equal
        /// to `1e-9 * nano_lat()` wherever that fits `i64`.
        #[inline]
        pub fn lat(&self) -> f64 {
            $crate::scan::node::nano_to_degrees(self.lat_parts())
        }

        /// The longitude in degrees, from the exact value.
        #[inline]
        pub fn lon(&self) -> f64 {
            $crate::scan::node::nano_to_degrees(self.lon_parts())
        }

        /// The modular `i64` latitude every pre-checked reader returned.
        /// Only for `inspect`, which reports what that walk reports.
        #[inline]
        #[allow(dead_code)]
        pub(crate) fn nano_lat_wrapping(&self) -> i64 {
            let (offset, granularity, raw) = self.lat_parts();
            offset.wrapping_add(granularity.wrapping_mul(raw))
        }

        /// The longitude counterpart of [`nano_lat_wrapping`](Self::nano_lat_wrapping).
        #[inline]
        #[allow(dead_code)]
        pub(crate) fn nano_lon_wrapping(&self) -> i64 {
            let (offset, granularity, raw) = self.lon_parts();
            offset.wrapping_add(granularity.wrapping_mul(raw))
        }

        /// The modular latitude divided by 100 and narrowed with `as i32`.
        /// Only for `inspect --nodes`.
        #[inline]
        #[allow(dead_code, clippy::cast_possible_truncation)]
        pub(crate) fn decimicro_lat_wrapping(&self) -> i32 {
            (self.nano_lat_wrapping() / 100) as i32
        }

        /// The longitude counterpart of
        /// [`decimicro_lat_wrapping`](Self::decimicro_lat_wrapping).
        #[inline]
        #[allow(dead_code, clippy::cast_possible_truncation)]
        pub(crate) fn decimicro_lon_wrapping(&self) -> i32 {
            (self.nano_lon_wrapping() / 100) as i32
        }
    };
}

/// Decode an element's `Info` sub-message for the `info()` accessors.
///
/// Absent metadata reads as all-`None` ([`WireInfo::default`]). Present
/// metadata cannot fail to parse here, and this relies on that rather than
/// guessing: `Node`, `Way` and `Relation` are built only by the block
/// iterators in `block.rs`, from element bytes of a block that
/// was constructed, and construction runs `validate_block` first
/// (`BlockBuffer::project`). That pass parses every element with the
/// same `WireNode` / `WireWay` / `WireRelation::parse` the iterators use, on
/// the same immutable bytes, and runs `validate_info` - exactly
/// `WireInfo::parse` - on the slice that parse retains. The element parsers
/// refuse a second data-bearing `Info` (`take_message` in `wire.rs`), so that
/// slice is the element's only metadata.
///
/// A parse failure here is therefore a pbfhogg bug (an element built from
/// bytes the validation pass never saw), not hostile input, and it panics
/// rather than fall back to default metadata: the default is indistinguishable
/// from absent metadata, so a silent fallback would hand version 0 /
/// timestamp 0 to `time-filter`, the owned conversions and the OSC writers
/// with no signal. The policy and its panic live in `validated` in `block.rs`,
/// shared with the block iterators.
#[inline]
#[track_caller]
fn validated_info(info_data: Option<&[u8]>) -> WireInfo {
    match info_data {
        None => WireInfo::default(),
        Some(data) => super::block::validated(WireInfo::parse(data), "element Info"),
    }
}

/// An enum with the OSM core elements: nodes, ways and relations.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Element<'a> {
    /// A node. Also, see [`DenseNode`](Self::DenseNode).
    Node(Node<'a>),

    /// Just like [`Node`](Self::Node), but with a different representation in memory. This distinction is
    /// usually not important but is not abstracted away to avoid copying. So, if you want to match
    /// `Node`, you also likely want to match [`DenseNode`].
    DenseNode(DenseNode<'a>),

    /// A way.
    Way(Way<'a>),

    /// A relation.
    Relation(Relation<'a>),
}

/// An OpenStreetMap node element (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Node)).
#[derive(Clone, Debug)]
pub struct Node<'a> {
    block: &'a WireBlock<'static>,
    node: WireNode<'a>,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}

impl<'a> Node<'a> {
    pub(super) fn new(block: &'a WireBlock<'static>, node: WireNode<'a>) -> Node<'a> {
        Node {
            block,
            node,
            granularity: i64::from(block.granularity),
            lat_offset: block.lat_offset,
            lon_offset: block.lon_offset,
        }
    }

    /// Returns the node id.
    #[inline]
    pub fn id(&self) -> i64 {
        self.node.id
    }

    /// Returns an iterator over the tags of this node.
    pub fn tags(&self) -> TagIter<'a> {
        TagIter {
            block: self.block,
            key_indices: PackedUint32Iter::new(self.node.keys_data),
            val_indices: PackedUint32Iter::new(self.node.vals_data),
        }
    }

    /// Returns additional metadata for this element.
    ///
    /// Metadata the element does not carry reads as `None` fields. The bytes
    /// were validated when the block was constructed, so a corrupt `Info`
    /// never reaches this accessor.
    ///
    /// # Panics
    ///
    /// Only on a broken internal invariant: an `Info` that fails to parse
    /// although block construction validated it. That is a pbfhogg bug, not a
    /// property of the input.
    pub fn info(&self) -> Info<'a> {
        Info::new(self.block, validated_info(self.node.info_data))
    }

    #[inline]
    fn lat_parts(&self) -> (i64, i64, i64) {
        (self.lat_offset, self.granularity, self.node.lat)
    }

    #[inline]
    fn lon_parts(&self) -> (i64, i64, i64) {
        (self.lon_offset, self.granularity, self.node.lon)
    }

    impl_coordinate_conversions!(
        crate::scan::node::unrepresentable_coordinate_err,
        crate::scan::node::unrepresentable_nano_err
    );

    /// Returns an iterator over the tags of this node as raw index pairs.
    pub fn raw_tags(&self) -> RawTagIter<'a> {
        RawTagIter {
            key_indices: PackedUint32Iter::new(self.node.keys_data),
            val_indices: PackedUint32Iter::new(self.node.vals_data),
        }
    }
}

/// An OpenStreetMap way element (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Way)).
///
/// A way contains an ordered list of node references that can be accessed with the `refs`
/// method.
#[derive(Clone, Debug)]
pub struct Way<'a> {
    block: &'a WireBlock<'static>,
    way: WireWay<'a>,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}

impl<'a> Way<'a> {
    pub(super) fn new(block: &'a WireBlock<'static>, way: WireWay<'a>) -> Way<'a> {
        Way {
            block,
            way,
            granularity: i64::from(block.granularity),
            lat_offset: block.lat_offset,
            lon_offset: block.lon_offset,
        }
    }

    /// Returns the way id.
    #[inline]
    pub fn id(&self) -> i64 {
        self.way.id
    }

    /// Returns an iterator over the tags of this way.
    pub fn tags(&self) -> TagIter<'a> {
        TagIter {
            block: self.block,
            key_indices: PackedUint32Iter::new(self.way.keys_data),
            val_indices: PackedUint32Iter::new(self.way.vals_data),
        }
    }

    /// Returns additional metadata for this element.
    ///
    /// Metadata the element does not carry reads as `None` fields. The bytes
    /// were validated when the block was constructed, so a corrupt `Info`
    /// never reaches this accessor.
    ///
    /// # Panics
    ///
    /// Only on a broken internal invariant: an `Info` that fails to parse
    /// although block construction validated it. That is a pbfhogg bug, not a
    /// property of the input.
    pub fn info(&self) -> Info<'a> {
        Info::new(self.block, validated_info(self.way.info_data))
    }

    /// Returns an iterator over the references of this way. Each reference should correspond to a
    /// node id.
    pub fn refs(&self) -> WayRefIter<'a> {
        WayRefIter::delta_decoded(self.way.refs_data)
    }

    /// Returns an iterator over the way's node locations (latitude, longitude).
    /// Only available if the optional `LocationsOnWays` feature is included in the
    /// [`HeaderBlock`](crate::block::HeaderBlock) and should return an empty iterator otherwise.
    pub fn node_locations(&self) -> WayNodeLocationsIter<'a> {
        WayNodeLocationsIter {
            dlats: PackedSint64Iter::new(self.way.lat_data),
            dlons: PackedSint64Iter::new(self.way.lon_data),
            clat: 0,
            clon: 0,
            granularity: self.granularity,
            lat_offset: self.lat_offset,
            lon_offset: self.lon_offset,
        }
    }

    /// Raw field-20 shared-node pin bitmap, if present.
    pub fn shared_node_pins(&self) -> Option<&'a [u8]> {
        self.way.pins_data
    }

    /// Returns an iterator over the tags of this way as raw index pairs.
    pub fn raw_tags(&self) -> RawTagIter<'a> {
        RawTagIter {
            key_indices: PackedUint32Iter::new(self.way.keys_data),
            val_indices: PackedUint32Iter::new(self.way.vals_data),
        }
    }

    /// Raw packed uint32 bytes for tag key string table indices.
    pub(crate) fn keys_data(&self) -> &[u8] {
        self.way.keys_data
    }

    /// Raw packed uint32 bytes for tag value string table indices.
    pub(crate) fn vals_data(&self) -> &[u8] {
        self.way.vals_data
    }

    /// Raw packed sint64 delta-encoded bytes for node refs.
    pub(crate) fn refs_data(&self) -> &[u8] {
        self.way.refs_data
    }

    /// Raw Info submessage bytes (if present).
    pub(crate) fn info_data(&self) -> Option<&[u8]> {
        self.way.info_data
    }

    /// Raw packed sint64 delta-encoded bytes for node latitudes (LocationsOnWays).
    pub(crate) fn lat_data(&self) -> &[u8] {
        self.way.lat_data
    }

    /// Raw packed sint64 delta-encoded bytes for node longitudes (LocationsOnWays).
    pub(crate) fn lon_data(&self) -> &[u8] {
        self.way.lon_data
    }
}

/// An OpenStreetMap relation element (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Relation)).
///
/// A relation contains an ordered list of members that can be of any element type.
#[derive(Clone, Debug)]
pub struct Relation<'a> {
    block: &'a WireBlock<'static>,
    rel: WireRelation<'a>,
}

/// Name the relation, the member position and the string index on a failed
/// role lookup.
#[cold]
#[inline(never)]
fn bad_role(err: crate::Error, id: i64, position: usize, sid: i32) -> crate::Error {
    err.context(format!(
        "relation {id} member {position}: role string index {sid}"
    ))
}

impl<'a> Relation<'a> {
    pub(super) fn new(block: &'a WireBlock<'static>, rel: WireRelation<'a>) -> Relation<'a> {
        Relation { block, rel }
    }

    /// Returns the relation id.
    #[inline]
    pub fn id(&self) -> i64 {
        self.rel.id
    }

    /// Returns an iterator over the tags of this relation.
    pub fn tags(&self) -> TagIter<'a> {
        TagIter {
            block: self.block,
            key_indices: PackedUint32Iter::new(self.rel.keys_data),
            val_indices: PackedUint32Iter::new(self.rel.vals_data),
        }
    }

    /// Returns additional metadata for this element.
    ///
    /// Metadata the element does not carry reads as `None` fields. The bytes
    /// were validated when the block was constructed, so a corrupt `Info`
    /// never reaches this accessor.
    ///
    /// # Panics
    ///
    /// Only on a broken internal invariant: an `Info` that fails to parse
    /// although block construction validated it. That is a pbfhogg bug, not a
    /// property of the input.
    pub fn info(&self) -> Info<'a> {
        Info::new(self.block, validated_info(self.rel.info_data))
    }

    /// Returns an iterator over the members of this relation.
    pub fn members(&self) -> RelMemberIter<'a> {
        RelMemberIter::new(self.block, &self.rel)
    }

    /// Resolve every member of this relation into `buf`, in member order,
    /// ready for [`BlockBuilder::add_relation`](crate::block_builder::BlockBuilder::add_relation).
    /// `buf` is cleared first, so one buffer can be reused across the
    /// relations of a block.
    ///
    /// # Errors
    ///
    /// A member whose role string index is negative or past the block's
    /// string table is an error naming the relation, the member's position
    /// and the index - never an empty role, which would re-encode the
    /// relation silently changed. On an error `buf` holds the members before
    /// the bad one.
    pub fn members_into(&self, buf: &mut Vec<crate::block_builder::MemberData<'a>>) -> Result<()> {
        buf.clear();
        for (position, m) in self.members().enumerate() {
            let role = self.member_role(position, &m)?;
            buf.push(crate::block_builder::MemberData { id: m.id, role });
        }
        Ok(())
    }

    /// The role of member `m` at `position`, a failed lookup carrying the
    /// relation id, the position and the string index.
    pub(crate) fn member_role(&self, position: usize, m: &RelMember<'a>) -> Result<&'a str> {
        m.role()
            .map_err(|e| bad_role(e, self.id(), position, m.role_sid))
    }

    /// Returns an iterator over the tags of this relation as raw index pairs.
    pub fn raw_tags(&self) -> RawTagIter<'a> {
        RawTagIter {
            key_indices: PackedUint32Iter::new(self.rel.keys_data),
            val_indices: PackedUint32Iter::new(self.rel.vals_data),
        }
    }

    /// Raw packed uint32 bytes for tag key string table indices.
    pub(crate) fn keys_data(&self) -> &[u8] {
        self.rel.keys_data
    }

    /// Raw packed uint32 bytes for tag value string table indices.
    pub(crate) fn vals_data(&self) -> &[u8] {
        self.rel.vals_data
    }

    /// Raw packed int32 bytes for member role string table indices.
    pub(crate) fn roles_sid_data(&self) -> &[u8] {
        self.rel.roles_sid_data
    }

    /// Raw packed sint64 delta-encoded bytes for member IDs.
    pub(crate) fn memids_data(&self) -> &[u8] {
        self.rel.memids_data
    }

    /// Raw packed int32 bytes for member types.
    pub(crate) fn types_data(&self) -> &[u8] {
        self.rel.types_data
    }

    /// Raw Info submessage bytes (if present).
    pub(crate) fn info_data(&self) -> Option<&[u8]> {
        self.rel.info_data
    }
}

/// An iterator over the references of a way.
///
/// Each reference corresponds to a node id. Signed wire deltas reconstruct
/// modulo 2^64, matching the writer even across MIN/MAX boundaries.
#[derive(Clone)]
pub struct WayRefIter<'a> {
    deltas: PackedSint64Iter<'a>,
    current: i64,
}

impl<'a> WayRefIter<'a> {
    /// The values of a packed `sint64` delta array, running sums from zero
    /// wrapping modulo 2^64: a way's `refs`, a relation's `memids` (through
    /// [`MemberIdIter`]), and for the projection scanners a dense group's
    /// `id` array, whose values `DenseNodeIter` reconstructs by the same
    /// wrapping sum in lockstep with its coordinates.
    pub(crate) fn delta_decoded(data: &'a [u8]) -> Self {
        WayRefIter {
            deltas: PackedSint64Iter::new(data),
            current: 0,
        }
    }
}

impl Iterator for WayRefIter<'_> {
    type Item = i64;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.deltas.next().map(|d| {
            self.current = self.current.wrapping_add(d);
            self.current
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.deltas.size_hint()
    }
}

/// One inline node location of a way (LocationsOnWays).
///
/// Carries the reconstructed raw values with the block's framing, so the
/// accessors scale exactly and refuse what the target type cannot hold,
/// like [`Node`]'s.
#[derive(Clone, Copy, Debug)]
pub struct WayNodeLocation {
    lat: i64,
    lon: i64,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}

impl WayNodeLocation {
    #[inline]
    fn lat_parts(&self) -> (i64, i64, i64) {
        (self.lat_offset, self.granularity, self.lat)
    }

    #[inline]
    fn lon_parts(&self) -> (i64, i64, i64) {
        (self.lon_offset, self.granularity, self.lon)
    }

    impl_coordinate_conversions!(
        crate::scan::node::unrepresentable_location_err,
        crate::scan::node::unrepresentable_location_nano_err
    );
}

/// Combine the `size_hint`s of iterators advanced in lockstep, the way
/// `std::iter::Zip` does: the minimum of the lower bounds and the minimum of
/// the known upper bounds.
///
/// Block construction (`validate_block` in `block.rs`) rejects
/// parallel arrays of different lengths, so on a constructed block every
/// component reports the same count and any one of them would do. Taking the
/// minimum keeps the hint true without leaning on that invariant: if
/// a path ever bypasses the check, `next` stops at the shortest array and the
/// hint already says so. The component hints are protohoggr's packed-array
/// hints, whose lower bound is a documented overclaim on a malformed varint
/// (see the "Size hint" section on protohoggr's `PackedIter`); construction
/// also rejects those, and this combination neither fixes nor worsens that.
pub(super) fn zip_hint(hints: &[(usize, Option<usize>)]) -> (usize, Option<usize>) {
    let lower = hints.iter().map(|h| h.0).min().unwrap_or(0);
    let upper = hints.iter().filter_map(|h| h.1).min();
    let upper = if hints.is_empty() { Some(0) } else { upper };
    (lower, upper)
}

/// An iterator over the node locations of a way.
#[derive(Clone)]
pub struct WayNodeLocationsIter<'a> {
    dlats: PackedSint64Iter<'a>,
    dlons: PackedSint64Iter<'a>,
    clat: i64,
    clon: i64,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}

impl Iterator for WayNodeLocationsIter<'_> {
    type Item = WayNodeLocation;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match (self.dlats.next(), self.dlons.next()) {
            (Some(dlat), Some(dlon)) => {
                self.clat = self.clat.wrapping_add(dlat);
                self.clon = self.clon.wrapping_add(dlon);
                // The sums wrap like every reader's; the accessors scale
                // them exactly.
                Some(WayNodeLocation {
                    lat: self.clat,
                    lon: self.clon,
                    granularity: self.granularity,
                    lat_offset: self.lat_offset,
                    lon_offset: self.lon_offset,
                })
            }
            (dlat, dlon) => {
                // Construction (`validate_way_geometry_arrays` in `block.rs`)
                // rejects lat and lon arrays of different lengths, so both
                // end together.
                super::block::arrays_end_together(dlat.is_none() && dlon.is_none(), "way lat/lon");
                None
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        zip_hint(&[self.dlats.size_hint(), self.dlons.size_hint()])
    }
}

/// The element type of a relation member.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemberType {
    Node,
    Way,
    Relation,
    /// A member type value not recognized by this version of the library.
    Unknown(i32),
}

impl From<i32> for MemberType {
    #[inline]
    fn from(v: i32) -> MemberType {
        match v {
            0 => MemberType::Node,
            1 => MemberType::Way,
            2 => MemberType::Relation,
            other => MemberType::Unknown(other),
        }
    }
}

/// A typed relation member reference combining element type and ID.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum MemberId {
    Node(i64),
    Way(i64),
    Relation(i64),
    Unknown(i32, i64),
}

impl MemberId {
    /// Returns the raw element ID regardless of type.
    #[inline]
    pub fn id(self) -> i64 {
        match self {
            MemberId::Node(id)
            | MemberId::Way(id)
            | MemberId::Relation(id)
            | MemberId::Unknown(_, id) => id,
        }
    }

    /// Returns the element type of this member reference.
    #[inline]
    pub fn member_type(self) -> MemberType {
        match self {
            MemberId::Node(_) => MemberType::Node,
            MemberId::Way(_) => MemberType::Way,
            MemberId::Relation(_) => MemberType::Relation,
            MemberId::Unknown(raw, _) => MemberType::Unknown(raw),
        }
    }

    /// Construct a MemberId from a raw id and type.
    #[inline]
    pub fn from_id_and_type(id: i64, member_type: MemberType) -> Self {
        match member_type {
            MemberType::Node => MemberId::Node(id),
            MemberType::Way => MemberId::Way(id),
            MemberType::Relation => MemberId::Relation(id),
            MemberType::Unknown(raw) => MemberId::Unknown(raw, id),
        }
    }
}

/// A member of a relation.
#[derive(Clone, Debug)]
pub struct RelMember<'a> {
    block: &'a WireBlock<'static>,
    pub role_sid: i32,
    pub id: MemberId,
}

impl<'a> RelMember<'a> {
    /// Returns the role of a relation member.
    #[allow(clippy::cast_sign_loss)]
    pub fn role(&self) -> Result<&'a str> {
        if self.role_sid < 0 {
            return Err(crate::error::new_wire_error(
                "relation member has negative role string index",
            ));
        }
        str_from_stringtable(self.block, self.role_sid as usize)
    }
}

/// An iterator over the members of a relation.
#[derive(Clone)]
pub struct RelMemberIter<'a> {
    block: &'a WireBlock<'static>,
    role_sids: PackedInt32Iter<'a>,
    ids: MemberIdIter<'a>,
}

impl<'a> RelMemberIter<'a> {
    fn new(block: &'a WireBlock<'static>, rel: &WireRelation<'a>) -> RelMemberIter<'a> {
        RelMemberIter {
            block,
            role_sids: PackedInt32Iter::new(rel.roles_sid_data),
            ids: MemberIdIter::new(rel.memids_data, rel.types_data),
        }
    }
}

impl<'a> Iterator for RelMemberIter<'a> {
    type Item = RelMember<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match (self.role_sids.next(), self.ids.next()) {
            (Some(role_sid), Some(id)) => Some(RelMember {
                block: self.block,
                role_sid,
                id,
            }),
            (role_sid, id) => {
                // Construction (`validate_relation_member_arrays` in
                // `block.rs`) rejects member arrays of different lengths, so
                // all three end together.
                super::block::arrays_end_together(
                    role_sid.is_none() && id.is_none(),
                    "relation member",
                );
                None
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        zip_hint(&[self.role_sids.size_hint(), self.ids.size_hint()])
    }
}

/// A relation's typed member IDs, from its `memids` and `types` arrays
/// alone: what [`RelMemberIter`] yields without the roles, and what the
/// projection scanners read members through, so a member's ID and type are
/// one decode however it is reached. Each `memids` value is the wrapping
/// running sum of its deltas ([`WayRefIter::delta_decoded`]), advanced by
/// every member, an `Unknown` one included; each type narrows to `i32` and
/// converts through [`MemberType::from`].
#[derive(Clone)]
pub(crate) struct MemberIdIter<'a> {
    ids: WayRefIter<'a>,
    types: PackedInt32Iter<'a>,
}

impl<'a> MemberIdIter<'a> {
    pub(crate) fn new(memids_data: &'a [u8], types_data: &'a [u8]) -> Self {
        MemberIdIter {
            ids: WayRefIter::delta_decoded(memids_data),
            types: PackedInt32Iter::new(types_data),
        }
    }
}

impl Iterator for MemberIdIter<'_> {
    type Item = MemberId;

    #[inline]
    fn next(&mut self) -> Option<MemberId> {
        match (self.ids.next(), self.types.next()) {
            (Some(id), Some(member_type)) => Some(MemberId::from_id_and_type(
                id,
                MemberType::from(member_type),
            )),
            (id, member_type) => {
                // Construction (`validate_relation_member_arrays` in
                // `block.rs`) rejects member arrays of different lengths.
                // Every constructor pairs the arrays of one relation from a
                // block that passed it (the element walk, and the projection
                // scanners on their accepted blob).
                super::block::arrays_end_together(
                    id.is_none() && member_type.is_none(),
                    "relation member",
                );
                None
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        zip_hint(&[self.ids.size_hint(), self.types.size_hint()])
    }
}

/// An iterator over the tags of an element. It returns a pair of strings (key and value).
#[derive(Clone)]
pub struct TagIter<'a> {
    block: &'a WireBlock<'static>,
    key_indices: PackedUint32Iter<'a>,
    val_indices: PackedUint32Iter<'a>,
}

impl<'a> Iterator for TagIter<'a> {
    type Item = (&'a str, &'a str);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match (self.key_indices.next(), self.val_indices.next()) {
            (Some(k), Some(v)) => Some(resolved_tag(self.block, k as usize, v as usize)),
            (k, v) => {
                // Construction (`validate_tag_arrays` in `block.rs`) rejects
                // keys and vals arrays of different lengths.
                super::block::arrays_end_together(k.is_none() && v.is_none(), "tag key/value");
                None
            }
        }
    }

    /// The packed arrays' byte-based hint: the arrays are equal in count
    /// and every pair resolves (block construction), but the count itself
    /// is not stored.
    fn size_hint(&self) -> (usize, Option<usize>) {
        (
            0,
            zip_hint(&[self.key_indices.size_hint(), self.val_indices.size_hint()]).1,
        )
    }
}

impl std::iter::FusedIterator for TagIter<'_> {}

/// An iterator over the tags of an element as raw index pairs.
#[derive(Clone)]
pub struct RawTagIter<'a> {
    key_indices: PackedUint32Iter<'a>,
    val_indices: PackedUint32Iter<'a>,
}

impl Iterator for RawTagIter<'_> {
    type Item = (u32, u32);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match (self.key_indices.next(), self.val_indices.next()) {
            (Some(key_index), Some(val_index)) => Some((key_index, val_index)),
            (k, v) => {
                // Construction (`validate_tag_arrays` in `block.rs`) rejects
                // keys and vals arrays of different lengths.
                super::block::arrays_end_together(k.is_none() && v.is_none(), "tag key/value");
                None
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        zip_hint(&[self.key_indices.size_hint(), self.val_indices.size_hint()])
    }
}

/// Additional metadata that might be included in each element.
#[derive(Clone, Debug)]
pub struct Info<'a> {
    block: &'a WireBlock<'static>,
    info: WireInfo,
}

impl<'a> Info<'a> {
    fn new(block: &'a WireBlock<'static>, info: WireInfo) -> Info<'a> {
        Info { block, info }
    }

    /// Returns the version of this element.
    #[inline]
    pub fn version(&self) -> Option<i32> {
        self.info.version
    }

    /// Returns the version this element claims, or `None` when it claims
    /// none.
    ///
    /// OSM versions start at 1. A present version of 0 is how pbfhogg's
    /// `BlockBuilder` and libosmium spell "no version" when they still write
    /// an `Info` - an apply-changes element that carried a timestamp but no
    /// version, for one - so it is not a claim. The Osmosis -1 sentinel is
    /// already `None` from [`version`](Self::version). Only the version is
    /// filtered: every other `Info` field keeps its own presence.
    #[inline]
    pub fn claimed_version(&self) -> Option<i32> {
        self.info.version.filter(|&v| v >= 1)
    }

    /// Returns the time stamp in milliseconds since the epoch.
    ///
    /// Scaling by `date_granularity` wraps on `i64` overflow: a timestamp
    /// that far out is garbage in, garbage out, not a debug panic.
    #[inline]
    pub fn milli_timestamp(&self) -> Option<i64> {
        self.info
            .timestamp
            .map(|ts| ts.wrapping_mul(i64::from(self.block.date_granularity)))
    }

    /// The time stamp in milliseconds, scaled exactly in `i128` so it never
    /// wraps. Agrees with [`milli_timestamp`](Self::milli_timestamp)
    /// whenever it fits `i64`.
    #[inline]
    pub fn milli_timestamp_exact(&self) -> Option<i128> {
        self.info
            .timestamp
            .map(|ts| i128::from(ts) * i128::from(self.block.date_granularity))
    }

    /// The timestamp in milliseconds this element claims, `None` when the
    /// field is absent, and an `Err` when its scaling by `date_granularity`
    /// overflows `i64` (see `read::claims::scale_milli_timestamp`). The
    /// reading every metadata output shares, in place of the wrapping
    /// [`milli_timestamp`](Self::milli_timestamp).
    #[inline]
    pub(crate) fn claimed_milli_timestamp(&self) -> Option<crate::Result<i64>> {
        self.info.timestamp.map(|ts| {
            crate::read::claims::scale_milli_timestamp(ts, i64::from(self.block.date_granularity))
        })
    }

    /// The timestamp as a re-encoded output block stores it; see
    /// `owned::output_timestamp_seconds`.
    #[inline]
    pub(crate) fn output_timestamp_seconds(&self) -> Option<i64> {
        self.info.timestamp.map(|ts| {
            crate::owned::output_timestamp_seconds(ts, i64::from(self.block.date_granularity))
        })
    }

    /// Returns the changeset id.
    #[inline]
    pub fn changeset(&self) -> Option<i64> {
        self.info.changeset
    }

    /// Returns the user id.
    #[inline]
    pub fn uid(&self) -> Option<i32> {
        self.info.uid
    }

    /// Returns the raw string table index for the user name, if present.
    #[inline]
    pub fn raw_user_sid(&self) -> Option<i32> {
        self.info.user_sid
    }

    /// Returns the user name, `None` when the element carries none.
    ///
    /// A string index past the block's string table is
    /// `StringtableIndexOutOfBounds`; a negative one is a wire-format error,
    /// as for [`RelMember::role`].
    #[allow(clippy::cast_sign_loss)]
    pub fn user(&self) -> Option<Result<&'a str>> {
        self.info.user_sid.map(|sid| {
            if sid < 0 {
                return Err(crate::error::new_wire_error(
                    "element info has negative user string index",
                ));
            }
            str_from_stringtable(self.block, sid as usize)
        })
    }

    /// Returns the visibility status of an element.
    // wontfix(name-is-has-bool): inherited from osmpbf public API
    #[inline]
    pub fn visible(&self) -> bool {
        self.info.visible.unwrap_or(true)
    }

    /// Return visibility only when the source info message carries the field.
    pub(crate) fn visible_opt(&self) -> Option<bool> {
        self.info.visible
    }

    /// Returns true if the element was deleted.
    // wontfix(name-is-has-bool): inherited from osmpbf public API
    #[inline]
    pub fn deleted(&self) -> bool {
        !self.visible()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::write::block_builder::{BlockBuilder, MemberData};

    #[test]
    fn way_ref_and_location_accumulators_wrap_at_signed_boundaries() {
        let deltas = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1, 1];
        let refs = WayRefIter {
            deltas: PackedSint64Iter::new(&deltas),
            current: 0,
        };
        assert_eq!(refs.collect::<Vec<_>>(), [i64::MIN, i64::MAX]);
        let locations = WayNodeLocationsIter {
            dlats: PackedSint64Iter::new(&deltas),
            dlons: PackedSint64Iter::new(&deltas),
            clat: 0,
            clon: 0,
            granularity: 1,
            lat_offset: 0,
            lon_offset: 0,
        };
        let coords: Vec<_> = locations
            .map(|p| (p.nano_lat().unwrap(), p.nano_lon().unwrap()))
            .collect();
        assert_eq!(coords, [(i64::MIN, i64::MIN), (i64::MAX, i64::MAX)]);
    }

    /// A way location whose framing scales it past `i64` nanodegrees, or past
    /// `i32` decimicrodegrees, is refused, not wrapped; the exact and degree
    /// forms still answer.
    #[test]
    fn way_location_scaling_is_exact_and_refuses_overflow() {
        // One delta of +1.
        let one = [2u8];
        let past_i64 = WayNodeLocationsIter {
            dlats: PackedSint64Iter::new(&one),
            dlons: PackedSint64Iter::new(&one),
            clat: 0,
            clon: 0,
            granularity: 1,
            lat_offset: i64::MAX,
            lon_offset: 0,
        }
        .next()
        .unwrap();
        assert!(past_i64.nano_lat().is_err());
        assert!(past_i64.decimicro_lat().is_err());
        assert_eq!(past_i64.nano_lat_exact(), i128::from(i64::MAX) + 1);
        assert!(past_i64.lat() > 9.2e9);
        assert_eq!(past_i64.nano_lat_wrapping(), i64::MIN);
        assert_eq!(past_i64.nano_lon().unwrap(), 1);

        // The BUG-class example: 429.4967296 degrees fits i64 but not i32 e7.
        let past_i32 = WayNodeLocationsIter {
            dlats: PackedSint64Iter::new(&one),
            dlons: PackedSint64Iter::new(&one),
            clat: -1,
            clon: -1,
            granularity: 100,
            lat_offset: 429_496_729_600,
            lon_offset: 0,
        }
        .next()
        .unwrap();
        assert_eq!(past_i32.nano_lat().unwrap(), 429_496_729_600);
        assert!(past_i32.decimicro_lat().is_err());
        assert_eq!(past_i32.decimicro_lat_wrapping(), 0);
        assert_eq!(past_i32.decimicro_lon().unwrap(), 0);
    }

    /// Parallel arrays of different lengths cannot come out of block
    /// construction, so an iterator over them is a broken invariant and
    /// panics instead of silently dropping the longer array's tail. Built
    /// here by hand, as nothing else can build one.
    #[test]
    #[should_panic(expected = "tag key/value arrays differ in length")]
    fn mismatched_tag_arrays_panic_instead_of_ending_early() {
        let keys = [1_u8, 2];
        let vals = [1_u8];
        let tags = RawTagIter {
            key_indices: PackedUint32Iter::new(&keys),
            val_indices: PackedUint32Iter::new(&vals),
        };
        for _ in tags {}
    }

    #[test]
    #[should_panic(expected = "way lat/lon arrays differ in length")]
    fn mismatched_way_location_arrays_panic_instead_of_ending_early() {
        let lats = [2_u8, 2];
        let lons = [2_u8];
        let locations = WayNodeLocationsIter {
            dlats: PackedSint64Iter::new(&lats),
            dlons: PackedSint64Iter::new(&lons),
            clat: 0,
            clon: 0,
            granularity: 1,
            lat_offset: 0,
            lon_offset: 0,
        };
        for _ in locations {}
    }

    #[test]
    fn relation_member_ids_roundtrip_across_signed_boundaries() {
        let mut builder = BlockBuilder::new();
        let members = [
            MemberData {
                id: MemberId::Node(i64::MIN),
                role: "",
            },
            MemberData {
                id: MemberId::Way(i64::MAX),
                role: "",
            },
        ];
        builder.add_relation(1, [], &members, None);
        let bytes = builder.take().unwrap().unwrap().to_vec();
        let block = crate::PrimitiveBlock::from_vec(bytes).unwrap();
        let ids: Vec<_> = block
            .elements()
            .flat_map(|element| match element {
                Element::Relation(relation) => {
                    relation.members().map(|m| m.id.id()).collect::<Vec<_>>()
                }
                _ => panic!("fixture contains only a relation"),
            })
            .collect();
        assert_eq!(ids, [i64::MIN, i64::MAX]);
    }

    /// `members_into` resolves members in order into a cleared buffer and
    /// refuses a role index past the string table or negative, naming the
    /// relation, the position and the index.
    #[test]
    fn members_into_resolves_or_refuses_roles() {
        use crate::read::wire_fixtures::{Group, block, relation};

        let negative = u64::from_ne_bytes((-1i64).to_ne_bytes());
        let bytes = block(
            &["", "outer", "inner"],
            &[Group {
                relations: vec![
                    relation(1, &[(1, 10, 2), (2, 11, 1)]),
                    relation(2, &[(1, 10, 2), (9, 11, 1)]),
                    relation(3, &[(negative, 10, 2)]),
                ],
                ..Group::default()
            }],
        );
        let block = crate::PrimitiveBlock::from_vec(bytes).unwrap();
        let relations: Vec<_> = block
            .elements()
            .map(|element| match element {
                Element::Relation(relation) => relation,
                _ => panic!("fixture contains only relations"),
            })
            .collect();

        let mut buf = vec![crate::block_builder::MemberData {
            id: MemberId::from_id_and_type(99, MemberType::Node),
            role: "stale",
        }];
        relations[0].members_into(&mut buf).unwrap();
        let got: Vec<_> = buf.iter().map(|m| (m.id.id(), m.role)).collect();
        assert_eq!(got, [(10, "outer"), (11, "inner")]);

        let err = relations[1].members_into(&mut buf).unwrap_err().to_string();
        assert!(
            err.contains("relation 2 member 1: role string index 9"),
            "{err}"
        );
        let got: Vec<_> = buf.iter().map(|m| (m.id.id(), m.role)).collect();
        assert_eq!(got, [(10, "outer")], "the prefix before the bad member");

        let err = relations[2].members_into(&mut buf).unwrap_err().to_string();
        assert!(
            err.contains("relation 3 member 0: role string index -1"),
            "{err}"
        );
        assert!(buf.is_empty());
    }
}
