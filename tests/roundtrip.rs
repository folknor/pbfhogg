//! Round-trip tests: write PBF → read back → verify.
#![allow(clippy::unwrap_used, clippy::cognitive_complexity, clippy::too_many_lines)]

use pbfhogg::block_builder::{self, BlockBuilder, MemberData, Metadata};
use pbfhogg::MemberId;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};
use std::io::Cursor;

/// Write a PBF with known nodes (all with metadata), read back, verify every field.
#[test]
fn roundtrip_dense_nodes_with_metadata() {
    let mut buf = Vec::new();

    // Write
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::default());

        let header_bytes = block_builder::build_header(
            Some((9.0, 54.0, 13.0, 58.0)),
            Some(1_700_000_000),
            Some(42),
            Some("https://example.com/replication"),
            &[],
        )
        .unwrap();
        writer.write_header(&header_bytes).unwrap();

        let mut bb = BlockBuilder::new();

        // Node 1: with tags and metadata
        bb.add_node(
            1001,
            556_789_000, // 55.6789°
            126_543_000, // 12.6543°
            &[("name", "TestNode"), ("highway", "bus_stop")],
            Some(&Metadata {
                version: 3,
                timestamp: 1_700_000_000,
                changeset: 12345,
                uid: 42,
                user: "testuser",
                visible: true,
            }),
        );

        // Node 2: tagless, with metadata
        bb.add_node(
            2002,
            570_000_000,
            100_000_000,
            &[],
            Some(&Metadata {
                version: 1,
                timestamp: 1_600_000_000,
                changeset: 9999,
                uid: 7,
                user: "mapper",
                visible: true,
            }),
        );

        // Node 3: southern/western hemisphere, with metadata
        bb.add_node(
            3003,
            -335_000_000,
            -580_000_000,
            &[("natural", "tree")],
            Some(&Metadata {
                version: 2,
                timestamp: 1_500_000_000,
                changeset: 5555,
                uid: 100,
                user: "botanist",
                visible: true,
            }),
        );

        let block_bytes = bb.take().unwrap().unwrap();
        writer.write_primitive_block(&block_bytes).unwrap();
        writer.flush().unwrap();
    }

    // Read back
    let reader = BlobReader::new(Cursor::new(&buf));
    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(blobs.len(), 2, "expected header + 1 data blob");

    // Verify header
    let header = match blobs[0].decode().unwrap() {
        BlobDecode::OsmHeader(h) => h,
        _ => panic!("expected OsmHeader"),
    };

    let bbox = header.bbox().unwrap();
    assert!((bbox.left - 9.0).abs() < 1e-6);
    assert!((bbox.bottom - 54.0).abs() < 1e-6);
    assert!((bbox.right - 13.0).abs() < 1e-6);
    assert!((bbox.top - 58.0).abs() < 1e-6);
    assert_eq!(header.osmosis_replication_timestamp(), Some(1_700_000_000));
    assert_eq!(header.osmosis_replication_sequence_number(), Some(42));
    assert_eq!(
        header.osmosis_replication_base_url(),
        Some("https://example.com/replication")
    );
    assert_eq!(header.writing_program(), Some("pbfhogg"));

    let features: Vec<&str> = header
        .required_features()
        .iter()
        .map(|f| f.as_ref())
        .collect();
    assert!(features.contains(&"OsmSchema-V0.6"));
    assert!(features.contains(&"DenseNodes"));

    // Verify data block
    let prim = match blobs[1].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };

    let elements: Vec<_> = prim.elements().collect();
    assert_eq!(elements.len(), 3, "expected 3 dense nodes");

    // Node 1
    match &elements[0] {
        Element::DenseNode(dn) => {
            assert_eq!(dn.id(), 1001);
            assert_eq!(dn.decimicro_lat(), 556_789_000);
            assert_eq!(dn.decimicro_lon(), 126_543_000);
            let tags: Vec<_> = dn.tags().collect();
            assert_eq!(tags.len(), 2);
            assert_eq!(tags[0], ("name", "TestNode"));
            assert_eq!(tags[1], ("highway", "bus_stop"));
            let info = dn.info().unwrap();
            assert_eq!(info.version(), 3);
            assert_eq!(info.uid(), 42);
            assert_eq!(info.user().unwrap(), "testuser");
        }
        _ => panic!("expected DenseNode"),
    }

    // Node 2 — tagless
    match &elements[1] {
        Element::DenseNode(dn) => {
            assert_eq!(dn.id(), 2002);
            assert_eq!(dn.decimicro_lat(), 570_000_000);
            assert_eq!(dn.decimicro_lon(), 100_000_000);
            let tags: Vec<_> = dn.tags().collect();
            assert_eq!(tags.len(), 0);
            let info = dn.info().unwrap();
            assert_eq!(info.version(), 1);
            assert_eq!(info.uid(), 7);
        }
        _ => panic!("expected DenseNode"),
    }

    // Node 3 — southern/western hemisphere
    match &elements[2] {
        Element::DenseNode(dn) => {
            assert_eq!(dn.id(), 3003);
            assert_eq!(dn.decimicro_lat(), -335_000_000);
            assert_eq!(dn.decimicro_lon(), -580_000_000);
            let tags: Vec<_> = dn.tags().collect();
            assert_eq!(tags.len(), 1);
            assert_eq!(tags[0], ("natural", "tree"));
            let info = dn.info().unwrap();
            assert_eq!(info.version(), 2);
            assert_eq!(info.uid(), 100);
        }
        _ => panic!("expected DenseNode"),
    }
}

