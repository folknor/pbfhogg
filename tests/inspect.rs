mod common;

use common::{TestNode, TestRelation, TestWay};

fn write_simple_pbf(path: &std::path::Path) {
    common::write_test_pbf(
        path,
        &[
            TestNode { id: 1, lat: 510_000_000, lon: -1_000_000, tags: vec![] },
            TestNode { id: 2, lat: 520_000_000, lon: -2_000_000, tags: vec![("name", "foo")] },
            TestNode { id: 3, lat: 530_000_000, lon: -3_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "residential")] },
            TestWay { id: 11, refs: vec![2, 3], tags: vec![] },
        ],
        &[
            TestRelation { id: 100, members: vec![], tags: vec![("type", "route")] },
        ],
    );
}

#[test]
fn inspect_json_base() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(&input, false, false, false, false)
        .expect("inspect");
    let json = report.to_json(None);

    // Schema version
    assert_eq!(json["schema_version"], 1);

    // File metadata
    assert!(json["file"].as_str().expect("value").contains("test.osm.pbf"));
    assert!(json["file_size"].as_u64().expect("value") > 0);

    // Header
    assert!(!json["header"].is_null());
    assert!(json["header"]["required_features"].is_array());
    assert!(json["header"]["optional_features"].is_array());

    // Elements
    assert_eq!(json["elements"]["nodes"], 3);
    // tagged_nodes is 0 when the index-only fast path is used (no decompression),
    // because indexdata doesn't store per-element tag counts.
    assert!(json["elements"]["tagged_nodes"].is_u64());
    assert_eq!(json["elements"]["ways"], 2);
    assert_eq!(json["elements"]["relations"], 1);
    assert_eq!(json["elements"]["total"], 6);

    // Indexed
    assert!(json["indexed"].is_boolean());

    // Blocks
    assert!(json["blocks"]["total"].as_u64().expect("value") > 0);

    // Ordering
    assert!(json["ordering"]["sequence"].is_array());
    assert!(json["ordering"]["standard"].is_boolean());

    // Optional fields should be null when not requested
    assert!(json["id_ranges"].is_null());
    assert!(json["blocks_detail"].is_null());
    assert!(json["locations"].is_null());
}

#[test]
fn inspect_json_with_id_ranges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(&input, false, true, false, false)
        .expect("inspect");
    let json = report.to_json(None);

    // id_ranges should be an object, not null
    assert!(!json["id_ranges"].is_null());

    let nodes = &json["id_ranges"]["nodes"];
    assert_eq!(nodes["min"], 1);
    assert_eq!(nodes["max"], 3);
    assert_eq!(nodes["count"], 3);
    assert_eq!(nodes["monotonic"], true);

    let ways = &json["id_ranges"]["ways"];
    assert_eq!(ways["min"], 10);
    assert_eq!(ways["max"], 11);
    assert_eq!(ways["count"], 2);

    let rels = &json["id_ranges"]["relations"];
    assert_eq!(rels["min"], 100);
    assert_eq!(rels["max"], 100);
    assert_eq!(rels["count"], 1);
}

#[test]
fn inspect_json_with_blocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(&input, true, false, false, false)
        .expect("inspect");
    let json = report.to_json(Some(0));

    // blocks_detail should be an array, not null
    assert!(json["blocks_detail"].is_array());
    let detail = json["blocks_detail"].as_array().expect("value");
    assert!(!detail.is_empty());

    // Each block has the expected fields
    let first = &detail[0];
    assert!(first["number"].is_u64());
    assert!(first["type"].is_string());
    assert!(first["elements"].is_u64());
    assert!(first["compressed_bytes"].is_u64());
}

#[test]
fn inspect_json_blocks_limit_honored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(&input, true, false, false, false)
        .expect("inspect");

    let json_all = report.to_json(Some(0));
    let total = json_all["blocks_detail"].as_array().expect("value").len();

    // With limit=1, if total > 2, should get first 1 + last 1 = 2
    if total > 2 {
        let json_limited = report.to_json(Some(1));
        let limited = json_limited["blocks_detail"].as_array().expect("value").len();
        assert_eq!(limited, 2);
    }
}

#[test]
fn inspect_json_combined_flags() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(&input, true, true, false, false)
        .expect("inspect");
    let json = report.to_json(Some(0));

    // All optional fields should be present
    assert!(!json["id_ranges"].is_null());
    assert!(json["blocks_detail"].is_array());
    // locations still null (not requested)
    assert!(json["locations"].is_null());

    // Verify deterministic key set — all top-level keys present
    let obj = json.as_object().expect("value");
    for key in &[
        "schema_version", "file", "file_size", "header", "indexed",
        "blocks", "elements", "ordering", "id_ranges", "blocks_detail", "locations",
    ] {
        assert!(obj.contains_key(*key), "missing key: {key}");
    }
}
