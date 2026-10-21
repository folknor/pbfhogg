//! Edge-case and boundary coverage.
//!
//! Each test pins graceful behaviour - correct output or an explicit
//! error - on an input shape that the main test suite rarely or
//! never exercises: empty PBFs, zero-ref ways, zero-member relations,
//! empty-string tag keys, negative ids.
//!
//! These tests are cheap to add and often catch assertion panics in
//! pre-allocated code paths or off-by-one errors in block-level
//! bookkeeping.

use crate::common::{
    TestMember, TestNode, TestRelation, TestWay, read_normalized, write_test_pbf_sorted,
};
use pbfhogg::block_builder;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobReader, BlobType, ElementReader, MemberId};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Empty PBF (header only, no data blobs)
// ---------------------------------------------------------------------------

fn write_empty_pbf(path: &std::path::Path) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(64 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");
    writer.flush().expect("flush");
}

#[test]
fn empty_pbf_reads_cleanly() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("empty.osm.pbf");
    write_empty_pbf(&path);

    // BlobReader should emit exactly one blob (the header) and then terminate.
    let reader = BlobReader::from_path(&path).expect("open");
    let mut seen_types: Vec<&'static str> = Vec::new();
    for b in reader {
        let blob = b.expect("read blob");
        seen_types.push(match blob.get_type() {
            BlobType::OsmHeader => "OsmHeader",
            BlobType::OsmData => "OsmData",
            _ => "Unknown",
        });
    }
    assert_eq!(seen_types, vec!["OsmHeader"]);

    // ElementReader::for_each must traverse with zero callbacks.
    let mut count = 0usize;
    let rdr = ElementReader::from_path(&path).expect("open element reader");
    rdr.for_each(|_| count += 1).expect("for_each");
    assert_eq!(count, 0, "empty PBF must yield zero elements");

    // Normalized view: all three sections empty.
    let c = read_normalized(&path);
    assert_eq!(c.nodes.len(), 0);
    assert_eq!(c.ways.len(), 0);
    assert_eq!(c.relations.len(), 0);
}

/// Running a re-write command (`sort`) on an empty PBF must produce
/// another empty PBF, not panic and not produce a malformed file.
#[test]
fn empty_pbf_survives_sort() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("empty.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");
    write_empty_pbf(&input);

    let opts = pbfhogg::sort::SortOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
    };
    pbfhogg::commands::sort::sort(
        &input,
        &output,
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("sort empty");

    let c = read_normalized(&output);
    assert_eq!(c.nodes.len(), 0);
    assert_eq!(c.ways.len(), 0);
    assert_eq!(c.relations.len(), 0);
}

// ---------------------------------------------------------------------------
// Zero-ref ways / zero-member relations
// ---------------------------------------------------------------------------

#[test]
fn zero_ref_way_roundtrips() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("noref.osm.pbf");

    write_test_pbf_sorted(
        &path,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![],
            tags: vec![("note", "empty")],
            meta: None,
        }],
        &[],
    );

    let c = read_normalized(&path);
    assert_eq!(c.ways.len(), 1);
    assert_eq!(c.ways[0].id, 10);
    assert_eq!(
        c.ways[0].refs.len(),
        0,
        "zero-ref way must round-trip with empty refs"
    );
    // Tag must still be present.
    assert_eq!(
        c.ways[0].tags.get("note").map(String::as_str),
        Some("empty")
    );
}

#[test]
fn zero_member_relation_roundtrips() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("nomember.osm.pbf");

    write_test_pbf_sorted(
        &path,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![],
            tags: vec![("type", "empty")],
            meta: None,
        }],
    );

    let c = read_normalized(&path);
    assert_eq!(c.relations.len(), 1);
    assert_eq!(c.relations[0].id, 100);
    assert_eq!(
        c.relations[0].members.len(),
        0,
        "zero-member relation must round-trip with empty members"
    );
    assert_eq!(
        c.relations[0].tags.get("type").map(String::as_str),
        Some("empty")
    );
}

// ---------------------------------------------------------------------------
// Empty-string tag keys / values
// ---------------------------------------------------------------------------

