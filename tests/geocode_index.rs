//! Reverse-geocode coverage on synthetic inputs.
//!
//! The index builder lives behind the `commands` feature (and the
//! reader behind `geocode-reader`, which `commands` enables), so
//! `tests/all.rs` mounts this module only with `commands` on.

use std::path::Path;

use crate::common::{TestMember, TestNode, TestRelation, TestWay, write_test_pbf_sorted};
use pbfhogg::MemberId;

/// Build an index into `output_dir`, replacing an existing one when
/// `overwrite` is set: the output is checked and staged first, exactly as
/// the CLI does, then handed to the builder, which publishes it.
pub(crate) fn build_index(
    config: &pbfhogg::geocode_index::builder::BuildConfig,
    output_dir: &Path,
    overwrite: bool,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<pbfhogg::geocode_index::builder::BuildStats> {
    let output = pbfhogg::geocode_index::builder::index_output(
        output_dir,
        pbfhogg::Overwrite::from_flag(overwrite),
    )?;
    pbfhogg::geocode_index::builder::build_geocode_index(
        config,
        output,
        host,
        crate::common::scratch_in(output_dir.parent().unwrap_or(Path::new("."))),
    )
}

pub(crate) fn write_synthetic_indexed_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode {
                id: 1,
                lat: 557_000_000,
                lon: 125_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 557_000_000,
                lon: 125_010_000,
                tags: vec![],
                meta: None,
            },
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

fn assert_invalid_queries_are_rejected(reader: &pbfhogg::geocode_index::reader::Reader) {
    use pbfhogg::geocode_index::reader::QueryError;

    assert!(matches!(
        reader.query(f64::NAN, 12.0),
        Err(QueryError::NonFiniteLatitude(value)) if value.is_nan()
    ));
    assert!(matches!(
        reader.query(55.0, f64::INFINITY),
        Err(QueryError::NonFiniteLongitude(value)) if value.is_infinite()
    ));
    assert_eq!(
        reader
            .query(90.000_000_1, 100.0)
            .expect_err("invalid latitude"),
        QueryError::LatitudeOutOfRange(90.000_000_1)
    );
    assert_eq!(
        reader
            .candidates(80.0, -180.000_000_1)
            .expect_err("invalid longitude"),
        QueryError::LongitudeOutOfRange(-180.000_000_1)
    );
}

fn write_synthetic_admin_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode {
                id: 10,
                lat: 556_990_000,
                lon: 124_990_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 11,
                lat: 556_990_000,
                lon: 125_020_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 12,
                lat: 557_010_000,
                lon: 125_020_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 13,
                lat: 557_010_000,
                lon: 124_990_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 20,
            refs: vec![10, 11, 12, 13, 10],
            tags: vec![],
            meta: None,
        }],
        &[TestRelation {
            id: 30,
            members: vec![TestMember {
                id: MemberId::Way(20),
                role: "outer",
            }],
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

#[allow(clippy::too_many_lines)]
fn write_synthetic_nested_admin_same_level_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode {
                id: 40,
                lat: 556_980_000,
                lon: 124_980_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 41,
                lat: 556_980_000,
                lon: 125_030_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 42,
                lat: 557_020_000,
                lon: 125_030_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 43,
                lat: 557_020_000,
                lon: 124_980_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 44,
                lat: 556_995_000,
                lon: 124_995_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 45,
                lat: 556_995_000,
                lon: 125_015_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 46,
                lat: 557_005_000,
                lon: 125_015_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 47,
                lat: 557_005_000,
                lon: 124_995_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 50,
                refs: vec![40, 41, 42, 43, 40],
                tags: vec![],
                meta: None,
            },
            TestWay {
                id: 51,
                refs: vec![44, 45, 46, 47, 44],
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestRelation {
                id: 60,
                members: vec![TestMember {
                    id: MemberId::Way(50),
                    role: "outer",
                }],
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
                members: vec![TestMember {
                    id: MemberId::Way(51),
                    role: "outer",
                }],
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
            TestNode {
                id: 70,
                lat: 556_990_000,
                lon: 124_990_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 71,
                lat: 556_990_000,
                lon: 125_020_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 72,
                lat: 557_010_000,
                lon: 125_020_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 73,
                lat: 557_010_000,
                lon: 124_990_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 74,
                lat: 556_997_000,
                lon: 124_997_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 75,
                lat: 556_997_000,
                lon: 125_013_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 76,
                lat: 557_003_000,
                lon: 125_013_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 77,
                lat: 557_003_000,
                lon: 124_997_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 80,
                refs: vec![70, 71, 72, 73, 70],
                tags: vec![],
                meta: None,
            },
            TestWay {
                id: 81,
                refs: vec![74, 75, 76, 77, 74],
                tags: vec![],
                meta: None,
            },
        ],
        &[TestRelation {
            id: 90,
            members: vec![
                TestMember {
                    id: MemberId::Way(80),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Way(81),
                    role: "inner",
                },
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

/// A single admin polygon large enough to span many admin S2 cells, so the
/// builder's pass-3 centroid flood-fill flags genuinely-interior cells (their
/// centers lie strictly inside the polygon). Used to pin the reader-side
/// invariant that the interior high bit is only a hint, never a
/// point-in-polygon bypass: a query just outside the boundary must not be
/// attributed to the polygon via an adjacent interior-flagged cell.
///
/// Axis-aligned square: latitude 54.0..57.0, longitude 10.0..13.0 (each side
/// ~3 degrees, hundreds of km - far wider than one ~8 km level-10 admin cell).
fn write_synthetic_large_admin_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode {
                id: 100,
                lat: 540_000_000,
                lon: 100_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 101,
                lat: 540_000_000,
                lon: 130_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 102,
                lat: 570_000_000,
                lon: 130_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 103,
                lat: 570_000_000,
                lon: 100_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 110,
            refs: vec![100, 101, 102, 103, 100],
            tags: vec![],
            meta: None,
        }],
        &[TestRelation {
            id: 120,
            members: vec![TestMember {
                id: MemberId::Way(110),
                role: "outer",
            }],
            tags: vec![
                ("type", "boundary"),
                ("boundary", "administrative"),
                ("admin_level", "2"),
                ("name", "Bigland"),
                ("ISO3166-1:alpha2", "BL"),
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
            tags: vec![
                ("addr:interpolation", "even"),
                ("addr:street", "Interp Street"),
            ],
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
                tags: vec![
                    ("addr:housenumber", "10"),
                    ("addr:street", "Missing End Street"),
                ],
                meta: None,
            },
            TestNode {
                id: 111,
                lat: 557_000_000,
                lon: 125_060_000,
                tags: vec![],
                meta: None,
            },
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

/// An interpolation way spanning the antimeridian: 179.9E to 179.9W, a
/// segment 0.2 degrees wide whose midpoint is at +/-180, not at 0.
fn write_synthetic_antimeridian_interpolation_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode {
                id: 120,
                lat: 0,
                lon: 1_799_000_000,
                tags: vec![
                    ("addr:housenumber", "10"),
                    ("addr:street", "Dateline Street"),
                ],
                meta: None,
            },
            TestNode {
                id: 121,
                lat: 0,
                lon: -1_799_000_000,
                tags: vec![
                    ("addr:housenumber", "30"),
                    ("addr:street", "Dateline Street"),
                ],
                meta: None,
            },
        ],
        &[TestWay {
            id: 220,
            refs: vec![120, 121],
            tags: vec![
                ("addr:interpolation", "even"),
                ("addr:street", "Dateline Street"),
            ],
            meta: None,
        }],
        &[],
    );
}

/// A near but UNRESOLVABLE interpolation way (node 131 carries no house
/// number, so the way's end number stays 0) plus a farther resolvable one.
/// The near way is inside the fine radius, the far one only inside coarse.
fn write_synthetic_shadowed_interpolation_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            // ~10 m north of the query point: unresolvable.
            TestNode {
                id: 130,
                lat: 557_000_898,
                lon: 125_000_000,
                tags: vec![("addr:housenumber", "10"), ("addr:street", "Near Street")],
                meta: None,
            },
            TestNode {
                id: 131,
                lat: 557_000_898,
                lon: 125_060_000,
                tags: vec![],
                meta: None,
            },
            // ~100 m north of the query point: fully resolvable.
            TestNode {
                id: 132,
                lat: 557_008_983,
                lon: 125_000_000,
                tags: vec![("addr:housenumber", "10"), ("addr:street", "Far Street")],
                meta: None,
            },
            TestNode {
                id: 133,
                lat: 557_008_983,
                lon: 125_060_000,
                tags: vec![("addr:housenumber", "30"), ("addr:street", "Far Street")],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 230,
                refs: vec![130, 131],
                tags: vec![
                    ("addr:interpolation", "all"),
                    ("addr:street", "Near Street"),
                ],
                meta: None,
            },
            TestWay {
                id: 231,
                refs: vec![132, 133],
                tags: vec![
                    ("addr:interpolation", "even"),
                    ("addr:street", "Far Street"),
                ],
                meta: None,
            },
        ],
        &[],
    );
}

fn write_synthetic_postal_boundary_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode {
                id: 300,
                lat: 556_990_000,
                lon: 124_990_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 301,
                lat: 556_990_000,
                lon: 125_020_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 302,
                lat: 557_010_000,
                lon: 125_020_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 303,
                lat: 557_010_000,
                lon: 124_990_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 310,
            refs: vec![300, 301, 302, 303, 300],
            tags: vec![],
            meta: None,
        }],
        &[TestRelation {
            id: 320,
            members: vec![TestMember {
                id: MemberId::Way(310),
                role: "outer",
            }],
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
        TestNode {
            id: 1,
            lat: 557_000_000,
            lon: 125_000_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 557_000_010,
            lon: 125_000_010,
            tags: vec![],
            meta: None,
        },
    ];
    let ways: Vec<TestWay> = (0..65_536u32)
        .map(|i| {
            let name: &'static str = Box::leak(format!("Street{i}").into_boxed_str());
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
            let street: &'static str = Box::leak(format!("Interp{i}").into_boxed_str());
            TestWay {
                id: 1_000 + i64::from(i),
                refs: vec![1, 2],
                tags: vec![("addr:interpolation", "all"), ("addr:street", street)],
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
        TestNode {
            id: 1,
            lat: 557_000_000,
            lon: 125_000_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 557_000_010,
            lon: 125_000_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 3,
            lat: 557_000_010,
            lon: 125_000_010,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 4,
            lat: 557_000_000,
            lon: 125_000_010,
            tags: vec![],
            meta: None,
        },
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
            let name: &'static str = Box::leak(format!("Polygon{i}").into_boxed_str());
            TestRelation {
                id: 1_000 + i64::from(i),
                members: vec![TestMember {
                    id: MemberId::Way(100),
                    role: "outer",
                }],
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
fn synthetic_build_query_matches_low_level_api() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_indexed_input(&input);

    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build should succeed");

    assert_eq!(
        stats.addr_points, 1,
        "synthetic fixture has one address node"
    );
    assert_eq!(
        stats.street_ways, 1,
        "synthetic fixture has one named street way"
    );
    assert_eq!(
        stats.interp_ways, 0,
        "synthetic fixture has no interpolation ways"
    );
    assert_eq!(
        stats.admin_polygons, 0,
        "synthetic fixture has no admin relations"
    );
    assert_eq!(
        stats.smoke_test,
        pbfhogg::geocode_index::builder::SmokeOutcome::Passed
    );

    let reader =
        pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("reader should open");

    assert_invalid_queries_are_rejected(&reader);

    let result = reader.query(55.70005, 12.5005).expect("valid query");
    let addr = result
        .address
        .as_ref()
        .expect("query should find the address");
    assert_eq!(addr.house_number, "10");
    assert_eq!(addr.street, "Main Street");
    assert_eq!(addr.postcode, Some("1234"));
    let street = result
        .street
        .as_ref()
        .expect("query should find the street");
    assert_eq!(street.name, "Main Street");
    assert!(
        result.interpolation.is_none(),
        "fixture has no interpolation data"
    );
    assert!(result.admin.is_empty(), "fixture has no admin polygons");

    let via_candidates = reader
        .candidates(55.70005, 12.5005)
        .expect("valid query")
        .into_result(&reader);
    let cand_addr = via_candidates
        .address
        .as_ref()
        .expect("candidates should find the address");
    assert_eq!(cand_addr.house_number, "10");
    assert_eq!(cand_addr.street, "Main Street");
    assert_eq!(cand_addr.postcode, Some("1234"));
    let cand_street = via_candidates
        .street
        .as_ref()
        .expect("candidates should find the street");
    assert_eq!(cand_street.name, "Main Street");
    assert!(
        via_candidates.interpolation.is_none(),
        "fixture has no interpolation data"
    );
    assert!(
        via_candidates.admin.is_empty(),
        "fixture has no admin polygons"
    );

    let far = reader.query(0.0, 0.0).expect("valid query");
    assert!(
        far.address.is_none(),
        "far-away query must not invent an address"
    );
    assert!(
        far.street.is_none(),
        "far-away query must not invent a street"
    );
    assert!(
        far.interpolation.is_none(),
        "far-away query must not invent interpolation"
    );
    assert!(far.admin.is_empty(), "far-away query must not invent admin");
}

#[test]
fn null_island_street_vertex_is_not_treated_as_absent() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");
    write_test_pbf_sorted(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 0,
                lon: 10_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "residential"), ("name", "Null Island Road")],
            meta: None,
        }],
        &[],
    );
    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("Null Island is a real coordinate");
    assert_eq!(stats.street_ways, 1);
    let reader = pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("open index");
    let result = reader.query(0.0, 0.0).expect("valid query");
    assert_eq!(
        result.street.expect("street at Null Island").name,
        "Null Island Road"
    );
}

