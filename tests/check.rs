//! `check` command coverage.
//!
//! `pbfhogg::check` lives behind the `commands` feature, so `tests/all.rs`
//! mounts this module only with that feature on.

use crate::common::{TestMember, TestNode, TestRelation, TestWay, write_test_pbf_sorted};
use pbfhogg::MemberId;

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

fn verify_node_ids_with_max_errors(
    path: &std::path::Path,
    max_errors: usize,
) -> pbfhogg::check::verify_ids::VerifyIdsReport {
    let opts = pbfhogg::check::verify_ids::VerifyIdsOptions {
        check: pbfhogg::check::CheckOptions {
            max_errors,
            direct_io: false,
        },
        full: false,
        type_filter: None,
    };
    pbfhogg::check::verify_ids::verify_ids(path, &opts, &crate::common::test_host())
        .expect("verify_ids")
}

fn verify_node_ids(path: &std::path::Path) -> pbfhogg::check::verify_ids::VerifyIdsReport {
    verify_node_ids_with_max_errors(path, 16)
}

fn run_check_refs(
    path: &std::path::Path,
    check_relations: bool,
    show_ids: bool,
    max_errors: usize,
) -> pbfhogg::Result<pbfhogg::check::refs::RefCheckResult> {
    let opts = pbfhogg::check::refs::CheckRefsOptions {
        check: pbfhogg::check::CheckOptions {
            max_errors,
            direct_io: false,
        },
        check_relations,
        show_ids,
    };
    pbfhogg::check::refs::check_refs(path, &opts, &crate::common::test_host())
}

/// `check --ids` must judge monotonicity by the same canonical OSM ID order
/// (`0`, then `-1, -2, ...`, then `1, 2, ...`) that `sort` produces, not by
/// raw `i64` comparison. The two agree on every positive-only file, so this
/// pair of fixtures - both node-only, differing only in the order of the same
/// five IDs - is what separates them.
#[test]
fn check_ids_uses_canonical_osm_order_for_negative_ids() {
    let dir = crate::common::test_scratch();

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

#[test]
fn check_ids_zero_max_errors_retains_all_violation_details() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("raw_sorted.osm.pbf");
    write_nodes_in_order(&input, &[-2, -1, 0, 1, 2]);

    let unlimited = verify_node_ids_with_max_errors(&input, 0);
    assert_eq!(unlimited.total_violations, 2);
    assert_eq!(
        unlimited.violations.len(),
        2,
        "zero should retain every violation detail"
    );

    let limited = verify_node_ids_with_max_errors(&input, 1);
    assert_eq!(limited.total_violations, 2);
    assert_eq!(limited.violations.len(), 1);
}

fn node(id: i64) -> TestNode {
    TestNode {
        id,
        lat: 0,
        lon: 0,
        tags: vec![],
        meta: None,
    }
}

fn way(id: i64, refs: &[i64]) -> TestWay {
    TestWay {
        id,
        refs: refs.to_vec(),
        tags: vec![],
        meta: None,
    }
}

fn relation(id: i64, member: MemberId) -> TestRelation {
    TestRelation {
        id,
        members: vec![TestMember {
            id: member,
            role: "",
        }],
        tags: vec![],
        meta: None,
    }
}

/// Expect `check --refs` to refuse `path` as invalid data, naming `needle`.
fn assert_refs_refused(path: &std::path::Path, check_relations: bool, needle: &str) {
    let Err(e) = run_check_refs(path, check_relations, false, 0) else {
        panic!("{}: check_refs must refuse it", path.display());
    };
    assert!(
        matches!(e.kind(), pbfhogg::ErrorKind::InvalidData { .. }),
        "{}: the refusal must be InvalidData, got: {e:?}",
        path.display()
    );
    let message = e.to_string();
    assert!(
        message.contains(needle),
        "{}: the refusal must name {needle}, got: {message}",
        path.display()
    );
}

