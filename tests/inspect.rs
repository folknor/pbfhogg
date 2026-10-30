use crate::common::{TestNode, TestRelation, TestWay, generate_nodes, write_multi_block_test_pbf};
use pbfhogg::block_builder::{self, BlockBuilder};
use pbfhogg::writer::{Compression, PbfWriter};

fn write_simple_pbf(path: &std::path::Path) {
    crate::common::write_test_pbf(
        path,
        &[
            TestNode {
                id: 1,
                lat: 510_000_000,
                lon: -1_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 520_000_000,
                lon: -2_000_000,
                tags: vec![("name", "foo")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 530_000_000,
                lon: -3_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2, 3],
                tags: vec![("highway", "residential")],
                meta: None,
            },
            TestWay {
                id: 11,
                refs: vec![2, 3],
                tags: vec![],
                meta: None,
            },
        ],
        &[TestRelation {
            id: 100,
            members: vec![],
            tags: vec![("type", "route")],
            meta: None,
        }],
    );
}

fn write_nodes_blocks(path: &std::path::Path, blocks: &[usize]) {
    let header = block_builder::HeaderBuilder::new()
        .build()
        .expect("build header");
    let mut writer = PbfWriter::to_path(
        path,
        Compression::default(),
        &header,
        pbfhogg::host::Executor::Ambient,
    )
    .expect("create writer");
    let mut bb = BlockBuilder::new();
    let mut next_id: i64 = 1;
    for &count in blocks {
        for _ in 0..count {
            bb.add_node(
                next_id,
                500_000_000,
                100_000_000,
                std::iter::empty::<(&str, &str)>(),
                None,
            );
            next_id += 1;
        }
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(bytes).expect("write block");
        }
    }
    writer.flush().expect("flush");
}

fn inspect_opts(show_id_ranges: bool, distrust_metadata: bool) -> pbfhogg::inspect::InspectOptions {
    pbfhogg::inspect::InspectOptions {
        show_blocks: false,
        show_id_ranges,
        show_locations: false,
        extended: false,
        distrust_metadata,
        direct_io: false,
    }
}

/// `inspect` is the tool you reach for when a file is suspect, and its
/// index-only fast path answers counts and ID ranges out of that same suspect
/// file's `indexdata` without decoding a single element. On a file whose index
/// lies, the report is a transcription of the lie. `--distrust-metadata` is
/// the way to make it measure instead.
#[test]
fn inspect_distrust_metadata_measures_instead_of_quoting_the_index() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let lying = dir.path().join("lying.osm.pbf");

    // Three nodes, two ways, one relation, written through the pipelined
    // writer so every blob carries indexdata.
    write_indexed_simple_pbf(&honest);

    // Honest file: both modes agree, which is the counterpart that keeps the
    // divergence below meaningful.
    let quoted = pbfhogg::inspect::inspect(
        &honest,
        &inspect_opts(false, false),
        &crate::common::test_host(),
    )
    .expect("inspect");
    let measured = pbfhogg::inspect::inspect(
        &honest,
        &inspect_opts(false, true),
        &crate::common::test_host(),
    )
    .expect("inspect");
    assert_eq!(quoted.to_json(None)["elements"]["nodes"], 3);
    assert_eq!(measured.to_json(None)["elements"]["nodes"], 3);

    pbfhogg::degrade::degrade(
        &honest,
        crate::common::out(&lying),
        pbfhogg::degrade::DegradeFlags {
            corrupt_indexdata: true,
            ..Default::default()
        },
        pbfhogg::degrade::DEFAULT_BLOCK_CAP,
        &pbfhogg::write::WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("degrade --corrupt-indexdata");

    let quoted = pbfhogg::inspect::inspect(
        &lying,
        &inspect_opts(false, false),
        &crate::common::test_host(),
    )
    .expect("inspect");
    assert_eq!(
        quoted.to_json(None)["elements"]["nodes"],
        1_000_003_u64,
        "the default fast path reports the file's own inflated claim as a measurement"
    );

    let measured = pbfhogg::inspect::inspect(
        &lying,
        &inspect_opts(false, true),
        &crate::common::test_host(),
    )
    .expect("inspect");
    let json = measured.to_json(None);
    assert_eq!(
        json["elements"]["nodes"], 3,
        "--distrust-metadata must decode and count, not read the index"
    );
    assert_eq!(json["elements"]["ways"], 2);
    assert_eq!(json["elements"]["relations"], 1);
}

