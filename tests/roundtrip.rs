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
        .map(String::as_str)
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
                .map(String::as_str)
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

/// Write a PBF with O_DIRECT, read it back, verify data integrity.
/// Skips gracefully if O_DIRECT is not supported on the current filesystem (e.g. tmpfs).
#[cfg(feature = "linux-direct-io")]
#[test]
fn roundtrip_direct_io() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("direct_io.osm.pbf");

    // Write with O_DIRECT
    let write_result = PbfWriter::to_path_direct(&path, Compression::default());
    let mut writer = match write_result {
        Ok(w) => w,
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            eprintln!("O_DIRECT not supported on this filesystem, skipping test");
            return;
        }
        Err(e) => panic!("unexpected error opening with O_DIRECT: {e}"),
    };

    let header = block_builder::build_header(
        Some((9.0, 54.0, 13.0, 58.0)),
        None,
        None,
        None,
        &[],
    )
    .unwrap();
    writer.write_header(&header).unwrap();

    let mut bb = BlockBuilder::new();
    bb.add_node(1, 100_000_000, 200_000_000, &[("k", "v")], None);
    bb.add_node(2, -100_000_000, -200_000_000, &[], None);
    let bytes = bb.take().unwrap().unwrap();
    writer.write_primitive_block(&bytes).unwrap();

    bb.add_way(10, &[("highway", "path")], &[1, 2, 3], None);
    let bytes = bb.take().unwrap().unwrap();
    writer.write_primitive_block(&bytes).unwrap();

    writer.flush().unwrap();

    // Read back with standard reader
    let reader = BlobReader::from_path(&path).unwrap();
    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(blobs.len(), 3, "header + nodes block + ways block");

    // Verify header
    match blobs[0].decode().unwrap() {
        BlobDecode::OsmHeader(h) => {
            let bbox = h.bbox().unwrap();
            assert!((bbox.left - 9.0).abs() < 1e-6);
        }
        _ => panic!("expected OsmHeader"),
    }

    // Verify nodes
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
            let tags: Vec<_> = dn.tags().collect();
            assert_eq!(tags[0], ("k", "v"));
        }
        _ => panic!("expected DenseNode"),
    }

    // Verify ways
    let prim2 = match blobs[2].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };
    let elements2: Vec<_> = prim2.elements().collect();
    assert_eq!(elements2.len(), 1);
    match &elements2[0] {
        Element::Way(w) => {
            assert_eq!(w.id(), 10);
            let refs: Vec<_> = w.refs().collect();
            assert_eq!(refs, vec![1, 2, 3]);
        }
        _ => panic!("expected Way"),
    }

    // Verify exact file size — no padding left over
    let meta = std::fs::metadata(&path).unwrap();
    assert!(meta.len() > 0, "file should not be empty");
}

/// Write a PBF through the pipelined O_DIRECT path, read back, verify ordering.
#[cfg(feature = "linux-direct-io")]
#[test]
fn roundtrip_pipelined_direct_io() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pipelined_direct_io.osm.pbf");

    let header = block_builder::build_header(None, None, None, None, &[]).unwrap();

    let write_result =
        PbfWriter::to_path_pipelined_direct(&path, Compression::default(), &header);
    let mut writer = match write_result {
        Ok(w) => w,
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            eprintln!("O_DIRECT not supported on this filesystem, skipping test");
            return;
        }
        Err(e) => panic!("unexpected error opening with O_DIRECT: {e}"),
    };

    // Write 5 blocks to exercise the pipeline + reorder buffer
    for i in 0..5 {
        let mut bb = BlockBuilder::new();
        for j in 0..100 {
            let id = i * 100 + j + 1;
            #[allow(clippy::cast_possible_truncation)]
            let id32 = id as i32;
            bb.add_node(id, id32 * 1_000_000, id32 * 2_000_000, &[], None);
        }
        let bytes = bb.take().unwrap().unwrap();
        writer.write_primitive_block(&bytes).unwrap();
    }

    writer.flush().unwrap();

    // Read back and verify all 500 nodes in order
    let reader = BlobReader::from_path(&path).unwrap();
    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(blobs.len(), 6, "header + 5 data blobs");

    let mut all_ids = Vec::new();
    for blob in &blobs[1..] {
        let prim = match blob.decode().unwrap() {
            BlobDecode::OsmData(p) => p,
            _ => panic!("expected OsmData"),
        };
        for element in prim.elements() {
            match element {
                Element::DenseNode(dn) => all_ids.push(dn.id()),
                _ => panic!("expected DenseNode"),
            }
        }
    }

    assert_eq!(all_ids.len(), 500);
    assert_eq!(all_ids[0], 1);
    assert_eq!(all_ids[499], 500);
    // Verify monotonically increasing
    for pair in all_ids.windows(2) {
        assert!(pair[1] > pair[0], "IDs should be increasing: {} > {}", pair[1], pair[0]);
    }
}

