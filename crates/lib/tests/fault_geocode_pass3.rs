//! Fault injection into geocode builder Pass 3 Stage A, through the fault
//! plan on the run's token (`geocode_streets_panic_at_way`, see
//! `pbfhogg::test_hooks`).

#![allow(clippy::unwrap_used)]

mod geocode_pass3 {
    use std::sync::atomic::Ordering;

    use pbfhogg::block_builder::{self, BlockBuilder};
    use pbfhogg::geocode_index::builder::{BuildConfig, build_geocode_index, index_output};
    use pbfhogg::writer::{Compression, PbfWriter};

    use crate::common::snapshot_dir;

    /// A rayon worker panic inside geocode builder's Pass 3 Stage A
    /// streets loop unwinds through Rayon's collection boundary to the
    /// caller, and the run scratch owner must sweep the bucket trees on that
    /// unwind. The unwind is the contract: panic disposition belongs to the
    /// caller (ADR-0016), so the builder frames no panic-to-error boundary,
    /// like every other command's worker pools.
    #[test]
    fn fault_injection_geocode_pass3_streets_panic_sweeps_bucket_dirs() {
        let dir = crate::common::test_scratch();
        let input = dir.path().join("input.osm.pbf");
        let index_dir = dir.path().join("index");

        // Multi-way fixture: 6 tagged highway ways at distinct
        // coordinates so Stage A's streets chunk processes more
        // than one way_idx before the panic hook fires at way 2.
        // Keep it small so test wall-time stays low.
        build_multi_street_input(&input);

        let before = snapshot_dir(dir.path());

        let host = crate::common::test_host();
        host.invocation
            .faults()
            .geocode_streets_panic_at_way
            .store(2, Ordering::Relaxed);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_geocode_index(
                &BuildConfig {
                    input_path: input.clone(),
                    ..Default::default()
                },
                index_output(
                    &index_dir,
                    pbfhogg::Overwrite::Refuse,
                    &pbfhogg::host::Diagnostics::discard(),
                )
                .expect("stage output"),
                &host,
                crate::common::scratch_in(dir.path()),
            )
        }));

        // Require the current unwind behavior and the armed hook's payload.
        // Accepting both Err and unwind would hide a change in that boundary.
        let payload = match result {
            Err(payload) => payload,
            Ok(Err(error)) => panic!(
                "expected the worker panic to unwind, got: {}",
                error.report()
            ),
            Ok(Ok(_)) => panic!("the armed streets panic returned success"),
        };
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        assert!(
            message.contains("geocode pass3 stage A streets way 2 panicking"),
            "the build failed, but not at the armed hook: {message}"
        );
        // A forced mid-build failure publishes nothing: no index directory
        // under the name the caller chose, and nothing new beside it - no
        // staging directory and no `.buckets-levelN` scratch.
        assert!(
            !index_dir.exists(),
            "a failed build left a directory at the output path"
        );
        let after = snapshot_dir(dir.path());
        let leaks: Vec<_> = after.difference(&before).collect();
        assert!(
            leaks.is_empty(),
            "geocode builder left entries behind after the panic: {leaks:?}"
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
