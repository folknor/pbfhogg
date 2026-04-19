//! Lightweight protobuf scanner: extract element type + ID range + bbox
//! without full PrimitiveBlock parsing.
//!
//! Uses `Cursor` from `protohoggr` for varint/tag/skip primitives.

use protohoggr::{zigzag_decode_64, Cursor};

use super::{BlobBbox, BlobIndex, ElemKind};

/// Scan decompressed PrimitiveBlock bytes to extract element type, ID range,
/// and spatial bbox (for node blobs).
///
/// Walks the protobuf wire format manually, reading only element IDs and
/// DenseNodes coordinates. Much cheaper than a full PrimitiveBlock parse
/// (skips string tables, tags, metadata, etc.).
///
/// Collects PrimitiveGroup data before processing so that granularity/offset
/// fields (which appear after groups in the wire format) are available for
/// coordinate conversion.
#[hotpath::measure]
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
pub(crate) fn scan_block_ids(raw: &[u8]) -> Option<BlobIndex> {
    let mut cur = Cursor::new(raw);
    let mut groups: Vec<&[u8]> = Vec::new();
    let mut granularity: i32 = 100; // PrimitiveBlock default
    let mut lat_offset: i64 = 0;
    let mut lon_offset: i64 = 0;

    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        match (tag, wire_type) {
            (2, 2) => {
                // PrimitiveGroup (field 2, length-delimited)
                groups.push(cur.read_len_delimited().ok()?);
            }
            (17, 0) => {
                // granularity (field 17, varint, int32)
                granularity = cur.read_varint().ok()? as i32;
            }
            (19, 0) => {
                // lat_offset (field 19, varint, int64)
                lat_offset = cur.read_varint().ok()? as i64;
            }
            (20, 0) => {
                // lon_offset (field 20, varint, int64)
                lon_offset = cur.read_varint().ok()? as i64;
            }
            _ => {
                cur.skip_field(wire_type).ok()?;
            }
        }
    }

    let mut result: Option<BlobIndex> = None;
    for group_data in groups {
        if let Some(scan) = scan_primitive_group(group_data, granularity, lat_offset, lon_offset) {
            result = Some(match result {
                None => scan,
                Some(mut prev) => {
                    // Mixed-type blobs cannot be safely indexed - the fast paths
                    // (raw passthrough, ID-range skip) trust `kind` as exact.
                    // Return None so mixed blobs fall through to full decode.
                    if prev.kind != scan.kind {
                        return None;
                    }
                    prev.min_id = prev.min_id.min(scan.min_id);
                    prev.max_id = prev.max_id.max(scan.max_id);
                    prev.count += scan.count;
                    prev.bbox = merge_bbox(prev.bbox, scan.bbox);
                    prev
                }
            });
        }
    }
    result
}

/// Merge two optional bboxes, expanding to cover both.
fn merge_bbox(a: Option<BlobBbox>, b: Option<BlobBbox>) -> Option<BlobBbox> {
    match (a, b) {
        (Some(a), Some(b)) => Some(BlobBbox {
            min_lat: a.min_lat.min(b.min_lat),
            max_lat: a.max_lat.max(b.max_lat),
            min_lon: a.min_lon.min(b.min_lon),
            max_lon: a.max_lon.max(b.max_lon),
        }),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// Scan a PrimitiveGroup submessage for element type + IDs + bbox.
fn scan_primitive_group(
    raw: &[u8],
    granularity: i32,
    lat_offset: i64,
    lon_offset: i64,
) -> Option<BlobIndex> {
    let mut cur = Cursor::new(raw);

    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        match (tag, wire_type) {
            (2, 2) => {
                // DenseNodes (field 2, length-delimited)
                let data = cur.read_len_delimited().ok()?;
                return scan_dense_nodes(data, granularity, lat_offset, lon_offset);
            }
            (3, 2) => {
                // Way (field 3, length-delimited)
                let first_msg = cur.read_len_delimited().ok()?;
                return scan_repeated_element_ids(first_msg, &mut cur, 3, ElemKind::Way);
            }
            (4, 2) => {
                // Relation (field 4, length-delimited)
                let first_msg = cur.read_len_delimited().ok()?;
                return scan_repeated_element_ids(first_msg, &mut cur, 4, ElemKind::Relation);
            }
            (1, 2) => {
                // Node (field 1, length-delimited) - rare, non-dense
                let first_msg = cur.read_len_delimited().ok()?;
                return scan_repeated_element_ids(first_msg, &mut cur, 1, ElemKind::Node);
            }
            _ => {
                cur.skip_field(wire_type).ok()?;
            }
        }
    }
    None
}

/// Scan DenseNodes to extract min/max IDs, count, and spatial bbox.
///
/// DenseNodes fields:
/// - field 1: packed sint64 IDs (delta-encoded)
/// - field 8: packed sint64 lats (delta-encoded)
/// - field 9: packed sint64 lons (delta-encoded)
///
/// Coordinates are converted to decimicrodegrees using the PrimitiveBlock's
/// granularity and offsets: `decimicro = (offset + granularity * raw) / 100`.
/// Min values use floor division, max values use ceiling for conservative bounds.
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
fn scan_dense_nodes(
    raw: &[u8],
    granularity: i32,
    lat_offset: i64,
    lon_offset: i64,
) -> Option<BlobIndex> {
    let mut cur = Cursor::new(raw);
    let mut ids_data: Option<&[u8]> = None;
    let mut lat_data: Option<&[u8]> = None;
    let mut lon_data: Option<&[u8]> = None;

    // Collect all relevant packed fields
    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        match (tag, wire_type) {
            (1, 2) => ids_data = Some(cur.read_len_delimited().ok()?),
            (8, 2) => lat_data = Some(cur.read_len_delimited().ok()?),
            (9, 2) => lon_data = Some(cur.read_len_delimited().ok()?),
            _ => cur.skip_field(wire_type).ok()?,
        }
    }

    // Process IDs (required)
    let ids_data = ids_data?;
    let mut id_cur = Cursor::new(ids_data);
    let mut min_id = i64::MAX;
    let mut max_id = i64::MIN;
    let mut current_id: i64 = 0;
    let mut count: u64 = 0;

    while !id_cur.is_empty() {
        let raw_val = id_cur.read_varint().ok()?;
        let delta = zigzag_decode_64(raw_val);
        current_id += delta;
        min_id = min_id.min(current_id);
        max_id = max_id.max(current_id);
        count += 1;
    }

    if count == 0 {
        return None;
    }

    // Process coordinates (optional - v1-only files may lack them in theory,
    // but in practice all DenseNodes have lat/lon).
    let bbox = if let (Some(lats), Some(lons)) = (lat_data, lon_data) {
        let gran = i64::from(granularity);
        let (min_raw_lat, max_raw_lat) = scan_packed_sint64_minmax(lats)?;
        let (min_raw_lon, max_raw_lon) = scan_packed_sint64_minmax(lons)?;

        // Convert to decimicrodegrees: (offset + gran * raw) / 100
        // Floor for min, ceil for max (conservative bounds).
        let min_nano_lat = lat_offset + gran * min_raw_lat;
        let max_nano_lat = lat_offset + gran * max_raw_lat;
        let min_nano_lon = lon_offset + gran * min_raw_lon;
        let max_nano_lon = lon_offset + gran * max_raw_lon;

        Some(BlobBbox {
            min_lat: floor_div(min_nano_lat, 100) as i32,
            max_lat: ceil_div(max_nano_lat, 100) as i32,
            min_lon: floor_div(min_nano_lon, 100) as i32,
            max_lon: ceil_div(max_nano_lon, 100) as i32,
        })
    } else {
        None
    };

    Some(BlobIndex { kind: ElemKind::Node, min_id, max_id, count, bbox })
}

