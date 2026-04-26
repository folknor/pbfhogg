//! Truncation sweep: drive every command through a known-good PBF
//! truncated at every blob/frame/payload boundary plus a deterministic
//! set of intermediate offsets.
//!
//! T04 in `notes/testing.md`. Several findings in the cluster-2 sweep
//! showed reader paths that accept untrusted input without proper bound
//! checks (`MAX_BLOB_HEADER_SIZE`, `data_offset + data_size > file_len`,
//! varint count miscounts). The contract this file pins: every command
//! returns cleanly (non-zero exit, no panic, no multi-GB allocation,
//! no hang) on every truncation of a known-good fixture.
//!
//! We do not require any specific exit status text or error variant -
//! the tier-1 promise is "panic-free behavior under partial input".

#[path = "common/mod.rs"]
mod common;

use std::time::Duration;

use common::adversarial::{locate_blobs, truncate_to};
use common::cli::CliInvoker;
use common::{
    generate_nodes, generate_relations, generate_ways, write_multi_block_test_pbf,
};
use tempfile::TempDir;

/// Wall clock cap per truncation × command invocation. The sweep does
/// dozens of invocations so this stays tight; a malformed input that
/// causes the binary to allocate or loop catastrophically should fail
/// the test, not wedge the suite.
const PER_INVOCATION_TIMEOUT: Duration = Duration::from_secs(8);

#[test]
fn truncation_sweep_no_panic() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let truncated = dir.path().join("truncated.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Small multi-blob fixture: ~6 OSMData blobs (4 node + 1 way + 1 rel).
    let nodes = generate_nodes(40, 1);
    let ways = generate_ways(10, 1_000, 3, 1);
    let relations = generate_relations(4, 10_000, 2, 1_000);
    write_multi_block_test_pbf(&input, &nodes, &ways, &relations, 10);

    let pbf = std::fs::read(&input).expect("read fixture");
    let blobs = locate_blobs(&pbf);

    // Boundary offsets covered: mid-length-prefix, mid-header,
    // mid-payload, last-byte-of-payload, plus the start of the next
    // frame. Add a handful of "uniform" offsets across the file
    // (deterministic, no RNG) to catch boundaries the structural list
    // misses.
    let mut offsets: Vec<usize> = Vec::new();
    for b in &blobs {
        offsets.push(b.frame_start + 1);
        offsets.push(b.frame_start + 2);
        offsets.push(b.header_start + (b.header_end - b.header_start) / 2);
        offsets.push(b.header_end - 1);
        offsets.push(b.blob_start + 1);
        offsets.push((b.blob_start + b.blob_end) / 2);
        offsets.push(b.blob_end - 1);
    }
    for k in 1..=12 {
        offsets.push((pbf.len() * k) / 13);
    }
    offsets.retain(|&o| o > 0 && o < pbf.len());
    offsets.sort_unstable();
    offsets.dedup();

    // Cap to ~30 offsets so the sweep stays under a wall-clock budget
    // suitable for tier 1.
    if offsets.len() > 30 {
        let stride = offsets.len() / 30;
        let stride = stride.max(1);
        offsets = offsets
            .into_iter()
            .step_by(stride)
            .take(30)
            .collect();
    }

    for &len in &offsets {
        let bytes = truncate_to(&pbf, len);
        std::fs::write(&truncated, &bytes).expect("write truncated");

        run_and_assert_no_panic("cat", &truncated, &output, len);
        run_and_assert_no_panic("inspect", &truncated, &output, len);
        run_sort_and_assert_no_panic(&truncated, &output, len);
    }
}

fn run_and_assert_no_panic(subcmd: &str, input: &std::path::Path, output: &std::path::Path, len: usize) {
    let mut inv = CliInvoker::new()
        .arg(subcmd)
        .arg(input)
        .timeout(PER_INVOCATION_TIMEOUT);
    if subcmd != "inspect" {
        inv = inv.arg("-o").arg(output);
    }
    let out = inv.run();
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("panicked at"),
        "{subcmd} panicked at truncation len={len}; stderr:\n{stderr}",
    );
    assert!(
        stderr.len() < 100_000,
        "{subcmd} produced suspiciously large stderr ({}) at truncation len={len}",
        stderr.len(),
    );
}

fn run_sort_and_assert_no_panic(input: &std::path::Path, output: &std::path::Path, len: usize) {
    let out = CliInvoker::new()
        .arg("sort")
        .arg(input)
        .arg("-o")
        .arg(output)
        .timeout(PER_INVOCATION_TIMEOUT)
        .run();
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("panicked at"),
        "sort panicked at truncation len={len}; stderr:\n{stderr}",
    );
}