/// A node ID far enough above the others to land in a different 4 MB
/// `IdSet` chunk (2^25 IDs each), which is what made Pass 1.5's
/// `set_atomic` panic when its pre-allocation bound understated it.
const HIGH_NODE_ID: i64 = 50_000_000;

fn street_nodes(ids: &[i64]) -> Vec<TestNode> {
    ids.iter()
        .zip(0i32..)
        .map(|(&id, i)| TestNode {
            id,
            lat: 557_000_000,
            lon: 125_000_000 + i * 5_000,
            tags: vec![],
            meta: None,
        })
        .collect()
}

fn street_way(refs: Vec<i64>) -> TestWay {
    TestWay {
        id: 10,
        refs,
        tags: vec![("highway", "residential"), ("name", "Main Street")],
        meta: None,
    }
}

/// Vertices written for all street ways of the index in `index_dir`.
fn street_vertex_count(index_dir: &Path) -> u64 {
    use pbfhogg::geocode_index::format::{FILE_STREET_NODES, NODE_COORD_SIZE};
    let len = std::fs::metadata(index_dir.join(FILE_STREET_NODES))
        .expect("stat street nodes")
        .len();
    len / u64::try_from(NODE_COORD_SIZE).expect("record size fits u64")
}

fn build_into(
    input: std::path::PathBuf,
    index_dir: &Path,
    force: bool,
) -> pbfhogg::geocode_index::builder::BuildStats {
    build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            allow_unindexed: force,
            ..Default::default()
        },
        index_dir,
        force,
        &crate::common::test_host(),
    )
    .expect("build must succeed")
}

