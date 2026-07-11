//! Wire-format way-blob reframe for the dense / sparse pass 2 way arm.
//!
//! Lifted from `external/stage4.rs::reframe_way_blob_with_locations`. Same
//! structure (StringTable byte-pass, group iteration, way reframe) but the
//! coordinate source is a `NodeIndex` lookup instead of a stage-3
//! pre-encoded `coord_payload`.
//!
//! Hot path: decompress -> walk wire -> per-way `NodeIndex::get` per ref ->
//! zigzag-delta-encode lat / lon -> append fields 9 / 10 to original way
//! bytes -> compress. No `BlockBuilder`, no `StringTable::add`, no Info
//! decode / encode, no ref redelta, no tag re-intern.
//!
//! If the input way already declares fields 9 and 10 (input is itself a
//! `LocationsOnWays` PBF), those fields are stripped before append - we
//! own the coordinates from `NodeIndex`, the input's are stale at best.

use protohoggr::{
    Cursor, PackedSint64Iter, WIRE_LEN, WIRE_VARINT, encode_bytes_field, encode_int64_field,
    encode_tag, encode_varint, zigzag_encode_64,
};

use super::NodeIndex;

/// Per-blob scratch buffers reused across way-blob reframes within a worker.
#[derive(Default)]
pub(super) struct WayReframeScratch {
    pub group_ranges: Vec<(usize, usize)>,
    pub scalar_fields: Vec<u8>,
    pub group_out: Vec<u8>,
    pub reframed_way: Vec<u8>,
    pub packed_lats: Vec<u8>,
    pub packed_lons: Vec<u8>,
    pub refs: Vec<i64>,
    pub member_ways: Vec<bool>,
    pub pins: Vec<u8>,
}

/// Per-blob counters that the caller folds into `Stats` after the reframe.
#[derive(Default)]
pub(super) struct WayReframeStats {
    pub way_count: u64,
    pub min_way_id: i64,
    pub max_way_id: i64,
    pub missing_locations: u64,
}

/// Reframe one decompressed way-blob `PrimitiveBlock` by appending packed
/// `lat` / `lon` fields (9 / 10) to each way using `node_index` for
/// coordinate lookups. Writes the resulting `PrimitiveBlock` bytes into
/// `output` (caller compresses + writes).
///
/// Errors are returned as `String` to match the existing par_iter result
/// channel in `process_slot_batch`.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn reframe_way_blob_with_locations(
    decompressed: &[u8],
    node_index: &NodeIndex,
    output: &mut Vec<u8>,
    scratch: &mut WayReframeScratch,
    shared_node_ids: Option<&crate::idset::IdSet>,
    member_way_ids: Option<&crate::idset::IdSet>,
    inject_prepass: bool,
) -> std::result::Result<WayReframeStats, String> {
    let (st_offset, st_len) = parse_block_top(decompressed, scratch)?;
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    output.clear();
    encode_bytes_field(output, 1, stringtable_bytes);

    let mut stats = WayReframeStats {
        min_way_id: i64::MAX,
        max_way_id: i64::MIN,
        ..WayReframeStats::default()
    };
    scratch.member_ways.clear();

    for i in 0..scratch.group_ranges.len() {
        let (gr_offset, gr_len) = scratch.group_ranges[i];
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];
        process_group(
            group_bytes,
            node_index,
            output,
            scratch,
            &mut stats,
            shared_node_ids,
            member_way_ids,
            inject_prepass,
        )?;
    }

    output.extend_from_slice(&scratch.scalar_fields);

    if stats.way_count == 0 {
        stats.min_way_id = 0;
        stats.max_way_id = 0;
    }

    Ok(stats)
}

