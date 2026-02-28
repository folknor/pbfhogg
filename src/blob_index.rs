//! Blob-level index: lightweight element type + ID range scanning and serialization.
//!
//! Used by the write path to embed per-blob metadata in the BlobHeader's `indexdata`
//! field, and by the merge read path to classify blobs without decompression.
//!
//! **Format versions:**
//! - v1 (26 bytes): element type, ID range, count.
//! - v2 (42 bytes): v1 fields + spatial bbox (min/max lat/lon in decimicrodegrees)
//!   for node blobs. Enables the pipeline to skip decompression of node blobs
//!   outside an extraction bbox.

/// Element type stored in a blob index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElemKind {
    Node,
    Way,
    Relation,
}

/// Spatial bounding box in decimicrodegrees (10⁻⁷ degrees).
///
/// Stored in the v2 indexdata format for node blobs. Enables spatial blob
/// filtering: the pipeline can skip decompression of node blobs whose bbox
/// does not intersect the extraction region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobBbox {
    pub(crate) min_lat: i32,
    pub(crate) max_lat: i32,
    pub(crate) min_lon: i32,
    pub(crate) max_lon: i32,
}

impl BlobBbox {
    /// Create a new bounding box from decimicrodegree coordinates.
    pub fn new(min_lat: i32, max_lat: i32, min_lon: i32, max_lon: i32) -> Self {
        Self { min_lat, max_lat, min_lon, max_lon }
    }

    /// Returns `true` if this bbox intersects `other` (AABB intersection test).
    pub fn intersects(&self, other: &BlobBbox) -> bool {
        self.min_lat <= other.max_lat
            && self.max_lat >= other.min_lat
            && self.min_lon <= other.max_lon
            && self.max_lon >= other.min_lon
    }
}

/// Blob-level index: element type, ID range, element count, and optional spatial bbox.
///
/// Produced by [`scan_block_ids`] from decompressed PrimitiveBlock bytes,
/// or deserialized from BlobHeader `indexdata`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlobIndex {
    pub kind: ElemKind,
    pub min_id: i64,
    pub max_id: i64,
    pub count: u64,
    /// Spatial bbox for node blobs (decimicrodegrees). `None` for way/relation
    /// blobs and for v1 indexdata.
    pub bbox: Option<BlobBbox>,
}

/// Indexdata wire format version.
const INDEX_VERSION_V1: u8 = 0x01;
/// Current version (v2 with spatial bbox).
const INDEX_VERSION: u8 = 0x02;

/// v1 serialized size: 1 version + 1 type + 8 min_id + 8 max_id + 8 count = 26 bytes.
const INDEX_SIZE_V1: usize = 26;

/// v2 serialized size: v1 fields + 4×i32 bbox = 42 bytes.
pub const INDEX_SIZE: usize = 42;

impl BlobIndex {
    /// Serialize to the 42-byte v2 indexdata format.
    ///
    /// Node blobs include their spatial bbox. Way/relation blobs have zero bbox fields.
    pub fn serialize(&self) -> [u8; INDEX_SIZE] {
        let mut buf = [0u8; INDEX_SIZE];
        buf[0] = INDEX_VERSION;
        buf[1] = match self.kind {
            ElemKind::Node => 0,
            ElemKind::Way => 1,
            ElemKind::Relation => 2,
        };
        buf[2..10].copy_from_slice(&self.min_id.to_le_bytes());
        buf[10..18].copy_from_slice(&self.max_id.to_le_bytes());
        buf[18..26].copy_from_slice(&self.count.to_le_bytes());
        if let Some(ref bbox) = self.bbox {
            buf[26..30].copy_from_slice(&bbox.min_lat.to_le_bytes());
            buf[30..34].copy_from_slice(&bbox.max_lat.to_le_bytes());
            buf[34..38].copy_from_slice(&bbox.min_lon.to_le_bytes());
            buf[38..42].copy_from_slice(&bbox.max_lon.to_le_bytes());
        }
        buf
    }

