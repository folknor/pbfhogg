//! Structural roundtrip invariants.
//!
//! These tests pin algebraic properties of the command surface:
//! idempotence (`f(f(x)) == f(x)`), composability, and
//! inverse-pair roundtrips (`apply(base, derive(base, m)) == m`).
//! Unlike point tests that verify specific inputs produce specific
//! outputs, invariants catch silent drift in encoder/decoder
//! asymmetries, stable-sort guarantees, and operation commutativity
//! without needing hand-specified expected outputs.

mod common;

use common::{
    assert_elements_equivalent, generate_nodes, generate_relations, generate_ways,
    write_multi_block_test_pbf, write_test_pbf_sorted, TestMember, TestNode, TestRelation,
};
use pbfhogg::MemberId;
use pbfhogg::writer::Compression;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared fixture
// ---------------------------------------------------------------------------

fn sample_fixture(path: &Path) {
    let mut nodes = generate_nodes(10, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        if i < 3 {
            n.tags = vec![("amenity", "cafe")];
        }
    }
    let mut ways = generate_ways(4, 1_000, 3, 1);
    for (i, w) in ways.iter_mut().enumerate() {
        w.tags = if i % 2 == 0 {
            vec![("highway", "primary")]
        } else {
            vec![("building", "yes")]
        };
    }
    let relations = vec![TestRelation {
        id: 100,
        members: vec![TestMember { id: MemberId::Way(1_000), role: "outer" }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }];
    write_test_pbf_sorted(path, &nodes, &ways, &relations);
}

// ---------------------------------------------------------------------------
// sort idempotence
// ---------------------------------------------------------------------------

/// `sort(sort(x))` must be element-equivalent to `sort(x)`. Running a
/// second sort on already-sorted input should be a no-op modulo blob
/// layout; the element set must not drift.
#[test]
fn sort_idempotence() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let once = dir.path().join("once.osm.pbf");
    let twice = dir.path().join("twice.osm.pbf");
    sample_fixture(&input);

    let opts = pbfhogg::sort::SortOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
    };
    pbfhogg::commands::sort::sort(&input, &once, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("sort once");
    pbfhogg::commands::sort::sort(&once, &twice, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("sort twice");

    assert_elements_equivalent(&once, &twice);
}

// ---------------------------------------------------------------------------
// extract idempotence
// ---------------------------------------------------------------------------

/// `extract(extract(x, bbox), bbox)` element-equivalent to
/// `extract(x, bbox)`. The second extract on an already-clipped PBF
/// shouldn't change anything: every surviving element is within the
/// bbox by construction.
#[test]
fn extract_simple_idempotence() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let once = dir.path().join("once.osm.pbf");
    let twice = dir.path().join("twice.osm.pbf");
    sample_fixture(&input);

    let bbox = pbfhogg::commands::extract::parse_bbox("0.0,0.0,0.002,0.002")
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
    extract(&input, &once);
    extract(&once, &twice);

    assert_elements_equivalent(&once, &twice);
}

// ---------------------------------------------------------------------------
// derive -> apply roundtrip
// ---------------------------------------------------------------------------

