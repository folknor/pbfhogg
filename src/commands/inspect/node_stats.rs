use std::path::Path;

use crate::elements::Element;

use crate::commands::require_indexdata;
use crate::BoxResult as Result;

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

/// Per-worker state for [`node_stats`]. Held across all blobs a worker
/// processes and merged into the global report at completion.
///
/// `lat_block` / `lon_block` are the in-flight 128-value windows that
/// feed [`CoordStats::record_block`]; they persist across blobs within a
/// worker and are flushed once at merge time. Cross-blob carry is kept
/// deliberately - the FOR block stat is about locality of consecutive
/// coordinate values, not blob-level grouping, and matching the
/// sequential behaviour of "only the last block is partial" keeps the
/// output closest to the pre-parallel path. Ordering is still different
/// in the parallel path (workers see blobs out of file order), so the
/// final block distribution is not byte-identical between runs but the
/// histogram shape is preserved.
struct WorkerAccum {
    node_count: u64,
    min_lat: i32,
    max_lat: i32,
    min_lon: i32,
    max_lon: i32,
    lat_stats: CoordStats,
    lon_stats: CoordStats,
    lat_block: Vec<i32>,
    lon_block: Vec<i32>,
}

impl WorkerAccum {
    fn new() -> Self {
        Self {
            node_count: 0,
            min_lat: i32::MAX,
            max_lat: i32::MIN,
            min_lon: i32::MAX,
            max_lon: i32::MIN,
            lat_stats: CoordStats::new(),
            lon_stats: CoordStats::new(),
            lat_block: Vec::with_capacity(BLOCK_SIZE),
            lon_block: Vec::with_capacity(BLOCK_SIZE),
        }
    }

    fn finalize(&mut self) {
        if !self.lat_block.is_empty() {
            self.lat_stats.record_block(&self.lat_block);
            self.lon_stats.record_block(&self.lon_block);
            self.lat_block.clear();
            self.lon_block.clear();
        }
    }
}

/// Analyze node coordinate statistics from a PBF file.
///
/// Streams through all node blobs in parallel (pread workers via
/// [`parallel_classify_accumulate`]), collecting coordinate ranges and
/// FOR block bit-width distributions into per-worker accumulators that
/// are merged at completion. Per-worker state is bounded
/// (`CoordStats` + two 128-entry block buffers + scalar mins/maxes /
/// counters ≈ ~1 KB), so the `parallel_classify_accumulate` safety
/// envelope applies comfortably.
#[hotpath::measure]
pub fn node_stats(
    path: &Path,
    direct_io: bool,
    force: bool,
    jobs: usize,
) -> Result<NodeStatsReport> {
    require_indexdata(path, direct_io, force,
        "input PBF has no blob-level indexdata. Without indexdata, the node-only \
         filter is a no-op - all blobs are decompressed (significantly slower).")?;

    crate::debug::emit_marker("NODESTATS_START");

    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        path,
        Some(crate::blob_meta::ElemKind::Node),
    )?;

    // `jobs == 0` means auto; `parallel_classify_accumulate` interprets
    // `Some(0)` as auto too, so pass through unchanged.
    let thread_override = (jobs > 0).then_some(jobs);
    let mut global = WorkerAccum::new();

    crate::scan::classify::parallel_classify_accumulate(
        &shared_file,
        &schedule,
        thread_override,
        WorkerAccum::new,
        |block, accum| {
            for element in block.elements_skip_metadata() {
                let (lat_e7, lon_e7) = match &element {
                    Element::DenseNode(dn) => (dn.decimicro_lat(), dn.decimicro_lon()),
                    Element::Node(n) => (n.decimicro_lat(), n.decimicro_lon()),
                    _ => continue,
                };

                accum.node_count += 1;
                if lat_e7 < accum.min_lat { accum.min_lat = lat_e7; }
                if lat_e7 > accum.max_lat { accum.max_lat = lat_e7; }
                if lon_e7 < accum.min_lon { accum.min_lon = lon_e7; }
                if lon_e7 > accum.max_lon { accum.max_lon = lon_e7; }

                accum.lat_block.push(lat_e7);
                accum.lon_block.push(lon_e7);

                if accum.lat_block.len() == BLOCK_SIZE {
                    accum.lat_stats.record_block(&accum.lat_block);
                    accum.lon_stats.record_block(&accum.lon_block);
                    accum.lat_block.clear();
                    accum.lon_block.clear();
                }
            }
        },
        |mut worker| {
            worker.finalize();
            global.node_count += worker.node_count;
            global.min_lat = global.min_lat.min(worker.min_lat);
            global.max_lat = global.max_lat.max(worker.max_lat);
            global.min_lon = global.min_lon.min(worker.min_lon);
            global.max_lon = global.max_lon.max(worker.max_lon);
            // CoordStats merge is additive on every field.
            for (dst, src) in global
                .lat_stats
                .bucket_counts
                .iter_mut()
                .zip(worker.lat_stats.bucket_counts.iter())
            {
                *dst += src;
            }
            global.lat_stats.total_blocks += worker.lat_stats.total_blocks;
            global.lat_stats.total_bits_weighted += worker.lat_stats.total_bits_weighted;
            for (dst, src) in global
                .lon_stats
                .bucket_counts
                .iter_mut()
                .zip(worker.lon_stats.bucket_counts.iter())
            {
                *dst += src;
            }
            global.lon_stats.total_blocks += worker.lon_stats.total_blocks;
            global.lon_stats.total_bits_weighted += worker.lon_stats.total_bits_weighted;
        },
    )?;

    if global.node_count == 0 {
        global.min_lat = 0;
        global.max_lat = 0;
        global.min_lon = 0;
        global.max_lon = 0;
    }

    crate::debug::emit_marker("NODESTATS_END");
    Ok(NodeStatsReport {
        node_count: global.node_count,
        min_lat: global.min_lat,
        max_lat: global.max_lat,
        min_lon: global.min_lon,
        max_lon: global.max_lon,
        lat_stats: global.lat_stats,
        lon_stats: global.lon_stats,
    })
}
