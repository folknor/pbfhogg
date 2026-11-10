//! Wire-format encoders for `PrimitiveGroup` elements. These are called from
//! `BlockBuilder`'s `add_*` methods to append serialized bytes into the
//! current group buffer.

use rustc_hash::FxHashSet;

use protohoggr::{encode_bytes_field, encode_int64_field, encode_varint, zigzag_encode_64};

use super::block_builder::{MemberData, Metadata, member_type_value};
use super::string_table::StringTable;

/// Bytes `protohoggr::encode_varint` writes for `value`: one per started
/// group of seven bits, and one for zero.
pub(super) const fn varint_len(value: u64) -> usize {
    let bits = 64 - (value | 1).leading_zeros() as usize;
    bits.div_ceil(7)
}

/// Bytes a length-delimited field with a one-byte tag occupies around a
/// `body_len`-byte body: the tag, the length varint and the body. `None` on
/// overflow.
pub(super) fn len_field_size(body_len: usize) -> Option<usize> {
    (1 + varint_len(body_len as u64)).checked_add(body_len)
}

/// The one-byte varint-field tag of field `FIELD`. A field number outside
/// `1..=15` (zero is not a protobuf field; above 15 the tag takes two bytes)
/// fails to compile.
#[inline]
#[allow(clippy::cast_possible_truncation)]
const fn varint_tag<const FIELD: u32>() -> u8 {
    const {
        assert!(
            FIELD >= 1 && FIELD <= 15,
            "a one-byte tag needs a field number in 1..=15"
        );
    }
    (FIELD << 3) as u8 // wire type 0 (varint)
}

/// Encode an `int32` field unconditionally (even when value is 0).
///
/// Writes the field tag + varint(0) even for the zero value. This differs from
/// `encode_int32_field` which skips zero values (matching non-optional fields).
#[inline]
#[allow(clippy::cast_sign_loss)]
fn encode_optional_int32<const FIELD: u32>(buf: &mut Vec<u8>, value: i32) {
    buf.push(varint_tag::<FIELD>());
    encode_varint(buf, value as i64 as u64);
}

/// Encode an `int64` field unconditionally (even when value is 0).
#[inline]
#[allow(clippy::cast_sign_loss)]
fn encode_optional_int64<const FIELD: u32>(buf: &mut Vec<u8>, value: i64) {
    buf.push(varint_tag::<FIELD>());
    encode_varint(buf, value as u64);
}

/// Encode a `uint32` field unconditionally (even when value is 0).
#[inline]
fn encode_optional_uint32<const FIELD: u32>(buf: &mut Vec<u8>, value: u32) {
    buf.push(varint_tag::<FIELD>());
    encode_varint(buf, u64::from(value));
}

/// Encode an `Info` submessage from high-level [`Metadata`].
///
/// Uses unconditional field writers to always emit all metadata fields,
/// even when their value is zero (matching the OSMPBF convention).
pub(super) fn encode_info_to(
    info: &mut Vec<u8>,
    string_table: &mut StringTable,
    meta: &Metadata<'_>,
) {
    info.clear();
    // Field 1: version (optional int32) - always present
    encode_optional_int32::<1>(info, meta.version);
    // Field 2: timestamp (optional int64) - always present
    encode_optional_int64::<2>(info, meta.timestamp);
    // Field 3: changeset (optional int64) - always present
    encode_optional_int64::<3>(info, meta.changeset);
    // Field 4: uid (optional int32) - always present
    encode_optional_int32::<4>(info, meta.uid);
    // Field 5: user_sid (optional uint32) - always present
    encode_optional_uint32::<5>(info, string_table.add(meta.user));
    // Field 6: visible (optional bool) - only emit when false
    // When visible=true, the current code leaves info.visible as None (prost skips it).
    // When visible=false, it sets Some(false), and prost writes tag + varint(0).
    if !meta.visible {
        info.push(6 << 3); // tag for field 6, wire type 0
        info.push(0x00); // false = varint(0)
    }
}

pub(super) trait EncodeMetadata {
    fn encode_info(&self, info: &mut Vec<u8>, strings: &mut StringTable);
}

impl EncodeMetadata for Metadata<'_> {
    fn encode_info(&self, info: &mut Vec<u8>, strings: &mut StringTable) {
        encode_info_to(info, strings, self);
    }
}

