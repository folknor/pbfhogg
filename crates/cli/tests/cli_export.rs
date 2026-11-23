//! CLI coverage for streaming GeoJSON export.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use crate::invoker::CliInvoker;
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};

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

fn write_node_with_timestamp(path: &Path, timestamp: i64) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(&HeaderBuilder::new().build().unwrap())
        .unwrap();
    let mut builder = BlockBuilder::new();
    let meta = Metadata {
        version: 1,
        timestamp,
        changeset: 1,
        uid: 1,
        user: "mapper",
        visible: true,
    };
    builder.add_node(5, 0, 0, [("name", "dated")], Some(&meta));
    write_block(&mut writer, &mut builder);
    writer.flush().unwrap();
}

/// `@timestamp` is RFC 3339: the last second of year 9999 is written, the
/// first of year 10000 is refused naming the element, not spelled with a
/// five-digit year.
#[test]
fn metadata_timestamp_beyond_year_9999_is_refused() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let temp = crate::common::test_scratch();
    let last = temp.path().join("last.pbf");
    write_node_with_timestamp(&last, 253_402_300_799);
    let output = CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(last.as_os_str())
        .arg("--type")
        .arg("node")
        .arg("--metadata")
        .assert_success();
    assert_eq!(
        features(&output.stdout)[0]["properties"]["@timestamp"],
        "9999-12-31T23:59:59Z"
    );
    let beyond = temp.path().join("beyond.pbf");
    write_node_with_timestamp(&beyond, 253_402_300_800);
    CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(beyond.as_os_str())
        .arg("--type")
        .arg("node")
        .arg("--metadata")
        .assert_failure()
        .assert_stderr_contains("node 5")
        .assert_stderr_contains("outside years 0000 to 9999");
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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let temp = crate::common::test_scratch();
    let input = temp.path().join("input.pbf");
    write_fixture(&input, true);

    let output = CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(input.as_os_str())
        .arg("--type")
        .arg("node")
        .arg("amenity")
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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let temp = crate::common::test_scratch();
    let input = temp.path().join("input.pbf");
    write_fixture(&input, true);
    let output = CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(input.as_os_str())
        .arg("--type")
        .arg("way")
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
fn positions_outside_the_geographic_domain_are_skipped_and_counted() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let temp = crate::common::test_scratch();
    let input = temp.path().join("input.pbf");
    let file = std::fs::File::create(&input).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(
            &HeaderBuilder::new()
                .optional_feature("LocationsOnWays")
                .build()
                .unwrap(),
        )
        .unwrap();
    let mut builder = BlockBuilder::new();
    builder.add_node(1, 1_000_000_000, 0, [("name", "lat 100")], None);
    builder.add_node(2, 900_000_000, -1_800_000_000, [("name", "corner")], None);
    write_block(&mut writer, &mut builder);
    // libosmium's undefined location on both axes.
    builder.add_way_with_locations(
        10,
        [("highway", "primary")],
        &[1, 2],
        &[(0, 0), (i32::MAX, i32::MAX)],
        None,
    );
    write_block(&mut writer, &mut builder);
    writer.flush().unwrap();

    let output = CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(input.as_os_str())
        .assert_success();
    let values = features(&output.stdout);
    assert_eq!(values.len(), 1);
    assert_eq!(
        values[0]["geometry"]["coordinates"],
        serde_json::json!([-180, 90])
    );
    output.assert_stderr_contains("1 invalid nodes, and 1 invalid ways");
}

#[test]
fn filters_properties_bbox_and_collection_compose() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let temp = crate::common::test_scratch();
    let input = temp.path().join("input.pbf");
    write_fixture(&input, true);

    let expression_file = temp.path().join("expressions.txt");
    std::fs::write(&expression_file, "# exported roads\nhighway\n").unwrap();

    let output = CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(input.as_os_str())
        .arg("--type")
        .arg("way")
        .arg("--format")
        .arg("geojson")
        .arg("--properties")
        .arg("highway")
        .arg("--bbox")
        .arg("12.4,55.6,12.7,55.9")
        .arg("--expressions")
        .arg(expression_file.as_os_str())
        .arg("missing_tag")
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

    let empty = CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(input.as_os_str())
        .arg("--type")
        .arg("node")
        .arg("--format")
        .arg("geojson")
        .arg("missing_tag")
        .assert_success();
    assert_eq!(
        empty.stdout,
        b"{\"type\":\"FeatureCollection\",\"features\":[]}\n"
    );

    let nodes = CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(input.as_os_str())
        .arg("--type")
        .arg("node")
        .arg("--bbox")
        .arg("12.4,55.6,12.7,55.9")
        .assert_success();
    let nodes = features(&nodes.stdout);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["properties"]["@id"], 1);
}

