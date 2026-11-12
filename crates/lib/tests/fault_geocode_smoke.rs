//! The geocode builder's post-write smoke test, driven by a fault plan that
//! files one address point under the wrong cell at one level
//! (`arm_geocode_misfile_address`, see `pbfhogg::test_hooks`). The street
//! entries stay where they belong, so a check that accepted a street near
//! the address would pass; this one must refuse publication instead.

#![allow(clippy::unwrap_used)]

mod geocode_smoke {
    use std::path::Path;

    use pbfhogg::geocode_index::builder::{
        BuildConfig, BuildStats, SmokeOutcome, build_geocode_index, index_output,
    };
    use pbfhogg::geocode_index::reader::Reader;
    use pbfhogg::test_hooks::GeocodeCellLevel;

    fn build(
        input: &Path,
        index_dir: &Path,
        overwrite: pbfhogg::Overwrite,
        misfile: Option<GeocodeCellLevel>,
        scratch_root: &Path,
    ) -> pbfhogg::Result<BuildStats> {
        let host = crate::common::test_host();
        if let Some(level) = misfile {
            host.invocation
                .faults()
                .arm_geocode_misfile_address(0, level);
        }
        build_geocode_index(
            input,
            &BuildConfig::default(),
            index_output(index_dir, overwrite, &pbfhogg::host::Diagnostics::discard())?,
            &host,
            crate::common::scratch_in(scratch_root),
        )
    }

    fn assert_refused(result: pbfhogg::Result<BuildStats>, level: &str) {
        let message = match result {
            Ok(stats) => panic!("misfiled address built and published: {stats:?}"),
            Err(e) => e.report(),
        };
        assert!(
            message.contains(&format!("is not listed in its own {level} cell")),
            "the build failed, but not at the smoke test: {message}"
        );
    }

    fn refuses_new_destination(level: GeocodeCellLevel, name: &str) {
        let dir = crate::common::test_scratch();
        let input = dir.path().join("input.osm.pbf");
        crate::common::write_synthetic_indexed_input(&input);
        let index_dir = dir.path().join("index");

        let result = build(
            &input,
            &index_dir,
            pbfhogg::Overwrite::Refuse,
            Some(level),
            dir.path(),
        );
        assert_refused(result, name);
        assert!(
            !index_dir.exists(),
            "a refused build left a directory at the output path"
        );
    }

    fn keeps_existing_index(level: GeocodeCellLevel, name: &str) {
        let dir = crate::common::test_scratch();
        let input = dir.path().join("input.osm.pbf");
        crate::common::write_synthetic_indexed_input(&input);
        let index_dir = dir.path().join("index");

        let stats = build(
            &input,
            &index_dir,
            pbfhogg::Overwrite::Refuse,
            None,
            dir.path(),
        )
        .expect("clean build");
        assert_eq!(stats.smoke_test, SmokeOutcome::Passed);
        let before = read_tree(&index_dir);

        let result = build(
            &input,
            &index_dir,
            pbfhogg::Overwrite::Replace,
            Some(level),
            dir.path(),
        );
        assert_refused(result, name);
        assert_eq!(
            read_tree(&index_dir),
            before,
            "a refused build changed the index it would have replaced"
        );
        Reader::open(&index_dir).expect("the existing index still opens");
    }

    fn read_tree(dir: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn misfiled_fine_address_refuses_publication() {
        refuses_new_destination(GeocodeCellLevel::Fine, "fine");
    }

    #[test]
    fn misfiled_coarse_address_refuses_publication() {
        refuses_new_destination(GeocodeCellLevel::Coarse, "coarse");
    }

    #[test]
    fn misfiled_fine_address_keeps_the_existing_index() {
        keeps_existing_index(GeocodeCellLevel::Fine, "fine");
    }

    #[test]
    fn misfiled_coarse_address_keeps_the_existing_index() {
        keeps_existing_index(GeocodeCellLevel::Coarse, "coarse");
    }
}
