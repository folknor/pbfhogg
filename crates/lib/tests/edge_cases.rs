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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("empty.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");
    write_empty_pbf(&input);

    // No `force`: a header-only file has no blob lacking indexdata.
    pbfhogg::commands::sort::sort(
        &input,
        crate::common::out(&output),
        &pbfhogg::write::WriteOptions::default(),
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
    let dir = crate::common::test_scratch();
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
    assert_eq!(c.ways[0].tag("note"), Some("empty"));
}

#[test]
fn zero_member_relation_roundtrips() {
    let dir = crate::common::test_scratch();
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
    assert_eq!(c.relations[0].tag("type"), Some("empty"));
}

// ---------------------------------------------------------------------------
// Empty-string tag keys / values
// ---------------------------------------------------------------------------

#[test]
fn empty_string_tag_value_roundtrips() {
    let dir = crate::common::test_scratch();
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
        c.nodes[0].tag("empty_value"),
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
        write: pbfhogg::write::WriteOptions {
            force: true,
            ..Default::default()
        },
        index_type: pbfhogg::altw::IndexType::Sparse,
        inject_prepass: false,
        ignore_missing_nodes: false,
    }
}

/// The library entry point, not the CLI.
///
/// The missing-locations gate can only run after a complete output exists -
/// the external backend does not know its resolved count until its last
/// stage - so the write happens before the refusal. A library caller used
/// to be handed an error together with a well-formed PBF at the path it
/// named, full of fabricated `(0, 0)` vertices that no reader can tell from
/// genuine data. The output's staging file is what keeps it out now: the
/// entry point publishes only after the gate passes.
#[test]
fn altw_refusal_writes_no_output_for_a_library_caller() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_unresolvable_way_pbf(&input);

    // `Stats` is not `Debug`, so no `expect_err` here.
    let Err(err) = pbfhogg::altw::add_locations_to_ways(
        &input,
        crate::common::out(&output),
        &altw_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
        crate::common::scratch_in(dir.path()),
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
        crate::common::out(&output),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
        crate::common::scratch_in(dir.path()),
    )
    .expect("--ignore-missing-nodes must accept the input");
    assert!(output.exists(), "an accepted run must publish its output");
}

/// add-locations-to-ways counts unresolved references, not nodes: one absent
/// node used three times across two ways is three, on both index types,
/// and the refusal says so.
#[test]
fn altw_refusal_counts_each_unresolved_reference() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    let nodes = vec![TestNode {
        id: 1,
        lat: 550_000_000,
        lon: 120_000_000,
        tags: vec![("name", "present")],
        meta: None,
    }];
    let ways = vec![
        TestWay {
            id: 10,
            refs: vec![1, 999, 999],
            tags: vec![("highway", "primary")],
            meta: None,
        },
        TestWay {
            id: 11,
            refs: vec![999, 1],
            tags: vec![("highway", "primary")],
            meta: None,
        },
    ];
    write_test_pbf_sorted(&input, &nodes, &ways, &[]);
    for index_type in [
        pbfhogg::altw::IndexType::Sparse,
        pbfhogg::altw::IndexType::External,
    ] {
        let mut opts = altw_options();
        opts.index_type = index_type;
        // External takes indexed input only and refuses `force`.
        opts.write.force = matches!(index_type, pbfhogg::altw::IndexType::Sparse);
        let Err(err) = pbfhogg::altw::add_locations_to_ways(
            &input,
            crate::common::out(&output),
            &opts,
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
            crate::common::scratch_in(dir.path()),
        ) else {
            panic!("{index_type:?}: an unresolvable way ref must fail the default policy");
        };
        assert!(
            matches!(
                err.kind(),
                pbfhogg::ErrorKind::MissingNodeLocations {
                    count: 3,
                    counted: pbfhogg::Unresolved::WayNodeReferences,
                }
            ),
            "{index_type:?}: expected three unresolved references, got: {err:?}"
        );
        assert!(
            err.to_string()
                .contains("3 way node references (each use of a node in each way counted)"),
            "{index_type:?}: the message must say what it counts: {err}"
        );
        assert!(!output.exists(), "{index_type:?}: nothing may be published");
    }
}

/// The overwrite half: a library caller pointing at an existing file must
/// get that file back byte-identical when the run is refused, not a
/// truncated or replaced one. Opening the destination for write would have
/// destroyed it before the gate ever ran.
#[test]
fn altw_refusal_leaves_a_preexisting_destination_intact() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("precious.osm.pbf");
    write_unresolvable_way_pbf(&input);
    std::fs::write(&output, b"user data").expect("write pre-existing destination");

    assert!(
        pbfhogg::altw::add_locations_to_ways(
            &input,
            crate::common::out_replace(&output),
            &altw_options(),
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
            crate::common::scratch_in(dir.path())
        )
        .is_err(),
        "an unresolvable way ref must fail the default policy"
    );

    assert_eq!(
        std::fs::read(&output).expect("destination still readable"),
        b"user data",
        "a refused run must leave the pre-existing destination untouched"
    );
    assert!(
        dir.path_state(&output)
            .expect("inspect destination")
            .expect("destination")
            .is_file()
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .map(|e| e.expect("read scratch directory entry").file_name())
        .filter(|n| n != "precious.osm.pbf" && n != "in.osm.pbf")
        .collect();
    assert!(leftovers.is_empty(), "scratch file leaked: {leftovers:?}");
}