impl EncodeMetadata for super::block_builder::PartialMetadata<'_> {
    fn encode_info(&self, info: &mut Vec<u8>, strings: &mut StringTable) {
        let raw = super::block_builder::RawPartialMetadata {
            version: self.version,
            timestamp: self.timestamp,
            changeset: self.changeset,
            uid: self.uid,
            // `StringTable::add` keeps indices far below `i32::MAX`.
            user_sid: self.user.map(|user| strings.add(user).cast_signed()),
            visible: self.visible,
        };
        encode_raw_partial_info(info, &raw);
    }
}

/// Encode an `Info` body carrying exactly the fields present in `meta`,
/// explicit zeros and an explicit `visible` of either value included.
pub(super) fn encode_raw_partial_info(
    info: &mut Vec<u8>,
    meta: &super::block_builder::RawPartialMetadata,
) {
    info.clear();
    if let Some(value) = meta.version {
        encode_optional_int32::<1>(info, value);
    }
    if let Some(value) = meta.timestamp {
        encode_optional_int64::<2>(info, value);
    }
    if let Some(value) = meta.changeset {
        encode_optional_int64::<3>(info, value);
    }
    if let Some(value) = meta.uid {
        encode_optional_int32::<4>(info, value);
    }
    if let Some(value) = meta.user_sid {
        // A checked, nonnegative pre-seeded or interned index.
        encode_optional_uint32::<5>(info, value.cast_unsigned());
    }
    if let Some(value) = meta.visible {
        info.push(6 << 3);
        info.push(u8::from(value));
    }
}

/// Encode tag key/value pairs into packed fields 2 (keys) and 3 (vals) on `elem`.
///
/// Uses two scratch buffers (`keys_buf` and `vals_buf`) for single-pass dual-buffer
/// encoding. Each tag key index is also inserted into `tag_key_indices`.
pub(super) fn encode_tags<'t>(
    string_table: &mut StringTable,
    elem: &mut Vec<u8>,
    keys_buf: &mut Vec<u8>,
    vals_buf: &mut Vec<u8>,
    tag_key_indices: &mut FxHashSet<u32>,
    tags: impl IntoIterator<Item = (&'t str, &'t str)>,
) {
    keys_buf.clear();
    vals_buf.clear();
    for (key, val) in tags {
        let key_idx = string_table.add(key);
        tag_key_indices.insert(key_idx);
        encode_varint(keys_buf, u64::from(key_idx));
        encode_varint(vals_buf, u64::from(string_table.add(val)));
    }
    if !keys_buf.is_empty() {
        encode_bytes_field(elem, 2, keys_buf);
        encode_bytes_field(elem, 3, vals_buf);
    }
}

/// Encode a Way and append it as `PrimitiveGroup.ways` (field 3) to `group_buf`.
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_way<'t, M: EncodeMetadata>(
    string_table: &mut StringTable,
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    packed_keys: &mut Vec<u8>,
    packed_vals: &mut Vec<u8>,
    info_buf: &mut Vec<u8>,
    tag_key_indices: &mut FxHashSet<u32>,
    id: i64,
    tags: impl IntoIterator<Item = (&'t str, &'t str)>,
    refs: &[i64],
    metadata: Option<&M>,
) {
    elem.clear();

    // Field 1: id (int64)
    encode_int64_field(elem, 1, id);

    // Fields 2+3: keys/vals
    encode_tags(
        string_table,
        elem,
        packed_keys,
        packed_vals,
        tag_key_indices,
        tags,
    );

    // Field 4: info (submessage)
    if let Some(meta) = metadata {
        meta.encode_info(info_buf, string_table);
        encode_bytes_field(elem, 4, info_buf);
    }

    // Field 8: refs (packed sint64, delta-encoded)
    if !refs.is_empty() {
        packed_keys.clear();
        let mut last_ref: i64 = 0;
        for &r in refs {
            // PBF sint64 deltas are modular; keep signed-boundary inputs
            // consistent between debug and release builds.
            encode_varint(packed_keys, zigzag_encode_64(r.wrapping_sub(last_ref)));
            last_ref = r;
        }
        encode_bytes_field(elem, 8, packed_keys);
    }

    // Wrap as PrimitiveGroup field 3 (Way submessage)
    encode_bytes_field(group_buf, 3, elem);
}

