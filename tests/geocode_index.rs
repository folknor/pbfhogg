//! Reverse-geocode coverage: default synthetic smoke tests plus ignored
//! Denmark-scale integration tests.

mod common;

use std::path::Path;

use common::{TestMember, TestNode, TestRelation, TestWay, write_test_pbf_sorted};
use pbfhogg::MemberId;
use tempfile::TempDir;

fn write_synthetic_indexed_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode { id: 1, lat: 557_000_000, lon: 125_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 557_000_000, lon: 125_010_000, tags: vec![], meta: None },
            TestNode {
                id: 3,
                lat: 557_000_500,
                lon: 125_005_000,
                tags: vec![
                    ("addr:housenumber", "10"),
                    ("addr:street", "Main Street"),
                    ("addr:postcode", "1234"),
                ],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "residential"), ("name", "Main Street")],
            meta: None,
        }],
        &[],
    );
}

fn write_synthetic_admin_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode { id: 10, lat: 556_990_000, lon: 124_990_000, tags: vec![], meta: None },
            TestNode { id: 11, lat: 556_990_000, lon: 125_020_000, tags: vec![], meta: None },
            TestNode { id: 12, lat: 557_010_000, lon: 125_020_000, tags: vec![], meta: None },
            TestNode { id: 13, lat: 557_010_000, lon: 124_990_000, tags: vec![], meta: None },
        ],
        &[TestWay {
            id: 20,
            refs: vec![10, 11, 12, 13, 10],
            tags: vec![],
            meta: None,
        }],
        &[TestRelation {
            id: 30,
            members: vec![TestMember { id: MemberId::Way(20), role: "outer" }],
            tags: vec![
                ("type", "boundary"),
                ("boundary", "administrative"),
                ("admin_level", "2"),
                ("name", "Syntheticland"),
                ("ISO3166-1:alpha2", "SL"),
            ],
            meta: None,
        }],
    );
}

fn write_synthetic_nested_admin_same_level_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode { id: 40, lat: 556_980_000, lon: 124_980_000, tags: vec![], meta: None },
            TestNode { id: 41, lat: 556_980_000, lon: 125_030_000, tags: vec![], meta: None },
            TestNode { id: 42, lat: 557_020_000, lon: 125_030_000, tags: vec![], meta: None },
            TestNode { id: 43, lat: 557_020_000, lon: 124_980_000, tags: vec![], meta: None },
            TestNode { id: 44, lat: 556_995_000, lon: 124_995_000, tags: vec![], meta: None },
            TestNode { id: 45, lat: 556_995_000, lon: 125_015_000, tags: vec![], meta: None },
            TestNode { id: 46, lat: 557_005_000, lon: 125_015_000, tags: vec![], meta: None },
            TestNode { id: 47, lat: 557_005_000, lon: 124_995_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 50, refs: vec![40, 41, 42, 43, 40], tags: vec![], meta: None },
            TestWay { id: 51, refs: vec![44, 45, 46, 47, 44], tags: vec![], meta: None },
        ],
        &[
            TestRelation {
                id: 60,
                members: vec![TestMember { id: MemberId::Way(50), role: "outer" }],
                tags: vec![
                    ("type", "boundary"),
                    ("boundary", "administrative"),
                    ("admin_level", "8"),
                    ("name", "Bigshire"),
                ],
                meta: None,
            },
            TestRelation {
                id: 61,
                members: vec![TestMember { id: MemberId::Way(51), role: "outer" }],
                tags: vec![
                    ("type", "boundary"),
                    ("boundary", "administrative"),
                    ("admin_level", "8"),
                    ("name", "Smallshire"),
                ],
                meta: None,
            },
        ],
    );
}

fn write_synthetic_admin_with_hole_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode { id: 70, lat: 556_990_000, lon: 124_990_000, tags: vec![], meta: None },
            TestNode { id: 71, lat: 556_990_000, lon: 125_020_000, tags: vec![], meta: None },
            TestNode { id: 72, lat: 557_010_000, lon: 125_020_000, tags: vec![], meta: None },
            TestNode { id: 73, lat: 557_010_000, lon: 124_990_000, tags: vec![], meta: None },
            TestNode { id: 74, lat: 556_997_000, lon: 124_997_000, tags: vec![], meta: None },
            TestNode { id: 75, lat: 556_997_000, lon: 125_013_000, tags: vec![], meta: None },
            TestNode { id: 76, lat: 557_003_000, lon: 125_013_000, tags: vec![], meta: None },
            TestNode { id: 77, lat: 557_003_000, lon: 124_997_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 80, refs: vec![70, 71, 72, 73, 70], tags: vec![], meta: None },
            TestWay { id: 81, refs: vec![74, 75, 76, 77, 74], tags: vec![], meta: None },
        ],
        &[TestRelation {
            id: 90,
            members: vec![
                TestMember { id: MemberId::Way(80), role: "outer" },
                TestMember { id: MemberId::Way(81), role: "inner" },
            ],
            tags: vec![
                ("type", "boundary"),
                ("boundary", "administrative"),
                ("admin_level", "6"),
                ("name", "Holeland"),
            ],
            meta: None,
        }],
    );
}