/// Indexdata whose node `max_id` understates the blob's real IDs. Pass 1.5
/// sized its referenced-node set from that claim, so a way ref to the
/// node above it used to panic inside the scan's `thread::scope` and take
/// the library down with it. It has to be a reported error naming the
/// metadata, not a panic and not a silently shortened street.
#[test]
fn understated_node_max_id_is_an_error_not_a_panic() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let lying = dir.path().join("lying.osm.pbf");
    write_test_pbf_sorted(
        &honest,
        &street_nodes(&[1, 2, HIGH_NODE_ID]),
        &[street_way(vec![1, 2, HIGH_NODE_ID])],
        &[],
    );

    // Non-regression first: the honest file builds, so the failure below is
    // about the forged claim and not the fixture shape.
    let honest_index = dir.path().join("honest-index");
    let stats = build_into(honest.clone(), &honest_index, false);
    assert_eq!(stats.street_ways, 1);
    assert_eq!(street_vertex_count(&honest_index), 3);

    // Blob 1 is the node blob (blob 0 is the OsmHeader). Claim its last node
    // is 2 while the payload still holds HIGH_NODE_ID.
    let bytes = std::fs::read(&honest).expect("read fixture");
    let forged = crate::common::adversarial::mutate_blob_header_indexdata(&bytes, 1, |ix| {
        assert_eq!(ix.len(), 42, "fixture blob must carry a v3 index");
        ix[10..18].copy_from_slice(&2i64.to_le_bytes());
    });
    std::fs::write(&lying, forged).expect("write forged");

    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: lying,
            ..Default::default()
        },
        &dir.path().join("lying-index"),
        false,
        &crate::common::test_host(),
    )
    .expect_err("an understated node max_id must fail the build");
    let msg = err.to_string();
    assert!(
        msg.contains(&format!("node {HIGH_NODE_ID} exceeds the node-ID bound 2")),
        "error should name the node and the understated bound; got: {msg}"
    );
}

/// A way ref above every node in the file is a dangling ref (a simple
/// extract keeps ways whose nodes it cut away), not an error: the street
/// keeps the vertices it has. This used to panic in Pass 1.5 for the same
/// reason as the understated-indexdata case.
#[test]
fn dangling_ref_above_every_node_id_is_dropped_not_fatal() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");
    write_test_pbf_sorted(
        &input,
        &street_nodes(&[1, 2]),
        &[street_way(vec![1, 2, HIGH_NODE_ID])],
        &[],
    );
    let stats = build_into(input, &index_dir, false);
    assert_eq!(stats.street_ways, 1);
    assert_eq!(
        street_vertex_count(&index_dir),
        2,
        "the street keeps exactly its two present nodes"
    );
}

/// `--force` on input without indexdata: there is no `max_id` to read, so
/// the builder must learn the node-ID range from the blobs themselves. It
/// used to take 0 as the bound and panic on the first ref past the first
/// `IdSet` chunk.
#[test]
fn force_without_indexdata_bounds_node_ids_from_the_blobs() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");
    crate::common::write_test_pbf_non_indexed(
        &input,
        &street_nodes(&[1, 2, HIGH_NODE_ID]),
        &[street_way(vec![1, 2, HIGH_NODE_ID])],
        &[],
    );
    crate::common::assert_non_indexed(&input);
    let stats = build_into(input, &index_dir, true);
    assert_eq!(stats.street_ways, 1);
    assert_eq!(
        street_vertex_count(&index_dir),
        3,
        "HIGH_NODE_ID's coordinate must be resolved, not dropped"
    );
}

// BlockBuilder emits dense nodes only. Exercise plain nodes with osmium:
// brokkr test legal_non_dense_nodes_build_a_queryable_index --sweep all
#[test]
#[ignore = "external"]
fn legal_non_dense_nodes_build_a_queryable_index() {
    let dir = crate::common::test_scratch();
    let dense = dir.path().join("dense.osm.pbf");
    let input = dir.path().join("non-dense.osm.pbf");
    let index_dir = dir.path().join("index");
    write_synthetic_indexed_input(&dense);
    let status = std::process::Command::new("osmium")
        .args(["cat", "-f", "pbf,pbf_dense_nodes=false", "-o"])
        .arg(&input)
        .arg(&dense)
        .status();
    let status = match status {
        Ok(status) => status,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::common::skip("osmium not found in PATH");
            return;
        }
        Err(error) => panic!("run osmium to produce a legal non-dense fixture: {error}"),
    };
    assert!(status.success(), "osmium fixture conversion failed");
    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            allow_unindexed: true,
            ..Default::default()
        },
        &index_dir,
        true,
        &crate::common::test_host(),
    )
    .expect("legal non-dense Node messages must be indexed");
    assert_eq!(stats.addr_points, 1);
    assert_eq!(stats.street_ways, 1);
    let reader = pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("open index");
    let result = reader.query(55.70005, 12.5005).expect("valid query");
    assert!(
        result.address.is_some(),
        "non-dense address node must be queryable"
    );
    assert!(
        result.street.is_some(),
        "way coordinates from non-dense nodes must be queryable"
    );
}

#[test]
fn builder_refuses_a_truly_empty_index() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");
    write_test_pbf_sorted(
        &input,
        &[TestNode {
            id: 1,
            lat: 557_000_000,
            lon: 125_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect_err("a build with no queryable records must fail");
    assert!(
        err.to_string()
            .contains("refusing to publish an empty index"),
        "unexpected error: {err}"
    );
}

#[test]
fn reader_rejects_hostile_entry_offset_instead_of_panicking() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");
    write_synthetic_indexed_input(&input);
    build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build index");
    let cells_path = index_dir.join("geo_cells.bin");
    let mut cells = std::fs::read(&cells_path).expect("read cells");
    cells[16..24].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
    std::fs::write(&cells_path, cells).expect("corrupt offset");
    let err = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .err()
        .expect("hostile offset must be a typed open error");
    assert!(
        err.to_string().contains("address entry offset"),
        "unexpected error: {err}"
    );
}

/// Reader open-time validation must cover every offset-into-a-file
/// relationship in the format, not just the geo entry lists. Validating some
/// of the kinds and not the others is how a truncated file opens successfully
/// and then silently answers with incomplete results: every read path is
/// individually bounds-safe and returns "nothing here" rather than an error,
/// so the caller cannot tell a truncated list from an empty one.
#[test]
fn reader_rejects_every_out_of_bounds_offset_kind() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let good = dir.path().join("good");
    write_all_record_kinds_input(&input);
    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &good,
        false,
        &crate::common::test_host(),
    )
    .expect("build index");
    assert!(
        stats.addr_points > 0
            && stats.street_ways > 0
            && stats.interp_ways > 0
            && stats.admin_polygons > 0,
        "fixture must populate every record kind, got {stats:?}"
    );
    pbfhogg::geocode_index::reader::Reader::open(&good)
        .expect("the pristine index must open, or every case below is vacuous");

    // (file to corrupt, byte range holding the offset, substring of the
    // expected error). One case per offset kind in the format.
    let cases: &[(&str, std::ops::Range<usize>, &str)] = &[
        ("admin_cells.bin", 8..16, "admin entry"),
        ("street_ways.bin", 0..8, "street way node"),
        ("interp_ways.bin", 0..8, "interpolation way node"),
        ("admin_polygons.bin", 4..12, "admin polygon vertex"),
    ];
    for (file, range, expected) in cases {
        let index_dir = dir.path().join(format!("case-{file}"));
        copy_dir(&good, &index_dir);
        let path = index_dir.join(file);
        let mut bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {file}: {e}"));
        assert!(
            bytes.len() >= range.end,
            "{file} is empty in the fixture, so this case proves nothing"
        );
        let poison = vec![0xFEu8; range.len()];
        bytes[range.clone()].copy_from_slice(&poison);
        std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {file}: {e}"));

        let err = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
            .err()
            .unwrap_or_else(|| panic!("{file}: out-of-bounds offset must fail open, not open"));
        assert!(
            err.to_string().contains(expected),
            "{file}: expected an error mentioning {expected}, got: {err}"
        );
    }
}