    /// Deserialize from indexdata bytes. Accepts both v1 (26 bytes) and v2 (42 bytes).
    ///
    /// Returns `None` if the data is invalid, too short, or has an unrecognized version.
    pub fn deserialize(data: &[u8]) -> Option<Self> {
        if data.len() < INDEX_SIZE_V1 {
            return None;
        }
        let version = data[0];
        if version != INDEX_VERSION_V1 && version != INDEX_VERSION {
            return None;
        }
        let kind = match data[1] {
            0 => ElemKind::Node,
            1 => ElemKind::Way,
            2 => ElemKind::Relation,
            _ => return None,
        };
        let min_id = i64::from_le_bytes(data[2..10].try_into().ok()?);
        let max_id = i64::from_le_bytes(data[10..18].try_into().ok()?);
        let count = u64::from_le_bytes(data[18..26].try_into().ok()?);

        // v2: parse spatial bbox for node blobs
        let bbox = if version == INDEX_VERSION && data.len() >= INDEX_SIZE && kind == ElemKind::Node {
            let min_lat = i32::from_le_bytes(data[26..30].try_into().ok()?);
            let max_lat = i32::from_le_bytes(data[30..34].try_into().ok()?);
            let min_lon = i32::from_le_bytes(data[34..38].try_into().ok()?);
            let max_lon = i32::from_le_bytes(data[38..42].try_into().ok()?);
            // All zeros means no meaningful bbox (way/relation or missing coordinates)
            if min_lat == 0 && max_lat == 0 && min_lon == 0 && max_lon == 0 {
                None
            } else {
                Some(BlobBbox { min_lat, max_lat, min_lon, max_lon })
            }
        } else {
            None
        };

        Some(BlobIndex { kind, min_id, max_id, count, bbox })
    }
}

// ---------------------------------------------------------------------------
// Blob-type filter
// ---------------------------------------------------------------------------

/// Filter for skipping blobs by element type during pipelined reads.
///
/// When a `BlobFilter` is set on an [`ElementReader`](crate::ElementReader),
/// the pipeline skips decompressing blobs whose element type (from indexdata)
/// does not match the filter. Files without indexdata are unaffected — all
/// blobs pass through.
///
/// # Example
/// ```no_run
/// use pbfhogg::{ElementReader, BlobFilter};
///
/// let reader = ElementReader::from_path("data.osm.pbf")?;
/// let reader = reader.with_blob_filter(BlobFilter::only_ways());
/// // Only way blobs are decompressed; node and relation blobs are skipped.
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy)]
pub struct BlobFilter {
    pub(crate) want_nodes: bool,
    pub(crate) want_ways: bool,
    pub(crate) want_relations: bool,
    /// Optional spatial bbox for node blob filtering. When set, node blobs
    /// whose bbox does not intersect this bbox are skipped.
    pub(crate) node_bbox: Option<BlobBbox>,
}

impl BlobFilter {
    /// Create a filter that accepts only the specified element types.
    pub fn new(want_nodes: bool, want_ways: bool, want_relations: bool) -> Self {
        Self { want_nodes, want_ways, want_relations, node_bbox: None }
    }

    /// Filter that accepts only node blobs.
    pub fn only_nodes() -> Self {
        Self { want_nodes: true, want_ways: false, want_relations: false, node_bbox: None }
    }

    /// Filter that accepts only way blobs.
    pub fn only_ways() -> Self {
        Self { want_nodes: false, want_ways: true, want_relations: false, node_bbox: None }
    }

    /// Filter that accepts only relation blobs.
    pub fn only_relations() -> Self {
        Self { want_nodes: false, want_ways: false, want_relations: true, node_bbox: None }
    }

    /// Add a spatial bbox filter for node blobs. Node blobs whose coordinate
    /// bbox does not intersect the given bbox are skipped (no decompression).
    ///
    /// Only effective on files with v2 indexdata. Blobs without spatial
    /// indexdata always pass through (conservative).
    pub fn with_node_bbox(mut self, bbox: BlobBbox) -> Self {
        self.node_bbox = Some(bbox);
        self
    }

