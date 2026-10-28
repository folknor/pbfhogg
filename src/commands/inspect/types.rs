//! Shared types and small classification helpers used across the inspect
//! submodules.

use crate::Element;
use crate::ElementKind;

// ---------------------------------------------------------------------------
// Block type classification
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockKind {
    Nodes,
    Ways,
    Relations,
    Mixed,
}

impl BlockKind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Nodes => "DenseNodes:",
            Self::Ways => "Ways:",
            Self::Relations => "Relations:",
            Self::Mixed => "Mixed:",
        }
    }

    pub(super) fn short_label(self) -> &'static str {
        match self {
            Self::Nodes => "nodes",
            Self::Ways => "ways",
            Self::Relations => "relations",
            Self::Mixed => "mixed",
        }
    }

    pub(super) fn from_elem_kind(kind: ElementKind) -> Self {
        match kind {
            ElementKind::Node => Self::Nodes,
            ElementKind::Way => Self::Ways,
            ElementKind::Relation => Self::Relations,
        }
    }

    /// Rank for standard ordering check (nodes < ways < relations).
    fn rank(self) -> u8 {
        match self {
            Self::Nodes => 1,
            Self::Ways => 2,
            Self::Relations => 3,
            Self::Mixed => 0, // always non-standard
        }
    }
}

// ---------------------------------------------------------------------------
// Accumulated stats per block type
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(super) struct TypeStats {
    pub(super) block_count: u64,
    pub(super) frame_bytes: u64,
    pub(super) element_count: u64,
}

// ---------------------------------------------------------------------------
// Ordering segment: contiguous run of same block type
// ---------------------------------------------------------------------------

pub(super) struct OrderingSegment {
    pub(super) kind: BlockKind,
    pub(super) first_block: u64,
    pub(super) last_block: u64,
}

// ---------------------------------------------------------------------------
// Ordering summary: bounded record of the segment sequence
// ---------------------------------------------------------------------------

const _: () = assert!(
    INSPECT_ORDERING_HEAD_SEGMENTS >= 4,
    "the head must reach the fourth segment, where a standard order can first break"
);

/// The block-kind sequence of a file, observed one data blob at a time,
/// held in bounded memory however many segments the file has.
///
/// A segment is a run of consecutive blobs of one kind. The summary keeps
/// the exact count of segments, whether the whole sequence is standard,
/// and the first `INSPECT_ORDERING_HEAD_SEGMENTS` segments with their exact
/// block ranges (the last retained one keeps growing while it is the
/// current run). A standard sequence has at most three segments, so the
/// segment where a file first leaves standard order is always in the head.
pub(super) struct OrderingSummary {
    current: Option<BlockKind>,
    segment_count: u64,
    standard: bool,
    head: Vec<OrderingSegment>,
}

impl OrderingSummary {
    pub(super) fn new() -> Self {
        Self {
            current: None,
            segment_count: 0,
            standard: true,
            head: Vec::new(),
        }
    }

    /// Record data blob `block` (1-based, in file order) of kind `kind`.
    pub(super) fn observe(&mut self, kind: BlockKind, block: u64) {
        if self.current == Some(kind) {
            if self.segment_count <= INSPECT_ORDERING_HEAD_SEGMENTS as u64
                && let Some(last) = self.head.last_mut()
            {
                last.last_block = block;
            }
            return;
        }
        // Standard: at most one run each of nodes, ways, relations, in that
        // order. Mixed (rank 0) is never standard.
        let prev_rank = self.current.map_or(0, BlockKind::rank);
        let rank = kind.rank();
        if rank == 0 || rank <= prev_rank {
            self.standard = false;
        }
        self.current = Some(kind);
        self.segment_count += 1;
        if self.head.len() < INSPECT_ORDERING_HEAD_SEGMENTS {
            self.head.push(OrderingSegment {
                kind,
                first_block: block,
                last_block: block,
            });
        }
    }

