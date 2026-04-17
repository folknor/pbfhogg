//! Measure way-blob partition selectivity for partitioned ALTW.
//!
//! For each way blob, collects all node refs and determines which partitions
//! (by node ID range) the blob touches. Reports the fraction of blobs that
//! are single-partition vs multi-partition, for various partition counts.
//!
//! Usage:
//!     cargo run --release --example partition_stats -- <input.osm.pbf>
//!
//! See notes/altw-partitioned.md for context.

#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args
)]

use std::path::Path;

use pbfhogg::{BlobFilter, Element, ElementReader};

/// Maximum node ID in current OSM data (~13B, use 14B for headroom).
const MAX_NODE_ID: u64 = 14_000_000_000;

/// Partition counts to evaluate.
const PARTITION_COUNTS: &[u64] = &[2, 4, 8, 16, 32, 64];

struct BlobStats {
    /// Number of way-node refs in this blob.
    ref_count: u64,
    /// Minimum node ref ID.
    min_ref: i64,
    /// Maximum node ref ID.
    max_ref: i64,
    /// Partition membership: for each N, a u64 bitmap.
    bitmaps: Vec<u64>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: partition_stats <input.osm.pbf>");
        std::process::exit(1);
    }
    let input = Path::new(&args[1]);

    eprintln!("Scanning way blobs in {}...", input.display());

    let reader = ElementReader::open(input, false)
        .expect("failed to open PBF")
        .with_blob_filter(BlobFilter::only_ways());

    let mut blobs: Vec<BlobStats> = Vec::new();
    let mut current_blob_refs: Vec<i64> = Vec::new();
    let mut blob_count: u64 = 0;

    // We need per-blob granularity. Use into_blocks_pipelined and treat each
    // PrimitiveBlock as one blob (which it is - one blob = one block).
    for block in reader.into_blocks_pipelined() {
        let block = block.expect("failed to read block");
        current_blob_refs.clear();

        for element in block.elements_skip_metadata() {
            if let Element::Way(w) = element {
                for node_id in w.refs() {
                    if node_id >= 0 {
                        current_blob_refs.push(node_id);
                    }
                }
            }
        }

        if current_blob_refs.is_empty() {
            continue;
        }

        let min_ref = current_blob_refs.iter().copied().min().unwrap_or(0);
        let max_ref = current_blob_refs.iter().copied().max().unwrap_or(0);
        let ref_count = current_blob_refs.len() as u64;

        // Compute partition bitmaps for each N.
        let bitmaps: Vec<u64> = PARTITION_COUNTS
            .iter()
            .map(|&n| {
                let range_size = (MAX_NODE_ID + n - 1) / n;
                let mut bitmap: u64 = 0;
                for &node_id in &current_blob_refs {
                    let partition = (node_id as u64) / range_size;
                    if partition < 64 {
                        bitmap |= 1u64 << partition;
                    }
                }
                bitmap
            })
            .collect();

        blobs.push(BlobStats {
            ref_count,
            min_ref,
            max_ref,
            bitmaps,
        });

        blob_count += 1;
        if blob_count % 10_000 == 0 {
            eprint!("\r  {} blobs scanned...", blob_count);
        }
    }

    if blob_count >= 10_000 {
        eprintln!();
    }

    let total_refs: u64 = blobs.iter().map(|b| b.ref_count).sum();
    eprintln!(
        "Scanned {} way blobs, {} total refs.\n",
        blobs.len(),
        total_refs
    );

    // Report per partition count.
    println!("# Partition selectivity report");
    println!();
    println!(
        "Input: {} ({} way blobs, {} way-node refs)",
        input.display(),
        blobs.len(),
        total_refs
    );
    println!();

    for (i, &n) in PARTITION_COUNTS.iter().enumerate() {
        let range_size = (MAX_NODE_ID + n - 1) / n;
        let index_per_partition_gb =
            (range_size as f64 * 8.0) / 1_000_000_000.0;

        let single_count = blobs
            .iter()
            .filter(|b| b.bitmaps[i].count_ones() == 1)
            .count();
        let single_refs: u64 = blobs
            .iter()
            .filter(|b| b.bitmaps[i].count_ones() == 1)
            .map(|b| b.ref_count)
            .sum();

        let multi_count = blobs.len() - single_count;
        let multi_refs = total_refs - single_refs;

        // Partition-touch distribution: how many partitions does each blob touch?
        let mut touch_dist: Vec<u64> = vec![0; (n as usize) + 1];
        for blob in &blobs {
            let touches = blob.bitmaps[i].count_ones() as usize;
            if touches < touch_dist.len() {
                touch_dist[touches] += 1;
            }
        }

        // Total way-blob reads across all partition passes.
        let total_blob_reads: u64 = blobs
            .iter()
            .map(|b| b.bitmaps[i].count_ones() as u64)
            .sum();

        println!("## N={} partitions", n);
        println!(
            "  Range per partition: ~{}M node IDs, ~{:.1} GB dense index",
            range_size / 1_000_000,
            index_per_partition_gb
        );
        println!(
            "  Single-partition blobs: {} / {} ({:.1}%)",
            single_count,
            blobs.len(),
            100.0 * single_count as f64 / blobs.len() as f64
        );
        println!(
            "  Single-partition refs:  {} / {} ({:.1}%)",
            single_refs,
            total_refs,
            100.0 * single_refs as f64 / total_refs as f64
        );
        println!(
            "  Multi-partition blobs:  {} ({:.1}%)",
            multi_count,
            100.0 * multi_count as f64 / blobs.len() as f64
        );
        println!(
            "  Multi-partition refs:   {} ({:.1}%)",
            multi_refs,
            100.0 * multi_refs as f64 / total_refs as f64
        );
        println!(
            "  Total blob reads across all passes: {} ({:.1}x)",
            total_blob_reads,
            total_blob_reads as f64 / blobs.len() as f64
        );
        println!("  Touch distribution:");
        for (touches, count) in touch_dist.iter().enumerate() {
            if *count > 0 {
                println!(
                    "    {} partition(s): {} blobs ({:.1}%)",
                    touches,
                    count,
                    100.0 * *count as f64 / blobs.len() as f64
                );
            }
        }
        println!();
    }

    // Min/max ref range analysis (for v1 metadata evaluation).
    println!("## Min/max ref range analysis (v1 metadata)");
    println!();
    for (i, &n) in PARTITION_COUNTS.iter().enumerate() {
        let range_size = (MAX_NODE_ID + n - 1) / n;

        // How many blobs would min/max skip vs bitmap skip?
        let minmax_single = blobs
            .iter()
            .filter(|b| {
                let min_p = (b.min_ref as u64) / range_size;
                let max_p = (b.max_ref as u64) / range_size;
                min_p == max_p
            })
            .count();

        let bitmap_single = blobs
            .iter()
            .filter(|b| b.bitmaps[i].count_ones() == 1)
            .count();

        println!(
            "  N={:2}: min/max single={} ({:.1}%), bitmap single={} ({:.1}%), bitmap advantage: +{} blobs",
            n,
            minmax_single,
            100.0 * minmax_single as f64 / blobs.len() as f64,
            bitmap_single,
            100.0 * bitmap_single as f64 / blobs.len() as f64,
            bitmap_single.saturating_sub(minmax_single),
        );
    }
}