    /// Returns true if the filter accepts blobs of the given element kind.
    pub(crate) fn wants(&self, kind: ElemKind) -> bool {
        match kind {
            ElemKind::Node => self.want_nodes,
            ElemKind::Way => self.want_ways,
            ElemKind::Relation => self.want_relations,
        }
    }

    /// Returns true if the filter accepts the blob described by `index`.
    ///
    /// Checks element type first, then spatial intersection for node blobs
    /// when a `node_bbox` is set and the index has spatial data.
    pub(crate) fn wants_index(&self, index: &BlobIndex) -> bool {
        if !self.wants(index.kind) {
            return false;
        }
        // Spatial check for node blobs only
        if let Some(ref filter_bbox) = self.node_bbox
            && index.kind == ElemKind::Node
            && let Some(ref blob_bbox) = index.bbox
        {
            return filter_bbox.intersects(blob_bbox);
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Lightweight protobuf scanner: extract element type + ID range + bbox
// without full PrimitiveBlock parsing.
// Uses Cursor from protohoggr for varint/tag/skip primitives.
// ---------------------------------------------------------------------------

use protohoggr::{zigzag_decode_64, Cursor};

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
                // Node (field 1, length-delimited) — rare, non-dense
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

    // Process coordinates (optional — v1-only files may lack them in theory,
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
    fn roundtrip_v2_way_no_bbox() {
        let index = BlobIndex {
            kind: ElemKind::Way,
            min_id: 100,
            max_id: 9999,
            count: 42,
            bbox: None,
        };
        let bytes = index.serialize();
        assert_eq!(bytes.len(), INDEX_SIZE);
        assert_eq!(bytes[0], INDEX_VERSION);
        let recovered = BlobIndex::deserialize(&bytes).expect("deserialize should succeed");
        assert_eq!(recovered.kind, ElemKind::Way);
        assert_eq!(recovered.min_id, 100);
        assert_eq!(recovered.max_id, 9999);
        assert_eq!(recovered.count, 42);
        assert!(recovered.bbox.is_none());
    }

    #[test]
    fn roundtrip_v2_node_with_bbox() {
        let index = BlobIndex {
            kind: ElemKind::Node,
            min_id: 1,
            max_id: 8000,
            count: 8000,
            bbox: Some(BlobBbox {
                min_lat: 510_000_000,
                max_lat: 520_000_000,
                min_lon: -1_000_000,
                max_lon: 10_000_000,
            }),
        };
        let bytes = index.serialize();
        let recovered = BlobIndex::deserialize(&bytes).expect("deserialize should succeed");
        assert_eq!(recovered.kind, ElemKind::Node);
        assert_eq!(recovered.min_id, 1);
        assert_eq!(recovered.max_id, 8000);
        assert_eq!(recovered.count, 8000);
        let bbox = recovered.bbox.expect("should have bbox");
        assert_eq!(bbox.min_lat, 510_000_000);
        assert_eq!(bbox.max_lat, 520_000_000);
        assert_eq!(bbox.min_lon, -1_000_000);
        assert_eq!(bbox.max_lon, 10_000_000);
    }

    #[test]
    fn v1_backward_compat() {
        // Simulate v1 data: 26 bytes with version 0x01
        let v1_index = BlobIndex {
            kind: ElemKind::Node,
            min_id: 1,
            max_id: 100,
            count: 100,
            bbox: None,
        };
        let mut v1_bytes = [0u8; INDEX_SIZE_V1];
        v1_bytes[0] = INDEX_VERSION_V1;
        v1_bytes[1] = 0; // Node
        v1_bytes[2..10].copy_from_slice(&v1_index.min_id.to_le_bytes());
        v1_bytes[10..18].copy_from_slice(&v1_index.max_id.to_le_bytes());
        v1_bytes[18..26].copy_from_slice(&v1_index.count.to_le_bytes());

        let recovered = BlobIndex::deserialize(&v1_bytes).expect("v1 should deserialize");
        assert_eq!(recovered.kind, ElemKind::Node);
        assert_eq!(recovered.min_id, 1);
        assert_eq!(recovered.max_id, 100);
        assert_eq!(recovered.count, 100);
        assert!(recovered.bbox.is_none(), "v1 data should have no bbox");
    }

    #[test]
    fn deserialize_rejects_bad_version() {
        let mut bytes = BlobIndex {
            kind: ElemKind::Node,
            min_id: 0,
            max_id: 0,
            count: 0,
            bbox: None,
        }
        .serialize();
        bytes[0] = 0xFF;
        assert!(BlobIndex::deserialize(&bytes).is_none());
    }

    #[test]
    fn deserialize_rejects_short_data() {
        assert!(BlobIndex::deserialize(&[0x02, 0x00]).is_none());
    }

    #[test]
    fn deserialize_rejects_bad_type() {
        let mut bytes = BlobIndex {
            kind: ElemKind::Node,
            min_id: 0,
            max_id: 0,
            count: 0,
            bbox: None,
        }
        .serialize();
        bytes[1] = 5; // invalid element type
        assert!(BlobIndex::deserialize(&bytes).is_none());
    }

    #[test]
    fn roundtrip_negative_ids() {
        let index = BlobIndex {
            kind: ElemKind::Node,
            min_id: -100,
            max_id: -1,
            count: 100,
            bbox: None,
        };
        let bytes = index.serialize();
        let recovered = BlobIndex::deserialize(&bytes).expect("deserialize should succeed");
        assert_eq!(recovered.min_id, -100);
        assert_eq!(recovered.max_id, -1);
    }

    #[test]
    fn bbox_intersects() {
        let a = BlobBbox::new(0, 100, 0, 100);
        let b = BlobBbox::new(50, 150, 50, 150);
        assert!(a.intersects(&b), "overlapping boxes should intersect");

        let c = BlobBbox::new(200, 300, 200, 300);
        assert!(!a.intersects(&c), "non-overlapping boxes should not intersect");

        // Edge-touching
        let d = BlobBbox::new(100, 200, 100, 200);
        assert!(a.intersects(&d), "edge-touching boxes should intersect");
    }

    #[test]
    fn bbox_intersects_negative_coords() {
        let a = BlobBbox::new(-900_000_000, -800_000_000, -1_800_000_000, -1_700_000_000);
        let b = BlobBbox::new(-850_000_000, -750_000_000, -1_750_000_000, -1_650_000_000);
        assert!(a.intersects(&b));

        let c = BlobBbox::new(100_000_000, 200_000_000, 100_000_000, 200_000_000);
        assert!(!a.intersects(&c));
    }

    #[test]
    fn wants_index_spatial_filter() {
        let filter = BlobFilter::new(true, true, true).with_node_bbox(
            BlobBbox::new(500_000_000, 520_000_000, 100_000_000, 120_000_000),
        );

        // Node blob inside filter bbox → accepted
        let inside = BlobIndex {
            kind: ElemKind::Node,
            min_id: 1,
            max_id: 100,
            count: 100,
            bbox: Some(BlobBbox::new(510_000_000, 515_000_000, 110_000_000, 115_000_000)),
        };
        assert!(filter.wants_index(&inside));

        // Node blob outside filter bbox → rejected
        let outside = BlobIndex {
            kind: ElemKind::Node,
            min_id: 200,
            max_id: 300,
            count: 100,
            bbox: Some(BlobBbox::new(-100_000_000, -50_000_000, -100_000_000, -50_000_000)),
        };
        assert!(!filter.wants_index(&outside));

        // Node blob without bbox (v1 data) → accepted (conservative)
        let no_bbox = BlobIndex {
            kind: ElemKind::Node,
            min_id: 400,
            max_id: 500,
            count: 100,
            bbox: None,
        };
        assert!(filter.wants_index(&no_bbox));

        // Way blob → always accepted (no spatial filtering)
        let way = BlobIndex {
            kind: ElemKind::Way,
            min_id: 1,
            max_id: 100,
            count: 100,
            bbox: None,
        };
        assert!(filter.wants_index(&way));
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
