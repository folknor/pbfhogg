//! `check::locations::check_locations`: inline way-node coordinates against
//! the node records of the same file.
//!
//! Every detection test has an honest counterpart through the same
//! assertions, because a check that fails everything passes every detection
//! test. The partitioning tests run the same fixtures at several store
//! budgets: the number of partitions is a resource decision and must never
//! change the answer.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::check::CheckOptions;
use pbfhogg::check::locations::{
    CheckLocationsOptions, LocationViolationKind, LocationsReport, check_locations,
};
use pbfhogg::writer::{Compression, PbfWriter};

/// One way: id, refs, and inline coordinates (`None`: no coordinate arrays).
type Way = (i64, Vec<i64>, Option<Vec<(i32, i32)>>);

/// A file's contents, written one blob per inner vector.
struct Fixture {
    header_locations: bool,
    historical: bool,
    indexed: bool,
    node_blobs: Vec<Vec<(i64, i32, i32)>>,
    way_blobs: Vec<Vec<Way>>,
}

impl Fixture {
    fn new(node_blobs: Vec<Vec<(i64, i32, i32)>>, way_blobs: Vec<Vec<Way>>) -> Self {
        Self {
            header_locations: true,
            historical: false,
            indexed: true,
            node_blobs,
            way_blobs,
        }
    }

    fn write(&self, path: &Path) {
        let file = std::fs::File::create(path).unwrap();
        let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
        let mut header = HeaderBuilder::new().sorted();
        if self.header_locations {
            header = header.optional_feature("LocationsOnWays");
        }
        if self.historical {
            header = header.historical();
        }
        writer.write_header(&header.build().unwrap()).unwrap();
        let mut emit = |bb: &mut BlockBuilder| {
            let bytes = bb.take().unwrap().unwrap();
            if self.indexed {
                writer.write_primitive_block(bytes).unwrap();
            } else {
                writer.write_primitive_block_no_indexdata(bytes).unwrap();
            }
        };
        let mut bb = BlockBuilder::new();
        for blob in &self.node_blobs {
            for &(id, lat, lon) in blob {
                bb.add_node(id, lat, lon, std::iter::empty(), None);
            }
            emit(&mut bb);
        }
        for blob in &self.way_blobs {
            for (id, refs, coords) in blob {
                match coords {
                    Some(coords) => {
                        bb.add_way_with_locations(*id, std::iter::empty(), refs, coords, None);
                    }
                    None => bb.add_way(*id, std::iter::empty(), refs, None),
                }
            }
            emit(&mut bb);
        }
        drop(emit);
        writer.flush().unwrap();
    }
}

fn opts(agreement: bool, budget: Option<u64>) -> CheckLocationsOptions {
    CheckLocationsOptions {
        check: CheckOptions {
            max_errors: 0,
            direct_io: false,
        },
        agreement,
        store_budget_bytes: budget,
    }
}

fn run(path: &Path, agreement: bool, budget: Option<u64>) -> LocationsReport {
    check_locations(path, &opts(agreement, budget), &crate::common::test_host()).unwrap()
}

/// Node IDs far enough apart to land in different `IdSet` chunks and so
/// exercise the partition boundaries on more than one chunk.
const A: i64 = 7;
const B: i64 = 1 << 26;
const C: i64 = (1 << 27) + 3;
const D: i64 = 5_000_000_000;

fn honest() -> Fixture {
    Fixture::new(
        vec![
            vec![(A, 100, 200), (A + 1, 300, 400)],
            vec![(B, -500, 600), (C, 0, 0)],
            vec![(D, 899_999_999, -1_799_999_999)],
        ],
        vec![
            vec![
                (
                    10,
                    vec![A, B, A + 1],
                    Some(vec![(100, 200), (-500, 600), (300, 400)]),
                ),
                (
                    11,
                    vec![C, D],
                    Some(vec![(0, 0), (899_999_999, -1_799_999_999)]),
                ),
            ],
            vec![(
                12,
                vec![D, A, D],
                Some(vec![
                    (899_999_999, -1_799_999_999),
                    (100, 200),
                    (899_999_999, -1_799_999_999),
                ]),
            )],
        ],
    )
}