/// Encode a Way with embedded node locations (fields 9/10: lat/lon).
///
/// Uses three packed buffers in a single zip loop for refs/lat/lon to avoid
/// iterating the data three separate times.
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_way_with_locations<'t, M: EncodeMetadata>(
    string_table: &mut StringTable,
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    packed_refs: &mut Vec<u8>,
    packed_vals: &mut Vec<u8>,
    packed_lats: &mut Vec<u8>,
    packed_lons: &mut Vec<u8>,
    info_buf: &mut Vec<u8>,
    tag_key_indices: &mut FxHashSet<u32>,
    id: i64,
    tags: impl IntoIterator<Item = (&'t str, &'t str)>,
    refs: &[i64],
    locations: &[(i32, i32)],
    metadata: Option<&M>,
) {
    elem.clear();
    encode_int64_field(elem, 1, id);

    // Fields 2+3: keys/vals
    encode_tags(
        string_table,
        elem,
        packed_refs,
        packed_vals,
        tag_key_indices,
        tags,
    );

    if let Some(meta) = metadata {
        meta.encode_info(info_buf, string_table);
        encode_bytes_field(elem, 4, info_buf);
    }

    // Fields 8, 9, 10: refs + lat + lon (all delta-encoded, single pass)
    if !refs.is_empty() {
        let mut last_ref: i64 = 0;
        let mut last_lat: i64 = 0;
        let mut last_lon: i64 = 0;

        packed_refs.clear();
        packed_lats.clear();
        packed_lons.clear();

        for (&r, &(loc_lat, loc_lon)) in refs.iter().zip(locations.iter()) {
            // PBF sint64 deltas are modular; keep signed-boundary inputs
            // consistent between debug and release builds.
            encode_varint(packed_refs, zigzag_encode_64(r.wrapping_sub(last_ref)));
            last_ref = r;
            let lat = i64::from(loc_lat);
            encode_varint(packed_lats, zigzag_encode_64(lat - last_lat));
            last_lat = lat;
            let lon = i64::from(loc_lon);
            encode_varint(packed_lons, zigzag_encode_64(lon - last_lon));
            last_lon = lon;
        }

        encode_bytes_field(elem, 8, packed_refs);
        encode_bytes_field(elem, 9, packed_lats);
        encode_bytes_field(elem, 10, packed_lons);
    }

    encode_bytes_field(group_buf, 3, elem);
}

/// Encode a Way from raw wire-format bytes (zero decode/reencode passthrough).
///
/// All byte slices are raw protobuf packed field content from `WireWay`,
/// written directly with field tag + length prefix. That the keys and vals
/// arrays pair up is the source block's: construction refuses arrays of
/// different lengths (`validate_tag_arrays`), as for every raw encoder here.
pub(super) fn encode_way_raw_bytes(
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    id: i64,
    keys_data: &[u8],
    vals_data: &[u8],
    refs_data: &[u8],
    info_data: Option<&[u8]>,
) {
    elem.clear();
    encode_int64_field(elem, 1, id);
    encode_bytes_field(elem, 2, keys_data);
    encode_bytes_field(elem, 3, vals_data);
    if let Some(info) = info_data {
        encode_bytes_field(elem, 4, info);
    }
    encode_bytes_field(elem, 8, refs_data);
    encode_bytes_field(group_buf, 3, elem);
}

/// Encode a Way with LocationsOnWays raw bytes and append to `group_buf`.
///
/// Same as `encode_way_raw_bytes` but also writes fields 9 (lat) and 10 (lon).
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_way_raw_bytes_with_locations(
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    id: i64,
    keys_data: &[u8],
    vals_data: &[u8],
    refs_data: &[u8],
    lat_data: &[u8],
    lon_data: &[u8],
    info_data: Option<&[u8]>,
) {
    elem.clear();
    encode_int64_field(elem, 1, id);
    encode_bytes_field(elem, 2, keys_data);
    encode_bytes_field(elem, 3, vals_data);
    if let Some(info) = info_data {
        encode_bytes_field(elem, 4, info);
    }
    encode_bytes_field(elem, 8, refs_data);
    if !lat_data.is_empty() {
        encode_bytes_field(elem, 9, lat_data);
    }
    if !lon_data.is_empty() {
        encode_bytes_field(elem, 10, lon_data);
    }
    encode_bytes_field(group_buf, 3, elem);
}