fn write_indexed_simple_pbf(path: &std::path::Path) {
    crate::common::write_indexed_pbf(
        path,
        &[
            TestNode {
                id: 1,
                lat: 510_000_000,
                lon: -1_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 520_000_000,
                lon: -2_000_000,
                tags: vec![("name", "foo")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 530_000_000,
                lon: -3_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2, 3],
                tags: vec![("highway", "residential")],
                meta: None,
            },
            TestWay {
                id: 11,
                refs: vec![2, 3],
                tags: vec![],
                meta: None,
            },
        ],
        &[TestRelation {
            id: 100,
            members: vec![],
            tags: vec![("type", "route")],
            meta: None,
        }],
    );
}

#[test]
fn inspect_json_base() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(
        &input,
        &pbfhogg::inspect::InspectOptions {
            show_blocks: false,
            show_id_ranges: false,
            show_locations: false,
            extended: false,
            distrust_metadata: false,
            direct_io: false,
        },
        &crate::common::test_host(),
    )
    .expect("inspect");
    let json = report.to_json(None);

    // Schema version
    assert_eq!(json["schema_version"], 2);

    // File metadata
    assert!(
        json["file"]
            .as_str()
            .expect("value")
            .contains("test.osm.pbf")
    );
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
    // test.osm.pbf is canonical: one segment per kind, each listed whole.
    assert_eq!(json["ordering"]["standard"], true);
    assert_eq!(json["ordering"]["segment_count"], 3);
    let head = json["ordering"]["segments_head"]
        .as_array()
        .expect("segments_head");
    let kinds: Vec<&str> = head
        .iter()
        .map(|s| s["kind"].as_str().expect("kind"))
        .collect();
    assert_eq!(kinds, ["nodes", "ways", "relations"]);
    assert_eq!(head[0]["first_block"], 1);
    assert!(json["ordering"].get("sequence").is_none());

    // Optional fields should be null when not requested
    assert!(json["id_ranges"].is_null());
    assert!(json["blocks_detail"].is_null());
    assert!(json["locations"].is_null());
}

#[test]
fn inspect_json_with_id_ranges() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(
        &input,
        &pbfhogg::inspect::InspectOptions {
            show_blocks: false,
            show_id_ranges: true,
            show_locations: false,
            extended: false,
            distrust_metadata: false,
            direct_io: false,
        },
        &crate::common::test_host(),
    )
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(
        &input,
        &pbfhogg::inspect::InspectOptions {
            show_blocks: true,
            show_id_ranges: false,
            show_locations: false,
            extended: false,
            distrust_metadata: false,
            direct_io: false,
        },
        &crate::common::test_host(),
    )
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(
        &input,
        &pbfhogg::inspect::InspectOptions {
            show_blocks: true,
            show_id_ranges: false,
            show_locations: false,
            extended: false,
            distrust_metadata: false,
            direct_io: false,
        },
        &crate::common::test_host(),
    )
    .expect("inspect");

    let json_all = report.to_json(Some(0));
    let total = json_all["blocks_detail"].as_array().expect("value").len();

    // With limit=1, if total > 2, should get first 1 + last 1 = 2
    if total > 2 {
        let json_limited = report.to_json(Some(1));
        let limited = json_limited["blocks_detail"]
            .as_array()
            .expect("value")
            .len();
        assert_eq!(limited, 2);
    }
}

