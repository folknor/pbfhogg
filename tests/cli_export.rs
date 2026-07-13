//! CLI coverage for streaming GeoJSON export.

#![cfg(feature = "commands")]
#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;

use common::cli::CliInvoker;
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};
use tempfile::TempDir;

fn write_block<W: std::io::Write>(writer: &mut PbfWriter<W>, builder: &mut BlockBuilder) {
    if let Some(bytes) = builder.take().unwrap() {
        writer.write_primitive_block(bytes).unwrap();
    }
}

fn write_fixture(path: &Path, locations_on_ways: bool) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let mut header = HeaderBuilder::new();
    if locations_on_ways {
        header = header.optional_feature("LocationsOnWays");
    }
    writer.write_header(&header.build().unwrap()).unwrap();

    let metadata = Metadata {
        version: 3,
        timestamp: 1_700_000_000,
        changeset: 42,
        uid: 7,
        user: "mapper",
        visible: true,
    };
    let mut builder = BlockBuilder::new();
    builder.add_node(
        1,
        557_000_000,
        125_000_000,
        [
            ("name", "quoted \"value\" \\ path"),
            ("name", "later duplicate"),
            ("amenity", "cafe"),
            ("@id", "source-id"),
            ("@type", "source-type"),
        ],
        Some(&metadata),
    );
    builder.add_node(2, 540_000_000, 140_000_000, [], None);
    builder.add_node(3, 540_000_000, 140_000_000, [("name", "outside")], None);
    write_block(&mut writer, &mut builder);

    if locations_on_ways {
        builder.add_way_with_locations(
            10,
            [("highway", "primary"), ("name", "main")],
            &[1, 2],
            &[(557_000_000, 125_000_000), (558_000_000, 126_000_000)],
            None,
        );
        builder.add_way_with_locations(
            11,
            [("building", "yes")],
            &[3, 4, 5, 3],
            &[(0, 0), (10_000_000, 0), (0, 10_000_000), (0, 0)],
            None,
        );
        builder.add_way_with_locations(
            12,
            [("building", "yes"), ("area", "no")],
            &[3, 4, 5, 3],
            &[(0, 0), (10_000_000, 0), (0, 10_000_000), (0, 0)],
            None,
        );
        builder.add_way_with_locations(
            13,
            [("area", "yes")],
            &[3, 4, 5, 3],
            &[(0, 0), (10_000_000, 0), (0, 10_000_000), (0, 0)],
            None,
        );
        builder.add_way_with_locations(
            14,
            [("highway", "service")],
            &[3, 4, 5, 3],
            &[(0, 0), (10_000_000, 0), (0, 10_000_000), (0, 0)],
            None,
        );
        builder.add_way(15, [("building", "yes")], &[20, 21, 22, 20], None);
        builder.add_way_with_locations(
            16,
            [],
            &[30, 31],
            &[(500_000_000, 100_000_000), (501_000_000, 101_000_000)],
            None,
        );
        write_block(&mut writer, &mut builder);
    }
    writer.flush().unwrap();
}

fn write_node_without_metadata(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(&HeaderBuilder::new().build().unwrap())
        .unwrap();
    let mut builder = BlockBuilder::new();
    builder.add_node(99, 550_000_000, 120_000_000, [("name", "plain")], None);
    write_block(&mut writer, &mut builder);
    writer.flush().unwrap();
}

fn features(output: &[u8]) -> Vec<serde_json::Value> {
    output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

#[test]
fn default_sequence_is_exact_and_escaped() {
    let temp = TempDir::new().unwrap();
    let input = temp.path().join("input.pbf");
    write_fixture(&input, true);

    let output = CliInvoker::new()
        .args([
            "export",
            input.to_str().unwrap(),
            "--type",
            "node",
            "amenity",
        ])
        .assert_success();
    assert!(!output.stdout.contains(&0x1e));
    assert_eq!(
        output.stdout_str(),
        "{\"type\":\"Feature\",\"geometry\":{\"type\":\"Point\",\"coordinates\":[12.5,55.7]},\"properties\":{\"@id\":1,\"@type\":\"node\",\"name\":\"quoted \\\"value\\\" \\\\ path\",\"amenity\":\"cafe\"}}\n"
    );
    output.assert_stderr_contains("Exported 1 features");
    output.assert_stderr_contains("1 untagged nodes");
}

#[test]
fn way_geometry_and_area_heuristic() {
    let temp = TempDir::new().unwrap();
    let input = temp.path().join("input.pbf");
    write_fixture(&input, true);
    let output = CliInvoker::new()
        .args(["export", input.to_str().unwrap(), "--type", "way"])
        .assert_success();
    let values = features(&output.stdout);
    assert_eq!(values.len(), 5);
    output.assert_stderr_contains("1 invalid ways");
    output.assert_stderr_contains("1 untagged ways");
    assert_eq!(values[0]["geometry"]["type"], "LineString");
    assert_eq!(values[1]["geometry"]["type"], "Polygon");
    assert_eq!(
        values[1]["geometry"]["coordinates"][0],
        serde_json::json!([[0, 0], [1, 0], [0, 1], [0, 0]])
    );
    assert_eq!(values[2]["geometry"]["type"], "LineString");
    assert_eq!(values[3]["geometry"]["type"], "Polygon");
    assert_eq!(values[4]["geometry"]["type"], "LineString");
}

#[test]
fn filters_properties_bbox_and_collection_compose() {
    let temp = TempDir::new().unwrap();
    let input = temp.path().join("input.pbf");
    write_fixture(&input, true);

    let expression_file = temp.path().join("expressions.txt");
    std::fs::write(&expression_file, "# exported roads\nhighway\n").unwrap();

    let output = CliInvoker::new()
        .args([
            "export",
            input.to_str().unwrap(),
            "--type",
            "way",
            "--format",
            "geojson",
            "--properties",
            "highway",
            "--bbox",
            "12.4,55.6,12.7,55.9",
            "--expressions",
            expression_file.to_str().unwrap(),
            "missing_tag",
        ])
        .assert_success();
    let collection: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(collection["type"], "FeatureCollection");
    assert_eq!(collection["features"].as_array().unwrap().len(), 1);
    assert_eq!(collection["features"][0]["properties"]["@id"], 10);
    assert_eq!(
        collection["features"][0]["properties"]["highway"],
        "primary"
    );
    assert!(
        collection["features"][0]["properties"]
            .get("name")
            .is_none()
    );

    let empty = CliInvoker::new()
        .args([
            "export",
            input.to_str().unwrap(),
            "--type",
            "node",
            "--format",
            "geojson",
            "missing_tag",
        ])
        .assert_success();
    assert_eq!(
        empty.stdout,
        b"{\"type\":\"FeatureCollection\",\"features\":[]}\n"
    );

    let nodes = CliInvoker::new()
        .args([
            "export",
            input.to_str().unwrap(),
            "--type",
            "node",
            "--bbox",
            "12.4,55.6,12.7,55.9",
        ])
        .assert_success();
    let nodes = features(&nodes.stdout);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["properties"]["@id"], 1);
}

