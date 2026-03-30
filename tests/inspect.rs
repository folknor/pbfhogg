#![cfg(feature = "commands")]

mod common;

use common::{TestNode, TestRelation, TestWay};
use pbfhogg::block_builder::{self, BlockBuilder};
use pbfhogg::writer::{Compression, PbfWriter};

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

fn write_nodes_blocks(path: &std::path::Path, blocks: &[usize]) {
    let header = block_builder::HeaderBuilder::new().build().expect("build header");
    let mut writer = PbfWriter::to_path(path, Compression::default(), &header).expect("create writer");
    let mut bb = BlockBuilder::new();
    let mut next_id: i64 = 1;
    for &count in blocks {
        for _ in 0..count {
            bb.add_node(next_id, 500_000_000, 100_000_000, std::iter::empty::<(&str, &str)>(), None);
            next_id += 1;
        }
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(bytes).expect("write block");
        }
    }
    writer.flush().expect("flush");
}

#[test]
fn inspect_json_base() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(&input, false, false, false, false, false)
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

    let report = pbfhogg::inspect::inspect(&input, false, true, false, false, false)
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

    let report = pbfhogg::inspect::inspect(&input, true, false, false, false, false)
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

    let report = pbfhogg::inspect::inspect(&input, true, false, false, false, false)
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

    let report = pbfhogg::inspect::inspect(&input, true, true, false, false, false)
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
        "blocks", "elements", "ordering", "id_ranges", "anomalies_only",
        "blocks_detail", "locations",
    ] {
        assert!(obj.contains_key(*key), "missing key: {key}");
    }
}

#[test]
fn inspect_json_blocks_anomalies_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("anomalies.osm.pbf");
    // Three node blocks with element counts [100, 100, 10].
    // Median=100, so the 10-element block is anomalously small (<50% of median).
    write_nodes_blocks(&input, &[100, 100, 10]);

    let report = pbfhogg::inspect::inspect(&input, true, false, false, false, false)
        .expect("inspect");
    let json = report.to_json_filtered(Some(0), true);

    assert!(json["anomalies_only"].as_bool().expect("bool"));
    assert!(json["blocks_detail"].is_array());
    let detail = json["blocks_detail"].as_array().expect("value");
    assert_eq!(detail.len(), 1);
    assert_eq!(detail[0]["type"], "nodes");
    assert_eq!(detail[0]["elements"], 10);
    assert_eq!(detail[0]["anomaly"], "small");
}

#[test]
fn inspect_extended() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    // extended=true forces full decode and collects timestamps, bbox, metadata
    let report = pbfhogg::inspect::inspect(&input, false, false, false, true, false)
        .expect("inspect");
    let json = report.to_json(None);

    // Extended automatically enables id_ranges
    assert!(!json["id_ranges"].is_null());

    // data section should be present
    assert!(!json["data"].is_null());
    assert!(json["data"]["objects_ordered"].is_boolean());
    // bbox should be present (we have nodes with coordinates)
    assert!(json["data"]["bbox"].is_array());
    // metadata section
    assert!(!json["metadata"].is_null());
    assert!(json["metadata"]["all_objects"]["version"].is_boolean());
    assert!(json["metadata"]["some_objects"]["version"].is_boolean());
}

#[test]
fn inspect_get_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(&input, false, false, false, true, false)
        .expect("inspect");

    // Basic keys
    assert_eq!(report.get_value("file.format"), Some("PBF".to_string()));
    assert_eq!(report.get_value("elements.total"), Some("6".to_string()));
    assert_eq!(report.get_value("elements.nodes"), Some("3".to_string()));
    assert_eq!(report.get_value("indexed"), Some("true".to_string()));

    // Extended keys
    assert!(report.get_value("data.objects_ordered").is_some());
    assert!(report.get_value("metadata.all_objects.version").is_some());

    // Unknown key
    assert!(report.get_value("nonexistent.key").is_none());
}
