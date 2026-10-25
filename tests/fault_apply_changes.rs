//! Fault-injection tests for `apply_changes::merge`, through the fault plan
//! on the run's token (see `pbfhogg::test_hooks`): worker panic recovery
//! (`apply_changes_panic_at_blob_seq`) and a base renamed over mid-run
//! (`arm_input_replacement`).
//!
//! Companion: `fault_parallel_writer.rs` covers apply-changes'
//! default output backend.

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

        // Arm the hook (below, on the run's token) at blob seq 3 (an
        // arbitrary early-middle blob that
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
        };
        let host = crate::common::test_host();
        host.cancel
            .faults()
            .apply_changes_panic_at_blob_seq
            .store(3, std::sync::atomic::Ordering::Relaxed);

        // Catch only as a guard: the public API must return an error,
        // including when a missing node seq would otherwise hang the barrier.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            merge(
                &base,
                &osc,
                common::out(&output),
                &opts,
                &pbfhogg::HeaderOverrides::default(),
                &host,
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

    const REPLACEMENT_OSC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="7" lat="0.0000007" lon="0.0000007" version="2"/>
    <node id="23" lat="0.0000023" lon="0.0000023" version="2"/>
  </modify>
</osmChange>"#;

    /// A base renamed over mid-run: every later open of the base comes from
    /// the pin taken at the start, so on a host that reopens through
    /// `/proc/self/fd` the run reads the file it started on and writes what
    /// an unreplaced run writes, and where opens go through the path the
    /// run fails with `InputReplaced` and publishes nothing.
    ///
    /// On the splice route the base is opened four times after the pin: the
    /// front read, the splice source, then the scanner's walker and the
    /// worker pool's descriptor from their own threads. The replacement
    /// fires right after the splice source, after the OSC parse and before
    /// either producer opens, so both producer opens follow it and both of
    /// their failure paths run in the forced mode. It does not replace the
    /// base while blobs are in flight.
    #[test]
    fn base_replaced_after_the_splice_open_is_never_read() {
        let dir = crate::common::test_scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        // Different IDs and blob layout: a run that read any of it would
        // write different bytes, or fail.
        write_multi_block_test_pbf(&base, &generate_nodes(40, 1), &[], &[], 5);
        write_osc(&osc, REPLACEMENT_OSC);
        let opts = MergeOptions {
            write: WriteOptions {
                // A splice-capable backend: the base's untouched blobs are
                // copied from the splice source, the fourth derived open.
                backend: pbfhogg::write::OutputBackend::Parallel,
                ..WriteOptions::default()
            },
            parallelism: pbfhogg::Parallelism::from_jobs(2),
            ..MergeOptions::default()
        };
        let run = |host: &pbfhogg::host::Host, output: &Path| {
            merge(
                &base,
                &osc,
                common::out(output),
                &opts,
                &pbfhogg::HeaderOverrides::default(),
                host,
            )
        };

        let reference = dir.path().join("reference.osm.pbf");
        run(&crate::common::test_host(), &reference).expect("unreplaced run");
        let expected = std::fs::read(&reference).expect("read reference output");

        for force_path in [true, false] {
            let replacement = dir.path().join("replacement.osm.pbf");
            write_multi_block_test_pbf(&replacement, &generate_nodes(60, 1), &[], &[], 7);
            let host = crate::common::test_host();
            host.cancel
                .faults()
                .force_pin_path_reopen
                .store(force_path, std::sync::atomic::Ordering::Relaxed);
            let procfs = pbfhogg::read::input::PinnedInput::pin(&base, &host)
                .expect("probe pin")
                .reopens_through_procfs();
            host.cancel
                .faults()
                .arm_input_replacement(&base, &replacement, 2);
            let output = dir.path().join(format!("output-{force_path}.osm.pbf"));
            let before = common::snapshot_dir(dir.path());
            let result = run(&host, &output);

            let report = host
                .cancel
                .faults()
                .input_replacement()
                .expect("armed replacement");
            assert!(report.fired, "the replacement must fire: {report:?}");
            assert_eq!(report.attempts, 4, "{report:?}");
            assert_eq!(report.attempts_after_fired, 2, "{report:?}");

            if procfs {
                result.expect("a procfs-pinned run is unaffected by the replacement");
                assert_eq!(report.verified, 4, "{report:?}");
                assert_eq!(
                    std::fs::read(&output).expect("read output"),
                    expected,
                    "the run must read only the base it pinned"
                );
                std::fs::remove_file(&output).expect("remove output");
            } else if cfg!(unix) {
                // Off Unix a pin verifies nothing; the identity refusal is
                // a Unix guarantee.
                let err = result.err().expect("a path-reopened run must refuse");
                assert!(
                    matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                    "{err:?}"
                );
                assert_eq!(report.refused, 2, "{report:?}");
                assert!(!output.exists(), "a refused run must publish nothing");
                // The replacement file was renamed over the base, so it is
                // the one path the run itself did not account for.
                let mut after = common::snapshot_dir(dir.path());
                after.insert(std::path::PathBuf::from("replacement.osm.pbf"));
                common::assert_scratch_unchanged(&before, &after);
            }
            // Restore the original base for the next mode.
            write_multi_block_test_pbf(&base, &generate_nodes(40, 1), &[], &[], 5);
        }
    }
}
