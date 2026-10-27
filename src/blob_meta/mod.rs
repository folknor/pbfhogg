//! Blob-level index: lightweight element type + ID range scanning and serialization.
//!
//! Used by the write path to embed per-blob metadata in the BlobHeader's `indexdata`
//! field, and by the merge read path to classify blobs without decompression.
//!
//! **Format versions:**
//! - v1 (26 bytes): element type, ID range, count.
//! - v2 (42 bytes): v1 fields + spatial bbox (min/max lat/lon in decimicrodegrees)
//!   for node blobs. Enables the pipeline to skip decompression of node blobs
//!   outside an extraction bbox. Spells "no bbox" as four zero words, which is
//!   also the exact bbox of a blob at (0, 0) - see [`BBOX_ABSENT`].
//! - v3 (42 bytes, current): identical layout, absence spelled as an inverted
//!   box instead of zeros, so every real bbox is representable.
//!
//! All three versions are read. New indexdata is written as v3.
//!
//! A version byte fixes the field's length: v1 is exactly 26 bytes, v2 and v3
//! exactly 42. A field whose length disagrees with its version byte is not an
//! index of either shape and is refused - see [`classify_wire_indexdata`].

use crate::ElementKind;

mod scan;
mod tag_index;
mod tally;

pub(crate) use scan::{
    try_scan_block_ids_and_tags, try_scan_block_ids_checked, try_scan_block_tags,
};
pub(crate) use tag_index::{TagIndex, encode_tag_index};
pub(crate) use tally::{AscendingTally, ClaimWalk, IdTally, Order, replay_refusal};

/// The `N` bytes of `data` at `at`, or `None` when they run past its end.
/// The fixed-width field read for the indexdata decoders: no slice-to-array
/// conversion whose impossible failure has to be swallowed.
#[inline]
fn le_at<const N: usize>(data: &[u8], at: usize) -> Option<[u8; N]> {
    data.get(at..)?.first_chunk::<N>().copied()
}

/// Scan a decompressed PrimitiveBlock for its [`BlobIndex`], or `None`.
///
/// `None` covers both "this blob cannot be described by one index" and "the
/// block bytes are malformed" - see [`try_scan_block_ids_checked`], which keeps
/// the two apart. Never a partial index: a parse failure anywhere in the block
/// yields `None`, not an index built from the part that parsed.
#[cfg(test)]
pub(crate) fn scan_block_ids(raw: &[u8]) -> Option<BlobIndex> {
    scan_block_ids_checked(raw).map(|(index, _sorted)| index)
}

/// A decompressed PrimitiveBlock's [`BlobIndex`] and whether its elements
/// are in canonical ID order, or `None`. `None` collapses
/// malformed bytes into "no index", which no production caller wants: they
/// use [`try_scan_block_ids_checked`] and treat its `Err` as an error.
#[cfg(test)]
pub(crate) fn scan_block_ids_checked(raw: &[u8]) -> Option<(BlobIndex, bool)> {
    try_scan_block_ids_checked(raw).ok().flatten() // ok-deliberate: advisory, see doc
}

/// Scan a decompressed PrimitiveBlock for its tag key set, or `None`.
///
/// Same collapse as [`scan_block_ids_checked`]; [`try_scan_block_tags`] keeps
/// malformed bytes as an `Err`. Never a truncated key set.
pub(crate) fn scan_block_tags(raw: &[u8]) -> Option<TagIndex> {
    try_scan_block_tags(raw).ok().flatten() // ok-deliberate: advisory, see doc
}

/// Scan IDs and tags in one walk, preserving malformed-block errors for
/// index-generating callers. A valid unsupported shape still has no index.
pub(crate) fn scan_block_ids_and_tags(
    raw: &[u8],
) -> crate::Result<(Option<BlobIndex>, Option<TagIndex>)> {
    let (index, tags) = try_scan_block_ids_and_tags(raw)?;
    Ok((index.map(|(index, _sorted)| index), tags))
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
/// Produced by [`try_scan_block_ids_checked`] from decompressed PrimitiveBlock bytes,
/// or deserialized from BlobHeader `indexdata`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlobIndex {
    pub kind: ElementKind,
    pub min_id: i64,
    pub max_id: i64,
    pub count: u64,
    /// Spatial bbox for node blobs (decimicrodegrees). `None` for way/relation
    /// blobs and for v1 indexdata.
    pub bbox: Option<BlobBbox>,
}

