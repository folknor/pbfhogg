//! Inspect PBF file: comprehensive metadata, block breakdown, ordering analysis.

use std::path::Path;

use super::read_raw_frame;
use crate::blob::{
    decode_blob_to_headerblock, decompress_blob_data_into, parse_primitive_block_from_bytes,
    BlobKind,
};
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
    raw: usize,
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
}

impl ScanState {
    fn new(show_id_ranges: bool, show_locations: bool) -> Self {
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
// Main report struct
// ---------------------------------------------------------------------------

pub struct InspectReport {
    file_name: String,
    file_size: u64,
    writing_program: Option<String>,
    required_features: Vec<String>,
    optional_features: Vec<String>,
    bbox: Option<(f64, f64, f64, f64)>,
    replication_timestamp: Option<i64>,
    replication_sequence: Option<i64>,
    replication_url: Option<String>,
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

    // Header metadata
    let mut writing_program = None;
    let mut required_features = Vec::new();
    let mut optional_features = Vec::new();
    let mut bbox = None;
    let mut replication_timestamp = None;
    let mut replication_sequence = None;
    let mut replication_url = None;

    // Indexdata tracking
    let mut indexed_blobs = 0u64;
    let mut total_data_blobs = 0u64;

    let mut accum = BlockAccum::new(show_blocks);
    let mut block_number = 0u32;
    let mut state = ScanState::new(show_id_ranges, show_locations);

    while let Some(frame) = read_raw_frame(&mut reader, &mut offset)? {
        match frame.blob_type {
            BlobKind::OsmHeader => {
                let header = decode_blob_to_headerblock(frame.blob_bytes())?;
                writing_program = header.writing_program().map(String::from);
                required_features = header
                    .required_features()
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                optional_features = header
                    .optional_features()
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                if let Some(bb) = header.bbox() {
                    bbox = Some((bb.left, bb.bottom, bb.right, bb.top));
                }
                replication_timestamp = header.osmosis_replication_timestamp();
                replication_sequence = header.osmosis_replication_sequence_number();
                replication_url = header.osmosis_replication_base_url().map(String::from);
            }
            BlobKind::OsmData => {
                total_data_blobs += 1;
                if frame.index.is_some() {
                    indexed_blobs += 1;
                }
                block_number += 1;
                scan_data_blob(&frame, &mut decompress_buf, &mut state, block_number, &mut accum)?;
            }
            BlobKind::Unknown(_) => {}
        }
    }

    let is_indexed = total_data_blobs > 0 && indexed_blobs == total_data_blobs;

    Ok(InspectReport {
        file_name,
        file_size: meta.len(),
        writing_program,
        required_features,
        optional_features,
        bbox,
        replication_timestamp,
        replication_sequence,
        replication_url,
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
    state: &mut ScanState,
    block_number: u32,
    accum: &mut BlockAccum,
) -> Result<()> {
    let frame_size = frame.frame_bytes.len();
    let compressed_size = frame.blob_bytes().len();

    decompress_blob_data_into(frame.blob_bytes(), decompress_buf)?;
    let raw_size = decompress_buf.len();
    let block = parse_primitive_block_from_bytes(decompress_buf)?;

    let mut has_nodes = false;
    let mut has_ways = false;
    let mut has_relations = false;
    let mut block_elements = 0u64;

    for element in block.elements() {
        block_elements += 1;
        let (n, w, r) = state.process_element(&element);
        has_nodes |= n;
        has_ways |= w;
        has_relations |= r;
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
            raw: raw_size,
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
    #[allow(clippy::cast_precision_loss)]
    pub fn print_report(&self) {
        self.print_header();
        println!();
        self.print_blocks_summary();
        println!();
        self.print_elements();
        println!();
        self.print_ordering();

        if let Some(ref infos) = self.accum.block_infos {
            println!();
            Self::print_block_table(infos);
        }
        if let Some((n, w, r)) = self.id_range_tuple() {
            println!();
            Self::print_id_ranges(n, w, r);
        }
        if let Some(ref stats) = self.state.loc_stats {
            println!();
            Self::print_locations(stats);
        }
    }

    fn print_header(&self) {
        println!("File:     {} ({})", self.file_name, format_size(self.file_size));
        if let Some(ref prog) = self.writing_program {
            println!("Program:  {prog}");
        }

        // Combine features, skip boilerplate (OsmSchema-V0.6, DenseNodes)
        let features: Vec<&str> = self
            .required_features
            .iter()
            .chain(self.optional_features.iter())
            .map(String::as_str)
            .filter(|f| *f != "OsmSchema-V0.6" && *f != "DenseNodes")
            .collect();
        if !features.is_empty() {
            println!("Features: {}", features.join(", "));
        }

        if let Some((left, bottom, right, top)) = self.bbox {
            println!("Bbox:     {left},{bottom},{right},{top}");
        }

        if self.replication_sequence.is_some() || self.replication_timestamp.is_some() {
            let mut parts = Vec::new();
            if let Some(seq) = self.replication_sequence {
                parts.push(format!("seq {seq}"));
            }
            if let Some(ts) = self.replication_timestamp {
                parts.push(format!("timestamp {ts}"));
            }
            if let Some(ref url) = self.replication_url {
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

    fn print_block_table(infos: &[BlockInfo]) {
        println!(
            "{:>6}  {:12}{:>8}  {:>10}  {:>10}",
            "Block", "Type", "Elements", "Compressed", "Raw"
        );
        for info in infos {
            println!(
                "{:>6}  {:12}{:>8}  {:>10}  {:>10}",
                info.number,
                info.kind.label(),
                info.elements,
                format_size(info.compressed as u64),
                format_size(info.raw as u64)
            );
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
    fn print_locations(stats: &LocationStats) {
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
            let mut sorted = stats.coord_counts.clone();
            sorted.sort_unstable();
            let min = sorted[0];
            let max = sorted[sorted.len() - 1];
            let median = sorted[sorted.len() / 2];
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let p99_idx = ((sorted.len() as f64 - 1.0) * 0.99) as usize;
            let p99 = sorted[p99_idx.min(sorted.len() - 1)];
            println!("Coords per way:         min {min}, max {max}, median {median}, p99 {p99}");
        }
    }

    fn id_range_tuple(&self) -> Option<(&TypeIdRange, &TypeIdRange, &TypeIdRange)> {
        match (&self.state.node_ids, &self.state.way_ids, &self.state.relation_ids) {
            (Some(n), Some(w), Some(r)) => Some((n, w, r)),
            _ => None,
        }
    }
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