/// One input that populates every record kind at once: address points,
/// street ways, interpolation ways and admin polygons. The offset-validation
/// test needs all four non-empty, or its cases are vacuous.
fn write_all_record_kinds_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &all_record_kinds_nodes(),
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![("highway", "residential"), ("name", "Main Street")],
                meta: None,
            },
            TestWay {
                id: 20,
                refs: vec![10, 11, 12, 13, 10],
                tags: vec![],
                meta: None,
            },
            TestWay {
                id: 200,
                refs: vec![100, 101],
                tags: vec![
                    ("addr:interpolation", "even"),
                    ("addr:street", "Interp Street"),
                ],
                meta: None,
            },
        ],
        &[TestRelation {
            id: 30,
            members: vec![TestMember {
                id: MemberId::Way(20),
                role: "outer",
            }],
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

fn all_record_kinds_nodes() -> Vec<TestNode> {
    vec![
        TestNode {
            id: 1,
            lat: 557_000_000,
            lon: 125_000_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 557_000_000,
            lon: 125_010_000,
            tags: vec![],
            meta: None,
        },
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
        TestNode {
            id: 10,
            lat: 556_990_000,
            lon: 124_990_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 11,
            lat: 556_990_000,
            lon: 125_020_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 12,
            lat: 557_010_000,
            lon: 125_020_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 13,
            lat: 557_010_000,
            lon: 124_990_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 100,
            lat: 557_002_000,
            lon: 125_000_000,
            tags: vec![("addr:housenumber", "10"), ("addr:street", "Interp Street")],
            meta: None,
        },
        TestNode {
            id: 101,
            lat: 557_002_000,
            lon: 125_006_000,
            tags: vec![("addr:housenumber", "30"), ("addr:street", "Interp Street")],
            meta: None,
        },
    ]
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("dir entry");
        if entry.file_type().expect("file type").is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name())).expect("copy");
        }
    }
}

#[test]
fn force_with_bad_input_leaves_the_existing_index_intact() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");
    write_synthetic_indexed_input(&input);
    build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("initial build");
    let before = std::fs::read(index_dir.join("geocode_header.bin")).expect("header exists");

    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: dir.path().join("missing.pbf"),
            allow_unindexed: true,
            ..Default::default()
        },
        &index_dir,
        true,
        &crate::common::test_host(),
    )
    .expect_err("a forced rebuild from a nonexistent input must fail");

    let after = std::fs::read(index_dir.join("geocode_header.bin")).unwrap_or_else(|e| {
        panic!(
            "a failed force rebuild must not remove the existing header ({e}); build error: {err}"
        )
    });
    assert_eq!(
        before, after,
        "a failed force rebuild must leave the header byte-identical"
    );
    let reader = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .expect("the surviving index must still open and answer queries");
    assert!(
        reader
            .query(55.70005, 12.5005)
            .expect("valid query")
            .address
            .is_some(),
        "the surviving index must still resolve what it resolved before"
    );
}

/// A failed build after input validation preserves the entire old generation.
#[test]
fn force_preserves_the_index_when_pass_two_fails() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");
    write_synthetic_indexed_input(&input);
    build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input.clone(),
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("initial build");

    // A second input that passes every read-only check but yields nothing
    // indexable, so the staged build fails after Pass 2.
    let empty = dir.path().join("empty.osm.pbf");
    write_test_pbf_sorted(
        &empty,
        &[TestNode {
            id: 1,
            lat: 557_000_000,
            lon: 125_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: empty,
            allow_unindexed: true,
            ..Default::default()
        },
        &index_dir,
        true,
        &crate::common::test_host(),
    )
    .expect_err("a build with no queryable records must fail");
    assert!(
        index_dir.join("geocode_header.bin").exists(),
        "failed staged build must retain the old header: {err}"
    );
    let reader = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .expect("the original generation must remain readable");
    assert!(
        reader
            .query(55.70005, 12.5005)
            .expect("query")
            .address
            .is_some()
    );
}

/// Overwrite permission (on the output) and `allow_unindexed` (on the
/// config) grant one permission each; the CLI's legacy `--force` grants
/// both. Each refusal must leave the existing index byte-identical.
#[test]
fn overwrite_and_allow_unindexed_are_independent_permissions() {
    let dir = crate::common::test_scratch();
    let indexed = dir.path().join("indexed.osm.pbf");
    let unindexed = dir.path().join("unindexed.osm.pbf");
    let index_dir = dir.path().join("index");
    write_synthetic_indexed_input(&indexed);
    crate::common::write_test_pbf_non_indexed(
        &unindexed,
        &street_nodes(&[1, 2, 3]),
        &[street_way(vec![1, 2, 3])],
        &[],
    );
    crate::common::assert_non_indexed(&unindexed);
    let build = |overwrite: bool, allow_unindexed: bool| {
        build_index(
            &pbfhogg::geocode_index::builder::BuildConfig {
                input_path: unindexed.clone(),
                allow_unindexed,
                ..Default::default()
            },
            &index_dir,
            overwrite,
            &crate::common::test_host(),
        )
    };
    build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: indexed,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("initial build");
    let before = std::fs::read(index_dir.join("geocode_header.bin")).expect("header exists");

    let err = build(true, false).expect_err("overwrite alone must not waive indexdata");
    assert!(
        err.to_string().contains("no blob-level indexdata"),
        "unexpected error: {err}"
    );
    let err = build(false, true).expect_err("allow_unindexed alone must not overwrite");
    assert!(
        matches!(
            err.kind(),
            pbfhogg::ErrorKind::Output(pbfhogg::OutputError::Exists { .. })
        ),
        "unexpected error: {err}"
    );
    assert_eq!(
        std::fs::read(index_dir.join("geocode_header.bin")).expect("header survives"),
        before,
        "refused builds must leave the existing index untouched"
    );

    let stats = build(true, true).expect("both permissions together must rebuild");
    assert_eq!(stats.street_ways, 1);
}

/// Publication renames the output path itself, so a symlinked output
/// directory would be replaced by a real directory. It is refused before any
/// input is read, and the link and its target are left alone.
#[cfg(unix)]
#[test]
fn symlinked_output_dir_is_refused_before_the_build() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let target = dir.path().join("real-index");
    let link = dir.path().join("index-link");
    write_synthetic_indexed_input(&input);
    std::fs::create_dir(&target).expect("create link target");
    std::os::unix::fs::symlink(&target, &link).expect("create symlink");
    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            allow_unindexed: true,
            ..Default::default()
        },
        &link,
        true,
        &crate::common::test_host(),
    )
    .expect_err("a symlinked output directory must be refused");
    assert!(
        err.to_string().contains("is a symlink"),
        "unexpected error: {err}"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link survives")
            .file_type()
            .is_symlink(),
        "the link must not be replaced"
    );
    assert!(
        std::fs::read_dir(&target)
            .expect("target survives")
            .next()
            .is_none(),
        "nothing may be written through the link"
    );
}