/// What a stored bbox claim amounts to against the blob's payload: the one
/// rule `check --index` reports on and the `sort` / `cat --dedupe` merge
/// engine keeps or replaces indexdata by.
///
/// A bbox claims to contain the blob's exact coordinates, `offset +
/// granularity * raw` nanodegrees, which is what the scan's outward-rounded
/// box contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BboxClaim {
    /// The stored index carries no bbox: no spatial claim (every v1 index,
    /// and v2 and v3 spelling absence).
    Absent,
    /// The payload holds no nodes, so there is no box to compare. A stored
    /// bbox exists only on an index claiming nodes, so its kind claim is
    /// already false, and that is the finding.
    NotNodes,
    /// The stored box contains the scanned one.
    Holds,
    /// The stored box, `claimed`, does not contain the scanned one,
    /// `observed`.
    TooSmall {
        claimed: BlobBbox,
        observed: BlobBbox,
    },
    /// A node coordinate lies outside the `i32` decimicrodegree range, so no
    /// box can contain it and the stored one, `claimed`, is false.
    OutOfRange { claimed: BlobBbox },
}

impl BlobIndex {
    /// Compare this stored index's bbox claim with `scanned`.
    ///
    /// `scanned` must be the scan's index of this blob's payload
    /// (`try_scan_block_ids_checked` and its siblings), not another stored
    /// index: only from the scan does a node index without a box mean a
    /// coordinate out of range rather than an index that never had one.
    pub(crate) fn bbox_claim(&self, scanned: &BlobIndex) -> BboxClaim {
        let Some(claimed) = self.bbox else {
            return BboxClaim::Absent;
        };
        if scanned.kind != ElementKind::Node {
            return BboxClaim::NotNodes;
        }
        match scanned.bbox {
            Some(observed) if claimed.contains(&observed) => BboxClaim::Holds,
            Some(observed) => BboxClaim::TooSmall { claimed, observed },
            None => BboxClaim::OutOfRange { claimed },
        }
    }
}

/// Indexdata wire format version.
const INDEX_VERSION_V1: u8 = 0x01;
/// v2: adds the spatial bbox, with all-zero meaning "no bbox".
const INDEX_VERSION_V2: u8 = 0x02;
/// Current version. Same 42-byte layout as v2; the only change is how an
/// absent bbox is spelled - see [`BBOX_ABSENT`].
const INDEX_VERSION: u8 = 0x03;

/// The bbox an index writes when it has none: an inverted (empty) box.
///
/// v2 spelled "no bbox" as four zero words, a sentinel drawn from the value
/// space it was describing - `(0, 0, 0, 0)` is also the exact, legal bbox of a
/// single-node blob at Null Island, and v2 cannot tell the two apart. The
/// consequence was small but real and one-directional: such a blob read back
/// with `bbox: None` and lost spatial filtering, and the encoding had no way to
/// say what was true.
///
/// v3 picks a value the field genuinely cannot take. A bbox is well-formed only
/// when `min <= max` on both axes, so `min = i32::MAX`, `max = i32::MIN` is
/// unconstructible as a real box - it describes the empty set - and every real
/// box, `(0, 0, 0, 0)` included, is now distinguishable from absence.
///
/// v2 files keep their original reading (all-zero means absent), because that
/// is what their writer meant; only v3 files get the exact one.
const BBOX_ABSENT: [i32; 4] = [i32::MAX, i32::MIN, i32::MAX, i32::MIN];

/// v1 serialized size: 1 version + 1 type + 8 min_id + 8 max_id + 8 count = 26 bytes.
pub(crate) const INDEX_SIZE_V1: usize = 26;

/// v2/v3 serialized size: v1 fields + 4×i32 bbox = 42 bytes.
pub const INDEX_SIZE: usize = 42;

/// The serialized size a version byte declares, or `None` for a version this
/// crate does not read.
///
/// The single table both [`classify_wire_indexdata`] and
/// [`BlobIndex::deserialize`] consult, so a new version cannot be readable by
/// one and length-unchecked by the other.
const fn version_size(version: u8) -> Option<usize> {
    match version {
        INDEX_VERSION_V1 => Some(INDEX_SIZE_V1),
        INDEX_VERSION_V2 | INDEX_VERSION => Some(INDEX_SIZE),
        _ => None,
    }
}