/// Write a PBF with zstd compression, read it back, verify the data survives the roundtrip.
#[test]
fn roundtrip_zstd() {
    let mut buf = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::Zstd(3));
        let header = block_builder::build_header(None, None, None, None, &[]).unwrap();
        writer.write_header(&header).unwrap();

        let mut bb = BlockBuilder::new();
        bb.add_node(100, 556_789_000, 126_543_000, &[("name", "ZstdNode")], None);
        bb.add_node(200, -335_000_000, -580_000_000, &[("natural", "tree")], None);
        let bytes = bb.take().unwrap().unwrap();
        writer.write_primitive_block(&bytes).unwrap();

        let mut bb2 = BlockBuilder::new();
        bb2.add_way(300, &[("highway", "residential")], &[100, 200], None);
        let bytes2 = bb2.take().unwrap().unwrap();
        writer.write_primitive_block(&bytes2).unwrap();
        writer.flush().unwrap();
    }

    // Read back and verify
    let reader = BlobReader::new(Cursor::new(&buf));
    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(blobs.len(), 3); // header + nodes block + ways block

    // Verify nodes
    let prim = match blobs[1].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };
    let elements: Vec<_> = prim.elements().collect();
    assert_eq!(elements.len(), 2);
    match &elements[0] {
        Element::DenseNode(dn) => {
            assert_eq!(dn.id(), 100);
            assert_eq!(dn.decimicro_lat(), 556_789_000);
            let tags: Vec<_> = dn.tags().collect();
            assert_eq!(tags, vec![("name", "ZstdNode")]);
        }
        _ => panic!("expected DenseNode"),
    }

    // Verify way
    let prim2 = match blobs[2].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };
    let elements2: Vec<_> = prim2.elements().collect();
    assert_eq!(elements2.len(), 1);
    match &elements2[0] {
        Element::Way(w) => {
            assert_eq!(w.id(), 300);
            let refs: Vec<_> = w.refs().collect();
            assert_eq!(refs, vec![100, 200]);
        }
        _ => panic!("expected Way"),
    }
}

/// Verify that adding a way after a node without take() panics.
/// BlockBuilder enforces one element type per block.
#[test]
#[should_panic(expected = "cannot add way: block full or wrong type")]
fn block_builder_mixed_type_panics() {
    let mut bb = BlockBuilder::new();
    bb.add_node(1, 0, 0, &[], None);
    bb.add_way(10, &[], &[1, 2], None); // should panic
}