    /// Segments in the file, including those past the head.
    pub(super) fn segment_count(&self) -> u64 {
        self.segment_count
    }

    /// Whether the whole sequence is standard ordering.
    pub(super) fn is_standard(&self) -> bool {
        self.standard
    }

    /// The first segments, at most `INSPECT_ORDERING_HEAD_SEGMENTS`.
    pub(super) fn head(&self) -> &[OrderingSegment] {
        &self.head
    }

    /// Segments counted but not retained in the head.
    pub(super) fn omitted(&self) -> u64 {
        self.segment_count - self.head.len() as u64
    }
}

// ---------------------------------------------------------------------------
// Per-block info (--blocks)
// ---------------------------------------------------------------------------

use crate::tunables::{
    ANOMALY_LARGE_DENOMINATOR, ANOMALY_LARGE_NUMERATOR, ANOMALY_SMALL_DENOMINATOR,
    ANOMALY_SMALL_NUMERATOR, INSPECT_ORDERING_HEAD_SEGMENTS,
};

/// Describe the thresholds used by `inspect --anomalies`.
#[must_use]
pub(super) fn anomaly_threshold_description() -> String {
    let small_percent = 100 * ANOMALY_SMALL_NUMERATOR / ANOMALY_SMALL_DENOMINATOR;
    let large_percent = 100 * ANOMALY_LARGE_NUMERATOR / ANOMALY_LARGE_DENOMINATOR;
    format!("<{small_percent}% or >{large_percent}% of per-type median")
}

pub(super) struct BlockInfo {
    pub(super) number: u64,
    pub(super) kind: BlockKind,
    pub(super) elements: u64,
    pub(super) compressed: usize,
    pub(super) raw: Option<usize>,
}

// ---------------------------------------------------------------------------
// ID range per element type (--id-ranges)
// ---------------------------------------------------------------------------

pub(super) struct TypeIdRange {
    pub(super) min_id: i64,
    pub(super) max_id: i64,
    pub(super) monotonic: bool,
    prev_id: i64,
    pub(super) count: u64,
}

impl TypeIdRange {
    pub(super) fn new() -> Self {
        Self {
            min_id: i64::MAX,
            max_id: i64::MIN,
            monotonic: true,
            prev_id: i64::MIN,
            count: 0,
        }
    }

    pub(super) fn update(&mut self, id: i64) {
        if id < self.min_id {
            self.min_id = id;
        }
        if id > self.max_id {
            self.max_id = id;
        }
        if self.count > 0 && id <= self.prev_id {
            self.monotonic = false;
        }
        self.prev_id = id;
        self.count += 1;
    }

    /// Update from a blob's aggregated ID range (index-only mode).
    /// Monotonicity is checked at inter-blob granularity: each blob's min_id
    /// must exceed the previous blob's max_id for the same element type.
    pub(super) fn update_from_blob(&mut self, blob_min: i64, blob_max: i64, blob_count: u64) {
        if blob_min < self.min_id {
            self.min_id = blob_min;
        }
        if blob_max > self.max_id {
            self.max_id = blob_max;
        }
        if self.count > 0 && blob_min <= self.prev_id {
            self.monotonic = false;
        }
        self.prev_id = blob_max;
        self.count += blob_count;
    }

    pub(super) fn has_data(&self) -> bool {
        self.count > 0
    }
}

// ---------------------------------------------------------------------------
// Location-on-ways stats (--locations)
// ---------------------------------------------------------------------------

pub(super) struct LocationStats {
    pub(super) with_locations: u64,
    pub(super) without_locations: u64,
    pub(super) coord_counts: Vec<u32>,
}

// ---------------------------------------------------------------------------
// Extended stats: timestamp range, data bbox, metadata coverage
// ---------------------------------------------------------------------------

pub(super) struct DataBbox {
    pub(super) min_lat: i64, // nanodegrees
    pub(super) max_lat: i64,
    pub(super) min_lon: i64,
    pub(super) max_lon: i64,
}

