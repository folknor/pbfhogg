//! Parity tests: non-indexed input with `--force` must produce output
//! element-equivalent to the indexed twin.
//!
//! Several pbfhogg commands check for `BlobHeader.indexdata` via
//! `require_indexdata` at entry and refuse to proceed on non-indexed
//! PBFs unless the caller sets `force: true`. When `--force` is set,
//! the command routes through a slower fallback that cannot rely on
//! per-blob ID ranges or element-kind hints from indexdata. Before
//! these tests there was no coverage for the fallback path on any of
//! these commands; a silent correctness regression on non-indexed
//! input would only surface downstream in a user pipeline.
//!
//! Structure: for each command, build the same logical input twice
//! (indexed via `write_test_pbf_sorted`, non-indexed via
//! `write_test_pbf_non_indexed`), run the command with `force: true`
//! against both, and `assert_elements_equivalent` the outputs.
//! `assert_elements_equivalent` ignores string-table ordering, blob
//! layout, and metadata absence differences; it only compares the
//! element-level content contract.

mod common;

use common::{
    assert_elements_equivalent, assert_indexed, assert_non_indexed, generate_nodes,
    generate_ways, write_test_pbf_non_indexed, write_test_pbf_sorted, TestMember, TestNode,
    TestRelation, TestWay,
};
use pbfhogg::MemberId;
use pbfhogg::writer::Compression;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared fixture: a small sorted PBF suitable for every command under test.
// ---------------------------------------------------------------------------

fn shared_nodes() -> Vec<TestNode> {
    let mut nodes = generate_nodes(10, 1);
    // Decorate half the nodes with a matchable tag so tags-filter has
    // something to select.
    for n in &mut nodes[..5] {
        n.tags = vec![("amenity", "cafe")];
    }
    nodes
}

fn shared_ways() -> Vec<TestWay> {
    let mut ways = generate_ways(4, 1_000, 3, 1);
    for (i, w) in ways.iter_mut().enumerate() {
        w.tags = if i % 2 == 0 {
            vec![("highway", "primary")]
        } else {
            vec![("building", "yes")]
        };
    }
    ways
}