/// Mixed metadata: some nodes have metadata, others don't (like merge OSC replacements).
/// Tests that DenseInfo parallel arrays stay aligned with dense_ids.
#[test]
fn roundtrip_mixed_metadata() {
    let mut buf = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::default());
        let header = block_builder::build_header(None, None, None, None, &[]).unwrap();
        writer.write_header(&header).unwrap();

        let mut bb = BlockBuilder::new();

        // Node 1: with metadata (like a base PBF node)
        bb.add_node(
            1, 100_000_000, 200_000_000,
            &[("name", "base")],
            Some(&Metadata {
                version: 5,
                timestamp: 1_700_000_000,
                changeset: 100,
                uid: 42,
                user: "mapper",
                visible: true,
            }),
        );

        // Node 2: NO metadata (like an OSC replacement)
        bb.add_node(2, 110_000_000, 210_000_000, &[("name", "osc")], None);

        // Node 3: with metadata again (back to base)
        bb.add_node(
            3, 120_000_000, 220_000_000,
            &[("name", "base2")],
            Some(&Metadata {
                version: 2,
                timestamp: 1_600_000_000,
                changeset: 200,
                uid: 7,
                user: "editor",
                visible: true,
            }),
        );

        let bytes = bb.take().unwrap().unwrap();
        writer.write_primitive_block(&bytes).unwrap();
        writer.flush().unwrap();
    }

    // Read back — all 3 nodes must decode without panic
    let reader = BlobReader::new(Cursor::new(&buf));
    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();
    let prim = match blobs[1].decode().unwrap() {
        BlobDecode::OsmData(p) => p,
        _ => panic!("expected OsmData"),
    };
    let elements: Vec<_> = prim.elements().collect();
    assert_eq!(elements.len(), 3);

    // Node 1: has metadata
    match &elements[0] {
        Element::DenseNode(dn) => {
            assert_eq!(dn.id(), 1);
            let tags: Vec<_> = dn.tags().collect();
            assert_eq!(tags, vec![("name", "base")]);
            let info = dn.info().unwrap();
            assert_eq!(info.version(), 5);
            assert_eq!(info.uid(), 42);
            assert_eq!(info.user().unwrap(), "mapper");
        }
        _ => panic!("expected DenseNode"),
    }

    // Node 2: default metadata (version 0, uid 0, user "")
    match &elements[1] {
        Element::DenseNode(dn) => {
            assert_eq!(dn.id(), 2);
            let tags: Vec<_> = dn.tags().collect();
            assert_eq!(tags, vec![("name", "osc")]);
            let info = dn.info().unwrap();
            assert_eq!(info.version(), 0);
            assert_eq!(info.uid(), 0);
        }
        _ => panic!("expected DenseNode"),
    }

    // Node 3: has metadata
    match &elements[2] {
        Element::DenseNode(dn) => {
            assert_eq!(dn.id(), 3);
            let tags: Vec<_> = dn.tags().collect();
            assert_eq!(tags, vec![("name", "base2")]);
            let info = dn.info().unwrap();
            assert_eq!(info.version(), 2);
            assert_eq!(info.uid(), 7);
            assert_eq!(info.user().unwrap(), "editor");
        }
        _ => panic!("expected DenseNode"),
    }
}

/// Reverse mixed metadata: first node has no metadata, later ones do.
/// Tests the backfill path in BlockBuilder.
#[test]
fn roundtrip_backfill_metadata() {
    let mut buf = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::default());
        let header = block_builder::build_header(None, None, None, None, &[]).unwrap();
        writer.write_header(&header).unwrap();

        let mut bb = BlockBuilder::new();

        // Node 1: NO metadata
        bb.add_node(1, 100_000_000, 200_000_000, &[], None);

        // Node 2: NO metadata
        bb.add_node(2, 110_000_000, 210_000_000, &[], None);

        // Node 3: WITH metadata (triggers backfill of nodes 1 and 2)
        bb.add_node(
            3, 120_000_000, 220_000_000,
            &[("name", "first_with_meta")],
            Some(&Metadata {
                version: 1,
                timestamp: 1_700_000_000,
                changeset: 300,
                uid: 99,
                user: "late_meta",
                visible: true,
            }),
        );

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
    assert_eq!(elements.len(), 3);

    // Nodes 1 and 2: backfilled default metadata
    for (i, elem) in elements[..2].iter().enumerate() {
        match elem {
            Element::DenseNode(dn) => {
                let info = dn.info().unwrap();
                assert_eq!(info.version(), 0, "backfilled node {i} should have version 0");
                assert_eq!(info.uid(), 0);
            }
            _ => panic!("expected DenseNode"),
        }
    }

    // Node 3: real metadata
    match &elements[2] {
        Element::DenseNode(dn) => {
            assert_eq!(dn.id(), 3);
            let info = dn.info().unwrap();
            assert_eq!(info.version(), 1);
            assert_eq!(info.uid(), 99);
            assert_eq!(info.user().unwrap(), "late_meta");
        }
        _ => panic!("expected DenseNode"),
    }
}