#[test]
fn inspect_json_combined_flags() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(
        &input,
        &pbfhogg::inspect::InspectOptions {
            show_blocks: true,
            show_id_ranges: true,
            show_locations: false,
            extended: false,
            distrust_metadata: false,
            direct_io: false,
        },
        &crate::common::test_host(),
    )
    .expect("inspect");
    let json = report.to_json(Some(0));

    // All optional fields should be present
    assert!(!json["id_ranges"].is_null());
    assert!(json["blocks_detail"].is_array());
    // locations still null (not requested)
    assert!(json["locations"].is_null());

    // Verify deterministic key set - all top-level keys present
    let obj = json.as_object().expect("value");
    for key in &[
        "schema_version",
        "file",
        "file_size",
        "header",
        "indexed",
        "blocks",
        "elements",
        "ordering",
        "id_ranges",
        "anomalies_only",
        "blocks_detail",
        "locations",
    ] {
        assert!(obj.contains_key(*key), "missing key: {key}");
    }
}

#[test]
fn inspect_json_blocks_anomalies_only() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("anomalies.osm.pbf");
    // Three node blocks with element counts [100, 100, 10].
    // Median=100, so the 10-element block is anomalously small (<50% of median).
    write_nodes_blocks(&input, &[100, 100, 10]);

    let report = pbfhogg::inspect::inspect(
        &input,
        &pbfhogg::inspect::InspectOptions {
            show_blocks: true,
            show_id_ranges: false,
            show_locations: false,
            extended: false,
            distrust_metadata: false,
            direct_io: false,
        },
        &crate::common::test_host(),
    )
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    // extended=true forces full decode and collects timestamps, bbox, metadata
    let report = pbfhogg::inspect::inspect(
        &input,
        &pbfhogg::inspect::InspectOptions {
            show_blocks: false,
            show_id_ranges: false,
            show_locations: false,
            extended: true,
            distrust_metadata: false,
            direct_io: false,
        },
        &crate::common::test_host(),
    )
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("test.osm.pbf");
    write_simple_pbf(&input);

    let report = pbfhogg::inspect::inspect(
        &input,
        &pbfhogg::inspect::InspectOptions {
            show_blocks: false,
            show_id_ranges: false,
            show_locations: false,
            extended: true,
            distrust_metadata: false,
            direct_io: false,
        },
        &crate::common::test_host(),
    )
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

// ---------------------------------------------------------------------------
// Parallel classify parity for inspect::node_stats
// ---------------------------------------------------------------------------
//
// `node_stats` (src/commands/inspect/node_stats.rs:220) dispatches
// per-node scanning through `parallel_classify_accumulate` with a
// `jobs` override (jobs=0 -> auto, jobs>0 -> exact worker count). Each
// worker maintains per-blob `CoordStats` that get merged at the end.
// With a single-blob fixture every worker except one is idle; this
// test forces 4 node blobs and asserts jobs=1 and jobs=4 produce the
// same summary numbers.

#[test]
fn node_stats_parallel_classify_parity() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");

    // 40 sequential nodes at block_size=10 -> 4 node blobs. No ways,
    // no relations (node_stats only looks at node blobs).
    let nodes = generate_nodes(40, 1);
    write_multi_block_test_pbf(&input, &nodes, &[], &[], 10);

    let seq = pbfhogg::inspect::node_stats::node_stats(
        &input,
        false,
        true,
        pbfhogg::Parallelism::from_jobs(1),
        &crate::common::test_host(),
    )
    .expect("node_stats seq");
    let par = pbfhogg::inspect::node_stats::node_stats(
        &input,
        false,
        true,
        pbfhogg::Parallelism::from_jobs(4),
        &crate::common::test_host(),
    )
    .expect("node_stats par");

    // Raw totals must match exactly. These are simple sums /
    // min-reductions over blobs; if workers double-count or drop a
    // blob, these will diverge first.
    assert_eq!(seq.node_count, par.node_count, "node_count parity");
    assert_eq!(seq.min_lat, par.min_lat, "min_lat parity");
    assert_eq!(seq.max_lat, par.max_lat, "max_lat parity");
    assert_eq!(seq.min_lon, par.min_lon, "min_lon parity");
    assert_eq!(seq.max_lon, par.max_lon, "max_lon parity");
    assert_eq!(seq.node_count, 40, "sanity: all 40 nodes scanned");

    // `avg_bits` intentionally NOT asserted for parity: with a tiny
    // fixture (1 blob per worker at jobs=4) the jobs=1 path sees all
    // 4 blobs on one worker vs the jobs=4 path where each worker sees
    // one blob, and the sub-block scalar bucketing resolution inside
    // CoordStats can produce a different weighted mean on two
    // independent 14-15 bit buckets versus a single merged scan. The
    // node_count / min / max assertions above already pin that every
    // blob's bytes were classified exactly once.
}