/// Write nodes without any metadata, read back, verify.
#[test]
fn roundtrip_dense_nodes_no_metadata() {
    let mut buf = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::default());
        let header = block_builder::build_header(None, None, None, None, &[]).unwrap();
        writer.write_header(&header).unwrap();

        let mut bb = BlockBuilder::new();
        bb.add_node(1, 100_000_000, 200_000_000, &[("k", "v")], None);
        bb.add_node(2, -100_000_000, -200_000_000, &[], None);
        let bytes = bb.take().unwrap().unwrap();
        writer.write_primitive_block(&bytes).unwrap();
        writer.flush().unwrap();
    }

    let reader = BlobReader::new(Cursor::new(&buf));
    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();

    let prim = match blobs[1].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };

    let elements: Vec<_> = prim.elements().collect();
    assert_eq!(elements.len(), 2);

    match &elements[0] {
        Element::DenseNode(dn) => {
            assert_eq!(dn.id(), 1);
            assert_eq!(dn.decimicro_lat(), 100_000_000);
            assert_eq!(dn.decimicro_lon(), 200_000_000);
            let tags: Vec<_> = dn.tags().collect();
            assert_eq!(tags[0], ("k", "v"));
        }
        _ => panic!("expected DenseNode"),
    }

    match &elements[1] {
        Element::DenseNode(dn) => {
            assert_eq!(dn.id(), 2);
            assert_eq!(dn.decimicro_lat(), -100_000_000);
            assert_eq!(dn.decimicro_lon(), -200_000_000);
            let tags: Vec<_> = dn.tags().collect();
            assert_eq!(tags.len(), 0);
        }
        _ => panic!("expected DenseNode"),
    }
}

/// Write a PBF with ways, read it back, verify.
#[test]
fn roundtrip_ways() {
    let mut buf = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::Zlib(6));
        let header = block_builder::build_header(None, None, None, None, &[]).unwrap();
        writer.write_header(&header).unwrap();

        let mut bb = BlockBuilder::new();
        bb.add_way(
            5001,
            &[("highway", "residential"), ("name", "Main St")],
            &[100, 200, 300, 400],
            Some(&Metadata {
                version: 1,
                timestamp: 1_600_000_000,
                changeset: 9999,
                uid: 7,
                user: "mapper",
                visible: true,
            }),
        );
        bb.add_way(
            5002,
            &[("building", "yes")],
            &[500, 501, 502, 500], // closed way
            None,
        );

        let block_bytes = bb.take().unwrap().unwrap();
        writer.write_primitive_block(&block_bytes).unwrap();
        writer.flush().unwrap();
    }

    let reader = BlobReader::new(Cursor::new(&buf));
    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(blobs.len(), 2);

    let prim = match blobs[1].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };

    let elements: Vec<_> = prim.elements().collect();
    assert_eq!(elements.len(), 2);

    match &elements[0] {
        Element::Way(w) => {
            assert_eq!(w.id(), 5001);
            let tags: Vec<_> = w.tags().collect();
            assert_eq!(tags.len(), 2);
            assert_eq!(tags[0], ("highway", "residential"));
            assert_eq!(tags[1], ("name", "Main St"));
            let refs: Vec<_> = w.refs().collect();
            assert_eq!(refs, vec![100, 200, 300, 400]);
            let info = w.info();
            assert_eq!(info.version(), Some(1));
            assert_eq!(info.changeset(), Some(9999));
            assert_eq!(info.uid(), Some(7));
            assert_eq!(info.user().unwrap().unwrap(), "mapper");
        }
        _ => panic!("expected Way"),
    }

    match &elements[1] {
        Element::Way(w) => {
            assert_eq!(w.id(), 5002);
            let tags: Vec<_> = w.tags().collect();
            assert_eq!(tags[0], ("building", "yes"));
            let refs: Vec<_> = w.refs().collect();
            assert_eq!(refs, vec![500, 501, 502, 500]);
        }
        _ => panic!("expected Way"),
    }
}