/// Encode a Relation and append it as `PrimitiveGroup.relations` (field 4) to `group_buf`.
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_relation<'t, M: EncodeMetadata>(
    string_table: &mut StringTable,
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    packed: &mut Vec<u8>,
    packed_vals: &mut Vec<u8>,
    info_buf: &mut Vec<u8>,
    tag_key_indices: &mut FxHashSet<u32>,
    id: i64,
    tags: impl IntoIterator<Item = (&'t str, &'t str)>,
    members: &[MemberData<'_>],
    metadata: Option<&M>,
) {
    elem.clear();
    encode_int64_field(elem, 1, id);

    // Fields 2+3: keys/vals
    encode_tags(
        string_table,
        elem,
        packed,
        packed_vals,
        tag_key_indices,
        tags,
    );

    if let Some(meta) = metadata {
        meta.encode_info(info_buf, string_table);
        encode_bytes_field(elem, 4, info_buf);
    }

    // Members: three parallel packed arrays
    if !members.is_empty() {
        // Field 8: roles_sid (packed int32)
        packed.clear();
        #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
        for m in members {
            let role_sid = string_table.add(m.role) as i32;
            encode_varint(packed, role_sid as i64 as u64);
        }
        encode_bytes_field(elem, 8, packed);

        // Field 9: memids (packed sint64, delta-encoded)
        packed.clear();
        let mut last_memid: i64 = 0;
        for m in members {
            // PBF sint64 deltas are modular; keep signed-boundary inputs
            // consistent between debug and release builds.
            encode_varint(packed, zigzag_encode_64(m.id.id().wrapping_sub(last_memid)));
            last_memid = m.id.id();
        }
        encode_bytes_field(elem, 9, packed);

        // Field 10: types (packed int32)
        packed.clear();
        for m in members {
            // Protobuf int32 wire encoding: sign-extend i32 → i64 → u64.
            // MemberType enum values are 0/1/2 so no actual sign extension occurs.
            let mt = member_type_value(m.id.member_type());
            #[allow(clippy::cast_sign_loss)]
            encode_varint(packed, mt as u64);
        }
        encode_bytes_field(elem, 10, packed);
    }

    // PrimitiveGroup field 4 = Relation
    encode_bytes_field(group_buf, 4, elem);
}

/// Encode a Relation from raw wire-format bytes (zero decode/reencode passthrough).
///
/// All byte slices are raw protobuf packed field content from `WireRelation`,
/// written directly with field tag + length prefix.
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_relation_raw_bytes(
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    id: i64,
    keys_data: &[u8],
    vals_data: &[u8],
    roles_sid_data: &[u8],
    memids_data: &[u8],
    types_data: &[u8],
    info_data: Option<&[u8]>,
) {
    elem.clear();
    encode_int64_field(elem, 1, id);
    encode_bytes_field(elem, 2, keys_data);
    encode_bytes_field(elem, 3, vals_data);
    if let Some(info) = info_data {
        encode_bytes_field(elem, 4, info);
    }
    encode_bytes_field(elem, 8, roles_sid_data);
    encode_bytes_field(elem, 9, memids_data);
    encode_bytes_field(elem, 10, types_data);
    encode_bytes_field(group_buf, 4, elem);
}