#[test]
fn empty_string_tag_value_roundtrips() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("emptytag.osm.pbf");

    write_test_pbf_sorted(
        &path,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("empty_value", "")],
            meta: None,
        }],
        &[],
        &[],
    );

    let c = read_normalized(&path);
    assert_eq!(c.nodes.len(), 1);
    assert_eq!(
        c.nodes[0].tags.get("empty_value").map(String::as_str),
        Some(""),
        "empty-string tag value must round-trip as \"\" (not dropped)"
    );
}

/// `PbfWriter`'s `StringTable` reserves index 0 for the empty string.
/// Verify that empty string tag KEYS don't collide with the reserved
/// slot - they should either round-trip or raise an error, not
/// silently corrupt.
#[test]
fn empty_string_tag_key_behaviour() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("emptykey.osm.pbf");

    write_test_pbf_sorted(
        &path,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("", "value")],
            meta: None,
        }],
        &[],
        &[],
    );

    // The fixture writer shouldn't panic; what the reader observes is
    // the pin. Accept either behaviour (empty key preserved, or
    // dropped as invalid) - don't silently corrupt ids/other tags.
    let c = read_normalized(&path);
    assert_eq!(c.nodes.len(), 1);
    assert_eq!(
        c.nodes[0].id, 1,
        "node id must be preserved whatever the tag fate"
    );
}

// ---------------------------------------------------------------------------
// Relation referencing another relation (transitive)
// ---------------------------------------------------------------------------

#[test]
fn relation_referencing_relation_roundtrips() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("rel_of_rel.osm.pbf");

    write_test_pbf_sorted(
        &path,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember {
                    id: MemberId::Node(1),
                    role: "pin",
                }],
                tags: vec![("type", "leaf")],
                meta: None,
            },
            TestRelation {
                id: 200,
                members: vec![TestMember {
                    id: MemberId::Relation(100),
                    role: "child",
                }],
                tags: vec![("type", "super")],
                meta: None,
            },
        ],
    );

    let c = read_normalized(&path);
    assert_eq!(c.relations.len(), 2);
    let super_rel = c
        .relations
        .iter()
        .find(|r| r.id == 200)
        .expect("super relation");
    assert_eq!(super_rel.members.len(), 1);
    assert_eq!(super_rel.members[0].member_type, "relation");
    assert_eq!(super_rel.members[0].ref_id, 100);
}

// ---------------------------------------------------------------------------
// Large / negative IDs
// ---------------------------------------------------------------------------