impl DataBbox {
    fn new() -> Self {
        Self {
            min_lat: i64::MAX,
            max_lat: i64::MIN,
            min_lon: i64::MAX,
            max_lon: i64::MIN,
        }
    }

    fn update(&mut self, nano_lat: i64, nano_lon: i64) {
        self.min_lat = self.min_lat.min(nano_lat);
        self.max_lat = self.max_lat.max(nano_lat);
        self.min_lon = self.min_lon.min(nano_lon);
        self.max_lon = self.max_lon.max(nano_lon);
    }

    pub(super) fn has_data(&self) -> bool {
        self.min_lat != i64::MAX
    }
}

#[derive(Default)]
pub(super) struct MetadataCoverage {
    pub(super) total: u64,
    pub(super) has_version: u64,
    pub(super) has_timestamp: u64,
    pub(super) has_changeset: u64,
    pub(super) has_uid: u64,
    pub(super) has_user: u64,
}

impl MetadataCoverage {
    pub(super) fn all_have(&self, count: u64) -> bool {
        self.total > 0 && count == self.total
    }

    pub(super) fn some_have(&self, count: u64) -> bool {
        count > 0
    }
}

pub(super) struct ExtendedStats {
    pub(super) min_timestamp: i64, // milliseconds since epoch
    pub(super) max_timestamp: i64,
    pub(super) data_bbox: DataBbox,
    pub(super) metadata: MetadataCoverage,
    pub(super) objects_ordered: bool,
}

impl ExtendedStats {
    pub(super) fn new() -> Self {
        Self {
            min_timestamp: i64::MAX,
            max_timestamp: i64::MIN,
            data_bbox: DataBbox::new(),
            metadata: MetadataCoverage::default(),
            objects_ordered: true,
        }
    }

    fn update_timestamp(&mut self, millis: i64) {
        if millis != 0 {
            self.min_timestamp = self.min_timestamp.min(millis);
            self.max_timestamp = self.max_timestamp.max(millis);
        }
    }

    pub(super) fn has_timestamps(&self) -> bool {
        self.min_timestamp != i64::MAX
    }
}

// ---------------------------------------------------------------------------
// Mutable state for the scan loop, factored out to reduce cognitive complexity.
// ---------------------------------------------------------------------------

pub(super) struct ScanState {
    // Element counts
    pub(super) node_count: u64,
    pub(super) tagged_node_count: u64,
    pub(super) way_count: u64,
    pub(super) relation_count: u64,
    // Optional collectors
    pub(super) node_ids: Option<TypeIdRange>,
    pub(super) way_ids: Option<TypeIdRange>,
    pub(super) relation_ids: Option<TypeIdRange>,
    pub(super) loc_stats: Option<LocationStats>,
    pub(super) extended: Option<ExtendedStats>,
}

impl ScanState {
    pub(super) fn new(show_id_ranges: bool, show_locations: bool, extended: bool) -> Self {
        let show_id_ranges = show_id_ranges || extended;
        Self {
            node_count: 0,
            tagged_node_count: 0,
            way_count: 0,
            relation_count: 0,
            node_ids: if show_id_ranges {
                Some(TypeIdRange::new())
            } else {
                None
            },
            way_ids: if show_id_ranges {
                Some(TypeIdRange::new())
            } else {
                None
            },
            relation_ids: if show_id_ranges {
                Some(TypeIdRange::new())
            } else {
                None
            },
            loc_stats: if show_locations {
                Some(LocationStats {
                    with_locations: 0,
                    without_locations: 0,
                    coord_counts: Vec::new(),
                })
            } else {
                None
            },
            extended: if extended {
                Some(ExtendedStats::new())
            } else {
                None
            },
        }
    }