/// `inspect --nodes` schedules node blobs on the claim, so a way blob
/// claiming nodes is decoded and refused (its ways were skipped by kind
/// before: the lie is made visible, the node statistics are unchanged).
/// The honest file runs.
#[test]
fn node_stats_refuses_a_way_blob_claiming_nodes() {
    const WAY_BLOB: usize = 2; // header, nodes, ways
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    crate::common::write_test_pbf_sorted(
        &honest,
        &generate_nodes(4, 1),
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![],
            meta: None,
        }],
        &[],
    );
    let run = |path: &std::path::Path| {
        pbfhogg::inspect::node_stats::node_stats(
            path,
            false,
            false,
            pbfhogg::Parallelism::from_jobs(1),
            &crate::common::test_host(),
        )
    };
    assert_eq!(run(&honest).expect("honest claims run").node_count, 4);

    let bytes = std::fs::read(&honest).expect("read the fixture");
    let lying =
        crate::common::adversarial::mutate_blob_header_indexdata(&bytes, WAY_BLOB, |index| {
            assert_eq!(index[1], 1, "frame {WAY_BLOB} is the way blob");
            index[1] = 0;
        });
    let forged = dir.path().join("forged.osm.pbf");
    std::fs::write(&forged, lying).expect("write the forged file");
    let Err(err) = run(&forged) else {
        panic!("a lying claim must be refused");
    };
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::IndexdataMismatch { .. }),
        "{err:?}"
    );
    assert_eq!(
        err.kind_contradiction(),
        Some(pbfhogg::KindContradiction {
            claimed: pbfhogg::ElementKind::Node,
            found: pbfhogg::ElementKind::Way,
        })
    );
    assert_eq!(err.blob_index(), Some(WAY_BLOB as u64));
}