/// Walk the `PrimitiveBlock` top-level fields. Captures the StringTable
/// byte range (no parse), group offsets, and scalar fields (17-20)
/// verbatim into `scratch`. Returns the StringTable's `(offset, len)` in
/// `decompressed`.
fn parse_block_top(
    decompressed: &[u8],
    scratch: &mut WayReframeScratch,
) -> std::result::Result<(usize, usize), String> {
    scratch.group_ranges.clear();
    scratch.scalar_fields.clear();
    let mut stringtable_range: Option<(usize, usize)> = None;

    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|e| format!("reframe block tag: {e}"))?
    {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("reframe st: {e}"))?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                stringtable_range = Some((offset, data.len()));
            }
            (2, WIRE_LEN) => {
                let data = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("reframe group: {e}"))?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                scratch.group_ranges.push((offset, data.len()));
            }
            (17..=20, WIRE_VARINT) => {
                let raw = cursor
                    .read_raw_field(wire_type)
                    .map_err(|e| format!("reframe scalar: {e}"))?;
                encode_tag(&mut scratch.scalar_fields, field, wire_type);
                scratch.scalar_fields.extend_from_slice(raw);
            }
            _ => cursor
                .skip_field(wire_type)
                .map_err(|e| format!("reframe skip: {e}"))?,
        }
    }

    stringtable_range.ok_or_else(|| "reframe: no StringTable in PrimitiveBlock".to_string())
}

/// Walk one `PrimitiveGroup`. Way submessages get reframed (locations
/// appended); non-way fields are copied verbatim. Builds the
/// `PrimitiveGroup` bytes in `scratch.group_out`, then emits as field 2
/// of the `PrimitiveBlock` in `output`.
#[allow(clippy::too_many_arguments)]
fn process_group(
    group_bytes: &[u8],
    node_index: &NodeIndex,
    output: &mut Vec<u8>,
    scratch: &mut WayReframeScratch,
    stats: &mut WayReframeStats,
    shared_node_ids: Option<&crate::idset::IdSet>,
    member_way_ids: Option<&crate::idset::IdSet>,
    inject_prepass: bool,
) -> std::result::Result<(), String> {
    scratch.group_out.clear();

    let mut gr_cursor = Cursor::new(group_bytes);
    while let Some((field, wire_type)) = gr_cursor
        .read_tag()
        .map_err(|e| format!("reframe gtag: {e}"))?
    {
        if field == 3 && wire_type == WIRE_LEN {
            let way_bytes = gr_cursor
                .read_len_delimited()
                .map_err(|e| format!("reframe way: {e}"))?;
            splice_way_locations(
                way_bytes,
                node_index,
                scratch,
                stats,
                shared_node_ids,
                member_way_ids,
                inject_prepass,
            )?;
            encode_bytes_field(&mut scratch.group_out, 3, &scratch.reframed_way);
        } else {
            let raw = gr_cursor
                .read_raw_field(wire_type)
                .map_err(|e| format!("reframe gskip: {e}"))?;
            encode_tag(&mut scratch.group_out, field, wire_type);
            scratch.group_out.extend_from_slice(raw);
        }
    }

    encode_bytes_field(output, 2, &scratch.group_out);
    Ok(())
}

