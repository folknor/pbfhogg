//! Benchmark: apply an OSC diff to a base PBF.
//!
//! Usage: bench_merge <base.osm.pbf> <diff.osc.gz> [runs] [--direct-io]

use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: bench_merge <base.osm.pbf> <diff.osc.gz> [runs] [--direct-io]");
        std::process::exit(1);
    }

    let base = Path::new(&args[1]);
    let diff = Path::new(&args[2]);
    let direct_io = args.iter().any(|a| a == "--direct-io");

    let runs: usize = args
        .iter()
        .skip(3)
        .filter(|a| !a.starts_with('-'))
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);

    let base_mb = std::fs::metadata(base)
        .map(|m| m.len() / 1_000_000)
        .unwrap_or(0);

    let mode = if direct_io { "merge-direct" } else { "merge" };
    eprintln!("=== pbfhogg merge benchmark ===");
    eprintln!("base: {} ({base_mb} MB)", base.display());
    eprintln!("diff: {}", diff.display());
    eprintln!("runs: {runs} (best of)");
    eprintln!("direct-io: {direct_io}");
    eprintln!();

    let output = bench_output_path();
    drop(std::fs::create_dir_all(output.parent().expect("has parent")));

    let mut best_ms = u64::MAX;
    let mut best_stats = None;

    for _ in 0..runs {
        drop(std::fs::remove_file(&output));
        let start = Instant::now();
        let stats =
            pbfhogg::merge::merge(base, diff, &output, pbfhogg::writer::Compression::default(), direct_io).expect("merge failed");
        #[allow(clippy::cast_possible_truncation)]
        let ms = start.elapsed().as_millis() as u64;
        if ms < best_ms {
            best_ms = ms;
            best_stats = Some(stats);
        }
    }

    let stats = best_stats.expect("no runs completed");
    let output_mb = std::fs::metadata(&output)
        .map(|m| m.len() / 1_000_000)
        .unwrap_or(0);

    drop(std::fs::remove_file(&output));

    eprintln!("---");
    eprintln!("tool=pbfhogg");
    eprintln!("mode={mode}");
    eprintln!("elapsed_ms={best_ms}");
    eprintln!("base_nodes={}", stats.base_nodes);
    eprintln!("base_ways={}", stats.base_ways);
    eprintln!("base_relations={}", stats.base_relations);
    eprintln!("diff_nodes={}", stats.diff_nodes);
    eprintln!("diff_ways={}", stats.diff_ways);
    eprintln!("diff_relations={}", stats.diff_relations);
    eprintln!("blobs_passthrough={}", stats.blobs_passthrough);
    eprintln!("blobs_rewritten={}", stats.blobs_rewritten);
    eprintln!("blobs_skip_decompress={}", stats.blobs_skip_decompress);
    eprintln!("file_mb={base_mb}");
    eprintln!("output_mb={output_mb}");
}

/// Output path under data/bench-tmp/ (real filesystem, O_DIRECT compatible).
fn bench_output_path() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("data");
    dir.push("bench-tmp");
    dir.join("bench-merge-output.osm.pbf")
}
