//! CLI-driven integration tests for `pbfhogg cat`.
//!
//! Replaces the removed library-API `cat.rs` test. Fixture PBFs are
//! written via the stable-allowlist writers; `cat` runs through
//! `CliInvoker`; output is verified by reading the resulting
//! PBF with the stable-allowlist readers, with stats inspected
//! through stderr (`CatStats::write_summary`). No imports from
//! `pbfhogg::cat::*` - a rewrite of `src/commands/cat/` cannot
//! break these tests by type changes alone.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use crate::common::{
    NormalizedPbf, TestMember, TestMeta, TestNode, TestRelation, TestWay, generate_nodes,
    generate_ways, node_ids_with_coords as node_ids, read_all_elements_with_coords,
    read_normalized, relation_ids_with_coords as relation_ids, way_ids_with_coords as way_ids,
    write_multi_block_test_pbf, write_test_pbf,
};
use crate::invoker::CliInvoker;
use pbfhogg::MemberId;

/// Invoke `pbfhogg cat <inputs...> -o <output> [--type T] [--clean A]...
/// [--direct-io] [--force]` and assert success. Returns the captured
/// `CliOutput` so callers can inspect stderr for stats and stdout if needed.
/// `--force` is passed only with `--type` or `--clean`: plain cat never
/// refuses unindexed input, so it refuses `--force`.
///
/// `clean_attrs` is the list of `--clean <name>` flags to pass; passing
/// `&[]` is equivalent to `CleanAttrs::default()` (no fields cleaned).
fn run_cat(
    inputs: &[&Path],
    output: &Path,
    type_filter: Option<&str>,
    clean_attrs: &[&str],
    direct_io: bool,
) -> crate::invoker::CliOutput {
    let mut cli = CliInvoker::new().arg("cat");
    for input in inputs {
        cli = cli.arg(*input);
    }
    cli = cli.arg("-o").arg(output);
    if let Some(types) = type_filter {
        cli = cli.arg("--type").arg(types);
    }
    for attr in clean_attrs {
        cli = cli.arg("--clean").arg(*attr);
    }
    if direct_io {
        cli = cli.arg("--direct-io");
    }
    if type_filter.is_some() || !clean_attrs.is_empty() {
        cli = cli.arg("--force");
    }
    cli.run()
}

#[test]
fn cat_passthrough_buffered() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
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
                tags: vec![("name", "b")],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "path")],
            meta: None,
        }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let out = run_cat(&[&input], &output, None, &[], false);
    assert!(
        out.status.success(),
        "cat failed; stderr:\n{}",
        out.stderr_str()
    );
    // Passthrough path summary: "{N} blobs passed through"
    assert!(
        out.stderr_str().contains("blobs passed through"),
        "stderr missing passthrough summary; stderr:\n{}",
        out.stderr_str(),
    );

    let contents = read_all_elements_with_coords(&output);
    assert_eq!(contents.nodes.len(), 2);
    assert_eq!(contents.ways.len(), 1);
    assert_eq!(contents.relations.len(), 1);

    // Verify element data preserved
    assert_eq!(contents.nodes[0].0, 1);
    assert_eq!(contents.nodes[1].0, 2);
    assert_eq!(contents.ways[0].0, 10);
    assert_eq!(contents.relations[0].0, 20);
}

// ---------------------------------------------------------------------------
// Type filtering
// ---------------------------------------------------------------------------

#[test]
fn cat_type_filter_nodes_only() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
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
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "path")],
            meta: None,
        }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let out = run_cat(&[&input], &output, Some("node"), &[], false);
    assert!(
        out.status.success(),
        "cat --type node failed; stderr:\n{}",
        out.stderr_str()
    );

    let c = read_all_elements_with_coords(&output);
    assert_eq!(node_ids(&c), vec![1, 2]);
    assert!(way_ids(&c).is_empty(), "ways should be filtered out");
    assert!(
        relation_ids(&c).is_empty(),
        "relations should be filtered out"
    );
}

#[test]
fn cat_type_filter_ways_only() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
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
            tags: vec![("highway", "path")],
            meta: None,
        }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let out = run_cat(&[&input], &output, Some("way"), &[], false);
    assert!(
        out.status.success(),
        "cat --type way failed; stderr:\n{}",
        out.stderr_str()
    );

    let c = read_all_elements_with_coords(&output);
    assert!(node_ids(&c).is_empty(), "nodes should be filtered out");
    assert_eq!(way_ids(&c), vec![10]);
    assert!(
        relation_ids(&c).is_empty(),
        "relations should be filtered out"
    );
}

#[test]
fn cat_type_filter_node_way() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![1],
            tags: vec![("highway", "path")],
            meta: None,
        }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let out = run_cat(&[&input], &output, Some("node,way"), &[], false);
    assert!(
        out.status.success(),
        "cat --type node,way failed; stderr:\n{}",
        out.stderr_str()
    );

    let c = read_all_elements_with_coords(&output);
    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![10]);
    assert!(
        relation_ids(&c).is_empty(),
        "relations should be filtered out"
    );
}

#[test]
fn cat_rejects_unknown_type_without_creating_output() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf(&input, &[], &[], &[]);

    let result = run_cat(&[&input], &output, Some("nodes"), &[], false);
    assert!(!result.status.success());
    assert!(result.stderr_str().contains("invalid element type 'nodes'"));
    assert!(!output.exists());
}

#[test]
fn cat_rejects_input_output_alias_before_truncation() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    write_test_pbf(&input, &[], &[], &[]);
    let before = std::fs::read(&input).expect("read input before command");

    let result = run_cat(&[&input], &input, None, &[], false);
    assert!(!result.status.success());
    assert!(result.stderr_str().contains("aliases input"));
    assert_eq!(std::fs::read(&input).expect("read preserved input"), before);
}

// ---------------------------------------------------------------------------
// Multi-file concatenation
// ---------------------------------------------------------------------------

#[test]
fn cat_multi_file() {
    let dir = crate::common::test_scratch();
    let input1 = dir.path().join("a.osm.pbf");
    let input2 = dir.path().join("b.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input1,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "a")],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf(
        &input2,
        &[TestNode {
            id: 2,
            lat: 110_000_000,
            lon: 210_000_000,
            tags: vec![("name", "b")],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![2],
            tags: vec![("highway", "road")],
            meta: None,
        }],
        &[],
    );

    let out = run_cat(&[&input1, &input2], &output, None, &[], false);
    assert!(
        out.status.success(),
        "cat multi-file failed; stderr:\n{}",
        out.stderr_str()
    );

    let c = read_all_elements_with_coords(&output);
    assert_eq!(c.nodes.len(), 2);
    assert_eq!(c.ways.len(), 1);
    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(way_ids(&c), vec![10]);
}

/// Write a node-only PBF whose header carries an explicit bbox and a full set
/// of replication fields, so a `cat` of two of them has something to
/// reconcile.
fn write_pbf_with_header(
    path: &Path,
    node_id: i64,
    bbox: (f64, f64, f64, f64),
    replication: (i64, i64, &str),
) {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};

    let (left, bottom, right, top) = bbox;
    let (timestamp, sequence, url) = replication;
    let header = HeaderBuilder::new()
        .bbox(left, bottom, right, top)
        .replication_timestamp(timestamp)
        .replication_sequence_number(sequence)
        .replication_base_url(url)
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
    bb.add_node(node_id, 0, 0, [("name", "x")], None);
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush");
}

