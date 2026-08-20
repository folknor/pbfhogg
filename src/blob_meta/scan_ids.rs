//! Lightweight protobuf scanner: extract element type + ID range + bbox
//! without full PrimitiveBlock parsing.
//!
//! Uses `Cursor` from `protohoggr` for varint/tag/skip primitives.

use protohoggr::{Cursor, zigzag_decode_64};

use super::{BlobBbox, BlobIndex, ElemKind};

/// Scan decompressed PrimitiveBlock bytes to extract element type, ID range,
/// and spatial bbox (for node blobs).
///
/// Walks the protobuf wire format manually, reading only element IDs and
/// DenseNodes coordinates. Much cheaper than a full PrimitiveBlock parse
/// (skips string tables, tags, metadata, etc.).
///
/// Thin wrapper over [`scan_block_ids_checked`] that discards the
/// intra-blob-sortedness flag; use the checked variant when a consumer
/// needs to know whether the blob's elements are internally in canonical
/// OSM ID order (today: `sort` pass 1).
///
/// The scanner is deliberately NOT split into checked/unchecked variants:
/// the order check is one `osm_id_cmp` per element inside a loop dominated
/// by varint decode + zigzag + min/max updates, and on sorted data the
/// branch is perfectly predicted. Callers that discard the flag (writer
/// indexing, cat reframe) pay a cost that is noise next to the compression
/// work the scan overlaps with; a const-generic split would thread a mode
/// flag through every group scanner for no measured win.
pub(crate) fn scan_block_ids(raw: &[u8]) -> Option<BlobIndex> {
    scan_block_ids_checked(raw).map(|(index, _sorted)| index)
}

/// Result of scanning a single `PrimitiveGroup`: its id range/count/bbox plus
/// enough order information to reason about the whole blob's sortedness.
struct GroupScan {
    index: BlobIndex,
    /// ID of the first element encountered in wire order.
    first_id: i64,
    /// ID of the last element encountered in wire order.
    last_id: i64,
    /// True when every element in this group is in non-decreasing canonical
    /// OSM ID order relative to its predecessor.
    sorted: bool,
}