    /// Process one element: update counts, ID ranges, and location stats.
    /// Returns `true` for node, `false` for way/relation (for block type classification).
    pub(super) fn process_element(&mut self, element: &Element<'_>) -> (bool, bool, bool) {
        match *element {
            Element::DenseNode(ref dn) => {
                self.node_count += 1;
                if dn.tags().next().is_some() {
                    self.tagged_node_count += 1;
                }
                if let Some(ref mut ids) = self.node_ids {
                    ids.update(dn.id());
                }
                (true, false, false)
            }
            Element::Node(ref n) => {
                self.node_count += 1;
                if n.tags().next().is_some() {
                    self.tagged_node_count += 1;
                }
                if let Some(ref mut ids) = self.node_ids {
                    ids.update(n.id());
                }
                (true, false, false)
            }
            Element::Way(ref w) => {
                self.way_count += 1;
                if let Some(ref mut ids) = self.way_ids {
                    ids.update(w.id());
                }
                if let Some(ref mut stats) = self.loc_stats {
                    #[allow(clippy::cast_possible_truncation)]
                    let count = w.node_locations().count() as u32;
                    if count > 0 {
                        stats.with_locations += 1;
                        stats.coord_counts.push(count);
                    } else {
                        stats.without_locations += 1;
                    }
                }
                (false, true, false)
            }
            Element::Relation(ref r) => {
                self.relation_count += 1;
                if let Some(ref mut ids) = self.relation_ids {
                    ids.update(r.id());
                }
                (false, false, true)
            }
        }
    }
}

/// Update extended stats for a single element. Separate function to avoid
/// inflating cognitive complexity in `process_element`.
pub(super) fn update_extended_for_element(ext: &mut ExtendedStats, element: &Element<'_>) {
    ext.metadata.total += 1;
    match *element {
        Element::DenseNode(ref dn) => {
            ext.data_bbox.update(dn.nano_lat(), dn.nano_lon());
            update_extended_dense_node(ext, dn);
        }
        Element::Node(ref n) => {
            ext.data_bbox.update(n.nano_lat(), n.nano_lon());
            update_extended_info(ext, &n.info());
        }
        Element::Way(ref w) => update_extended_info(ext, &w.info()),
        Element::Relation(ref r) => update_extended_info(ext, &r.info()),
    }
}

fn update_extended_dense_node(ext: &mut ExtendedStats, dn: &crate::DenseNode<'_>) {
    // Presence by value, as libosmium's `detect_available_metadata` reads
    // it: a dense slot cannot mark a field absent. A negative version (the
    // Osmosis -1 sentinel) is a slot with no metadata at all; 0 is a slot
    // with no version - which is also what a node without metadata gets in
    // a block whose other nodes carry it - whose other fields count only
    // where non-zero, so padding counts as nothing.
    if let Some(info) = dn.info()
        && info.version() >= 0
    {
        if info.claimed_version().is_some() {
            ext.metadata.has_version += 1;
        }
        let ts = info.milli_timestamp();
        if ts != 0 {
            ext.metadata.has_timestamp += 1;
            ext.update_timestamp(ts);
        }
        if info.changeset() > 0 {
            ext.metadata.has_changeset += 1;
        }
        if info.uid() > 0 {
            ext.metadata.has_uid += 1;
        }
        if info.raw_user_sid() > 0 {
            ext.metadata.has_user += 1;
        }
    }
}

fn update_extended_info(ext: &mut ExtendedStats, info: &crate::Info<'_>) {
    if info.claimed_version().is_some() {
        ext.metadata.has_version += 1;
    }
    if let Some(ts) = info.milli_timestamp()
        && ts != 0
    {
        ext.metadata.has_timestamp += 1;
        ext.update_timestamp(ts);
    }
    if info.changeset().is_some() {
        ext.metadata.has_changeset += 1;
    }
    if info.uid().is_some() {
        ext.metadata.has_uid += 1;
    }
    if info.raw_user_sid().is_some() {
        ext.metadata.has_user += 1;
    }
}

// ---------------------------------------------------------------------------
// Block-level accumulators (bundled to reduce function argument count)
// ---------------------------------------------------------------------------