/// Negative IDs are invalid input (ADR-0002) and the tracking sets cannot
/// hold them, so `check --refs` refuses one wherever it would compare it,
/// instead of reporting every reference to it as missing. Each fixture has
/// one negative ID and is otherwise consistent, so a refusal can only come
/// from that ID.
#[test]
fn check_refs_refuses_the_negative_ids_it_compares() {
    let dir = crate::common::test_scratch();
    let write = |name: &str, nodes: &[TestNode], ways: &[TestWay], rels: &[TestRelation]| {
        let path = dir.path().join(name);
        write_test_pbf_sorted(&path, nodes, ways, rels);
        path
    };

    // An element ID the check tracks.
    let neg_node = write(
        "neg-node.osm.pbf",
        &[node(-5), node(1)],
        &[way(10, &[1])],
        &[],
    );
    assert_refs_refused(&neg_node, false, "node -5");
    let neg_way = write("neg-way.osm.pbf", &[node(1)], &[way(-10, &[1])], &[]);
    assert_refs_refused(&neg_way, true, "way -10");
    let neg_rel = write(
        "neg-rel.osm.pbf",
        &[node(1)],
        &[],
        &[relation(-3, MemberId::Node(1))],
    );
    assert_refs_refused(&neg_rel, true, "relation -3");

    // A checked reference, on each path: a way ref and node and way members
    // fail a lookup first, a relation member is deferred without one.
    let neg_ref = write("neg-ref.osm.pbf", &[node(1)], &[way(10, &[1, -5])], &[]);
    assert_refs_refused(&neg_ref, false, "w10 references negative ID n-5");
    for (name, member, needle) in [
        (
            "neg-node-member.osm.pbf",
            MemberId::Node(-5),
            "r3 references negative ID n-5",
        ),
        (
            "neg-way-member.osm.pbf",
            MemberId::Way(-6),
            "r3 references negative ID w-6",
        ),
        (
            "neg-rel-member.osm.pbf",
            MemberId::Relation(-7),
            "r3 references negative ID r-7",
        ),
    ] {
        let path = write(name, &[node(1)], &[way(10, &[1])], &[relation(3, member)]);
        assert_refs_refused(&path, true, needle);
    }

    // Passing counterpart: a negative way ID with relations off feeds no
    // comparison, so it is not refused, and the file is otherwise valid.
    let report = run_check_refs(&neg_way, false, false, 0).expect("way IDs are not tracked");
    assert!(report.is_valid());
    assert_eq!(report.way_count, 1);
}

/// A relation member of a type no set can resolve is refused rather than
/// skipped, which would certify a reference nobody checked.
#[test]
fn check_refs_refuses_an_unknown_member_type() {
    use crate::common::adversarial;

    let dir = crate::common::test_scratch();
    let path = dir.path().join("unknown-member.osm.pbf");
    // The writers refuse an unknown member type, so write a node member and
    // turn its type into 7 on the wire. The relation blob is the last frame.
    write_test_pbf_sorted(&path, &[node(1)], &[], &[relation(3, MemberId::Node(99))]);
    let bytes = std::fs::read(&path).expect("read fixture");
    let last = adversarial::locate_blobs(&bytes).len() - 1;
    let mutated = adversarial::set_relation_last_member_type(&bytes, last, 0, 7);
    std::fs::write(&path, mutated).expect("write fixture");
    assert_refs_refused(&path, true, "r3 has member 99 of unsupported member type 7");

    // Without relations the member is never looked at.
    let report = run_check_refs(&path, false, false, 0).expect("relations are not scanned");
    assert!(report.is_valid());
}

/// The unique-missing counts come from one set per category: a node missing
/// from several ways counts once, a node missing from a way and from a
/// relation counts in both categories, and the counts span blobs.
#[test]
fn check_refs_counts_unique_missing_ids_per_category_across_blobs() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("missing.osm.pbf");
    let ways = [way(10, &[5, 6, 5]), way(11, &[6, 7]), way(12, &[1])];
    let rels = [
        relation(3, MemberId::Node(5)),
        relation(4, MemberId::Node(5)),
        relation(5, MemberId::Way(20)),
    ];
    crate::common::write_multi_block_test_pbf(&path, &[node(1)], &ways, &rels, 1);

    let report = run_check_refs(&path, true, true, 0).expect("check_refs");
    assert_eq!(report.missing_node_refs, 3, "n5, n6 and n7, each once");
    assert_eq!(
        report.missing_node_members, 1,
        "n5 again, in its own category"
    );
    assert_eq!(report.missing_way_refs, 1);
    assert_eq!(report.total_missing(), 5);
    assert_eq!(
        report.missing_refs.len(),
        8,
        "every occurrence keeps its detail: five way refs, three members"
    );
}

/// IDs above what a small file pre-allocates: its kind's floor
/// (`*_ID_FLOOR` in `src/tunables.rs`), rounded up to the 2^25-ID chunk
/// `IdSet` allocates whole.
const HIGH_NODE: i64 = 15_000_000_000;
const HIGH_WAY: i64 = 2_500_000_000;
const HIGH_RELATION: i64 = 40_000_000;