/// Scan a PrimitiveBlock for its [`BlobIndex`] and whether its elements are
/// internally sorted in canonical OSM ID order.
///
/// The second tuple element is `true` iff the blob is internally sorted:
/// every group is non-decreasing in canonical OSM ID order AND each group's
/// first element is `>=` the previous group's last element. This is what a
/// blob-level permutation sort's passthrough fast path implicitly assumes.
/// A blob that is internally out of order but whose `(min_id, max_id)` range
/// does not overlap its neighbours would otherwise slip through unnoticed -
/// `sort` uses this flag to route such a blob into its decode + re-encode
/// path (see `src/commands/sort/mod.rs` and CORRECTNESS.md).
///
/// Collects PrimitiveGroup data before processing so that granularity/offset
/// fields (which appear after groups in the wire format) are available for
/// coordinate conversion.
#[hotpath::measure]
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
pub(crate) fn scan_block_ids_checked(raw: &[u8]) -> Option<(BlobIndex, bool)> {
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
    let mut sorted = true;
    let mut prev_last: Option<i64> = None;
    for group_data in groups {
        // Refuse to index the whole blob when any group mixes element kinds:
        // `scan_primitive_group` stops at the first element field, so its
        // `kind` and `count` would describe only part of the group and the
        // cross-group guard below could never see it. Bailing out on the blob
        // (not just skipping the group) is the point - skipping would leave a
        // BlobIndex built from the blob's *other* groups, which understates
        // `count` just as badly. See `group_mixes_element_kinds`.
        if group_mixes_element_kinds(group_data) {
            return None;
        }
        if let Some(scan) = scan_primitive_group(group_data, granularity, lat_offset, lon_offset) {
            if !scan.sorted {
                sorted = false;
            }
            // Cross-group seam: the first id of this group must not sort
            // before the last id of the previous group.
            if let Some(prev_last_id) = prev_last
                && crate::osm_id::osm_id_cmp(scan.first_id, prev_last_id).is_lt()
            {
                sorted = false;
            }
            prev_last = Some(scan.last_id);
            result = Some(match result {
                None => scan.index,
                Some(mut prev) => {
                    // Mixed-type blobs cannot be safely indexed - the fast paths
                    // (raw passthrough, ID-range skip) trust `kind` as exact.
                    // Return None so mixed blobs fall through to full decode.
                    if prev.kind != scan.index.kind {
                        return None;
                    }
                    prev.min_id = prev.min_id.min(scan.index.min_id);
                    prev.max_id = prev.max_id.max(scan.index.max_id);
                    prev.count += scan.index.count;
                    prev.bbox = merge_bbox(prev.bbox, scan.index.bbox);
                    prev
                }
            });
        }
    }
    result.map(|index| (index, sorted))
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

/// Report whether a `PrimitiveGroup` carries more than one element field.
///
/// `PrimitiveGroup` has four element fields - 1 `nodes`, 2 `dense`, 3 `ways`,
/// 4 `relations` - and the OSM PBF spec says a group holds exactly one kind.
/// Nothing enforces it on the wire, and every scanner in this crate stops at
/// the first element field it meets, so a group holding e.g. ways *and*
/// relations gets indexed as `kind=Way` with a `count` covering only the ways.
/// The cross-group mixed-kind guard in [`scan_block_ids_checked`] never fires,
/// because the mixing is inside one group.
///
/// That lie is load-bearing downstream: `cat --type way` passes such a blob
/// through raw (relations leak into a way-only output) while `cat --type
/// relation` skips it (those relations vanish), `repack` only schedules it as
/// ways, and `renumber` splices only way IDs and leaves the off-kind elements
/// carrying un-renumbered IDs. So a group we cannot describe exactly must not
/// be described at all - the caller returns `None`, which every consumer
/// already handles as "no indexdata, decode it".
///
/// A group with zero or one element field is fine: zero contributes nothing
/// and one is exactly describable. Only two or more is the ambiguous case.
pub(super) fn group_mixes_element_kinds(raw: &[u8]) -> bool {
    let mut cur = Cursor::new(raw);
    let mut seen: u32 = 0;
    while let Ok(Some((tag, wire_type))) = cur.read_tag() {
        if matches!(tag, 1..=4) && wire_type == 2 {
            seen |= 1 << tag;
        }
        if cur.skip_field(wire_type).is_err() {
            break;
        }
    }
    seen.count_ones() > 1
}

/// Scan a PrimitiveGroup submessage for element type + IDs + bbox + order.
fn scan_primitive_group(
    raw: &[u8],
    granularity: i32,
    lat_offset: i64,
    lon_offset: i64,
) -> Option<GroupScan> {
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
) -> Option<GroupScan> {
    // Route through the shared DenseNodes parser rather than matching the
    // packed fields here. Protobuf permits a packed repeated field in either
    // of two wire forms - one length-delimited segment, or individually tagged
    // varints - and this scanner used to match only the length-delimited one.
    // An unpacked `id` array then read as empty, the scan returned `None`, and
    // the blob silently got no indexdata at all (or, where a sibling group did
    // index, a `count` that understated the blob). `WireDenseNodes::parse`
    // accepts both forms and errors on the one genuinely ambiguous shape (two
    // occurrences that both carry data). See `src/read/wire.rs::take_packed`.
    let dense = crate::read::wire::WireDenseNodes::parse(raw).ok()?;
    let lat_data = (!dense.lat_data.is_empty()).then_some(dense.lat_data);
    let lon_data = (!dense.lon_data.is_empty()).then_some(dense.lon_data);

    // Process IDs (required)
    let ids_data = dense.id_data;
    let mut id_cur = Cursor::new(ids_data);
    let mut min_id = i64::MAX;
    let mut max_id = i64::MIN;
    let mut current_id: i64 = 0;
    let mut count: u64 = 0;
    let mut first_id: i64 = 0;
    let mut prev_id: i64 = 0;
    let mut sorted = true;

    while !id_cur.is_empty() {
        let raw_val = id_cur.read_varint().ok()?;
        let delta = zigzag_decode_64(raw_val);
        current_id += delta;
        if count == 0 {
            first_id = current_id;
        } else if crate::osm_id::osm_id_cmp(current_id, prev_id).is_lt() {
            sorted = false;
        }
        prev_id = current_id;
        min_id = min_id.min(current_id);
        max_id = max_id.max(current_id);
        count += 1;
    }

    if count == 0 {
        return None;
    }

    // Process coordinates (optional - v1-only files may lack them in theory,
    // but in practice all DenseNodes have lat/lon).
    //
    // Use checked arithmetic throughout: `granularity` and the `*_offset`
    // fields are attacker-controllable (the spec allows any i32 for
    // granularity and any i64 for offsets), and unchecked `gran * raw`
    // wraps silently in release builds. A wrapped bbox would be
    // serialized into indexdata and trusted by every spatial filter
    // downstream. On overflow, drop the bbox for this blob rather than
    // the whole BlobIndex - the caller still gets id-range coverage,
    // spatial filters fall back to full decode for this blob.
    let bbox = lat_data.zip(lon_data).and_then(|(lats, lons)| {
        let gran = i64::from(granularity);
        let (min_raw_lat, max_raw_lat) = scan_packed_sint64_minmax(lats)?;
        let (min_raw_lon, max_raw_lon) = scan_packed_sint64_minmax(lons)?;

        let min_nano_lat = lat_offset.checked_add(gran.checked_mul(min_raw_lat)?)?;
        let max_nano_lat = lat_offset.checked_add(gran.checked_mul(max_raw_lat)?)?;
        let min_nano_lon = lon_offset.checked_add(gran.checked_mul(min_raw_lon)?)?;
        let max_nano_lon = lon_offset.checked_add(gran.checked_mul(max_raw_lon)?)?;

        // Convert nanodegrees to decimicrodegrees (floor for min, ceil
        // for max to keep the bbox conservative).
        Some(BlobBbox {
            min_lat: i32::try_from(floor_div(min_nano_lat, 100)).ok()?,
            max_lat: i32::try_from(ceil_div(max_nano_lat, 100)).ok()?,
            min_lon: i32::try_from(floor_div(min_nano_lon, 100)).ok()?,
            max_lon: i32::try_from(ceil_div(max_nano_lon, 100)).ok()?,
        })
    });

    Some(GroupScan {
        index: BlobIndex {
            kind: ElemKind::Node,
            min_id,
            max_id,
            count,
            bbox,
        },
        first_id,
        last_id: current_id,
        sorted,
    })
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
) -> Option<GroupScan> {
    let first_id = extract_element_id(first_msg)?;
    let mut min_id = first_id;
    let mut max_id = first_id;
    let mut count: u64 = 1;
    let mut last_id = first_id;
    let mut sorted = true;

    // Scan remaining messages in the group
    while let Some((tag, wire_type)) = rest.read_tag().ok()? {
        if tag == expected_tag && wire_type == 2 {
            let msg = rest.read_len_delimited().ok()?;
            if let Some(id) = extract_element_id(msg) {
                if crate::osm_id::osm_id_cmp(id, last_id).is_lt() {
                    sorted = false;
                }
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

    Some(GroupScan {
        index: BlobIndex {
            kind,
            min_id,
            max_id,
            count,
            bbox: None,
        },
        first_id,
        last_id,
        sorted,
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
#[allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::*;

    // --- Minimal protobuf wire-format builders --------------------------
    //
    // scan_block_ids_checked walks raw PrimitiveBlock bytes, so these tests
    // hand-encode the smallest blocks that exercise each order-check branch:
    // DenseNodes packed sint64 IDs, cross-group seams, and the repeated
    // Way/Relation/Node message path. Coordinates are omitted (bbox falls to
    // None); only the ID-order signal matters here.

    fn encode_varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                break;
            }
        }
    }

    fn zigzag(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    fn write_tag(field: u32, wire: u32, out: &mut Vec<u8>) {
        encode_varint(u64::from((field << 3) | wire), out);
    }

    fn write_len_delimited(field: u32, payload: &[u8], out: &mut Vec<u8>) {
        write_tag(field, 2, out);
        encode_varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    /// PrimitiveGroup body wrapping a DenseNodes message whose packed field-1
    /// IDs are `ids` (delta + zigzag encoded, as on the wire).
    fn dense_group_body(ids: &[i64]) -> Vec<u8> {
        let mut packed = Vec::new();
        let mut prev = 0i64;
        for &id in ids {
            encode_varint(zigzag(id - prev), &mut packed);
            prev = id;
        }
        let mut dense_msg = Vec::new();
        write_len_delimited(1, &packed, &mut dense_msg); // DenseNodes.id
        let mut group_body = Vec::new();
        write_len_delimited(2, &dense_msg, &mut group_body); // PrimitiveGroup.dense
        group_body
    }

    /// Single Node/Way/Relation message carrying only field 1 (id, plain
    /// int64 varint - not zigzag, matching the OSM PBF encoding of these IDs).
    fn element_msg(id: i64) -> Vec<u8> {
        let mut msg = Vec::new();
        write_tag(1, 0, &mut msg);
        encode_varint(id as u64, &mut msg);
        msg
    }

    /// PrimitiveGroup body holding repeated messages of `field`
    /// (1=Node, 3=Way, 4=Relation), one per id in wire order.
    fn repeated_group_body(field: u32, ids: &[i64]) -> Vec<u8> {
        let mut group_body = Vec::new();
        for &id in ids {
            let msg = element_msg(id);
            write_len_delimited(field, &msg, &mut group_body);
        }
        group_body
    }

    /// PrimitiveBlock wrapping the given PrimitiveGroup bodies in order.
    fn block_from_groups(groups: &[&[u8]]) -> Vec<u8> {
        let mut block = Vec::new();
        for g in groups {
            write_len_delimited(2, g, &mut block); // PrimitiveBlock.primitivegroup
        }
        block
    }

    // --- DenseNodes: single-group and cross-group order checks ----------

    #[test]
    fn dense_two_groups_in_order_stay_sorted() {
        let a = dense_group_body(&[1, 2]);
        let b = dense_group_body(&[3, 4]);
        let block = block_from_groups(&[&a, &b]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(sorted, "two ascending same-kind groups must be sorted");
        assert_eq!(index.kind, ElemKind::Node);
        assert_eq!(index.min_id, 1);
        assert_eq!(index.max_id, 4);
        assert_eq!(index.count, 4);
    }

    #[test]
    fn dense_cross_group_inversion_detected() {
        // Each group is internally monotone, but group B's first id (15) sorts
        // before group A's last id (20): the cross-group seam check must fire.
        let a = dense_group_body(&[10, 20]);
        let b = dense_group_body(&[15, 25]);
        let block = block_from_groups(&[&a, &b]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(
            !sorted,
            "an inversion across two same-kind groups must be detected"
        );
        assert_eq!(index.kind, ElemKind::Node);
        assert_eq!(index.min_id, 10);
        assert_eq!(index.max_id, 25);
        assert_eq!(index.count, 4);
    }

    #[test]
    fn dense_within_group_inversion_detected() {
        let g = dense_group_body(&[1, 3, 2, 4]);
        let block = block_from_groups(&[&g]);
        let (_, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(
            !sorted,
            "a within-group DenseNodes inversion must be detected"
        );
    }

    // --- Canonical negative order and negative inversions ---------------

    #[test]
    fn dense_canonical_negative_order_not_flagged() {
        // Canonical OSM order for negatives is ascending absolute value:
        // -1, -2, -3. This is the standard osmium layout and must NOT be
        // flagged as unsorted (osm_id_cmp keys on abs value for negatives).
        let g = dense_group_body(&[-1, -2, -3]);
        let block = block_from_groups(&[&g]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(sorted, "canonical -1,-2,-3 order must not be flagged");
        assert_eq!(index.kind, ElemKind::Node);
        assert_eq!(index.min_id, -3);
        assert_eq!(index.max_id, -1);
        assert_eq!(index.count, 3);
    }

    #[test]
    fn dense_negative_inversion_detected() {
        // -1 sorts before -2 in canonical order, so the sequence -2, -1 is an
        // inversion the checked scan must catch.
        let g = dense_group_body(&[-2, -1, -3]);
        let block = block_from_groups(&[&g]);
        let (_, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(!sorted, "a negative-ID inversion must be detected");
    }

    #[test]
    fn dense_cross_group_negative_seam_detected() {
        // Group A ends at -3 (abs 3), group B starts at -1 (abs 1). In
        // canonical order -1 precedes -3, so the seam is an inversion.
        let a = dense_group_body(&[-2, -3]);
        let b = dense_group_body(&[-1, -4]);
        let block = block_from_groups(&[&a, &b]);
        let (_, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(
            !sorted,
            "a negative cross-group seam inversion must be detected"
        );
    }

    // --- Repeated Way/Relation/non-dense-Node message path --------------

    #[test]
    fn way_repeated_in_order_sorted() {
        let g = repeated_group_body(3, &[1, 2, 3]);
        let block = block_from_groups(&[&g]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(sorted, "ascending ways must be sorted");
        assert_eq!(index.kind, ElemKind::Way);
        assert_eq!(index.min_id, 1);
        assert_eq!(index.max_id, 3);
        assert_eq!(index.count, 3);
    }

    #[test]
    fn way_repeated_inversion_detected() {
        let g = repeated_group_body(3, &[1, 3, 2]);
        let block = block_from_groups(&[&g]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(!sorted, "a within-group way inversion must be detected");
        assert_eq!(index.kind, ElemKind::Way);
    }

    #[test]
    fn relation_repeated_inversion_detected() {
        let g = repeated_group_body(4, &[5, 4]);
        let block = block_from_groups(&[&g]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(
            !sorted,
            "a within-group relation inversion must be detected"
        );
        assert_eq!(index.kind, ElemKind::Relation);
    }

    #[test]
    fn non_dense_node_repeated_inversion_detected() {
        // Rare non-dense Node encoding (repeated field 1 messages).
        let g = repeated_group_body(1, &[2, 1]);
        let block = block_from_groups(&[&g]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(
            !sorted,
            "a within-group non-dense node inversion must be detected"
        );
        assert_eq!(index.kind, ElemKind::Node);
    }

    // --- Unpacked wire form of DenseNodes.id ----------------------------

    /// PrimitiveGroup body whose DenseNodes writes `ids` in the UNPACKED wire
    /// form: each delta as its own varint-typed field 1 rather than one packed
    /// length-delimited segment. Both spellings are legal protobuf.
    fn dense_group_body_unpacked(ids: &[i64]) -> Vec<u8> {
        let mut dense_msg = Vec::new();
        let mut prev = 0i64;
        for &id in ids {
            write_tag(1, 0, &mut dense_msg);
            encode_varint(zigzag(id - prev), &mut dense_msg);
            prev = id;
        }
        let mut group_body = Vec::new();
        write_len_delimited(2, &dense_msg, &mut group_body);
        group_body
    }

    #[test]
    fn unpacked_dense_ids_are_indexed_not_read_as_absent() {
        // A single unpacked value is byte-identical to a one-element packed
        // segment, so this is the shape the scanner has to accept. Matching
        // WIRE_LEN only made `ids_data` None and the whole blob unindexable -
        // silently, since "no indexdata" is a legal state.
        let g = dense_group_body_unpacked(&[7]);
        let block = block_from_groups(&[&g]);
        let (index, _sorted) = scan_block_ids_checked(&block)
            .expect("an unpacked DenseNodes.id array must still produce a BlobIndex");
        assert_eq!(index.kind, ElemKind::Node);
        assert_eq!(index.count, 1);
        assert_eq!(index.min_id, 7);
        assert_eq!(index.max_id, 7);

        // Passing counterpart: the packed spelling of the same single node
        // must produce the identical index, so a reject-everything or
        // accept-nothing regression cannot pass both halves.
        let packed = block_from_groups(&[&dense_group_body(&[7])]);
        let (packed_index, _) = scan_block_ids_checked(&packed).unwrap();
        assert_eq!(packed_index.count, index.count);
        assert_eq!(packed_index.min_id, index.min_id);
    }

    // --- Mixed-kind groups ----------------------------------------------

    /// One PrimitiveGroup body carrying both a Way (field 3) and a Relation
    /// (field 4) message - spec-illegal but wire-legal, and undetectable by
    /// the cross-group kind guard because it is all one group.
    fn mixed_way_relation_group_body(way_id: i64, rel_id: i64) -> Vec<u8> {
        let mut group_body = Vec::new();
        write_len_delimited(3, &element_msg(way_id), &mut group_body);
        write_len_delimited(4, &element_msg(rel_id), &mut group_body);
        group_body
    }

    #[test]
    fn a_group_mixing_ways_and_relations_is_not_indexed() {
        let g = mixed_way_relation_group_body(1, 2);
        let block = block_from_groups(&[&g]);
        assert!(
            scan_block_ids_checked(&block).is_none(),
            "a group holding both a way and a relation cannot be described by a \
             single (kind, count) pair, so it must yield no indexdata at all - \
             indexing it as kind=Way with count=1 makes `cat --type relation` \
             drop the relation and `renumber` leave its id unrenumbered"
        );
    }

    #[test]
    fn a_mixed_group_poisons_the_whole_blob_not_just_itself() {
        // Skipping only the offending group would leave an index built from
        // the clean group - correct `kind`, but a `count` that understates the
        // blob. Every consumer treats `count` as exact.
        let clean = repeated_group_body(3, &[10, 11]);
        let mixed = mixed_way_relation_group_body(12, 2);
        let block = block_from_groups(&[&clean, &mixed]);
        assert!(
            scan_block_ids_checked(&block).is_none(),
            "one mixed group must make the whole blob unindexable"
        );
    }

    #[test]
    fn single_kind_groups_still_index() {
        // Passing counterpart to the two rejections above: the guard must not
        // have turned into "refuse every multi-group blob".
        let a = repeated_group_body(3, &[1, 2]);
        let b = repeated_group_body(3, &[3, 4]);
        let block = block_from_groups(&[&a, &b]);
        let (index, sorted) = scan_block_ids_checked(&block)
            .expect("two single-kind way groups must still be indexable");
        assert!(sorted);
        assert_eq!(index.kind, ElemKind::Way);
        assert_eq!(index.count, 4);
    }

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
