//! CLI-driven integration tests for `pbfhogg cat`.
//!
//! Replaces the library-API `tests/cat.rs`. Fixture PBFs are
//! written via the stable-allowlist writers; `cat` runs through
//! `CliInvoker`; output is verified by reading the resulting
//! PBF with the stable-allowlist readers, with stats inspected
//! through stderr (`CatStats::print_summary`). No imports from
//! `pbfhogg::cat::*` - a rewrite of `src/commands/cat/` cannot
//! break these tests by type changes alone.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;

use common::cli::CliInvoker;
use common::{
    generate_nodes, generate_ways, node_ids_with_coords as node_ids,
    read_all_elements_with_coords, read_normalized, relation_ids_with_coords as relation_ids,
    way_ids_with_coords as way_ids, write_multi_block_test_pbf, write_test_pbf, NormalizedPbf,
    TestMember, TestMeta, TestNode, TestRelation, TestWay,
};
use pbfhogg::MemberId;

/// Invoke `pbfhogg cat <inputs...> -o <output> [--type T] [-c A]... [--direct-io] --force`
/// and assert success. Returns the captured `CliOutput` so callers
/// can inspect stderr for stats and stdout if needed.
///
/// `clean_attrs` is the list of `-c <name>` flags to pass; passing
/// `&[]` is equivalent to `CleanAttrs::default()` (no fields cleaned).
fn run_cat(
    inputs: &[&Path],
    output: &Path,
    type_filter: Option<&str>,
    clean_attrs: &[&str],
    direct_io: bool,
) -> common::cli::CliOutput {
    let mut cli = CliInvoker::new().arg("cat");
    for input in inputs {
        cli = cli.arg(*input);
    }
    cli = cli.arg("-o").arg(output);
    if let Some(types) = type_filter {
        cli = cli.arg("--type").arg(types);
    }
    for attr in clean_attrs {
        cli = cli.arg("-c").arg(*attr);
    }
    if direct_io {
        cli = cli.arg("--direct-io");
    }
    cli.arg("--force").run()
}

#[test]
fn cat_passthrough_buffered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "path")], meta: None }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let out = run_cat(&[&input], &output, None, &[], false);
    assert!(out.status.success(), "cat failed; stderr:\n{}", out.stderr_str());
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
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "path")], meta: None }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let out = run_cat(&[&input], &output, Some("node"), &[], false);
    assert!(out.status.success(), "cat --type node failed; stderr:\n{}", out.stderr_str());

    let c = read_all_elements_with_coords(&output);
    assert_eq!(node_ids(&c), vec![1, 2]);
    assert!(way_ids(&c).is_empty(), "ways should be filtered out");
    assert!(relation_ids(&c).is_empty(), "relations should be filtered out");
}

#[test]
fn cat_type_filter_ways_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "path")], meta: None }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let out = run_cat(&[&input], &output, Some("way"), &[], false);
    assert!(out.status.success(), "cat --type way failed; stderr:\n{}", out.stderr_str());

    let c = read_all_elements_with_coords(&output);
    assert!(node_ids(&c).is_empty(), "nodes should be filtered out");
    assert_eq!(way_ids(&c), vec![10]);
    assert!(relation_ids(&c).is_empty(), "relations should be filtered out");
}

#[test]
fn cat_type_filter_node_way() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1], tags: vec![("highway", "path")], meta: None }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let out = run_cat(&[&input], &output, Some("node,way"), &[], false);
    assert!(out.status.success(), "cat --type node,way failed; stderr:\n{}", out.stderr_str());

    let c = read_all_elements_with_coords(&output);
    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![10]);
    assert!(relation_ids(&c).is_empty(), "relations should be filtered out");
}

// ---------------------------------------------------------------------------
// Multi-file concatenation
// ---------------------------------------------------------------------------

#[test]
fn cat_multi_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input1 = dir.path().join("a.osm.pbf");
    let input2 = dir.path().join("b.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input1,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")], meta: None }],
        &[],
        &[],
    );
    write_test_pbf(
        &input2,
        &[TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")], meta: None }],
        &[TestWay { id: 10, refs: vec![2], tags: vec![("highway", "road")], meta: None }],
        &[],
    );

    let out = run_cat(&[&input1, &input2], &output, None, &[], false);
    assert!(out.status.success(), "cat multi-file failed; stderr:\n{}", out.stderr_str());

    let c = read_all_elements_with_coords(&output);
    assert_eq!(c.nodes.len(), 2);
    assert_eq!(c.ways.len(), 1);
    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(way_ids(&c), vec![10]);
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
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "route")],
            meta,
        }],
    );
}

/// Run cat with the supplied `-c` flag list and return the normalized
/// output PBF. Shared by every `clean_*` test.
fn cat_with_clean(clean_attrs: &[&str]) -> NormalizedPbf {
    let dir = tempfile::tempdir().expect("tempdir");
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
        assert_eq!(m.timestamp, sentinel.timestamp, "{what} timestamp must survive");
        assert_eq!(m.changeset, sentinel.changeset, "{what} changeset must survive");
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
        assert_eq!(m.timestamp, sentinel.timestamp, "{what} timestamp must survive");
        assert_eq!(m.changeset, sentinel.changeset, "{what} changeset must survive");
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

    // Every cleanable field is zero / empty on every element type.
    for (what, meta) in [
        ("node", c.nodes[0].meta.as_ref()),
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
    let dir = tempfile::tempdir().expect("tempdir");
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
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // 20 nodes at block_size=10 -> 2 node blobs (both skipped).
    // 20 ways  at block_size=10 -> 2 way blobs  (both pass through raw).
    // No relations.
    let nodes = generate_nodes(20, 1);
    let ways = generate_ways(20, 1_000, 2, 1);
    write_multi_block_test_pbf(&input, &nodes, &ways, &[], 10);

    let out = run_cat(&[&input], &output, Some("way"), &[], false);
    assert!(out.status.success(), "cat --type way failed; stderr:\n{}", out.stderr_str());

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

// ---------------------------------------------------------------------------
// Platform tier
// ---------------------------------------------------------------------------
//
// `--direct-io` on a filesystem that supports O_DIRECT must produce
// identical output to the default path. Wrapped in `mod platform` so
// the brokkr platform profile (T11) can target it via
// `cargo test platform::`.

#[cfg(feature = "linux-direct-io")]
mod platform {
    use super::*;

    #[test]
    fn cat_passthrough_direct_io() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("input.osm.pbf");
        let output = dir.path().join("output.osm.pbf");

        write_test_pbf(
            &input,
            &[
                TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")], meta: None },
                TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")], meta: None },
            ],
            &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "path")], meta: None }],
            &[TestRelation {
                id: 20,
                members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
                tags: vec![("type", "multipolygon")],
                meta: None,
            }],
        );

        let out = run_cat(&[&input], &output, None, &[], true);
        if out.is_o_direct_unsupported() {
            eprintln!("O_DIRECT not supported on this filesystem, skipping test");
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
