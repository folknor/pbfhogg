//! derive-changes correctness tests.

mod common;

use common::{
    node_ids_with_coords as node_ids, read_all_elements_with_coords as read_all_elements,
    way_ids_with_coords as way_ids, relation_ids_with_coords as relation_ids,
    write_test_pbf, write_test_pbf_sorted, TestMember, TestNode, TestRelation, TestWay,
};
use pbfhogg::MemberId;
use pbfhogg::block_builder::{self, BlockBuilder, Metadata};
use pbfhogg::derive_changes::derive_changes;
use pbfhogg::merge::{merge, MergeOptions};
use pbfhogg::writer::{Compression, PbfWriter};
use std::io::Read;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn identical_files_no_changes() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    let nodes = [
        TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
        TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
    ];
    let ways = [
        TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
    ];

    write_test_pbf_sorted(&old, &nodes, &ways, &[]);
    write_test_pbf_sorted(&new, &nodes, &ways, &[]);

    let stats = derive_changes(&old, &new, &osc, false, false, false).expect("derive");
    assert_eq!(stats.creates, 0);
    assert_eq!(stats.modifies, 0);
    assert_eq!(stats.deletes, 0);
}

#[test]
fn create_only() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "new")] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] }],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false).expect("derive");
    assert_eq!(stats.creates, 2); // node 2 + way 10
    assert_eq!(stats.modifies, 0);
    assert_eq!(stats.deletes, 0);
}

#[test]
fn delete_only() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(
        &old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![] }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false).expect("derive");
    assert_eq!(stats.creates, 0);
    assert_eq!(stats.modifies, 0);
    assert_eq!(stats.deletes, 2); // node 2 + way 10
}

#[test]
fn modify_node_coords() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 150_000_000, lon: 250_000_000, tags: vec![] }],
        &[],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false).expect("derive");
    assert_eq!(stats.creates, 0);
    assert_eq!(stats.modifies, 1);
    assert_eq!(stats.deletes, 0);
}

#[test]
fn modify_node_tags() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "old")] }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "new")] }],
        &[],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false).expect("derive");
    assert_eq!(stats.modifies, 1);
}

#[test]
fn modify_way_refs() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(
        &old,
        &[],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] }],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false).expect("derive");
    assert_eq!(stats.modifies, 1);
}

#[test]
fn modify_relation_members() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(
        &old,
        &[],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![TestMember { id: MemberId::Node(1), role: "stop" }],
            tags: vec![("type", "route")],
        }],
    );
    write_test_pbf_sorted(
        &new,
        &[],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![
                TestMember { id: MemberId::Node(1), role: "stop" },
                TestMember { id: MemberId::Way(2), role: "outer" },
            ],
            tags: vec![("type", "route")],
        }],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false).expect("derive");
    assert_eq!(stats.modifies, 1);
}

#[test]
fn mixed_create_modify_delete() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(
        &old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "one")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "ONE")] }, // modified tag
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] }, // unchanged
            // node 3 deleted
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] }, // created
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] }], // modified refs
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false).expect("derive");
    assert_eq!(stats.creates, 1);  // node 4
    assert_eq!(stats.modifies, 2); // node 1 + way 10
    assert_eq!(stats.deletes, 1);  // node 3
}

/// Full roundtrip: old → derive_changes → osc → merge(old, osc) → result ≈ new
#[test]
fn roundtrip_with_merge() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");
    let result = dir.path().join("result.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "one")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("to_delete", "yes")] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
        ],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "ONE")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 5, lat: 140_000_000, lon: 240_000_000, tags: vec![("new", "yes")] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 5], tags: vec![("highway", "secondary")] },
        ],
        &[],
    );

    // Derive changes
    let stats = derive_changes(&old, &new, &osc, false, false, false).expect("derive");
    assert_eq!(stats.creates, 1);  // node 5
    assert_eq!(stats.modifies, 2); // node 1 (tags) + way 10 (refs + tags)
    assert_eq!(stats.deletes, 1);  // node 3

    // Apply changes back to old → should produce equivalent of new
    merge(&old, &osc, &result, &MergeOptions {
        compression: pbfhogg::writer::Compression::default(),
        direct_io: false, io_uring: false, force: true, locations_on_ways: false,
    }).expect("merge");

    let result_contents = read_all_elements(&result);
    let new_contents = read_all_elements(&new);

    // Compare node IDs and data
    assert_eq!(node_ids(&result_contents), node_ids(&new_contents));
    for (r, n) in result_contents.nodes.iter().zip(new_contents.nodes.iter()) {
        assert_eq!(r.0, n.0, "node ID mismatch");
        assert_eq!(r.1, n.1, "node lat mismatch for id={}", r.0);
        assert_eq!(r.2, n.2, "node lon mismatch for id={}", r.0);
        assert_eq!(r.3, n.3, "node tags mismatch for id={}", r.0);
    }

    // Compare way IDs and data
    assert_eq!(way_ids(&result_contents), way_ids(&new_contents));
    for (r, n) in result_contents.ways.iter().zip(new_contents.ways.iter()) {
        assert_eq!(r.0, n.0, "way ID mismatch");
        assert_eq!(r.1, n.1, "way refs mismatch for id={}", r.0);
        assert_eq!(r.2, n.2, "way tags mismatch for id={}", r.0);
    }

    assert_eq!(relation_ids(&result_contents), relation_ids(&new_contents));
}