/// A multi-input `cat` output must not advertise a bounding box that fails to
/// cover its own data. All three cat paths built the output header from the
/// first input alone, so the second input's elements fell outside the declared
/// extent and the replication fields described whichever file was listed
/// first.
#[test]
fn cat_multi_file_unions_the_bbox_and_reconciles_replication() {
    let dir = crate::common::test_scratch();
    let west = dir.path().join("west.osm.pbf");
    let east = dir.path().join("east.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Disjoint boxes, and replication fields that agree on the base URL but
    // disagree on timestamp and sequence.
    write_pbf_with_header(
        &west,
        1,
        (1.0, 10.0, 2.0, 20.0),
        (1_700_000_000, 100, "https://example.invalid/replication"),
    );
    write_pbf_with_header(
        &east,
        2,
        (30.0, 40.0, 40.0, 50.0),
        (1_800_000_000, 200, "https://example.invalid/replication"),
    );

    let out = run_cat(&[&west, &east], &output, None, &[], false);
    assert!(
        out.status.success(),
        "cat multi-file failed; stderr:\n{}",
        out.stderr_str()
    );

    let header = crate::common::read_header(&output);
    let bbox = header
        .bbox()
        .expect("both inputs declare a bbox, so the output must declare their union");
    let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
    assert!(
        close(bbox.left, 1.0)
            && close(bbox.bottom, 10.0)
            && close(bbox.right, 40.0)
            && close(bbox.top, 50.0),
        "expected the union (1, 10, 40, 50), got ({}, {}, {}, {}) - the first \
         input's box alone does not cover the second input's elements",
        bbox.left,
        bbox.bottom,
        bbox.right,
        bbox.top,
    );

    assert_eq!(
        header.osmosis_replication_timestamp(),
        None,
        "the inputs disagree on the replication timestamp; dropping it is honest, \
         inheriting whichever file was listed first is not"
    );
    assert_eq!(
        header.osmosis_replication_sequence_number(),
        None,
        "same for the sequence number"
    );
    assert_eq!(
        header.osmosis_replication_base_url(),
        Some("https://example.invalid/replication"),
        "the one replication field both inputs agree on must survive"
    );
}

/// The single-input counterpart: nothing to reconcile, so the header is
/// carried through unchanged. Without this, a fix that simply blanked the
/// header for every `cat` would pass the test above.
#[test]
fn cat_single_file_preserves_the_input_header() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_pbf_with_header(
        &input,
        1,
        (1.0, 10.0, 2.0, 20.0),
        (1_700_000_000, 100, "https://example.invalid/replication"),
    );

    let out = run_cat(&[&input], &output, None, &[], false);
    assert!(out.status.success(), "cat failed:\n{}", out.stderr_str());

    let header = crate::common::read_header(&output);
    let bbox = header.bbox().expect("bbox preserved");
    let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
    assert!(close(bbox.left, 1.0) && close(bbox.right, 2.0));
    assert_eq!(
        header.osmosis_replication_timestamp(),
        Some(1_700_000_000),
        "a single input has nothing to disagree with"
    );
    assert_eq!(header.osmosis_replication_sequence_number(), Some(100));
}

/// `cat` has three output paths - plain passthrough, the `--type` blob-filter
/// passthrough, and the full-decode filtered path - and each built the output
/// header independently from `files[0]`. The reconciliation test above drives
/// only the first of them, so a fix applied to one path and not the others
/// would look complete. This drives the remaining two over the same inputs and
/// demands the same header.
#[test]
fn cat_reconciles_the_header_on_every_output_path() {
    let dir = crate::common::test_scratch();
    let west = dir.path().join("west.osm.pbf");
    let east = dir.path().join("east.osm.pbf");

    write_pbf_with_header(
        &west,
        1,
        (1.0, 10.0, 2.0, 20.0),
        (1_700_000_000, 100, "https://example.invalid/replication"),
    );
    write_pbf_with_header(
        &east,
        2,
        (30.0, 40.0, 40.0, 50.0),
        (1_800_000_000, 200, "https://example.invalid/replication"),
    );

    // (label, --type, -c attrs): the type-filter passthrough, then the
    // full-decode filtered path.
    let cases: [(&str, Option<&str>, &[&str]); 2] =
        [("type", Some("node"), &[]), ("clean", None, &["version"])];

    for (label, type_filter, clean) in cases {
        let output = dir.path().join(format!("out-{label}.osm.pbf"));
        let out = run_cat(&[&west, &east], &output, type_filter, clean, false);
        assert!(
            out.status.success(),
            "cat ({label}) failed:\n{}",
            out.stderr_str()
        );

        let header = crate::common::read_header(&output);
        let bbox = header
            .bbox()
            .unwrap_or_else(|| panic!("{label}: the union of two declared boxes is known"));
        let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
        assert!(
            close(bbox.left, 1.0)
                && close(bbox.bottom, 10.0)
                && close(bbox.right, 40.0)
                && close(bbox.top, 50.0),
            "{label}: expected the union (1, 10, 40, 50), got ({}, {}, {}, {})",
            bbox.left,
            bbox.bottom,
            bbox.right,
            bbox.top,
        );
        assert_eq!(
            header.osmosis_replication_timestamp(),
            None,
            "{label}: the inputs disagree, so the field must be dropped"
        );
        assert_eq!(
            header.osmosis_replication_base_url(),
            Some("https://example.invalid/replication"),
            "{label}: the agreed field must survive"
        );
    }
}

/// A missing bbox on any input makes the union unknown, so the output must
/// declare no bbox rather than the boxes it happens to know about.
#[test]
fn cat_multi_file_drops_the_bbox_when_an_input_declares_none() {
    let dir = crate::common::test_scratch();
    let boxed = dir.path().join("boxed.osm.pbf");
    let unboxed = dir.path().join("unboxed.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_pbf_with_header(
        &boxed,
        1,
        (1.0, 10.0, 2.0, 20.0),
        (1_700_000_000, 100, "https://example.invalid/replication"),
    );
    write_test_pbf(
        &unboxed,
        &[TestNode {
            id: 2,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let out = run_cat(&[&boxed, &unboxed], &output, None, &[], false);
    assert!(out.status.success(), "cat failed:\n{}", out.stderr_str());

    let header = crate::common::read_header(&output);
    assert!(
        header.bbox().is_none(),
        "an input of unknown extent makes the union unknown"
    );
}

// ---------------------------------------------------------------------------
// CleanAttrs field stripping
// ---------------------------------------------------------------------------
//
// `--clean <ATTR>` selectively zeros metadata fields (version,
// changeset, timestamp, uid) and empties `user` via
// `clean_metadata` in `src/commands/mod.rs`. Each test below writes a
// fixture whose elements all carry metadata with distinctive non-zero
// values, runs `cat` with a specific set of `-c` flags, reads the
// result via `read_normalized`, and asserts that exactly the requested
// fields were cleaned while every other field round-tripped intact.

/// Metadata values that are impossible to get "accidentally": every
/// field is distinct and non-zero, so a cleared field is unambiguous.
fn sentinel_meta() -> TestMeta {
    TestMeta {
        version: 7,
        timestamp: 1_700_000_000,
        changeset: 12_345,
        uid: 99,
        user: "alice",
        visible: true,
    }
}

/// Build a small sorted+indexed fixture with sentinel metadata on every
/// node, way, and relation. One of each type is enough - `clean_metadata`
/// runs per-element and has no cross-element coupling we need to stress.
fn write_clean_fixture(path: &Path) {
    let meta = Some(sentinel_meta());
    write_test_pbf(
        path,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "n1")],
            meta,
        }],
        &[TestWay {
            id: 10,
            refs: vec![1],
            tags: vec![("highway", "primary")],
            meta,
        }],
        &[TestRelation {
            id: 100,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("type", "route")],
            meta,
        }],
    );
}