#[test]
fn an_honest_file_passes_with_every_ref_verified() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("honest.osm.pbf");
    honest().write(&path);
    let report = run(&path, false, None);
    assert!(report.passed(), "{report:?}");
    assert_eq!(report.nodes, 5);
    assert_eq!(report.ways, 3);
    assert_eq!(report.refs, 8);
    assert_eq!(report.refs_verified, 8);
    assert_eq!(report.refs_without_node, 0);
    assert!(report.violations.is_empty());
}

#[test]
fn a_stale_coordinate_is_a_mismatch_naming_both_values() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("stale.osm.pbf");
    let mut fixture = honest();
    // Way 12's second ref carries an old position of node A.
    fixture.way_blobs[1][0].2.as_mut().unwrap()[1] = (101, 200);
    fixture.write(&path);
    let report = run(&path, false, None);
    assert!(!report.passed());
    assert_eq!(report.refs_mismatched, 1);
    assert_eq!(report.nodes_mismatched, 1);
    assert_eq!(report.refs_verified, 7);
    assert_eq!(
        report
            .violations
            .iter()
            .map(|v| &v.kind)
            .collect::<Vec<_>>(),
        [&LocationViolationKind::Mismatch {
            way_id: 12,
            node_id: A,
            position: 1,
            inline: (101, 200),
            node: (100, 200),
        }]
    );
}

/// The fixture whose answer every budget must reproduce: mismatches in
/// several partitions, absent nodes on both sides of present ones, a
/// conflicting node and a way without coordinates.
fn mixed() -> Fixture {
    Fixture::new(
        vec![
            vec![(A, 100, 200), (A, 100, 200), (B, 5, 5), (B, 6, 6)],
            vec![(C, 0, 0), (D, 1, 1)],
        ],
        vec![
            vec![
                (20, vec![A, B, C], Some(vec![(100, 201), (5, 5), (0, 0)])),
                (21, vec![3, A + 1, D], Some(vec![(0, 0), (9, 9), (2, 1)])),
                (22, vec![C, D + 1], None),
            ],
            vec![
                (
                    23,
                    vec![A + 1, D + 1, 3],
                    Some(vec![(9, 8), (4, 4), (0, 0)]),
                ),
                (24, vec![D, C], Some(vec![(1, 1), (0, 1)])),
            ],
        ],
    )
}

/// The report with its cost figures cleared: what must not depend on the
/// budget or on whether the file is indexed.
fn comparable(mut r: LocationsReport) -> LocationsReport {
    r.partitions = 0;
    r.blob_decodes = 0;
    r
}

/// Every reference lands in exactly one bucket.
fn assert_refs_partitioned(r: &LocationsReport) {
    assert_eq!(
        r.refs,
        r.refs_verified
            + r.refs_mismatched
            + r.refs_not_compared
            + r.refs_to_conflicting_nodes
            + r.refs_to_invalid_nodes
            + r.refs_without_node,
        "{r:?}"
    );
}

/// Routing reads only blobs that can hold what a pass needs, for unindexed
/// input too: the up-front pass records what each blob holds. The mixed
/// fixture at a one-slot budget has four partitions over two node blobs
/// (IDs 7..=B and C..=D) and two way blobs.
#[test]
fn partitioned_runs_read_each_blob_only_where_needed() {
    let dir = crate::common::test_scratch();
    let indexed = dir.path().join("indexed.osm.pbf");
    let plain = dir.path().join("plain.osm.pbf");
    mixed().write(&indexed);
    let mut fixture = mixed();
    fixture.indexed = false;
    fixture.write(&plain);
    // Node phases: [MIN, B) and [B, C) need the first node blob, [C, D)
    // and [D, MAX] the second: 4. Way phases: 4 x 2. The up-front pass reads
    // the node blobs (indexed) or every blob (unindexed).
    let indexed_report = run(&indexed, false, Some(1));
    assert_eq!(indexed_report.partitions, 4);
    assert_eq!(indexed_report.blob_decodes, 2 + 4 + 8);
    let plain_report = run(&plain, false, Some(1));
    assert_eq!(plain_report.partitions, 4);
    assert_eq!(plain_report.blob_decodes, 4 + 4 + 8);
}

