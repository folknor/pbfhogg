//! CLI-driven integration tests for `pbfhogg diff --format osc`
//! (the derive-changes operation).
//!
//! Replaces `tests/derive_changes.rs`. `derive_changes.rs` lives
//! under `pbfhogg::diff::derive::derive_changes` but is logically
//! tied to the apply-changes pipeline because most tests do a
//! derive->apply roundtrip. The CLI surface is `pbfhogg diff
//! --format osc -o <osc> [--increment-version] [--update-timestamp]
//! [-j N] <old> <new>`.
//!
//! Roundtrip tests run both `pbfhogg diff --format osc` and
//! `pbfhogg apply-changes` via `CliInvoker`. No imports from
//! `pbfhogg::diff::derive::*` or `pbfhogg::apply_changes::*`.

#![allow(clippy::unwrap_used)]
#![allow(clippy::too_many_lines)]

mod common;

use std::io::Read;
use std::path::Path;

use common::cli::{CliInvoker, CliOutput};
use common::{
    assert_elements_equivalent, generate_nodes, generate_ways,
    node_ids_with_coords as node_ids, read_all_elements_with_coords as read_all_elements,
    relation_ids_with_coords as relation_ids, way_ids_with_coords as way_ids,
    write_multi_block_test_pbf, write_test_pbf, write_test_pbf_sorted, TestMember, TestNode,
    TestRelation, TestWay,
};
use pbfhogg::block_builder::{self, BlockBuilder, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::MemberId;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn run_derive(
    old: &Path,
    new: &Path,
    osc: &Path,
    increment_version: bool,
    update_timestamp: bool,
    jobs: Option<usize>,
) -> CliOutput {
    let mut cli = CliInvoker::new()
        .arg("diff")
        .arg("--format")
        .arg("osc")
        .arg("-o")
        .arg(osc);
    if increment_version {
        cli = cli.arg("--increment-version");
    }
    if update_timestamp {
        cli = cli.arg("--update-timestamp");
    }
    if let Some(j) = jobs {
        cli = cli.arg("-j").arg(j.to_string());
    }
    cli.arg(old).arg(new).run()
}

fn run_derive_simple(old: &Path, new: &Path, osc: &Path) -> CliOutput {
    let out = run_derive(old, new, osc, false, false, Some(1));
    assert!(
        out.status.success(),
        "diff --format osc failed; stderr:\n{}",
        out.stderr_str(),
    );
    out
}

fn run_apply(base: &Path, osc: &Path, output: &Path) -> CliOutput {
    let out = CliInvoker::new()
        .arg("apply-changes")
        .arg(base)
        .arg(osc)
        .arg("-o")
        .arg(output)
        .arg("--force")
        .run();
    assert!(
        out.status.success(),
        "apply-changes failed; stderr:\n{}",
        out.stderr_str(),
    );
    out
}

/// Parse `{total} changes: {creates} creates, {modifies} modifies,
/// {deletes} deletes` out of derive-changes' stderr.
fn parse_derive_stats(stderr: &str) -> Option<(u64, u64, u64)> {
    let line = stderr.lines().find(|l| l.contains(" changes: "))?;
    let nums: Vec<u64> = line
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() < 4 { return None; }
    // [total, creates, modifies, deletes]
    Some((nums[1], nums[2], nums[3]))
}

fn assert_derive_stats(stderr: &str, creates: u64, modifies: u64, deletes: u64) {
    let parsed = parse_derive_stats(stderr).unwrap_or_else(|| {
        panic!("could not parse derive stats; stderr:\n{stderr}")
    });
    assert_eq!(
        parsed,
        (creates, modifies, deletes),
        "derive stats mismatch; stderr:\n{stderr}",
    );
}

/// Read gzipped OSC into a string. Uses `MultiGzDecoder` so the
/// parallel-gzip writer's concatenated multi-member output decodes as
/// a single logical stream.
fn read_osc(path: &Path) -> String {
    let file = std::fs::File::open(path).expect("open osc");
    let mut gz = flate2::read::MultiGzDecoder::new(file);
    let mut xml = String::new();
    gz.read_to_string(&mut xml).expect("decompress osc");
    xml
}

/// Write a sorted PBF with version metadata on each element.
fn write_versioned_pbf(
    path: &Path,
    nodes: &[(i64, i32, i32, i32)],
    ways: &[(i64, Vec<i64>, i32)],
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
        let meta = Metadata { version: ver, timestamp: 0, changeset: 0, uid: 0, user: "", visible: true };
        bb.add_node(id, lat, lon, std::iter::empty::<(&str, &str)>(), Some(&meta));
    }
    if !bb.is_empty() && let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    for (id, refs, ver) in ways {
        let meta = Metadata { version: *ver, timestamp: 0, changeset: 0, uid: 0, user: "", visible: true };
        bb.add_way(*id, std::iter::empty::<(&str, &str)>(), refs, Some(&meta));
    }
    if !bb.is_empty() && let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush");
}