/// Run cat with the supplied `-c` flag list and return the normalized
/// output PBF. Shared by every `clean_*` test.
fn cat_with_clean(clean_attrs: &[&str]) -> NormalizedPbf {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_clean_fixture(&input);

    let out = run_cat(&[&input], &output, None, clean_attrs, false);
    assert!(
        out.status.success(),
        "cat --clean failed; stderr:\n{}",
        out.stderr_str(),
    );

    read_normalized(&output)
}

#[test]
fn clean_default_preserves_all_metadata() {
    let c = cat_with_clean(&[]);

    // Nothing cleaned - every field should round-trip to the sentinel.
    let sentinel = sentinel_meta();
    for (what, meta) in [
        ("node", c.nodes[0].meta.as_ref()),
        ("way", c.ways[0].meta.as_ref()),
        ("relation", c.relations[0].meta.as_ref()),
    ] {
        let m = meta.unwrap_or_else(|| panic!("{what} should carry metadata"));
        assert_eq!(m.version, sentinel.version, "{what} version");
        assert_eq!(m.timestamp, sentinel.timestamp, "{what} timestamp");
        assert_eq!(m.changeset, sentinel.changeset, "{what} changeset");
        assert_eq!(m.uid, sentinel.uid, "{what} uid");
        assert_eq!(m.user, sentinel.user, "{what} user");
    }
}

#[test]
fn clean_version_only() {
    let c = cat_with_clean(&["version"]);
    let sentinel = sentinel_meta();

    for (what, meta) in [
        ("node", c.nodes[0].meta.as_ref()),
        ("way", c.ways[0].meta.as_ref()),
        ("relation", c.relations[0].meta.as_ref()),
    ] {
        let m = meta.unwrap_or_else(|| panic!("{what} should carry metadata"));
        assert_eq!(m.version, 0, "{what} version must be zeroed");
        assert_eq!(
            m.timestamp, sentinel.timestamp,
            "{what} timestamp must survive"
        );
        assert_eq!(
            m.changeset, sentinel.changeset,
            "{what} changeset must survive"
        );
        assert_eq!(m.uid, sentinel.uid, "{what} uid must survive");
        assert_eq!(m.user, sentinel.user, "{what} user must survive");
    }
}

#[test]
fn clean_user_only() {
    let c = cat_with_clean(&["user"]);
    let sentinel = sentinel_meta();

    for (what, meta) in [
        ("node", c.nodes[0].meta.as_ref()),
        ("way", c.ways[0].meta.as_ref()),
        ("relation", c.relations[0].meta.as_ref()),
    ] {
        let m = meta.unwrap_or_else(|| panic!("{what} should carry metadata"));
        assert_eq!(m.version, sentinel.version, "{what} version must survive");
        assert_eq!(
            m.timestamp, sentinel.timestamp,
            "{what} timestamp must survive"
        );
        assert_eq!(
            m.changeset, sentinel.changeset,
            "{what} changeset must survive"
        );
        assert_eq!(m.uid, sentinel.uid, "{what} uid must survive");
        assert_eq!(m.user, "", "{what} user must be empty");
    }
}

#[test]
fn clean_timestamp_and_changeset() {
    let c = cat_with_clean(&["timestamp", "changeset"]);
    let sentinel = sentinel_meta();

    for (what, meta) in [
        ("node", c.nodes[0].meta.as_ref()),
        ("way", c.ways[0].meta.as_ref()),
        ("relation", c.relations[0].meta.as_ref()),
    ] {
        let m = meta.unwrap_or_else(|| panic!("{what} should carry metadata"));
        assert_eq!(m.version, sentinel.version, "{what} version must survive");
        assert_eq!(m.timestamp, 0, "{what} timestamp must be zeroed");
        assert_eq!(m.changeset, 0, "{what} changeset must be zeroed");
        assert_eq!(m.uid, sentinel.uid, "{what} uid must survive");
        assert_eq!(m.user, sentinel.user, "{what} user must survive");
    }
}

#[test]
fn clean_all_fields() {
    let c = cat_with_clean(&["version", "changeset", "timestamp", "uid", "user"]);

    // A dense node with every field cleaned is version 0 with all-zero
    // fields, which on the wire is indistinguishable from a metadata-less
    // padding slot, so it reads back as no metadata (the product's dense
    // presence rule, `DenseNodeInfo::claims`).
    assert!(
        c.nodes[0].meta.is_none(),
        "an all-zero dense slot reads as no metadata"
    );

    // Every cleanable field is zero / empty on ways and relations, whose
    // Info carries presence per field.
    for (what, meta) in [
        ("way", c.ways[0].meta.as_ref()),
        ("relation", c.relations[0].meta.as_ref()),
    ] {
        let m = meta.unwrap_or_else(|| panic!("{what} should carry metadata"));
        assert_eq!(m.version, 0, "{what} version");
        assert_eq!(m.timestamp, 0, "{what} timestamp");
        assert_eq!(m.changeset, 0, "{what} changeset");
        assert_eq!(m.uid, 0, "{what} uid");
        assert_eq!(m.user, "", "{what} user");
    }
}

/// Elements that started with no metadata must remain without metadata
/// after a clean - `clean_metadata` maps `None -> None` and must not
/// synthesise a zeroed metadata block.
#[test]
fn clean_does_not_fabricate_metadata_on_meta_less_elements() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[TestNode {
            id: 1,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let out = run_cat(
        &[&input],
        &output,
        None,
        &["version", "changeset", "timestamp", "uid", "user"],
        false,
    );
    assert!(
        out.status.success(),
        "cat --clean on metadata-less input failed; stderr:\n{}",
        out.stderr_str(),
    );

    let c = read_normalized(&output);
    assert_eq!(c.nodes.len(), 1);
    assert!(
        c.nodes[0].meta.is_none(),
        "clean_metadata must not fabricate metadata on elements that had none"
    );
}

// ---------------------------------------------------------------------------
// Multi-blob raw passthrough for `cat --type way`
// ---------------------------------------------------------------------------
//
// `cat_type_passthrough` at `src/commands/cat/mod.rs` uses the per-blob
// indexdata to decide whether a blob's elements match the type filter.
// Matching blobs go through `writer.write_raw_owned` as pre-framed
// bytes (counts toward `blobs_passthrough`); non-matching blobs are
// skipped entirely. This test forces multiple blobs per type via
// `write_multi_block_test_pbf` and asserts the stderr summary names
// the expected number of passthrough blobs.

#[test]
fn cat_type_way_passthrough_across_multiple_blobs() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // 20 nodes at block_size=10 -> 2 node blobs (both skipped).
    // 20 ways  at block_size=10 -> 2 way blobs  (both pass through raw).
    // No relations.
    let nodes = generate_nodes(20, 1);
    let ways = generate_ways(20, 1_000, 2, 1);
    write_multi_block_test_pbf(&input, &nodes, &ways, &[], 10);

    let out = run_cat(&[&input], &output, Some("way"), &[], false);
    assert!(
        out.status.success(),
        "cat --type way failed; stderr:\n{}",
        out.stderr_str()
    );

    // Passthrough summary fires when blobs_decoded == 0; in
    // type-filter raw passthrough we expect exactly 2 way blobs
    // through, so the line is "2 blobs passed through".
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("2 blobs passed through"),
        "expected '2 blobs passed through' in stderr; got:\n{stderr}",
    );

    // Round-trip check: all 20 ways present, zero nodes, zero relations.
    let c = read_all_elements_with_coords(&output);
    assert_eq!(c.nodes.len(), 0, "node blobs must be skipped");
    assert_eq!(c.relations.len(), 0, "no relations in input");
    assert_eq!(
        way_ids(&c),
        (1_000..1_020).collect::<Vec<_>>(),
        "all way ids must survive raw passthrough"
    );
}