/// Regression: `show_element` must not early-exit on `idx.min_id > target_id`
/// unless the header declares `Sort.Type_then_ID`.
///
/// On an unsorted PBF, a later same-kind blob can legitimately have a
/// smaller `min_id` than an earlier blob. The pre-fix early-exit at
/// show_element.rs:53-57 assumed sorted layout unconditionally and
/// returned "not found" as soon as it saw a blob whose `min_id`
/// exceeded the target, skipping every later blob - including the one
/// actually containing the target.
///
/// Fixture layout (unsorted by design, no `Sort.Type_then_ID`):
///   blob 0: nodes 100, 101 (min=100)
///   blob 1: nodes  50,  51 (min= 50, overlaps/precedes blob 0)
/// Target: node 51 - pre-fix: returns false; post-fix: returns true.
#[test]
fn show_element_unsorted_pbf_finds_target_in_later_blob() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("unsorted.osm.pbf");

    // Emit nodes in order 100, 101, 50, 51 with block_size=2 and the
    // `sorted=false` header so the writer produces two 2-node blobs
    // without declaring `Sort.Type_then_ID` - a layout that's legal
    // for non-canonical PBFs (hand-edited fixtures, extract outputs
    // before sort, etc.) but would wrongly trigger `show_element`'s
    // min_id-based early-exit.
    let nodes = vec![
        TestNode {
            id: 100,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 101,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 50,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 51,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        },
    ];
    crate::common::write_test_pbf_impl(&input, &nodes, &[], &[], false, Some(2));

    use pbfhogg::ElementKind;
    use pbfhogg::inspect::show_element_to;
    let mut out: Vec<u8> = Vec::new();
    let found = show_element_to(
        &input,
        ElementKind::Node,
        51,
        false,
        &mut out,
        &crate::common::test_host(),
    )
    .expect("show_element");
    assert!(
        found,
        "node 51 lives in the second blob; must be found on unsorted PBF"
    );
    let text = String::from_utf8(out).expect("utf8 output");
    assert!(text.starts_with("node/51\n"), "unexpected output: {text:?}");

    // Sanity: also find 100 (first blob).
    let mut out100: Vec<u8> = Vec::new();
    let found100 = show_element_to(
        &input,
        ElementKind::Node,
        100,
        false,
        &mut out100,
        &crate::common::test_host(),
    )
    .expect("show_element 100");
    assert!(found100, "node 100 is in the first blob");
    let text100 = String::from_utf8(out100).expect("utf8 output");
    assert!(
        text100.starts_with("node/100\n"),
        "unexpected output: {text100:?}"
    );

    // Miss path still works: id 999 doesn't exist anywhere.
    let mut out_miss: Vec<u8> = Vec::new();
    let miss = show_element_to(
        &input,
        ElementKind::Node,
        999,
        false,
        &mut out_miss,
        &crate::common::test_host(),
    )
    .expect("show miss");
    assert!(!miss, "node 999 shouldn't exist");
    assert!(out_miss.is_empty(), "a miss must write nothing");
}

/// The report's header lines print the file name and each header feature
/// as a quoted literal: a newline in a file name cannot forge a label line.
#[test]
fn report_header_quotes_file_name_and_features() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("x\nProgram:  forged.osm.pbf");
    crate::common::write_test_pbf_impl(&input, &generate_nodes(2, 1), &[], &[], true, None);

    let mut report = pbfhogg::inspect::inspect(
        &input,
        &pbfhogg::inspect::InspectOptions {
            show_blocks: false,
            show_id_ranges: false,
            show_locations: false,
            extended: false,
            distrust_metadata: false,
            direct_io: false,
        },
        &crate::common::test_host(),
    )
    .expect("inspect");
    let mut out: Vec<u8> = Vec::new();
    report.write_report(&mut out, None).expect("write_report");
    let text = String::from_utf8(out).expect("utf8 output");
    assert!(
        text.starts_with("File:     \"x\\nProgram:  forged.osm.pbf\" ("),
        "unexpected output: {text:?}"
    );
    assert!(
        !text.contains("\nProgram:  forged"),
        "forged line: {text:?}"
    );
    assert!(
        text.contains("\nFeatures: \"Sort.Type_then_ID\""),
        "unexpected output: {text:?}"
    );
    // `--get` stays the raw value: a scripting interface, not framed text.
    assert_eq!(
        report.get_value("file.name").as_deref(),
        Some("x\nProgram:  forged.osm.pbf")
    );
}