fn write_synthetic_interpolation_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode {
                id: 100,
                lat: 557_000_000,
                lon: 125_000_000,
                tags: vec![("addr:housenumber", "10"), ("addr:street", "Interp Street")],
                meta: None,
            },
            TestNode {
                id: 101,
                lat: 557_000_000,
                lon: 125_060_000,
                tags: vec![("addr:housenumber", "30"), ("addr:street", "Interp Street")],
                meta: None,
            },
        ],
        &[TestWay {
            id: 200,
            refs: vec![100, 101],
            tags: vec![("addr:interpolation", "even"), ("addr:street", "Interp Street")],
            meta: None,
        }],
        &[],
    );
}

fn write_synthetic_unresolved_interpolation_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode {
                id: 110,
                lat: 557_000_000,
                lon: 125_000_000,
                tags: vec![("addr:housenumber", "10"), ("addr:street", "Missing End Street")],
                meta: None,
            },
            TestNode { id: 111, lat: 557_000_000, lon: 125_060_000, tags: vec![], meta: None },
        ],
        &[TestWay {
            id: 210,
            refs: vec![110, 111],
            tags: vec![
                ("addr:interpolation", "all"),
                ("addr:street", "Missing End Street"),
            ],
            meta: None,
        }],
        &[],
    );
}

fn write_synthetic_postal_boundary_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode { id: 300, lat: 556_990_000, lon: 124_990_000, tags: vec![], meta: None },
            TestNode { id: 301, lat: 556_990_000, lon: 125_020_000, tags: vec![], meta: None },
            TestNode { id: 302, lat: 557_010_000, lon: 125_020_000, tags: vec![], meta: None },
            TestNode { id: 303, lat: 557_010_000, lon: 124_990_000, tags: vec![], meta: None },
        ],
        &[TestWay {
            id: 310,
            refs: vec![300, 301, 302, 303, 300],
            tags: vec![],
            meta: None,
        }],
        &[TestRelation {
            id: 320,
            members: vec![TestMember { id: MemberId::Way(310), role: "outer" }],
            tags: vec![
                ("type", "boundary"),
                ("boundary", "postal_code"),
                ("postal_code", "4242"),
            ],
            meta: None,
        }],
    );
}

#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
fn generate_nonzero_nodes(count: usize, start_id: i64) -> Vec<TestNode> {
    (0..count)
        .map(|i| TestNode {
            id: start_id + i as i64,
            lat: 557_000_000 + (i % 1000) as i32,
            lon: 125_000_000 + (i / 1000) as i32,
            tags: vec![],
            meta: None,
        })
        .collect()
}

#[allow(clippy::cast_possible_wrap)]
fn write_oversized_street_input(path: &Path) {
    let node_count = 65_536;
    let nodes = generate_nonzero_nodes(node_count, 1);
    let refs: Vec<i64> = (1..=node_count).map(|id| id as i64).collect();

    write_test_pbf_sorted(
        path,
        &nodes,
        &[TestWay {
            id: 400,
            refs,
            tags: vec![("highway", "residential"), ("name", "Huge Street")],
            meta: None,
        }],
        &[],
    );
}

#[allow(clippy::cast_possible_wrap)]
fn write_oversized_interpolation_input(path: &Path) {
    let node_count = 65_536;
    let nodes = generate_nonzero_nodes(node_count, 1);
    let refs: Vec<i64> = (1..=node_count).map(|id| id as i64).collect();

    write_test_pbf_sorted(
        path,
        &nodes,
        &[TestWay {
            id: 410,
            refs,
            tags: vec![
                ("addr:interpolation", "all"),
                ("addr:street", "Huge Interp Street"),
            ],
            meta: None,
        }],
        &[],
    );
}

/// Tier B5b fixture: 65 536 tiny streets all sharing the same two
/// nodes, so every street's single segment lands in the same S2
/// cell. Triggers the per-cell street u16 cap at
/// `src/geocode_index/builder/pass3.rs:570`.
#[allow(clippy::cast_possible_wrap)]
fn write_street_cell_overflow_input(path: &Path) {
    let nodes = vec![
        TestNode { id: 1, lat: 557_000_000, lon: 125_000_000, tags: vec![], meta: None },
        TestNode { id: 2, lat: 557_000_010, lon: 125_000_010, tags: vec![], meta: None },
    ];
    let ways: Vec<TestWay> = (0..65_536u32)
        .map(|i| {
            let name: &'static str =
                Box::leak(format!("Street{i}").into_boxed_str());
            TestWay {
                id: 1_000 + i64::from(i),
                refs: vec![1, 2],
                tags: vec![("highway", "residential"), ("name", name)],
                meta: None,
            }
        })
        .collect();
    write_test_pbf_sorted(path, &nodes, &ways, &[]);
}

