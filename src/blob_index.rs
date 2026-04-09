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

    /// Returns `true` if `inner` is fully contained within this bbox.
    pub fn contains(&self, inner: &BlobBbox) -> bool {
        self.min_lat <= inner.min_lat
            && self.max_lat >= inner.max_lat
            && self.min_lon <= inner.min_lon
            && self.max_lon >= inner.max_lon
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
#[derive(Debug, Clone)]
pub struct BlobFilter {
    pub(crate) want_nodes: bool,
    pub(crate) want_ways: bool,
    pub(crate) want_relations: bool,
    /// Optional spatial bbox for node blob filtering. When set, node blobs
    /// whose bbox does not intersect this bbox are skipped.
    pub(crate) node_bbox: Option<BlobBbox>,
    /// Required tag keys for blob-level filtering. Blobs whose tag index
    /// contains none of these keys are skipped. `None` = no tag key filter.
    pub(crate) required_tag_keys: Option<Box<[Box<[u8]>]>>,
    /// Required tag key prefixes for blob-level filtering. Blobs whose tag
    /// index contains no key starting with any prefix are skipped.
    pub(crate) required_tag_prefixes: Option<Box<[Box<[u8]>]>>,
}

impl BlobFilter {
    /// Create a filter that accepts only the specified element types.
    pub fn new(want_nodes: bool, want_ways: bool, want_relations: bool) -> Self {
        Self {
            want_nodes, want_ways, want_relations,
            node_bbox: None, required_tag_keys: None, required_tag_prefixes: None,
        }
    }

    /// Filter that accepts only node blobs.
    pub fn only_nodes() -> Self {
        Self::new(true, false, false)
    }

    /// Filter that accepts only way blobs.
    pub fn only_ways() -> Self {
        Self::new(false, true, false)
    }

    /// Filter that accepts only relation blobs.
    pub fn only_relations() -> Self {
        Self::new(false, false, true)
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

    /// Add required tag keys for blob-level tag filtering.
    ///
    /// Blobs whose tag index contains none of these keys are skipped.
    /// Only effective on files with tag index data (BlobHeader field 4).
    /// Blobs without tag data always pass through (conservative).
    pub fn with_required_tag_keys(mut self, keys: Vec<String>) -> Self {
        self.required_tag_keys = Some(
            keys.into_iter()
                .map(|s| s.into_bytes().into_boxed_slice())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        self
    }

    /// Add required tag key prefixes for blob-level tag filtering.
    ///
    /// Blobs whose tag index contains no key starting with any of these
    /// prefixes are skipped (e.g. `addr:` matches `addr:city`, `addr:street`).
    pub fn with_required_tag_prefixes(mut self, prefixes: Vec<String>) -> Self {
        self.required_tag_prefixes = Some(
            prefixes.into_iter()
                .map(|s| s.into_bytes().into_boxed_slice())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        self
    }

    /// Returns true if a tag-based blob filter is configured.
    pub(crate) fn has_tag_filter(&self) -> bool {
        self.required_tag_keys.is_some() || self.required_tag_prefixes.is_some()
    }

    /// Returns true if the blob's tag index contains any required key or prefix.
    ///
    /// Returns true (conservative) if no tag filter is set. When both keys and
    /// prefixes are configured, the blob passes if ANY key OR ANY prefix matches
    /// (union semantics — each expression contributes either a key or prefix).
    pub(crate) fn wants_tag_index(&self, tag_index: &TagIndex) -> bool {
        if let Some(ref keys) = self.required_tag_keys
            && tag_index.has_any_key(keys)
        {
            return true;
        }
        if let Some(ref prefixes) = self.required_tag_prefixes
            && tag_index.has_any_prefix(prefixes)
        {
            return true;
        }
        // If no filter was configured, pass through (conservative)
        !self.has_tag_filter()
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
                    // Mixed-type blobs cannot be safely indexed — the fast paths
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

// ---------------------------------------------------------------------------
// Per-blob tag key index: lightweight scanning and serialization.
// ---------------------------------------------------------------------------

/// Tag key index version.
pub(crate) const TAG_INDEX_VERSION: u8 = 0x01;

/// Per-blob tag key index: the set of unique tag keys present in a blob.
///
/// Stored in BlobHeader field 4 as a variable-length binary blob.
/// Used by the pipeline to skip decompression of blobs that provably
/// lack required tag keys (e.g. a blob with no `highway` key can be
/// skipped when filtering for `highway=primary`).
#[derive(Debug, Clone)]
pub(crate) struct TagIndex {
    /// Sorted unique tag key byte strings.
    keys: Vec<Box<[u8]>>,
}

impl TagIndex {
    /// Serialize to the tag index wire format.
    ///
    /// Format: version (u8) + key_count (u16 LE) + repeated [key_len (u16 LE) + key bytes].
    #[hotpath::measure]
    pub fn serialize(&self) -> Vec<u8> {
        let total: usize = 3 + self.keys.iter().map(|k| 2 + k.len()).sum::<usize>();
        let mut buf = Vec::with_capacity(total);
        buf.push(TAG_INDEX_VERSION);
        #[allow(clippy::cast_possible_truncation)]
        let count = self.keys.len() as u16;
        buf.extend_from_slice(&count.to_le_bytes());
        for key in &self.keys {
            #[allow(clippy::cast_possible_truncation)]
            let key_len = key.len() as u16;
            buf.extend_from_slice(&key_len.to_le_bytes());
            buf.extend_from_slice(key);
        }
        buf
    }

    /// Deserialize from tag index bytes (BlobHeader field 4).
    ///
    /// Returns `None` if the data is invalid or has an unrecognized version.
    pub fn deserialize(data: &[u8]) -> Option<Self> {
        if data.len() < 3 {
            return None;
        }
        if data[0] != TAG_INDEX_VERSION {
            return None;
        }
        let key_count = u16::from_le_bytes(data[1..3].try_into().ok()?) as usize;
        let mut pos = 3;
        let mut keys = Vec::with_capacity(key_count);
        for _ in 0..key_count {
            if pos + 2 > data.len() {
                return None;
            }
            let key_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
            pos += 2;
            if pos + key_len > data.len() {
                return None;
            }
            keys.push(data[pos..pos + key_len].into());
            pos += key_len;
        }
        Some(TagIndex { keys })
    }

    /// Returns `true` if the tag index has no keys (blob contains no tagged elements).
    pub fn keys_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Returns `true` if any of the given keys is present in this tag index.
    pub fn has_any_key(&self, required: &[Box<[u8]>]) -> bool {
        required.iter().any(|rk| self.keys.binary_search(rk).is_ok())
    }

    /// Returns `true` if any key in this tag index starts with any of the given prefixes.
    pub fn has_any_prefix(&self, prefixes: &[Box<[u8]>]) -> bool {
        prefixes.iter().any(|prefix| {
            let idx = self.keys.partition_point(|k| k.as_ref() < prefix.as_ref());
            idx < self.keys.len() && self.keys[idx].starts_with(prefix)
        })
    }
}

/// Parse a StringTable message into byte slices for each entry.
///
/// StringTable is PrimitiveBlock field 1. Inside, each entry is field 1 (bytes).
fn parse_string_table(raw: &[u8]) -> Option<Vec<&[u8]>> {
    let mut cur = Cursor::new(raw);
    let mut entries = Vec::new();
    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        if tag == 1 && wire_type == 2 {
            entries.push(cur.read_len_delimited().ok()?);
        } else {
            cur.skip_field(wire_type).ok()?;
        }
    }
    Some(entries)
}

/// Scan decompressed PrimitiveBlock bytes to extract the set of unique tag keys.
///
/// Walks the protobuf wire format, parsing the StringTable and tag key indices
/// from all PrimitiveGroups. Returns a `TagIndex` with sorted unique keys.
///
/// Returns `None` if the block has no groups or cannot be parsed.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn scan_block_tags(raw: &[u8]) -> Option<TagIndex> {
    let mut cur = Cursor::new(raw);
    let mut string_table_data: Option<&[u8]> = None;
    let mut groups: Vec<&[u8]> = Vec::new();

    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        match (tag, wire_type) {
            (1, 2) => {
                // StringTable (field 1, length-delimited)
                string_table_data = Some(cur.read_len_delimited().ok()?);
            }
            (2, 2) => {
                // PrimitiveGroup (field 2, length-delimited)
                groups.push(cur.read_len_delimited().ok()?);
            }
            _ => {
                cur.skip_field(wire_type).ok()?;
            }
        }
    }

    let string_table = parse_string_table(string_table_data?)?;
    if groups.is_empty() {
        return None;
    }

    let mut key_indices: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for group_data in groups {
        scan_group_tag_keys(group_data, &mut key_indices);
    }

    // Resolve indices to string table entries
    let mut keys: Vec<Box<[u8]>> = key_indices
        .into_iter()
        .filter_map(|idx| {
            let i = idx as usize;
            if i < string_table.len() && !string_table[i].is_empty() {
                Some(string_table[i].into())
            } else {
                None
            }
        })
        .collect();
    keys.sort();
    keys.dedup();

    Some(TagIndex { keys })
}

/// Scan a PrimitiveGroup for tag key string-table indices.
fn scan_group_tag_keys(raw: &[u8], key_indices: &mut std::collections::HashSet<u32>) {
    let mut cur = Cursor::new(raw);
    while let Some((tag, wire_type)) = cur.read_tag().ok().flatten() {
        match (tag, wire_type) {
            (2, 2) => {
                // DenseNodes (field 2)
                if let Ok(data) = cur.read_len_delimited() {
                    scan_dense_node_tag_keys(data, key_indices);
                }
                return;
            }
            (1, 2) | (3, 2) | (4, 2) => {
                // Node (1), Way (3), Relation (4)
                if let Ok(msg) = cur.read_len_delimited() {
                    scan_element_tag_keys(msg, key_indices);
                }
                // Continue scanning remaining elements of same type
                scan_remaining_element_tag_keys(&mut cur, tag, key_indices);
                return;
            }
            _ => {
                if cur.skip_field(wire_type).is_err() {
                    return;
                }
            }
        }
    }
}

/// Scan DenseNodes keys_vals (field 10) for tag key indices.
///
/// Format: interleaved [key_sid, val_sid, key_sid, val_sid, ..., 0, ...]
/// where 0 separates nodes. Keys are at even positions within each node's
/// tag sequence (before the 0 delimiter).
fn scan_dense_node_tag_keys(raw: &[u8], key_indices: &mut std::collections::HashSet<u32>) {
    let mut cur = Cursor::new(raw);
    while let Some((tag, wire_type)) = cur.read_tag().ok().flatten() {
        if tag == 10 && wire_type == 2 {
            // keys_vals packed field
            if let Ok(data) = cur.read_len_delimited() {
                let mut kv_cur = Cursor::new(data);
                let mut is_key = true;
                while !kv_cur.is_empty() {
                    if let Ok(val) = kv_cur.read_varint() {
                        #[allow(clippy::cast_possible_truncation)]
                        let val = val as u32;
                        if val == 0 {
                            // Delimiter: next node's tags start
                            is_key = true;
                        } else if is_key {
                            key_indices.insert(val);
                            is_key = false;
                        } else {
                            // Value — skip
                            is_key = true;
                        }
                    } else {
                        break;
                    }
                }
            }
            return;
        }
        if cur.skip_field(wire_type).is_err() {
            return;
        }
    }
}

/// Scan a Way/Relation/Node message for tag key indices (field 2 = keys packed uint32).
fn scan_element_tag_keys(msg: &[u8], key_indices: &mut std::collections::HashSet<u32>) {
    let mut cur = Cursor::new(msg);
    while let Some((tag, wire_type)) = cur.read_tag().ok().flatten() {
        if tag == 2 && wire_type == 2 {
            // keys: packed uint32
            if let Ok(data) = cur.read_len_delimited() {
                let mut k_cur = Cursor::new(data);
                while !k_cur.is_empty() {
                    if let Ok(val) = k_cur.read_varint() {
                        #[allow(clippy::cast_possible_truncation)]
                        key_indices.insert(val as u32);
                    } else {
                        break;
                    }
                }
            }
            return;
        }
        if cur.skip_field(wire_type).is_err() {
            return;
        }
    }
}

/// Scan remaining element messages in a group after the first.
fn scan_remaining_element_tag_keys(
    cur: &mut Cursor<'_>,
    expected_tag: u32,
    key_indices: &mut std::collections::HashSet<u32>,
) {
    while let Some((tag, wire_type)) = cur.read_tag().ok().flatten() {
        if tag == expected_tag && wire_type == 2 {
            if let Ok(msg) = cur.read_len_delimited() {
                scan_element_tag_keys(msg, key_indices);
            }
        } else if cur.skip_field(wire_type).is_err() {
            return;
        }
    }
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

    // ----- TagIndex tests -----

    #[test]
    fn tag_index_roundtrip_empty() {
        let ti = TagIndex { keys: Vec::new() };
        let bytes = ti.serialize();
        let recovered = TagIndex::deserialize(&bytes).expect("should deserialize");
        assert!(recovered.keys.is_empty());
    }

    #[test]
    fn tag_index_roundtrip_single_key() {
        let ti = TagIndex {
            keys: vec![b"highway"[..].into()],
        };
        let bytes = ti.serialize();
        let recovered = TagIndex::deserialize(&bytes).expect("should deserialize");
        assert_eq!(recovered.keys.len(), 1);
        assert_eq!(&*recovered.keys[0], b"highway");
    }

    #[test]
    fn tag_index_roundtrip_many_keys() {
        let ti = TagIndex {
            keys: vec![
                b"addr:city"[..].into(),
                b"amenity"[..].into(),
                b"highway"[..].into(),
                b"name"[..].into(),
            ],
        };
        let bytes = ti.serialize();
        let recovered = TagIndex::deserialize(&bytes).expect("should deserialize");
        assert_eq!(recovered.keys.len(), 4);
        assert_eq!(&*recovered.keys[0], b"addr:city");
        assert_eq!(&*recovered.keys[1], b"amenity");
        assert_eq!(&*recovered.keys[2], b"highway");
        assert_eq!(&*recovered.keys[3], b"name");
    }

    #[test]
    fn tag_index_deserialize_rejects_bad_version() {
        let mut bytes = TagIndex { keys: Vec::new() }.serialize();
        bytes[0] = 0xFF;
        assert!(TagIndex::deserialize(&bytes).is_none());
    }

    #[test]
    fn tag_index_deserialize_rejects_truncated() {
        assert!(TagIndex::deserialize(&[0x01]).is_none());
        assert!(TagIndex::deserialize(&[]).is_none());
    }

    #[test]
    fn tag_index_has_any_key() {
        let ti = TagIndex {
            keys: vec![
                b"amenity"[..].into(),
                b"highway"[..].into(),
                b"name"[..].into(),
            ],
        };
        assert!(ti.has_any_key(&[b"highway"[..].into()]));
        assert!(ti.has_any_key(&[b"building"[..].into(), b"amenity"[..].into()]));
        assert!(!ti.has_any_key(&[b"building"[..].into()]));
        assert!(!ti.has_any_key(&[]));
    }

    #[test]
    fn tag_index_has_any_prefix() {
        let ti = TagIndex {
            keys: vec![
                b"addr:city"[..].into(),
                b"addr:street"[..].into(),
                b"highway"[..].into(),
            ],
        };
        assert!(ti.has_any_prefix(&[b"addr:"[..].into()]));
        assert!(ti.has_any_prefix(&[b"high"[..].into()]));
        assert!(!ti.has_any_prefix(&[b"building"[..].into()]));
        assert!(!ti.has_any_prefix(&[]));
    }
}
