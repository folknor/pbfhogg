//! Per-binary fault-injection test for `write::parallel_writer`
//! (apply-changes' default output backend).
//!
//! Split out of `tests/apply_changes_invariants.rs` (2026-04-25).
//! Each `tests/fault_*.rs` compiles to its own integration-test
//! binary, so the static `PANIC_AT_POOL_OP_COUNT` is per-process
//! and race-free without `#[ignore]` or `--test-threads=1`. The
//! hook shape was chosen over a per-instance `MergeOptions` field
//! because pool workers are spawned deep inside
//! `parallel_writer_thread_inner` - threading a config through
//! every entry point would be invasive. The serialization cost
//! that previously came from `--test-threads=1` is replaced by
//! per-binary isolation here.
//!
//! Note: `parallel_writer` assigns absolute `pwrite` offsets to
//! every op up-front. If a pool worker panics holding queued ops
//! in its channel, those ops' offsets go unwritten - the kernel
//! leaves zero-filled holes at those byte ranges. The command
//! must still return an error and must not claim success; that's
//! the invariant this test locks. Whether the resulting file is
//! truncated or merely malformed-and-holey is a secondary concern
//! covered by CHANGELOG notes, not by the
//! absence-or-truncation assertion (which is too strong for this
//! backend).

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(feature = "test-hooks")]
mod parallel_writer {
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    use flate2::write::GzEncoder;
    use pbfhogg::apply_changes::{MergeOptions, merge};
    use pbfhogg::write::parallel_writer_test_hooks as pw_hooks;
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

    /// Pool-worker panic inside `write::parallel_writer` must surface as
    /// a hard failure, not a silent short file.
    #[test]
    fn fault_injection_parallel_writer_pool_panic_surfaces_error() {
        // Static hook is process-global; reset before and after to
        // avoid leaking state to sibling tests in this binary (there
        // are none today, but keep the discipline so future additions
        // don't reintroduce a race).
        pw_hooks::reset();

        let dir = TempDir::new().expect("tempdir");
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let output = dir.path().join("output.osm.pbf");

        // Multi-blob base so the parallel_writer pool dispatches multiple
        // ops. 40 nodes in blocks of 5 + header = 9+ ops in flight.
        let nodes = generate_nodes(40, 1);
        write_multi_block_test_pbf(&base, &nodes, &[], &[], 5);
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

        let before = common::snapshot_dir(dir.path());

        // Arm: panic on the 3rd dispatched pool op. Low enough that many
        // later ops are still in-flight when the panic fires.
        pw_hooks::PANIC_AT_POOL_OP_COUNT.store(3, Ordering::Relaxed);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            merge(
                &base,
                &osc,
                &output,
                &MergeOptions {
                    compression: Compression::default(),
                    direct_io: false,
                    io_uring: false,
                    force: true,
                    locations_on_ways: false,
                    jobs: Some(2),
                    panic_at_blob_seq: None,
                },
                &pbfhogg::HeaderOverrides::default(),
            )
        }));

        // Always disarm; sibling tests must not inherit the hook.
        pw_hooks::reset();

        let silently_succeeded = matches!(result, Ok(Ok(_)));
        assert!(
            !silently_succeeded,
            "parallel_writer pool-worker panic must not produce a silent success"
        );

        let mut after = common::snapshot_dir(dir.path());
        after.remove(std::path::Path::new("output.osm.pbf"));
        common::assert_scratch_unchanged(&before, &after);
    }
}