/// Iterate delta-encoded packed sint64, returning (min, max) of accumulated values.
fn scan_packed_sint64_minmax(data: &[u8]) -> Option<(i64, i64)> {
    let mut cur = Cursor::new(data);
    let mut acc: i64 = 0;
    let mut min_val = i64::MAX;
    let mut max_val = i64::MIN;

    while !cur.is_empty() {
        let raw_val = cur.read_varint().ok()?;
        let delta = zigzag_decode_64(raw_val);
        acc += delta;
        min_val = min_val.min(acc);
        max_val = max_val.max(acc);
    }

    if min_val <= max_val {
        Some((min_val, max_val))
    } else {
        None // empty data
    }
}

/// Floor division for signed integers (rounds toward negative infinity).
fn floor_div(a: i64, b: i64) -> i64 {
    let d = a / b;
    let r = a % b;
    if (r != 0) && ((r ^ b) < 0) { d - 1 } else { d }
}

/// Ceiling division for signed integers (rounds toward positive infinity).
fn ceil_div(a: i64, b: i64) -> i64 {
    let d = a / b;
    let r = a % b;
    if (r != 0) && ((r ^ b) >= 0) { d + 1 } else { d }
}

/// Scan repeated Way/Relation/Node messages to extract min/max IDs.
/// `first_msg` is the first message body; `rest` is positioned after it
/// in the parent group for scanning remaining messages.
fn scan_repeated_element_ids(
    first_msg: &[u8],
    rest: &mut Cursor<'_>,
    expected_tag: u32,
    kind: ElemKind,
) -> Option<BlobIndex> {
    let first_id = extract_element_id(first_msg)?;
    let mut min_id = first_id;
    let mut max_id = first_id;
    let mut count: u64 = 1;
    let mut last_id = first_id;

    // Scan remaining messages in the group
    while let Some((tag, wire_type)) = rest.read_tag().ok()? {
        if tag == expected_tag && wire_type == 2 {
            let msg = rest.read_len_delimited().ok()?;
            if let Some(id) = extract_element_id(msg) {
                min_id = min_id.min(id);
                max_id = max_id.max(id);
                last_id = id;
                count += 1;
            }
        } else {
            rest.skip_field(wire_type).ok()?;
        }
    }

    // For sorted PBFs, last_id == max_id, but be safe
    max_id = max_id.max(last_id);

    Some(BlobIndex {
        kind,
        min_id,
        max_id,
        count,
        bbox: None,
    })
}

/// Extract the element ID (field 1, varint/int64) from a Node/Way/Relation message.
#[allow(clippy::cast_possible_wrap)]
fn extract_element_id(msg: &[u8]) -> Option<i64> {
    let mut cur = Cursor::new(msg);
    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        if tag == 1 && wire_type == 0 {
            return Some(cur.read_varint().ok()? as i64);
        }
        cur.skip_field(wire_type).ok()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_ceil_div_correctness() {
        assert_eq!(floor_div(7, 2), 3);
        assert_eq!(floor_div(-7, 2), -4);
        assert_eq!(floor_div(6, 2), 3);
        assert_eq!(floor_div(-6, 2), -3);
        assert_eq!(ceil_div(7, 2), 4);
        assert_eq!(ceil_div(-7, 2), -3);
        assert_eq!(ceil_div(6, 2), 3);
        assert_eq!(ceil_div(-6, 2), -3);

        // nanodegrees to decimicrodegrees
        assert_eq!(floor_div(510_000_050, 100), 5_100_000);
        assert_eq!(ceil_div(510_000_050, 100), 5_100_001);
        assert_eq!(floor_div(-510_000_050, 100), -5_100_001);
        assert_eq!(ceil_div(-510_000_050, 100), -5_100_000);
    }
}
