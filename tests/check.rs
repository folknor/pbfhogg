//! `check` command coverage.
//!
//! `pbfhogg::check` lives behind the `commands` feature, so this file
//! compiles out of the no-features sweep rather than breaking it.
#![cfg(feature = "commands")]

mod common;

use common::{TestMember, TestNode, TestRelation, TestWay, write_test_pbf_sorted};
use pbfhogg::MemberId;
use tempfile::TempDir;

/// Build a node-only sorted fixture with the given IDs, in the given order.
fn write_nodes_in_order(path: &std::path::Path, ids: &[i64]) {
    let nodes: Vec<TestNode> = ids
        .iter()
        .map(|&id| TestNode {
            id,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        })
        .collect();
    write_test_pbf_sorted(path, &nodes, &[], &[]);
}

fn verify_node_ids(path: &std::path::Path) -> pbfhogg::check::verify_ids::VerifyIdsReport {
    let opts = pbfhogg::check::verify_ids::VerifyIdsOptions {
        full: false,
        type_filter: None,
        max_errors: 16,
        direct_io: false,
    };
    pbfhogg::check::verify_ids::verify_ids(path, &opts).expect("verify_ids")
}

/// `check --ids` must judge monotonicity by the same canonical OSM ID order
/// (`0`, then `-1, -2, ...`, then `1, 2, ...`) that `sort` produces, not by
/// raw `i64` comparison. The two agree on every positive-only file, so this
/// pair of fixtures - both node-only, differing only in the order of the same
/// five IDs - is what separates them.
#[test]
fn check_ids_uses_canonical_osm_order_for_negative_ids() {
    let dir = TempDir::new().expect("tempdir");

    // `sort`'s own output order. Raw `<=` calls the step from 0 to -1
    // non-monotonic and rejects a file the sorter just produced.
    let canonical = dir.path().join("canonical.osm.pbf");
    write_nodes_in_order(&canonical, &[0, -1, -2, 1, 2]);
    let report = verify_node_ids(&canonical);
    assert_eq!(report.node_count, 5);
    assert_eq!(
        report.total_violations, 0,
        "canonical OSM order is 0, then negatives by ascending absolute value, \
         then positives - `sort` emits exactly this and `check --ids` must accept it"
    );
    assert!(report.passed);

    // The passing counterpart's mirror image: raw-i64-sorted, which `sort`
    // considers unsorted. Every adjacent pair up to 0 is an inversion under
    // canonical order, so this must be rejected rather than waved through.
    let raw_sorted = dir.path().join("raw_sorted.osm.pbf");
    write_nodes_in_order(&raw_sorted, &[-2, -1, 0, 1, 2]);
    let report = verify_node_ids(&raw_sorted);
    assert_eq!(report.node_count, 5);
    assert!(
        !report.passed,
        "ascending raw i64 order puts -2 before -1 before 0, which is backwards \
         in canonical OSM order; a raw `<=` verifier passes it silently"
    );
    assert_eq!(
        report.total_violations, 2,
        "-2 -> -1 and -1 -> 0 are both canonical-order inversions; 0 -> 1 -> 2 are fine"
    );
}

