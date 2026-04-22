mod common;

use common::{TestMember, TestNode, TestRelation, TestWay, write_test_pbf_sorted};
use pbfhogg::MemberId;
use tempfile::TempDir;

#[test]
fn check_refs_show_ids_reports_unique_counts_and_occurrences() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[TestNode { id: 1, lat: 0, lon: 0, tags: vec![], meta: None }],
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
                    TestMember { id: MemberId::Node(3), role: "label" },
                    TestMember { id: MemberId::Way(20), role: "outer" },
                    TestMember { id: MemberId::Relation(200), role: "subarea" },
                    TestMember { id: MemberId::Relation(200), role: "defaults" },
                ],
                tags: vec![("type", "boundary")],
                meta: None,
            },
            TestRelation {
                id: 101,
                members: vec![TestMember { id: MemberId::Relation(200), role: "subarea" }],
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
    assert_eq!(report.missing_node_members, 1, "relation refs dedup to node 3");
    assert_eq!(
        report.missing_relation_members,
        1,
        "three relation-member misses all point to the same missing relation"
    );
    assert_eq!(
        report.missing_relation_member_occurrences,
        3,
        "all three missing relation-member occurrences should be counted"
    );
    assert_eq!(report.total_missing(), 4, "total_missing sums the unique counters");
    assert!(!report.is_valid(), "fixture contains missing refs in all checked categories");

    let mut got: Vec<_> = report
        .missing_refs
        .iter()
        .map(|m| (m.missing_type, m.missing_id, m.referencing_type, m.referencing_id))
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
    assert!(counts_only.missing_refs.is_empty(), "show_ids=false should not materialize missing_refs");
    assert_eq!(counts_only.missing_node_refs, report.missing_node_refs);
    assert_eq!(counts_only.missing_way_refs, report.missing_way_refs);
    assert_eq!(counts_only.missing_node_members, report.missing_node_members);
    assert_eq!(
        counts_only.missing_relation_members,
        report.missing_relation_members
    );
    assert_eq!(
        counts_only.missing_relation_member_occurrences,
        report.missing_relation_member_occurrences
    );
}