// These are controls for the output assertions themselves: a plausible empty
// normalized value must not hide an absent header or an incomplete frame.
#[test]
fn normalized_output_requires_header_and_complete_framing() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("output.pbf");
    std::fs::write(&path, b"").expect("empty output");
    assert!(std::panic::catch_unwind(|| read_normalized(&path)).is_err());

    let mut block = block_builder::BlockBuilder::new();
    block.add_node(1, 0, 0, [], None);
    {
        let file = std::fs::File::create(&path).expect("create headerless output");
        let mut writer = PbfWriter::new(file, Compression::default());
        writer
            .write_primitive_block(block.take().expect("take").expect("block"))
            .expect("write headerless data");
        writer.flush().expect("flush");
    }
    assert!(std::panic::catch_unwind(|| read_normalized(&path)).is_err());

    write_empty_pbf(&path);
    let valid = std::fs::read(&path).expect("header bytes");
    read_normalized(&path);
    for tail in [&[0u8][..], &[0, 0][..], &[0, 0, 0][..], &[0, 0, 0, 1][..]] {
        let mut bytes = valid.clone();
        bytes.extend_from_slice(tail);
        std::fs::write(&path, bytes).expect("malformed tail");
        assert!(std::panic::catch_unwind(|| read_normalized(&path)).is_err());
    }
    std::fs::write(&path, &valid[..valid.len() - 1]).expect("truncated payload");
    assert!(std::panic::catch_unwind(|| read_normalized(&path)).is_err());
}

/// Write one element per blob so the controls cover both kind transitions and
/// cross-blob ID ordering without a fixture writer rearranging the sequence.
fn write_order_control(path: &std::path::Path, elements: &[(u8, i64)]) {
    let file = std::fs::File::create(path).expect("create ordering control");
    let mut writer = PbfWriter::new(file, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .expect("header");
    writer.write_header(&header).expect("write header");
    for &(kind, id) in elements {
        let mut block = block_builder::BlockBuilder::new();
        // Untagged on purpose: an id-0 element with no other content must
        // still be written and read back.
        let tags: [(&str, &str); 0] = [];
        match kind {
            0 => {
                block.add_node(id, 0, 0, tags, None);
            }
            1 => {
                block.add_way(id, tags, &[], None);
            }
            2 => {
                block.add_relation(id, tags, &[], None);
            }
            _ => panic!("unknown control kind"),
        }
        writer
            .write_primitive_block(block.take().expect("take").expect("block"))
            .expect("write control block");
    }
    writer.flush().expect("flush");
}

/// An id-0 node, way and relation with no tags, refs, members or metadata
/// encode with no other content than their id; each must read back.
#[test]
fn all_default_id_zero_elements_round_trip() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("id0.pbf");
    write_order_control(&path, &[(0, 0), (1, 0), (2, 0)]);
    let c = read_normalized(&path);
    assert_eq!(c.nodes.len(), 1, "node");
    assert_eq!(c.ways.len(), 1, "way");
    assert_eq!(c.relations.len(), 1, "relation");
    let mut ids = Vec::new();
    ElementReader::from_path(&path)
        .expect("open element reader")
        .for_each(|element| {
            ids.push(match element {
                pbfhogg::Element::Node(n) => n.id(),
                pbfhogg::Element::DenseNode(n) => n.id(),
                pbfhogg::Element::Way(w) => w.id(),
                pbfhogg::Element::Relation(r) => r.id(),
                _ => panic!("unexpected element kind"),
            });
        })
        .expect("for_each");
    assert_eq!(ids, [0, 0, 0]);
}

#[test]
fn sorted_output_assertion_checks_stream_and_canonical_ids() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("order.pbf");
    let mut canonical = Vec::new();
    for kind in 0..3 {
        canonical.extend([0, -1, -2, i64::MIN, 1, 2, i64::MAX].map(|id| (kind, id)));
    }
    write_order_control(&path, &canonical);
    crate::common::assert_sorted_file(&path);
    for invalid in [
        vec![(1, 1), (0, 1)],
        vec![(0, 1), (1, 1), (0, 2)],
        vec![(2, 1), (1, 1)],
        vec![(0, 2), (0, 1)],
        vec![(0, -2), (0, -1)],
        vec![(1, 1), (1, -1)],
        vec![(2, -1), (2, 0)],
    ] {
        write_order_control(&path, &invalid);
        assert!(
            std::panic::catch_unwind(|| crate::common::assert_sorted_file(&path)).is_err(),
            "accepted out-of-order control: {invalid:?}"
        );
    }
}

#[test]
fn fixture_tags_borrow_local_strings_for_every_kind() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("borrowed-tags.pbf");
    let key = String::from("local-key");
    let value = String::from("local-value");
    write_test_pbf_sorted(
        &path,
        &[TestNode {
            id: 1,
            lat: 0,
            lon: 0,
            tags: vec![(key.as_str(), value.as_str())],
            meta: None,
        }],
        &[TestWay {
            id: 1,
            refs: vec![1],
            tags: vec![(key.as_str(), value.as_str())],
            meta: None,
        }],
        &[TestRelation {
            id: 1,
            members: vec![],
            tags: vec![(key.as_str(), value.as_str())],
            meta: None,
        }],
    );
    let contents = read_normalized(&path);
    for tags in [
        &contents.nodes[0].tags,
        &contents.ways[0].tags,
        &contents.relations[0].tags,
    ] {
        assert_eq!(tags, &vec![(key.clone(), value.clone())]);
    }
}