#[allow(clippy::cast_possible_wrap)]
fn write_roundtrip_multiblob_pair(old: &Path, new: &Path) {
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
        .map(|n| TestNode { id: n.id, lat: n.lat, lon: n.lon, tags: n.tags.clone(), meta: None })
        .collect();
    new_nodes.retain(|n| n.id != 23);
    if let Some(node5) = new_nodes.iter_mut().find(|n| n.id == 5) {
        node5.lat = 555_555;
        node5.lon = 444_444;
        node5.tags = vec![("name", "modified")];
    }
    new_nodes.push(TestNode { id: 30, lat: 300_000, lon: 600_000, tags: vec![("created", "yes")], meta: None });

    let mut new_ways: Vec<TestWay> = old_ways
        .iter()
        .map(|w| TestWay { id: w.id, refs: w.refs.clone(), tags: w.tags.clone(), meta: None })
        .collect();
    new_ways.retain(|w| w.id != 1_007);
    if let Some(way1003) = new_ways.iter_mut().find(|w| w.id == 1_003) {
        way1003.refs = vec![7, 5, 30];
        way1003.tags = vec![("highway", "secondary"), ("surface", "gravel")];
    }
    new_ways.push(TestWay { id: 2_000, refs: vec![5, 30, 6], tags: vec![("highway", "primary")], meta: None });

    write_multi_block_test_pbf(old, &old_nodes, &old_ways, &[], 4);
    write_multi_block_test_pbf(new, &new_nodes, &new_ways, &[], 4);
}

// ---------------------------------------------------------------------------
// Basic derive
// ---------------------------------------------------------------------------

#[test]
fn identical_files_no_changes() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    let nodes = [
        TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")], meta: None },
        TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
    ];
    let ways = [TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }];

    write_test_pbf_sorted(&old, &nodes, &ways, &[]);
    write_test_pbf_sorted(&new, &nodes, &ways, &[]);

    let out = run_derive_simple(&old, &new, &osc);
    assert_derive_stats(&out.stderr_str(), 0, 0, 0);
}

#[test]
fn create_only() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "new")], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );

    let out = run_derive_simple(&old, &new, &osc);
    assert_derive_stats(&out.stderr_str(), 2, 0, 0);
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![], meta: None }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );

    let out = run_derive_simple(&old, &new, &osc);
    assert_derive_stats(&out.stderr_str(), 0, 0, 2);
}

#[test]
fn modify_node_coords() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(&old, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode { id: 1, lat: 150_000_000, lon: 250_000_000, tags: vec![], meta: None }], &[], &[]);

    let out = run_derive_simple(&old, &new, &osc);
    assert_derive_stats(&out.stderr_str(), 0, 1, 0);
}

#[test]
fn modify_node_tags() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(&old, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "old")], meta: None }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "new")], meta: None }], &[], &[]);

    let out = run_derive_simple(&old, &new, &osc);
    let (_, modifies, _) = parse_derive_stats(&out.stderr_str()).expect("stats");
    assert_eq!(modifies, 1);
}

#[test]
fn modify_way_refs() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(&old, &[], &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }], &[]);
    write_test_pbf_sorted(&new, &[], &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None }], &[]);

    let out = run_derive_simple(&old, &new, &osc);
    let (_, modifies, _) = parse_derive_stats(&out.stderr_str()).expect("stats");
    assert_eq!(modifies, 1);
}

#[test]
fn modify_relation_members() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf_sorted(&old, &[], &[], &[TestRelation {
        id: 100,
        members: vec![TestMember { id: MemberId::Node(1), role: "stop" }],
        tags: vec![("type", "route")],
        meta: None,
    }]);
    write_test_pbf_sorted(&new, &[], &[], &[TestRelation {
        id: 100,
        members: vec![
            TestMember { id: MemberId::Node(1), role: "stop" },
            TestMember { id: MemberId::Way(2), role: "outer" },
        ],
        tags: vec![("type", "route")],
        meta: None,
    }]);

    let out = run_derive_simple(&old, &new, &osc);
    let (_, modifies, _) = parse_derive_stats(&out.stderr_str()).expect("stats");
    assert_eq!(modifies, 1);
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "one")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "ONE")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );

    let out = run_derive_simple(&old, &new, &osc);
    assert_derive_stats(&out.stderr_str(), 1, 2, 1);
}