/// The segment sampling budget is enforced at config validation, before any
/// input is read, because that is the only place the work can be bounded
/// without ever discarding coverage. This asserts the bound is enforced, not
/// that a typical segment happens to be cheap.
#[test]
fn levels_past_the_sampling_budget_are_refused_before_reading_input() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");
    write_synthetic_indexed_input(&input);
    let cases: [(&str, u8, u8); 2] = [("street", 20, 14), ("coarse", 21, 20)];
    for (name, street, coarse) in cases {
        let err = build_index(
            &pbfhogg::geocode_index::builder::BuildConfig {
                input_path: input.clone(),
                allow_unindexed: true,
                street_level: street,
                coarse_level: coarse,
                ..Default::default()
            },
            &index_dir,
            true,
            &crate::common::test_host(),
        )
        .expect_err("a level past the sampling budget must be refused");
        assert!(
            err.to_string().contains("S2 level must be in 0..=19"),
            "{name}: unexpected error: {err}"
        );
        assert!(
            !index_dir.exists(),
            "{name}: rejection must happen before any output directory is created"
        );
    }

    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input.clone(),
            allow_unindexed: true,
            admin_level: 11,
            ..Default::default()
        },
        &index_dir,
        true,
        &crate::common::test_host(),
    )
    .expect_err("an admin level over the flood-fill budget must be refused");
    assert!(
        err.to_string().contains("admin S2 level must be in 0..=10"),
        "unexpected admin level error: {err}"
    );
    assert!(
        !index_dir.exists(),
        "admin level rejection must happen before any output directory is created"
    );

    // Level 19 remains accepted for the segment-sampled street index while
    // admin indexing uses its separate, lower flood-fill budget.
    build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            allow_unindexed: true,
            street_level: 19,
            coarse_level: 14,
            admin_level: 10,
            ..Default::default()
        },
        &index_dir,
        true,
        &crate::common::test_host(),
    )
    .expect("the highest accepted level must build");
}

#[test]
fn fallback_recovers_hits_outside_fine_radius() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let no_fallback_dir = dir.path().join("index_no_fallback");
    let coarse_fallback_dir = dir.path().join("index_with_fallback");

    write_synthetic_indexed_input(&input);

    for (output_dir, coarse_search_radius_m) in [
        (&no_fallback_dir, 1.0_f32),
        (&coarse_fallback_dir, 100.0_f32),
    ] {
        build_index(
            &pbfhogg::geocode_index::builder::BuildConfig {
                input_path: input.clone(),
                fine_search_radius_m: 1.0,
                coarse_search_radius_m,
                ..Default::default()
            },
            output_dir,
            false,
            &crate::common::test_host(),
        )
        .expect("build should succeed");
    }

    let query_lat = 55.7002;
    let query_lon = 12.5005;

    let no_fallback =
        pbfhogg::geocode_index::reader::Reader::open(&no_fallback_dir).expect("reader should open");
    let no_fallback_result = no_fallback
        .query(query_lat, query_lon)
        .expect("valid query");
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
    let with_fallback_result = with_fallback
        .query(query_lat, query_lon)
        .expect("valid query");
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
fn fallback_fills_address_when_fine_street_exists() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");
    write_synthetic_indexed_input(&input);
    build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            fine_search_radius_m: 2.0,
            coarse_search_radius_m: 100.0,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build should succeed");

    let reader =
        pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("reader should open");
    let result = reader.query(55.7, 12.5005).expect("valid query");
    assert!(result.street.is_some(), "fine search must find the street");
    assert_eq!(
        result.address.as_ref().map(|address| address.postcode),
        Some(Some("1234")),
        "coarse search must independently fill the missing address and postcode"
    );

    let candidates = reader.candidates(55.7, 12.5005).expect("valid query");
    assert!(
        !candidates.streets.is_empty(),
        "fine candidates keep the street"
    );
    assert!(
        !candidates.addresses.is_empty(),
        "coarse fill must add the address"
    );
    let ranked = candidates.into_result(&reader);
    assert_eq!(
        ranked.address.and_then(|address| address.postcode),
        Some("1234")
    );
}

#[test]
fn synthetic_admin_polygon_query_returns_country_match() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_admin_input(&input);

    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build should succeed");

    assert_eq!(
        stats.addr_points, 0,
        "admin-only fixture has no address points"
    );
    assert_eq!(
        stats.smoke_test,
        pbfhogg::geocode_index::builder::SmokeOutcome::SkippedNoAddressPoints
    );
    assert_eq!(
        stats.street_ways, 0,
        "admin-only fixture has no street ways"
    );
    assert_eq!(stats.admin_polygons, 1, "fixture has one admin boundary");

    let reader =
        pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("reader should open");

    let inside = reader.query(55.7000, 12.5005).expect("valid query");
    assert!(
        inside.address.is_none(),
        "admin-only fixture has no addresses"
    );
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

    let outside = reader.query(55.7020, 12.5005).expect("valid query");
    assert!(
        outside.admin.is_empty(),
        "query outside the polygon must not match the country boundary"
    );
}

#[test]
fn nested_same_level_admin_prefers_smallest_polygon() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_nested_admin_same_level_input(&input);

    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build should succeed");

    assert_eq!(
        stats.admin_polygons, 2,
        "fixture has two admin polygons at the same level"
    );

    let reader =
        pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("reader should open");

    let query_lat = 55.7000;
    let query_lon = 12.5005;

    let raw_candidates = reader
        .candidates(query_lat, query_lon)
        .expect("valid query");
    let raw_level8: Vec<_> = raw_candidates
        .admin
        .iter()
        .filter(|admin| admin.admin_level == 8)
        .map(|admin| admin.name)
        .collect();
    assert_eq!(
        raw_level8.len(),
        2,
        "raw candidates should expose both containing polygons"
    );
    assert!(raw_level8.contains(&"Bigshire"));
    assert!(raw_level8.contains(&"Smallshire"));

    let collapsed = reader.query(query_lat, query_lon).expect("valid query");
    let collapsed_level8: Vec<_> = collapsed
        .admin
        .iter()
        .filter(|admin| admin.admin_level == 8)
        .collect();
    assert_eq!(
        collapsed_level8.len(),
        1,
        "query() should collapse same-level admin matches"
    );
    assert_eq!(collapsed_level8[0].name, "Smallshire");

    let via_into_result = reader
        .candidates(query_lat, query_lon)
        .expect("valid query")
        .into_result(&reader);
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_admin_with_hole_input(&input);

    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build should succeed");

    assert_eq!(
        stats.admin_polygons, 1,
        "fixture has one admin polygon with one hole"
    );

    let reader =
        pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("reader should open");

    let shell = reader.query(55.6992, 12.4992).expect("valid query");
    let shell_admin = shell
        .admin
        .iter()
        .find(|admin| admin.admin_level == 6)
        .expect("query inside the shell should match the admin polygon");
    assert_eq!(shell_admin.name, "Holeland");

    let hole = reader.query(55.7000, 12.5005).expect("valid query");
    assert!(
        hole.admin.iter().all(|admin| admin.admin_level != 6),
        "query inside the inner ring must not match the polygon"
    );

    let raw_hole = reader.candidates(55.7000, 12.5005).expect("valid query");
    assert!(
        raw_hole.admin.iter().all(|admin| admin.admin_level != 6),
        "candidates() must also respect admin holes"
    );
}

