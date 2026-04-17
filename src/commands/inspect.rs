//! Inspect PBF file: comprehensive metadata, block breakdown, ordering analysis.
//! Also provides `show_element` for displaying a single element by ID.

use std::io::{Read, Write};
use std::path::Path;

use super::{read_blob_header_only, read_raw_frame};
use crate::blob::{
    decode_blob_to_headerblock, decompress_blob_data_into,
    BlobKind,
};
use crate::blob_index::ElemKind;
use crate::file_reader::FileReader;
use crate::Element;

use super::Result;

// ---------------------------------------------------------------------------
// Block type classification
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockKind {
    Nodes,
    Ways,
    Relations,
    Mixed,
}

impl BlockKind {
    fn label(self) -> &'static str {
        match self {
            Self::Nodes => "DenseNodes:",
            Self::Ways => "Ways:",
            Self::Relations => "Relations:",
            Self::Mixed => "Mixed:",
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::Nodes => "nodes",
            Self::Ways => "ways",
            Self::Relations => "relations",
            Self::Mixed => "mixed",
        }
    }

    fn from_elem_kind(kind: ElemKind) -> Self {
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
struct TypeStats {
    block_count: u64,
    frame_bytes: u64,
    element_count: u64,
}

// ---------------------------------------------------------------------------
// Ordering segment: contiguous run of same block type
// ---------------------------------------------------------------------------

struct OrderingSegment {
    kind: BlockKind,
    first_block: u32,
    last_block: u32,
}

// ---------------------------------------------------------------------------
// Per-block info (--blocks)
// ---------------------------------------------------------------------------

struct BlockInfo {
    number: u32,
    kind: BlockKind,
    elements: u64,
    compressed: usize,
    raw: Option<usize>,
}

// ---------------------------------------------------------------------------
// ID range per element type (--id-ranges)
// ---------------------------------------------------------------------------

struct TypeIdRange {
    min_id: i64,
    max_id: i64,
    monotonic: bool,
    prev_id: i64,
    count: u64,
}

impl TypeIdRange {
    fn new() -> Self {
        Self {
            min_id: i64::MAX,
            max_id: i64::MIN,
            monotonic: true,
            prev_id: i64::MIN,
            count: 0,
        }
    }

    fn update(&mut self, id: i64) {
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
    fn update_from_blob(&mut self, blob_min: i64, blob_max: i64, blob_count: u64) {
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

    fn has_data(&self) -> bool {
        self.count > 0
    }
}

// ---------------------------------------------------------------------------
// Location-on-ways stats (--locations)
// ---------------------------------------------------------------------------

struct LocationStats {
    with_locations: u64,
    without_locations: u64,
    coord_counts: Vec<u32>,
}

// ---------------------------------------------------------------------------
// Extended stats: timestamp range, data bbox, metadata coverage
// ---------------------------------------------------------------------------

struct DataBbox {
    min_lat: i64, // nanodegrees
    max_lat: i64,
    min_lon: i64,
    max_lon: i64,
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

    fn has_data(&self) -> bool {
        self.min_lat != i64::MAX
    }
}

#[derive(Default)]
struct MetadataCoverage {
    total: u64,
    has_version: u64,
    has_timestamp: u64,
    has_changeset: u64,
    has_uid: u64,
    has_user: u64,
}

impl MetadataCoverage {
    fn all_have(&self, count: u64) -> bool {
        self.total > 0 && count == self.total
    }

    fn some_have(&self, count: u64) -> bool {
        count > 0
    }
}

struct ExtendedStats {
    min_timestamp: i64, // milliseconds since epoch
    max_timestamp: i64,
    data_bbox: DataBbox,
    metadata: MetadataCoverage,
    objects_ordered: bool,
}

impl ExtendedStats {
    fn new() -> Self {
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

    fn has_timestamps(&self) -> bool {
        self.min_timestamp != i64::MAX
    }
}

// ---------------------------------------------------------------------------
// Mutable state for the scan loop, factored out to reduce cognitive complexity.
// ---------------------------------------------------------------------------

struct ScanState {
    // Element counts
    node_count: u64,
    tagged_node_count: u64,
    way_count: u64,
    relation_count: u64,
    // Optional collectors
    node_ids: Option<TypeIdRange>,
    way_ids: Option<TypeIdRange>,
    relation_ids: Option<TypeIdRange>,
    loc_stats: Option<LocationStats>,
    extended: Option<ExtendedStats>,
}

impl ScanState {
    fn new(show_id_ranges: bool, show_locations: bool, extended: bool) -> Self {
        let show_id_ranges = show_id_ranges || extended;
        Self {
            node_count: 0,
            tagged_node_count: 0,
            way_count: 0,
            relation_count: 0,
            node_ids: if show_id_ranges { Some(TypeIdRange::new()) } else { None },
            way_ids: if show_id_ranges { Some(TypeIdRange::new()) } else { None },
            relation_ids: if show_id_ranges { Some(TypeIdRange::new()) } else { None },
            loc_stats: if show_locations {
                Some(LocationStats {
                    with_locations: 0,
                    without_locations: 0,
                    coord_counts: Vec::new(),
                })
            } else {
                None
            },
            extended: if extended { Some(ExtendedStats::new()) } else { None },
        }
    }

    /// Process one element: update counts, ID ranges, and location stats.
    /// Returns `true` for node, `false` for way/relation (for block type classification).
    fn process_element(&mut self, element: &Element<'_>) -> (bool, bool, bool) {
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
fn update_extended_for_element(ext: &mut ExtendedStats, element: &Element<'_>) {
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

struct BlockAccum {
    node_type: TypeStats,
    way_type: TypeStats,
    relation_type: TypeStats,
    mixed_type: TypeStats,
    segments: Vec<OrderingSegment>,
    block_infos: Option<Vec<BlockInfo>>,
}

impl BlockAccum {
    fn new(show_blocks: bool) -> Self {
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
struct HeaderMeta {
    writing_program: Option<String>,
    required_features: Vec<String>,
    optional_features: Vec<String>,
    bbox: Option<(f64, f64, f64, f64)>,
    replication_timestamp: Option<i64>,
    replication_sequence: Option<i64>,
    replication_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Main report struct
// ---------------------------------------------------------------------------

pub struct InspectReport {
    file_name: String,
    file_size: u64,
    header_meta: HeaderMeta,
    is_indexed: bool,
    total_blocks: u64,
    accum: BlockAccum,
    state: ScanState,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[hotpath::measure]
pub fn inspect(
    path: &Path,
    show_blocks: bool,
    show_id_ranges: bool,
    show_locations: bool,
    extended: bool,
    direct_io: bool,
) -> Result<InspectReport> {
    // Index-only fast path: skip decompression when all blobs have indexdata.
    // --locations and --extended require per-element data, so they need full decode.
    if !show_locations
        && !extended
        && let Some(report) =
            try_index_only_scan(path, show_blocks, show_id_ranges, direct_io)?
    {
        return Ok(report);
    }

    full_decode_scan(path, show_blocks, show_id_ranges, show_locations, extended, direct_io)
}

// ---------------------------------------------------------------------------
// Index-only scan: reads frame headers, skips blob data entirely
// ---------------------------------------------------------------------------

/// Attempt an index-only scan. Returns `None` if any OsmData blob lacks indexdata,
/// signalling the caller to fall back to full decode.
fn try_index_only_scan(
    path: &Path,
    show_blocks: bool,
    show_id_ranges: bool,
    direct_io: bool,
) -> Result<Option<InspectReport>> {
    let meta = std::fs::metadata(path)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let mut reader = FileReader::open(path, direct_io)?;
    let mut offset = 0u64;
    let mut header_meta = HeaderMeta::default();

    let mut accum = BlockAccum::new(show_blocks);
    let mut block_number = 0u32;
    let mut state = ScanState::new(show_id_ranges, false, false);
    let mut total_data_blobs = 0u64;

    while let Some(info) = read_blob_header_only(&mut reader, &mut offset)? {
        match info.blob_type {
            BlobKind::OsmHeader => {
                let mut blob_bytes = vec![0u8; info.data_size];
                reader.read_exact(&mut blob_bytes)?;
                offset += info.data_size as u64;
                let header = decode_blob_to_headerblock(&blob_bytes)?;
                header_meta = extract_header_metadata(&header);
            }
            BlobKind::OsmData => {
                total_data_blobs += 1;
                let Some(index) = info.index else {
                    return Ok(None); // fallback to full decode
                };
                block_number += 1;
                accumulate_from_index(
                    &index, &info, block_number, &mut state, &mut accum,
                );
                reader.skip(info.data_size as u64)?;
                offset += info.data_size as u64;
            }
            BlobKind::Unknown(_) => {
                reader.skip(info.data_size as u64)?;
                offset += info.data_size as u64;
            }
        }
    }

    Ok(Some(InspectReport {
        file_name,
        file_size: meta.len(),
        header_meta,
        is_indexed: true,
        total_blocks: total_data_blobs,
        accum,
        state,
    }))
}

/// Update accumulators from a single blob's index metadata (no decompression).
fn accumulate_from_index(
    index: &crate::blob_index::BlobIndex,
    info: &super::BlobHeaderInfo,
    block_number: u32,
    state: &mut ScanState,
    accum: &mut BlockAccum,
) {
    let kind = BlockKind::from_elem_kind(index.kind);

    // Element counts
    match index.kind {
        ElemKind::Node => state.node_count += index.count,
        ElemKind::Way => state.way_count += index.count,
        ElemKind::Relation => state.relation_count += index.count,
    }

    // ID ranges (inter-blob monotonicity)
    let ids = match index.kind {
        ElemKind::Node => &mut state.node_ids,
        ElemKind::Way => &mut state.way_ids,
        ElemKind::Relation => &mut state.relation_ids,
    };
    if let Some(ids) = ids {
        ids.update_from_blob(index.min_id, index.max_id, index.count);
    }

    // Per-type stats
    let stats = match kind {
        BlockKind::Nodes => &mut accum.node_type,
        BlockKind::Ways => &mut accum.way_type,
        BlockKind::Relations => &mut accum.relation_type,
        BlockKind::Mixed => &mut accum.mixed_type,
    };
    stats.block_count += 1;
    stats.frame_bytes += info.frame_size as u64;
    stats.element_count += index.count;

    // Ordering segments
    if let Some(last) = accum.segments.last_mut().filter(|s| s.kind == kind) {
        last.last_block = block_number;
    } else {
        accum.segments.push(OrderingSegment {
            kind,
            first_block: block_number,
            last_block: block_number,
        });
    }

    // Per-block detail
    if let Some(ref mut infos) = accum.block_infos {
        infos.push(BlockInfo {
            number: block_number,
            kind,
            elements: index.count,
            compressed: info.data_size,
            raw: None,
        });
    }
}

/// Extract header metadata fields from a parsed `HeaderBlock`.
fn extract_header_metadata(header: &crate::HeaderBlock) -> HeaderMeta {
    HeaderMeta {
        writing_program: header.writing_program().map(String::from),
        required_features: header
            .required_features()
            .iter()
            .map(ToString::to_string)
            .collect(),
        optional_features: header
            .optional_features()
            .iter()
            .map(ToString::to_string)
            .collect(),
        bbox: header.bbox().map(|bb| (bb.left, bb.bottom, bb.right, bb.top)),
        replication_timestamp: header.osmosis_replication_timestamp(),
        replication_sequence: header.osmosis_replication_sequence_number(),
        replication_url: header.osmosis_replication_base_url().map(String::from),
    }
}

// ---------------------------------------------------------------------------
// Full decode scan (original path)
// ---------------------------------------------------------------------------

fn full_decode_scan(
    path: &Path,
    show_blocks: bool,
    show_id_ranges: bool,
    show_locations: bool,
    extended: bool,
    direct_io: bool,
) -> Result<InspectReport> {
    let meta = std::fs::metadata(path)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let mut reader = FileReader::open(path, direct_io)?;
    let mut offset = 0u64;
    let mut decompress_buf = Vec::new();
    let mut header_meta = HeaderMeta::default();

    // Indexdata tracking
    let mut indexed_blobs = 0u64;
    let mut total_data_blobs = 0u64;

    let mut accum = BlockAccum::new(show_blocks);
    let mut block_number = 0u32;
    let mut state = ScanState::new(show_id_ranges, show_locations, extended);
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    while let Some(frame) = read_raw_frame(&mut reader, &mut offset)? {
        match frame.blob_type {
            BlobKind::OsmHeader => {
                let header = decode_blob_to_headerblock(frame.blob_bytes())?;
                header_meta = extract_header_metadata(&header);
            }
            BlobKind::OsmData => {
                total_data_blobs += 1;
                if frame.index.is_some() {
                    indexed_blobs += 1;
                }
                block_number += 1;
                scan_data_blob(&frame, &mut decompress_buf, &mut st_scratch, &mut gr_scratch, &mut state, block_number, &mut accum)?;
            }
            BlobKind::Unknown(_) => {}
        }
    }

    let is_indexed = total_data_blobs > 0 && indexed_blobs == total_data_blobs;

    // Compute objects_ordered from ordering segments + ID monotonicity.
    if let Some(ref mut ext) = state.extended {
        let type_ordered = is_standard_ordering(&accum.segments);
        let ids_monotonic = state
            .node_ids
            .as_ref()
            .is_none_or(|r| !r.has_data() || r.monotonic)
            && state
                .way_ids
                .as_ref()
                .is_none_or(|r| !r.has_data() || r.monotonic)
            && state
                .relation_ids
                .as_ref()
                .is_none_or(|r| !r.has_data() || r.monotonic);
        ext.objects_ordered = type_ordered && ids_monotonic;
    }

    Ok(InspectReport {
        file_name,
        file_size: meta.len(),
        header_meta,
        is_indexed,
        total_blocks: total_data_blobs,
        accum,
        state,
    })
}

/// Decompress, parse, and scan one OsmData blob. Updates all accumulators.
fn scan_data_blob(
    frame: &super::RawBlobFrame,
    decompress_buf: &mut Vec<u8>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
    state: &mut ScanState,
    block_number: u32,
    accum: &mut BlockAccum,
) -> Result<()> {
    let frame_size = frame.frame_bytes.len();
    let compressed_size = frame.blob_bytes().len();

    decompress_blob_data_into(frame.blob_bytes(), decompress_buf)?;
    let raw_size = decompress_buf.len();
    let block = crate::block::PrimitiveBlock::new_with_scratch(
        bytes::Bytes::copy_from_slice(decompress_buf),
        st_scratch,
        gr_scratch,
    )?;

    let mut has_nodes = false;
    let mut has_ways = false;
    let mut has_relations = false;
    let mut block_elements = 0u64;

    let need_metadata = state.extended.is_some();
    if need_metadata {
        for element in block.elements() {
            block_elements += 1;
            let (n, w, r) = state.process_element(&element);
            has_nodes |= n;
            has_ways |= w;
            has_relations |= r;
            if let Some(ref mut ext) = state.extended {
                update_extended_for_element(ext, &element);
            }
        }
    } else {
        for element in block.elements_skip_metadata() {
            block_elements += 1;
            let (n, w, r) = state.process_element(&element);
            has_nodes |= n;
            has_ways |= w;
            has_relations |= r;
        }
    }

    let kind = classify_block(has_nodes, has_ways, has_relations);

    // Update per-type stats
    let stats = match kind {
        BlockKind::Nodes => &mut accum.node_type,
        BlockKind::Ways => &mut accum.way_type,
        BlockKind::Relations => &mut accum.relation_type,
        BlockKind::Mixed => &mut accum.mixed_type,
    };
    stats.block_count += 1;
    stats.frame_bytes += frame_size as u64;
    stats.element_count += block_elements;

    // Update ordering segments
    if let Some(last) = accum.segments.last_mut().filter(|s| s.kind == kind) {
        last.last_block = block_number;
    } else {
        accum.segments.push(OrderingSegment {
            kind,
            first_block: block_number,
            last_block: block_number,
        });
    }

    // Per-block detail
    if let Some(ref mut infos) = accum.block_infos {
        infos.push(BlockInfo {
            number: block_number,
            kind,
            elements: block_elements,
            compressed: compressed_size,
            raw: Some(raw_size),
        });
    }

    Ok(())
}

fn classify_block(has_nodes: bool, has_ways: bool, has_relations: bool) -> BlockKind {
    match (has_nodes, has_ways, has_relations) {
        (true, false, false) => BlockKind::Nodes,
        (false, true, false) => BlockKind::Ways,
        (false, false, true) => BlockKind::Relations,
        _ => BlockKind::Mixed,
    }
}

// ---------------------------------------------------------------------------
// Report output
// ---------------------------------------------------------------------------

impl InspectReport {
    /// Print the full inspect report.
    ///
    /// `block_limit`: `None` = no block detail, `Some(0)` = distribution stats
    /// + full listing, `Some(N)` = distribution stats + first/last N blocks.
    pub fn print_report(&mut self, block_limit: Option<usize>) {
        self.print_report_filtered(block_limit, false);
    }

    /// Print the inspect report with optional anomalies-only block detail.
    pub fn print_report_filtered(&mut self, block_limit: Option<usize>, anomalies_only: bool) {
        self.print_header();
        println!();
        self.print_blocks_summary();
        println!();
        self.print_elements();
        println!();
        self.print_ordering();

        if let Some(ref infos) = self.accum.block_infos {
            println!();
            Self::print_block_distribution(infos);
            println!();
            if anomalies_only {
                let selected = anomaly_blocks(infos);
                println!(
                    "Block anomalies ({} of {} - <50% or >150% of per-type median):",
                    selected.len(),
                    infos.len()
                );
                Self::print_block_table_with_reason(&selected);
            } else {
                let selected: Vec<&BlockInfo> = infos.iter().collect();
                let limit = block_limit.unwrap_or(0);
                Self::print_block_table_refs(&selected, limit);
            }
        }
        if let Some((n, w, r)) = self.id_range_tuple() {
            println!();
            Self::print_id_ranges(n, w, r);
        }
        if let Some(ref ext) = self.state.extended {
            println!();
            Self::print_extended(ext);
        }
        if let Some(ref mut stats) = self.state.loc_stats {
            println!();
            Self::print_locations(stats);
        }
    }

    fn print_header(&self) {
        println!("File:     {} ({})", self.file_name, format_size(self.file_size));
        if let Some(ref prog) = self.header_meta.writing_program {
            println!("Program:  {prog}");
        }

        // Combine features, skip boilerplate (OsmSchema-V0.6, DenseNodes)
        let features: Vec<&str> = self
            .header_meta
            .required_features
            .iter()
            .chain(self.header_meta.optional_features.iter())
            .map(String::as_str)
            .filter(|f| *f != "OsmSchema-V0.6" && *f != "DenseNodes")
            .collect();
        if !features.is_empty() {
            println!("Features: {}", features.join(", "));
        }

        if let Some((left, bottom, right, top)) = self.header_meta.bbox {
            println!("Bbox:     {left},{bottom},{right},{top}");
        }

        let hm = &self.header_meta;
        if hm.replication_sequence.is_some() || hm.replication_timestamp.is_some() {
            let mut parts = Vec::new();
            if let Some(seq) = hm.replication_sequence {
                parts.push(format!("seq {seq}"));
            }
            if let Some(ts) = hm.replication_timestamp {
                parts.push(format!("timestamp {ts}"));
            }
            if let Some(ref url) = hm.replication_url {
                parts.push(url.clone());
            }
            println!("Repl:     {}", parts.join(", "));
        }

        println!("Indexed:  {}", if self.is_indexed { "yes" } else { "no" });
    }

    fn print_blocks_summary(&self) {
        println!("Blocks:   {} total", self.total_blocks);
        for (label, stats) in [
            (BlockKind::Nodes.label(), &self.accum.node_type),
            (BlockKind::Ways.label(), &self.accum.way_type),
            (BlockKind::Relations.label(), &self.accum.relation_type),
            (BlockKind::Mixed.label(), &self.accum.mixed_type),
        ] {
            if stats.block_count > 0 {
                println!(
                    "  {:13}{:>6}  ({} compressed)",
                    label,
                    stats.block_count,
                    format_size(stats.frame_bytes)
                );
            }
        }
    }

    fn print_elements(&self) {
        let total = self.state.node_count + self.state.way_count + self.state.relation_count;
        println!("Elements: {} total", format_number(total));

        if self.state.tagged_node_count > 0 {
            println!(
                "  {:13}{}  ({} tagged)",
                "Nodes:",
                format_number(self.state.node_count),
                format_number(self.state.tagged_node_count)
            );
        } else {
            println!("  {:13}{}", "Nodes:", format_number(self.state.node_count));
        }
        println!("  {:13}{}", "Ways:", format_number(self.state.way_count));
        println!(
            "  {:13}{}",
            "Relations:",
            format_number(self.state.relation_count)
        );
    }

    fn print_ordering(&self) {
        if self.accum.segments.is_empty() {
            println!("Ordering: (empty file)");
            return;
        }

        let labels: Vec<&str> = self
            .accum
            .segments
            .iter()
            .map(|s| s.kind.short_label())
            .collect();
        let sequence = labels.join(" \u{2192} ");

        if is_standard_ordering(&self.accum.segments) {
            println!("Ordering: {sequence} (strict)");
        } else {
            println!("Ordering: {sequence} (NON-STANDARD)");
            let ranges: Vec<String> = self
                .accum
                .segments
                .iter()
                .map(|s| {
                    if s.first_block == s.last_block {
                        format!("block {}", s.first_block)
                    } else {
                        format!("blocks {}-{}", s.first_block, s.last_block)
                    }
                })
                .collect();
            println!("          {}", ranges.join("  "));
        }
    }

    /// Print per-type distribution stats (min/max/median/p99) for block
    /// element counts and compressed sizes.
    fn print_block_distribution(infos: &[BlockInfo]) {
        println!("Block distribution:");
        for kind in [BlockKind::Nodes, BlockKind::Ways, BlockKind::Relations, BlockKind::Mixed] {
            let mut elements: Vec<u64> = infos
                .iter()
                .filter(|i| i.kind == kind)
                .map(|i| i.elements)
                .collect();
            if elements.is_empty() {
                continue;
            }
            elements.sort_unstable();
            let mut sizes: Vec<u64> = infos
                .iter()
                .filter(|i| i.kind == kind)
                .map(|i| i.compressed as u64)
                .collect();
            sizes.sort_unstable();

            println!("  {}", kind.label());
            print_distribution_line("    elements/block:", &elements, false);
            print_distribution_line("    bytes/block:   ", &sizes, true);
        }
    }

    /// Print the per-block table with optional head/tail limiting.
    ///
    /// `limit`: 0 = show all blocks, N = show first N and last N blocks.
    fn print_block_table_refs(infos: &[&BlockInfo], limit: usize) {
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());

        let has_raw = infos.iter().any(|i| i.raw.is_some());
        let truncate = limit > 0 && limit * 2 < infos.len();

        if has_raw {
            let _ok = writeln!(
                out,
                "{:>6}  {:12}{:>8}  {:>10}  {:>10}",
                "Block", "Type", "Elements", "Compressed", "Raw"
            );
            if truncate {
                write_block_rows_raw(&mut out, &infos[..limit]);
                let omitted = infos.len() - limit * 2;
                let _ok = writeln!(out, "   ...  ({omitted} blocks omitted)");
                write_block_rows_raw(&mut out, &infos[infos.len() - limit..]);
            } else {
                write_block_rows_raw(&mut out, infos);
            }
        } else {
            let _ok = writeln!(
                out,
                "{:>6}  {:12}{:>8}  {:>10}",
                "Block", "Type", "Elements", "Compressed"
            );
            if truncate {
                write_block_rows_compressed(&mut out, &infos[..limit]);
                let omitted = infos.len() - limit * 2;
                let _ok = writeln!(out, "   ...  ({omitted} blocks omitted)");
                write_block_rows_compressed(&mut out, &infos[infos.len() - limit..]);
            } else {
                write_block_rows_compressed(&mut out, infos);
            }
        }
    }

    /// Print block table with an anomaly reason column (no truncation).
    fn print_block_table_with_reason(infos: &[(&BlockInfo, &str)]) {
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());

        let has_raw = infos.iter().any(|(i, _)| i.raw.is_some());

        if has_raw {
            let _ok = writeln!(
                out,
                "{:>6}  {:12}{:>8}  {:>10}  {:>10}  Reason",
                "Block", "Type", "Elements", "Compressed", "Raw"
            );
            for (info, reason) in infos {
                let _ok = writeln!(
                    out,
                    "{:>6}  {:12}{:>8}  {:>10}  {:>10}  {}",
                    info.number,
                    info.kind.label(),
                    info.elements,
                    format_size(info.compressed as u64),
                    format_size(info.raw.unwrap_or(0) as u64),
                    reason
                );
            }
        } else {
            let _ok = writeln!(
                out,
                "{:>6}  {:12}{:>8}  {:>10}  Reason",
                "Block", "Type", "Elements", "Compressed"
            );
            for (info, reason) in infos {
                let _ok = writeln!(
                    out,
                    "{:>6}  {:12}{:>8}  {:>10}  {}",
                    info.number,
                    info.kind.label(),
                    info.elements,
                    format_size(info.compressed as u64),
                    reason
                );
            }
        }
    }

    fn print_id_ranges(node_ids: &TypeIdRange, way_ids: &TypeIdRange, rel_ids: &TypeIdRange) {
        for (label, ids) in [("Nodes:", node_ids), ("Ways:", way_ids), ("Relations:", rel_ids)] {
            if ids.has_data() {
                println!(
                    "  {:13}{} .. {}   (monotonic: {})",
                    label,
                    format_number_signed(ids.min_id),
                    format_number_signed(ids.max_id),
                    if ids.monotonic { "yes" } else { "no" }
                );
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn print_extended(ext: &ExtendedStats) {
        println!(
            "Ordered:  {}",
            if ext.objects_ordered { "yes" } else { "no" }
        );
        if ext.has_timestamps() {
            println!(
                "Timestamps: {} .. {}",
                format_timestamp(ext.min_timestamp),
                format_timestamp(ext.max_timestamp)
            );
        }
        if ext.data_bbox.has_data() {
            let bb = &ext.data_bbox;
            println!(
                "Data bbox:  {},{},{},{}",
                bb.min_lon as f64 * 1e-9,
                bb.min_lat as f64 * 1e-9,
                bb.max_lon as f64 * 1e-9,
                bb.max_lat as f64 * 1e-9
            );
        }
        let m = &ext.metadata;
        if m.total > 0 {
            print_metadata_line("All objects have:", m, true);
            print_metadata_line("Some objects have:", m, false);
        }
    }

    /// Retrieve a single value by dot-path key, for `--get` scripting.
    ///
    /// Returns `None` for unknown keys.
    pub fn get_value(&self, key: &str) -> Option<String> {
        self.get_value_inner(key)
    }

    #[allow(clippy::cast_precision_loss)]
    fn get_value_inner(&self, key: &str) -> Option<String> {
        match key {
            "file.name" => Some(self.file_name.clone()),
            "file.size" => Some(self.file_size.to_string()),
            "file.format" => Some("PBF".to_string()),
            "header.bbox" => self.header_meta.bbox.map(|(l, b, r, t)| format!("{l} {b} {r} {t}")),
            "header.writing_program" => self.header_meta.writing_program.clone(),
            "header.replication.url" => self.header_meta.replication_url.clone(),
            "header.replication.sequence" => {
                self.header_meta.replication_sequence.map(|s| s.to_string())
            }
            "header.replication.timestamp" => {
                self.header_meta.replication_timestamp.map(|t| t.to_string())
            }
            "indexed" => Some(self.is_indexed.to_string()),
            "blocks.total" => Some(self.total_blocks.to_string()),
            "elements.nodes" => Some(self.state.node_count.to_string()),
            "elements.ways" => Some(self.state.way_count.to_string()),
            "elements.relations" => Some(self.state.relation_count.to_string()),
            "elements.total" => Some(
                (self.state.node_count + self.state.way_count + self.state.relation_count)
                    .to_string(),
            ),
            _ => self.get_extended_value(key),
        }
    }

    fn get_extended_value(&self, key: &str) -> Option<String> {
        let ext = self.state.extended.as_ref()?;
        match key {
            "data.objects_ordered" => Some(yes_no(ext.objects_ordered)),
            "data.timestamp.first" => {
                if ext.has_timestamps() {
                    Some(format_timestamp(ext.min_timestamp))
                } else {
                    Some(String::new())
                }
            }
            "data.timestamp.last" => {
                if ext.has_timestamps() {
                    Some(format_timestamp(ext.max_timestamp))
                } else {
                    Some(String::new())
                }
            }
            "data.bbox" => {
                if ext.data_bbox.has_data() {
                    let bb = &ext.data_bbox;
                    #[allow(clippy::cast_precision_loss)]
                    Some(format!(
                        "{} {} {} {}",
                        bb.min_lon as f64 * 1e-9,
                        bb.min_lat as f64 * 1e-9,
                        bb.max_lon as f64 * 1e-9,
                        bb.max_lat as f64 * 1e-9
                    ))
                } else {
                    Some(String::new())
                }
            }
            "data.count.nodes" => Some(self.state.node_count.to_string()),
            "data.count.ways" => Some(self.state.way_count.to_string()),
            "data.count.relations" => Some(self.state.relation_count.to_string()),
            "metadata.all_objects.version" => Some(yes_no(ext.metadata.all_have(ext.metadata.has_version))),
            "metadata.all_objects.timestamp" => Some(yes_no(ext.metadata.all_have(ext.metadata.has_timestamp))),
            "metadata.all_objects.changeset" => Some(yes_no(ext.metadata.all_have(ext.metadata.has_changeset))),
            "metadata.all_objects.uid" => Some(yes_no(ext.metadata.all_have(ext.metadata.has_uid))),
            "metadata.all_objects.user" => Some(yes_no(ext.metadata.all_have(ext.metadata.has_user))),
            "metadata.some_objects.version" => Some(yes_no(ext.metadata.some_have(ext.metadata.has_version))),
            "metadata.some_objects.timestamp" => Some(yes_no(ext.metadata.some_have(ext.metadata.has_timestamp))),
            "metadata.some_objects.changeset" => Some(yes_no(ext.metadata.some_have(ext.metadata.has_changeset))),
            "metadata.some_objects.uid" => Some(yes_no(ext.metadata.some_have(ext.metadata.has_uid))),
            "metadata.some_objects.user" => Some(yes_no(ext.metadata.some_have(ext.metadata.has_user))),
            _ => None,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn print_locations(stats: &mut LocationStats) {
        let total = stats.with_locations + stats.without_locations;
        if total == 0 {
            println!("Locations: no ways in file");
            return;
        }

        let with_pct = stats.with_locations as f64 / total as f64 * 100.0;
        let without_pct = stats.without_locations as f64 / total as f64 * 100.0;

        println!(
            "Ways with locations:    {} ({:.3}%)",
            format_number(stats.with_locations),
            with_pct
        );
        println!(
            "Ways without locations: {} ({:.3}%)",
            format_number(stats.without_locations),
            without_pct
        );

        if !stats.coord_counts.is_empty() {
            stats.coord_counts.sort_unstable();
            let len = stats.coord_counts.len();
            let min = stats.coord_counts[0];
            let max = stats.coord_counts[len - 1];
            let median = stats.coord_counts[len / 2];
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let p99_idx = ((len as f64 - 1.0) * 0.99) as usize;
            let p99 = stats.coord_counts[p99_idx.min(len - 1)];
            println!("Coords per way:         min {min}, max {max}, median {median}, p99 {p99}");
        }
    }

    fn id_range_tuple(&self) -> Option<(&TypeIdRange, &TypeIdRange, &TypeIdRange)> {
        match (&self.state.node_ids, &self.state.way_ids, &self.state.relation_ids) {
            (Some(n), Some(w), Some(r)) => Some((n, w, r)),
            _ => None,
        }
    }

    /// Serialize the inspect report to a JSON value.
    ///
    /// `block_limit`: `None` = no `blocks_detail` field, `Some(0)` = full listing,
    /// `Some(N)` = first N + last N blocks.
    #[cfg(feature = "commands")]
    pub fn to_json(&self, block_limit: Option<usize>) -> serde_json::Value {
        self.to_json_filtered(block_limit, false)
    }

    /// Serialize the inspect report to JSON with optional anomalies-only block detail.
    #[cfg(feature = "commands")]
    pub fn to_json_filtered(
        &self,
        block_limit: Option<usize>,
        anomalies_only: bool,
    ) -> serde_json::Value {
        let hm = &self.header_meta;

        let bbox = hm.bbox.map(|(left, bottom, right, top)| {
            serde_json::json!({ "left": left, "bottom": bottom, "right": right, "top": top })
        });

        let header = serde_json::json!({
            "writing_program": hm.writing_program,
            "required_features": hm.required_features,
            "optional_features": hm.optional_features,
            "bbox": bbox,
            "replication": {
                "sequence": hm.replication_sequence,
                "timestamp": hm.replication_timestamp,
                "url": hm.replication_url,
            },
        });

        let sequence: Vec<&str> = self.accum.segments.iter()
            .map(|s| s.kind.short_label()).collect();

        let mut json = serde_json::json!({
            "schema_version": 1,
            "file": self.file_name,
            "file_size": self.file_size,
            "header": header,
            "indexed": self.is_indexed,
            "blocks": {
                "total": self.total_blocks,
                "nodes": type_stats_json(&self.accum.node_type),
                "ways": type_stats_json(&self.accum.way_type),
                "relations": type_stats_json(&self.accum.relation_type),
                "mixed": type_stats_json(&self.accum.mixed_type),
            },
            "elements": {
                "nodes": self.state.node_count,
                "tagged_nodes": self.state.tagged_node_count,
                "ways": self.state.way_count,
                "relations": self.state.relation_count,
                "total": self.state.node_count + self.state.way_count + self.state.relation_count,
            },
            "ordering": {
                "sequence": sequence,
                "standard": is_standard_ordering(&self.accum.segments),
            },
            "id_ranges": id_ranges_json(&self.state),
            "anomalies_only": anomalies_only,
            "blocks_detail": blocks_detail_json(block_limit, &self.accum.block_infos, anomalies_only),
            "locations": locations_json(&self.state.loc_stats),
        });

        if let Some(ref ext) = self.state.extended {
            json["data"] = extended_json(ext);
            json["metadata"] = metadata_json(&ext.metadata);
        }

        json
    }
}

#[cfg(feature = "commands")]
fn type_stats_json(ts: &TypeStats) -> serde_json::Value {
    serde_json::json!({
        "count": ts.block_count,
        "compressed_bytes": ts.frame_bytes,
        "elements": ts.element_count,
    })
}

#[cfg(feature = "commands")]
fn id_range_json(r: &TypeIdRange) -> serde_json::Value {
    if r.has_data() {
        serde_json::json!({ "min": r.min_id, "max": r.max_id, "monotonic": r.monotonic, "count": r.count })
    } else {
        serde_json::Value::Null
    }
}

#[cfg(feature = "commands")]
fn id_ranges_json(state: &ScanState) -> serde_json::Value {
    match (&state.node_ids, &state.way_ids, &state.relation_ids) {
        (Some(n), Some(w), Some(r)) => serde_json::json!({
            "nodes": id_range_json(n),
            "ways": id_range_json(w),
            "relations": id_range_json(r),
        }),
        _ => serde_json::Value::Null,
    }
}

#[allow(clippy::cast_precision_loss)]
#[cfg(feature = "commands")]
fn extended_json(ext: &ExtendedStats) -> serde_json::Value {
    let bbox = if ext.data_bbox.has_data() {
        let bb = &ext.data_bbox;
        serde_json::json!([
            bb.min_lon as f64 * 1e-9,
            bb.min_lat as f64 * 1e-9,
            bb.max_lon as f64 * 1e-9,
            bb.max_lat as f64 * 1e-9
        ])
    } else {
        serde_json::Value::Null
    };

    let timestamp = if ext.has_timestamps() {
        serde_json::json!({
            "first": format_timestamp(ext.min_timestamp),
            "last": format_timestamp(ext.max_timestamp),
        })
    } else {
        serde_json::Value::Null
    };

    serde_json::json!({
        "bbox": bbox,
        "timestamp": timestamp,
        "objects_ordered": ext.objects_ordered,
    })
}

#[cfg(feature = "commands")]
fn metadata_json(m: &MetadataCoverage) -> serde_json::Value {
    serde_json::json!({
        "all_objects": {
            "version": m.all_have(m.has_version),
            "timestamp": m.all_have(m.has_timestamp),
            "changeset": m.all_have(m.has_changeset),
            "uid": m.all_have(m.has_uid),
            "user": m.all_have(m.has_user),
        },
        "some_objects": {
            "version": m.some_have(m.has_version),
            "timestamp": m.some_have(m.has_timestamp),
            "changeset": m.some_have(m.has_changeset),
            "uid": m.some_have(m.has_uid),
            "user": m.some_have(m.has_user),
        },
    })
}

#[cfg(feature = "commands")]
fn blocks_detail_json(
    block_limit: Option<usize>,
    block_infos: &Option<Vec<BlockInfo>>,
    anomalies_only: bool,
) -> serde_json::Value {
    let (Some(limit), Some(infos)) = (block_limit, block_infos) else {
        return serde_json::Value::Null;
    };
    if anomalies_only {
        let selected = anomaly_blocks(infos);
        let arr: Vec<serde_json::Value> = selected
            .iter()
            .map(|(info, reason)| serde_json::json!({
                "number": info.number,
                "type": info.kind.short_label(),
                "elements": info.elements,
                "compressed_bytes": info.compressed,
                "raw_bytes": info.raw,
                "anomaly": reason,
            }))
            .collect();
        return serde_json::Value::Array(arr);
    }
    let selected: Vec<&BlockInfo> = infos.iter().collect();
    let truncate = limit > 0 && limit * 2 < selected.len();
    let iter: Box<dyn Iterator<Item = &BlockInfo>> = if truncate {
        Box::new(
            selected[..limit]
                .iter()
                .copied()
                .chain(selected[selected.len() - limit..].iter().copied()),
        )
    } else {
        Box::new(selected.iter().copied())
    };
    let arr: Vec<serde_json::Value> = iter
        .map(|info| serde_json::json!({
            "number": info.number,
            "type": info.kind.short_label(),
            "elements": info.elements,
            "compressed_bytes": info.compressed,
            "raw_bytes": info.raw,
        }))
        .collect();
    serde_json::Value::Array(arr)
}

fn anomaly_blocks(infos: &[BlockInfo]) -> Vec<(&BlockInfo, &'static str)> {
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

#[cfg(feature = "commands")]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn locations_json(loc_stats: &Option<LocationStats>) -> serde_json::Value {
    let Some(stats) = loc_stats else {
        return serde_json::Value::Null;
    };
    let coords_per_way = if stats.coord_counts.is_empty() {
        serde_json::Value::Null
    } else {
        let mut sorted = stats.coord_counts.clone();
        sorted.sort_unstable();
        let len = sorted.len();
        let p99_idx = ((len as f64 - 1.0) * 0.99) as usize;
        serde_json::json!({
            "min": sorted[0],
            "max": sorted[len - 1],
            "median": sorted[len / 2],
            "p99": sorted[p99_idx.min(len - 1)],
        })
    };
    serde_json::json!({
        "with_locations": stats.with_locations,
        "without_locations": stats.without_locations,
        "coords_per_way": coords_per_way,
    })
}

// ---------------------------------------------------------------------------
// Block table row helpers (free functions to avoid cognitive_complexity in methods)
// ---------------------------------------------------------------------------

fn write_block_rows_raw(out: &mut impl std::io::Write, infos: &[&BlockInfo]) {
    for info in infos {
        let _ok = writeln!(
            out,
            "{:>6}  {:12}{:>8}  {:>10}  {:>10}",
            info.number,
            info.kind.label(),
            info.elements,
            format_size(info.compressed as u64),
            format_size(info.raw.unwrap_or(0) as u64)
        );
    }
}

fn write_block_rows_compressed(out: &mut impl std::io::Write, infos: &[&BlockInfo]) {
    for info in infos {
        let _ok = writeln!(
            out,
            "{:>6}  {:12}{:>8}  {:>10}",
            info.number,
            info.kind.label(),
            info.elements,
            format_size(info.compressed as u64)
        );
    }
}

/// Print a distribution line (min/max/median/p99) for a sorted slice of values.
///
/// `label` is printed as-is (should include leading whitespace and trailing colon).
/// `is_bytes` controls formatting: `true` uses `format_size`, `false` uses `format_number`.
#[allow(clippy::cast_precision_loss)]
fn print_distribution_line(label: &str, sorted: &[u64], is_bytes: bool) {
    let fmt = if is_bytes { format_size } else { format_number };
    let len = sorted.len();
    let min = sorted[0];
    let max = sorted[len - 1];
    if len == 1 {
        println!("{label} {}", fmt(min));
        return;
    }
    let median = sorted[len / 2];
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let p99_idx = ((len as f64 - 1.0) * 0.99) as usize;
    let p99 = sorted[p99_idx.min(len - 1)];
    println!(
        "{label} min {}  max {}  median {}  p99 {}",
        fmt(min),
        fmt(max),
        fmt(median),
        fmt(p99),
    );
}

/// Standard ordering: at most one run each of [Nodes, Ways, Relations] in that order.
/// Mixed blocks or repeated/out-of-order segments make it non-standard.
fn is_standard_ordering(segments: &[OrderingSegment]) -> bool {
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

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

#[allow(clippy::cast_precision_loss)]
fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn format_number(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(c);
    }
    result
}

fn format_number_signed(n: i64) -> String {
    if n < 0 {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let abs = (n as i128).unsigned_abs() as u64;
        format!("-{}", format_number(abs))
    } else {
        #[allow(clippy::cast_sign_loss)]
        format_number(n as u64)
    }
}

/// Format a millisecond epoch timestamp as ISO-8601 UTC.
fn format_timestamp(millis: i64) -> String {
    let secs = millis / 1000;
    // Use manual formatting: seconds since epoch → date/time components
    // This avoids a chrono dependency for a single formatting call.
    const SECS_PER_DAY: i64 = 86400;
    let days = secs.div_euclid(SECS_PER_DAY);
    let day_secs = secs.rem_euclid(SECS_PER_DAY);
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
/// Algorithm from Howard Hinnant's `civil_from_days`.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d)
}

fn yes_no(b: bool) -> String {
    if b { "yes".to_string() } else { "no".to_string() }
}

fn print_metadata_line(label: &str, m: &MetadataCoverage, all: bool) {
    let check = |count: u64| -> bool {
        if all { m.all_have(count) } else { m.some_have(count) }
    };
    let mut attrs = Vec::new();
    if check(m.has_version) {
        attrs.push("version");
    }
    if check(m.has_timestamp) {
        attrs.push("timestamp");
    }
    if check(m.has_changeset) {
        attrs.push("changeset");
    }
    if check(m.has_uid) {
        attrs.push("uid");
    }
    if check(m.has_user) {
        attrs.push("user");
    }
    if attrs.is_empty() {
        println!("  {label} (none)");
    } else {
        println!("  {label} {}", attrs.join(", "));
    }
}

// ---------------------------------------------------------------------------
// Show element by ID
// ---------------------------------------------------------------------------

/// Element type filter for `show_element`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ShowElementType {
    Node,
    Way,
    Relation,
}

/// Look up a single element by type and ID. Prints all metadata, tags,
/// refs/members to stdout. Uses blob-level indexdata when available to
/// skip non-matching blobs. On sorted PBFs, exits early once past the
/// target ID range.
pub fn show_element(
    path: &Path,
    elem_type: ShowElementType,
    target_id: i64,
    direct_io: bool,
) -> Result<bool> {
    let mut reader = crate::blob::BlobReader::open(path, direct_io)?;
    reader.set_parse_indexdata(true);
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    let target_kind = match elem_type {
        ShowElementType::Node => ElemKind::Node,
        ShowElementType::Way => ElemKind::Way,
        ShowElementType::Relation => ElemKind::Relation,
    };

    for blob_result in &mut reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }

        // Skip blobs that cannot contain the target element.
        if let Some(idx) = blob.index() {
            // Wrong element type.
            if idx.kind != target_kind {
                continue;
            }
            // Target ID outside this blob's range.
            if target_id < idx.min_id || target_id > idx.max_id {
                // On sorted PBFs, if min_id > target_id we're past it.
                if idx.min_id > target_id {
                    return Ok(false);
                }
                continue;
            }
        }

        blob.decompress_into(&mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf),
            &mut st_scratch,
            &mut gr_scratch,
        )?;

        for element in block.elements() {
            match (&element, elem_type) {
                (Element::DenseNode(dn), ShowElementType::Node) if dn.id() == target_id => {
                    print_node_header(target_id, dn.lat(), dn.lon());
                    print_dense_node_info(dn);
                    print_tags_dense(dn);
                    return Ok(true);
                }
                (Element::Node(n), ShowElementType::Node) if n.id() == target_id => {
                    print_node_header(target_id, n.lat(), n.lon());
                    print_node_info(n);
                    print_tags(n);
                    return Ok(true);
                }
                (Element::Way(w), ShowElementType::Way) if w.id() == target_id => {
                    println!("way/{target_id}");
                    print_info(&w.info());
                    print_tags(w);
                    print_way_refs(w);
                    return Ok(true);
                }
                (Element::Relation(r), ShowElementType::Relation) if r.id() == target_id => {
                    println!("relation/{target_id}");
                    print_info(&r.info());
                    print_tags(r);
                    print_relation_members(r);
                    return Ok(true);
                }
                _ => {}
            }
        }
    }

    Ok(false)
}

fn print_node_header(id: i64, lat: f64, lon: f64) {
    println!("node/{id}");
    println!("  lat: {lat:.7}");
    println!("  lon: {lon:.7}");
}

fn print_dense_node_info(dn: &crate::DenseNode<'_>) {
    if let Some(info) = dn.info() {
        if info.version() != -1 {
            println!("  version: {}", info.version());
            let ts = info.milli_timestamp();
            if ts != 0 {
                println!("  timestamp: {}", ts / 1000);
            }
            let cs = info.changeset();
            if cs != -1 && cs != 0 {
                println!("  changeset: {cs}");
            }
            let uid = info.uid();
            if uid != 0 {
                println!("  uid: {uid}");
            }
            if let Ok(user) = info.user() {
                if !user.is_empty() {
                    println!("  user: {user}");
                }
            }
        }
    }
}

fn print_node_info(n: &crate::Node<'_>) {
    print_info(&n.info());
}

fn print_info(info: &crate::Info<'_>) {
    if let Some(v) = info.version() {
        println!("  version: {v}");
    }
    if let Some(ts) = info.milli_timestamp() {
        if ts != 0 {
            println!("  timestamp: {}", ts / 1000);
        }
    }
    if let Some(cs) = info.changeset() {
        if cs != 0 {
            println!("  changeset: {cs}");
        }
    }
    if let Some(uid) = info.uid() {
        if uid != 0 {
            println!("  uid: {uid}");
        }
    }
    if let Some(Ok(user)) = info.user() {
        if !user.is_empty() {
            println!("  user: {user}");
        }
    }
}

/// Print tags for elements that implement the standard `tags()` iterator.
fn print_tags<'a>(element: &impl HasTags<'a>) {
    let mut has_tags = false;
    for (k, v) in element.tags() {
        if !has_tags {
            println!("  tags:");
            has_tags = true;
        }
        println!("    {k} = {v}");
    }
}

fn print_tags_dense(dn: &crate::DenseNode<'_>) {
    let mut has_tags = false;
    for (k, v) in dn.tags() {
        if !has_tags {
            println!("  tags:");
            has_tags = true;
        }
        println!("    {k} = {v}");
    }
}

fn print_way_refs(w: &crate::Way<'_>) {
    let refs: Vec<i64> = w.refs().collect();
    if !refs.is_empty() {
        println!("  refs: ({} nodes)", refs.len());
        for id in &refs {
            println!("    {id}");
        }
    }
}

fn print_relation_members(r: &crate::Relation<'_>) {
    let members: Vec<_> = r.members().collect();
    if !members.is_empty() {
        println!("  members: ({})", members.len());
        for m in &members {
            let type_str = match m.id {
                crate::MemberId::Node(_) => "node",
                crate::MemberId::Way(_) => "way",
                crate::MemberId::Relation(_) => "relation",
                crate::MemberId::Unknown(_, _) => "unknown",
            };
            let role = m.role().unwrap_or("<invalid>");
            println!("    {type_str}/{} ({})", m.id.id(), role);
        }
    }
}

/// Trait to abstract over `Node`/`Way`/`Relation` tag access.
trait HasTags<'a> {
    fn tags(&self) -> crate::TagIter<'a>;
}

impl<'a> HasTags<'a> for crate::Node<'a> {
    fn tags(&self) -> crate::TagIter<'a> {
        self.tags()
    }
}

impl<'a> HasTags<'a> for crate::Way<'a> {
    fn tags(&self) -> crate::TagIter<'a> {
        self.tags()
    }
}

impl<'a> HasTags<'a> for crate::Relation<'a> {
    fn tags(&self) -> crate::TagIter<'a> {
        self.tags()
    }
}