/// Tier B5b fixture: 65 536 tiny interpolation ways all sharing the
/// same two nodes, so every interp way's single segment lands in
/// the same S2 cell. Triggers the per-cell interp u16 cap at
/// `src/geocode_index/builder/pass3.rs:599`.
#[allow(clippy::cast_possible_wrap)]
fn write_interp_cell_overflow_input(path: &Path) {
    // Interp endpoints carry house numbers so endpoint resolution
    // produces real entries (otherwise unresolved interp ways may
    // be filtered before reaching the per-cell aggregation).
    let nodes = vec![
        TestNode {
            id: 1,
            lat: 557_000_000,
            lon: 125_000_000,
            tags: vec![("addr:housenumber", "1")],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 557_000_010,
            lon: 125_000_010,
            tags: vec![("addr:housenumber", "3")],
            meta: None,
        },
    ];
    let ways: Vec<TestWay> = (0..65_536u32)
        .map(|i| {
            let street: &'static str =
                Box::leak(format!("Interp{i}").into_boxed_str());
            TestWay {
                id: 1_000 + i64::from(i),
                refs: vec![1, 2],
                tags: vec![
                    ("addr:interpolation", "all"),
                    ("addr:street", street),
                ],
                meta: None,
            }
        })
        .collect();
    write_test_pbf_sorted(path, &nodes, &ways, &[]);
}

/// Tier B5 fixture: 65 536 admin relations all referencing the same
/// tiny outer ring, forcing every polygon's bbox into one S2 cell.
/// Each relation gets a unique `name` so it counts as a distinct
/// admin polygon. Triggers the per-cell admin u16 cap at
/// `src/geocode_index/builder/admin.rs:227`.
#[allow(clippy::cast_possible_wrap)]
fn write_admin_cell_overflow_input(path: &Path) {
    // Tiny closed ring: 4 nodes around (55.7, 12.5).
    let nodes = vec![
        TestNode { id: 1, lat: 557_000_000, lon: 125_000_000, tags: vec![], meta: None },
        TestNode { id: 2, lat: 557_000_010, lon: 125_000_000, tags: vec![], meta: None },
        TestNode { id: 3, lat: 557_000_010, lon: 125_000_010, tags: vec![], meta: None },
        TestNode { id: 4, lat: 557_000_000, lon: 125_000_010, tags: vec![], meta: None },
    ];
    // Closed way: ring of the 4 nodes.
    let ways = vec![TestWay {
        id: 100,
        refs: vec![1, 2, 3, 4, 1],
        tags: vec![],
        meta: None,
    }];
    // 65 536 admin relations, all referencing the same outer way.
    // Each gets a unique name (via leaked Box<str>) so the builder
    // treats them as distinct admin polygons. The names are leaked
    // because TestRelation::tags wants `&'static str`; the test
    // process exits soon after, so the leak is bounded.
    let relations: Vec<TestRelation> = (0..65_536u32)
        .map(|i| {
            let name: &'static str =
                Box::leak(format!("Polygon{i}").into_boxed_str());
            TestRelation {
                id: 1_000 + i64::from(i),
                members: vec![TestMember { id: MemberId::Way(100), role: "outer" }],
                tags: vec![
                    ("boundary", "administrative"),
                    ("admin_level", "8"),
                    ("name", name),
                    ("type", "boundary"),
                ],
                meta: None,
            }
        })
        .collect();

    write_test_pbf_sorted(path, &nodes, &ways, &relations);
}

fn write_addr_cell_overflow_input(path: &Path) {
    let node_count = 65_536;
    let nodes: Vec<TestNode> = (0..node_count)
        .map(|i| TestNode {
            id: 1 + i as i64,
            lat: 557_000_000,
            lon: 125_000_000,
            tags: vec![
                ("addr:housenumber", "1"),
                ("addr:street", "Dense Address Cell"),
            ],
            meta: None,
        })
        .collect();

    write_test_pbf_sorted(path, &nodes, &[], &[]);
}