/// Walk admin_cells.bin + admin_entries.bin and map every admin S2 cell id to
/// whether it carries an entry with the interior high bit set. On-disk layout
/// (see `src/geocode_index/format.rs`): admin_cells.bin is fixed 16-byte
/// records (cell_id u64, entries_offset u64); admin_entries.bin holds, at
/// each cell's entries_offset, a u16 count followed by that many u32 entries
/// whose top bit (`INTERIOR_FLAG`) marks an interior hint. Keyed by cell id
/// (rather than a single yes/anywhere bool) so callers can ask "is *this*
/// cell interior-flagged" instead of "is some cell somewhere interior
/// flagged" - the latter proves nothing about any particular query.
fn admin_cell_interior_flags(index_dir: &Path) -> std::collections::HashMap<u64, bool> {
    use pbfhogg::geocode_index::format::{ADMIN_CELL_SIZE, FILE_ADMIN_CELLS, FILE_ADMIN_ENTRIES};

    const INTERIOR_FLAG: u32 = 0x8000_0000;

    let cells = std::fs::read(index_dir.join(FILE_ADMIN_CELLS)).expect("read admin_cells.bin");
    let entries =
        std::fs::read(index_dir.join(FILE_ADMIN_ENTRIES)).expect("read admin_entries.bin");

    let cell_count = cells.len() / ADMIN_CELL_SIZE;
    let mut flags = std::collections::HashMap::with_capacity(cell_count);
    for i in 0..cell_count {
        let base = i * ADMIN_CELL_SIZE;
        let cell_id = u64::from_le_bytes(cells[base..base + 8].try_into().expect("8 bytes"));
        let entries_offset = usize::try_from(u64::from_le_bytes(
            cells[base + 8..base + 16].try_into().expect("8 bytes"),
        ))
        .expect("test platform must address the entries file");

        let mut has_interior = false;
        if entries_offset + 2 <= entries.len() {
            let count = u16::from_le_bytes([entries[entries_offset], entries[entries_offset + 1]]);
            let mut pos = entries_offset + 2;
            for _ in 0..count {
                if pos + 4 > entries.len() {
                    break;
                }
                let raw = u32::from_le_bytes([
                    entries[pos],
                    entries[pos + 1],
                    entries[pos + 2],
                    entries[pos + 3],
                ]);
                if raw & INTERIOR_FLAG != 0 {
                    has_interior = true;
                }
                pos += 4;
            }
        }
        flags.insert(cell_id, has_interior);
    }
    flags
}

/// Returns a query point's own admin S2 cell id and its neighbor cell ids at
/// `level`, mirroring the reader's private `cell_neighborhood` helper (the
/// point's cell plus up to 8 S2 neighbors) so a test can reason about
/// exactly the same per-query cell set the reader's admin search visits.
fn admin_cell_neighborhood(lat: f64, lon: f64, level: u8) -> (u64, Vec<u64>) {
    let cell =
        s2::cellid::CellID::from(s2::latlng::LatLng::from_degrees(lat, lon)).parent(level as u64);
    let neighbors = cell
        .all_neighbors(level as u64)
        .iter()
        .map(|n| n.0)
        .collect();
    (cell.0, neighbors)
}

/// Regression: the admin interior high bit is a hint only, never a
/// point-in-polygon bypass. Before the fix at both reader call sites
/// (`search_admin_ranked` / `search_admin_all`), `is_interior || contains(...)`
/// accepted a candidate whenever ANY cell in the query's 9-cell neighborhood
/// carried an interior-flagged entry for the polygon - even if the query point
/// itself lay outside the polygon. pbfhogg's builder does not make that skip
/// sound (no interior erosion, cell-center sampling), so a point just outside
/// the boundary but adjacent to a genuinely-interior cell was wrongly attributed
/// to the polygon. The fix always runs the PIP test; this pins that behavior.
#[test]
fn admin_interior_hint_does_not_bypass_point_in_polygon() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_large_admin_input(&input);

    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build should succeed");
    assert_eq!(stats.admin_polygons, 1, "fixture has one admin boundary");

    let reader =
        pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("reader should open");
    let admin_level = reader.admin_cell_level();
    // Fixture has exactly one admin polygon (asserted above), so poly_index 0
    // is always Bigland - any interior-flagged entry found below belongs to it.
    let interior_flags = admin_cell_interior_flags(&index_dir);

    // Sanity: a point well inside the polygon resolves to the boundary.
    let inside = reader.query(55.5, 11.5).expect("valid query");
    let inside_admin = inside
        .admin
        .iter()
        .find(|admin| admin.admin_level == 2)
        .expect("query inside the polygon should return the boundary");
    assert_eq!(inside_admin.name, "Bigland");

    // Regression: several points just NORTH of the top edge (latitude 57.0).
    // Every point is unambiguously outside (latitude > 57.0), yet each sits
    // within one admin-cell width of the boundary, so its 9-cell neighborhood
    // may include interior-flagged cells one step inside. Pre-fix, the
    // interior bypass attributed these to "Bigland"; post-fix, the
    // unconditional PIP test rejects them. Both the ranked query() and the
    // raw candidates() paths must reject.
    //
    // Non-vacuity guard, made query-local (not "some cell somewhere is
    // interior-flagged", which a future fixture/builder change could satisfy
    // without any tested query having an interior-flagged neighbor): for
    // every tested point, assert its OWN admin cell carries no interior flag
    // - a flagged own-cell here would mean a *different* bug (a
    // false-interior cell, i.e. the builder flagging a cell whose center
    // reads outside the polygon), not the neighbor-cell bypass this test
    // targets. Separately, track whether at least one tested point's
    // 8-neighbor S2 neighborhood contains an interior-flagged cell; that is
    // what actually exercises the neighbor-cell bypass the fix removed, and
    // is asserted after the loop so the test fails loudly (not silently
    // vacuously) if no tested query ever reaches an interior-flagged
    // neighbor.
    let mut exercised_neighbor_bypass = false;
    for lon in [10.5_f64, 11.0, 11.5, 12.0, 12.5] {
        let outside_lat = 57.02;

        let (own_cell, neighbors) = admin_cell_neighborhood(outside_lat, lon, admin_level);
        let own_flagged = interior_flags.get(&own_cell).copied().unwrap_or(false);
        assert!(
            !own_flagged,
            "query ({outside_lat}, {lon}) is outside Bigland, but its own admin \
             cell carries the interior flag - that is a false-interior-cell bug \
             in the builder, distinct from the neighbor-cell bypass this test \
             targets"
        );
        if neighbors
            .iter()
            .any(|cell_id| interior_flags.get(cell_id).copied().unwrap_or(false))
        {
            exercised_neighbor_bypass = true;
        }

        let ranked = reader.query(outside_lat, lon).expect("valid query");
        assert!(
            ranked.admin.iter().all(|admin| admin.name != "Bigland"),
            "query() at ({outside_lat}, {lon}) is outside the polygon and must \
             not be attributed to Bigland via an interior-flagged neighbor cell"
        );
        #[cfg(feature = "test-hooks")]
        reader.reset_admin_pip_calls();
        let raw = reader.candidates(outside_lat, lon).expect("valid query");
        #[cfg(feature = "test-hooks")]
        assert!(
            reader.admin_pip_calls() <= 1,
            "one polygon shared by neighboring cells must be containment-tested at most once"
        );
        assert!(
            raw.admin.iter().all(|admin| admin.name != "Bigland"),
            "candidates() at ({outside_lat}, {lon}) is outside the polygon and \
             must not be attributed to Bigland via an interior-flagged neighbor cell"
        );
    }
    assert!(
        exercised_neighbor_bypass,
        "none of the tested outside queries had an interior-flagged neighbor \
         cell for Bigland (with their own cell unflagged); the fixture/level \
         no longer exercises the interior-hint neighbor-cell bypass this test \
         guards against, so the assertions above would pass vacuously"
    );
}

#[test]
fn synthetic_interpolation_query_resolves_even_house_number() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_interpolation_input(&input);

    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            coarse_search_radius_m: 1.0,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build should succeed");

    assert_eq!(
        stats.addr_points, 2,
        "fixture has two endpoint address nodes"
    );
    assert_eq!(stats.street_ways, 0, "fixture has no named street way");
    assert_eq!(stats.interp_ways, 1, "fixture has one interpolation way");

    let reader =
        pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("reader should open");

    let query_lat = 55.7000;
    let query_lon = 12.5030;
    let result = reader.query(query_lat, query_lon).expect("valid query");
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
    assert_eq!(
        interp.house_number, 20,
        "midpoint on the interpolation should resolve to 20"
    );
    assert!(result.admin.is_empty(), "fixture has no admin polygons");

    let candidates = reader
        .candidates(query_lat, query_lon)
        .expect("valid query");
    assert!(
        !candidates.interpolations.is_empty(),
        "candidates should expose the raw interpolation hit"
    );
    let via_candidates = reader
        .candidates(query_lat, query_lon)
        .expect("valid query")
        .into_result(&reader);
    let cand_interp = via_candidates
        .interpolation
        .as_ref()
        .expect("candidates().into_result() should resolve the interpolation");
    assert_eq!(cand_interp.street, "Interp Street");
    assert_eq!(cand_interp.house_number, 20);
}

