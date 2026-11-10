//! `check` command coverage.
//!
//! `pbfhogg::check` lives behind the `commands` feature, so `crates/lib/tests/all.rs`
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
        max_errors,
        direct_io: false,
        full: false,
        type_filter: None,
    };
    pbfhogg::check::verify_ids::verify_ids(path, &opts, &crate::common::test_host())
        .expect("verify_ids")
        .complete()
        .expect("verify_ids completes")
}

fn verify_node_ids(path: &std::path::Path) -> pbfhogg::check::verify_ids::VerifyIdsReport {
    verify_node_ids_with_max_errors(path, 16)
}

/// `show_ids` is the `--show-ids` detail cap, `None` for no details.
fn run_check_refs(
    path: &std::path::Path,
    check_relations: bool,
    show_ids: Option<usize>,
) -> pbfhogg::Result<pbfhogg::check::refs::RefCheckResult> {
    check_refs_outcome(path, check_relations, show_ids)
        .map(|outcome| outcome.complete().expect("check_refs completes"))
}

fn check_refs_outcome(
    path: &std::path::Path,
    check_relations: bool,
    show_ids: Option<usize>,
) -> pbfhogg::Result<pbfhogg::check::CheckOutcome<pbfhogg::check::refs::RefCheckResult>> {
    let opts = pbfhogg::check::refs::CheckRefsOptions {
        direct_io: false,
        check_relations,
        show_ids: show_ids.map(|max_errors| pbfhogg::check::refs::ShowIds { max_errors }),
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
    let Err(e) = run_check_refs(path, check_relations, None) else {
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
    let report = run_check_refs(&neg_way, false, None).expect("way IDs are not tracked");
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
    let report = run_check_refs(&path, false, None).expect("relations are not scanned");
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

    let report = run_check_refs(&path, true, Some(0)).expect("check_refs");
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
/// (`*_ID_FLOOR` in `crates/lib/src/tunables.rs`), rounded up to the 2^25-ID chunk
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
            direct_io: false,
            check_relations: true,
            show_ids: Some(pbfhogg::check::refs::ShowIds { max_errors: 0 }),
        };
        let report = pbfhogg::check::refs::check_refs(path, &opts, &host)
            .expect("check_refs")
            .complete()
            .expect("check_refs completes");
        // The high node, way and relation lay above the pre-allocation.
        #[cfg(feature = "test-hooks")]
        assert_eq!(
            host.invocation
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
    let report = run_check_refs(&without, true, None).expect("check_refs");
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
        max_errors: 0,
        direct_io: false,
        full: true,
        type_filter: None,
    };
    let host = crate::common::test_host();
    let report = pbfhogg::check::verify_ids::verify_ids(&input, &opts, &host)
        .expect("verify_ids")
        .complete()
        .expect("verify_ids completes");
    // Every copy of the high node lay above the pre-allocation, so the
    // duplicates were found by the serial overflow insertion. Its blobs
    // settle in completion order, which may decide which copy counts as the
    // first, but never how many duplicates there are.
    #[cfg(feature = "test-hooks")]
    assert_eq!(
        host.invocation
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

fn verify_ids_full(
    path: &std::path::Path,
    max_errors: usize,
    host: &pbfhogg::host::Host,
) -> pbfhogg::check::verify_ids::VerifyIdsReport {
    let opts = pbfhogg::check::verify_ids::VerifyIdsOptions {
        max_errors,
        direct_io: false,
        full: true,
        type_filter: None,
    };
    pbfhogg::check::verify_ids::verify_ids(path, &opts, host)
        .expect("verify_ids")
        .complete()
        .expect("verify_ids completes")
}

/// A kind in strict order under `osm_id_cmp` holds no duplicate, so
/// `--ids --full` settles it from the order alone and never builds its ID
/// set: an ID above the pre-allocation is never inserted into the overflow
/// side set the duplicate pass would use.
#[test]
fn check_ids_full_proves_an_ordered_kind_duplicate_free_without_its_set() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("ordered.osm.pbf");
    let nodes = [node(1), node(2), node(HIGH_NODE)];
    crate::common::write_multi_block_test_pbf(&input, &nodes, &[], &[], 1);

    let host = crate::common::test_host();
    let report = verify_ids_full(&input, 0, &host);
    #[cfg(feature = "test-hooks")]
    assert_eq!(
        host.invocation
            .faults()
            .check_overflow_ids
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "an ordered kind needs no duplicate pass"
    );
    assert!(report.full);
    assert!(report.passed);
    assert_eq!(report.node_count, 3);
}

/// The order proof does not change what `--ids --full` says about negative
/// IDs: each one is still an `IdOutOfRange` violation, ordered file or not.
#[test]
fn check_ids_full_still_reports_negative_ids_on_an_ordered_file() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("negatives.osm.pbf");
    write_nodes_in_order(&input, &[0, -1, -2, 1, 2]);

    let report = verify_ids_full(&input, 0, &crate::common::test_host());
    assert!(!report.passed);
    assert_eq!(report.total_violations, 2);
    let negatives: Vec<i64> = report
        .violations
        .iter()
        .filter_map(|v| match v {
            pbfhogg::check::verify_ids::IdViolation::IdOutOfRange { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(negatives, [-1, -2]);
}

/// On a kind out of order `--ids --full` reads it again with its set, and
/// the stored details stay in report order under the error cap: the second
/// pass's findings replace the first's rather than adding to them.
#[test]
fn check_ids_full_duplicate_pass_replaces_the_order_pass_findings() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("dup_neg.osm.pbf");
    let nodes = [node(-1), node(5), node(5)];
    crate::common::write_multi_block_test_pbf(&input, &nodes, &[], &[], 1);

    let report = verify_ids_full(&input, 0, &crate::common::test_host());
    // -1 out of range, 5 -> 5 out of order, the second 5 a duplicate.
    assert_eq!(report.total_violations, 3);
    assert_eq!(report.violations.len(), 3);

    let capped = verify_ids_full(&input, 1, &crate::common::test_host());
    assert_eq!(capped.total_violations, 3);
    assert_eq!(capped.violations.len(), 1);
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

    let report = run_check_refs(&input, true, Some(0)).expect("check_refs with show_ids");

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

    let limited = run_check_refs(&input, true, Some(2)).expect("check_refs with max_errors");
    assert_eq!(limited.missing_refs.len(), 2);
    assert_eq!(limited.total_missing(), report.total_missing());
    assert_eq!(
        limited.missing_relation_member_occurrences, report.missing_relation_member_occurrences,
        "limiting displayed details must not limit the counts"
    );

    let counts_only = run_check_refs(&input, true, None).expect("check_refs without show_ids");
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

// ---------------------------------------------------------------------------
// A blob whose indexdata kind claim its contents contradict
// ---------------------------------------------------------------------------

/// Frame of the way blob in [`write_relabel_fixture`]'s file: the header,
/// then one blob per kind.
const WAY_BLOB: usize = 2;

/// A sorted, indexed file one blob per kind: nodes 1 and 2; ways 10 and 10
/// again (a duplicate) whose refs include the missing node 99; relation 30.
/// Honest, it fails both `--refs` (n99) and `--ids` (w10 twice).
fn write_relabel_fixture(path: &std::path::Path) {
    let relations = [relation(30, MemberId::Node(1))];
    write_test_pbf_sorted(
        path,
        &[node(1), node(2)],
        &[way(10, &[1, 99]), way(10, &[2])],
        &relations,
    );
}

/// The fixture with the way blob's indexdata kind byte set to `kind`
/// (0 node, 2 relation): a lie no producer writes, forged in the header.
fn relabelled_way_blob(dir: &std::path::Path, kind: u8) -> std::path::PathBuf {
    let honest = dir.join("honest.osm.pbf");
    write_relabel_fixture(&honest);
    let bytes = std::fs::read(&honest).expect("read the fixture");
    let lying =
        crate::common::adversarial::mutate_blob_header_indexdata(&bytes, WAY_BLOB, |index| {
            assert_eq!(index[1], 1, "frame {WAY_BLOB} is the way blob");
            index[1] = kind;
        });
    let path = dir.join(format!("way-blob-claims-{kind}.osm.pbf"));
    std::fs::write(&path, lying).expect("write the forged file");
    path
}

fn assert_stopped<R>(
    outcome: pbfhogg::check::CheckOutcome<R>,
    claimed: pbfhogg::ElementKind,
    what: &str,
) {
    let pbfhogg::check::CheckOutcome::Stopped(stopped) = outcome else {
        panic!("{what}: the check must stop at the contradicted claim");
    };
    assert_eq!(
        stopped.contradiction(),
        Some(pbfhogg::KindContradiction {
            claimed,
            found: pbfhogg::ElementKind::Way,
        }),
        "{what}"
    );
    assert_eq!(stopped.blob, Some(WAY_BLOB as u64), "{what}");
    assert!(stopped.offset.is_some(), "{what}");
}

fn verify_ids_outcome(
    path: &std::path::Path,
    full: bool,
    type_filter: Option<pbfhogg::TypeFilter>,
) -> pbfhogg::check::CheckOutcome<pbfhogg::check::verify_ids::VerifyIdsReport> {
    verify_ids_limited(path, full, type_filter, 0)
}

fn verify_ids_limited(
    path: &std::path::Path,
    full: bool,
    type_filter: Option<pbfhogg::TypeFilter>,
    max_errors: usize,
) -> pbfhogg::check::CheckOutcome<pbfhogg::check::verify_ids::VerifyIdsReport> {
    let opts = pbfhogg::check::verify_ids::VerifyIdsOptions {
        max_errors,
        direct_io: false,
        full,
        type_filter,
    };
    pbfhogg::check::verify_ids::verify_ids(path, &opts, &crate::common::test_host())
        .expect("verify_ids runs")
}

/// The honest fixture fails both checks on its contents, so the relabelled
/// runs below fail for the claim, not for what the honest one already shows.
#[test]
fn the_relabel_fixture_fails_both_checks_honestly() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("honest.osm.pbf");
    write_relabel_fixture(&path);
    let refs = run_check_refs(&path, false, None).expect("check_refs");
    assert_eq!(refs.missing_node_refs, 1, "n99");
    let ids = verify_ids_outcome(&path, true, None)
        .complete()
        .expect("verify_ids completes");
    assert!(!ids.passed, "w10 twice");
}

/// A way blob labelled node or relation used to be skipped by the way phase
/// of `check --refs`, which then passed a file with a dangling reference.
/// Every phase that reads the blob now stops the check on it.
#[test]
fn check_refs_stops_on_a_way_blob_claiming_another_kind() {
    let dir = crate::common::test_scratch();
    let as_node = relabelled_way_blob(dir.path(), 0);
    for check_relations in [false, true] {
        assert_stopped(
            check_refs_outcome(&as_node, check_relations, None).expect("check_refs runs"),
            pbfhogg::ElementKind::Node,
            &format!("claims node, check_relations {check_relations}"),
        );
    }

    let as_relation = relabelled_way_blob(dir.path(), 2);
    assert_stopped(
        check_refs_outcome(&as_relation, true, None).expect("check_refs runs"),
        pbfhogg::ElementKind::Relation,
        "claims relation, check_relations",
    );
    // Without `check_relations` no phase decodes a blob claiming relations:
    // the gap `check --index` covers, pinned so a change to it is deliberate.
    let unseen = check_refs_outcome(&as_relation, false, None)
        .expect("check_refs runs")
        .complete()
        .expect("the relation-claimed blob is never decoded");
    assert!(unseen.is_valid());
    assert_eq!(unseen.way_count, 0);
}

/// `check --ids` used to examine no way of a way blob labelled node or
/// relation, passing a duplicate way ID; both modes now stop on it.
#[test]
fn check_ids_stops_on_a_way_blob_claiming_another_kind() {
    let dir = crate::common::test_scratch();
    for (kind, claimed) in [
        (0, pbfhogg::ElementKind::Node),
        (2, pbfhogg::ElementKind::Relation),
    ] {
        let path = relabelled_way_blob(dir.path(), kind);
        for full in [false, true] {
            assert_stopped(
                verify_ids_outcome(&path, full, None),
                claimed,
                &format!("claims {claimed}, full {full}"),
            );
        }
    }
    // Under `-t way` only way-claimed blobs are decoded: the lie is unseen.
    let as_node = relabelled_way_blob(dir.path(), 0);
    let ways_only = pbfhogg::TypeFilter {
        nodes: false,
        ways: true,
        relations: false,
    };
    let unseen = verify_ids_outcome(&as_node, true, Some(ways_only))
        .complete()
        .expect("the node-claimed blob is never decoded under -t way");
    assert_eq!(unseen.way_count, 0);
}

// ---------------------------------------------------------------------------
// Unindexed input: one combined pass, rejudged on a miss
// ---------------------------------------------------------------------------

/// One blob of a fixture, all of one kind.
#[derive(Clone, Copy)]
enum Blob<'a> {
    Nodes(&'a [TestNode]),
    Ways(&'a [TestWay]),
    Relations(&'a [TestRelation]),
}

/// Write `blobs` as unindexed blobs in the given order, under a header that
/// claims no sort order.
fn write_unindexed_blobs(path: &std::path::Path, blobs: &[Blob<'_>]) {
    write_blobs(path, blobs, false);
}

/// Write `blobs` in the given order, each with its indexdata when `indexed`,
/// under a header that claims no sort order.
fn write_blobs(path: &std::path::Path, blobs: &[Blob<'_>], indexed: bool) {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
    use pbfhogg::writer::{Compression, PbfWriter};

    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(&HeaderBuilder::new().build().expect("header"))
        .expect("write header");
    for blob in blobs {
        let mut bb = BlockBuilder::new();
        match *blob {
            Blob::Nodes(nodes) => {
                for n in nodes {
                    bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), None);
                }
            }
            Blob::Ways(ways) => {
                for w in ways {
                    bb.add_way(w.id, w.tags.iter().copied(), &w.refs, None);
                }
            }
            Blob::Relations(relations) => {
                for r in relations {
                    let members: Vec<MemberData<'_>> = r
                        .members
                        .iter()
                        .map(|m| MemberData {
                            id: m.id,
                            role: m.role,
                        })
                        .collect();
                    bb.add_relation(r.id, r.tags.iter().copied(), &members, None);
                }
            }
        }
        let bytes = bb.take().expect("take").expect("a block");
        if indexed {
            writer.write_primitive_block(bytes)
        } else {
            writer.write_primitive_block_no_indexdata(bytes)
        }
        .expect("write block");
    }
    writer.flush().expect("flush");
}

/// Every counter a run emits, by name (the last sample of each).
#[derive(Default)]
struct Counters(std::sync::Mutex<Vec<(String, i64)>>);

impl pbfhogg::debug::InstrumentSink for Counters {
    fn marker(&self, _name: &str) {}
    fn counter(&self, name: &str, value: i64) {
        self.0
            .lock()
            .expect("counters")
            .push((name.to_owned(), value));
    }
}

impl Counters {
    fn get(&self, name: &str) -> i64 {
        self.0
            .lock()
            .expect("counters")
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map_or_else(|| panic!("no counter {name}"), |&(_, v)| v)
    }
}

/// A refs report as comparable values: the counts and the sorted details.
type RefsSummary = ([u64; 8], Vec<(char, i64, char, i64)>);

fn refs_summary(report: &pbfhogg::check::refs::RefCheckResult) -> RefsSummary {
    let mut details: Vec<_> = report
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
    details.sort_unstable();
    (
        [
            report.node_count,
            report.way_count,
            report.relation_count,
            report.missing_node_refs,
            report.missing_way_refs,
            report.missing_node_members,
            report.missing_relation_members,
            report.missing_relation_member_occurrences,
        ],
        details,
    )
}

/// The elements of the unindexed fixtures below, and the indexed sorted
/// file holding the same: nodes 1 to 4, way 10 resolving, way 11 missing
/// n99 twice, relations whose node, way and relation members resolve and
/// miss (n98, w97, r96) and one forward relation member (r21).
fn refs_fixture_elements() -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let nodes = vec![node(1), node(2), node(3), node(4)];
    let ways = vec![way(10, &[1, 2]), way(11, &[3, 99, 4, 99])];
    let member = |id| TestMember { id, role: "" };
    let relations = vec![
        TestRelation {
            id: 20,
            members: vec![
                member(MemberId::Node(1)),
                member(MemberId::Way(10)),
                member(MemberId::Relation(21)),
            ],
            tags: vec![],
            meta: None,
        },
        TestRelation {
            id: 21,
            members: vec![
                member(MemberId::Node(98)),
                member(MemberId::Way(97)),
                member(MemberId::Relation(96)),
                member(MemberId::Way(11)),
            ],
            tags: vec![],
            meta: None,
        },
    ];
    (nodes, ways, relations)
}

/// An unindexed file is judged exactly as its indexed twin, in any blob
/// order: references that miss while their targets were not yet in their
/// set are judged again after them, and a dropped provisional judgement
/// counts nothing twice. Each blob is read once by the combined pass and at
/// most once more per kind.
#[test]
fn check_refs_judges_an_unindexed_file_as_its_indexed_twin_in_any_order() {
    let dir = crate::common::test_scratch();
    let (nodes, ways, relations) = refs_fixture_elements();
    let indexed = dir.path().join("indexed.osm.pbf");
    write_test_pbf_sorted(&indexed, &nodes, &ways, &relations);

    let (n, w, r) = (
        Blob::Nodes(&nodes),
        [Blob::Ways(&ways[..1]), Blob::Ways(&ways[1..])],
        [
            Blob::Relations(&relations[..1]),
            Blob::Relations(&relations[1..]),
        ],
    );
    let orders: [(&str, Vec<Blob<'_>>); 2] = [
        ("sorted", vec![n, w[0], w[1], r[0], r[1]]),
        ("reversed", vec![r[1], r[0], w[1], w[0], n]),
    ];

    for check_relations in [false, true] {
        for show_ids in [None, Some(0)] {
            let want = refs_summary(
                &run_check_refs(&indexed, check_relations, show_ids).expect("indexed"),
            );
            for (name, blobs) in &orders {
                let path = dir.path().join(format!("{name}.osm.pbf"));
                write_unindexed_blobs(&path, blobs);
                let counters = std::sync::Arc::new(Counters::default());
                let mut host = crate::common::test_host();
                host.invocation =
                    pbfhogg::host::Invocation::with_sink(std::sync::Arc::clone(&counters)
                        as std::sync::Arc<dyn pbfhogg::debug::InstrumentSink>);
                let opts = pbfhogg::check::refs::CheckRefsOptions {
                    direct_io: false,
                    check_relations,
                    show_ids: show_ids
                        .map(|max_errors| pbfhogg::check::refs::ShowIds { max_errors }),
                };
                let report = pbfhogg::check::refs::check_refs(&path, &opts, &host)
                    .expect("check_refs")
                    .complete()
                    .expect("completes");
                let what = format!("{name}, relations {check_relations}, show_ids {show_ids:?}");
                assert_eq!(refs_summary(&report), want, "{what}");
                assert_eq!(counters.get("checkrefs_unindexed_blobs"), 5, "{what}");
                assert_eq!(counters.get("checkrefs_node_blobs"), 0, "{what}");
                // Way 11 misses n99 in fact, so its blob is always judged
                // again; way 10's only if it raced the node blob.
                let ways_again = counters.get("checkrefs_way_blobs");
                assert!((1..=2).contains(&ways_again), "{what}: {ways_again}");
                let relations_again = counters.get("checkrefs_relation_blobs");
                if check_relations {
                    assert!(
                        (1..=2).contains(&relations_again),
                        "{what}: {relations_again}"
                    );
                } else {
                    assert_eq!(relations_again, 0, "{what}");
                }
                assert_eq!(
                    counters.get("checkrefs_missing_node_refs_occurrences"),
                    2,
                    "{what}: n99 twice, counted once per occurrence"
                );
            }
        }
    }
}

/// The refusals stand wherever the combined pass meets them: a negative ID
/// it would compare, in any blob order, is refused as on an indexed file.
#[test]
fn check_refs_refuses_negative_ids_in_an_unindexed_file_in_any_order() {
    let dir = crate::common::test_scratch();
    let nodes = [node(1)];
    let neg_ref = [way(10, &[99, -5])];
    let neg_way = [way(-10, &[1])];
    let neg_member = [relation(3, MemberId::Way(-6))];
    let cases: [(&str, Vec<Blob<'_>>, bool, &str); 3] = [
        (
            "neg-ref",
            vec![Blob::Ways(&neg_ref), Blob::Nodes(&nodes)],
            false,
            "w10 references negative ID n-5",
        ),
        (
            "neg-way",
            vec![Blob::Ways(&neg_way), Blob::Nodes(&nodes)],
            true,
            "way -10",
        ),
        (
            "neg-way-member",
            vec![Blob::Relations(&neg_member), Blob::Nodes(&nodes)],
            true,
            "r3 references negative ID w-6",
        ),
    ];
    for (name, blobs, check_relations, needle) in cases {
        let path = dir.path().join(format!("{name}.osm.pbf"));
        write_unindexed_blobs(&path, &blobs);
        assert_refs_refused(&path, check_relations, needle);
    }
}

/// `check --ids` reads an unindexed file once and reports exactly what it
/// reports for the indexed twin, in both modes and under a type filter:
/// order violations and duplicates of each kind, in kind order.
#[test]
fn check_ids_reports_an_unindexed_file_as_its_indexed_twin() {
    let dir = crate::common::test_scratch();
    let nodes = [node(1), node(3), node(2), node(5), node(5)];
    let ways = [way(10, &[1]), way(12, &[1]), way(11, &[1]), way(12, &[1])];
    let relations = [
        relation(30, MemberId::Node(1)),
        relation(30, MemberId::Node(1)),
    ];
    let indexed = dir.path().join("indexed.osm.pbf");
    crate::common::write_multi_block_test_pbf(&indexed, &nodes, &ways, &relations, 2);
    let unindexed = dir.path().join("unindexed.osm.pbf");
    crate::common::write_multi_block_non_indexed_pbf(&unindexed, &nodes, &ways, &relations, 2);
    crate::common::assert_non_indexed(&unindexed);

    let filters = [
        None,
        Some(pbfhogg::TypeFilter {
            nodes: false,
            ways: true,
            relations: true,
        }),
    ];
    for full in [false, true] {
        for filter in filters {
            let report = |path: &std::path::Path| {
                let r = verify_ids_outcome(path, full, filter)
                    .complete()
                    .expect("verify_ids completes");
                // Which copy of a duplicate is reported, and so where it sits
                // among its kind's violations, follows completion order on
                // either file; the violations themselves do not.
                let json = r.to_json_value("f");
                let mut violations: Vec<String> = json["violations"]
                    .as_array()
                    .expect("violations")
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                violations.sort_unstable();
                (
                    [
                        r.node_count,
                        r.way_count,
                        r.relation_count,
                        r.total_violations,
                    ],
                    violations,
                )
            };
            let want = report(&indexed);
            assert!(want.0[3] > 0, "the fixture has violations");
            assert_eq!(report(&unindexed), want, "full {full}, filter {filter:?}");
        }
    }
}

/// The stored violations of a complete `check --ids` report as JSON
/// strings, and its type-order inversions as `(found, after)`, in report
/// order.
fn ids_violations(r: &pbfhogg::check::verify_ids::VerifyIdsReport) -> (Vec<String>, Vec<String>) {
    let json = r.to_json_value("f");
    let violations = json["violations"].as_array().expect("violations");
    let pairs = violations
        .iter()
        .filter(|v| v["type"] == "type_order")
        .map(|v| format!("{} after {}", v["found"], v["after"]).replace('"', ""))
        .collect();
    (violations.iter().map(ToString::to_string).collect(), pairs)
}

const NOT_NODES: pbfhogg::TypeFilter = pbfhogg::TypeFilter {
    nodes: false,
    ways: true,
    relations: true,
};

const NODES_ONLY: pbfhogg::TypeFilter = pbfhogg::TypeFilter {
    nodes: true,
    ways: false,
    relations: false,
};

/// Type order on single-kind blobs: the unindexed file, judged from its
/// decoded kinds, reports what its indexed twin reports from the claims,
/// in both modes and whatever `-t` selects, since type order covers every
/// kind in both arms.
#[test]
fn check_ids_judges_type_order_on_an_unindexed_file_as_on_its_indexed_twin() {
    let dir = crate::common::test_scratch();
    let nodes = [node(1), node(2)];
    let ways = [way(10, &[1])];
    let relations = [relation(30, MemberId::Node(1))];
    let (n, w, r) = (
        Blob::Nodes(&nodes),
        Blob::Ways(&ways),
        Blob::Relations(&relations),
    );
    let cases: [(&str, Vec<Blob<'_>>, &[&str]); 4] = [
        ("in-order", vec![n, w, r], &[]),
        (
            "reversed",
            vec![r, w, n],
            &[
                "node after way",
                "node after relation",
                "way after relation",
            ],
        ),
        ("way-after-relation", vec![n, r, w], &["way after relation"]),
        (
            "node-after-relation-no-ways",
            vec![r, n],
            &["node after relation"],
        ),
    ];
    for (name, blobs, want) in cases {
        let indexed = dir.path().join(format!("{name}-indexed.osm.pbf"));
        write_blobs(&indexed, &blobs, true);
        crate::common::assert_every_data_blob_indexed(&indexed);
        let unindexed = dir.path().join(format!("{name}-unindexed.osm.pbf"));
        write_blobs(&unindexed, &blobs, false);
        crate::common::assert_non_indexed(&unindexed);
        for full in [false, true] {
            for filter in [None, Some(NOT_NODES), Some(NODES_ONLY)] {
                let judge = |path: &std::path::Path| {
                    let r = verify_ids_outcome(path, full, filter)
                        .complete()
                        .expect("verify_ids completes");
                    let counts = [r.node_count, r.way_count, r.relation_count];
                    (ids_violations(&r), r.total_violations, r.passed, counts)
                };
                let got = judge(&unindexed);
                let what = format!("{name}, full {full}, filter {filter:?}");
                assert_eq!(got, judge(&indexed), "{what}");
                assert_eq!(got.0.1, want, "{what}");
                assert_eq!(got.2, want.is_empty(), "{what}");
            }
        }
    }
}

/// An unindexed blob is judged by the order the element walk reads it in:
/// its groups in turn, each group's nodes, then ways, then relations,
/// whatever its field order (a group holding two kinds breaks the format,
/// but is read). So a later group holding a kind below an earlier group's
/// is an inversion, two kinds in one group are not, and an empty group or
/// an empty dense message holds no kind.
#[test]
fn check_ids_judges_an_unindexed_blob_by_its_group_order() {
    use crate::common::groups::{Field, block, write_unindexed_blocks};
    use Field::{Dense, Nodes, Relations, Ways};

    /// A fixture's name, its blocks, and the inversions it must report.
    type Case = (&'static str, Vec<Vec<u8>>, &'static [&'static str]);

    let dir = crate::common::test_scratch();
    let cases: [Case; 7] = [
        (
            "ways-group-then-dense-group",
            vec![block(&[&[Ways(&[10])], &[Dense(&[1])]])],
            &["node after way"],
        ),
        (
            "dense-group-then-ways-group",
            vec![block(&[&[Dense(&[1])], &[Ways(&[10])]])],
            &[],
        ),
        (
            "ways-field-before-dense-field-in-one-group",
            vec![block(&[&[Ways(&[10]), Dense(&[1])]])],
            &[],
        ),
        (
            "plain-nodes-ways-plain-nodes",
            vec![block(&[&[Nodes(&[1])], &[Ways(&[10])], &[Nodes(&[2])]])],
            &["node after way"],
        ),
        (
            "sorted-mixed-blob-then-node-blob",
            vec![
                block(&[&[Dense(&[1])], &[Ways(&[10])]]),
                block(&[&[Dense(&[2])]]),
            ],
            &["node after way"],
        ),
        (
            "empty-groups-hold-no-kind",
            vec![block(&[
                &[Ways(&[10])],
                &[Dense(&[])],
                &[],
                &[Relations(&[30])],
            ])],
            &[],
        ),
        (
            "relations-group-then-ways-group",
            vec![block(&[
                &[Dense(&[1])],
                &[Relations(&[30])],
                &[Ways(&[10])],
            ])],
            &["way after relation"],
        ),
    ];
    for (name, blocks, want) in cases {
        let path = dir.path().join(format!("{name}.osm.pbf"));
        write_unindexed_blocks(&path, &blocks);
        crate::common::assert_non_indexed(&path);
        for full in [false, true] {
            for filter in [None, Some(NODES_ONLY)] {
                let r = verify_ids_outcome(&path, full, filter)
                    .complete()
                    .expect("verify_ids completes");
                let what = format!("{name}, full {full}, filter {filter:?}");
                assert_eq!(ids_violations(&r).1, want, "{what}");
                assert_eq!(r.total_violations, want.len() as u64, "{what}");
                assert_eq!(r.passed, want.is_empty(), "{what}");
            }
        }
    }
}

/// Type-order inversions come first in the report and count against
/// `--max-errors` like any violation, every one counted in the total, on an
/// unindexed file as on its indexed twin.
#[test]
fn check_ids_type_order_is_reported_first_and_counted_past_the_limit() {
    let dir = crate::common::test_scratch();
    let nodes_hi = [node(2)];
    let nodes_lo = [node(1)];
    let ways = [way(10, &[1])];
    let relations = [relation(30, MemberId::Node(1))];
    let blobs = [
        Blob::Relations(&relations),
        Blob::Ways(&ways),
        Blob::Nodes(&nodes_hi),
        Blob::Nodes(&nodes_lo),
    ];
    let all = [
        r#"{"after":"way","found":"node","type":"type_order"}"#,
        r#"{"after":"relation","found":"node","type":"type_order"}"#,
        r#"{"after":"relation","found":"way","type":"type_order"}"#,
        r#"{"elem_type":"node","id":1,"prev_id":2,"type":"non_monotonic"}"#,
    ];
    for indexed in [false, true] {
        let path = dir.path().join(format!("limits-{indexed}.osm.pbf"));
        write_blobs(&path, &blobs, indexed);
        for (max_errors, stored) in [(1, 1), (3, 3), (0, 4)] {
            let r = verify_ids_limited(&path, false, None, max_errors)
                .complete()
                .expect("verify_ids completes");
            let what = format!("indexed {indexed}, max_errors {max_errors}");
            assert_eq!(ids_violations(&r).0, all[..stored], "{what}");
            assert_eq!(r.total_violations, 4, "{what}");
        }
    }
}
