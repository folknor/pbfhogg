mod common;

use std::path::Path;

use common::{
    read_all_elements_with_coords, read_header, write_test_pbf, PbfContentsWithCoords, TestNode,
    TestRelation, TestWay,
};
use pbfhogg::block_builder::{self, BlockBuilder, Metadata};
use pbfhogg::sort::SortOptions;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};

/// Write a PBF with deliberately overlapping node blobs.
///
/// Creates two node blobs with interleaving IDs (blob 1: odd, blob 2: even),
/// followed by ways and relations. This forces the sort command to decode and
/// re-encode the node blobs rather than passing them through.
#[allow(clippy::cast_possible_truncation)]
fn write_unsorted_overlapping_pbf(path: &Path) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new().build().expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    // Blob 1: odd node IDs
    for id in (1..=9).step_by(2) {
        bb.add_node(id, id as i32 * 1_000_000, id as i32 * 2_000_000, std::iter::empty::<(&str, &str)>(), None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Blob 2: even node IDs (overlapping range with blob 1)
    for id in (2..=10).step_by(2) {
        bb.add_node(id, id as i32 * 1_000_000, id as i32 * 2_000_000, std::iter::empty::<(&str, &str)>(), None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Ways (already sorted)
    bb.add_way(100, [("highway", "residential")], &[1, 2, 3], None);
    bb.add_way(200, [("highway", "primary")], &[4, 5, 6], None);
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Relation
    bb.add_relation(
        300,
        [("type", "route")],
        &[pbfhogg::block_builder::MemberData { id: pbfhogg::MemberId::Way(100), role: "outer" }],
        None,
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

/// Write a PBF with mixed element types out of order: ways, then nodes, then
/// relations. Each type is internally sorted but the type order is wrong.
#[allow(clippy::cast_possible_truncation)]
fn write_type_unsorted_pbf(path: &Path) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new().build().expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    // Ways first (wrong order - should come after nodes)
    bb.add_way(100, [("highway", "residential")], &[1, 2, 3], None);
    bb.add_way(200, [("highway", "primary")], &[4, 5, 6], None);
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Then nodes
    for id in 1..=6 {
        bb.add_node(id, id as i32 * 1_000_000, id as i32 * 2_000_000, std::iter::empty::<(&str, &str)>(), None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Then relations
    bb.add_relation(
        300,
        [("type", "route")],
        &[pbfhogg::block_builder::MemberData { id: pbfhogg::MemberId::Way(100), role: "outer" }],
        None,
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

fn assert_sorted(contents: &PbfContentsWithCoords) {
    // All node IDs strictly ascending
    for w in contents.nodes.windows(2) {
        assert!(w[0].0 < w[1].0, "nodes not sorted: {} >= {}", w[0].0, w[1].0);
    }
    // All way IDs strictly ascending
    for w in contents.ways.windows(2) {
        assert!(w[0].0 < w[1].0, "ways not sorted: {} >= {}", w[0].0, w[1].0);
    }
    // All relation IDs strictly ascending
    for w in contents.relations.windows(2) {
        assert!(w[0].0 < w[1].0, "relations not sorted: {} >= {}", w[0].0, w[1].0);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawNodeMeta {
    id: i64,
    version: Option<i32>,
    changeset: Option<i64>,
    uid: Option<i32>,
    user: Option<String>,
    visible: Option<bool>,
}

fn read_raw_node_meta(path: &Path) -> Vec<RawNodeMeta> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut metas = Vec::new();

    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => {
                        let info = dn.info();
                        metas.push(RawNodeMeta {
                            id: dn.id(),
                            version: info.as_ref().map(|i| i.version()),
                            changeset: info.as_ref().map(|i| i.changeset()),
                            uid: info.as_ref().map(|i| i.uid()),
                            user: info
                                .as_ref()
                                .map(|i| i.user().unwrap_or("").to_string()),
                            visible: info.as_ref().map(|i| i.visible()),
                        });
                    }
                    Element::Node(n) => {
                        let info = n.info();
                        metas.push(RawNodeMeta {
                            id: n.id(),
                            version: info.version(),
                            changeset: info.changeset(),
                            uid: info.uid(),
                            user: info
                                .user()
                                .and_then(std::result::Result::ok)
                                .map(ToString::to_string),
                            visible: Some(info.visible()),
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    metas
}

/// Sort a PBF with overlapping node blobs (forces rewrite path).
/// Verify output is correctly sorted and all elements are preserved.
#[test]
fn sort_overlapping_blobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("overlapping.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");

    write_unsorted_overlapping_pbf(&input);
    pbfhogg::commands::sort::sort(&input, &output, &SortOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true }, &pbfhogg::HeaderOverrides::default()).expect("sort");

    let result = read_all_elements_with_coords(&output);

    // All 10 nodes preserved
    assert_eq!(result.nodes.len(), 10);
    // 2 ways, 1 relation preserved
    assert_eq!(result.ways.len(), 2);
    assert_eq!(result.relations.len(), 1);

    // Correctly sorted
    assert_sorted(&result);

    // Header declares Sort.Type_then_ID
    assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");

    // Node IDs are 1..=10
    let node_ids: Vec<i64> = result.nodes.iter().map(|(id, _, _, _)| *id).collect();
    assert_eq!(node_ids, (1..=10).collect::<Vec<_>>());

    // Node coordinates preserved
    #[allow(clippy::cast_possible_truncation)]
    for (id, lat, lon, _) in &result.nodes {
        assert_eq!(*lat, *id as i32 * 1_000_000);
        assert_eq!(*lon, *id as i32 * 2_000_000);
    }
}

/// Sort a PBF with types in wrong order (ways before nodes).
/// Verify output has correct type order: nodes, ways, relations.
#[test]
fn sort_wrong_type_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("type_unsorted.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");

    write_type_unsorted_pbf(&input);
    pbfhogg::commands::sort::sort(&input, &output, &SortOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true }, &pbfhogg::HeaderOverrides::default()).expect("sort");

    let result = read_all_elements_with_coords(&output);

    assert_eq!(result.nodes.len(), 6);
    assert_eq!(result.ways.len(), 2);
    assert_eq!(result.relations.len(), 1);
    assert_sorted(&result);

    // Header declares Sort.Type_then_ID
    assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");

    // Tags preserved on ways
    let way_tags: Vec<&str> = result
        .ways
        .iter()
        .map(|(_, _, tags)| tags[0].1.as_str())
        .collect();
    assert_eq!(way_tags, vec!["residential", "primary"]);
}

#[test]
fn sort_overlap_rewrite_normalizes_dense_node_changeset_minus_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let file = std::fs::File::create(&input).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new().build().expect("build header");
    writer.write_header(&header).expect("write header");

    let meta = Metadata {
        version: 5,
        timestamp: 1_700_000_000,
        changeset: -1,
        uid: 9,
        user: "osmosis",
        visible: false,
    };

    let mut bb = BlockBuilder::new();
    for id in [1_i64, 3] {
        bb.add_node(
            id,
            id as i32 * 1_000_000,
            id as i32 * 2_000_000,
            [("name", "sentinel")],
            Some(&meta),
        );
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    bb.add_node(
        2,
        2_000_000,
        4_000_000,
        [("name", "sentinel")],
        Some(&meta),
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush");

    pbfhogg::commands::sort::sort(
        &input,
        &output,
        &SortOptions {
            compression: Compression::default(),
            direct_io: false,
            io_uring: false,
            force: true,
        },
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("sort");

    let nodes = read_raw_node_meta(&output);
    assert_eq!(nodes.len(), 3);
    assert_eq!(
        nodes,
        vec![
            RawNodeMeta {
                id: 1,
                version: Some(5),
                changeset: Some(0),
                uid: Some(9),
                user: Some("osmosis".to_string()),
                visible: Some(false),
            },
            RawNodeMeta {
                id: 2,
                version: Some(5),
                changeset: Some(0),
                uid: Some(9),
                user: Some("osmosis".to_string()),
                visible: Some(false),
            },
            RawNodeMeta {
                id: 3,
                version: Some(5),
                changeset: Some(0),
                uid: Some(9),
                user: Some("osmosis".to_string()),
                visible: Some(false),
            },
        ]
    );
}

/// Sort an already-sorted PBF (passthrough path).
/// Verify output is identical to input.
#[test]
fn sort_already_sorted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("sorted_input.osm.pbf");
    let output = dir.path().join("sorted_output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "path")], meta: None }],
        &[TestRelation {
            id: 20,
            members: vec![common::TestMember {
                id: pbfhogg::MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    pbfhogg::commands::sort::sort(&input, &output, &SortOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true }, &pbfhogg::HeaderOverrides::default()).expect("sort");

    let before = read_all_elements_with_coords(&input);
    let after = read_all_elements_with_coords(&output);

    assert_eq!(before.nodes.len(), after.nodes.len());
    assert_eq!(before.ways.len(), after.ways.len());
    assert_eq!(before.relations.len(), after.relations.len());

    // Header declares Sort.Type_then_ID
    assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");

    // Element data preserved exactly
    for (a, b) in before.nodes.iter().zip(after.nodes.iter()) {
        assert_eq!(a, b);
    }
    for (a, b) in before.ways.iter().zip(after.ways.iter()) {
        assert_eq!(a, b);
    }
    for (a, b) in before.relations.iter().zip(after.relations.iter()) {
        assert_eq!(a, b);
    }
}

/// Cross-validate against osmium sort (skipped if osmium not available).
#[test]
fn sort_cross_validate_osmium() {
    // Skip if osmium is not installed
    let osmium_check = std::process::Command::new("osmium").arg("--version").output();
    if osmium_check.is_err() {
        eprintln!("osmium not found, skipping cross-validation");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("overlapping.osm.pbf");
    let pbfhogg_out = dir.path().join("pbfhogg_sorted.osm.pbf");
    let osmium_out = dir.path().join("osmium_sorted.osm.pbf");

    write_unsorted_overlapping_pbf(&input);

    // Sort with pbfhogg
    pbfhogg::commands::sort::sort(&input, &pbfhogg_out, &SortOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true }, &pbfhogg::HeaderOverrides::default())
        .expect("pbfhogg sort");

    // Sort with osmium
    let status = std::process::Command::new("osmium")
        .args(["sort", input.to_str().expect("path"), "-o"])
        .arg(&osmium_out)
        .arg("--overwrite")
        .status()
        .expect("run osmium");
    assert!(status.success(), "osmium sort failed");

    // Compare element-by-element
    let pbfhogg_result = read_all_elements_with_coords(&pbfhogg_out);
    let osmium_result = read_all_elements_with_coords(&osmium_out);

    assert_eq!(pbfhogg_result.nodes.len(), osmium_result.nodes.len(), "node count mismatch");
    assert_eq!(pbfhogg_result.ways.len(), osmium_result.ways.len(), "way count mismatch");
    assert_eq!(
        pbfhogg_result.relations.len(),
        osmium_result.relations.len(),
        "relation count mismatch"
    );

    for (p, o) in pbfhogg_result.nodes.iter().zip(osmium_result.nodes.iter()) {
        assert_eq!(p.0, o.0, "node ID mismatch");
        assert_eq!(p.1, o.1, "node lat mismatch for id {}", p.0);
        assert_eq!(p.2, o.2, "node lon mismatch for id {}", p.0);
    }

    for (p, o) in pbfhogg_result.ways.iter().zip(osmium_result.ways.iter()) {
        assert_eq!(p.0, o.0, "way ID mismatch");
        assert_eq!(p.1, o.1, "way refs mismatch for id {}", p.0);
        assert_eq!(p.2, o.2, "way tags mismatch for id {}", p.0);
    }

    for (p, o) in pbfhogg_result.relations.iter().zip(osmium_result.relations.iter()) {
        assert_eq!(p.0, o.0, "relation ID mismatch");
        assert_eq!(p.1, o.1, "relation members mismatch for id {}", p.0);
        assert_eq!(p.2, o.2, "relation tags mismatch for id {}", p.0);
    }
}

/// Sort a PBF with 10 interleaving node blobs (deep overlap run).
/// Each blob has IDs i, i+10, i+20, ..., i+90 for i in 1..=10.
/// Forces a 10-blob overlap run through the streaming sweep merge.
#[allow(clippy::cast_possible_truncation)]
#[test]
fn sort_many_overlapping_blobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("many_overlap.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");

    // Write 10 blobs with interleaving node IDs
    let file = std::fs::File::create(&input).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new().build().expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();
    for blob_idx in 1..=10_i64 {
        for step in 0..10_i64 {
            let id = blob_idx + step * 10;
            bb.add_node(id, id as i32 * 100_000, id as i32 * 200_000, std::iter::empty::<(&str, &str)>(), None);
        }
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(bytes).expect("write block");
        }
    }
    writer.flush().expect("flush");

    pbfhogg::commands::sort::sort(
        &input, &output, &SortOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true }, &pbfhogg::HeaderOverrides::default(),
    )
    .expect("sort");

    let result = read_all_elements_with_coords(&output);

    // All 100 nodes preserved
    assert_eq!(result.nodes.len(), 100);
    assert_sorted(&result);

    // Node IDs are 1..=100
    let node_ids: Vec<i64> = result.nodes.iter().map(|(id, _, _, _)| *id).collect();
    assert_eq!(node_ids, (1..=100).collect::<Vec<_>>());

    // Coordinates preserved
    for (id, lat, lon, _) in &result.nodes {
        assert_eq!(*lat, *id as i32 * 100_000);
        assert_eq!(*lon, *id as i32 * 200_000);
    }

    assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");
}

#[test]
fn sort_preserves_historical_information_feature() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("history-input.osm.pbf");
    let output = dir.path().join("history-output.osm.pbf");

    let file = std::fs::File::create(&input).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .historical()
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();
    bb.add_node(
        2,
        20_000_000,
        20_000_000,
        std::iter::empty::<(&str, &str)>(),
        Some(&Metadata {
            version: 2,
            timestamp: 1_700_000_000,
            changeset: 10,
            uid: 1,
            user: "u",
            visible: false,
        }),
    );
    bb.add_node(
        1,
        10_000_000,
        10_000_000,
        std::iter::empty::<(&str, &str)>(),
        Some(&Metadata {
            version: 1,
            timestamp: 1_700_000_001,
            changeset: 11,
            uid: 1,
            user: "u",
            visible: true,
        }),
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush");

    pbfhogg::commands::sort::sort(
        &input,
        &output,
        &SortOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true },
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("sort");

    let header = read_header(&output);
    assert!(
        header.has_historical_information(),
        "output header must declare HistoricalInformation",
    );
}

// ---------------------------------------------------------------------------
// O_DIRECT variant
// ---------------------------------------------------------------------------

#[cfg(feature = "linux-direct-io")]
#[test]
fn sort_overlapping_blobs_direct_io() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("overlapping.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");

    write_unsorted_overlapping_pbf(&input);
    let result = pbfhogg::commands::sort::sort(
        &input,
        &output,
        &SortOptions {
            compression: Compression::default(),
            direct_io: true,
            io_uring: false,
            force: true,
        },
        &pbfhogg::HeaderOverrides::default(),
    );

    match result {
        Ok(_) => {
            let contents = read_all_elements_with_coords(&output);

            // All 10 nodes preserved
            assert_eq!(contents.nodes.len(), 10);
            // 2 ways, 1 relation preserved
            assert_eq!(contents.ways.len(), 2);
            assert_eq!(contents.relations.len(), 1);

            // Correctly sorted
            assert_sorted(&contents);

            // Header declares Sort.Type_then_ID
            assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");

            // Node IDs are 1..=10
            let node_ids: Vec<i64> = contents.nodes.iter().map(|(id, _, _, _)| *id).collect();
            assert_eq!(node_ids, (1..=10).collect::<Vec<_>>());

            // Node coordinates preserved
            #[allow(clippy::cast_possible_truncation)]
            for (id, lat, lon, _) in &contents.nodes {
                assert_eq!(*lat, *id as i32 * 1_000_000);
                assert_eq!(*lon, *id as i32 * 2_000_000);
            }
        }
        Err(e) if common::is_einval(&*e) => {
            eprintln!("O_DIRECT not supported on this filesystem, skipping test");
        }
        Err(e) => panic!("unexpected error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// io_uring variant
// ---------------------------------------------------------------------------

#[cfg(feature = "linux-io-uring")]
#[test]
#[ignore = "pre-existing io_uring writer bug for small outputs; see TODO.md"]
fn sort_overlapping_blobs_uring() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("overlapping.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");

    write_unsorted_overlapping_pbf(&input);
    let result = pbfhogg::commands::sort::sort(
        &input,
        &output,
        &SortOptions {
            compression: Compression::default(),
            direct_io: false,
            io_uring: true,
            force: true,
        },
        &pbfhogg::HeaderOverrides::default(),
    );

    match result {
        Ok(_) => {
            let contents = read_all_elements_with_coords(&output);

            // All 10 nodes preserved
            assert_eq!(contents.nodes.len(), 10);
            // 2 ways, 1 relation preserved
            assert_eq!(contents.ways.len(), 2);
            assert_eq!(contents.relations.len(), 1);

            // Correctly sorted
            assert_sorted(&contents);

            // Header declares Sort.Type_then_ID
            assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");

            // Node IDs are 1..=10
            let node_ids: Vec<i64> = contents.nodes.iter().map(|(id, _, _, _)| *id).collect();
            assert_eq!(node_ids, (1..=10).collect::<Vec<_>>());

            // Node coordinates preserved
            #[allow(clippy::cast_possible_truncation)]
            for (id, lat, lon, _) in &contents.nodes {
                assert_eq!(*lat, *id as i32 * 1_000_000);
                assert_eq!(*lon, *id as i32 * 2_000_000);
            }
        }
        Err(e) if common::is_uring_unavailable(&*e) => {
            eprintln!("io_uring not available, skipping test");
        }
        Err(e) => panic!("unexpected error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Feature-missing error paths
// ---------------------------------------------------------------------------

/// When built without `linux-direct-io`, passing `direct_io: true` must fail
/// with a clear error message rather than silently falling back.
#[cfg(not(feature = "linux-direct-io"))]
#[test]
fn sort_direct_io_feature_missing_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_unsorted_overlapping_pbf(&input);

    let result = pbfhogg::sort::sort(
        &input,
        &output,
        &SortOptions {
            compression: Compression::default(),
            direct_io: true,
            io_uring: false,
            force: false,
        },
        &pbfhogg::HeaderOverrides::default(),
    );

    match result {
        Ok(_) => panic!("should fail without linux-direct-io feature"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("direct-io") && msg.contains("feature"),
                "error should mention the missing feature, got: {msg}"
            );
        }
    }
}

/// When built without `linux-io-uring`, passing `io_uring: true` must fail
/// with a clear error message.
#[cfg(not(feature = "linux-io-uring"))]
#[test]
fn sort_io_uring_feature_missing_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_unsorted_overlapping_pbf(&input);

    let result = pbfhogg::sort::sort(
        &input,
        &output,
        &SortOptions {
            compression: Compression::default(),
            direct_io: false,
            io_uring: true,
            force: false,
        },
        &pbfhogg::HeaderOverrides::default(),
    );

    match result {
        Ok(_) => panic!("should fail without linux-io-uring feature"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("io-uring") && msg.contains("feature"),
                "error should mention the missing feature, got: {msg}"
            );
        }
    }
}