/// Apply derive_changes(base, modified) back to base: should recover
/// modified exactly (element-equivalent). This is the strongest
/// encoder/decoder invariant in the crate - any asymmetry between
/// how derive serialises ops and how apply-changes interprets them
/// surfaces here.
///
/// Caveat: derive_changes by default does NOT increment version or
/// update timestamp on modified elements, which apply-changes expects
/// to match the base. Enable both flags so the produced OSC has
/// version N+1 and matching stamps, which is what apply-changes needs
/// to route a `modify` op correctly.
#[test]
fn derive_then_apply_roundtrip() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let modified = dir.path().join("modified.osm.pbf");
    let osc = dir.path().join("delta.osc.gz");
    let reconstructed = dir.path().join("reconstructed.osm.pbf");
    sample_fixture(&base);

    // Build `modified` from `base` with a create + delete + modify.
    let mut nodes = generate_nodes(10, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        if i < 3 {
            n.tags = vec![("amenity", "cafe")];
        }
    }
    nodes[1].tags = vec![("amenity", "restaurant")]; // modify
    nodes.remove(2); // delete n3
    nodes.push(TestNode {
        id: 100,
        lat: 555_000_000,
        lon: 120_000_000,
        tags: vec![],
        meta: None,
    }); // create
    let mut ways = generate_ways(4, 1_000, 3, 1);
    for (i, w) in ways.iter_mut().enumerate() {
        w.tags = if i % 2 == 0 {
            vec![("highway", "primary")]
        } else {
            vec![("building", "yes")]
        };
    }
    let relations = vec![TestRelation {
        id: 100,
        members: vec![TestMember { id: MemberId::Way(1_000), role: "outer" }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }];
    write_test_pbf_sorted(&modified, &nodes, &ways, &relations);

    // derive_changes(base, modified) -> delta.osc.gz
    let stats = pbfhogg::diff::derive::derive_changes(
        &base,
        &modified,
        &osc,
        false,
        true, // increment_version
        true, // update_timestamp
        1,
    )
    .expect("derive_changes");
    assert!(
        stats.creates + stats.modifies + stats.deletes > 0,
        "derive must produce non-empty delta"
    );

    // apply_changes(base, delta) -> reconstructed
    let opts = pbfhogg::apply_changes::MergeOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
        locations_on_ways: false,
        jobs: None,
    };
    pbfhogg::apply_changes::merge(&base, &osc, &reconstructed, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("apply_changes");

    assert_elements_equivalent(&modified, &reconstructed);
}

// ---------------------------------------------------------------------------
// tags_filter composability
// ---------------------------------------------------------------------------

