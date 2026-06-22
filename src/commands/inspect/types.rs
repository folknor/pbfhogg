//! Shared types and small classification helpers used across the inspect
//! submodules.

use crate::Element;
use crate::blob_meta::ElemKind;

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

    pub(super) fn from_elem_kind(kind: ElemKind) -> Self {
        match kind {
            ElemKind::Node => Self::Nodes,
            ElemKind::Way => Self::Ways,
            ElemKind::Relation => Self::Relations,
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
    pub(super) first_block: u32,
    pub(super) last_block: u32,
}

// ---------------------------------------------------------------------------
// Per-block info (--blocks)
// ---------------------------------------------------------------------------

pub(super) struct BlockInfo {
    pub(super) number: u32,
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
    if let Some(info) = dn.info().filter(|i| i.version() != -1) {
        ext.metadata.has_version += 1;
        let ts = info.milli_timestamp();
        if ts != 0 {
            ext.metadata.has_timestamp += 1;
            ext.update_timestamp(ts);
        }
        if info.changeset() != 0 {
            ext.metadata.has_changeset += 1;
        }
        if info.uid() != 0 {
            ext.metadata.has_uid += 1;
        }
        if info.raw_user_sid() > 0 {
            ext.metadata.has_user += 1;
        }
    }
}

fn update_extended_info(ext: &mut ExtendedStats, info: &crate::Info<'_>) {
    if info.version().is_some() {
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
    pub(super) segments: Vec<OrderingSegment>,
    pub(super) block_infos: Option<Vec<BlockInfo>>,
}

impl BlockAccum {
    pub(super) fn new(show_blocks: bool) -> Self {
        Self {
            node_type: TypeStats::default(),
            way_type: TypeStats::default(),
            relation_type: TypeStats::default(),
            mixed_type: TypeStats::default(),
            segments: Vec::new(),
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

/// Standard ordering: at most one run each of [Nodes, Ways, Relations] in that order.
/// Mixed blocks or repeated/out-of-order segments make it non-standard.
pub(super) fn is_standard_ordering(segments: &[OrderingSegment]) -> bool {
    let mut prev_rank = 0u8;
    for seg in segments {
        let rank = seg.kind.rank();
        if rank == 0 || rank <= prev_rank {
            return false;
        }
        prev_rank = rank;
    }
    true
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
            // Anomalous if block is <50% or >150% of the per-type median.
            if info.elements.saturating_mul(2) < median {
                Some((info, "small"))
            } else if info.elements > median.saturating_mul(3) / 2 {
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