/// `--clean` on an unindexed input must refuse without `--force`.
///
/// `--clean` routes through `cat_filtered`, whose per-kind schedules cannot
/// tell which kind an unindexed blob holds and therefore replicate it into all
/// three - so every blob is read and decompressed three times. The output is
/// correct, which is why this went unnoticed: the guard used to key on
/// `--type` alone and let `--clean` past silently.
#[test]
fn clean_on_an_unindexed_input_refuses_without_force() {
    let dir = crate::common::test_scratch();
    let indexed = dir.path().join("indexed.osm.pbf");
    let unindexed = dir.path().join("unindexed.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_clean_fixture(&indexed);
    CliInvoker::new()
        .arg("degrade")
        .arg(&indexed)
        .arg("-o")
        .arg(&unindexed)
        .arg("--strip-indexdata")
        .assert_success();

    let out = CliInvoker::new()
        .arg("cat")
        .arg(&unindexed)
        .arg("-o")
        .arg(&output)
        .arg("--clean")
        .arg("version")
        .run();
    assert!(
        !out.status.success(),
        "--clean on an unindexed input costs three decode passes and must say \
         so; stderr:\n{}",
        out.stderr_str(),
    );
    out.assert_stderr_contains("three times");

    // Two paired counterparts, so this is not satisfied by "cat now refuses
    // everything": --force proceeds, and the same file with an index is fine.
    CliInvoker::new()
        .arg("cat")
        .arg(&unindexed)
        .arg("-o")
        .arg(&output)
        .arg("--clean")
        .arg("version")
        .arg("--force")
        .assert_success();
    CliInvoker::new()
        .arg("cat")
        .arg(&indexed)
        .arg("-o")
        .arg(&output)
        .arg("--overwrite")
        .arg("--clean")
        .arg("version")
        .assert_success();
}

// ---------------------------------------------------------------------------
// --dedupe: header reconciliation, input framing, observed vs claimed counts
// ---------------------------------------------------------------------------

/// Write a sorted single-node PBF whose header is `configure(new().sorted())`.
/// `--dedupe` refuses unsorted inputs, so its fixtures need the stamp.
fn write_sorted_pbf_with(
    path: &Path,
    node_id: i64,
    configure: impl FnOnce(
        pbfhogg::block_builder::HeaderBuilder<'static>,
    ) -> pbfhogg::block_builder::HeaderBuilder<'static>,
) {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};

    let header = configure(HeaderBuilder::new().sorted())
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
    bb.add_node(node_id, 0, 0, [("name", "x")], None);
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush");
}

fn run_dedupe(inputs: &[&Path], output: &Path) -> crate::invoker::CliOutput {
    let mut cli = CliInvoker::new().arg("cat");
    for input in inputs {
        cli = cli.arg(*input);
    }
    cli.arg("--dedupe")
        .arg("-o")
        .arg(output)
        .arg("--force")
        .run()
}