/// Write a PBF with relations, read it back, verify.
#[test]
fn roundtrip_relations() {
    let mut buf = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::None);
        let header = block_builder::build_header(None, None, None, None, &[]).unwrap();
        writer.write_header(&header).unwrap();

        let mut bb = BlockBuilder::new();
        bb.add_relation(
            9001,
            &[("type", "multipolygon"), ("natural", "water")],
            &[
                MemberData {
                    id: MemberId::Way(100),
                    role: "outer",
                },
                MemberData {
                    id: MemberId::Way(200),
                    role: "inner",
                },
                MemberData {
                    id: MemberId::Node(300),
                    role: "admin_centre",
                },
            ],
            Some(&Metadata {
                version: 5,
                timestamp: 1_650_000_000,
                changeset: 77777,
                uid: 99,
                user: "relbuilder",
                visible: true,
            }),
        );

        let block_bytes = bb.take().unwrap().unwrap();
        writer.write_primitive_block(&block_bytes).unwrap();
        writer.flush().unwrap();
    }

    let reader = BlobReader::new(Cursor::new(&buf));
    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(blobs.len(), 2);

    let prim = match blobs[1].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };

    let elements: Vec<_> = prim.elements().collect();
    assert_eq!(elements.len(), 1);

    match &elements[0] {
        Element::Relation(r) => {
            assert_eq!(r.id(), 9001);
            let tags: Vec<_> = r.tags().collect();
            assert_eq!(tags.len(), 2);
            assert_eq!(tags[0], ("type", "multipolygon"));
            assert_eq!(tags[1], ("natural", "water"));

            let members: Vec<_> = r.members().collect();
            assert_eq!(members.len(), 3);

            assert_eq!(members[0].id, MemberId::Way(100));
            assert_eq!(members[0].role().unwrap(), "outer");

            assert_eq!(members[1].id, MemberId::Way(200));
            assert_eq!(members[1].role().unwrap(), "inner");

            assert_eq!(members[2].id, MemberId::Node(300));
            assert_eq!(members[2].role().unwrap(), "admin_centre");

            let info = r.info();
            assert_eq!(info.version(), Some(5));
            assert_eq!(info.uid(), Some(99));
            assert_eq!(info.user().unwrap().unwrap(), "relbuilder");
        }
        _ => panic!("expected Relation"),
    }
}

/// Verify that should_flush triggers at 8000 entities and take() resets the builder.
#[test]
fn block_builder_flush_at_8000() {
    let mut bb = BlockBuilder::new();

    for i in 0..8000 {
        assert!(!bb.should_flush(), "should not flush at {i}");
        bb.add_node(i as i64, 0, 0, &[], None);
    }
    assert!(bb.should_flush());
    assert!(!bb.can_add_node());

    let block_bytes = bb.take().unwrap().unwrap();
    assert!(!block_bytes.is_empty());

    // After take(), builder should be empty and ready for more
    assert!(bb.is_empty());
    assert!(bb.can_add_node());
    assert!(bb.can_add_way());

    // take() on empty returns None
    assert!(bb.take().unwrap().is_none());
}

