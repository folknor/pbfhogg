//! derive-changes correctness tests.

mod common;

use common::{
    TestMember, TestNode, TestRelation, TestWay, assert_elements_equivalent, generate_nodes,
    generate_ways, node_ids_with_coords as node_ids,
    read_all_elements_with_coords as read_all_elements, relation_ids_with_coords as relation_ids,
    way_ids_with_coords as way_ids, write_multi_block_test_pbf, write_test_pbf,
    write_test_pbf_sorted,
};
use pbfhogg::MemberId;
use pbfhogg::apply_changes::{MergeOptions, merge};
use pbfhogg::block_builder::{self, BlockBuilder, Metadata};
use pbfhogg::diff::derive::derive_changes;
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
        TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "a")],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 110_000_000,
            lon: 210_000_000,
            tags: vec![],
            meta: None,
        },
    ];
    let ways = [TestWay {
        id: 10,
        refs: vec![1, 2],
        tags: vec![("highway", "primary")],
        meta: None,
    }];

    write_test_pbf_sorted(&old, &nodes, &ways, &[]);
    write_test_pbf_sorted(&new, &nodes, &ways, &[]);

    let stats = derive_changes(&old, &new, &osc, false, false, false, 1).expect("derive");
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
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "new")],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false, 1).expect("derive");
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
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![],
            meta: None,
        }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false, 1).expect("derive");
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
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 150_000_000,
            lon: 250_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false, 1).expect("derive");
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
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "old")],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "new")],
            meta: None,
        }],
        &[],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false, 1).expect("derive");
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
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false, 1).expect("derive");
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
            members: vec![TestMember {
                id: MemberId::Node(1),
                role: "stop",
            }],
            tags: vec![("type", "route")],
            meta: None,
        }],
    );
    write_test_pbf_sorted(
        &new,
        &[],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![
                TestMember {
                    id: MemberId::Node(1),
                    role: "stop",
                },
                TestMember {
                    id: MemberId::Way(2),
                    role: "outer",
                },
            ],
            tags: vec![("type", "route")],
            meta: None,
        }],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false, 1).expect("derive");
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
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("name", "one")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("name", "ONE")],
                meta: None,
            }, // modified tag
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            }, // unchanged
            // node 3 deleted
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![],
                meta: None,
            }, // created
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "primary")],
            meta: None,
        }], // modified refs
        &[],
    );

    let stats = derive_changes(&old, &new, &osc, false, false, false, 1).expect("derive");
    assert_eq!(stats.creates, 1); // node 4
    assert_eq!(stats.modifies, 2); // node 1 + way 10
    assert_eq!(stats.deletes, 1); // node 3
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
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("name", "one")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("to_delete", "yes")],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("name", "ONE")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 5,
                lat: 140_000_000,
                lon: 240_000_000,
                tags: vec![("new", "yes")],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 5],
            tags: vec![("highway", "secondary")],
            meta: None,
        }],
        &[],
    );

    // Derive changes
    let stats = derive_changes(&old, &new, &osc, false, false, false, 1).expect("derive");
    assert_eq!(stats.creates, 1); // node 5
    assert_eq!(stats.modifies, 2); // node 1 (tags) + way 10 (refs + tags)
    assert_eq!(stats.deletes, 1); // node 3

    // Apply changes back to old → should produce equivalent of new
    merge(
        &old,
        &osc,
        &result,
        &MergeOptions {
            compression: pbfhogg::writer::Compression::default(),
            direct_io: false,
            io_uring: false,
            force: true,
            locations_on_ways: false,
            jobs: None,
        },
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge");

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
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let err = derive_changes(&old, &new, &osc, false, false, false, 1)
        .expect_err("should reject unsorted input");
    let msg = err.to_string();
    assert!(
        msg.contains("not sorted"),
        "error should mention 'not sorted', got: {msg}"
    );
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
/// Uses `MultiGzDecoder` so the parallel-gzip writer's concatenated
/// multi-member output (see `src/write/parallel_gzip.rs`) decodes as
/// a single logical stream. `MultiGzDecoder` is a strict superset of
/// `GzDecoder` - it handles single-member files identically.
fn read_osc(path: &std::path::Path) -> String {
    let file = std::fs::File::open(path).expect("open osc");
    let mut gz = flate2::read::MultiGzDecoder::new(file);
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
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();
    for &(id, lat, lon, ver) in nodes {
        let meta = Metadata {
            version: ver,
            timestamp: 0,
            changeset: 0,
            uid: 0,
            user: "",
            visible: true,
        };
        bb.add_node(
            id,
            lat,
            lon,
            std::iter::empty::<(&str, &str)>(),
            Some(&meta),
        );
    }
    if !bb.is_empty() {
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(bytes).expect("write block");
        }
    }
    for (id, refs, ver) in ways {
        let meta = Metadata {
            version: *ver,
            timestamp: 0,
            changeset: 0,
            uid: 0,
            user: "",
            visible: true,
        };
        bb.add_way(*id, std::iter::empty::<(&str, &str)>(), refs, Some(&meta));
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
    // New has only node 1 (v3) - node 2 and way 10 are deleted.
    write_versioned_pbf(
        &old,
        &[
            (1, 100_000_000, 200_000_000, 3),
            (2, 110_000_000, 210_000_000, 5),
        ],
        &[(10, vec![1, 2], 2)],
    );
    write_versioned_pbf(&new, &[(1, 100_000_000, 200_000_000, 3)], &[]);

    let stats = derive_changes(&old, &new, &osc, false, true, false, 1).expect("derive");
    assert_eq!(stats.deletes, 2); // node 2 + way 10

    let xml = read_osc(&osc);
    // Node 2 should have version="6" (was 5, incremented)
    assert!(xml.contains(r#"id="2"#), "should contain node id=2");
    assert!(
        xml.contains(r#"version="6""#),
        "node 2 version should be 6, got:\n{xml}"
    );
    // Way 10 should have version="3" (was 2, incremented)
    assert!(xml.contains(r#"id="10"#), "should contain way id=10");
    assert!(
        xml.contains(r#"version="3""#),
        "way 10 version should be 3, got:\n{xml}"
    );
}

#[test]
fn no_increment_version_preserves_delete_versions() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_versioned_pbf(
        &old,
        &[
            (1, 100_000_000, 200_000_000, 3),
            (2, 110_000_000, 210_000_000, 5),
        ],
        &[],
    );
    write_versioned_pbf(&new, &[(1, 100_000_000, 200_000_000, 3)], &[]);

    let stats = derive_changes(&old, &new, &osc, false, false, false, 1).expect("derive");
    assert_eq!(stats.deletes, 1);

    let xml = read_osc(&osc);
    // Node 2 should have version="5" (unchanged)
    assert!(
        xml.contains(r#"version="5""#),
        "node 2 version should be 5, got:\n{xml}"
    );
}

#[test]
fn increment_version_and_update_timestamp_combined() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    // Old has node 1 (v2) and way 10 (v4).
    // New has neither - both are deleted.
    write_versioned_pbf(
        &old,
        &[(1, 100_000_000, 200_000_000, 2)],
        &[(10, vec![1], 4)],
    );
    write_versioned_pbf(&new, &[], &[]);

    // Both increment_version=true and update_timestamp=true
    let stats = derive_changes(&old, &new, &osc, false, true, true, 1).expect("derive");
    assert_eq!(stats.deletes, 2); // node 1 + way 10

    let xml = read_osc(&osc);

    // Versions should be bumped
    assert!(
        xml.contains(r#"version="3""#),
        "node 1 version should be 3 (was 2), got:\n{xml}"
    );
    assert!(
        xml.contains(r#"version="5""#),
        "way 10 version should be 5 (was 4), got:\n{xml}"
    );

    // Timestamps should be present and recent (the code uses current time)
    assert!(
        xml.contains("timestamp="),
        "delete elements should have a timestamp, got:\n{xml}"
    );
    // Verify the timestamp looks like a valid ISO 8601 date (20xx-)
    assert!(
        xml.contains("timestamp=\"20"),
        "timestamp should be a recent ISO date, got:\n{xml}"
    );
}

#[test]
fn derive_changes_jobs_parity_roundtrips_to_same_output() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc_seq = dir.path().join("changes_seq.osc.gz");
    let osc_par = dir.path().join("changes_par.osc.gz");
    let out_seq = dir.path().join("result_seq.osm.pbf");
    let out_par = dir.path().join("result_par.osm.pbf");

    let mut old_nodes = generate_nodes(24, 1);
    for (i, node) in old_nodes.iter_mut().enumerate() {
        if i % 4 == 0 {
            node.tags = vec![("name", "old")];
        }
    }

    let mut old_ways = generate_ways(10, 1_000, 3, 1);
    for (i, way) in old_ways.iter_mut().enumerate() {
        let start = 1 + i as i64 * 2;
        way.refs = vec![start, start + 1, start + 2];
        way.tags = if i % 2 == 0 {
            vec![("highway", "residential")]
        } else {
            vec![("highway", "service")]
        };
    }

    let mut new_nodes: Vec<TestNode> = old_nodes
        .iter()
        .map(|node| TestNode {
            id: node.id,
            lat: node.lat,
            lon: node.lon,
            tags: node.tags.clone(),
            meta: None,
        })
        .collect();
    new_nodes.retain(|node| node.id != 23);
    if let Some(node5) = new_nodes.iter_mut().find(|node| node.id == 5) {
        node5.lat = 555_555;
        node5.lon = 444_444;
        node5.tags = vec![("name", "modified")];
    }
    new_nodes.push(TestNode {
        id: 30,
        lat: 300_000,
        lon: 600_000,
        tags: vec![("created", "yes")],
        meta: None,
    });

    let mut new_ways: Vec<TestWay> = old_ways
        .iter()
        .map(|way| TestWay {
            id: way.id,
            refs: way.refs.clone(),
            tags: way.tags.clone(),
            meta: None,
        })
        .collect();
    new_ways.retain(|way| way.id != 1_007);
    if let Some(way1003) = new_ways.iter_mut().find(|way| way.id == 1_003) {
        way1003.refs = vec![7, 5, 30];
        way1003.tags = vec![("highway", "secondary"), ("surface", "gravel")];
    }
    new_ways.push(TestWay {
        id: 2_000,
        refs: vec![5, 30, 6],
        tags: vec![("highway", "primary")],
        meta: None,
    });

    write_multi_block_test_pbf(&old, &old_nodes, &old_ways, &[], 4);
    write_multi_block_test_pbf(&new, &new_nodes, &new_ways, &[], 4);

    let seq = derive_changes(&old, &new, &osc_seq, false, false, false, 1).expect("derive seq");
    let par = derive_changes(&old, &new, &osc_par, false, false, false, 4).expect("derive par");

    assert_eq!(seq.creates, par.creates);
    assert_eq!(seq.modifies, par.modifies);
    assert_eq!(seq.deletes, par.deletes);

    merge(
        &old,
        &osc_seq,
        &out_seq,
        &MergeOptions {
            compression: Compression::default(),
            direct_io: false,
            io_uring: false,
            force: true,
            locations_on_ways: false,
            jobs: None,
        },
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge seq");
    merge(
        &old,
        &osc_par,
        &out_par,
        &MergeOptions {
            compression: Compression::default(),
            direct_io: false,
            io_uring: false,
            force: true,
            locations_on_ways: false,
            jobs: None,
        },
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge par");

    assert_elements_equivalent(&out_seq, &out_par);
    assert_elements_equivalent(&out_seq, &new);
    assert_elements_equivalent(&out_par, &new);
}