/// `cat --dedupe` must reconcile its output header across every input, like
/// the other three cat paths. It used to keep the first OSMHeader its index
/// pass met, so the merge advertised the first input's bbox over both
/// inputs' data, carried the first input's replication state, and dropped
/// `HistoricalInformation` declared only by a later input.
#[test]
fn cat_dedupe_reconciles_the_header_across_every_input() {
    let dir = crate::common::test_scratch();
    let west = dir.path().join("west.osm.pbf");
    let east = dir.path().join("east.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    let url = "https://example.invalid/replication";
    write_sorted_pbf_with(&west, 1, |hb| {
        hb.bbox(1.0, 10.0, 2.0, 20.0)
            .replication_timestamp(1_700_000_000)
            .replication_sequence_number(100)
            .replication_base_url(url)
            .source("shared-source")
    });
    write_sorted_pbf_with(&east, 2, |hb| {
        hb.bbox(30.0, 40.0, 40.0, 50.0)
            .replication_timestamp(1_800_000_000)
            .replication_sequence_number(200)
            .replication_base_url(url)
            .source("shared-source")
            .historical()
    });

    let out = run_dedupe(&[&west, &east], &output);
    assert!(
        out.status.success(),
        "cat --dedupe failed:\n{}",
        out.stderr_str()
    );

    let header = crate::common::read_header(&output);
    let bbox = header
        .bbox()
        .expect("both inputs declare a bbox, so the merge must declare their union");
    let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
    assert!(
        close(bbox.left, 1.0)
            && close(bbox.bottom, 10.0)
            && close(bbox.right, 40.0)
            && close(bbox.top, 50.0),
        "expected the union (1, 10, 40, 50), got ({}, {}, {}, {}) - the first \
         input's box alone does not cover the second input's node",
        bbox.left,
        bbox.bottom,
        bbox.right,
        bbox.top,
    );
    assert_eq!(
        header.osmosis_replication_timestamp(),
        None,
        "the inputs disagree on the replication timestamp, so it must be dropped"
    );
    assert_eq!(
        header.osmosis_replication_sequence_number(),
        None,
        "and on the sequence number"
    );
    assert_eq!(
        header.osmosis_replication_base_url(),
        Some(url),
        "the agreed base URL must survive"
    );
    assert_eq!(
        header.source(),
        Some("shared-source"),
        "the agreed source must survive"
    );
    assert!(
        header.has_historical_information(),
        "one input declaring HistoricalInformation makes it true of the merge"
    );
    assert!(header.is_sorted(), "a dedupe merge is always sorted");

    let c = read_all_elements_with_coords(&output);
    assert_eq!(node_ids(&c), vec![1, 2]);
}

/// Every input must start with its own OSMHeader. When dedupe kept only the
/// first header found across all inputs, a headerless input never met the
/// sortedness check and was merged under the `Sort.Type_then_ID` stamp; a
/// second header inside one input was silently ignored along with whatever it
/// declared.
#[test]
fn cat_dedupe_refuses_an_input_without_exactly_one_leading_header() {
    let dir = crate::common::test_scratch();
    let good = dir.path().join("good.osm.pbf");
    let other = dir.path().join("other.osm.pbf");
    write_sorted_pbf_with(&good, 1, |hb| hb);
    write_sorted_pbf_with(&other, 2, |hb| hb);

    let bytes = std::fs::read(&other).expect("read fixture");
    let blobs = crate::common::adversarial::locate_blobs(&bytes);
    assert!(
        blobs.len() >= 2,
        "fixture must hold a header and a data blob"
    );
    let header_frame = &bytes[blobs[0].frame_start..blobs[0].blob_end];
    let data_frames = &bytes[blobs[1].frame_start..];

    let headerless = dir.path().join("headerless.osm.pbf");
    std::fs::write(&headerless, data_frames).expect("write headerless");
    let twice = dir.path().join("twice.osm.pbf");
    std::fs::write(&twice, [header_frame, &bytes[..]].concat()).expect("write twice");

    for (label, bad, expected) in [
        (
            "headerless",
            &headerless,
            "does not start with an OsmHeader blob",
        ),
        ("two headers", &twice, "a second OSMHeader blob"),
    ] {
        let output = dir.path().join(format!("out-{label}.osm.pbf"));
        let out = run_dedupe(&[&good, bad], &output);
        assert!(
            !out.status.success(),
            "{label}: cat --dedupe must refuse the input; stderr:\n{}",
            out.stderr_str()
        );
        assert!(
            out.stderr_str().contains(expected),
            "{label}: expected {expected:?} in stderr:\n{}",
            out.stderr_str()
        );
    }
}

/// A blob copied through without decoding has only its indexdata's word for
/// how many elements it holds. That claim must not reach the observed
/// "Merged N elements" headline: `degrade --corrupt-indexdata` inflates every
/// blob's count by 1 000 000 through a structurally valid index, and the
/// headline used to report the inflated figure as fact.
#[test]
fn cat_dedupe_keeps_indexdata_claims_out_of_the_observed_count() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let lying = dir.path().join("lying.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_sorted_pbf_with(&honest, 1, |hb| hb);
    CliInvoker::new()
        .arg("degrade")
        .arg(&honest)
        .arg("-o")
        .arg(&lying)
        .arg("--corrupt-indexdata")
        .assert_success();

    let out = run_dedupe(&[&lying], &output);
    assert!(
        out.status.success(),
        "cat --dedupe failed:\n{}",
        out.stderr_str()
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("Merged 0 elements: 0 nodes, 0 ways, 0 relations"),
        "the one blob was never decoded, so nothing was observed; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("plus 1000001 elements in blobs copied through without decoding"),
        "the claim must still be reported, labelled as unverified; stderr:\n{stderr}"
    );

    let c = read_all_elements_with_coords(&output);
    assert_eq!(node_ids(&c), vec![1], "the data itself is untouched");
}

/// The observed counterpart: elements the merge decoded are counted as
/// observed, and an all-decoded run prints no claimed line.
#[test]
fn cat_dedupe_counts_decoded_overlap_runs_as_observed() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_sorted_pbf_with(&a, 1, |hb| hb);
    write_sorted_pbf_with(&b, 1, |hb| hb);

    let out = run_dedupe(&[&a, &b], &output);
    assert!(
        out.status.success(),
        "cat --dedupe failed:\n{}",
        out.stderr_str()
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("Merged 1 elements: 1 nodes") && stderr.contains("1 duplicates removed"),
        "two copies of node 1 overlap, are decoded, and merge to one; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("copied through without decoding"),
        "nothing was copied through on a claim; stderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Platform tier
// ---------------------------------------------------------------------------
//
// `--direct-io` on a filesystem that supports O_DIRECT must produce
// identical output to the default path. Wrapped in `mod platform` so
// the brokkr platform profile can target it.

#[cfg(feature = "linux-direct-io")]
mod platform {
    use super::*;

    #[test]
    fn cat_passthrough_direct_io() {
        let dir = crate::common::test_scratch();
        let input = dir.path().join("input.osm.pbf");
        let output = dir.path().join("output.osm.pbf");

        write_test_pbf(
            &input,
            &[
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
                    tags: vec![("name", "b")],
                    meta: None,
                },
            ],
            &[TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![("highway", "path")],
                meta: None,
            }],
            &[TestRelation {
                id: 20,
                members: vec![TestMember {
                    id: MemberId::Way(10),
                    role: "outer",
                }],
                tags: vec![("type", "multipolygon")],
                meta: None,
            }],
        );

        let out = run_cat(&[&input], &output, None, &[], true);
        if out.is_o_direct_unsupported() {
            crate::common::skip("O_DIRECT not supported on this filesystem");
            return;
        }
        assert!(
            out.status.success(),
            "cat --direct-io failed unexpectedly; stderr:\n{}",
            out.stderr_str(),
        );

        assert!(
            out.stderr_str().contains("blobs passed through"),
            "stderr missing passthrough summary; stderr:\n{}",
            out.stderr_str(),
        );

        let contents = read_all_elements_with_coords(&output);
        assert_eq!(contents.nodes.len(), 2);
        assert_eq!(contents.ways.len(), 1);
        assert_eq!(contents.relations.len(), 1);

        // Verify element data preserved
        assert_eq!(contents.nodes[0].0, 1);
        assert_eq!(contents.nodes[1].0, 2);
        assert_eq!(contents.ways[0].0, 10);
        assert_eq!(contents.relations[0].0, 20);
    }
}

#[test]
fn passthrough_rejects_malformed_unindexed_payload() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("malformed.pbf");
    let output = dir.path().join("output.pbf");
    let file = std::fs::File::create(&input).expect("create input");
    let mut writer = pbfhogg::writer::PbfWriter::new(file, pbfhogg::writer::Compression::None);
    let header = pbfhogg::block_builder::HeaderBuilder::new()
        .build()
        .expect("header");
    writer.write_header(&header).expect("write header");
    writer
        .write_primitive_block_no_indexdata(&[0x80])
        .expect("write adversarial payload");
    writer.flush().expect("flush fixture");
    drop(writer);

    let out = crate::invoker::CliInvoker::new()
        .arg("cat")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .run();
    assert_eq!(
        out.status.code(),
        Some(2),
        "malformed payload was not refused as a failure"
    );
    assert!(
        out.stderr_str().contains(&input.display().to_string()),
        "scanner error omitted its input path; stderr:\n{}",
        out.stderr_str(),
    );
    assert!(
        out.stderr_str().contains("at byte offset"),
        "scanner error omitted blob location; stderr:\n{}",
        out.stderr_str(),
    );
    assert!(
        !output.exists(),
        "a refused cat must not publish its output"
    );
}