pub(super) struct BlockAccum {
    pub(super) node_type: TypeStats,
    pub(super) way_type: TypeStats,
    pub(super) relation_type: TypeStats,
    pub(super) mixed_type: TypeStats,
    pub(super) ordering: OrderingSummary,
    pub(super) block_infos: Option<Vec<BlockInfo>>,
}

impl BlockAccum {
    pub(super) fn new(show_blocks: bool) -> Self {
        Self {
            node_type: TypeStats::default(),
            way_type: TypeStats::default(),
            relation_type: TypeStats::default(),
            mixed_type: TypeStats::default(),
            ordering: OrderingSummary::new(),
            block_infos: if show_blocks { Some(Vec::new()) } else { None },
        }
    }
}

// ---------------------------------------------------------------------------
// Header metadata from OsmHeader blob
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(super) struct HeaderMeta {
    pub(super) writing_program: Option<String>,
    pub(super) required_features: Vec<String>,
    pub(super) optional_features: Vec<String>,
    pub(super) bbox: Option<(f64, f64, f64, f64)>,
    pub(super) replication_timestamp: Option<i64>,
    pub(super) replication_sequence: Option<i64>,
    pub(super) replication_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Main report struct
// ---------------------------------------------------------------------------

pub struct InspectReport {
    pub(super) file_name: String,
    pub(super) file_size: u64,
    pub(super) header_meta: HeaderMeta,
    pub(super) is_indexed: bool,
    pub(super) total_blocks: u64,
    pub(super) accum: BlockAccum,
    pub(super) state: ScanState,
}

// ---------------------------------------------------------------------------
// Shared classification / ordering helpers
// ---------------------------------------------------------------------------

pub(super) fn classify_block(has_nodes: bool, has_ways: bool, has_relations: bool) -> BlockKind {
    match (has_nodes, has_ways, has_relations) {
        (true, false, false) => BlockKind::Nodes,
        (false, true, false) => BlockKind::Ways,
        (false, false, true) => BlockKind::Relations,
        _ => BlockKind::Mixed,
    }
}

pub(super) fn anomaly_blocks(infos: &[BlockInfo]) -> Vec<(&BlockInfo, &'static str)> {
    let nodes = median_elements_for_kind(infos, BlockKind::Nodes);
    let ways = median_elements_for_kind(infos, BlockKind::Ways);
    let relations = median_elements_for_kind(infos, BlockKind::Relations);

    infos
        .iter()
        .filter_map(|info| {
            if info.kind == BlockKind::Mixed {
                return Some((info, "mixed"));
            }
            let median = match info.kind {
                BlockKind::Nodes => nodes,
                BlockKind::Ways => ways,
                BlockKind::Relations => relations,
                BlockKind::Mixed => None,
            };
            let median = median?;
            // Keep the displayed threshold and these comparisons tied to the
            // same ratios so CLI help and report output cannot drift.
            if info.elements.saturating_mul(ANOMALY_SMALL_DENOMINATOR)
                < median.saturating_mul(ANOMALY_SMALL_NUMERATOR)
            {
                Some((info, "small"))
            } else if info.elements
                > median.saturating_mul(ANOMALY_LARGE_NUMERATOR) / ANOMALY_LARGE_DENOMINATOR
            {
                Some((info, "large"))
            } else {
                None
            }
        })
        .collect()
}

fn median_elements_for_kind(infos: &[BlockInfo], kind: BlockKind) -> Option<u64> {
    let mut values: Vec<u64> = infos
        .iter()
        .filter(|info| info.kind == kind)
        .map(|info| info.elements)
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::{BlockKind, INSPECT_ORDERING_HEAD_SEGMENTS, OrderingSummary};

    const KINDS: [BlockKind; 4] = [
        BlockKind::Nodes,
        BlockKind::Ways,
        BlockKind::Relations,
        BlockKind::Mixed,
    ];

    /// The whole segment list, built the way the scan built it before the
    /// summary: one entry per run, with its block range.
    fn reference_segments(blobs: &[BlockKind]) -> Vec<(BlockKind, u64, u64)> {
        let mut segs: Vec<(BlockKind, u64, u64)> = Vec::new();
        for (i, &kind) in blobs.iter().enumerate() {
            let block = i as u64 + 1;
            match segs.last_mut() {
                Some(last) if last.0 == kind => last.2 = block,
                _ => segs.push((kind, block, block)),
            }
        }
        segs
    }

    fn reference_standard(segs: &[(BlockKind, u64, u64)]) -> bool {
        let mut prev = 0u8;
        for s in segs {
            let rank = s.0.rank();
            if rank == 0 || rank <= prev {
                return false;
            }
            prev = rank;
        }
        true
    }

    fn summarise(blobs: &[BlockKind]) -> OrderingSummary {
        let mut summary = OrderingSummary::new();
        for (i, &kind) in blobs.iter().enumerate() {
            summary.observe(kind, i as u64 + 1);
        }
        summary
    }

    fn assert_matches_reference(blobs: &[BlockKind]) {
        let segs = reference_segments(blobs);
        let summary = summarise(blobs);
        assert_eq!(summary.segment_count(), segs.len() as u64, "{blobs:?}");
        assert_eq!(
            summary.is_standard(),
            reference_standard(&segs),
            "{blobs:?}"
        );
        let keep = segs.len().min(INSPECT_ORDERING_HEAD_SEGMENTS);
        let head: Vec<(BlockKind, u64, u64)> = summary
            .head()
            .iter()
            .map(|s| (s.kind, s.first_block, s.last_block))
            .collect();
        assert_eq!(head, segs[..keep], "{blobs:?}");
        assert_eq!(summary.omitted(), (segs.len() - keep) as u64);
    }

    /// Every blob sequence of up to six blobs over all four kinds agrees
    /// with the full-list reference on count, verdict and ranges.
    #[test]
    fn short_sequences_match_the_full_list() {
        for len in 0..=6u32 {
            for code in 0..4usize.pow(len) {
                let mut c = code;
                let blobs: Vec<BlockKind> = (0..len)
                    .map(|_| {
                        let k = KINDS[c % 4];
                        c /= 4;
                        k
                    })
                    .collect();
                assert_matches_reference(&blobs);
            }
        }
    }

    /// Head boundaries: exactly N segments, N + 1, a long last retained
    /// run, and a long run past the head.
    #[test]
    fn head_boundaries_match_the_full_list() {
        let n = INSPECT_ORDERING_HEAD_SEGMENTS;
        let alternating = |segments: usize, last_run: usize| -> Vec<BlockKind> {
            let mut blobs = Vec::new();
            for i in 0..segments {
                let kind = if i % 2 == 0 {
                    BlockKind::Nodes
                } else {
                    BlockKind::Ways
                };
                let run = if i + 1 == segments { last_run } else { 1 };
                blobs.extend(std::iter::repeat_n(kind, run));
            }
            blobs
        };
        assert_matches_reference(&alternating(n, 1));
        assert_matches_reference(&alternating(n, 1000));
        assert_matches_reference(&alternating(n + 1, 1));
        assert_matches_reference(&alternating(n + 1, 1000));
        assert_matches_reference(&alternating(3 * n, 7));

        // The head's last segment keeps growing while it is the current run.
        let summary = summarise(&alternating(n, 1000));
        let last = summary.head().last().expect("head");
        assert_eq!(last.last_block - last.first_block, 999);
    }

    /// A Mixed blob, including a first one, makes the order non-standard;
    /// a header-only file is standard.
    #[test]
    fn mixed_is_never_standard() {
        assert!(!summarise(&[BlockKind::Mixed]).is_standard());
        assert!(!summarise(&[BlockKind::Nodes, BlockKind::Mixed]).is_standard());
        assert!(summarise(&[]).is_standard());
        assert!(
            summarise(&[BlockKind::Nodes, BlockKind::Ways, BlockKind::Relations]).is_standard()
        );
    }
}
