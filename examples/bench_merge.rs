//! Benchmark: apply an OSC diff to a base PBF.
//!
//! Usage: bench_merge <base.osm.pbf> <diff.osc.gz> [runs]

use std::path::Path;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: bench_merge <base.osm.pbf> <diff.osc.gz> [runs]");
        std::process::exit(1);
    }

    let base = Path::new(&args[1]);
    let diff = Path::new(&args[2]);
    let runs: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);

    let base_mb = std::fs::metadata(base)
        .map(|m| m.len() / 1_000_000)
        .unwrap_or(0);

    eprintln!("=== pbfhogg merge benchmark ===");
    eprintln!("base: {} ({base_mb} MB)", base.display());
    eprintln!("diff: {}", diff.display());
    eprintln!("runs: {runs} (best of)");
    eprintln!();

    let tmp_dir = std::env::temp_dir();
    let output = tmp_dir.join("pbfhogg-bench-merge-output.osm.pbf");

    let mut best_ms = u64::MAX;
    let mut best_stats = None;

    for _ in 0..runs {
        drop(std::fs::remove_file(&output));
        let start = Instant::now();
        let stats = pbfhogg::merge::merge(base, diff, &output, false).expect("merge failed");
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
    eprintln!("mode=merge");
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