#[test]
fn every_cat_path_refuses_non_leading_header_frames() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    write_sorted_pbf_with(&input, 1, |hb| hb);
    let bytes = std::fs::read(&input).expect("read fixture");
    let frames = crate::common::adversarial::locate_blobs(&bytes);
    let header = &bytes[..frames[0].blob_end];
    for insertion in [frames[0].blob_end, bytes.len()] {
        let bad = dir.path().join(format!("header-at-{insertion}.osm.pbf"));
        let doubled = [&bytes[..insertion], header, &bytes[insertion..]].concat();
        std::fs::write(&bad, doubled).expect("write repeated header");
        for (mode, flags) in [
            ("passthrough", vec![]),
            ("filtered", vec!["--type", "node"]),
            ("clean", vec!["--clean", "version"]),
            ("dedupe", vec!["--dedupe"]),
        ] {
            let mut cli = CliInvoker::new().arg("cat").arg(&bad);
            // Plain cat never refuses unindexed input, so it refuses --force.
            let force = !flags.is_empty();
            for flag in flags {
                cli = cli.arg(flag);
            }
            if force {
                cli = cli.arg("--force");
            }
            let out = cli
                .arg("-o")
                .arg(dir.path().join(format!("out-{mode}-{insertion}.osm.pbf")))
                .run();
            assert!(!out.status.success(), "{mode}: {}", out.stderr_str());
            assert!(
                out.stderr_str().contains("a second OSMHeader blob"),
                "{mode}: {}",
                out.stderr_str()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Unclassifiable blocks: plain cat re-encodes them into indexed blocks
// ---------------------------------------------------------------------------

/// Minimal protobuf emitter for blocks the crate's writers never produce.
mod pb {
    use crate::common::adversarial::write_varint;

    pub fn zz(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)).cast_unsigned()
    }

    pub fn key(buf: &mut Vec<u8>, field: u64, wire: u64) {
        write_varint(buf, (field << 3) | wire);
    }

    pub fn uint(buf: &mut Vec<u8>, field: u64, v: u64) {
        key(buf, field, 0);
        write_varint(buf, v);
    }

    pub fn bytes(buf: &mut Vec<u8>, field: u64, data: &[u8]) {
        key(buf, field, 2);
        write_varint(buf, u64::try_from(data.len()).expect("length fits u64"));
        buf.extend_from_slice(data);
    }

    /// A packed field of raw varints.
    pub fn packed(buf: &mut Vec<u8>, field: u64, vals: impl IntoIterator<Item = u64>) {
        let mut body = Vec::new();
        for v in vals {
            write_varint(&mut body, v);
        }
        bytes(buf, field, &body);
    }

    /// A packed sint64 field of the deltas of `vals`.
    pub fn deltas(buf: &mut Vec<u8>, field: u64, vals: &[i64]) {
        let mut prev = 0i64;
        packed(
            buf,
            field,
            vals.iter().map(|&v| {
                let d = v - prev;
                prev = v;
                zz(d)
            }),
        );
    }

    /// An `Info` with every field set; `user` is a string-table index.
    pub fn info(version: i32, user: u64) -> Vec<u8> {
        let mut b = Vec::new();
        uint(&mut b, 1, u64::from(version.cast_unsigned()));
        uint(&mut b, 2, 1_600_000_000);
        uint(&mut b, 3, 77);
        uint(&mut b, 4, 5);
        uint(&mut b, 5, user);
        uint(&mut b, 6, 1);
        b
    }

    /// `DenseNodes` for `(id, version)` pairs; version -1 means no metadata.
    /// Node `id` sits at `(id * 10, id * 20)` and the first node is tagged
    /// `name=a` (string indices 1 and 2).
    pub fn dense(nodes: &[(i64, i32)], user: u64) -> Vec<u8> {
        let ids: Vec<i64> = nodes.iter().map(|n| n.0).collect();
        let mut b = Vec::new();
        deltas(&mut b, 1, &ids);
        let mut di = Vec::new();
        packed(
            &mut di,
            1,
            nodes.iter().map(|n| i64::from(n.1).cast_unsigned()),
        );
        deltas(&mut di, 2, &vec![1_600_000_000; nodes.len()]);
        deltas(&mut di, 3, &vec![77; nodes.len()]);
        deltas(&mut di, 4, &vec![5; nodes.len()]);
        let user = i64::try_from(user).expect("string index fits i64");
        deltas(&mut di, 5, &vec![user; nodes.len()]);
        packed(&mut di, 6, nodes.iter().map(|_| 1));
        bytes(&mut b, 5, &di);
        deltas(&mut b, 8, &ids.iter().map(|id| id * 10).collect::<Vec<_>>());
        deltas(&mut b, 9, &ids.iter().map(|id| id * 20).collect::<Vec<_>>());
        let mut kv = vec![1, 2, 0];
        kv.extend(std::iter::repeat_n(0, nodes.len() - 1));
        packed(&mut b, 10, kv);
        b
    }

    /// A plain `Node` at `(id * 10, id * 20)`, tagged `name=a`.
    pub fn node(id: i64, info: Option<&[u8]>) -> Vec<u8> {
        let mut b = Vec::new();
        uint(&mut b, 1, zz(id));
        packed(&mut b, 2, [1]);
        packed(&mut b, 3, [2]);
        if let Some(info) = info {
            bytes(&mut b, 4, info);
        }
        uint(&mut b, 8, zz(id * 10));
        uint(&mut b, 9, zz(id * 20));
        b
    }

    /// A `Way` tagged `highway=path` (indices 3 and 4), with inline
    /// locations when given.
    pub fn way(id: i64, refs: &[i64], locations: Option<&[(i64, i64)]>, info: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        uint(&mut b, 1, id.cast_unsigned());
        packed(&mut b, 2, [3]);
        packed(&mut b, 3, [4]);
        bytes(&mut b, 4, info);
        deltas(&mut b, 8, refs);
        if let Some(locations) = locations {
            deltas(
                &mut b,
                9,
                &locations.iter().map(|l| l.0).collect::<Vec<_>>(),
            );
            deltas(
                &mut b,
                10,
                &locations.iter().map(|l| l.1).collect::<Vec<_>>(),
            );
        }
        b
    }

    /// A `Relation` tagged `type=route` (indices 5 and 6) with members
    /// `(id, type, role index)`.
    pub fn relation(id: i64, members: &[(i64, u64, u64)], info: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        uint(&mut b, 1, id.cast_unsigned());
        packed(&mut b, 2, [5]);
        packed(&mut b, 3, [6]);
        bytes(&mut b, 4, info);
        packed(&mut b, 8, members.iter().map(|m| m.2));
        deltas(&mut b, 9, &members.iter().map(|m| m.0).collect::<Vec<_>>());
        packed(&mut b, 10, members.iter().map(|m| m.1));
        b
    }

    /// A `PrimitiveBlock` over the shared string table, one entry per group;
    /// each group is a list of `(field, message)` pairs.
    pub fn block(groups: &[Vec<(u64, Vec<u8>)>]) -> Vec<u8> {
        let mut st = Vec::new();
        for s in [
            "", "name", "a", "highway", "path", "type", "route", "outer", "alice",
        ] {
            bytes(&mut st, 1, s.as_bytes());
        }
        let mut b = Vec::new();
        bytes(&mut b, 1, &st);
        for group in groups {
            let mut g = Vec::new();
            for (field, msg) in group {
                bytes(&mut g, *field, msg);
            }
            bytes(&mut b, 2, &g);
        }
        b
    }
}

fn tag_list<'a>(tags: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<(String, String)> {
    tags.map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

type MetaRepr = Option<(i32, i64, i64, i32, Option<String>, bool)>;

fn info_meta(i: &pbfhogg::Info<'_>) -> MetaRepr {
    i.version().map(|v| {
        (
            v,
            i.milli_timestamp().unwrap_or(0),
            i.changeset().unwrap_or(0),
            i.uid().unwrap_or(0),
            i.user().map(|u| u.expect("user").to_string()),
            i.visible(),
        )
    })
}

/// Every element of a PBF in file order, with everything a re-encode must
/// keep: kind, id, coordinates, tags in order, refs, inline locations,
/// members, and metadata.
fn ordered_elements(path: &Path) -> Vec<String> {
    use pbfhogg::{BlobDecode, BlobReader, Element};

    let mut out = Vec::new();
    for blob in BlobReader::from_path(path).expect("open pbf") {
        let blob = blob.expect("read blob");
        let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") else {
            continue;
        };
        for element in block.elements() {
            out.push(match element {
                Element::DenseNode(n) => {
                    // The product's presence rule (`DenseNodeInfo::claims`):
                    // a padding slot (version 0, all zeros) is no metadata.
                    let claims = |i: &pbfhogg::DenseNodeInfo<'_>| {
                        i.claimed_version().is_some()
                            || (i.version() == 0
                                && (i.milli_timestamp() != 0
                                    || i.changeset() > 0
                                    || i.uid() > 0
                                    || !i.user().unwrap_or("").is_empty()
                                    || !i.visible()))
                    };
                    let meta = n.info().filter(|i| claims(i)).map(|i| {
                        (
                            i.version(),
                            i.milli_timestamp(),
                            i.changeset(),
                            i.uid(),
                            Some(i.user().expect("user").to_string()),
                            i.visible(),
                        )
                    });
                    format!(
                        "node {} {} {} {:?} {meta:?}",
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        tag_list(n.tags()),
                    )
                }
                Element::Node(n) => format!(
                    "node {} {} {} {:?} {:?}",
                    n.id(),
                    n.decimicro_lat(),
                    n.decimicro_lon(),
                    tag_list(n.tags()),
                    info_meta(&n.info()),
                ),
                Element::Way(w) => format!(
                    "way {} {:?} {:?} {:?} {:?}",
                    w.id(),
                    tag_list(w.tags()),
                    w.refs().collect::<Vec<_>>(),
                    w.node_locations()
                        .map(|l| (l.decimicro_lat(), l.decimicro_lon()))
                        .collect::<Vec<_>>(),
                    info_meta(&w.info()),
                ),
                Element::Relation(r) => format!(
                    "relation {} {:?} {:?} {:?}",
                    r.id(),
                    tag_list(r.tags()),
                    r.members()
                        .map(|m| (m.id, m.role().expect("role").to_string()))
                        .collect::<Vec<_>>(),
                    info_meta(&r.info()),
                ),
                other => panic!("unexpected element {other:?}"),
            });
        }
    }
    out
}