/// A node the tracking `IdSet` cannot represent - a negative (JOSM
/// editor-local) ID - is never recorded as present, so the way that references
/// it is reported as having a missing ref. That over-report is deliberate, but
/// it has to be *labelled*: `verify_ids` reports the same situation as
/// `IdOutOfRange`, and `check --refs` silently dropped the
/// `set_atomic_checked` bool, leaving the reader unable to tell it from a
/// genuinely dangling reference.
#[test]
fn check_refs_reports_untrackable_ids_separately_from_missing_refs() {
    let dir = TempDir::new().expect("tempdir");

    let untrackable = dir.path().join("negative.osm.pbf");
    write_test_pbf_sorted(
        &untrackable,
        &[TestNode {
            id: -5,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![-5],
            tags: vec![],
            meta: None,
        }],
        &[],
    );

    let report = pbfhogg::check::refs::check_refs(&untrackable, true, false, false)
        .expect("check_refs on negative IDs");
    assert_eq!(report.node_count, 1);
    assert_eq!(
        report.untrackable_ids[0], 1,
        "node -5 cannot be entered into the IdSet and the report must say so"
    );
    assert_eq!(
        report.total_untrackable(),
        1,
        "the way's own ID is positive and trackable"
    );
    assert_eq!(
        report.missing_node_refs, 1,
        "w10's ref to n-5 is over-reported as missing - that is what the \
         untrackable counter exists to explain"
    );

    // Passing counterpart: the same shape with a trackable ID reports the
    // same missing ref and no untrackable IDs, so a test that always saw
    // zero untrackable IDs could not tell the two fixtures apart.
    let trackable = dir.path().join("positive.osm.pbf");
    write_test_pbf_sorted(
        &trackable,
        &[TestNode {
            id: 5,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![6],
            tags: vec![],
            meta: None,
        }],
        &[],
    );
    let report = pbfhogg::check::refs::check_refs(&trackable, true, false, false)
        .expect("check_refs on positive IDs");
    assert_eq!(report.missing_node_refs, 1, "n6 is genuinely absent");
    assert_eq!(
        report.total_untrackable(),
        0,
        "every ID in this fixture is representable, so nothing is untrackable"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn check_refs_show_ids_reports_unique_counts_and_occurrences() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[TestNode {
            id: 1,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "residential")],
            meta: None,
        }],
        &[
            TestRelation {
                id: 100,
                members: vec![
                    TestMember {
                        id: MemberId::Node(3),
                        role: "label",
                    },
                    TestMember {
                        id: MemberId::Way(20),
                        role: "outer",
                    },
                    TestMember {
                        id: MemberId::Relation(200),
                        role: "subarea",
                    },
                    TestMember {
                        id: MemberId::Relation(200),
                        role: "defaults",
                    },
                ],
                tags: vec![("type", "boundary")],
                meta: None,
            },
            TestRelation {
                id: 101,
                members: vec![TestMember {
                    id: MemberId::Relation(200),
                    role: "subarea",
                }],
                tags: vec![("type", "boundary")],
                meta: None,
            },
        ],
    );

    let report = pbfhogg::check::refs::check_refs(&input, true, true, false)
        .expect("check_refs with show_ids");

    assert_eq!(report.node_count, 1);
    assert_eq!(report.way_count, 1);
    assert_eq!(report.relation_count, 2);
    assert_eq!(report.missing_node_refs, 1, "way refs dedup to node 2");
    assert_eq!(report.missing_way_refs, 1, "relation refs dedup to way 20");
    assert_eq!(
        report.missing_node_members, 1,
        "relation refs dedup to node 3"
    );
    assert_eq!(
        report.missing_relation_members, 1,
        "three relation-member misses all point to the same missing relation"
    );
    assert_eq!(
        report.missing_relation_member_occurrences, 3,
        "all three missing relation-member occurrences should be counted"
    );
    assert_eq!(
        report.total_missing(),
        4,
        "total_missing sums the unique counters"
    );
    assert!(
        !report.is_valid(),
        "fixture contains missing refs in all checked categories"
    );

    let mut got: Vec<_> = report
        .missing_refs
        .iter()
        .map(|m| {
            (
                m.missing_type,
                m.missing_id,
                m.referencing_type,
                m.referencing_id,
            )
        })
        .collect();
    got.sort_unstable();

    let mut want = vec![
        ('n', 2, 'w', 10),
        ('n', 3, 'r', 100),
        ('w', 20, 'r', 100),
        ('r', 200, 'r', 100),
        ('r', 200, 'r', 100),
        ('r', 200, 'r', 101),
    ];
    want.sort_unstable();

    assert_eq!(
        got, want,
        "show_ids should preserve every missing-ref occurrence, including duplicate relation members"
    );

    let counts_only = pbfhogg::check::refs::check_refs(&input, true, false, false)
        .expect("check_refs without show_ids");
    assert!(
        counts_only.missing_refs.is_empty(),
        "show_ids=false should not materialize missing_refs"
    );
    assert_eq!(counts_only.missing_node_refs, report.missing_node_refs);
    assert_eq!(counts_only.missing_way_refs, report.missing_way_refs);
    assert_eq!(
        counts_only.missing_node_members,
        report.missing_node_members
    );
    assert_eq!(
        counts_only.missing_relation_members,
        report.missing_relation_members
    );
    assert_eq!(
        counts_only.missing_relation_member_occurrences,
        report.missing_relation_member_occurrences
    );
}