#[test]
fn synthetic_build_query_and_api_equivalence() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_indexed_input(&input);

    let stats = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir.clone(),
            force: false,
            ..Default::default()
        },
    )
    .expect("build should succeed");

    assert_eq!(stats.addr_points, 1, "synthetic fixture has one address node");
    assert_eq!(stats.street_ways, 1, "synthetic fixture has one named street way");
    assert_eq!(stats.interp_ways, 0, "synthetic fixture has no interpolation ways");
    assert_eq!(stats.admin_polygons, 0, "synthetic fixture has no admin relations");

    let reader = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .expect("reader should open");

    let result = reader.query(55.70005, 12.5005);
    let addr = result.address.as_ref().expect("query should find the address");
    assert_eq!(addr.house_number, "10");
    assert_eq!(addr.street, "Main Street");
    assert_eq!(addr.postcode, Some("1234"));
    let street = result.street.as_ref().expect("query should find the street");
    assert_eq!(street.name, "Main Street");
    assert!(result.interpolation.is_none(), "fixture has no interpolation data");
    assert!(result.admin.is_empty(), "fixture has no admin polygons");

    let via_candidates = reader.candidates(55.70005, 12.5005).into_result(&reader);
    let cand_addr = via_candidates.address.as_ref().expect("candidates should find the address");
    assert_eq!(cand_addr.house_number, "10");
    assert_eq!(cand_addr.street, "Main Street");
    assert_eq!(cand_addr.postcode, Some("1234"));
    let cand_street = via_candidates.street.as_ref().expect("candidates should find the street");
    assert_eq!(cand_street.name, "Main Street");
    assert!(via_candidates.interpolation.is_none(), "fixture has no interpolation data");
    assert!(
        via_candidates.admin.is_empty(),
        "fixture has no admin polygons"
    );

    let far = reader.query(0.0, 0.0);
    assert!(far.address.is_none(), "far-away query must not invent an address");
    assert!(far.street.is_none(), "far-away query must not invent a street");
    assert!(
        far.interpolation.is_none(),
        "far-away query must not invent interpolation"
    );
    assert!(far.admin.is_empty(), "far-away query must not invent admin");
}

#[test]
fn coarse_fallback_recovers_hits_outside_fine_radius() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let no_fallback_dir = dir.path().join("index_no_fallback");
    let coarse_fallback_dir = dir.path().join("index_with_fallback");

    write_synthetic_indexed_input(&input);

    for (output_dir, coarse_search_radius_m) in [
        (&no_fallback_dir, 1.0_f32),
        (&coarse_fallback_dir, 100.0_f32),
    ] {
        pbfhogg::geocode_index::builder::build_geocode_index(
            &pbfhogg::geocode_index::builder::BuildConfig {
                input_path: input.clone(),
                output_dir: output_dir.clone(),
                force: false,
                fine_search_radius_m: 1.0,
                coarse_search_radius_m,
                ..Default::default()
            },
        )
        .expect("build should succeed");
    }

    let query_lat = 55.7002;
    let query_lon = 12.5005;

    let no_fallback = pbfhogg::geocode_index::reader::Reader::open(&no_fallback_dir)
        .expect("reader should open");
    let no_fallback_result = no_fallback.query(query_lat, query_lon);
    assert!(
        no_fallback_result.address.is_none(),
        "1m fine radius should miss the address"
    );
    assert!(
        no_fallback_result.street.is_none(),
        "1m fine radius should miss the street"
    );

    let with_fallback = pbfhogg::geocode_index::reader::Reader::open(&coarse_fallback_dir)
        .expect("reader should open");
    let with_fallback_result = with_fallback.query(query_lat, query_lon);
    let addr = with_fallback_result
        .address
        .as_ref()
        .expect("coarse fallback should recover the nearby address");
    assert_eq!(addr.house_number, "10");
    assert_eq!(addr.street, "Main Street");
    let street = with_fallback_result
        .street
        .as_ref()
        .expect("coarse fallback should recover the nearby street");
    assert_eq!(street.name, "Main Street");
    assert!(
        with_fallback_result.admin.is_empty(),
        "fixture has no admin polygons"
    );
}

#[test]
fn synthetic_admin_polygon_query_returns_country_match() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_admin_input(&input);

    let stats = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir.clone(),
            force: false,
            ..Default::default()
        },
    )
    .expect("build should succeed");

    assert_eq!(stats.addr_points, 0, "admin-only fixture has no address points");
    assert_eq!(stats.street_ways, 0, "admin-only fixture has no street ways");
    assert_eq!(stats.admin_polygons, 1, "fixture has one admin boundary");

    let reader = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .expect("reader should open");

    let inside = reader.query(55.7000, 12.5005);
    assert!(inside.address.is_none(), "admin-only fixture has no addresses");
    assert!(inside.street.is_none(), "admin-only fixture has no streets");
    assert!(
        inside.interpolation.is_none(),
        "admin-only fixture has no interpolation"
    );
    let country = inside
        .admin
        .iter()
        .find(|admin| admin.admin_level == 2)
        .expect("query inside the polygon should return the country boundary");
    assert_eq!(country.name, "Syntheticland");
    assert_eq!(country.country_code, Some("SL"));

    let outside = reader.query(55.7020, 12.5005);
    assert!(
        outside.admin.is_empty(),
        "query outside the polygon must not match the country boundary"
    );
}