/// Every data block's kind, first and last id, and element count, in file
/// order.
fn block_layout(path: &Path) -> Vec<(&'static str, i64, i64, usize)> {
    use pbfhogg::{BlobDecode, BlobReader, Element};

    let mut out = Vec::new();
    for blob in BlobReader::from_path(path).expect("open pbf") {
        let blob = blob.expect("read blob");
        let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") else {
            continue;
        };
        let elements: Vec<(&'static str, i64)> = block
            .elements()
            .map(|e| match e {
                Element::DenseNode(n) => ("node", n.id()),
                Element::Node(n) => ("node", n.id()),
                Element::Way(w) => ("way", w.id()),
                Element::Relation(r) => ("relation", r.id()),
                other => panic!("unexpected element {other:?}"),
            })
            .collect();
        let (first, last) = (elements[0], elements[elements.len() - 1]);
        assert!(
            elements.iter().all(|e| e.0 == first.0),
            "a block holds more than one kind: {elements:?}"
        );
        out.push((first.0, first.1, last.1, elements.len()));
    }
    out
}

/// Write a PBF whose header declares `features` (and `Sort.Type_then_ID`
/// when `sorted`) and whose data blobs are `blocks`, each `(indexed, bytes)`.
fn write_raw_block_pbf(path: &Path, sorted: bool, features: &[&str], blocks: &[(bool, Vec<u8>)]) {
    use pbfhogg::block_builder::HeaderBuilder;
    use pbfhogg::writer::{Compression, PbfWriter};

    let mut hb = HeaderBuilder::new();
    if sorted {
        hb = hb.sorted();
    }
    for feature in features {
        hb = hb.optional_feature(*feature);
    }
    let header = hb.build().expect("header");
    let file = std::fs::File::create(path).expect("create input");
    let mut writer = PbfWriter::new(file, Compression::default());
    writer.write_header(&header).expect("write header");
    for (indexed, bytes) in blocks {
        if *indexed {
            writer.write_primitive_block(bytes).expect("write block");
        } else {
            writer
                .write_primitive_block_no_indexdata(bytes)
                .expect("write block");
        }
    }
    writer.flush().expect("flush fixture");
}

/// For every OsmData frame, whether its `BlobHeader` carries indexdata
/// (field 2). Parsed independently of the crate's own frame reader.
fn data_blob_indexdata(path: &Path) -> Vec<bool> {
    use crate::common::adversarial::{locate_blobs, read_varint};

    let bytes = std::fs::read(path).expect("read pbf");
    let mut out = Vec::new();
    for frame in locate_blobs(&bytes) {
        let header = &bytes[frame.header_start..frame.header_end];
        let (mut pos, mut is_data, mut indexed) = (0, false, false);
        while pos < header.len() {
            let (tag, n) = read_varint(&header[pos..]).expect("tag");
            pos += n;
            let (field, wire) = (tag >> 3, tag & 7);
            let (v, n) = read_varint(&header[pos..]).expect("value");
            pos += n;
            if wire == 2 {
                let len = usize::try_from(v).expect("len");
                if field == 1 {
                    is_data = &header[pos..pos + len] == b"OSMData";
                }
                indexed |= field == 2;
                pos += len;
            }
        }
        if is_data {
            out.push(indexed);
        }
    }
    out
}

