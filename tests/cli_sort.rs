//! CLI-driven integration tests for `pbfhogg sort`.
//!
//! Pattern-setter for the CLI-decoupled test reorg (see
//! `notes/testing.md` > "Reorg"). Fixture PBFs are written with the
//! stable-allowlist writer helpers; the sort command runs via the
//! compiled `pbfhogg` binary through `CliInvoker`; output is
//! verified by reading the resulting PBF with the stable-allowlist
//! reader helpers. No imports from `pbfhogg::commands::sort` or any
//! other internal module - a rewrite of `src/commands/sort/` cannot
//! break these tests by type changes alone.

mod common;

use std::path::Path;

use common::cli::CliInvoker;
use common::{
    read_all_elements_with_coords, read_header, write_test_pbf, PbfContentsWithCoords, TestNode,
    TestRelation, TestWay,
};
use pbfhogg::block_builder::{self, BlockBuilder, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};

/// Invoke `pbfhogg sort --force -o <output> <input>`.
fn run_sort(input: &Path, output: &Path) {
    CliInvoker::new()
        .arg("sort")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--force")
        .assert_success();
}

// ---------------------------------------------------------------------------
// Fixture writers - all use stable-allowlist types (BlockBuilder,
// PbfWriter, Compression). No internal-module imports.
// ---------------------------------------------------------------------------

/// Write a PBF with deliberately overlapping node blobs.
///
/// Two node blobs with interleaving IDs (blob 1: odd, blob 2: even),
/// followed by ways and relations. Forces the sort command to decode
/// and re-encode the node blobs rather than passing them through.
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

    bb.add_way(100, [("highway", "residential")], &[1, 2, 3], None);
    bb.add_way(200, [("highway", "primary")], &[4, 5, 6], None);
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

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

/// Write a PBF with mixed element types out of order: ways, then
/// nodes, then relations. Each type is internally sorted but the
/// type order is wrong.
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

    for id in 1..=6 {
        bb.add_node(id, id as i32 * 1_000_000, id as i32 * 2_000_000, std::iter::empty::<(&str, &str)>(), None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

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
    for w in contents.nodes.windows(2) {
        assert!(w[0].0 < w[1].0, "nodes not sorted: {} >= {}", w[0].0, w[1].0);
    }
    for w in contents.ways.windows(2) {
        assert!(w[0].0 < w[1].0, "ways not sorted: {} >= {}", w[0].0, w[1].0);
    }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Sort a PBF with overlapping node blobs (forces rewrite path).
/// Output must be correctly sorted with all elements preserved.
#[test]
fn sort_overlapping_blobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("overlapping.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");

    write_unsorted_overlapping_pbf(&input);
    run_sort(&input, &output);

    let result = read_all_elements_with_coords(&output);

    assert_eq!(result.nodes.len(), 10);
    assert_eq!(result.ways.len(), 2);
    assert_eq!(result.relations.len(), 1);
    assert_sorted(&result);
    assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");

    let node_ids: Vec<i64> = result.nodes.iter().map(|(id, _, _, _)| *id).collect();
    assert_eq!(node_ids, (1..=10).collect::<Vec<_>>());

    #[allow(clippy::cast_possible_truncation)]
    for (id, lat, lon, _) in &result.nodes {
        assert_eq!(*lat, *id as i32 * 1_000_000);
        assert_eq!(*lon, *id as i32 * 2_000_000);
    }
}

/// Sort a PBF with types in wrong order (ways before nodes).
/// Output must have correct type order: nodes, ways, relations.
#[test]
fn sort_wrong_type_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("type_unsorted.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");

    write_type_unsorted_pbf(&input);
    run_sort(&input, &output);

    let result = read_all_elements_with_coords(&output);

    assert_eq!(result.nodes.len(), 6);
    assert_eq!(result.ways.len(), 2);
    assert_eq!(result.relations.len(), 1);
    assert_sorted(&result);
    assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");

    let way_tags: Vec<&str> = result
        .ways
        .iter()
        .map(|(_, _, tags)| tags[0].1.as_str())
        .collect();
    assert_eq!(way_tags, vec!["residential", "primary"]);
}