#[test]
fn the_number_of_partitions_never_changes_the_answer() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("mixed.osm.pbf");
    mixed().write(&path);
    for agreement in [false, true] {
        let whole = run(&path, agreement, None);
        assert_eq!(whole.partitions, 1);
        for budget in [1, 9, 17, 25] {
            let split = run(&path, agreement, Some(budget));
            assert!(split.partitions > 1, "budget {budget} must split the run");
            assert_refs_partitioned(&split);
            assert_eq!(
                format!("{:?}", comparable(split)),
                format!("{:?}", comparable(whole.clone())),
                "agreement {agreement}, budget {budget}"
            );
        }
    }
}

#[test]
fn the_mixed_fixture_reports_each_category() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("mixed.osm.pbf");
    mixed().write(&path);
    let report = run(&path, false, Some(1));
    assert_refs_partitioned(&report);
    // w22's ref to C: a valid node, but the way carries no coordinates.
    assert_eq!(report.refs_not_compared, 1);
    // w20 ref A, w21 ref D, w24 ref C.
    assert_eq!(report.refs_mismatched, 3);
    assert_eq!(report.nodes_mismatched, 3);
    // w20 ref C, w24 ref D.
    assert_eq!(report.refs_verified, 2);
    // B has two records at different coordinates; its one ref is not compared.
    assert_eq!(report.nodes_conflicting, 1);
    assert_eq!(report.refs_to_conflicting_nodes, 1);
    assert_eq!(report.ways_without_locations, 1);
    // 3 twice, A+1 twice, D+1 twice (once from the way without arrays).
    assert_eq!(report.refs_without_node, 6);
    assert_eq!(report.refs_without_node_at_null_island, 2);
    assert_eq!(report.nodes_absent, None);
    assert_eq!(report.total_violations(), 5);
    // Details come in key order: mismatches by file position, then the way
    // without coordinates, then the conflicting node.
    let kinds: Vec<_> = report.violations.iter().map(|v| v.kind.clone()).collect();
    assert_eq!(
        kinds,
        [
            LocationViolationKind::Mismatch {
                way_id: 20,
                node_id: A,
                position: 0,
                inline: (100, 201),
                node: (100, 200),
            },
            LocationViolationKind::Mismatch {
                way_id: 21,
                node_id: D,
                position: 2,
                inline: (2, 1),
                node: (1, 1),
            },
            LocationViolationKind::Mismatch {
                way_id: 24,
                node_id: C,
                position: 1,
                inline: (0, 1),
                node: (0, 0),
            },
            LocationViolationKind::WayWithoutLocations { way_id: 22 },
            LocationViolationKind::ConflictingNode { node_id: B },
        ]
    );
}

#[test]
fn agreement_mode_reports_absent_nodes_whose_refs_disagree() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("mixed.osm.pbf");
    mixed().write(&path);
    let report = run(&path, true, Some(1));
    // A+1 appears at (9,9) and (9,8); 3 and D+1 agree (D+1's arrayless ref
    // has nothing to disagree with).
    assert_eq!(report.nodes_disagreeing, 1);
    assert_eq!(report.nodes_absent, Some(3));
    assert_eq!(report.total_violations(), 6);
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == LocationViolationKind::DisagreeingRefs { node_id: A + 1 })
    );
}