#[test]
fn nested_same_level_admin_prefers_smallest_polygon() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_nested_admin_same_level_input(&input);

    let stats = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir.clone(),
            force: false,
            ..Default::default()
        },
    )
    .expect("build should succeed");

    assert_eq!(stats.admin_polygons, 2, "fixture has two admin polygons at the same level");

    let reader = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .expect("reader should open");

    let query_lat = 55.7000;
    let query_lon = 12.5005;

    let raw_candidates = reader.candidates(query_lat, query_lon);
    let raw_level8: Vec<_> = raw_candidates
        .admin
        .iter()
        .filter(|admin| admin.admin_level == 8)
        .map(|admin| admin.name)
        .collect();
    assert_eq!(raw_level8.len(), 2, "raw candidates should expose both containing polygons");
    assert!(raw_level8.contains(&"Bigshire"));
    assert!(raw_level8.contains(&"Smallshire"));

    let collapsed = reader.query(query_lat, query_lon);
    let collapsed_level8: Vec<_> = collapsed
        .admin
        .iter()
        .filter(|admin| admin.admin_level == 8)
        .collect();
    assert_eq!(collapsed_level8.len(), 1, "query() should collapse same-level admin matches");
    assert_eq!(collapsed_level8[0].name, "Smallshire");

    let via_into_result = reader.candidates(query_lat, query_lon).into_result(&reader);
    let into_result_level8: Vec<_> = via_into_result
        .admin
        .iter()
        .filter(|admin| admin.admin_level == 8)
        .collect();
    assert_eq!(
        into_result_level8.len(),
        1,
        "candidates().into_result() should also collapse same-level admin matches"
    );
    assert_eq!(into_result_level8[0].name, "Smallshire");
}

#[test]
fn admin_polygon_hole_excludes_queries_inside_the_hole() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_admin_with_hole_input(&input);

    let stats = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir.clone(),
            force: false,
            ..Default::default()
        },
    )
    .expect("build should succeed");

    assert_eq!(stats.admin_polygons, 1, "fixture has one admin polygon with one hole");

    let reader = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .expect("reader should open");

    let shell = reader.query(55.6992, 12.4992);
    let shell_admin = shell
        .admin
        .iter()
        .find(|admin| admin.admin_level == 6)
        .expect("query inside the shell should match the admin polygon");
    assert_eq!(shell_admin.name, "Holeland");

    let hole = reader.query(55.7000, 12.5005);
    assert!(
        hole.admin.iter().all(|admin| admin.admin_level != 6),
        "query inside the inner ring must not match the polygon"
    );

    let raw_hole = reader.candidates(55.7000, 12.5005);
    assert!(
        raw_hole.admin.iter().all(|admin| admin.admin_level != 6),
        "candidates() must also respect admin holes"
    );
}

#[test]
fn synthetic_interpolation_query_resolves_even_house_number() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_interpolation_input(&input);

    let stats = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir.clone(),
            force: false,
            coarse_search_radius_m: 1.0,
            ..Default::default()
        },
    )
    .expect("build should succeed");

    assert_eq!(stats.addr_points, 2, "fixture has two endpoint address nodes");
    assert_eq!(stats.street_ways, 0, "fixture has no named street way");
    assert_eq!(stats.interp_ways, 1, "fixture has one interpolation way");

    let reader = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .expect("reader should open");

    let query_lat = 55.7000;
    let query_lon = 12.5030;
    let result = reader.query(query_lat, query_lon);
    assert!(
        result.address.is_none(),
        "midpoint query should stay outside the fine address radius"
    );
    assert!(result.street.is_none(), "fixture has no street geometry");
    let interp = result
        .interpolation
        .as_ref()
        .expect("query should resolve the interpolation");
    assert_eq!(interp.street, "Interp Street");
    assert_eq!(interp.house_number, 20, "midpoint on the interpolation should resolve to 20");
    assert!(result.admin.is_empty(), "fixture has no admin polygons");

    let candidates = reader.candidates(query_lat, query_lon);
    assert!(
        !candidates.interpolations.is_empty(),
        "candidates should expose the raw interpolation hit"
    );
    let via_candidates = reader.candidates(query_lat, query_lon).into_result(&reader);
    let cand_interp = via_candidates
        .interpolation
        .as_ref()
        .expect("candidates().into_result() should resolve the interpolation");
    assert_eq!(cand_interp.street, "Interp Street");
    assert_eq!(cand_interp.house_number, 20);
}

#[test]
fn unresolved_interpolation_stays_hidden_behind_zero_sentinel() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_unresolved_interpolation_input(&input);

    let stats = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir.clone(),
            force: false,
            coarse_search_radius_m: 1.0,
            ..Default::default()
        },
    )
    .expect("build should succeed");

    assert_eq!(stats.addr_points, 1, "fixture has only one matching endpoint address");
    assert_eq!(stats.interp_ways, 1, "fixture still writes one interpolation way");

    let reader = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .expect("reader should open");

    let query_lat = 55.7000;
    let query_lon = 12.5030;
    let result = reader.query(query_lat, query_lon);
    assert!(result.address.is_none(), "midpoint query should not hit the lone endpoint address");
    assert!(
        result.interpolation.is_none(),
        "unresolved endpoints stay at 0/0 and must not surface as an interpolation result"
    );

    let candidates = reader.candidates(query_lat, query_lon);
    let candidate = candidates
        .interpolations
        .iter()
        .min_by(|a, b| a.distance_m.partial_cmp(&b.distance_m).unwrap_or(std::cmp::Ordering::Equal))
        .expect("raw candidates should still expose the interpolation segment");
    assert!(
        reader.interpolate(candidate).is_none(),
        "reader.interpolate() should hide unresolved 0/0 interpolation endpoints"
    );
    assert!(
        reader
            .candidates(query_lat, query_lon)
            .into_result(&reader)
            .interpolation
            .is_none(),
        "candidates().into_result() should also hide unresolved interpolation"
    );
}

