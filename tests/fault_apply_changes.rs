//! Per-binary fault-injection test for `apply_changes::merge` worker
//! panic recovery (per-instance hook).
//!
//! Split out of `tests/apply_changes_invariants.rs` (2026-04-25).
//! Uses the per-instance `MergeOptions::panic_at_blob_seq` hook
//! (carried on the public config struct), so it does not race with
//! sibling tests even when run in the same binary - but lives here
//! as the canonical apply-changes fault test so the per-binary
//! pattern is uniform across pipelines.
//!
//! Companion: `fault_parallel_writer.rs` covers apply-changes'
//! default output backend via the static atomic in
//! `parallel_writer::test_hooks`.

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(feature = "test-hooks")]
mod apply_changes {
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;

    use flate2::write::GzEncoder;
    use pbfhogg::apply_changes::{MergeOptions, merge};
    use pbfhogg::writer::Compression;
    use tempfile::TempDir;

    use crate::common::{
        self, generate_nodes, write_multi_block_test_pbf,
    };

    fn write_osc(path: &Path, xml: &str) {
        let file = File::create(path).expect("create osc file");
        let mut enc = GzEncoder::new(file, flate2::Compression::fast());
        enc.write_all(xml.as_bytes()).expect("write xml");
        enc.finish().expect("finish gz");
    }

    /// A worker panic mid-stream surfaces as either a returned Err or a
    /// propagated panic (depending on which pipeline stage errors first),
    /// the command never silently succeeds, scratch is clean, and the
    /// output file is absent-or-truncated (never a zero-filled short file
    /// masquerading as valid output).
    #[test]
    fn fault_injection_worker_panic_surfaces_error_and_leaves_scratch_clean() {
        let dir = TempDir::new().expect("tempdir");
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let output = dir.path().join("output.osm.pbf");

        // Multi-blob base: 40 nodes split across blocks of 5 -> 8 node blobs.
        // Enough blobs that the scanner dispatches several candidates before
        // the injected panic fires.
        let nodes = generate_nodes(40, 1);
        write_multi_block_test_pbf(&base, &nodes, &[], &[], 5);

        // OSC that creates node candidates overlapping every base blob.
        // The scanner will route every node blob to the worker pool.
        write_osc(
            &osc,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="1" lat="0.0000001" lon="0.0000001" version="2"/>
    <node id="10" lat="0.0000010" lon="0.0000010" version="2"/>
    <node id="20" lat="0.0000020" lon="0.0000020" version="2"/>
    <node id="30" lat="0.0000030" lon="0.0000030" version="2"/>
  </modify>
</osmChange>"#,
        );

        // Snapshot the tempdir before the run. The three input files are
        // already present; the output file is not. After the panic we
        // compare and assert nothing beyond those + the output file
        // changed.
        let before = common::snapshot_dir(dir.path());

        // Arm the hook at blob seq 3 (an arbitrary early-middle blob that
        // corresponds to a Candidate dispatched to the worker pool).
        //
        // `jobs: Some(2)`: two workers so the surviving one keeps
        // draining `candidate_rx` when the other panics. `Some(1)` is
        // now rejected up front by `merge()` (the deadlock would fire
        // if it weren't); see the companion test
        // `merge_rejects_jobs_equal_one`.
        let opts = MergeOptions {
            compression: Compression::default(),
            direct_io: false,
            io_uring: false,
            force: true,
            locations_on_ways: false,
            jobs: Some(2),
            panic_at_blob_seq: Some(3),
        };

        // A worker panic propagates through thread::scope. Depending on
        // whether the drain's "channel closed" check fires first, merge()
        // may either return Err or panic - both are acceptable: the
        // invariant is "does not silently succeed" + "scratch stays clean"
        // + "output is absent-or-truncated-to-zero".
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            merge(&base, &osc, &output, &opts, &pbfhogg::HeaderOverrides::default())
        }));
        let silently_succeeded = matches!(result, Ok(Ok(_)));
        assert!(
            !silently_succeeded,
            "merge must not silently succeed when panic_at_blob_seq fires"
        );

        // Scratch tracking: the only new path relative to `before` is the
        // output file (possibly created). Remove it from the after-set
        // before comparing.
        let mut after = common::snapshot_dir(dir.path());
        after.remove(std::path::Path::new("output.osm.pbf"));
        common::assert_scratch_unchanged(&before, &after);

        // Output file: if it exists at all, must be short enough to be
        // unmistakably incomplete. A full output would be at least the
        // size of the input base; an abandoned stream is either absent
        // or smaller.
        if output.exists() {
            let out_len = std::fs::metadata(&output).expect("stat output").len();
            let base_len = std::fs::metadata(&base).expect("stat base").len();
            assert!(
                out_len < base_len,
                "output ({out_len} bytes) must be truncated relative to base ({base_len} bytes) \
                 when a worker panics mid-stream"
            );
        }
    }
}