/// A node every reference of which lacks coordinates is still an absent
/// node: absence is the lack of a record, not the lack of a coordinate.
#[test]
fn an_absent_node_without_any_coordinate_is_still_counted_absent() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("arrayless.osm.pbf");
    Fixture::new(vec![vec![(1, 0, 0)]], vec![vec![(5, vec![1, 2], None)]]).write(&path);
    let report = run(&path, true, None);
    assert_eq!(report.nodes_absent, Some(1));
    assert_eq!(report.ways_without_locations, 1);
}

#[test]
fn refs_without_a_node_record_are_counted_not_failed() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("absent.osm.pbf");
    Fixture::new(
        vec![vec![(1, 10, 10)]],
        vec![vec![(
            5,
            vec![1, 2, 3],
            Some(vec![(10, 10), (0, 0), (7, 7)]),
        )]],
    )
    .write(&path);
    let report = run(&path, false, None);
    assert!(report.passed(), "{report:?}");
    assert_eq!(report.refs_verified, 1);
    assert_eq!(report.refs_without_node, 2);
    assert_eq!(report.refs_without_node_at_null_island, 1);
}

/// libosmium writes `2147483647` on both axes for a location it could not
/// resolve; that is not a coordinate, so it fails a file claiming them.
#[test]
fn an_undefined_inline_location_is_a_violation() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("undefined.osm.pbf");
    Fixture::new(
        vec![vec![(1, 10, 10)]],
        vec![vec![(
            5,
            vec![1, 2],
            Some(vec![(10, 10), (i32::MAX, i32::MAX)]),
        )]],
    )
    .write(&path);
    let report = run(&path, false, None);
    assert_eq!(report.refs_invalid_inline, 1);
    assert_eq!(report.refs_verified, 1);
    assert!(!report.passed());
}

#[test]
fn unindexed_input_gets_the_same_answer() {
    let dir = crate::common::test_scratch();
    let indexed = dir.path().join("indexed.osm.pbf");
    let plain = dir.path().join("plain.osm.pbf");
    mixed().write(&indexed);
    let mut fixture = mixed();
    fixture.indexed = false;
    fixture.write(&plain);
    for budget in [None, Some(1)] {
        assert_eq!(
            format!("{:?}", comparable(run(&plain, true, budget))),
            format!("{:?}", comparable(run(&indexed, true, budget))),
        );
    }
}

#[test]
fn a_file_without_the_locations_claim_is_refused() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("plain.osm.pbf");
    let mut fixture = honest();
    fixture.header_locations = false;
    fixture.write(&path);
    let err = check_locations(&path, &opts(false, None), &crate::common::test_host())
        .expect_err("no claim to check");
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::MissingFeature(_)),
        "{err}"
    );
}

#[test]
fn a_negative_ref_is_refused() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("negative.osm.pbf");
    Fixture::new(
        vec![vec![(1, 10, 10)]],
        vec![vec![(5, vec![-1, 1], Some(vec![(10, 10), (10, 10)]))]],
    )
    .write(&path);
    let err = check_locations(&path, &opts(false, None), &crate::common::test_host())
        .expect_err("negative refs are not checked");
    assert!(err.to_string().contains("negative node ID"), "{err}");
}

fn pb_varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        #[allow(clippy::cast_possible_truncation)]
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

#[allow(clippy::cast_sign_loss)]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn pb_len(field: u32, payload: &[u8], out: &mut Vec<u8>) {
    pb_varint(u64::from((field << 3) | 2), out);
    pb_varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

#[allow(clippy::cast_sign_loss)]
fn pb_int(field: u32, value: i64, out: &mut Vec<u8>) {
    pb_varint(u64::from(field << 3), out);
    pb_varint(value as u64, out);
}

fn pb_packed_deltas(field: u32, values: &[i64], out: &mut Vec<u8>) {
    let mut seg = Vec::new();
    let mut prev = 0_i64;
    for &v in values {
        pb_varint(zigzag(v.wrapping_sub(prev)), &mut seg);
        prev = v;
    }
    pb_len(field, &seg, out);
}

