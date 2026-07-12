//! Forced grid-vs-linear byte-for-byte parity for multi-extract.
//!
//! The `RegionGrid` spatial index only prunes which regions each node is
//! tested against; every surviving candidate still runs the exact
//! `contains_decimicro` / bbox test, so classification output - and therefore
//! the written PBF bytes - must be identical whether the grid is engaged or
//! not. This test drives ONE fixed 20-region config through both paths on the
//! same input and asserts the per-region output files are byte-for-byte equal.
//!
//! Under the `test-hooks` feature a process-global
//! `region_grid::FORCE_LINEAR` makes `RegionGrid::build` return `None`, so the
//! at/above-threshold config that would normally build a grid instead runs the
//! verbatim linear scan. Each `tests/fault_*.rs` compiles to its own binary,
//! so the static is per-process; the single test below toggles it serially.

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(feature = "test-hooks")]
mod grid_parity {
    use std::sync::atomic::Ordering;

    use pbfhogg::MemberId;
    use pbfhogg::cat::CleanAttrs;
    use pbfhogg::commands::extract::{
        Bbox, ExtractSlot, ExtractStrategy, PolygonRings, Region, extract_multi, parse_bbox,
    };
    use pbfhogg::read::region_grid_test_hooks as grid_hooks;
    use pbfhogg::writer::Compression;
    use tempfile::TempDir;

    use crate::common::{TestMember, TestNode, TestRelation, TestWay, write_indexed_pbf};

    fn build_nodes() -> Vec<TestNode> {
        // 500 nodes on a 1-degree grid over lon 0..19, lat 40..64.
        let mut v = Vec::new();
        let mut id = 1_i64;
        for r in 0..25_i32 {
            for c in 0..20_i32 {
                v.push(TestNode {
                    id,
                    lat: 400_000_000 + r * 10_000_000,
                    lon: c * 10_000_000,
                    tags: vec![],
                    meta: None,
                });
                id += 1;
            }
        }
        v
    }

    fn build_ways() -> Vec<TestWay> {
        (0..10_i64)
            .map(|k| TestWay {
                id: 1_000 + k,
                refs: vec![1 + k * 5, 2 + k * 5, 3 + k * 5],
                tags: vec![],
                meta: None,
            })
            .collect()
    }

    fn build_relations() -> Vec<TestRelation> {
        (0..3_i64)
            .map(|k| TestRelation {
                id: 2_000 + k,
                members: vec![
                    TestMember {
                        id: MemberId::Node(1 + k * 10),
                        role: "",
                    },
                    TestMember {
                        id: MemberId::Way(1_000 + k),
                        role: "",
                    },
                ],
                tags: vec![],
                meta: None,
            })
            .collect()
    }

    /// Tile origin for region `i`: a 5x4 lattice of overlapping tiles.
    fn tile_origin(i: usize) -> (f64, f64) {
        let lon0 = (i % 5) as f64 * 3.0;
        let lat0 = 40.0 + (i / 5) as f64 * 5.0;
        (lon0, lat0)
    }

    fn bbox_region(i: usize) -> Region {
        let (lon0, lat0) = tile_origin(i);
        Region::Bbox(parse_bbox(&format!("{lon0},{lat0},{},{}", lon0 + 4.0, lat0 + 6.0)).unwrap())
    }

    fn poly_region(i: usize) -> Region {
        let (lon0, lat0) = tile_origin(i);
        let (max_lon, max_lat) = (lon0 + 4.0, lat0 + 6.0);
        Region::Polygon {
            polygons: vec![PolygonRings {
                exterior: vec![
                    (lon0, lat0),
                    (max_lon, lat0),
                    (max_lon, max_lat),
                    (lon0, max_lat),
                    (lon0, lat0),
                ],
                holes: vec![],
            }],
            bbox: Bbox {
                min_lon: lon0,
                min_lat: lat0,
                max_lon,
                max_lat,
            },
        }
    }

    fn run_config<F>(
        input: &std::path::Path,
        dir: &std::path::Path,
        region: F,
    ) -> Vec<std::path::PathBuf>
    where
        F: Fn(usize) -> Region,
    {
        let slots: Vec<ExtractSlot> = (0..20)
            .map(|i| ExtractSlot {
                region: region(i),
                output: dir.join(format!("out-{i}.osm.pbf")),
            })
            .collect();
        extract_multi(
            input,
            &slots,
            ExtractStrategy::Simple,
            true,
            &CleanAttrs::default(),
            Compression::default(),
            false,
            true,
            &pbfhogg::HeaderOverrides::default(),
        )
        .expect("extract_multi");
        slots.into_iter().map(|s| s.output).collect()
    }

    fn assert_byte_identical(
        input: &std::path::Path,
        dir: &std::path::Path,
        region: fn(usize) -> Region,
    ) {
        let grid_dir = dir.join("grid");
        let lin_dir = dir.join("linear");
        std::fs::create_dir_all(&grid_dir).unwrap();
        std::fs::create_dir_all(&lin_dir).unwrap();

        // Grid engaged (n = 20 >= threshold, within budget).
        grid_hooks::reset();
        let grid_outs = run_config(input, &grid_dir, region);

        // Same config, grid build forced off -> verbatim linear scan.
        grid_hooks::FORCE_LINEAR.store(true, Ordering::Relaxed);
        let lin_outs = run_config(input, &lin_dir, region);
        grid_hooks::reset();

        for (g, l) in grid_outs.iter().zip(lin_outs.iter()) {
            let gb = std::fs::read(g).unwrap();
            let lb = std::fs::read(l).unwrap();
            assert_eq!(gb, lb, "grid vs linear differ for {}", g.display());
        }
    }

    #[test]
    fn forced_linear_vs_grid_produces_identical_bytes() {
        let dir = TempDir::new().unwrap();
        let input = dir.path().join("input.osm.pbf");
        write_indexed_pbf(&input, &build_nodes(), &build_ways(), &build_relations());

        // Mixed bbox + polygon (non-all-bbox path).
        let mixed_dir = dir.path().join("mixed");
        std::fs::create_dir_all(&mixed_dir).unwrap();
        assert_byte_identical(&input, &mixed_dir, |i| {
            if i % 2 == 0 {
                bbox_region(i)
            } else {
                poly_region(i)
            }
        });

        // All-bbox (columnar path).
        let bbox_dir = dir.path().join("bbox");
        std::fs::create_dir_all(&bbox_dir).unwrap();
        assert_byte_identical(&input, &bbox_dir, bbox_region);
    }
}