// ---------------------------------------------------------------------------
// Roundtrip with apply-changes
// ---------------------------------------------------------------------------

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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "one")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("to_delete", "yes")], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "ONE")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 5, lat: 140_000_000, lon: 240_000_000, tags: vec![("new", "yes")], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 5], tags: vec![("highway", "secondary")], meta: None }],
        &[],
    );

    let derive_out = run_derive_simple(&old, &new, &osc);
    assert_derive_stats(&derive_out.stderr_str(), 1, 2, 1);

    run_apply(&old, &osc, &result);

    let result_contents = read_all_elements(&result);
    let new_contents = read_all_elements(&new);

    assert_eq!(node_ids(&result_contents), node_ids(&new_contents));
    for (r, n) in result_contents.nodes.iter().zip(new_contents.nodes.iter()) {
        assert_eq!(r.0, n.0);
        assert_eq!(r.1, n.1, "node lat mismatch for id={}", r.0);
        assert_eq!(r.2, n.2, "node lon mismatch for id={}", r.0);
        assert_eq!(r.3, n.3, "node tags mismatch for id={}", r.0);
    }
    assert_eq!(way_ids(&result_contents), way_ids(&new_contents));
    for (r, n) in result_contents.ways.iter().zip(new_contents.ways.iter()) {
        assert_eq!(r.0, n.0);
        assert_eq!(r.1, n.1, "way refs mismatch for id={}", r.0);
        assert_eq!(r.2, n.2, "way tags mismatch for id={}", r.0);
    }
    assert_eq!(relation_ids(&result_contents), relation_ids(&new_contents));
}

// ---------------------------------------------------------------------------
// Error path
// ---------------------------------------------------------------------------

#[test]
fn unsorted_input_rejected() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf(&old, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }], &[], &[]);

    let out = run_derive(&old, &new, &osc, false, false, Some(1));
    assert!(!out.status.success(), "should reject unsorted input; stderr:\n{}", out.stderr_str());

    let stderr = out.stderr_str();
    assert!(stderr.contains("not sorted"), "stderr:\n{stderr}");
    assert!(stderr.contains("Sort.Type_then_ID"), "stderr:\n{stderr}");
    assert!(stderr.contains("pbfhogg sort"), "stderr:\n{stderr}");
}

// ---------------------------------------------------------------------------
// --increment-version, --update-timestamp
// ---------------------------------------------------------------------------

