//! Fault-injection test for `apply_changes::merge` worker panic
//! recovery (per-instance hook).
//!
//! Uses the per-instance `MergeOptions::panic_at_blob_seq` hook
//! (carried on the public config struct), so it cannot fire in a
//! sibling test and lives in the shared `all` binary. The static-hook
//! fault tests are the ones that need a process of their own.
//!
//! Companion: `fault_parallel_writer.rs` covers apply-changes'
//! default output backend via the static atomic in
//! `parallel_writer::test_hooks`, in its own binary.

#![allow(clippy::unwrap_used)]

#[cfg(feature = "test-hooks")]
mod apply_changes {
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;

    use flate2::write::GzEncoder;
    use pbfhogg::apply_changes::{MergeOptions, merge};
    use pbfhogg::block_builder::{self, BlockBuilder};
    use pbfhogg::write::WriteOptions;
    use pbfhogg::writer::{Compression, PbfWriter};

    use crate::common::{self, TestNode, generate_nodes, write_multi_block_test_pbf};

    fn write_osc(path: &Path, xml: &str) {
        let file = File::create(path).expect("create osc file");
        let mut enc = GzEncoder::new(file, flate2::Compression::fast());
        enc.write_all(xml.as_bytes()).expect("write xml");
        enc.finish().expect("finish gz");
    }

    /// Node-only sorted base whose header claims LocationsOnWays, one blob
    /// per `block_size` nodes, written like `write_multi_block_test_pbf`.
    fn write_multi_block_locations_pbf(path: &Path, nodes: &[TestNode], block_size: usize) {
        let file = File::create(path).expect("create base");
        let mut writer = PbfWriter::new(file, Compression::default());
        let header = block_builder::HeaderBuilder::new()
            .sorted()
            .optional_feature("LocationsOnWays")
            .build()
            .expect("build header");
        writer.write_header(&header).expect("write header");
        let mut bb = BlockBuilder::new();
        for chunk in nodes.chunks(block_size) {
            for n in chunk {
                bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), None);
            }
            if let Some(bytes) = bb.take().expect("take") {
                writer.write_primitive_block(bytes).expect("write block");
            }
        }
        writer.flush().expect("flush base");
    }

    /// A worker panic must return its original message as an error, release
    /// the node barrier when enabled, and leave no scratch behind.
    #[test]
    fn fault_injection_worker_panic_surfaces_error_and_leaves_scratch_clean() {
        check_worker_panic(false);
        check_worker_panic(true);
    }

    fn check_worker_panic(locations_on_ways: bool) {
        let dir = crate::common::test_scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let output = dir.path().join("output.osm.pbf");

        // Multi-blob base: 40 nodes split across blocks of 5 -> 8 node blobs.
        // Enough blobs that the scanner dispatches several candidates before
        // the injected panic fires.
        let nodes = generate_nodes(40, 1);
        if locations_on_ways {
            // `merge --locations-on-ways` refuses a base whose header lacks
            // LocationsOnWays before any worker runs, so the panic path
            // needs a base that claims it.
            write_multi_block_locations_pbf(&base, &nodes, 5);
        } else {
            write_multi_block_test_pbf(&base, &nodes, &[], &[], 5);
        }

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
        // Two workers is the minimum supported worker count.
        // FailureSlot publication and drain shutdown handle the panic;
        // this test does not establish single-worker barrier safety.
        // One worker remains rejected up front by `merge()`; see
        // `merge_rejects_jobs_equal_one`. `force` is refused alongside
        // `locations_on_ways` (it can never take effect there).
        let opts = MergeOptions {
            write: WriteOptions {
                force: !locations_on_ways,
                ..WriteOptions::default()
            },
            locations_on_ways,
            ignore_missing_nodes: false,
            parallelism: pbfhogg::Parallelism::from_jobs(2),
            allow_version_regression: false,
            panic_at_blob_seq: Some(3),
        };

        // Catch only as a guard: the public API must return an error,
        // including when a missing node seq would otherwise hang the barrier.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            merge(
                &base,
                &osc,
                common::out(&output),
                &opts,
                &pbfhogg::HeaderOverrides::default(),
                &crate::common::test_host(),
            )
        }));
        let err = match result {
            Ok(Err(e)) => e.to_string(),
            Ok(Ok(_)) => panic!("merge must fail when the worker panic hook fires"),
            Err(_) => panic!("merge must return a worker panic as an error, not unwind"),
        };
        assert!(
            err.contains("streaming worker panicked")
                && err.contains("panic_at_blob_seq=3 triggered"),
            "merge must preserve the worker panic payload, got: {err}"
        );

        // A forced mid-run failure leaves nothing behind: no output file
        // under the name the caller chose, no staging file beside it, no
        // scratch.
        let after = common::snapshot_dir(dir.path());
        common::assert_scratch_unchanged(&before, &after);
        assert!(
            !output.exists(),
            "a worker panic mid-stream must leave no output file at the destination"
        );
    }
}