/// What a non-empty `BlobHeader.indexdata` field (field 2) amounts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireIndexdata {
    /// The field, stored in the fixed-size carrier every reader passes to
    /// [`BlobIndex::deserialize`]. A 26-byte field is zero-padded.
    Carrier([u8; INDEX_SIZE]),
    /// A length that is neither 26 nor 42.
    UnrecognisedLength(usize),
    /// A recognised length whose version byte declares the other one: a
    /// 26-byte field claiming v2/v3, or a 42-byte field claiming v1.
    VersionLengthMismatch,
}

/// Classify the raw bytes of a non-empty indexdata field.
///
/// # Why the carrier needs this
///
/// Readers hold indexdata in a `[u8; INDEX_SIZE]`, so a 26-byte field arrives
/// zero-padded and its original length is gone. `deserialize` must then read
/// the version byte to know the layout - and a 26-byte field whose version
/// byte says v3 would read four zero padding words as the bbox `(0, 0, 0, 0)`,
/// which under v3 is a real box at Null Island (absence is [`BBOX_ABSENT`]).
/// A spatial filter would then skip the blob for every region away from
/// (0, 0): a wrong answer manufactured by the padding.
///
/// Refusing every known version whose declared size differs from the field's
/// length makes the carrier lossless: for any carrier that holds a known
/// version, the original length is the one that version declares, so
/// deserialising the padded bytes reads exactly what the field said. A
/// version this crate does not know passes into the carrier unchanged and is
/// refused by `deserialize`, which is where unknown versions have always been
/// rejected.
pub(crate) fn classify_wire_indexdata(bytes: &[u8]) -> WireIndexdata {
    let len = bytes.len();
    if len != INDEX_SIZE_V1 && len != INDEX_SIZE {
        return WireIndexdata::UnrecognisedLength(len);
    }
    if let Some(declared) = bytes.first().copied().and_then(version_size)
        && declared != len
    {
        return WireIndexdata::VersionLengthMismatch;
    }
    let mut carrier = [0u8; INDEX_SIZE];
    carrier[..len].copy_from_slice(bytes);
    WireIndexdata::Carrier(carrier)
}

impl BlobIndex {
    /// Serialize to the 42-byte v3 indexdata format.
    ///
    /// Node blobs include their spatial bbox. Way/relation blobs, and node
    /// blobs whose coordinates could not be scanned, write [`BBOX_ABSENT`].
    pub fn serialize(&self) -> [u8; INDEX_SIZE] {
        let mut buf = [0u8; INDEX_SIZE];
        buf[0] = INDEX_VERSION;
        buf[1] = self.kind.ordinal();
        buf[2..10].copy_from_slice(&self.min_id.to_le_bytes());
        buf[10..18].copy_from_slice(&self.max_id.to_le_bytes());
        buf[18..26].copy_from_slice(&self.count.to_le_bytes());
        // Always write all four bbox words. Absence is the explicit
        // `BBOX_ABSENT` inverted box, never a run of zeros that a real box at
        // (0, 0) would also produce.
        let bbox = self.bbox.map_or(BBOX_ABSENT, |b| {
            [b.min_lat, b.max_lat, b.min_lon, b.max_lon]
        });
        buf[26..30].copy_from_slice(&bbox[0].to_le_bytes());
        buf[30..34].copy_from_slice(&bbox[1].to_le_bytes());
        buf[34..38].copy_from_slice(&bbox[2].to_le_bytes());
        buf[38..42].copy_from_slice(&bbox[3].to_le_bytes());
        buf
    }