/// IDs above a small file's pre-allocation (its floor; an indexed file's
/// claim cannot raise it past the file's size) take the serial overflow
/// path, counted on the run's token in test-hooks builds. Every reference
/// among them still resolves, for nodes, ways and relations, indexed or not.
#[test]
fn check_refs_resolves_ids_above_the_allocation() {
    let dir = crate::common::test_scratch();
    let nodes = [node(1), node(HIGH_NODE)];
    let ways = [
        TestWay {
            id: 10,
            refs: vec![1, HIGH_NODE],
            tags: vec![],
            meta: None,
        },
        TestWay {
            id: HIGH_WAY,
            refs: vec![HIGH_NODE, 1],
            tags: vec![],
            meta: None,
        },
    ];
    let relations = [
        TestRelation {
            id: 7,
            members: vec![TestMember {
                id: MemberId::Relation(HIGH_RELATION),
                role: "subarea",
            }],
            tags: vec![],
            meta: None,
        },
        TestRelation {
            id: HIGH_RELATION,
            members: vec![
                TestMember {
                    id: MemberId::Node(HIGH_NODE),
                    role: "label",
                },
                TestMember {
                    id: MemberId::Way(HIGH_WAY),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Relation(7),
                    role: "subarea",
                },
            ],
            tags: vec![],
            meta: None,
        },
    ];
    let indexed = dir.path().join("indexed.osm.pbf");
    write_test_pbf_sorted(&indexed, &nodes, &ways, &relations);
    let unindexed = dir.path().join("unindexed.osm.pbf");
    crate::common::write_test_pbf_non_indexed(&unindexed, &nodes, &ways, &relations);

    for path in [&indexed, &unindexed] {
        let host = crate::common::test_host();
        let opts = pbfhogg::check::refs::CheckRefsOptions {
            check: pbfhogg::check::CheckOptions {
                max_errors: 0,
                direct_io: false,
            },
            check_relations: true,
            show_ids: true,
        };
        let report = pbfhogg::check::refs::check_refs(path, &opts, &host).expect("check_refs");
        // The high node, way and relation lay above the pre-allocation.
        #[cfg(feature = "test-hooks")]
        assert_eq!(
            host.cancel
                .faults()
                .check_overflow_ids
                .load(std::sync::atomic::Ordering::Relaxed),
            3,
            "{}: the high IDs must take the overflow path",
            path.display()
        );
        let missing: Vec<_> = report
            .missing_refs
            .iter()
            .map(|m| (m.missing_type, m.missing_id))
            .collect();
        assert!(
            report.is_valid(),
            "{}: every reference resolves, missing {missing:?}",
            path.display()
        );
        assert_eq!(
            (report.node_count, report.way_count, report.relation_count),
            (2, 2, 2)
        );
    }

    // Passing counterpart: drop the high node and its references go missing,
    // so the overflow path is what resolved them above.
    let without = dir.path().join("without.osm.pbf");
    write_test_pbf_sorted(&without, &nodes[..1], &ways, &relations);
    let report = run_check_refs(&without, true, false, 0).expect("check_refs");
    assert_eq!(report.missing_node_refs, 1);
    assert_eq!(report.missing_node_members, 1);
}

/// `--ids --full` finds a duplicate of an ID above the file's pre-allocation
/// across blobs, as it does one inside it.
#[test]
fn check_ids_full_finds_a_duplicate_above_the_allocation() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("dup.osm.pbf");
    let nodes = [node(1), node(HIGH_NODE), node(HIGH_NODE), node(HIGH_NODE)];
    crate::common::write_multi_block_test_pbf(&input, &nodes, &[], &[], 1);

    let opts = pbfhogg::check::verify_ids::VerifyIdsOptions {
        check: pbfhogg::check::CheckOptions {
            max_errors: 0,
            direct_io: false,
        },
        full: true,
        type_filter: None,
    };
    let host = crate::common::test_host();
    let report = pbfhogg::check::verify_ids::verify_ids(&input, &opts, &host).expect("verify_ids");
    // Every copy of the high node lay above the pre-allocation, so the
    // duplicates were found by the serial overflow insertion. Its blobs
    // settle in completion order, which may decide which copy counts as the
    // first, but never how many duplicates there are.
    #[cfg(feature = "test-hooks")]
    assert_eq!(
        host.cancel
            .faults()
            .check_overflow_ids
            .load(std::sync::atomic::Ordering::Relaxed),
        3
    );
    assert!(!report.passed);
    let duplicates = report
        .violations
        .iter()
        .filter(|v| {
            matches!(
                v,
                pbfhogg::check::verify_ids::IdViolation::Duplicate { id, .. } if *id == HIGH_NODE
            )
        })
        .count();
    assert_eq!(
        duplicates,
        2,
        "three copies of {HIGH_NODE} are two duplicates, of {} violations",
        report.violations.len()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn check_refs_show_ids_reports_unique_counts_and_occurrences() {
    let dir = crate::common::test_scratch();
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

    let report = run_check_refs(&input, true, true, 0).expect("check_refs with show_ids");

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

    let limited = run_check_refs(&input, true, true, 2).expect("check_refs with max_errors");
    assert_eq!(limited.missing_refs.len(), 2);
    assert_eq!(limited.total_missing(), report.total_missing());
    assert_eq!(
        limited.missing_relation_member_occurrences, report.missing_relation_member_occurrences,
        "limiting displayed details must not limit the counts"
    );

    let counts_only = run_check_refs(&input, true, false, 0).expect("check_refs without show_ids");
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