/// A PrimitiveBlock with explicit framing around one group.
fn framed_block(group: &[u8], granularity: i64, lat_offset: i64, lon_offset: i64) -> Vec<u8> {
    let mut block = Vec::new();
    pb_len(1, &[0x0A, 0x00], &mut block);
    pb_len(2, group, &mut block);
    pb_int(17, granularity, &mut block);
    pb_int(19, lat_offset, &mut block);
    pb_int(20, lon_offset, &mut block);
    block
}

/// Plain nodes `(id, raw_lat, raw_lon)`.
fn raw_node_group(nodes: &[(i64, i64, i64)]) -> Vec<u8> {
    let mut group = Vec::new();
    for &(id, lat, lon) in nodes {
        let mut node = Vec::new();
        pb_varint(1 << 3, &mut node);
        pb_varint(zigzag(id), &mut node);
        pb_varint(8 << 3, &mut node);
        pb_varint(zigzag(lat), &mut node);
        pb_varint(9 << 3, &mut node);
        pb_varint(zigzag(lon), &mut node);
        pb_len(1, &node, &mut group);
    }
    group
}

/// One way with refs and raw (pre-framing) coordinates.
fn raw_way_group(id: i64, refs: &[i64], lats: &[i64], lons: &[i64]) -> Vec<u8> {
    let mut way = Vec::new();
    pb_int(1, id, &mut way);
    pb_packed_deltas(8, refs, &mut way);
    pb_packed_deltas(9, lats, &mut way);
    pb_packed_deltas(10, lons, &mut way);
    let mut group = Vec::new();
    pb_len(3, &way, &mut group);
    group
}

fn write_raw(path: &Path, blocks: &[Vec<u8>]) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let header = HeaderBuilder::new()
        .sorted()
        .optional_feature("LocationsOnWays")
        .build()
        .unwrap();
    writer.write_header(&header).unwrap();
    for block in blocks {
        writer.write_primitive_block(block).unwrap();
    }
    writer.flush().unwrap();
}

/// Node and way blocks with different non-default framings describe the
/// same place: the check compares decoded values, not raw ones. Raw-copying
/// coordinates between differently framed blocks is the hazard
/// `altw/reframe.rs` describes.
#[test]
fn coordinates_compare_after_each_block_framing() {
    let dir = crate::common::test_scratch();
    // Node 1 at 55 N, 12 E: 55e9 / 12e9 nanodegrees.
    let nodes = framed_block(&raw_node_group(&[(1, 55_000_000, 12_000_000)]), 1000, 0, 0);
    // The same place at granularity 10, offsets 1000 and -500 nanodegrees.
    let same = framed_block(
        &raw_way_group(5, &[1], &[5_499_999_900], &[1_200_000_050]),
        10,
        1000,
        -500,
    );
    let honest = dir.path().join("framed.osm.pbf");
    write_raw(&honest, &[nodes.clone(), same]);
    let report = run(&honest, false, None);
    assert!(report.passed(), "{report:?}");
    assert_eq!(report.refs_verified, 1);

    // The raw way value copied unscaled from the node block: what a writer
    // that forwards raw deltas across framings produces.
    let copied = framed_block(
        &raw_way_group(5, &[1], &[55_000_000], &[12_000_000]),
        10,
        1000,
        -500,
    );
    let stale = dir.path().join("copied.osm.pbf");
    write_raw(&stale, &[nodes, copied]);
    let report = run(&stale, false, None);
    assert_eq!(report.refs_mismatched, 1);
    assert!(
        matches!(
            report.violations[0].kind,
            LocationViolationKind::Mismatch {
                inline: (5_500_010, 1_199_995),
                node: (550_000_000, 120_000_000),
                ..
            }
        ),
        "{:?}",
        report.violations
    );
}

