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

mod scan_ids;
mod tag_index;

pub(crate) use scan_ids::scan_block_ids;
pub(crate) use tag_index::{TAG_INDEX_VERSION, TagIndex, scan_block_tags};

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
        Self {
            min_lat,
            max_lat,
            min_lon,
            max_lon,
        }
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
        let bbox = if version == INDEX_VERSION && data.len() >= INDEX_SIZE && kind == ElemKind::Node
        {
            let min_lat = i32::from_le_bytes(data[26..30].try_into().ok()?);
            let max_lat = i32::from_le_bytes(data[30..34].try_into().ok()?);
            let min_lon = i32::from_le_bytes(data[34..38].try_into().ok()?);
            let max_lon = i32::from_le_bytes(data[38..42].try_into().ok()?);
            // All zeros means no meaningful bbox (way/relation or missing coordinates)
            if min_lat == 0 && max_lat == 0 && min_lon == 0 && max_lon == 0 {
                None
            } else {
                Some(BlobBbox {
                    min_lat,
                    max_lat,
                    min_lon,
                    max_lon,
                })
            }
        } else {
            None
        };

        Some(BlobIndex {
            kind,
            min_id,
            max_id,
            count,
            bbox,
        })
    }
}

// ---------------------------------------------------------------------------
// Blob-type filter
// ---------------------------------------------------------------------------

/// Filter for skipping blobs by element type during pipelined reads.
///
/// When a `BlobFilter` is set on an [`ElementReader`](crate::ElementReader),
/// the pipeline skips decompressing blobs whose element type (from indexdata)
/// does not match the filter. Files without indexdata are unaffected - all
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
            want_nodes,
            want_ways,
            want_relations,
            node_bbox: None,
            required_tag_keys: None,
            required_tag_prefixes: None,
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
            prefixes
                .into_iter()
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
    /// (union semantics - each expression contributes either a key or prefix).
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
        assert!(
            !a.intersects(&c),
            "non-overlapping boxes should not intersect"
        );

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
        let filter = BlobFilter::new(true, true, true).with_node_bbox(BlobBbox::new(
            500_000_000,
            520_000_000,
            100_000_000,
            120_000_000,
        ));

        // Node blob inside filter bbox → accepted
        let inside = BlobIndex {
            kind: ElemKind::Node,
            min_id: 1,
            max_id: 100,
            count: 100,
            bbox: Some(BlobBbox::new(
                510_000_000,
                515_000_000,
                110_000_000,
                115_000_000,
            )),
        };
        assert!(filter.wants_index(&inside));

        // Node blob outside filter bbox → rejected
        let outside = BlobIndex {
            kind: ElemKind::Node,
            min_id: 200,
            max_id: 300,
            count: 100,
            bbox: Some(BlobBbox::new(
                -100_000_000,
                -50_000_000,
                -100_000_000,
                -50_000_000,
            )),
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
}