#[test]
fn unresolved_interpolation_stays_hidden_behind_zero_sentinel() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_unresolved_interpolation_input(&input);

    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            coarse_search_radius_m: 1.0,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build should succeed");

    assert_eq!(
        stats.addr_points, 1,
        "fixture has only one matching endpoint address"
    );
    assert_eq!(
        stats.interp_ways, 1,
        "fixture still writes one interpolation way"
    );

    let reader =
        pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("reader should open");

    let query_lat = 55.7000;
    let query_lon = 12.5030;
    let result = reader.query(query_lat, query_lon).expect("valid query");
    assert!(
        result.address.is_none(),
        "midpoint query should not hit the lone endpoint address"
    );
    assert!(
        result.interpolation.is_none(),
        "unresolved endpoints stay at 0/0 and must not surface as an interpolation result"
    );

    // candidates() must NOT expose a segment it cannot resolve. Its emptiness
    // is the coarse-fallback gate, so a candidate that yields no house number
    // would silently suppress a coarse hit that would have. This assertion was
    // previously the other way round - it pinned exactly that bug as correct.
    let candidates = reader
        .candidates(query_lat, query_lon)
        .expect("valid query");
    assert!(
        candidates.interpolations.is_empty(),
        "a 0/0 interpolation way is unresolvable and must not be offered as a candidate"
    );
    assert!(
        reader
            .candidates(query_lat, query_lon)
            .expect("valid query")
            .into_result(&reader)
            .interpolation
            .is_none(),
        "candidates().into_result() should also hide unresolved interpolation"
    );
}

#[test]
fn fallback_recovers_interpolation_outside_fine_radius() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let no_fallback_dir = dir.path().join("index_no_fallback");
    let coarse_fallback_dir = dir.path().join("index_with_fallback");

    write_synthetic_interpolation_input(&input);

    for (output_dir, coarse_search_radius_m) in [
        (&no_fallback_dir, 1.0_f32),
        (&coarse_fallback_dir, 100.0_f32),
    ] {
        build_index(
            &pbfhogg::geocode_index::builder::BuildConfig {
                input_path: input.clone(),
                fine_search_radius_m: 1.0,
                coarse_search_radius_m,
                ..Default::default()
            },
            output_dir,
            false,
            &crate::common::test_host(),
        )
        .expect("build should succeed");
    }

    let query_lat = 55.7002;
    let query_lon = 12.5030;

    let no_fallback =
        pbfhogg::geocode_index::reader::Reader::open(&no_fallback_dir).expect("reader should open");
    let no_fallback_result = no_fallback
        .query(query_lat, query_lon)
        .expect("valid query");
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
    let with_fallback_result = with_fallback
        .query(query_lat, query_lon)
        .expect("valid query");
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

    let candidates = with_fallback
        .candidates(query_lat, query_lon)
        .expect("valid query");
    assert!(
        !candidates.interpolations.is_empty(),
        "coarse candidates should expose the raw interpolation hit"
    );
}

/// `point_to_segment_distance_sq` computes `t` in a longitude frame unwrapped
/// around the segment start, so the snap point has to be interpolated in that
/// same frame. Interpolating the raw E7 values instead put the midpoint of a
/// 179.9E -> 179.9W segment at longitude 0 rather than 180 - a snap point on
/// the far side of the planet, which the interpolation path then fed into
/// `accumulated_length_radians` as a ~180 degree partial length against a 0.2
/// degree total. The assertion that bites is therefore the HOUSE NUMBER, not
/// the snap coordinate: a snap-point check alone would not show how wrong the
/// user-visible answer got.
#[test]
fn antimeridian_interpolation_snaps_on_the_short_arc() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_antimeridian_interpolation_input(&input);

    build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build should succeed");

    let reader =
        pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("reader should open");

    // Exactly on the segment, at its midpoint.
    let result = reader.query(0.0, 180.0).expect("valid query");
    let interp = result
        .interpolation
        .as_ref()
        .expect("the query point lies on the interpolation segment");
    assert_eq!(interp.street, "Dateline Street");
    assert_eq!(
        interp.house_number, 20,
        "midpoint of a 10..30 even interpolation is 20; a long-arc snap point \
         drives the accumulated length far past the way length and produces a \
         wildly wrong number"
    );

    // The snap point itself must also stay on the short arc. +/-180 are the
    // same meridian, so accept either sign, but nothing near 0.
    let candidate = reader
        .candidates(0.0, 180.0)
        .expect("valid query")
        .interpolations
        .into_iter()
        .next()
        .expect("segment should be a raw candidate too");
    let snap_lon_deg = f64::from(candidate.snap_lon_e7) * 1e-7;
    assert!(
        snap_lon_deg.abs() > 179.0,
        "snap longitude {snap_lon_deg} should sit on the antimeridian, not at 0"
    );
}

/// Coarse fallback eligibility must depend on whether a fine hit RESOLVES,
/// not on whether a fine segment exists. A nearby interpolation way whose
/// endpoint numbers are unusable used to both produce no house number and
/// suppress the coarse interpolation that would have produced one.
#[test]
fn unresolvable_fine_interpolation_does_not_shadow_the_fallback() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_shadowed_interpolation_input(&input);

    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            fine_search_radius_m: 30.0,
            coarse_search_radius_m: 300.0,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build should succeed");
    assert_eq!(
        stats.interp_ways, 2,
        "both the unresolvable near way and the resolvable far way are indexed"
    );

    let reader =
        pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("reader should open");

    let (lat, lon) = (55.7, 12.503);
    let result = reader.query(lat, lon).expect("valid query");
    let interp = result
        .interpolation
        .as_ref()
        .expect("the unresolvable near way must not suppress the coarse fallback");
    assert_eq!(interp.street, "Far Street");
    assert_eq!(interp.house_number, 20);

    // candidates() gates its fallback the same way, and into_result() ranks
    // what candidates() collected - all three surfaces must agree.
    let candidates = reader.candidates(lat, lon).expect("valid query");
    assert!(
        candidates
            .interpolations
            .iter()
            .all(|c| reader.interpolate(c).is_some()),
        "candidates() must not emit an interpolation it cannot resolve, or its \
         emptiness gate means something different from query()'s"
    );
    assert!(
        candidates
            .interpolations
            .iter()
            .any(|c| c.street == "Far Street"),
        "coarse fallback should have run for candidates() too"
    );
    let ranked = reader
        .candidates(lat, lon)
        .expect("valid query")
        .into_result(&reader);
    let ranked_interp = ranked
        .interpolation
        .as_ref()
        .expect("into_result() must agree with query()");
    assert_eq!(ranked_interp.street, interp.street);
    assert_eq!(ranked_interp.house_number, interp.house_number);
}

#[test]
fn synthetic_postal_boundary_query_returns_level_11_match() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_synthetic_postal_boundary_input(&input);

    let stats = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build should succeed");

    assert_eq!(
        stats.admin_polygons, 1,
        "fixture has one postal boundary polygon"
    );

    let reader =
        pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("reader should open");

    let inside = reader.query(55.7000, 12.5005).expect("valid query");
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

    let outside = reader.query(55.7020, 12.5005).expect("valid query");
    assert!(
        outside.admin.iter().all(|admin| admin.admin_level != 11),
        "query outside the polygon must not match the postal boundary"
    );
}