#[test]
fn an_out_of_domain_node_record_is_a_violation_and_its_refs_are_not_compared() {
    let dir = crate::common::test_scratch();
    // Latitude 100 degrees.
    let nodes = framed_block(
        &raw_node_group(&[(1, 1_000_000_000, 0), (2, 0, 0)]),
        100,
        0,
        0,
    );
    // Way 6 carries no coordinate arrays: its reference to the bad node is
    // still a reference to it.
    let mut group = raw_way_group(5, &[1, 2], &[0, 0], &[0, 0]);
    group.extend(raw_way_group(6, &[1], &[], &[]));
    let ways = framed_block(&group, 100, 0, 0);
    let path = dir.path().join("bad-node.osm.pbf");
    write_raw(&path, &[nodes, ways]);
    let report = run(&path, false, None);
    assert_eq!(report.invalid_node_records, 1);
    assert_eq!(report.refs_to_invalid_nodes, 2);
    assert_eq!(report.ways_without_locations, 1);
    assert_eq!(report.refs_verified, 1);
    assert_eq!(report.refs_without_node, 0);
    assert!(!report.passed());
}

/// The detail list is the `max_errors` smallest violations by file
/// position, whatever the partitioning.
#[test]
fn retained_details_are_the_first_in_file_order() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("many.osm.pbf");
    let nodes: Vec<(i64, i32, i32)> = (1..=40).map(|id| (id, 0, 0)).collect();
    let ways: Vec<Way> = (1..=40)
        .map(|id| (100 + id, vec![id], Some(vec![(1, 1)])))
        .collect();
    Fixture::new(vec![nodes], vec![ways[..20].to_vec(), ways[20..].to_vec()]).write(&path);
    for budget in [None, Some(1), Some(50)] {
        let mut o = opts(false, budget);
        o.check.max_errors = 3;
        let report = check_locations(&path, &o, &crate::common::test_host()).unwrap();
        assert_eq!(report.refs_mismatched, 40);
        let ways: Vec<i64> = report
            .violations
            .iter()
            .map(|v| match v.kind {
                LocationViolationKind::Mismatch { way_id, .. } => way_id,
                ref other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(ways, [101, 102, 103], "budget {budget:?}");
    }
}

/// Conflicting nodes in different partitions, with a cap below their count:
/// the retained ones are the smallest IDs whatever the partitioning.
#[test]
fn capped_node_details_are_the_smallest_ids_across_partitions() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("conflicts.osm.pbf");
    let ids = [A, B, C, D];
    let nodes = ids
        .iter()
        .flat_map(|&id| [(id, 1, 1), (id, 2, 2)])
        .collect();
    Fixture::new(
        vec![nodes],
        vec![vec![(5, ids.to_vec(), Some(vec![(1, 1); 4]))]],
    )
    .write(&path);
    for budget in [None, Some(1), Some(9)] {
        let mut o = opts(false, budget);
        o.check.max_errors = 2;
        let report = check_locations(&path, &o, &crate::common::test_host()).unwrap();
        assert_eq!(report.nodes_conflicting, 4);
        assert_eq!(report.refs_to_conflicting_nodes, 4);
        let kinds: Vec<_> = report.violations.iter().map(|v| v.kind.clone()).collect();
        assert_eq!(
            kinds,
            [
                LocationViolationKind::ConflictingNode { node_id: A },
                LocationViolationKind::ConflictingNode { node_id: B },
            ],
            "budget {budget:?}"
        );
    }
}

#[test]
fn a_history_file_is_refused() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("history.osm.pbf");
    let mut fixture = honest();
    fixture.historical = true;
    fixture.write(&path);
    let err = check_locations(&path, &opts(false, None), &crate::common::test_host())
        .expect_err("several versions of a node legitimately disagree");
    assert!(err.to_string().contains("history files"), "{err}");
}

/// The agreement pass reads every reference before the partitions do and
/// does not key a negative one; the first partition's way pass still
/// refuses it, as without agreement.
#[test]
fn a_negative_ref_is_refused_in_agreement_mode() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("negative.osm.pbf");
    Fixture::new(
        vec![vec![(1, 10, 10)]],
        vec![vec![(5, vec![1, -1], Some(vec![(10, 10), (10, 10)]))]],
    )
    .write(&path);
    let err = check_locations(&path, &opts(true, Some(1)), &crate::common::test_host())
        .expect_err("negative refs are not checked");
    assert!(err.to_string().contains("negative node ID"), "{err}");
}