#[test]
fn coarse_fallback_recovers_interpolation_outside_fine_radius() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let no_fallback_dir = dir.path().join("index_no_fallback");
    let coarse_fallback_dir = dir.path().join("index_with_fallback");

    write_synthetic_interpolation_input(&input);

    for (output_dir, coarse_search_radius_m) in [
        (&no_fallback_dir, 1.0_f32),
        (&coarse_fallback_dir, 100.0_f32),
    ] {
        pbfhogg::geocode_index::builder::build_geocode_index(
            &pbfhogg::geocode_index::builder::BuildConfig {
                input_path: input.clone(),
                output_dir: output_dir.clone(),
                force: false,
                fine_search_radius_m: 1.0,
                coarse_search_radius_m,
                ..Default::default()
            },
        )
        .expect("build should succeed");
    }

    let query_lat = 55.7002;
    let query_lon = 12.5030;

    let no_fallback = pbfhogg::geocode_index::reader::Reader::open(&no_fallback_dir)
        .expect("reader should open");
    let no_fallback_result = no_fallback.query(query_lat, query_lon);
    assert!(
        no_fallback_result.address.is_none(),
        "fine-only lookup should miss the endpoint addresses"
    );
    assert!(
        no_fallback_result.street.is_none(),
        "fixture has no street geometry"
    );
    assert!(
        no_fallback_result.interpolation.is_none(),
        "1m fine radius should miss the interpolation segment"
    );

    let with_fallback = pbfhogg::geocode_index::reader::Reader::open(&coarse_fallback_dir)
        .expect("reader should open");
    let with_fallback_result = with_fallback.query(query_lat, query_lon);
    assert!(
        with_fallback_result.address.is_none(),
        "coarse interpolation fallback should not invent an endpoint address"
    );
    assert!(
        with_fallback_result.street.is_none(),
        "fixture has no street geometry"
    );
    let interp = with_fallback_result
        .interpolation
        .as_ref()
        .expect("coarse fallback should recover the interpolation");
    assert_eq!(interp.street, "Interp Street");
    assert_eq!(interp.house_number, 20);

    let candidates = with_fallback.candidates(query_lat, query_lon);
    assert!(
        !candidates.interpolations.is_empty(),
        "coarse candidates should expose the raw interpolation hit"
    );
}

#[test]
fn synthetic_postal_boundary_query_returns_level_11_match() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_postal_boundary_input(&input);

    let stats = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir.clone(),
            force: false,
            ..Default::default()
        },
    )
    .expect("build should succeed");

    assert_eq!(stats.admin_polygons, 1, "fixture has one postal boundary polygon");

    let reader = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .expect("reader should open");

    let inside = reader.query(55.7000, 12.5005);
    let postal = inside
        .admin
        .iter()
        .find(|admin| admin.admin_level == 11)
        .expect("query inside the polygon should return the postal-code boundary");
    assert_eq!(postal.name, "4242");
    assert_eq!(
        postal.country_code, None,
        "postal-code boundaries should not fabricate a country code"
    );

    let outside = reader.query(55.7020, 12.5005);
    assert!(
        outside.admin.iter().all(|admin| admin.admin_level != 11),
        "query outside the polygon must not match the postal boundary"
    );
}

#[test]
fn build_rejects_street_way_coord_count_over_u16_max() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_oversized_street_input(&input);

    let err = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir,
            force: false,
            ..Default::default()
        },
    )
    .expect_err("builder should hard-error on street node_count overflow");

    let msg = err.to_string();
    assert!(
        msg.contains("street way: 65536 coords exceeds u16::MAX"),
        "unexpected error: {msg}"
    );
}

#[test]
fn build_rejects_interpolation_way_coord_count_over_u16_max() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_oversized_interpolation_input(&input);

    let err = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir,
            force: false,
            ..Default::default()
        },
    )
    .expect_err("builder should hard-error on interpolation node_count overflow");

    let msg = err.to_string();
    assert!(
        msg.contains("interp way: 65536 coords exceeds u16::MAX"),
        "unexpected error: {msg}"
    );
}

#[test]
fn build_rejects_addr_entry_count_over_u16_max_for_one_cell() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_addr_cell_overflow_input(&input);

    let err = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir,
            force: false,
            ..Default::default()
        },
    )
    .expect_err("builder should hard-error on per-cell addr entry overflow");

    let msg = err.to_string();
    assert!(
        msg.contains("has 65536 addr entries, exceeds u16::MAX"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("geocode Stage B: cell"),
        "unexpected error: {msg}"
    );
}