#[test]
fn metadata_types_and_locations_gate() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let temp = crate::common::test_scratch();
    let enriched = temp.path().join("enriched.pbf");
    write_fixture(&enriched, true);
    let output = CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(enriched.as_os_str())
        .arg("--type")
        .arg("node")
        .arg("--metadata")
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
    let output = CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(no_metadata.as_os_str())
        .arg("--type")
        .arg("node")
        .arg("--metadata")
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
    CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(raw.as_os_str())
        .arg("--type")
        .arg("way")
        .assert_failure()
        .assert_stderr_contains("input PBF is missing required feature: LocationsOnWays");
    CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(raw.as_os_str())
        .assert_failure()
        // The library half of the message must stay flag-free; the
        // `--type node` half is added by the CLI, not by `ErrorKind`.
        .assert_stderr_contains(
            "way geometry needs an input carrying inline way-node coordinates; \
             use --type node to export nodes only",
        );
    CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(raw.as_os_str())
        .arg("--type")
        .arg("node")
        .assert_success();
    CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(raw.as_os_str())
        .arg("--type")
        .arg("relation")
        .assert_failure()
        .assert_stderr_contains("export cannot export relations");
}

#[test]
fn file_output_is_guarded_against_input_aliases() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let temp = crate::common::test_scratch();
    let input = temp.path().join("input.pbf");
    write_fixture(&input, true);
    let before = std::fs::read(&input).unwrap();
    CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(input.as_os_str())
        .arg("--output")
        .arg(input.as_os_str())
        .assert_failure()
        .assert_stderr_contains("aliases input");
    assert_eq!(std::fs::read(&input).unwrap(), before);

    let hardlink = temp.path().join("hardlink.pbf");
    std::fs::hard_link(&input, &hardlink).unwrap();
    CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(input.as_os_str())
        .arg("--output")
        .arg(hardlink.as_os_str())
        .assert_failure()
        .assert_stderr_contains("aliases input");
    assert_eq!(std::fs::read(&input).unwrap(), before);

    std::fs::remove_file(&hardlink).unwrap();
    let output = temp.path().join("features.geojson");
    CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(input.as_os_str())
        .arg("--type")
        .arg("node")
        .arg("--output")
        .arg(output.as_os_str())
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
    CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(corrupt.as_os_str())
        .arg("--format")
        .arg("geojson")
        .arg("--output")
        .arg(partial.as_os_str())
        .assert_failure();
    assert!(!partial.exists(), "partial output survived decode failure");

    // A failing export must not touch a pre-existing file that merely
    // shares the output name - not delete it, not truncate it.
    let precious = temp.path().join("important.geojson");
    std::fs::write(&precious, b"user data").unwrap();
    CliInvoker::new(&invocation_scratch)
        .arg("export")
        .arg(corrupt.as_os_str())
        .arg("--format")
        .arg("geojson")
        .arg("--output")
        .arg(precious.as_os_str())
        .assert_failure();
    assert_eq!(
        std::fs::read(&precious).unwrap(),
        b"user data",
        "a failing export destroyed a pre-existing file of the output name"
    );
}

/// `pbfhogg export ... | head` once `head` has gone: only an error that is
/// itself the stdout write ends the run quietly with success. The truncated
/// input is the case that used to pass for success: the FeatureCollection
/// prefix sat in the stdout buffer when the truncation was detected, and the
/// buffer's drop-time flush hit the closed pipe on the way out.
#[test]
fn closed_stdout_keeps_an_input_error_and_ends_a_good_run_quietly() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let temp = crate::common::test_scratch();
    let input = temp.path().join("input.pbf");
    write_fixture(&input, false);

    let good = CliInvoker::new(&invocation_scratch)
        .stdout_closed()
        .arg("export")
        .arg(input.as_os_str())
        .arg("--format")
        .arg("geojson")
        .arg("--type")
        .arg("node")
        .run();
    assert_eq!(
        good.status.code(),
        Some(0),
        "stderr:\n{}",
        good.stderr_str()
    );
    assert!(
        !good.stderr_str().contains("Error:"),
        "{}",
        good.stderr_str()
    );

    let corrupt = temp.path().join("corrupt.pbf");
    let mut bytes = std::fs::read(&input).unwrap();
    bytes.truncate(bytes.len() - 5);
    std::fs::write(&corrupt, bytes).unwrap();
    let failed = CliInvoker::new(&invocation_scratch)
        .stdout_closed()
        .arg("export")
        .arg(corrupt.as_os_str())
        .arg("--format")
        .arg("geojson")
        .arg("--type")
        .arg("node")
        .run();
    assert_eq!(
        failed.status.code(),
        Some(crate::invoker::EXIT_FAILED),
        "a truncated input is a failure whatever stdout does; stderr:\n{}",
        failed.stderr_str(),
    );
    assert!(
        failed.stderr_str().contains("Error:"),
        "{}",
        failed.stderr_str()
    );
}