/// Overlap-rewrite must normalize the `changeset=-1` sentinel (used by
/// osmosis-produced history extracts) to 0 on the re-encoded output.
/// Regression: overlapping blobs that force rewrite previously
/// propagated the -1 verbatim.
#[allow(clippy::cast_possible_truncation)]
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

    run_sort(&input, &output);

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
/// Output must be element-equivalent to input.
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

    run_sort(&input, &output);

    let before = read_all_elements_with_coords(&input);
    let after = read_all_elements_with_coords(&output);

    assert_eq!(before.nodes.len(), after.nodes.len());
    assert_eq!(before.ways.len(), after.ways.len());
    assert_eq!(before.relations.len(), after.relations.len());
    assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");

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

/// Cross-validate pbfhogg sort against osmium sort on the same
/// handcrafted fixture. Skipped if osmium is not installed.
/// Complements `brokkr verify sort` which uses real datasets -
/// this pins per-element equivalence on overlapping-blob input.
#[test]
fn sort_cross_validate_osmium() {
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
    run_sort(&input, &pbfhogg_out);

    let status = std::process::Command::new("osmium")
        .args(["sort", input.to_str().expect("path"), "-o"])
        .arg(&osmium_out)
        .arg("--overwrite")
        .status()
        .expect("run osmium");
    assert!(status.success(), "osmium sort failed");

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
/// Each blob has IDs `i, i+10, i+20, ..., i+90` for `i in 1..=10`.
/// Forces a 10-blob overlap run through the streaming sweep merge.
#[allow(clippy::cast_possible_truncation)]
#[test]
fn sort_many_overlapping_blobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("many_overlap.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");

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

    run_sort(&input, &output);

    let result = read_all_elements_with_coords(&output);

    assert_eq!(result.nodes.len(), 100);
    assert_sorted(&result);

    let node_ids: Vec<i64> = result.nodes.iter().map(|(id, _, _, _)| *id).collect();
    assert_eq!(node_ids, (1..=100).collect::<Vec<_>>());

    for (id, lat, lon, _) in &result.nodes {
        assert_eq!(*lat, *id as i32 * 100_000);
        assert_eq!(*lon, *id as i32 * 200_000);
    }

    assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");
}

