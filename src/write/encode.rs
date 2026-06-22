//! Wire-format encoders for `PrimitiveGroup` elements. These are called from
//! `BlockBuilder`'s `add_*` methods to append serialized bytes into the
//! current group buffer.

use rustc_hash::FxHashSet;

use protohoggr::{encode_bytes_field, encode_int64_field, encode_varint, zigzag_encode_64};

use super::block_builder::{MemberData, Metadata, member_type_value};
use super::string_table::StringTable;

/// Encode an `int32` field unconditionally (even when value is 0).
///
/// Writes the field tag + varint(0) even for the zero value. This differs from
/// `encode_int32_field` which skips zero values (matching non-optional fields).
/// Only valid for field numbers <= 15 (single-byte tag encoding).
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn encode_optional_int32(buf: &mut Vec<u8>, field: u32, value: i32) {
    debug_assert!(
        field <= 15,
        "single-byte tag requires field <= 15, got {field}"
    );
    buf.push((field << 3) as u8); // wire type 0 (varint)
    encode_varint(buf, value as i64 as u64);
}

/// Encode an `int64` field unconditionally (even when value is 0).
///
/// Only valid for field numbers <= 15 (single-byte tag encoding).
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn encode_optional_int64(buf: &mut Vec<u8>, field: u32, value: i64) {
    debug_assert!(
        field <= 15,
        "single-byte tag requires field <= 15, got {field}"
    );
    buf.push((field << 3) as u8);
    encode_varint(buf, value as u64);
}

/// Encode a `uint32` field unconditionally (even when value is 0).
///
/// Only valid for field numbers <= 15 (single-byte tag encoding).
#[inline]
#[allow(clippy::cast_possible_truncation)]
fn encode_optional_uint32(buf: &mut Vec<u8>, field: u32, value: u32) {
    debug_assert!(
        field <= 15,
        "single-byte tag requires field <= 15, got {field}"
    );
    buf.push((field << 3) as u8);
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
    encode_optional_int32(info, 1, meta.version);
    // Field 2: timestamp (optional int64) - always present
    encode_optional_int64(info, 2, meta.timestamp);
    // Field 3: changeset (optional int64) - always present
    encode_optional_int64(info, 3, meta.changeset);
    // Field 4: uid (optional int32) - always present
    encode_optional_int32(info, 4, meta.uid);
    // Field 5: user_sid (optional uint32) - always present
    encode_optional_uint32(info, 5, string_table.add(meta.user));
    // Field 6: visible (optional bool) - only emit when false
    // When visible=true, the current code leaves info.visible as None (prost skips it).
    // When visible=false, it sets Some(false), and prost writes tag + varint(0).
    if !meta.visible {
        info.push(6 << 3); // tag for field 6, wire type 0
        info.push(0x00); // false = varint(0)
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
pub(super) fn encode_way<'t>(
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
    metadata: Option<&Metadata<'_>>,
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
        encode_info_to(info_buf, string_table, meta);
        encode_bytes_field(elem, 4, info_buf);
    }

    // Field 8: refs (packed sint64, delta-encoded)
    if !refs.is_empty() {
        packed_keys.clear();
        let mut last_ref: i64 = 0;
        for &r in refs {
            encode_varint(packed_keys, zigzag_encode_64(r - last_ref));
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
pub(super) fn encode_way_with_locations<'t>(
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
    metadata: Option<&Metadata<'_>>,
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
        encode_info_to(info_buf, string_table, meta);
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
            encode_varint(packed_refs, zigzag_encode_64(r - last_ref));
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
/// written directly with field tag + length prefix.
pub(super) fn encode_way_raw_bytes(
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    id: i64,
    keys_data: &[u8],
    vals_data: &[u8],
    refs_data: &[u8],
    info_data: Option<&[u8]>,
) {
    debug_assert!(
        keys_data.is_empty() == vals_data.is_empty(),
        "keys/vals must be paired: keys={} vals={} bytes",
        keys_data.len(),
        vals_data.len(),
    );
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
pub(super) fn encode_relation<'t>(
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
    metadata: Option<&Metadata<'_>>,
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
        encode_info_to(info_buf, string_table, meta);
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
            encode_varint(packed, zigzag_encode_64(m.id.id() - last_memid));
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

/// Decode packed varint uint32 values from raw bytes and insert them into a set.
///
/// Used to extract string table key indices from raw way/relation keys_data
/// (packed uint32 protobuf field) for tag key tracking.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn collect_packed_varint_keys(data: &[u8], indices: &mut FxHashSet<u32>) {
    let mut cur = protohoggr::Cursor::new(data);
    while !cur.is_empty() {
        if let Ok(val) = cur.read_varint() {
            indices.insert(val as u32);
        } else {
            break;
        }
    }
}