/// Indexdata that understates a node blob's ID range must not hide a node
/// from the partition that owns it. Routing follows the IDs the up-front
/// pass decoded, so the stale coordinate of node 100 is found although the
/// blob claims to end at 50.
#[test]
fn an_understated_node_range_hides_nothing() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("honest-range.osm.pbf");
    Fixture::new(
        vec![vec![(1, 0, 0), (100, 5, 5)]],
        vec![vec![(5, vec![1, 100], Some(vec![(0, 0), (6, 6)]))]],
    )
    .write(&path);
    let bytes = std::fs::read(&path).unwrap();
    // Frame 0 is the header, frame 1 the node blob; indexdata bytes 10..18
    // are max_id.
    let lying = crate::common::adversarial::mutate_blob_header_indexdata(&bytes, 1, |index| {
        index[10..18].copy_from_slice(&50_i64.to_le_bytes());
    });
    // Read the claim back: the frame really is the node blob and the file
    // really claims to end at 50 (the closure only reads).
    let mut claimed = None;
    crate::common::adversarial::mutate_blob_header_indexdata(&lying, 1, |index| {
        claimed = Some((
            index[1],
            i64::from_le_bytes(index[10..18].try_into().unwrap()),
        ));
    });
    assert_eq!(claimed, Some((0, 50)), "kind 0 (node), max_id 50");
    let lying_path = dir.path().join("lying-range.osm.pbf");
    std::fs::write(&lying_path, lying).unwrap();
    for budget in [None, Some(1)] {
        let report = run(&lying_path, false, budget);
        assert_eq!(report.refs_mismatched, 1, "budget {budget:?}: {report:?}");
        assert_eq!(report.refs_without_node, 0, "budget {budget:?}");
    }
    assert!(run(&lying_path, false, Some(1)).partitions > 1);
}

/// A block holding a string table (`entries`) and one group.
fn block_with(entries: &[&[u8]], group: &[u8]) -> Vec<u8> {
    let mut table = Vec::new();
    for entry in entries {
        pb_len(1, entry, &mut table);
    }
    let mut block = Vec::new();
    pb_len(1, &table, &mut block);
    pb_len(2, group, &mut block);
    block
}

/// A way with refs `[1]` at `(0, 0)`, optionally with a `keys` array and no
/// `vals` array.
fn way_group(unpaired_key: bool) -> Vec<u8> {
    let mut way = Vec::new();
    pb_int(1, 5, &mut way);
    if unpaired_key {
        pb_len(2, &[1], &mut way);
    }
    pb_packed_deltas(8, &[1], &mut way);
    pb_packed_deltas(9, &[0], &mut way);
    pb_packed_deltas(10, &[0], &mut way);
    let mut group = Vec::new();
    pb_len(3, &way, &mut group);
    group
}

/// Node 1 at `(0, 0)` as a plain node, optionally with an `Info` whose only
/// field is a truncated varint.
fn plain_node_group(truncated_info: bool) -> Vec<u8> {
    let mut node = Vec::new();
    pb_varint(1 << 3, &mut node);
    pb_varint(zigzag(1), &mut node);
    if truncated_info {
        pb_len(4, &[0x08], &mut node);
    }
    pb_varint(8 << 3, &mut node);
    pb_varint(0, &mut node);
    pb_varint(9 << 3, &mut node);
    pb_varint(0, &mut node);
    let mut group = Vec::new();
    pb_len(1, &node, &mut group);
    group
}

/// Node 1 at `(0, 0)` as a dense node, optionally with a `DenseInfo` that
/// carries a version array and none of the other metadata arrays.
fn dense_node_group(partial_info: bool) -> Vec<u8> {
    let mut dense = Vec::new();
    pb_packed_deltas(1, &[1], &mut dense);
    if partial_info {
        let mut info = Vec::new();
        pb_len(1, &[1], &mut info);
        pb_len(5, &info, &mut dense);
    }
    pb_packed_deltas(8, &[0], &mut dense);
    pb_packed_deltas(9, &[0], &mut dense);
    let mut group = Vec::new();
    pb_len(2, &dense, &mut group);
    group
}