/// `--show` prints every key, value, user and role as a quoted, escaped
/// literal, so a newline, tab or quote in string-table text cannot split a
/// line or forge another one.
#[test]
fn show_element_quotes_and_escapes_text() {
    use crate::common::{TestMember, TestMeta};
    use pbfhogg::ElementKind;
    use pbfhogg::MemberId;
    use pbfhogg::inspect::show_element_to;

    let dir = crate::common::test_scratch();
    let input = dir.path().join("text.osm.pbf");
    let meta = TestMeta {
        user: "a\nuser: forged",
        ..TestMeta::default()
    };
    crate::common::write_test_pbf(
        &input,
        &[TestNode {
            id: 1,
            lat: 0,
            lon: 0,
            tags: vec![("k\t=", "v\n  refs: (9 nodes)\u{202e}\"\\")],
            meta: Some(meta),
        }],
        &[],
        &[TestRelation {
            id: 7,
            members: vec![
                TestMember {
                    id: MemberId::Way(5),
                    role: "in) \nner",
                },
                TestMember {
                    id: MemberId::Node(1),
                    role: "",
                },
            ],
            tags: vec![("type", "multipolygon")],
            meta: Some(meta),
        }],
    );

    let show = |kind, id| {
        let mut out: Vec<u8> = Vec::new();
        let found = show_element_to(
            &input,
            kind,
            id,
            false,
            &mut out,
            &crate::common::test_host(),
        )
        .expect("show_element");
        assert!(found);
        String::from_utf8(out).expect("utf8 output")
    };

    let node = show(ElementKind::Node, 1);
    assert!(
        node.contains("\n  user: \"a\\nuser: forged\"\n"),
        "unexpected output: {node:?}"
    );
    assert!(
        node.contains("\n    \"k\\t=\"=\"v\\n  refs: (9 nodes)\\u202e\\\"\\\\\"\n"),
        "unexpected output: {node:?}"
    );
    assert!(!node.contains("\n  refs:"), "forged line: {node:?}");

    let rel = show(ElementKind::Relation, 7);
    assert!(
        rel.contains("\n    way/5 \"in) \\nner\"\n    node/1 \"\"\n"),
        "unexpected output: {rel:?}"
    );
    assert!(
        rel.contains("\n    \"type\"=\"multipolygon\"\n"),
        "unexpected output: {rel:?}"
    );
}

/// One raw frame: length prefix, a `BlobHeader` naming `blob_type` and
/// declaring the body's size, then `body` verbatim.
fn raw_frame(blob_type: &str, body: &[u8]) -> Vec<u8> {
    assert!(
        blob_type.len() < 128 && body.len() < 128,
        "single-byte varints only"
    );
    let mut header = vec![0x0a, u8::try_from(blob_type.len()).expect("len")];
    header.extend_from_slice(blob_type.as_bytes());
    header.extend_from_slice(&[0x18, u8::try_from(body.len()).expect("len")]);
    let mut frame = u32::try_from(header.len())
        .expect("len")
        .to_be_bytes()
        .to_vec();
    frame.extend_from_slice(&header);
    frame.extend_from_slice(body);
    frame
}

fn full_decode_opts(extended: bool) -> pbfhogg::inspect::InspectOptions {
    pbfhogg::inspect::InspectOptions {
        show_blocks: true,
        show_id_ranges: true,
        show_locations: false,
        extended,
        distrust_metadata: true,
        direct_io: false,
    }
}

