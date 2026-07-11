//! Per-binary fault-injection test for ALTW external stage 3.
//!
//! Split out of `tests/fault_injection.rs` (2026-04-25). Each
//! `tests/fault_*.rs` compiles to its own integration-test binary,
//! so the static `PANIC_AT_BUCKET_IDX` is per-process and race-free
//! without `#[ignore]` or `--test-threads=1`.

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(feature = "test-hooks")]
mod altw_external_stage3 {
    use std::sync::atomic::Ordering;

    use pbfhogg::altw::{
        AltwOptions, IndexType, add_locations_to_ways, external_test_hooks as altw_hooks,
    };
    use pbfhogg::writer::Compression;

    use crate::common::{
        TestNode, TestWay, generate_nodes, generate_ways, snapshot_dir, write_multi_block_test_pbf,
    };

    /// A stage-3 worker panic during altw external join must surface
    /// as an `Err`, wake stage-4 waiters via the `AbortOnDrop` guard
    /// (no indefinite hang), and leave no `external-join-*` scratch
    /// tree behind after `ScratchDir::drop` fires.
    #[test]
    fn fault_injection_altw_stage3_bucket_panic_surfaces_and_cleans_scratch() {
        altw_hooks::stage3::reset();

        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("in.osm.pbf");
        let output = dir.path().join("out.osm.pbf");

        // Multi-blob fixture: enough nodes + ways to produce several
        // slot buckets in stage 3. Block size 5 keeps a realistic
        // number of blobs without blowing out test wall time.
        let nodes: Vec<TestNode> = generate_nodes(60, 1);
        // Ways reference node ids 1..=60 so stage 1's way pass emits
        // real coord requests.
        let ways: Vec<TestWay> = generate_ways(10, 10_000, 6, 1);
        write_multi_block_test_pbf(&input, &nodes, &ways, &[], 5);

        let before = snapshot_dir(dir.path());

        // Arm bucket 1 (middle-ish of stage-3's slot_bucket_count).
        altw_hooks::stage3::PANIC_AT_BUCKET_IDX.store(1, Ordering::Relaxed);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            add_locations_to_ways(
                &input,
                &output,
                &AltwOptions {
                    keep_untagged_nodes: false,
                    compression: Compression::default(),
                    direct_io: false,
                    force: true,
                    index_type: IndexType::External,
                    inject_prepass: false,
                },
                &pbfhogg::HeaderOverrides::default(),
            )
        }));

        altw_hooks::stage3::reset();

        let silently_succeeded = matches!(result, Ok(Ok(_)));
        assert!(
            !silently_succeeded,
            "altw external with an armed stage-3 panic must not return Ok"
        );

        // ScratchDir::drop sweeps its entire tree. Assert no
        // `external-join-*` directory remains as a leftover.
        let mut after = snapshot_dir(dir.path());
        // Output file may or may not exist depending on how far the
        // pipeline got; exclude it from the scratch-leak check.
        after.remove(std::path::Path::new("out.osm.pbf"));
        let scratch_leaks: Vec<_> = after
            .difference(&before)
            .filter(|p| p.to_string_lossy().contains("external-join"))
            .collect();
        assert!(
            scratch_leaks.is_empty(),
            "altw external leaked scratch after panic: {scratch_leaks:?}"
        );
    }
}