/// Running `tags_filter` twice with different expressions on a
/// chained output must give the same element set as running once
/// with both expressions combined via OR semantics (tags_filter's
/// native combinator - `w/highway=primary` OR `w/building=yes` etc.).
///
/// Caveat: tags_filter's default `omit_referenced=false` pulls in
/// referenced nodes, which can differ between the chained and
/// single-pass runs because the chained second pass sees an
/// already-thinned node set. Use `omit_referenced: true` so the
/// test is a clean algebraic check on the element match logic.
/// tags_filter idempotence: running the same filter twice must be
/// element-equivalent to running it once. This is the cleanest
/// algebraic invariant on the filter - the second pass has nothing
/// new to match because the first already removed non-matching
/// elements.
///
/// Caveat: `omit_referenced: true` strips the two-pass node-closure
/// so the check is on direct matches only. Without this, the
/// referenced nodes behavior would make the test trivially false
/// (second pass sees only the already-kept nodes).
#[test]
fn tags_filter_idempotence() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let once = dir.path().join("once.osm.pbf");
    let twice = dir.path().join("twice.osm.pbf");
    sample_fixture(&input);

    let exprs = vec!["w/building=yes".to_string()];
    let opts = pbfhogg::tags_filter::TagsFilterOptions {
        expression_strs: &exprs,
        omit_referenced: true,
        invert: false,
        remove_tags: false,
        compression: Compression::default(),
        direct_io: false,
        force: true,
        jobs: None,
    };

    pbfhogg::tags_filter::tags_filter(&input, &once, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("tags_filter once");
    pbfhogg::tags_filter::tags_filter(&once, &twice, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("tags_filter twice");

    assert_elements_equivalent(&once, &twice);
}

// ---------------------------------------------------------------------------
// Blob-layout parity (Batch C)
// ---------------------------------------------------------------------------
//
// The same logical input laid out with different per-type block sizes
// (and therefore different blob counts) should produce identical
// element sets from any command - per-blob bookkeeping (classify
// schedules, passthrough decisions, parallel shard balance, merge-join
// block-pair state) must be layout-independent. These tests pin that
// invariant for a representative non-trivial command (`tags_filter`
// in two-pass mode) and for the raw read path (`read_normalized`).

fn write_at_block_size(path: &Path, block_size: usize) {
    let mut nodes = generate_nodes(30, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        if i < 10 {
            n.tags = vec![("amenity", "cafe")];
        }
    }
    let mut ways = generate_ways(10, 1_000, 3, 1);
    for (i, w) in ways.iter_mut().enumerate() {
        w.tags = if i % 2 == 0 {
            vec![("highway", "primary")]
        } else {
            vec![("building", "yes")]
        };
    }
    let relations = generate_relations(3, 10_000, 2, 1_000);
    write_multi_block_test_pbf(path, &nodes, &ways, &relations, block_size);
}

/// Reading the same logical PBF at different blob layouts must yield
/// the same normalized element set.
#[test]
fn read_path_blob_layout_independence() {
    let dir = TempDir::new().expect("tempdir");
    let b1 = dir.path().join("bs1.osm.pbf");
    let b5 = dir.path().join("bs5.osm.pbf");
    let b100 = dir.path().join("bs100.osm.pbf");
    write_at_block_size(&b1, 1);
    write_at_block_size(&b5, 5);
    write_at_block_size(&b100, 100);

    assert_elements_equivalent(&b1, &b5);
    assert_elements_equivalent(&b5, &b100);
}

/// `tags_filter` must produce the same output regardless of input
/// blob layout. Exercises `parallel_classify_phase` + follow-up
/// closure at different shard-work distributions.
#[test]
fn tags_filter_blob_layout_independence() {
    let dir = TempDir::new().expect("tempdir");
    let b1 = dir.path().join("bs1.osm.pbf");
    let b5 = dir.path().join("bs5.osm.pbf");
    let b100 = dir.path().join("bs100.osm.pbf");
    let out_1 = dir.path().join("o1.osm.pbf");
    let out_5 = dir.path().join("o5.osm.pbf");
    let out_100 = dir.path().join("o100.osm.pbf");
    write_at_block_size(&b1, 1);
    write_at_block_size(&b5, 5);
    write_at_block_size(&b100, 100);

    let exprs = vec!["w/highway=primary".to_string()];
    let opts = pbfhogg::tags_filter::TagsFilterOptions {
        expression_strs: &exprs,
        omit_referenced: false,
        invert: false,
        remove_tags: false,
        compression: Compression::default(),
        direct_io: false,
        force: true,
        jobs: None,
    };
    pbfhogg::tags_filter::tags_filter(&b1, &out_1, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("tags_filter bs=1");
    pbfhogg::tags_filter::tags_filter(&b5, &out_5, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("tags_filter bs=5");
    pbfhogg::tags_filter::tags_filter(&b100, &out_100, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("tags_filter bs=100");

    assert_elements_equivalent(&out_1, &out_5);
    assert_elements_equivalent(&out_5, &out_100);
}

/// `diff` must produce the same stats regardless of the new-side blob
/// layout when the logical content is identical. Different blob
/// layouts on the same content should produce byte-equal blob-pair
/// fast-path hits for matching layouts and overlapping-decode paths
/// otherwise - both yielding the same element-level stats.
#[test]
fn diff_blob_layout_independence() {
    let dir = TempDir::new().expect("tempdir");
    let old_b1 = dir.path().join("old_bs1.osm.pbf");
    let old_b100 = dir.path().join("old_bs100.osm.pbf");
    let new_b1 = dir.path().join("new_bs1.osm.pbf");
    let new_b100 = dir.path().join("new_bs100.osm.pbf");
    write_at_block_size(&old_b1, 1);
    write_at_block_size(&old_b100, 100);
    write_at_block_size(&new_b1, 1);
    write_at_block_size(&new_b100, 100);

    let opts = pbfhogg::diff::DiffOptions {
        suppress_common: false,
        verbose: false,
        summary: false,
        type_filter: None,
        jobs: 1,
    };
    let diff_stats = |old: &Path, new: &Path| -> pbfhogg::diff::DiffStats {
        let mut sink: Vec<u8> = Vec::new();
        pbfhogg::diff::diff(old, new, &mut sink, &opts, false).expect("diff")
    };

    let s_1_1 = diff_stats(&old_b1, &new_b1);
    let s_1_100 = diff_stats(&old_b1, &new_b100);
    let s_100_100 = diff_stats(&old_b100, &new_b100);

    // All four pairings compare the same logical content, so common
    // element count must match; differences must be zero.
    assert_eq!(s_1_1.common, s_100_100.common, "common stats diverge with layout");
    assert_eq!(s_1_100.common, s_100_100.common, "cross-layout common stats diverge");
    for (label, s) in [
        ("bs=1 vs bs=1", &s_1_1),
        ("bs=1 vs bs=100", &s_1_100),
        ("bs=100 vs bs=100", &s_100_100),
    ] {
        assert_eq!(s.created, 0, "{label}: same-content diff must have no created");
        assert_eq!(s.modified, 0, "{label}: same-content diff must have no modified");
        assert_eq!(s.deleted, 0, "{label}: same-content diff must have no deleted");
    }
}