/// The length of a PBF's first frame, read from its `BlobHeader`'s
/// datasize field.
fn first_frame_len(bytes: &[u8]) -> usize {
    let header_len = u32::from_be_bytes(bytes[..4].try_into().expect("prefix")) as usize;
    let header = &bytes[4..4 + header_len];
    let mut pos = 0;
    let mut datasize = None;
    while pos < header.len() {
        let tag = header[pos];
        pos += 1;
        let mut value = 0usize;
        let mut shift = 0;
        loop {
            let b = header[pos];
            pos += 1;
            value |= usize::from(b & 0x7f) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        match tag & 7 {
            0 if tag >> 3 == 3 => datasize = Some(value),
            0 => {}
            2 => pos += value,
            other => panic!("unexpected wire type {other}"),
        }
    }
    4 + header_len + datasize.expect("datasize")
}

/// The full-decode scan describes the file as it is: an unknown frame is
/// skipped opaque, so a body that is not a `Blob` message does not fail it.
#[test]
fn full_decode_skips_an_unknown_frame_without_parsing_its_body() {
    let dir = crate::common::test_scratch();
    let clean = dir.path().join("clean.osm.pbf");
    write_simple_pbf(&clean);
    let bytes = std::fs::read(&clean).expect("read");
    let split = first_frame_len(&bytes);
    let mut spliced = bytes[..split].to_vec();
    spliced.extend(raw_frame("x-vendor", &[0xff, 0xff, 0xff, 0xff]));
    spliced.extend_from_slice(&bytes[split..]);
    let input = dir.path().join("spliced.osm.pbf");
    std::fs::write(&input, spliced).expect("write");

    for extended in [false, true] {
        let json = pbfhogg::inspect::inspect(
            &input,
            &full_decode_opts(extended),
            &crate::common::test_host(),
        )
        .expect("inspect")
        .to_json(None);
        assert_eq!(json["elements"]["nodes"], 3, "extended {extended}");
        assert_eq!(json["elements"]["ways"], 2);
        assert_eq!(json["elements"]["relations"], 1);
    }
}

/// A file opening with a data frame is header-less, whatever that frame's
/// body holds: the type is judged before the body is parsed or decoded.
#[test]
fn full_decode_refuses_a_corrupt_leading_data_frame_as_header_less() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("headless.osm.pbf");
    std::fs::write(&input, raw_frame("OSMData", &[0xff, 0xff, 0xff])).expect("write");
    for extended in [false, true] {
        let Err(err) = pbfhogg::inspect::inspect(
            &input,
            &full_decode_opts(extended),
            &crate::common::test_host(),
        ) else {
            panic!("extended {extended}: a header-less file was described");
        };
        assert!(
            matches!(err.kind(), pbfhogg::ErrorKind::MissingHeader),
            "extended {extended}: {err}"
        );
    }
}

/// Node blocks with the given IDs, one block per slice.
fn write_node_id_blocks(path: &std::path::Path, blocks: &[&[i64]]) {
    let header = block_builder::HeaderBuilder::new()
        .build()
        .expect("build header");
    let mut writer = PbfWriter::to_path(
        path,
        Compression::default(),
        &header,
        pbfhogg::host::Executor::Ambient,
    )
    .expect("create writer");
    let mut bb = BlockBuilder::new();
    for ids in blocks {
        for &id in *ids {
            bb.add_node(id, 1_000, 2_000, std::iter::empty::<(&str, &str)>(), None);
        }
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(bytes).expect("write block");
        }
    }
    writer.flush().expect("flush");
}

/// Blobs summarized apart and folded in order give the serial answer: ID
/// order is judged across blob boundaries (a repeated ID at a boundary
/// breaks it), and blocks are numbered in file order.
#[test]
fn full_decode_judges_id_order_across_blob_boundaries() {
    let dir = crate::common::test_scratch();
    let cases: [(&[&[i64]], bool); 4] = [
        (&[&[1, 2], &[3, 4], &[5]], true),
        (&[&[1, 2], &[2, 3]], false),
        (&[&[1, 5], &[3, 9]], false),
        (&[&[2, 1], &[3]], false),
    ];
    for (i, (blocks, ordered)) in cases.iter().enumerate() {
        let input = dir.path().join(format!("case{i}.osm.pbf"));
        write_node_id_blocks(&input, blocks);
        let json =
            pbfhogg::inspect::inspect(&input, &full_decode_opts(true), &crate::common::test_host())
                .expect("inspect")
                .to_json(Some(0));
        let count: usize = blocks.iter().map(|b| b.len()).sum();
        assert_eq!(json["elements"]["nodes"], count, "case {i}");
        assert_eq!(
            json["data"]["objects_ordered"], *ordered,
            "case {i}: {json}"
        );
        let detail = json["blocks_detail"].as_array().expect("detail");
        assert_eq!(detail.len(), blocks.len(), "case {i}");
        for (n, block) in detail.iter().enumerate() {
            assert_eq!(block["number"], n + 1, "case {i}");
            assert_eq!(block["elements"], blocks[n].len(), "case {i}");
        }
    }
}