#[test]
fn metadata_types_and_locations_gate() {
    let temp = TempDir::new().unwrap();
    let enriched = temp.path().join("enriched.pbf");
    write_fixture(&enriched, true);
    let output = CliInvoker::new()
        .args([
            "export",
            enriched.to_str().unwrap(),
            "--type",
            "node",
            "--metadata",
        ])
        .assert_success();
    let value = &features(&output.stdout)[0]["properties"];
    assert_eq!(value["@version"], 3);
    assert_eq!(value["@timestamp"], "2023-11-14T22:13:20Z");
    assert_eq!(value["@changeset"], 42);
    assert_eq!(value["@uid"], 7);
    assert_eq!(value["@user"], "mapper");
    assert_eq!(value["@visible"], true);

    let no_metadata = temp.path().join("no-metadata.pbf");
    write_node_without_metadata(&no_metadata);
    let output = CliInvoker::new()
        .args([
            "export",
            no_metadata.to_str().unwrap(),
            "--type",
            "node",
            "--metadata",
        ])
        .assert_success();
    let properties = &features(&output.stdout)[0]["properties"];
    for key in [
        "@version",
        "@timestamp",
        "@changeset",
        "@uid",
        "@user",
        "@visible",
    ] {
        assert!(
            properties.get(key).is_none(),
            "unexpected metadata key {key}"
        );
    }

    let raw = temp.path().join("raw.pbf");
    write_fixture(&raw, false);
    CliInvoker::new()
        .args(["export", raw.to_str().unwrap(), "--type", "way"])
        .assert_failure()
        .assert_stderr_contains("input PBF is missing required feature: LocationsOnWays");
    CliInvoker::new()
        .args(["export", raw.to_str().unwrap()])
        .assert_failure()
        .assert_stderr_contains("use --type node or an altw input");
    CliInvoker::new()
        .args(["export", raw.to_str().unwrap(), "--type", "node"])
        .assert_success();
    CliInvoker::new()
        .args(["export", raw.to_str().unwrap(), "--type", "relation"])
        .assert_failure()
        .assert_stderr_contains("possible values: node, way");
}

#[test]
fn file_output_is_guarded_against_input_aliases() {
    let temp = TempDir::new().unwrap();
    let input = temp.path().join("input.pbf");
    write_fixture(&input, true);
    let before = std::fs::read(&input).unwrap();
    CliInvoker::new()
        .args([
            "export",
            input.to_str().unwrap(),
            "--output",
            input.to_str().unwrap(),
        ])
        .assert_failure()
        .assert_stderr_contains("aliases input");
    assert_eq!(std::fs::read(&input).unwrap(), before);

    let hardlink = temp.path().join("hardlink.pbf");
    std::fs::hard_link(&input, &hardlink).unwrap();
    CliInvoker::new()
        .args([
            "export",
            input.to_str().unwrap(),
            "--output",
            hardlink.to_str().unwrap(),
        ])
        .assert_failure()
        .assert_stderr_contains("aliases input");
    assert_eq!(std::fs::read(&input).unwrap(), before);

    std::fs::remove_file(&hardlink).unwrap();
    let output = temp.path().join("features.geojson");
    CliInvoker::new()
        .args([
            "export",
            input.to_str().unwrap(),
            "--type",
            "node",
            "--output",
            output.to_str().unwrap(),
        ])
        .assert_success();
    assert!(
        std::fs::read_to_string(output)
            .unwrap()
            .starts_with("{\"type\":\"Feature\"")
    );

    let corrupt = temp.path().join("corrupt.pbf");
    let mut bytes = before;
    bytes.truncate(bytes.len() - 5);
    std::fs::write(&corrupt, bytes).unwrap();
    let partial = temp.path().join("partial.geojson");
    CliInvoker::new()
        .args([
            "export",
            corrupt.to_str().unwrap(),
            "--format",
            "geojson",
            "--output",
            partial.to_str().unwrap(),
        ])
        .assert_failure();
    assert!(!partial.exists(), "partial output survived decode failure");
}