    /// Deserialize from indexdata bytes. Accepts v1 (26 bytes), v2 and v3
    /// (42 bytes).
    ///
    /// `data` is either a field of exactly the length its version declares, or
    /// the 42-byte reader carrier built by [`classify_wire_indexdata`]. The
    /// second form is a carrier convention, not a raw-field rule: a 42-byte
    /// slice holding a v1 index is accepted here because that is what a padded
    /// 26-byte field looks like, and the classifier is the place a genuine
    /// 42-byte v1 *field* is refused.
    ///
    /// Returns `None` for an unrecognised version, a length that fits neither
    /// form, an unknown element kind, or an inverted ID range (`min_id >
    /// max_id`). An inverted range cannot describe any set of elements, and
    /// every range-based skip would act on it in the skipping direction.
    ///
    /// It does not bound `count` or the ID range by kind: a way or relation
    /// index with negative IDs, or a count no blob could hold, parses. Both
    /// are invalid input (ADR-0002) and the readers size nothing from `count`
    /// unclamped; `check --index` reports a claim its payload contradicts.
    pub fn deserialize(data: &[u8]) -> Option<Self> {
        let version = *data.first()?;
        let declared = version_size(version)?;
        if data.len() != declared && data.len() != INDEX_SIZE {
            return None;
        }
        let kind = ElementKind::from_ordinal(data[1])?;
        let min_id = i64::from_le_bytes(le_at(data, 2)?);
        let max_id = i64::from_le_bytes(le_at(data, 10)?);
        let count = u64::from_le_bytes(le_at(data, 18)?);
        if min_id > max_id {
            return None;
        }

        // v2/v3: parse the spatial bbox for node blobs. Keyed on the declared
        // size, never on the slice length: a carrier holding a v1 index is 42
        // bytes long and its last 16 are padding.
        let bbox = if declared == INDEX_SIZE && kind == ElementKind::Node {
            let min_lat = i32::from_le_bytes(le_at(data, 26)?);
            let max_lat = i32::from_le_bytes(le_at(data, 30)?);
            let min_lon = i32::from_le_bytes(le_at(data, 34)?);
            let max_lon = i32::from_le_bytes(le_at(data, 38)?);
            let words = [min_lat, max_lat, min_lon, max_lon];
            // v3 spells absence as the inverted box; v2 spelled it with zeros,
            // so v2 files keep that reading - and keep the ambiguity their
            // writer baked in. Any inverted box is absence under both versions,
            // since no well-formed bbox can be inverted.
            let absent = (version == INDEX_VERSION_V2 && words == [0, 0, 0, 0])
                || min_lat > max_lat
                || min_lon > max_lon;
            if absent {
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
    ///
    /// The kind and bbox are the blob's own indexdata claims and are not
    /// checked: a blob whose bbox understates its nodes, or that is
    /// mislabelled as nodes, is skipped with everything in it. See
    /// `reference/indexdata-trust.md` in the repository; `check --index`
    /// verifies a file's claims.
    pub fn with_node_bbox(mut self, bbox: BlobBbox) -> Self {
        self.node_bbox = Some(bbox);
        self
    }

    /// Returns true if the filter accepts blobs of the given element kind.
    pub(crate) fn wants(&self, kind: ElementKind) -> bool {
        match kind {
            ElementKind::Node => self.want_nodes,
            ElementKind::Way => self.want_ways,
            ElementKind::Relation => self.want_relations,
        }
    }

    /// Returns true if any part of this filter prunes through indexdata (a
    /// kind is unwanted, or a node bbox is set). A tag-key-only filter prunes
    /// through tagdata alone, so missing indexdata costs it nothing; the
    /// "filter cannot skip unindexed blobs" notes key off this.
    pub(crate) fn uses_index(&self) -> bool {
        !(self.want_nodes && self.want_ways && self.want_relations) || self.node_bbox.is_some()
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
            && index.kind == ElementKind::Node
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
    fn combined_scanner_error_keeps_the_scanner_failure() {
        let error = match scan_block_ids_and_tags(&[0x80]) {
            Err(error) => error,
            Ok(_) => panic!("malformed block should fail scanning"),
        };

        assert!(
            matches!(error.kind(), crate::error::ErrorKind::WireFormat { .. }),
            "a malformed block must surface the wire-format failure, got {:?}",
            error.kind()
        );
    }

    #[test]
    fn roundtrip_v2_way_no_bbox() {
        let index = BlobIndex {
            kind: ElementKind::Way,
            min_id: 100,
            max_id: 9999,
            count: 42,
            bbox: None,
        };
        let bytes = index.serialize();
        assert_eq!(bytes.len(), INDEX_SIZE);
        assert_eq!(bytes[0], INDEX_VERSION);
        let recovered = BlobIndex::deserialize(&bytes).expect("deserialize should succeed");
        assert_eq!(recovered.kind, ElementKind::Way);
        assert_eq!(recovered.min_id, 100);
        assert_eq!(recovered.max_id, 9999);
        assert_eq!(recovered.count, 42);
        assert!(recovered.bbox.is_none());
    }

    #[test]
    fn roundtrip_v2_node_with_bbox() {
        let index = BlobIndex {
            kind: ElementKind::Node,
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
        assert_eq!(recovered.kind, ElementKind::Node);
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
            kind: ElementKind::Node,
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
        assert_eq!(recovered.kind, ElementKind::Node);
        assert_eq!(recovered.min_id, 1);
        assert_eq!(recovered.max_id, 100);
        assert_eq!(recovered.count, 100);
        assert!(recovered.bbox.is_none(), "v1 data should have no bbox");
    }

    /// The bug v3 exists to fix: a genuine single-node blob at exactly
    /// (0, 0) has the exact bbox `(0, 0, 0, 0)`, which v2 wrote as four zero
    /// words and read back as "no bbox" - silently dropping spatial filtering
    /// for that blob, with no way for the encoding to say what was true.
    #[test]
    fn a_real_bbox_at_null_island_survives_the_roundtrip() {
        let index = BlobIndex {
            kind: ElementKind::Node,
            min_id: 1,
            max_id: 1,
            count: 1,
            bbox: Some(BlobBbox::new(0, 0, 0, 0)),
        };
        let bytes = index.serialize();
        let recovered = BlobIndex::deserialize(&bytes).expect("deserialize should succeed");
        let bbox = recovered
            .bbox
            .expect("a real (0,0,0,0) bbox must not read back as absence");
        assert_eq!(
            (bbox.min_lat, bbox.max_lat, bbox.min_lon, bbox.max_lon),
            (0, 0, 0, 0)
        );

        // Paired counterpart, so this is not satisfied by "always report a
        // bbox": a genuinely absent bbox must still read back as absent, and
        // must NOT be spelled with zeros any more.
        let none = BlobIndex {
            bbox: None,
            ..index
        };
        let none_bytes = none.serialize();
        assert_ne!(
            &none_bytes[26..42],
            &[0u8; 16],
            "v3 must not spell absence with the zero bytes that a real \
             (0,0,0,0) box also produces"
        );
        assert!(
            BlobIndex::deserialize(&none_bytes)
                .expect("deserialize should succeed")
                .bbox
                .is_none()
        );
    }

    /// v2 files keep the reading their writer intended. Decoding an old file
    /// under the new rules would change what existing indexed PBFs mean.
    #[test]
    fn v2_still_reads_all_zero_as_absence() {
        let mut bytes = BlobIndex {
            kind: ElementKind::Node,
            min_id: 1,
            max_id: 1,
            count: 1,
            bbox: Some(BlobBbox::new(0, 0, 0, 0)),
        }
        .serialize();
        bytes[0] = INDEX_VERSION_V2;
        bytes[26..42].fill(0);
        assert!(
            BlobIndex::deserialize(&bytes)
                .expect("v2 must still deserialize")
                .bbox
                .is_none(),
            "a v2 index's all-zero bbox means absence, as its writer meant"
        );

        // Counterpart: a v2 index with a real box still reads it.
        let mut bytes = BlobIndex {
            kind: ElementKind::Node,
            min_id: 1,
            max_id: 1,
            count: 1,
            bbox: Some(BlobBbox::new(10, 20, 30, 40)),
        }
        .serialize();
        bytes[0] = INDEX_VERSION_V2;
        let bbox = BlobIndex::deserialize(&bytes)
            .expect("v2 must still deserialize")
            .bbox
            .expect("a v2 index with a real box still carries it");
        assert_eq!(bbox.min_lat, 10);
        assert_eq!(bbox.max_lon, 40);
    }

    #[test]
    fn deserialize_rejects_bad_version() {
        let mut bytes = BlobIndex {
            kind: ElementKind::Node,
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
            kind: ElementKind::Node,
            min_id: 0,
            max_id: 0,
            count: 0,
            bbox: None,
        }
        .serialize();
        bytes[1] = 5; // invalid element type
        assert!(BlobIndex::deserialize(&bytes).is_none());
    }

    fn node_index_bytes(version: u8, len: usize) -> Vec<u8> {
        let mut f = vec![0u8; len];
        f[0] = version;
        f[1] = 0; // node
        f[2..10].copy_from_slice(&1i64.to_le_bytes());
        f[10..18].copy_from_slice(&5i64.to_le_bytes());
        f[18..26].copy_from_slice(&5u64.to_le_bytes());
        f
    }

    /// The length a version byte declares is binding. A 26-byte v3
    /// slice would otherwise read past its end (or, in the padded carrier,
    /// read zeros) as the real Null Island bbox `(0, 0, 0, 0)`.
    #[test]
    fn deserialize_rejects_a_length_its_version_does_not_declare() {
        assert!(BlobIndex::deserialize(&node_index_bytes(INDEX_VERSION, INDEX_SIZE_V1)).is_none());
        assert!(
            BlobIndex::deserialize(&node_index_bytes(INDEX_VERSION_V2, INDEX_SIZE_V1)).is_none()
        );
        assert!(BlobIndex::deserialize(&node_index_bytes(INDEX_VERSION_V1, 30)).is_none());
        // Counterparts: each version at its own length reads.
        assert!(
            BlobIndex::deserialize(&node_index_bytes(INDEX_VERSION_V1, INDEX_SIZE_V1)).is_some()
        );
        assert!(BlobIndex::deserialize(&node_index_bytes(INDEX_VERSION, INDEX_SIZE)).is_some());
        // And the v1 carrier (a 26-byte field padded to 42) reads with no bbox.
        let carrier = BlobIndex::deserialize(&node_index_bytes(INDEX_VERSION_V1, INDEX_SIZE))
            .expect("the padded v1 carrier is accepted");
        assert!(carrier.bbox.is_none());
    }

    #[test]
    fn classifier_refuses_a_version_length_mismatch() {
        for version in [INDEX_VERSION_V2, INDEX_VERSION] {
            assert_eq!(
                classify_wire_indexdata(&node_index_bytes(version, INDEX_SIZE_V1)),
                WireIndexdata::VersionLengthMismatch
            );
        }
        assert_eq!(
            classify_wire_indexdata(&node_index_bytes(INDEX_VERSION_V1, INDEX_SIZE)),
            WireIndexdata::VersionLengthMismatch
        );
        assert_eq!(
            classify_wire_indexdata(&[0x03; 30]),
            WireIndexdata::UnrecognisedLength(30)
        );
        // A v1 field is padded into the carrier, and the padding stays inert.
        let WireIndexdata::Carrier(c) =
            classify_wire_indexdata(&node_index_bytes(INDEX_VERSION_V1, INDEX_SIZE_V1))
        else {
            panic!("an honest v1 field must be accepted");
        };
        assert_eq!(BlobIndex::deserialize(&c).expect("v1 carrier").count, 5);
        // An unknown version passes into the carrier; deserialize refuses it.
        let WireIndexdata::Carrier(c) = classify_wire_indexdata(&[0xAB; INDEX_SIZE_V1]) else {
            panic!("unknown versions are deserialize's to reject");
        };
        assert!(BlobIndex::deserialize(&c).is_none());
    }

    /// An inverted ID range describes no set of elements, and every
    /// range-based skip would act on it in the skipping direction.
    #[test]
    fn deserialize_rejects_an_inverted_id_range() {
        let mut bytes = node_index_bytes(INDEX_VERSION, INDEX_SIZE);
        bytes[2..10].copy_from_slice(&9i64.to_le_bytes());
        bytes[10..18].copy_from_slice(&3i64.to_le_bytes());
        assert!(BlobIndex::deserialize(&bytes).is_none());
        // A single-ID range is fine.
        bytes[10..18].copy_from_slice(&9i64.to_le_bytes());
        assert!(BlobIndex::deserialize(&bytes).is_some());
    }

    #[test]
    fn roundtrip_negative_ids() {
        let index = BlobIndex {
            kind: ElementKind::Node,
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
            kind: ElementKind::Node,
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
            kind: ElementKind::Node,
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
            kind: ElementKind::Node,
            min_id: 400,
            max_id: 500,
            count: 100,
            bbox: None,
        };
        assert!(filter.wants_index(&no_bbox));

        // Way blob → always accepted (no spatial filtering)
        let way = BlobIndex {
            kind: ElementKind::Way,
            min_id: 1,
            max_id: 100,
            count: 100,
            bbox: None,
        };
        assert!(filter.wants_index(&way));
    }
}