/// Decode a raw way's or relation's packed `keys` and insert each string
/// index into `indices`, stopping at the first that is not below `seeded`
/// (the source string table's length) and returning it.
///
/// The array comes from a `PrimitiveBlock`, whose construction checked its
/// varints and that every one fits `u32` (`validate_tag_arrays`), so the
/// decode is the reader's (`PackedUint32Iter`). Indices inserted before a
/// refused one stay in the set; the caller poisons the builder on an error.
pub(super) fn collect_checked_keys(
    data: &[u8],
    seeded: usize,
    indices: &mut FxHashSet<u32>,
) -> Result<(), u32> {
    for sid in crate::read::wire::PackedUint32Iter::new(data) {
        if usize::try_from(sid).is_ok_and(|s| s < seeded) {
            indices.insert(sid);
        } else {
            return Err(sid);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{encode_relation, encode_way, encode_way_with_locations, varint_len};

    /// `varint_len` counts what `encode_varint` writes, at zero, at every
    /// width transition and at the largest value.
    #[test]
    fn varint_len_matches_the_encoder_at_every_width() {
        let mut values = vec![0u64, 1, u64::MAX];
        for bits in (7..64).step_by(7) {
            values.extend([(1u64 << bits) - 1, 1u64 << bits]);
        }
        for value in values {
            let mut buf = Vec::new();
            protohoggr::encode_varint(&mut buf, value);
            assert_eq!(varint_len(value), buf.len(), "{value}");
        }
    }
    use crate::MemberId;
    use crate::read::wire::{Cursor, PackedSint64Iter, WireRelation, WireWay};
    use crate::write::block_builder::MemberData;
    use crate::write::string_table::StringTable;
    use rustc_hash::FxHashSet;

    fn first_message(group: &[u8], expected_field: u32) -> &[u8] {
        let mut cursor = Cursor::new(group);
        let (field, wire_type) = cursor.read_tag().unwrap().unwrap();
        assert_eq!(field, expected_field);
        assert_eq!(wire_type, crate::read::wire::WIRE_LEN);
        cursor.read_len_delimited().unwrap()
    }

    #[test]
    fn way_ref_deltas_wrap_at_signed_boundaries() {
        let mut string_table = StringTable::new();
        let mut group = Vec::new();
        let mut elem = Vec::new();
        let mut packed_keys = Vec::new();
        let mut packed_vals = Vec::new();
        let mut info = Vec::new();
        let mut tag_key_indices = FxHashSet::default();

        encode_way(
            &mut string_table,
            &mut group,
            &mut elem,
            &mut packed_keys,
            &mut packed_vals,
            &mut info,
            &mut tag_key_indices,
            1,
            [],
            &[i64::MIN, i64::MAX],
            None::<&super::Metadata<'_>>,
        );

        let way = WireWay::parse(first_message(&group, 3)).unwrap();
        assert_eq!(
            PackedSint64Iter::new(way.refs_data).collect::<Vec<_>>(),
            [i64::MIN, -1],
        );
    }

    #[test]
    fn way_ref_deltas_with_locations_wrap_at_signed_boundaries() {
        let mut string_table = StringTable::new();
        let mut group = Vec::new();
        let mut elem = Vec::new();
        let mut packed_refs = Vec::new();
        let mut packed_vals = Vec::new();
        let mut packed_lats = Vec::new();
        let mut packed_lons = Vec::new();
        let mut info = Vec::new();
        let mut tag_key_indices = FxHashSet::default();

        encode_way_with_locations(
            &mut string_table,
            &mut group,
            &mut elem,
            &mut packed_refs,
            &mut packed_vals,
            &mut packed_lats,
            &mut packed_lons,
            &mut info,
            &mut tag_key_indices,
            1,
            [],
            &[i64::MIN, i64::MAX],
            &[(1, 2), (3, 4)],
            None::<&super::Metadata<'_>>,
        );

        let way = WireWay::parse(first_message(&group, 3)).unwrap();
        assert_eq!(
            PackedSint64Iter::new(way.refs_data).collect::<Vec<_>>(),
            [i64::MIN, -1],
        );
    }

    #[test]
    fn relation_member_deltas_wrap_at_signed_boundaries() {
        let mut string_table = StringTable::new();
        let mut group = Vec::new();
        let mut elem = Vec::new();
        let mut packed = Vec::new();
        let mut packed_vals = Vec::new();
        let mut info = Vec::new();
        let mut tag_key_indices = FxHashSet::default();
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

        encode_relation(
            &mut string_table,
            &mut group,
            &mut elem,
            &mut packed,
            &mut packed_vals,
            &mut info,
            &mut tag_key_indices,
            1,
            [],
            &members,
            None::<&super::Metadata<'_>>,
        );

        let relation = WireRelation::parse(first_message(&group, 4)).unwrap();
        assert_eq!(
            PackedSint64Iter::new(relation.memids_data).collect::<Vec<_>>(),
            [i64::MIN, -1],
        );
    }
}