/// OSM uses non-negative ids in production, but the PBF protobuf
/// encoding permits i64 values including negatives. Test that a
/// large positive id near the OSM production ceiling round-trips.
/// (Negative ids are rejected by `renumber_external` explicitly per
/// CHANGELOG; roundtrip at the reader/writer layer should still work.)
#[test]
fn large_positive_id_roundtrips() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("bigid.osm.pbf");

    let big_id: i64 = 12_345_678_901; // well above current osm ceiling
    write_test_pbf_sorted(
        &path,
        &[TestNode {
            id: big_id,
            lat: 500_000_000,
            lon: 100_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let c = read_normalized(&path);
    assert_eq!(c.nodes.len(), 1);
    assert_eq!(c.nodes[0].id, big_id);
}

// ---------------------------------------------------------------------------
// BlockBuilder flush boundary (last element slot)
// ---------------------------------------------------------------------------

/// BlockBuilder is capped at 8 000 entities per block. A block that
/// ends exactly at 8 000 elements should flush cleanly and accept a
/// new element for the next block. Pin that by writing 8 001 nodes
/// via the single writer and verifying they all round-trip.
#[test]
fn block_builder_capacity_boundary() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("cap.osm.pbf");
    let nodes = crate::common::generate_nodes(8_001, 1);
    write_test_pbf_sorted(&path, &nodes, &[], &[]);

    let c = read_normalized(&path);
    assert_eq!(c.nodes.len(), 8_001);
    assert_eq!(c.nodes[0].id, 1);
    assert_eq!(c.nodes[8_000].id, 8_001);

    // Should be exactly two data blobs: one full (8_000) + one with 1.
    let mut data_blobs = 0usize;
    for b in BlobReader::from_path(&path).expect("open") {
        let blob = b.expect("read blob");
        if matches!(blob.get_type(), BlobType::OsmData) {
            data_blobs += 1;
        }
    }
    assert_eq!(
        data_blobs, 2,
        "expected 2 data blobs for 8001 nodes at cap 8000"
    );
}

// ---------------------------------------------------------------------------
// add-locations-to-ways: a refused run must publish nothing
// ---------------------------------------------------------------------------

/// A fixture whose single way references a node that is not in the file, so
/// the backend reports one missing location and the default policy refuses
/// the run.
fn write_unresolvable_way_pbf(path: &std::path::Path) {
    let nodes = vec![TestNode {
        id: 1,
        lat: 550_000_000,
        lon: 120_000_000,
        tags: vec![("name", "present")],
        meta: None,
    }];
    let ways = vec![TestWay {
        id: 10,
        // Node 999 is absent from the file.
        refs: vec![1, 999],
        tags: vec![("highway", "primary")],
        meta: None,
    }];
    write_test_pbf_sorted(path, &nodes, &ways, &[]);
}

fn altw_options() -> pbfhogg::altw::AltwOptions {
    pbfhogg::altw::AltwOptions {
        keep_untagged_nodes: true,
        compression: Compression::default(),
        direct_io: false,
        force: true,
        index_type: pbfhogg::altw::IndexType::Sparse,
        inject_prepass: false,
        ignore_missing_nodes: false,
    }
}

/// The library entry point, not the CLI.
///
/// The missing-locations gate can only run after a complete output exists -
/// the external backend does not know its resolved count until its last
/// stage - so the write happens before the refusal. Under the CLI that is
/// already covered: `guarded_output` hands the command a scratch sibling and
/// never renames it into place on a failing exit. A library caller has no
/// such front end, and used to be handed an error together with a
/// well-formed PBF at the path it named, full of fabricated `(0, 0)`
/// vertices that no reader can tell from genuine data.
#[test]
fn altw_refusal_writes_no_output_for_a_library_caller() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_unresolvable_way_pbf(&input);

    // `Stats` is not `Debug`, so no `expect_err` here.
    let Err(err) = pbfhogg::altw::add_locations_to_ways(
        &input,
        &output,
        &altw_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    ) else {
        panic!("an unresolvable way ref must fail the default policy");
    };
    assert!(
        err.to_string().contains("could not be resolved"),
        "expected the missing-locations guard specifically. Got: {err}"
    );
    assert!(
        !output.exists(),
        "a refused run must not leave a plausible-looking output behind"
    );

    // Passing counterpart: the same input with the policy waived does
    // publish an output, so the assertion above is not satisfied by a
    // command that simply never writes anything.
    let mut opts = altw_options();
    opts.ignore_missing_nodes = true;
    pbfhogg::altw::add_locations_to_ways(
        &input,
        &output,
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("--ignore-missing-nodes must accept the input");
    assert!(output.exists(), "an accepted run must publish its output");
}

/// The overwrite half: a library caller pointing at an existing file must
/// get that file back byte-identical when the run is refused, not a
/// truncated or replaced one. Opening the destination for write would have
/// destroyed it before the gate ever ran.
#[test]
fn altw_refusal_leaves_a_preexisting_destination_intact() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("precious.osm.pbf");
    write_unresolvable_way_pbf(&input);
    std::fs::write(&output, b"user data").expect("write pre-existing destination");

    assert!(
        pbfhogg::altw::add_locations_to_ways(
            &input,
            &output,
            &altw_options(),
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host()
        )
        .is_err(),
        "an unresolvable way ref must fail the default policy"
    );

    assert_eq!(
        std::fs::read(&output).expect("destination still readable"),
        b"user data",
        "a refused run must leave the pre-existing destination untouched"
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .filter(|n| n != "precious.osm.pbf" && n != "in.osm.pbf")
        .collect();
    assert!(leftovers.is_empty(), "scratch file leaked: {leftovers:?}");
}