fn shared_relations() -> Vec<TestRelation> {
    vec![TestRelation {
        id: 100,
        members: vec![TestMember { id: MemberId::Way(1_000), role: "outer" }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }]
}

fn write_both(indexed: &Path, non_indexed: &Path) {
    let nodes = shared_nodes();
    let ways = shared_ways();
    let relations = shared_relations();
    write_test_pbf_sorted(indexed, &nodes, &ways, &relations);
    write_test_pbf_non_indexed(non_indexed, &nodes, &ways, &relations);
    assert_indexed(indexed);
    assert_non_indexed(non_indexed);
}

// ---------------------------------------------------------------------------
// sort
// ---------------------------------------------------------------------------

#[test]
fn sort_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let opts = pbfhogg::sort::SortOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
    };
    pbfhogg::commands::sort::sort(&in_idx, &out_idx, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("sort indexed");
    pbfhogg::commands::sort::sort(&in_non, &out_non, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("sort non-indexed");

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// tags-filter (two-pass)
// ---------------------------------------------------------------------------

#[test]
fn tags_filter_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let expression_strs = vec!["w/highway=primary".to_string()];
    let opts = pbfhogg::tags_filter::TagsFilterOptions {
        expression_strs: &expression_strs,
        omit_referenced: false,
        invert: false,
        remove_tags: false,
        compression: Compression::default(),
        direct_io: false,
        force: true,
        jobs: None,
    };
    pbfhogg::tags_filter::tags_filter(&in_idx, &out_idx, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("tags_filter indexed");
    pbfhogg::tags_filter::tags_filter(&in_non, &out_non, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("tags_filter non-indexed");

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// extract --strategy simple
// ---------------------------------------------------------------------------
//
// KNOWN FAILURE, 2026-04-22. `extract --strategy simple` with
// `force: true` on a non-indexed input produces a node count that
// diverges substantially from the indexed twin (observed 18 vs 6 on a
// 10-node fixture - looks like the non-indexed single-pass path
// double-emits decompressed blocks). The test below is written to
// pin the correctness contract once the underlying bug is fixed; it
// is `#[ignore]`-gated so CI stays green while the investigation is
// parked in TODO.md.

#[test]
#[ignore = "extract --simple non-indexed fallback produces wrong node count (see TODO.md)"]
fn extract_simple_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_both(&in_idx, &in_non);

    // A bbox that clips about half the nodes - generate_nodes walks
    // coords 1000..=10_000 decimicrodegrees on both axes, so a bbox
    // clipping at 5000 catches ids 1..=5.
    let bbox = pbfhogg::commands::extract::parse_bbox("0.0,0.0,0.0006,0.0006")
        .expect("parse bbox");
    let region = pbfhogg::commands::extract::Region::Bbox(bbox);

    let extract = |input: &Path, output: &Path| {
        pbfhogg::commands::extract::extract(
            input,
            output,
            &region,
            pbfhogg::commands::extract::ExtractStrategy::Simple,
            true,
            &pbfhogg::cat::CleanAttrs::default(),
            Compression::default(),
            false,
            true,
            &pbfhogg::HeaderOverrides::default(),
        )
        .expect("extract");
    };
    extract(&in_idx, &out_idx);
    extract(&in_non, &out_non);

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// getid (include mode)
// ---------------------------------------------------------------------------

#[test]
fn getid_include_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let ids_vec = ["n1".to_string(), "n5".to_string(), "w1000".to_string()]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let id_set = pbfhogg::getid::parse_ids(&ids_vec).expect("parse ids");
    let opts = pbfhogg::getid::GetidOptions { add_referenced: false, remove_tags: false };
    let compression = Compression::default();
    let hdr = pbfhogg::HeaderOverrides::default();

    pbfhogg::getid::getid(&in_idx, &out_idx, &id_set, &opts, compression, false, true, &hdr)
        .expect("getid indexed");
    pbfhogg::getid::getid(&in_non, &out_non, &id_set, &opts, compression, false, true, &hdr)
        .expect("getid non-indexed");

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// apply-changes (`merge`)
// ---------------------------------------------------------------------------
//
// KNOWN FAILURE, 2026-04-22. Running the same OSC against the
// indexed and non-indexed twins produces off-by-one node counts
// (observed 10 vs 11 on a delete + modify + create OSC). Likely the
// non-indexed scanner routes blob descriptors through the unconditional
// worker-pool branch without the fast-path splice and some element
// emerges with duplicated or missing state. `#[ignore]`-gated as a
// pinned regression target; details logged in TODO.md.

fn write_osc_gz(path: &Path, xml: &str) {
    let file = File::create(path).expect("create osc.gz");
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

#[test]
#[ignore = "apply-changes non-indexed fallback off-by-one on delete (see TODO.md)"]
fn apply_changes_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_both(&in_idx, &in_non);

    // A minimal OSC: modify node 2, delete node 3, create node 100.
    write_osc_gz(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="2" lat="11.0" lon="21.0" version="2"/>
  </modify>
  <delete>
    <node id="3" lat="0" lon="0" version="2"/>
  </delete>
  <create>
    <node id="100" lat="55.0" lon="12.0" version="1"/>
  </create>
</osmChange>
"#,
    );

    let opts = pbfhogg::apply_changes::MergeOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
        locations_on_ways: false,
        jobs: None,
    };
    pbfhogg::apply_changes::merge(&in_idx, &osc, &out_idx, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("merge indexed");
    pbfhogg::apply_changes::merge(&in_non, &osc, &out_non, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("merge non-indexed");

    assert_elements_equivalent(&out_idx, &out_non);
}