/// Overlap-runs must not cross element-kind boundaries.
///
/// Fixture layout (four blobs, two adjacent same-kind overlap pairs
/// separated by a kind boundary):
///   blob 0: Nodes ids 1,3,5,7,9     (odd)
///   blob 1: Nodes ids 2,4,6,8,10    (even, overlaps blob 0)
///   blob 2: Ways  ids 101,103,105   (odd)
///   blob 3: Ways  ids 102,104,106   (even, overlaps blob 2)
///
/// `detect_overlaps` is kind-gated, so only (0,1) and (2,3) overlap.
/// A past bug in the pass-2 walker consumed consecutive
/// `overlaps[i]=true` entries without checking kind, then handed a
/// cross-kind slice to the kind-gated sweep - ways got silently
/// dropped because the extract closure's `_ => {}` arm ate every way
/// element when the run was classified as Node. Twin of the
/// `cat::dedupe::merge_pbf` bug fixed in commit `486d4d1`.
#[test]
fn sort_overlap_runs_scoped_to_single_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("kind_boundary.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");

    let file = std::fs::File::create(&input).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new().build().expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    for id in (1..=9).step_by(2) {
        bb.add_node(id, 0, 0, std::iter::empty::<(&str, &str)>(), None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    for id in (2..=10).step_by(2) {
        bb.add_node(id, 0, 0, std::iter::empty::<(&str, &str)>(), None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    for id in (101..=105).step_by(2) {
        bb.add_way(id, std::iter::empty::<(&str, &str)>(), &[1, 2], None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    for id in (102..=106).step_by(2) {
        bb.add_way(id, std::iter::empty::<(&str, &str)>(), &[1, 2], None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");

    run_sort(&input, &output);

    let result = read_all_elements_with_coords(&output);
    assert_eq!(result.nodes.len(), 10, "expected 10 nodes, got {}", result.nodes.len());
    assert_eq!(result.ways.len(), 6, "expected 6 ways, got {}", result.ways.len());
    assert_sorted(&result);

    let node_ids: Vec<i64> = result.nodes.iter().map(|(id, _, _, _)| *id).collect();
    assert_eq!(node_ids, (1..=10).collect::<Vec<_>>());
    let way_ids: Vec<i64> = result.ways.iter().map(|w| w.0).collect();
    assert_eq!(way_ids, (101..=106).collect::<Vec<_>>());
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

    run_sort(&input, &output);

    let header = read_header(&output);
    assert!(
        header.has_historical_information(),
        "output header must declare HistoricalInformation",
    );
}

// ---------------------------------------------------------------------------
// O_DIRECT variant
// ---------------------------------------------------------------------------

/// `--direct-io` on a filesystem that supports O_DIRECT must produce
/// the same sorted output as the default path. Skipped (via stderr
/// inspection) on filesystems that reject O_DIRECT with EINVAL.
#[cfg(feature = "linux-direct-io")]
#[test]
fn sort_overlapping_blobs_direct_io() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("overlapping.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");

    write_unsorted_overlapping_pbf(&input);

    let run = CliInvoker::new()
        .arg("sort")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--direct-io")
        .arg("--force")
        .run();

    if run.is_o_direct_unsupported() {
        eprintln!("O_DIRECT not supported on this filesystem, skipping test");
        return;
    }
    assert!(
        run.status.success(),
        "sort --direct-io failed unexpectedly; stderr:\n{}",
        run.stderr_str(),
    );

    let contents = read_all_elements_with_coords(&output);
    assert_eq!(contents.nodes.len(), 10);
    assert_eq!(contents.ways.len(), 2);
    assert_eq!(contents.relations.len(), 1);
    assert_sorted(&contents);
    assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");

    let node_ids: Vec<i64> = contents.nodes.iter().map(|(id, _, _, _)| *id).collect();
    assert_eq!(node_ids, (1..=10).collect::<Vec<_>>());

    #[allow(clippy::cast_possible_truncation)]
    for (id, lat, lon, _) in &contents.nodes {
        assert_eq!(*lat, *id as i32 * 1_000_000);
        assert_eq!(*lon, *id as i32 * 2_000_000);
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

    let run = CliInvoker::new()
        .arg("sort")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--io-uring")
        .arg("--force")
        .run();

    if run.is_uring_unsupported() {
        eprintln!("io_uring not available, skipping test");
        return;
    }
    assert!(
        run.status.success(),
        "sort --io-uring failed unexpectedly; stderr:\n{}",
        run.stderr_str(),
    );

    let contents = read_all_elements_with_coords(&output);
    assert_eq!(contents.nodes.len(), 10);
    assert_eq!(contents.ways.len(), 2);
    assert_eq!(contents.relations.len(), 1);
    assert_sorted(&contents);
    assert!(read_header(&output).is_sorted(), "output missing Sort.Type_then_ID");

    let node_ids: Vec<i64> = contents.nodes.iter().map(|(id, _, _, _)| *id).collect();
    assert_eq!(node_ids, (1..=10).collect::<Vec<_>>());

    #[allow(clippy::cast_possible_truncation)]
    for (id, lat, lon, _) in &contents.nodes {
        assert_eq!(*lat, *id as i32 * 1_000_000);
        assert_eq!(*lon, *id as i32 * 2_000_000);
    }
}

// ---------------------------------------------------------------------------
// Feature-missing error paths
// ---------------------------------------------------------------------------
//
// The feature-missing tests that lived here (verifying `--direct-io`
// and `--io-uring` emit a clear error when the corresponding Cargo
// feature is absent) cannot be expressed as CLI-driven tests in this
// harness: the `cargo test -p pbfhogg --no-default-features` sweep
// rebuilds the library without the feature, but cannot rebuild the
// pbfhogg-cli binary, so the invocation still targets an all-features
// binary. This is a library-level invariant that belongs in inline
// unit tests inside `src/commands/sort/` if it matters - not an
// integration-test shape.