#[test]
fn unsorted_input_rejected() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    // Write old without sorted header, new with sorted header.
    write_test_pbf(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );

    let err = derive_changes(&old, &new, &osc, false, false, false)
        .expect_err("should reject unsorted input");
    let msg = err.to_string();
    assert!(msg.contains("not sorted"), "error should mention 'not sorted', got: {msg}");
    assert!(
        msg.contains("Sort.Type_then_ID"),
        "error should mention Sort.Type_then_ID, got: {msg}",
    );
    assert!(
        msg.contains("pbfhogg sort"),
        "error should mention 'pbfhogg sort', got: {msg}",
    );
}

/// Read gzipped OSC file and return the decompressed XML string.
fn read_osc(path: &std::path::Path) -> String {
    let file = std::fs::File::open(path).expect("open osc");
    let mut gz = flate2::read::GzDecoder::new(file);
    let mut xml = String::new();
    gz.read_to_string(&mut xml).expect("decompress osc");
    xml
}

/// Write a sorted PBF with version metadata on each element.
fn write_versioned_pbf(
    path: &std::path::Path,
    nodes: &[(i64, i32, i32, i32)], // (id, lat, lon, version)
    ways: &[(i64, Vec<i64>, i32)],  // (id, refs, version)
) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new().sorted().build().expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();
    for &(id, lat, lon, ver) in nodes {
        let meta = Metadata {
            version: ver, timestamp: 0, changeset: 0, uid: 0, user: "", visible: true,
        };
        bb.add_node(id, lat, lon, &[], Some(&meta));
    }
    if !bb.is_empty() {
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(bytes).expect("write block");
        }
    }
    for (id, refs, ver) in ways {
        let meta = Metadata {
            version: *ver, timestamp: 0, changeset: 0, uid: 0, user: "", visible: true,
        };
        bb.add_way(*id, &[], refs, Some(&meta));
    }
    if !bb.is_empty() {
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(bytes).expect("write block");
        }
    }
    writer.flush().expect("flush");
}

#[test]
fn increment_version_bumps_delete_versions() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    // Old has node 1 (v3), node 2 (v5), way 10 (v2).
    // New has only node 1 (v3) — node 2 and way 10 are deleted.
    write_versioned_pbf(&old, &[
        (1, 100_000_000, 200_000_000, 3),
        (2, 110_000_000, 210_000_000, 5),
    ], &[
        (10, vec![1, 2], 2),
    ]);
    write_versioned_pbf(&new, &[
        (1, 100_000_000, 200_000_000, 3),
    ], &[]);

    let stats = derive_changes(&old, &new, &osc, false, true, false).expect("derive");
    assert_eq!(stats.deletes, 2); // node 2 + way 10

    let xml = read_osc(&osc);
    // Node 2 should have version="6" (was 5, incremented)
    assert!(xml.contains(r#"id="2"#), "should contain node id=2");
    assert!(xml.contains(r#"version="6""#), "node 2 version should be 6, got:\n{xml}");
    // Way 10 should have version="3" (was 2, incremented)
    assert!(xml.contains(r#"id="10"#), "should contain way id=10");
    assert!(xml.contains(r#"version="3""#), "way 10 version should be 3, got:\n{xml}");
}

#[test]
fn no_increment_version_preserves_delete_versions() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_versioned_pbf(&old, &[
        (1, 100_000_000, 200_000_000, 3),
        (2, 110_000_000, 210_000_000, 5),
    ], &[]);
    write_versioned_pbf(&new, &[
        (1, 100_000_000, 200_000_000, 3),
    ], &[]);

    let stats = derive_changes(&old, &new, &osc, false, false, false).expect("derive");
    assert_eq!(stats.deletes, 1);

    let xml = read_osc(&osc);
    // Node 2 should have version="5" (unchanged)
    assert!(xml.contains(r#"version="5""#), "node 2 version should be 5, got:\n{xml}");
}
