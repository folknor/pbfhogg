//! Truncation sweep: drive every command through a known-good PBF
//! truncated at every blob/frame/payload boundary plus a deterministic
//! set of intermediate offsets.
//!
//! T04 in `notes/testing.md`. The contract this file pins follows
//! [`reference/truncation-handling.md`](../reference/truncation-handling.md):
//!
//! - **Shape 1** (clean cut at a frame boundary, 0-3 leftover bytes
//!   from an incomplete next length prefix): tolerated. Commands may
//!   exit 0; we only require no panic + bounded stderr.
//! - **Shapes 2-4** (length prefix past EOF, mid-header EOF, mid-
//!   payload EOF): hard error. Commands MUST exit non-zero.
//!
//! Decompression failure (shape 5) is exercised separately by the
//! `mutate_blob_payload`-based tests in `cli_defensive_input.rs`.

#[path = "common/mod.rs"]
mod common;

use std::time::Duration;

use common::adversarial::{locate_blobs, truncate_to, BlobLocation};
use common::cli::CliInvoker;
use common::{
    generate_nodes, generate_relations, generate_ways, write_multi_block_test_pbf,
};
use tempfile::TempDir;

/// Wall clock cap per truncation × command invocation.
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

    let mut offsets: Vec<usize> = Vec::new();
    for b in &blobs {
        // Shape 1 boundaries (1-3 bytes into the length prefix).
        offsets.push(b.frame_start + 1);
        offsets.push(b.frame_start + 2);
        offsets.push(b.frame_start + 3);
        // Shape 3 boundaries (committed length prefix, header truncated).
        offsets.push(b.header_start + (b.header_end - b.header_start) / 2);
        offsets.push(b.header_end - 1);
        // Shape 4 boundaries (committed header, payload truncated).
        offsets.push(b.blob_start + 1);
        offsets.push((b.blob_start + b.blob_end) / 2);
        offsets.push(b.blob_end - 1);
    }
    // Uniform offsets across the file (deterministic, no RNG) to catch
    // boundaries the structural list misses. Each gets classified
    // below.
    for k in 1..=12 {
        offsets.push((pbf.len() * k) / 13);
    }
    offsets.retain(|&o| o > 0 && o < pbf.len());
    offsets.sort_unstable();
    offsets.dedup();

    for &len in &offsets {
        let bytes = truncate_to(&pbf, len);
        std::fs::write(&truncated, &bytes).expect("write truncated");

        let tolerated = is_tolerated_offset(&blobs, len);
        run_and_assert("cat", &truncated, &output, len, tolerated);
        run_and_assert("inspect", &truncated, &output, len, tolerated);
        run_and_assert("sort", &truncated, &output, len, tolerated);
    }
}

/// An offset is tolerated (shape 1) if it falls within [0, 4) bytes
/// past any frame_start - i.e. the file ends with an incomplete next
/// length prefix or exactly at a frame boundary.
fn is_tolerated_offset(blobs: &[BlobLocation], offset: usize) -> bool {
    for b in blobs {
        if offset >= b.frame_start && offset < b.frame_start + 4 {
            return true;
        }
        if offset >= b.frame_start + 4 && offset < b.blob_end {
            // Inside the header or payload of a committed frame.
            return false;
        }
    }
    // Past the end of the last blob - clean EOF.
    true
}

fn run_and_assert(
    subcmd: &str,
    input: &std::path::Path,
    output: &std::path::Path,
    len: usize,
    tolerated: bool,
) {
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
    if !tolerated {
        assert!(
            !out.status.success(),
            "{subcmd} must hard-error on a shape-2/3/4 truncation \
             (len={len}); per `reference/truncation-handling.md` only \
             shape 1 (0-3 bytes past a frame boundary) is tolerated. \
             stdout:\n{}\nstderr:\n{stderr}",
            out.stdout_str(),
        );
    }
    // Shape 1 (tolerated): no exit-status assertion; commands may
    // exit 0 with the partial frame skipped, or exit non-zero if
    // they choose to surface the leftover bytes more strictly.
    // Both behaviors comply with the reference doc.
}