#[test]
fn build_rejects_street_way_coord_count_over_u16_max() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_oversized_street_input(&input);

    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_oversized_interpolation_input(&input);

    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_addr_cell_overflow_input(&input);

    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_street_cell_overflow_input(&input);

    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_interp_cell_overflow_input(&input);

    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");

    write_admin_cell_overflow_input(&input);

    let err = build_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
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

/// A node still present in the shared node file must not extend a way whose
/// declared geometry ends before it. Exercise both fine/coarse query consumers.
#[test]
fn segment_distance_respects_the_owning_way_node_count() {
    use pbfhogg::geocode_index::{builder, reader::Reader};

    for interpolation in [false, true] {
        let dir = crate::common::test_scratch();
        let input = dir.path().join("input.osm.pbf");
        let index_dir = dir.path().join("index");
        if interpolation {
            write_synthetic_interpolation_input(&input);
        } else {
            write_synthetic_indexed_input(&input);
        }
        build_index(
            &builder::BuildConfig {
                input_path: input,
                ..Default::default()
            },
            &index_dir,
            false,
            &crate::common::test_host(),
        )
        .expect("build fixture");
        let lon = if interpolation { 12.5030 } else { 12.5005 };
        let reader = Reader::open(&index_dir).expect("open baseline");
        let baseline = reader.candidates(55.7, lon).expect("valid query");
        assert!(if interpolation {
            !baseline.interpolations.is_empty()
        } else {
            !baseline.streets.is_empty()
        });
        drop(baseline);
        drop(reader);

        let (file, count_offset) = if interpolation {
            ("interp_ways.bin", 20)
        } else {
            ("street_ways.bin", 12)
        };
        let path = index_dir.join(file);
        let original = std::fs::read(&path).expect("read way");
        let count = u16::from_le_bytes([original[count_offset], original[count_offset + 1]]);
        assert!(count >= 2);
        let (nodes_file, entries_files) = if interpolation {
            (
                "interp_nodes.bin",
                ["interp_entries.bin", "coarse_interp_entries.bin"],
            )
        } else {
            (
                "street_nodes.bin",
                ["street_entries.bin", "coarse_street_entries.bin"],
            )
        };
        // Keep the owning way intact and append another way's readable nodes.
        // The invalid last-node segment can now produce a plausible distance
        // unless the query checks the owning way's count.
        let nodes_path = index_dir.join(nodes_file);
        let mut nodes = std::fs::read(&nodes_path).expect("read nodes");
        let extra = nodes.clone();
        nodes.extend_from_slice(&extra);
        std::fs::write(nodes_path, nodes).expect("append adjacent geometry");
        for entries_file in entries_files {
            let entries_path = index_dir.join(entries_file);
            let mut entries = std::fs::read(&entries_path).expect("read entries");
            let mut offset = 0;
            while offset < entries.len() {
                let len = usize::from(u16::from_le_bytes([entries[offset], entries[offset + 1]]));
                offset += 2;
                for _ in 0..len {
                    entries[offset + 4..offset + 6].copy_from_slice(&(count - 1).to_le_bytes());
                    offset += 6;
                }
            }
            std::fs::write(entries_path, entries).expect("set last-node segment");
        }
        let reader = Reader::open(&index_dir).expect("spans remain in bounds");
        let candidates = reader.candidates(55.7, lon).expect("valid query");
        assert!(
            if interpolation {
                candidates.interpolations.is_empty()
            } else {
                candidates.streets.is_empty()
            },
            "a segment cannot use nodes outside its owning way"
        );
    }
}

#[test]
fn force_rebuild_preserves_open_reader_mappings() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");
    write_synthetic_indexed_input(&input);
    let config = pbfhogg::geocode_index::builder::BuildConfig {
        input_path: input,
        ..Default::default()
    };
    build_index(&config, &index_dir, false, &crate::common::test_host()).expect("initial build");
    let old = pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("old reader");
    let before = old.query(55.70005, 12.5005).expect("before");
    assert!(before.address.is_some());
    #[cfg(unix)]
    let old_inode = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(index_dir.join("strings.bin"))
            .expect("old strings")
            .ino()
    };
    build_index(&config, &index_dir, true, &crate::common::test_host()).expect("refresh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let new_inode = std::fs::metadata(index_dir.join("strings.bin"))
            .expect("new strings")
            .ino();
        assert_ne!(old_inode, new_inode, "refresh must replace mapped inodes");
    }
    let after = old
        .query(55.70005, 12.5005)
        .expect("old reader after refresh");
    assert_eq!(
        before.address.as_ref().map(|a| a.house_number),
        after.address.as_ref().map(|a| a.house_number)
    );
    pbfhogg::geocode_index::reader::Reader::open(&index_dir).expect("new reader");
}

#[test]
fn caller_built_interpolation_candidate_cannot_escape_its_way() {
    use pbfhogg::geocode_index::{builder, reader::Reader};

    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let index_dir = dir.path().join("index");
    write_synthetic_interpolation_input(&input);
    build_index(
        &builder::BuildConfig {
            input_path: input,
            ..Default::default()
        },
        &index_dir,
        false,
        &crate::common::test_host(),
    )
    .expect("build interpolation fixture");
    let reader = Reader::open(&index_dir).expect("open fixture");
    let mut candidates = reader.candidates(55.7, 12.5030).expect("valid query");
    let mut candidate = candidates.interpolations.pop().expect("interpolation hit");
    assert!(reader.interpolate(&candidate).is_some());
    // The fixture has two nodes: only segment zero belongs to the way.
    for segment in [1, 2, u16::MAX] {
        candidate.segment_index = segment;
        assert_eq!(reader.interpolate(&candidate), None);
    }
    candidate.segment_index = 0;
    candidate.way_index = u32::MAX;
    assert_eq!(reader.interpolate(&candidate), None);
}

#[test]
fn repeated_multiblob_geocode_builds_have_identical_files() {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::geocode_index::builder;
    use pbfhogg::writer::{Compression, PbfWriter};

    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let mut writer = PbfWriter::new(
        std::io::BufWriter::new(std::fs::File::create(&input).expect("create input")),
        Compression::None,
    );
    writer
        .write_header(&HeaderBuilder::new().sorted().build().expect("header"))
        .expect("write header");
    // Cross the admission boundary in both phases, with distinct strings to
    // expose any completion-order interning or record numbering.
    for id in 1..=260 {
        let mut block = BlockBuilder::new();
        let number = id.to_string();
        block.add_node(
            id,
            557_000_000,
            125_000_000 + i32::try_from(id).expect("small fixture ID"),
            [
                ("addr:housenumber", number.as_str()),
                ("addr:street", "Batch Street"),
            ],
            None,
        );
        writer
            .write_primitive_block(block.take().expect("block").expect("node"))
            .expect("write node");
    }
    for id in 1..=260 {
        let mut block = BlockBuilder::new();
        let name = format!("Street {id}");
        block.add_way(
            id,
            [("highway", "residential"), ("name", name.as_str())],
            &[1, 260],
            None,
        );
        writer
            .write_primitive_block(block.take().expect("block").expect("way"))
            .expect("write way");
    }
    writer.flush().expect("flush input");
    drop(writer);
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    for output_dir in [&a, &b] {
        build_index(
            &builder::BuildConfig {
                input_path: input.clone(),
                ..Default::default()
            },
            output_dir,
            false,
            &crate::common::test_host(),
        )
        .expect("build fixture");
    }
    let mut compared = 0;
    for entry in std::fs::read_dir(&a).expect("list output") {
        let entry = entry.expect("directory entry");
        assert!(entry.file_type().expect("type").is_file());
        assert_eq!(
            std::fs::read(entry.path()).expect("first build"),
            std::fs::read(b.join(entry.file_name())).expect("second build"),
            "different output: {:?}",
            entry.file_name(),
        );
        compared += 1;
    }
    assert!(compared > 0);
    assert_eq!(
        compared,
        std::fs::read_dir(&b).expect("list second build").count()
    );
}