/// Tier B5b: per-cell street u16 entry cap.
/// `src/geocode_index/builder/pass3.rs:570`. Distinct from the
/// pre-batch per-WAY node_count cap test
/// (`build_rejects_street_way_coord_count_over_u16_max`) - that one
/// pins the cap on a single oversized way; this one pins the cap
/// on aggregation across many small ways landing in the same S2
/// cell.
#[test]
fn build_rejects_street_entry_count_over_u16_max_for_one_cell() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_street_cell_overflow_input(&input);

    let err = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir,
            force: false,
            ..Default::default()
        },
    )
    .expect_err("builder should hard-error on per-cell street entry overflow");

    let msg = err.to_string();
    assert!(
        msg.contains("street entries, exceeds u16::MAX"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("geocode Stage B: cell"),
        "unexpected error: {msg}"
    );
}

/// Tier B5b: per-cell interp u16 entry cap.
/// `src/geocode_index/builder/pass3.rs:599`. Distinct from the
/// pre-batch per-WAY node_count cap test
/// (`build_rejects_interpolation_way_coord_count_over_u16_max`).
#[test]
fn build_rejects_interp_entry_count_over_u16_max_for_one_cell() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_interp_cell_overflow_input(&input);

    let err = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir,
            force: false,
            ..Default::default()
        },
    )
    .expect_err("builder should hard-error on per-cell interp entry overflow");

    let msg = err.to_string();
    assert!(
        msg.contains("interp entries, exceeds u16::MAX"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("geocode Stage B: cell"),
        "unexpected error: {msg}"
    );
}

/// Tier B5: per-cell admin u16 cap. Pre-batch tests covered the
/// street, interpolation, and addr u16 caps; the admin cell cap at
/// `src/geocode_index/builder/admin.rs:227` was unpinned. The
/// fixture creates 65 536 admin relations all sharing the same
/// outer ring (so every polygon's bbox lands in the same S2 cell).
#[test]
fn build_rejects_admin_entry_count_over_u16_max_for_one_cell() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_admin_cell_overflow_input(&input);

    let err = pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            output_dir: index_dir,
            force: false,
            ..Default::default()
        },
    )
    .expect_err("builder should hard-error on per-cell admin entry overflow");

    let msg = err.to_string();
    assert!(
        msg.contains("entries, exceeds u16::MAX"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("admin"),
        "error must mention the admin cell context: {msg}"
    );
}

fn denmark_indexed_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PBFHOGG_TEST_PBF_INDEXED") {
        return std::path::PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("data/denmark-20260220-seq4704-with-indexdata.osm.pbf")
}

fn output_dir() -> std::path::PathBuf {
    // Use existing scratch directory if available (built by brokkr run),
    // otherwise use target/geocode-test.
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/scratch/geocode-denmark");
    if scratch.join("geocode_header.bin").exists() {
        return scratch;
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/geocode-test")
}

fn ensure_index_exists() -> Option<std::path::PathBuf> {
    let dir = output_dir();
    if dir.join("geocode_header.bin").exists() {
        return Some(dir);
    }
    // Try building (only reasonable in release mode)
    let input = denmark_indexed_path();
    if !input.exists() {
        return None;
    }
    let build_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/geocode-test");
    if build_dir.exists() {
        std::fs::remove_dir_all(&build_dir).ok();
    }
    let config = pbfhogg::geocode_index::builder::BuildConfig {
        input_path: input,
        output_dir: build_dir.clone(),
        force: false,
        ..Default::default()
    };
    pbfhogg::geocode_index::builder::build_geocode_index(&config).ok()?;
    Some(build_dir)
}

/// Build the index and verify counts are reasonable for Denmark.
#[test]
#[ignore]
fn build_denmark_index() {
    let input = denmark_indexed_path();
    if !input.exists() {
        eprintln!("Skipping: {} not found", input.display());
        return;
    }

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/geocode-test-build");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).ok();
    }

    let config = pbfhogg::geocode_index::builder::BuildConfig {
        input_path: input,
        output_dir: dir.clone(),
        force: false,
        ..Default::default()
    };
    let stats = pbfhogg::geocode_index::builder::build_geocode_index(&config)
        .expect("build should succeed");

    assert!(stats.addr_points > 2_000_000, "expected >2M addr, got {}", stats.addr_points);
    assert!(stats.street_ways > 200_000, "expected >200K streets, got {}", stats.street_ways);
    assert!(stats.admin_polygons > 100, "expected >100 admin, got {}", stats.admin_polygons);
    assert!(stats.fine_cells > 100_000, "expected >100K fine cells, got {}", stats.fine_cells);

    // Verify all index files exist
    let expected_files = [
        "geocode_header.bin", "geo_cells.bin", "street_entries.bin",
        "addr_entries.bin", "interp_entries.bin", "coarse_geo_cells.bin",
        "coarse_street_entries.bin", "coarse_addr_entries.bin",
        "coarse_interp_entries.bin", "street_ways.bin", "street_nodes.bin",
        "addr_points.bin", "interp_ways.bin", "interp_nodes.bin",
        "admin_cells.bin", "admin_entries.bin", "admin_polygons.bin",
        "admin_vertices.bin", "strings.bin",
    ];
    for name in &expected_files {
        assert!(dir.join(name).exists(), "missing index file: {name}");
    }
}

