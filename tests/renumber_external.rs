//! End-to-end tests for the planet-safe external renumber implementation.
//!
//! These tests exercise `pbfhogg::renumber_external::renumber_external`,
//! which lives alongside the in-memory `renumber` module. The external
//! path builds out progressively: pass 1 (this file's initial coverage)
//! handles node renumbering and node_map bucket emission, pass 2 adds
//! way refs, and the relation two-pass rounds it out. See
//! `notes/renumber-planet-scale.md` for the design and the task
//! breakdown in TODO.md.

mod common;

use common::{read_normalized, write_test_pbf_sorted, TestNode, TestRelation, TestWay};
use pbfhogg::renumber::RenumberOptions;
use pbfhogg::renumber_external::renumber_external;
use pbfhogg::writer::Compression;
use tempfile::TempDir;

fn default_opts() -> RenumberOptions {
    RenumberOptions {
        start_node_id: 1,
        start_way_id: 1,
        start_relation_id: 1,
    }
}

// ---------------------------------------------------------------------------
// Pass 1: nodes only
// ---------------------------------------------------------------------------

#[test]
fn external_pass1_renumbers_nodes_sequentially() {
    // Pass 1 reads nodes, assigns new sequential ids, writes renumbered
    // nodes to output, and emits (old_id, new_id) tuples into bucket
    // files (which we don't inspect directly from tests — bucket contents
    // are verified indirectly when pass 2 lands).
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 100, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
            TestNode { id: 200, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 300, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "c")] },
        ],
        &[],
        &[],
    );

    let stats = renumber_external(
        &input,
        &output,
        &default_opts(),
        Compression::default(),
        false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");

    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 0);
    assert_eq!(stats.relations_written, 0);

    let norm = read_normalized(&output);
    assert_eq!(norm.nodes.len(), 3);
    assert_eq!(norm.nodes[0].id, 1);
    assert_eq!(norm.nodes[0].lat, 100_000_000);
    assert_eq!(norm.nodes[0].lon, 200_000_000);
    assert_eq!(norm.nodes[0].tags.get("name"), Some(&"a".to_string()));
    assert_eq!(norm.nodes[1].id, 2);
    assert_eq!(norm.nodes[1].lat, 110_000_000);
    assert_eq!(norm.nodes[2].id, 3);
    assert_eq!(norm.nodes[2].tags.get("name"), Some(&"c".to_string()));
}

#[test]
fn external_pass1_respects_custom_start_id() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    let opts = RenumberOptions {
        start_node_id: 5000,
        start_way_id: 2000,
        start_relation_id: 3000,
    };

    let stats = renumber_external(
        &input, &output, &opts, Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");

    assert_eq!(stats.nodes_written, 2);
    let norm = read_normalized(&output);
    assert_eq!(norm.nodes.len(), 2);
    assert_eq!(norm.nodes[0].id, 5000);
    assert_eq!(norm.nodes[1].id, 5001);
}

#[test]
fn external_pass1_skips_ways_and_relations_for_now() {
    // Explicit documentation that the pass-1-only skeleton drops ways
    // and relations from the output. When task #3 and #4 land, this test
    // should be updated to assert the full roundtrip instead.
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 10, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 20, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[pbfhogg_test_way(100, vec![10, 20])],
        &[pbfhogg_test_relation(500)],
    );

    let stats = renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");

    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.ways_written, 0, "pass 1 skeleton does not write ways yet (task #3)");
    assert_eq!(stats.relations_written, 0, "pass 1 skeleton does not write relations yet (task #4)");

    let norm = read_normalized(&output);
    assert_eq!(norm.nodes.len(), 2);
    assert_eq!(norm.ways.len(), 0);
    assert_eq!(norm.relations.len(), 0);
}

// ---------------------------------------------------------------------------
// Pass 2 stage 2a: way-ref COO pair emission
// ---------------------------------------------------------------------------

#[test]
fn external_stage2a_runs_on_input_with_ways() {
    // Stage 2a streams way blobs and emits (old_node_id, slot_pos) COO
    // pairs into way_ref buckets + a per-blob ref-count sidecar. The test
    // input has multiple ways with overlapping refs — enough to exercise
    // the bucket partitioning logic — and the assertion just verifies the
    // function doesn't error out and pass 1 still produces the expected
    // nodes. Bucket contents become observable when stage 2b lands.
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] },
        ],
        &[
            // 3 ways × avg 3 refs = 9 total slots. Duplicate ref (2 twice
            // in way 100) exercises the scatter-back position preservation
            // that tasks #3 stages 2c/2d will need to get right.
            TestWay { id: 100, refs: vec![1, 2, 2], tags: vec![("highway", "stop")] },
            TestWay { id: 200, refs: vec![2, 3], tags: vec![] },
            TestWay { id: 300, refs: vec![3, 4, 1], tags: vec![("barrier", "gate")] },
        ],
        &[],
    );

    let stats = renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");

    // Pass 1 still wrote 4 nodes.
    assert_eq!(stats.nodes_written, 4);
    // Ways still not written to output (stage 2d is what does that).
    assert_eq!(stats.ways_written, 0, "stage 2d will populate this");
    let norm = read_normalized(&output);
    assert_eq!(norm.nodes.len(), 4);
    assert_eq!(norm.ways.len(), 0);
}

fn pbfhogg_test_way(id: i64, refs: Vec<i64>) -> TestWay {
    TestWay { id, refs, tags: vec![] }
}

fn pbfhogg_test_relation(id: i64) -> TestRelation {
    TestRelation {
        id,
        members: vec![],
        tags: vec![("type", "test")],
    }
}