/// Reframe a single Way submessage. Copies every existing field verbatim
/// EXCEPT fields 9 (lat) and 10 (lon) - those are stripped if present
/// (input was already a LocationsOnWays PBF) since we own the
/// coordinates. Then appends fresh fields 9 / 10 from `node_index`
/// lookups.
fn splice_way_locations(
    way_bytes: &[u8],
    node_index: &NodeIndex,
    scratch: &mut WayReframeScratch,
    stats: &mut WayReframeStats,
    shared_node_ids: Option<&crate::idset::IdSet>,
    member_way_ids: Option<&crate::idset::IdSet>,
    inject_prepass: bool,
) -> std::result::Result<(), String> {
    scratch.reframed_way.clear();
    scratch.refs.clear();
    scratch.packed_lats.clear();
    scratch.packed_lons.clear();
    scratch.pins.clear();

    let mut way_id: i64 = 0;
    let mut have_id = false;

    let mut way_cursor = Cursor::new(way_bytes);
    while let Some((wf, wt)) = way_cursor
        .read_tag()
        .map_err(|e| format!("reframe wtag: {e}"))?
    {
        match (wf, wt) {
            (1, WIRE_VARINT) => {
                way_id = way_cursor
                    .read_varint_i64()
                    .map_err(|e| format!("reframe wid: {e}"))?;
                have_id = true;
                encode_int64_field(&mut scratch.reframed_way, 1, way_id);
            }
            (8, WIRE_LEN) => {
                let refs_data = way_cursor
                    .read_len_delimited()
                    .map_err(|e| format!("reframe wrefs: {e}"))?;
                let mut cum: i64 = 0;
                for delta in PackedSint64Iter::new(refs_data) {
                    cum += delta;
                    scratch.refs.push(cum);
                }
                encode_tag(&mut scratch.reframed_way, 8, WIRE_LEN);
                encode_varint(&mut scratch.reframed_way, refs_data.len() as u64);
                scratch.reframed_way.extend_from_slice(refs_data);
            }
            (9 | 10 | 20, WIRE_LEN) => {
                let _ = way_cursor
                    .read_raw_field(wt)
                    .map_err(|e| format!("reframe wstrip: {e}"))?;
            }
            _ => {
                let raw = way_cursor
                    .read_raw_field(wt)
                    .map_err(|e| format!("reframe wskip: {e}"))?;
                encode_tag(&mut scratch.reframed_way, wf, wt);
                scratch.reframed_way.extend_from_slice(raw);
            }
        }
    }

    if !have_id {
        return Err("reframe: way without field 1 (id)".to_string());
    }

    if way_id < stats.min_way_id {
        stats.min_way_id = way_id;
    }
    if way_id > stats.max_way_id {
        stats.max_way_id = way_id;
    }
    stats.way_count += 1;
    scratch
        .member_ways
        .push(inject_prepass && member_way_ids.is_some_and(|ids| ids.get(way_id)));

    // Field 20 is a fixed-width ceil(ref_count/8) bitmap; size it up front so
    // trailing unpinned refs still contribute their bytes (the consumer
    // validates the length, and the external backend emits full width too).
    if inject_prepass {
        scratch.pins.resize(scratch.refs.len().div_ceil(8), 0);
    }
    let mut last_lat: i64 = 0;
    let mut last_lon: i64 = 0;
    for (idx, &node_id) in scratch.refs.iter().enumerate() {
        let (lat, lon, resolved) = match node_index.get(node_id) {
            Some(loc) => (i64::from(loc.0), i64::from(loc.1), true),
            None => {
                stats.missing_locations += 1;
                (0, 0, false)
            }
        };
        encode_varint(&mut scratch.packed_lats, zigzag_encode_64(lat - last_lat));
        encode_varint(&mut scratch.packed_lons, zigzag_encode_64(lon - last_lon));
        last_lat = lat;
        last_lon = lon;
        if inject_prepass && resolved && shared_node_ids.is_some_and(|ids| ids.get(node_id)) {
            scratch.pins[idx / 8] |= 1 << (idx % 8);
        }
    }

    if inject_prepass {
        super::inject_metrics::record_pins(&scratch.pins);
    }
    if !scratch.refs.is_empty() {
        encode_bytes_field(&mut scratch.reframed_way, 9, &scratch.packed_lats);
        encode_bytes_field(&mut scratch.reframed_way, 10, &scratch.packed_lons);
        if inject_prepass && scratch.pins.iter().any(|&byte| byte != 0) {
            encode_bytes_field(&mut scratch.reframed_way, 20, &scratch.pins);
        }
    }

    Ok(())
}

/// Field-5 payload for a way blob. Bits are in file order and identify ways
/// that participate in a multipolygon or boundary relation.
pub(super) fn way_members_payload(way_count: u64, members: &[bool]) -> Vec<u8> {
    let mut payload = vec![1];
    encode_varint(&mut payload, way_count);
    let mut bits = vec![0; members.len().div_ceil(8)];
    for (i, member) in members.iter().enumerate() {
        if *member {
            bits[i / 8] |= 1 << (i % 8);
        }
    }
    payload.extend_from_slice(&bits);
    payload
}