/// Multiple blocks: nodes then ways in separate blocks.
#[test]
fn roundtrip_multi_block() {
    let mut buf = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::default());
        let header = block_builder::build_header(None, None, None, None, &[]).unwrap();
        writer.write_header(&header).unwrap();

        // Block 1: nodes
        let mut bb = BlockBuilder::new();
        bb.add_node(1, 100_000_000, 200_000_000, &[("k", "v")], None);
        let bytes = bb.take().unwrap().unwrap();
        writer.write_primitive_block(&bytes).unwrap();

        // Block 2: ways
        bb.add_way(10, &[("highway", "path")], &[1, 2, 3], None);
        let bytes = bb.take().unwrap().unwrap();
        writer.write_primitive_block(&bytes).unwrap();

        writer.flush().unwrap();
    }

    let reader = BlobReader::new(Cursor::new(&buf));
    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(blobs.len(), 3, "header + 2 data blobs");

    // Block 1: one node
    let prim1 = match blobs[1].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };
    let elems1: Vec<_> = prim1.elements().collect();
    assert_eq!(elems1.len(), 1);
    match &elems1[0] {
        Element::DenseNode(dn) => assert_eq!(dn.id(), 1),
        _ => panic!("expected DenseNode"),
    }

    // Block 2: one way
    let prim2 = match blobs[2].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };
    let elems2: Vec<_> = prim2.elements().collect();
    assert_eq!(elems2.len(), 1);
    match &elems2[0] {
        Element::Way(w) => {
            assert_eq!(w.id(), 10);
            let refs: Vec<_> = w.refs().collect();
            assert_eq!(refs, vec![1, 2, 3]);
        }
        _ => panic!("expected Way"),
    }
}

/// Round-trip ways with node locations (LocationsOnWays feature).
#[test]
fn roundtrip_way_with_locations() {
    let mut buf = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::default());
        let header =
            block_builder::build_header(None, None, None, None, &["LocationsOnWays"]).unwrap();
        writer.write_header(&header).unwrap();

        let mut bb = BlockBuilder::new();
        bb.add_way_with_locations(
            100,
            &[("highway", "primary")],
            &[1, 2, 3],
            &[(550_000_000, 120_000_000), (551_000_000, 121_000_000), (552_000_000, 122_000_000)],
            None,
        );
        let bytes = bb.take().unwrap().unwrap();
        writer.write_primitive_block(&bytes).unwrap();
        writer.flush().unwrap();
    }

    let reader = BlobReader::new(Cursor::new(&buf));
    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(blobs.len(), 2);

    // Verify header has LocationsOnWays feature
    match blobs[0].decode().unwrap() {
        BlobDecode::OsmHeader(header) => {
            let features: Vec<&str> = header
                .optional_features()
                .iter()
                .map(|s| s.as_ref())
                .collect();
            assert!(features.contains(&"LocationsOnWays"));
        }
        _ => panic!("expected OsmHeader"),
    }

    // Verify way has locations
    let prim = match blobs[1].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };
    let elements: Vec<_> = prim.elements().collect();
    assert_eq!(elements.len(), 1);
    match &elements[0] {
        Element::Way(w) => {
            assert_eq!(w.id(), 100);
            let refs: Vec<i64> = w.refs().collect();
            assert_eq!(refs, vec![1, 2, 3]);
            let locs: Vec<(i32, i32)> = w
                .node_locations()
                .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon()))
                .collect();
            assert_eq!(
                locs,
                vec![
                    (550_000_000, 120_000_000),
                    (551_000_000, 121_000_000),
                    (552_000_000, 122_000_000),
                ]
            );
        }
        _ => panic!("expected Way"),
    }
}

/// Uncompressed round-trip to test Compression::None path.
#[test]
fn roundtrip_uncompressed() {
    let mut buf = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::None);
        let header = block_builder::build_header(None, None, None, None, &[]).unwrap();
        writer.write_header(&header).unwrap();

        let mut bb = BlockBuilder::new();
        bb.add_node(42, 123_456_789, -987_654_321, &[("foo", "bar")], None);
        let bytes = bb.take().unwrap().unwrap();
        writer.write_primitive_block(&bytes).unwrap();
        writer.flush().unwrap();
    }

    let reader = BlobReader::new(Cursor::new(&buf));
    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(blobs.len(), 2);

    let prim = match blobs[1].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };
    let elements: Vec<_> = prim.elements().collect();
    assert_eq!(elements.len(), 1);
    match &elements[0] {
        Element::DenseNode(dn) => {
            assert_eq!(dn.id(), 42);
            assert_eq!(dn.decimicro_lat(), 123_456_789);
            assert_eq!(dn.decimicro_lon(), -987_654_321);
        }
        _ => panic!("expected DenseNode"),
    }
}
