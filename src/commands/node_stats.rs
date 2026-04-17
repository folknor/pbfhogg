use std::path::Path;

use crate::elements::Element;

use super::{require_indexdata, Result};

const BLOCK_SIZE: usize = 128;

/// Bit-width histogram bucket boundaries.
const BUCKETS: &[(u32, u32)] = &[
    (0, 8),
    (9, 16),
    (17, 20),
    (21, 24),
    (25, 28),
    (29, 32),
];

fn bucket_index(bits: u32) -> usize {
    match bits {
        0..=8 => 0,
        9..=16 => 1,
        17..=20 => 2,
        21..=24 => 3,
        25..=28 => 4,
        _ => 5,
    }
}

fn bits_needed(range: u32) -> u32 {
    if range == 0 {
        0
    } else {
        u32::BITS - range.leading_zeros()
    }
}

/// Statistics for one coordinate dimension (lat or lon).
pub struct CoordStats {
    /// Histogram: count of FOR blocks per bit-width bucket.
    pub bucket_counts: [u64; 6],
    /// Total number of complete or partial 128-value blocks analyzed.
    pub total_blocks: u64,
    /// Sum of bits_needed across all blocks (for weighted average).
    pub total_bits_weighted: u64,
}

impl CoordStats {
    fn new() -> Self {
        Self {
            bucket_counts: [0; 6],
            total_blocks: 0,
            total_bits_weighted: 0,
        }
    }

    fn record_block(&mut self, values: &[i32]) {
        if values.is_empty() {
            return;
        }
        let mut min = values[0];
        let mut max = values[0];
        for &v in &values[1..] {
            if v < min { min = v; }
            if v > max { max = v; }
        }
        // max >= min guaranteed, and both are i32, so difference is in [0, u32::MAX].
        // i32::MAX - i32::MIN = 4_294_967_295 = u32::MAX, so this always succeeds.
        debug_assert!(max >= min, "CoordStats: max ({max}) < min ({min})");
        let diff = u64::try_from(i64::from(max) - i64::from(min)).unwrap_or(0);
        let range = u32::try_from(diff).unwrap_or(u32::MAX);
        let bits = bits_needed(range);
        self.bucket_counts[bucket_index(bits)] += 1;
        self.total_blocks += 1;
        self.total_bits_weighted += u64::from(bits);
    }

    pub fn avg_bits(&self) -> f64 {
        if self.total_blocks == 0 {
            0.0
        } else {
            self.total_bits_weighted as f64 / self.total_blocks as f64
        }
    }
}

/// Full report from `node_stats`.
pub struct NodeStatsReport {
    pub node_count: u64,
    pub min_lat: i32,
    pub max_lat: i32,
    pub min_lon: i32,
    pub max_lon: i32,
    pub lat_stats: CoordStats,
    pub lon_stats: CoordStats,
}

impl NodeStatsReport {
    pub fn print_report(&self) {
        println!("Node count: {}", self.node_count);
        println!();
        println!("Coordinate ranges (e7):");
        println!("  lat: {} .. {}", self.min_lat, self.max_lat);
        println!("  lon: {} .. {}", self.min_lon, self.max_lon);
        println!();

        println!("FOR block bit-width distribution (block size = {BLOCK_SIZE}):");
        println!();
        print_histogram("lat", &self.lat_stats);
        println!();
        print_histogram("lon", &self.lon_stats);
        println!();

        let lat_avg = self.lat_stats.avg_bits();
        let lon_avg = self.lon_stats.avg_bits();
        println!("Weighted average bit-width:");
        println!("  lat: {lat_avg:.2} bits");
        println!("  lon: {lon_avg:.2} bits");
        println!();

        let num_blocks = self.lat_stats.total_blocks;
        // per block: avg_bits * 128 / 8 bytes for packed values + 4 bytes for min_value
        // two dimensions (lat + lon)
        let lat_bytes_per_block = lat_avg * BLOCK_SIZE as f64 / 8.0 + 4.0;
        let lon_bytes_per_block = lon_avg * BLOCK_SIZE as f64 / 8.0 + 4.0;
        let total_bytes = num_blocks as f64 * (lat_bytes_per_block + lon_bytes_per_block);
        let uncompressed_bytes = self.node_count as f64 * 8.0;

        println!("Estimated compressed size:");
        println!("  {:.2} GB ({} blocks x ({:.1} + {:.1}) bytes/block)",
            total_bytes / 1_073_741_824.0,
            num_blocks,
            lat_bytes_per_block,
            lon_bytes_per_block,
        );
        println!("  vs {:.2} GB uncompressed ({} nodes x 8 bytes)",
            uncompressed_bytes / 1_073_741_824.0,
            self.node_count,
        );
        if uncompressed_bytes > 0.0 {
            println!("  ratio: {:.1}%", total_bytes / uncompressed_bytes * 100.0);
        }
    }
}