#[test]
fn increment_version_bumps_delete_versions() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_versioned_pbf(
        &old,
        &[(1, 100_000_000, 200_000_000, 3), (2, 110_000_000, 210_000_000, 5)],
        &[(10, vec![1, 2], 2)],
    );
    write_versioned_pbf(&new, &[(1, 100_000_000, 200_000_000, 3)], &[]);

    let out = run_derive(&old, &new, &osc, true, false, Some(1));
    assert!(out.status.success(), "stderr:\n{}", out.stderr_str());

    let xml = read_osc(&osc);
    assert!(xml.contains(r#"id="2"#), "should contain node id=2; xml:\n{xml}");
    assert!(xml.contains(r#"version="6""#), "node 2 version should be 6; xml:\n{xml}");
    assert!(xml.contains(r#"id="10"#), "should contain way id=10; xml:\n{xml}");
    assert!(xml.contains(r#"version="3""#), "way 10 version should be 3; xml:\n{xml}");
}

#[test]
fn no_increment_version_preserves_delete_versions() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_versioned_pbf(
        &old,
        &[(1, 100_000_000, 200_000_000, 3), (2, 110_000_000, 210_000_000, 5)],
        &[],
    );
    write_versioned_pbf(&new, &[(1, 100_000_000, 200_000_000, 3)], &[]);

    let out = run_derive_simple(&old, &new, &osc);
    let (_, _, deletes) = parse_derive_stats(&out.stderr_str()).expect("stats");
    assert_eq!(deletes, 1);

    let xml = read_osc(&osc);
    assert!(xml.contains(r#"version="5""#), "node 2 version should be 5 (unchanged); xml:\n{xml}");
}

#[test]
fn increment_version_and_update_timestamp_combined() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_versioned_pbf(&old, &[(1, 100_000_000, 200_000_000, 2)], &[(10, vec![1], 4)]);
    write_versioned_pbf(&new, &[], &[]);

    let out = run_derive(&old, &new, &osc, true, true, Some(1));
    assert!(out.status.success(), "stderr:\n{}", out.stderr_str());

    let xml = read_osc(&osc);
    assert!(xml.contains(r#"version="3""#), "node 1 version should be 3 (was 2); xml:\n{xml}");
    assert!(xml.contains(r#"version="5""#), "way 10 version should be 5 (was 4); xml:\n{xml}");
    assert!(xml.contains("timestamp="), "delete elements should have a timestamp; xml:\n{xml}");
    assert!(xml.contains("timestamp=\"20"), "timestamp should be a recent ISO date; xml:\n{xml}");
}

// ---------------------------------------------------------------------------
// jobs parity + roundtrip stats
// ---------------------------------------------------------------------------

#[test]
fn derive_changes_jobs_parity_roundtrips_to_same_output() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc_seq = dir.path().join("changes_seq.osc.gz");
    let osc_par = dir.path().join("changes_par.osc.gz");
    let out_seq = dir.path().join("result_seq.osm.pbf");
    let out_par = dir.path().join("result_par.osm.pbf");

    write_roundtrip_multiblob_pair(&old, &new);

    let seq = run_derive(&old, &new, &osc_seq, false, false, Some(1));
    assert!(seq.status.success(), "derive seq; stderr:\n{}", seq.stderr_str());
    let par = run_derive(&old, &new, &osc_par, false, false, Some(4));
    assert!(par.status.success(), "derive par; stderr:\n{}", par.stderr_str());

    let seq_stats = parse_derive_stats(&seq.stderr_str()).expect("seq stats");
    let par_stats = parse_derive_stats(&par.stderr_str()).expect("par stats");
    assert_eq!(seq_stats, par_stats, "derive stats diverge under -j 4");

    run_apply(&old, &osc_seq, &out_seq);
    run_apply(&old, &osc_par, &out_par);

    assert_elements_equivalent(&out_seq, &out_par);
    assert_elements_equivalent(&out_seq, &new);
    assert_elements_equivalent(&out_par, &new);
}

/// Pins the merge-stats parity contract between the all-features and
/// consumer feature sweeps. The merge stats line accounting must match
/// the actual element count in the output PBF after a derive→apply
/// roundtrip.
#[test]
fn merge_stats_match_output_counts_after_roundtrip() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");
    let result = dir.path().join("result.osm.pbf");

    write_roundtrip_multiblob_pair(&old, &new);
    run_derive_simple(&old, &new, &osc);
    let merge_out = run_apply(&old, &osc, &result);

    assert_elements_equivalent(&result, &new);

    let result_contents = read_all_elements(&result);
    let result_nodes = u64::try_from(result_contents.nodes.len()).expect("node count");
    let result_ways = u64::try_from(result_contents.ways.len()).expect("way count");
    let result_relations =
        u64::try_from(result_contents.relations.len()).expect("relation count");
    let total_elements = result_nodes + result_ways + result_relations;

    let stderr = merge_out.stderr_str();

    // Pin the "Merge complete: {N} elements written" total.
    assert!(
        stderr.contains(&format!("Merge complete: {total_elements} elements written")),
        "MergeStats::total_elements must equal actual output element count; \
         expected {total_elements} elements; stderr:\n{stderr}",
    );

    // Pin the partition: base_X + diff_X == output_X for each kind.
    // The Base/Diff lines are "  Base: N nodes, M ways, K relations"
    // / "  Diff: A nodes, B ways, C relations".
    fn extract_three(line: &str) -> Option<(u64, u64, u64)> {
        let nums: Vec<u64> = line
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        if nums.len() >= 3 { Some((nums[0], nums[1], nums[2])) } else { None }
    }
    let base_line = stderr.lines().find(|l| l.trim_start().starts_with("Base:")).expect("Base line");
    let diff_line = stderr.lines().find(|l| l.trim_start().starts_with("Diff:")).expect("Diff line");
    let (b_n, b_w, b_r) = extract_three(base_line).expect("Base nums");
    let (d_n, d_w, d_r) = extract_three(diff_line).expect("Diff nums");

    assert_eq!(b_n + d_n, result_nodes, "node stats must partition the output node set");
    assert_eq!(b_w + d_w, result_ways, "way stats must partition the output way set");
    assert_eq!(b_r + d_r, result_relations, "relation stats must partition the output relation set");
}