/// The scanners read only IDs, references and coordinates, but a blob the
/// reader refuses to build is refused here too: every blob is validated once
/// as the reader validates it. Each malformed fixture has an honest twin
/// that must pass, written through the same writer, so the refusal is the
/// malformation's. Both writers matter: indexed, a way blob claims ways and
/// is validated in the first partition's way pass; unindexed, every blob is
/// validated in the up-front pass.
#[test]
fn blobs_the_reader_refuses_are_refused() {
    let dir = crate::common::test_scratch();
    let honest_way = block_with(&[b""], &way_group(false));
    // (what, node block, way block, honest twin of the malformed block)
    type Case = (&'static str, Vec<u8>, Vec<u8>, Vec<u8>);
    let wire = |kind: &pbfhogg::ErrorKind| matches!(kind, pbfhogg::ErrorKind::WireFormat { .. });
    let utf8 =
        |kind: &pbfhogg::ErrorKind| matches!(kind, pbfhogg::ErrorKind::StringtableUtf8 { .. });
    let expected: [&dyn Fn(&pbfhogg::ErrorKind) -> bool; 4] = [&wire, &wire, &wire, &utf8];
    let cases: [Case; 4] = [
        (
            "way keys without vals",
            block_with(&[b""], &plain_node_group(false)),
            block_with(&[b"", b"k"], &way_group(true)),
            block_with(&[b"", b"k"], &way_group(false)),
        ),
        (
            "node Info with a truncated varint",
            block_with(&[b""], &plain_node_group(true)),
            honest_way.clone(),
            block_with(&[b""], &plain_node_group(false)),
        ),
        (
            "DenseInfo with some of its arrays",
            block_with(&[b""], &dense_node_group(true)),
            honest_way.clone(),
            block_with(&[b""], &dense_node_group(false)),
        ),
        (
            "string table entry that is not UTF-8",
            block_with(&[b"", &[0xff]], &plain_node_group(false)),
            honest_way.clone(),
            block_with(&[b"", b"ok"], &plain_node_group(false)),
        ),
    ];
    for ((what, nodes, ways, honest_twin), expected) in cases.into_iter().zip(expected) {
        // The twin replaces whichever block is malformed.
        let honest_blocks = if ways == honest_way {
            [honest_twin, ways.clone()]
        } else {
            [nodes.clone(), honest_twin]
        };
        type Writer = fn(&Path, &[Vec<u8>]);
        let writers: [(&str, Writer); 2] =
            [("indexed", write_raw), ("unindexed", write_raw_unindexed)];
        for (how, write) in writers {
            let honest_path = dir.path().join("twin.osm.pbf");
            write(&honest_path, &honest_blocks);
            let report = run(&honest_path, false, None);
            assert!(
                report.passed() && report.refs_verified == 1,
                "{what}, {how} twin: {report:?}"
            );

            let path = dir.path().join("malformed.osm.pbf");
            write(&path, &[nodes.clone(), ways.clone()]);
            let err = check_locations(&path, &opts(false, None), &crate::common::test_host())
                .expect_err(what);
            assert!(expected(err.kind()), "{what}, {how}: {err}");
        }
    }
}

/// Like [`write_raw`], without indexdata: the index scanner is not what is
/// under test, and it need not accept a block the reader refuses.
fn write_raw_unindexed(path: &Path, blocks: &[Vec<u8>]) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let header = HeaderBuilder::new()
        .sorted()
        .optional_feature("LocationsOnWays")
        .build()
        .unwrap();
    writer.write_header(&header).unwrap();
    for block in blocks {
        writer.write_primitive_block_no_indexdata(block).unwrap();
    }
    writer.flush().unwrap();
}
