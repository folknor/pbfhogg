//! Per-binary fault-injection test for geocode builder Pass 3 Stage A.
//!
//! Split out of `tests/fault_injection.rs` (2026-04-25). Each
//! `tests/fault_*.rs` compiles to its own integration-test binary,
//! so the static `PANIC_AT_STREETS_WAY_IDX` is per-process and
//! race-free without `#[ignore]` or `--test-threads=1`.

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(feature = "test-hooks")]
mod geocode_pass3 {
    use std::sync::atomic::Ordering;

    use pbfhogg::block_builder::{self, BlockBuilder};
    use pbfhogg::geocode_index::builder::{
        build_geocode_index, pass3_test_hooks as pass3_hooks, BuildConfig,
    };
    use pbfhogg::writer::{Compression, PbfWriter};

    use crate::common::snapshot_dir;

    /// A rayon worker panic inside geocode builder's Pass 3 Stage A
    /// streets loop must surface as an `Err`, and the
    /// `PathGuard::dir()` wrappers around the `.buckets-levelN`
    /// scratch trees (see ADR-0003) must sweep them on the unwind.
    #[test]
    fn fault_injection_geocode_pass3_streets_panic_sweeps_bucket_dirs() {
        pass3_hooks::reset();

        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("input.osm.pbf");
        let index_dir = dir.path().join("index");

        // Multi-way fixture: 6 tagged highway ways at distinct
        // coordinates so Stage A's streets chunk processes more
        // than one way_idx before the panic hook fires at way 2.
        // Keep it small so test wall-time stays low.
        build_multi_street_input(&input);

        let before = snapshot_dir(dir.path());

        pass3_hooks::PANIC_AT_STREETS_WAY_IDX.store(2, Ordering::Relaxed);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_geocode_index(&BuildConfig {
                input_path: input.clone(),
                output_dir: index_dir.clone(),
                force: false,
                ..Default::default()
            })
        }));

        pass3_hooks::reset();

        let silently_succeeded = matches!(result, Ok(Ok(_)));
        assert!(
            !silently_succeeded,
            "geocode build with an armed Pass 3 streets panic must not return Ok"
        );

        // The `.buckets-levelN` directories are wrapped in
        // `PathGuard::dir()`; any unwind sweeps them. Assert none
        // remain in the index dir.
        let after = snapshot_dir(dir.path());
        let bucket_leaks: Vec<_> = after
            .difference(&before)
            .filter(|p| p.to_string_lossy().contains(".buckets-level"))
            .collect();
        assert!(
            bucket_leaks.is_empty(),
            "geocode builder leaked .buckets-level* scratch dirs after panic: {bucket_leaks:?}"
        );
    }

    /// Build a small PBF fixture with 6 highway ways (3 nodes each,
    /// non-collinear so they emit real S2 cell covers). Each way
    /// tagged `highway=residential` + `name=*` so geocode treats
    /// them as streets and Pass 3 Stage A processes them.
    fn build_multi_street_input(path: &std::path::Path) {
        let file = std::fs::File::create(path).expect("create");
        let header = block_builder::HeaderBuilder::new()
            .bbox(12.0, 55.0, 13.0, 56.0)
            .sorted()
            .build()
            .expect("header");
        let mut writer = PbfWriter::new(file, Compression::default());
        writer.write_header(&header).expect("write header");

        let mut bb = BlockBuilder::new();
        // 6 streets × 3 nodes = 18 nodes. Node ids start at 1 and
        // grow sequentially per street.
        const STREET_COUNT: i32 = 6;
        let mut node_id = 1i64;
        let mut ways: Vec<(i64, Vec<i64>, &'static str)> =
            Vec::with_capacity(STREET_COUNT as usize);
        for w in 0i32..STREET_COUNT {
            let base_lat = 557_000_000 + w * 10_000;
            let base_lon = 125_000_000 + w * 10_000;
            let way_id = 1_000 + i64::from(w);
            let mut refs = Vec::with_capacity(3);
            for i in 0i32..3 {
                bb.add_node(
                    node_id,
                    base_lat + i * 1_000,
                    base_lon + i * 1_000,
                    std::iter::empty::<(&str, &str)>(),
                    None,
                );
                refs.push(node_id);
                node_id += 1;
            }
            ways.push((way_id, refs, "Test Street"));
        }
        writer
            .write_primitive_block(bb.take().expect("nodes").expect("nonempty"))
            .expect("write nodes");

        for (way_id, refs, name) in &ways {
            bb.add_way(
                *way_id,
                [("highway", "residential"), ("name", *name)],
                refs,
                None,
            );
        }
        writer
            .write_primitive_block(bb.take().expect("ways").expect("nonempty"))
            .expect("write ways");
        writer.flush().expect("flush");
    }
}