/// Query Copenhagen City Hall and verify address/street/admin results.
#[test]
#[ignore]
fn query_copenhagen() {
    let Some(dir) = ensure_index_exists() else {
        eprintln!("Skipping: index not available");
        return;
    };
    let reader = pbfhogg::geocode_index::reader::Reader::open(&dir)
        .expect("reader should open");

    // 55.6761°N, 12.5683°E - Copenhagen City Hall / Rådhuspladsen
    let result = reader.query(55.6761, 12.5683);

    assert!(
        result.address.is_some() || result.street.is_some(),
        "expected address or street near Copenhagen City Hall"
    );
    assert!(!result.admin.is_empty(), "expected admin matches");

    // Country level
    let country = result.admin.iter().find(|a| a.admin_level == 2);
    assert!(country.is_some(), "expected country-level admin");
    if let Some(c) = country {
        assert!(
            c.name.contains("Danmark") || c.name.contains("Denmark"),
            "expected Danmark/Denmark, got '{}'", c.name
        );
    }

    if let Some(addr) = &result.address {
        eprintln!("Address: {} {} (dist: {:.1}m)", addr.street, addr.house_number, addr.distance_m);
    }
    if let Some(st) = &result.street {
        eprintln!("Street: {} (dist: {:.1}m)", st.name, st.distance_m);
    }
}

/// Rural query: admin should resolve even where streets are sparse.
#[test]
#[ignore]
fn query_rural_jutland() {
    let Some(dir) = ensure_index_exists() else {
        eprintln!("Skipping: index not available");
        return;
    };
    let reader = pbfhogg::geocode_index::reader::Reader::open(&dir)
        .expect("reader should open");

    // 57.5°N, 10.0°E - sparse area in northern Jutland
    let result = reader.query(57.5, 10.0);
    let country = result.admin.iter().find(|a| a.admin_level == 2);
    assert!(country.is_some(), "rural point should resolve country");
}

/// Outside Denmark: no results expected.
#[test]
#[ignore]
fn query_north_sea() {
    let Some(dir) = ensure_index_exists() else {
        eprintln!("Skipping: index not available");
        return;
    };
    let reader = pbfhogg::geocode_index::reader::Reader::open(&dir)
        .expect("reader should open");

    // 56.0°N, 4.0°E - North Sea
    let result = reader.query(56.0, 4.0);
    assert!(result.address.is_none(), "no address in the North Sea");
    assert!(result.street.is_none(), "no street in the North Sea");
}

/// API equivalence: query() should produce the same result as candidates().into_result().
#[test]
#[ignore]
fn api_equivalence() {
    let Some(dir) = ensure_index_exists() else {
        eprintln!("Skipping: index not available");
        return;
    };
    let reader = pbfhogg::geocode_index::reader::Reader::open(&dir)
        .expect("reader should open");

    let points = [(55.6761, 12.5683), (56.15, 10.2), (55.4, 12.3)];

    for &(lat, lon) in &points {
        let rq = reader.query(lat, lon);
        let rc = reader.candidates(lat, lon).into_result(&reader);

        assert_eq!(rq.address.is_some(), rc.address.is_some(),
            "disagree on address at ({lat}, {lon})");
        assert_eq!(rq.street.is_some(), rc.street.is_some(),
            "disagree on street at ({lat}, {lon})");

        let q_levels: Vec<u8> = rq.admin.iter().map(|a| a.admin_level).collect();
        let c_levels: Vec<u8> = rc.admin.iter().map(|a| a.admin_level).collect();
        assert_eq!(q_levels, c_levels,
            "disagree on admin levels at ({lat}, {lon})");

        if let (Some(qa), Some(ca)) = (&rq.address, &rc.address) {
            assert_eq!(qa.street, ca.street,
                "disagree on address street at ({lat}, {lon})");
        }
    }
}

/// Coarse fallback should find results in areas with sparse street coverage.
#[test]
#[ignore]
fn coarse_fallback() {
    let Some(dir) = ensure_index_exists() else {
        eprintln!("Skipping: index not available");
        return;
    };
    let reader = pbfhogg::geocode_index::reader::Reader::open(&dir)
        .expect("reader should open");

    // Thy National Park, northern Jutland: 56.92°N, 8.52°E
    let result = reader.query(56.92, 8.52);

    // Admin should resolve regardless
    let country = result.admin.iter().find(|a| a.admin_level == 2);
    assert!(country.is_some(), "should find country in rural Thy");

    eprintln!("Thy: address={} street={} admin={}",
        result.address.is_some(), result.street.is_some(), result.admin.len());
}