/// The advised indexing recipe, `pbfhogg cat <input> -o indexed.osm.pbf`,
/// must index a blob whose block no single index can describe instead of
/// copying it through unindexed - otherwise the command that refused the
/// input refuses the recipe's output again. Plain cat re-encodes such a
/// block into single-kind blocks, in element order, splitting at a change of
/// kind, at the block cap and where node metadata presence flips (one
/// builder block cannot hold both without inventing metadata), and drops an
/// element-less block.
#[test]
fn passthrough_reencodes_unclassifiable_blocks_into_indexed_ones() {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};

    let dir = crate::common::test_scratch();
    let input = dir.path().join("unclassifiable.osm.pbf");
    let output = dir.path().join("indexed.osm.pbf");

    let alice = 8;
    let meta = pb::info(3, alice);
    let header = HeaderBuilder::new().sorted().build().expect("header");
    let file = std::fs::File::create(&input).expect("create input");
    let mut writer = PbfWriter::new(file, Compression::default());
    writer.write_header(&header).expect("write header");

    // 1. No elements: one empty group. First, so the first-blob probe sees
    //    an unindexed blob too.
    writer
        .write_primitive_block_no_indexdata(&pb::block(&[vec![]]))
        .expect("write empty block");
    // 2. One group carrying two element fields: dense nodes 1 and 2 with
    //    metadata, then plain node 3 without (a Some -> None flip).
    writer
        .write_primitive_block_no_indexdata(&pb::block(&[vec![
            (2, pb::dense(&[(1, 3), (2, 3)], alice)),
            (1, pb::node(3, None)),
        ]]))
        .expect("write multi-field block");
    // 3. An ordinary unindexed node block: the reindex arm.
    let mut bb = BlockBuilder::new();
    bb.add_node(4, 40, 80, [("name", "a")], None);
    bb.add_node(5, 50, 100, [], None);
    let bytes = bb.take().expect("take").expect("block").to_vec();
    writer
        .write_primitive_block_no_indexdata(&bytes)
        .expect("write ordinary block");
    // 4. Kinds mixed across groups: node 6 without metadata, then 8001
    //    nodes with it (a None -> Some flip, and a run over the 8000 cap),
    //    then two ways.
    let mut nodes = vec![(6, -1), (7, 3)];
    nodes.extend((100..8100).map(|id| (id, 3)));
    writer
        .write_primitive_block_no_indexdata(&pb::block(&[
            vec![(2, pb::dense(&nodes, alice))],
            vec![
                (3, pb::way(10, &[1, 2], None, &meta)),
                (3, pb::way(11, &[2, 3], None, &meta)),
            ],
        ]))
        .expect("write mixed node/way block");
    // 5. An ordinary indexed way block: the raw-copy arm.
    bb.add_way(12, [("highway", "path")], &[4, 5], None);
    let bytes = bb.take().expect("take").expect("block").to_vec();
    writer
        .write_primitive_block(&bytes)
        .expect("write indexed block");
    // 6. Kinds mixed across groups: a way with inline locations, then a
    //    relation with roles.
    writer
        .write_primitive_block_no_indexdata(&pb::block(&[
            vec![(3, pb::way(13, &[4, 5], Some(&[(40, 80), (50, 100)]), &meta))],
            vec![(4, pb::relation(20, &[(10, 1, 7), (1, 0, 0)], &meta))],
        ]))
        .expect("write mixed way/relation block");
    writer.flush().expect("flush fixture");
    drop(writer);

    // Only the ordinary way block carries indexdata in the input.
    assert_eq!(
        data_blob_indexdata(&input),
        [false, false, false, false, true, false]
    );

    // A command that needs indexdata on every blob refuses the input.
    let refused = CliInvoker::new()
        .arg("renumber")
        .arg(&input)
        .arg("-o")
        .arg(dir.path().join("renumbered-input.osm.pbf"))
        .run();
    assert!(!refused.status.success(), "{}", refused.stderr_str());
    refused.assert_stderr_contains("indexdata");

    let out = CliInvoker::new()
        .arg("cat")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .run();
    assert!(out.status.success(), "cat failed:\n{}", out.stderr_str());
    out.assert_stderr_contains("Decoded 4 blobs, wrote 8009 elements; 2 blobs passed through");
    // The note tells the three rebuilt blobs from the omitted empty one.
    out.assert_stderr_contains("Note: 3 blobs held blocks no single index describes");
    out.assert_stderr_contains("; 1 more such blobs yielded no elements and were omitted");

    // Every data blob is indexed, and the splits are the expected ones. The
    // empty block is gone.
    assert_eq!(data_blob_indexdata(&output), [true; 10]);
    assert_eq!(
        block_layout(&output),
        [
            ("node", 1, 2, 2),
            ("node", 3, 3, 1),
            ("node", 4, 5, 2),
            ("node", 6, 6, 1),
            ("node", 7, 8098, 8000),
            ("node", 8099, 8099, 1),
            ("way", 10, 11, 2),
            ("way", 12, 12, 1),
            ("way", 13, 13, 1),
            ("relation", 20, 20, 1),
        ]
    );

    // Same elements, same order, same content.
    assert_eq!(ordered_elements(&output), ordered_elements(&input));

    // The recipe's output passes the gate that refused its input.
    CliInvoker::new()
        .arg("renumber")
        .arg(&output)
        .arg("-o")
        .arg(dir.path().join("renumbered-output.osm.pbf"))
        .assert_success();
}

/// The re-encode keeps the input's element order, not type-then-ID order:
/// an unsorted block with repeated kind changes and descending ids within a
/// run comes out in the same sequence, one block per run.
#[test]
fn passthrough_reencode_keeps_unsorted_element_order() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("unsorted.osm.pbf");
    let output = dir.path().join("indexed.osm.pbf");
    let meta = pb::info(1, 8);
    write_raw_block_pbf(
        &input,
        false,
        &[],
        &[(
            false,
            pb::block(&[
                vec![(3, pb::way(31, &[9, 5], None, &meta))],
                vec![(1, pb::node(9, Some(&meta))), (1, pb::node(5, Some(&meta)))],
                vec![
                    (3, pb::way(30, &[5, 9], None, &meta)),
                    (3, pb::way(2, &[9], None, &meta)),
                ],
                vec![(1, pb::node(1, Some(&meta)))],
            ]),
        )],
    );

    let out = CliInvoker::new()
        .arg("cat")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .run();
    assert!(out.status.success(), "cat failed:\n{}", out.stderr_str());
    assert_eq!(ordered_elements(&output), ordered_elements(&input));
    assert_eq!(
        block_layout(&output),
        [
            ("way", 31, 31, 1),
            ("node", 9, 5, 2),
            ("way", 30, 2, 2),
            ("node", 1, 1, 1),
        ]
    );
    assert_eq!(data_blob_indexdata(&output), [true; 4]);
}

/// The re-encode cannot write `WayMembers-v1` (blob field 5) or
/// `SharedNodePins-v1` (way field 20). Where the output header declares one,
/// a way in an unclassifiable block is refused, located at its input frame,
/// rather than written without the enrichment the header promised.
#[test]
fn passthrough_reencode_refuses_to_drop_a_declared_way_enrichment() {
    let dir = crate::common::test_scratch();
    let meta = pb::info(1, 8);
    let mut pinned = pb::way(10, &[1, 2], None, &meta);
    pb::bytes(&mut pinned, 20, &[1]);
    let mixed =
        |way: Vec<u8>| pb::block(&[vec![(2, pb::dense(&[(1, 1), (2, 1)], 8))], vec![(3, way)]]);
    let indexed_node = {
        let mut bb = pbfhogg::block_builder::BlockBuilder::new();
        bb.add_node(100, 0, 0, [], None);
        bb.take().expect("take").expect("block").to_vec()
    };

    // WayMembers-v1 is declared only when every blob is copied whole, so the
    // first data blob is indexed.
    let members = dir.path().join("way-members.osm.pbf");
    write_raw_block_pbf(
        &members,
        false,
        &["pbfhogg.WayMembers-v1"],
        &[
            (true, indexed_node),
            (false, mixed(pb::way(10, &[1, 2], None, &meta))),
        ],
    );
    // SharedNodePins-v1 survives a reframe, so an unindexed first blob keeps
    // it declared.
    let pins = dir.path().join("pins.osm.pbf");
    write_raw_block_pbf(
        &pins,
        false,
        &["pbfhogg.SharedNodePins-v1"],
        &[(false, mixed(pinned))],
    );

    for (input, feature) in [
        (&members, "pbfhogg.WayMembers-v1"),
        (&pins, "pbfhogg.SharedNodePins-v1"),
    ] {
        let output = dir.path().join("out.osm.pbf");
        let out = CliInvoker::new()
            .arg("cat")
            .arg(input)
            .arg("-o")
            .arg(&output)
            .run();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{feature}: {}",
            out.stderr_str()
        );
        let stderr = out.stderr_str();
        assert!(
            stderr.contains(feature) && stderr.contains("re-encoding"),
            "{feature}: {stderr}"
        );
        assert!(
            stderr.contains(&input.display().to_string()) && stderr.contains("at byte offset"),
            "{feature}: the refusal must name its input frame; stderr:\n{stderr}"
        );
        assert!(
            !output.exists(),
            "{feature}: a refused cat published output"
        );
    }
}