fn print_histogram(label: &str, stats: &CoordStats) {
    println!("  {label}:");
    for (i, &(lo, hi)) in BUCKETS.iter().enumerate() {
        let count = stats.bucket_counts[i];
        let pct = if stats.total_blocks > 0 {
            count as f64 / stats.total_blocks as f64 * 100.0
        } else {
            0.0
        };
        println!("    {lo:>2}-{hi:<2} bits: {count:>10} blocks ({pct:>5.1}%)");
    }
}

/// Analyze node coordinate statistics from a PBF file.
///
/// Streams through all nodes, collecting coordinate ranges and FOR block
/// bit-width distributions. Runs in constant memory.
#[hotpath::measure]
pub fn node_stats(path: &Path, direct_io: bool, force: bool) -> Result<NodeStatsReport> {
    require_indexdata(path, direct_io, force,
        "input PBF has no blob-level indexdata. Without indexdata, the node-only \
         filter is a no-op - all blobs are decompressed (significantly slower).")?;

    // Sequential reader to avoid PrimitiveBlock cross-thread retention
    // at planet scale (520K+ blobs). Diagnostic command - single-threaded
    // decode is acceptable.
    let mut blob_reader = crate::blob::BlobReader::open(path, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut decompress_buf: Vec<u8> = Vec::new();

    let mut node_count: u64 = 0;
    let mut min_lat = i32::MAX;
    let mut max_lat = i32::MIN;
    let mut min_lon = i32::MAX;
    let mut max_lon = i32::MIN;

    let mut lat_stats = CoordStats::new();
    let mut lon_stats = CoordStats::new();

    let mut lat_block = Vec::with_capacity(BLOCK_SIZE);
    let mut lon_block = Vec::with_capacity(BLOCK_SIZE);
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    crate::debug::emit_marker("NODESTATS_START");
    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(idx) = blob.index() {
            if !matches!(idx.kind, crate::blob_index::ElemKind::Node) { continue; }
        }
        blob.decompress_into(&mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf), &mut st_scratch, &mut gr_scratch,
        )?;
        for element in block.elements_skip_metadata() {
            let (lat_e7, lon_e7) = match &element {
                Element::DenseNode(dn) => (dn.decimicro_lat(), dn.decimicro_lon()),
                Element::Node(n) => (n.decimicro_lat(), n.decimicro_lon()),
                _ => continue,
            };

            node_count += 1;

            if lat_e7 < min_lat { min_lat = lat_e7; }
            if lat_e7 > max_lat { max_lat = lat_e7; }
            if lon_e7 < min_lon { min_lon = lon_e7; }
            if lon_e7 > max_lon { max_lon = lon_e7; }

            lat_block.push(lat_e7);
            lon_block.push(lon_e7);

            if lat_block.len() == BLOCK_SIZE {
                lat_stats.record_block(&lat_block);
                lon_stats.record_block(&lon_block);
                lat_block.clear();
                lon_block.clear();
            }
        }
    }

    // Flush the last partial block
    if !lat_block.is_empty() {
        lat_stats.record_block(&lat_block);
        lon_stats.record_block(&lon_block);
    }

    if node_count == 0 {
        min_lat = 0;
        max_lat = 0;
        min_lon = 0;
        max_lon = 0;
    }

    crate::debug::emit_marker("NODESTATS_END");
    Ok(NodeStatsReport {
        node_count,
        min_lat,
        max_lat,
        min_lon,
        max_lon,
        lat_stats,
        lon_stats,
    })
}
