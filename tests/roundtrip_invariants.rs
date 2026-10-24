//! Structural roundtrip invariants.
//!
//! These tests pin algebraic properties of the command surface:
//! idempotence (`f(f(x)) == f(x)`), composability, and
//! inverse-pair roundtrips (`apply(base, derive(base, m)) == m`).
//! Unlike point tests that verify specific inputs produce specific
//! outputs, invariants catch silent drift in encoder/decoder
//! asymmetries, stable-sort guarantees, and operation commutativity
//! without needing hand-specified expected outputs.

use crate::common::{
    TestMember, TestNode, TestRelation, TestWay, assert_elements_equivalent, generate_nodes,
    generate_relations, generate_ways, write_multi_block_test_pbf, write_test_pbf_sorted,
};
use pbfhogg::MemberId;
use pbfhogg::write::WriteOptions;
use pbfhogg::writer::Compression;
use std::path::Path;
use tempfile::TempDir;

/// Default backend with `force` set, at the given compression.
fn forced(compression: Compression) -> WriteOptions {
    WriteOptions {
        compression,
        force: true,
        ..WriteOptions::default()
    }
}

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
        members: vec![TestMember {
            id: MemberId::Way(1_000),
            role: "outer",
        }],
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

    let opts = forced(Compression::default());
    pbfhogg::commands::sort::sort(
        &input,
        crate::common::out(&once),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("sort once");
    pbfhogg::commands::sort::sort(
        &once,
        crate::common::out(&twice),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
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
#[cfg(feature = "commands")] // uses pbfhogg::extract, gated behind `commands`
#[test]
fn extract_simple_idempotence() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let once = dir.path().join("once.osm.pbf");
    let twice = dir.path().join("twice.osm.pbf");
    sample_fixture(&input);

    let bbox = pbfhogg::commands::extract::parse_bbox("0.0,0.0,0.002,0.002").expect("parse bbox");
    let region = pbfhogg::commands::extract::Region::Bbox(bbox);

    let extract = |input: &Path, output: &Path| {
        pbfhogg::commands::extract::extract(
            input,
            crate::common::out(output),
            &region,
            pbfhogg::commands::extract::ExtractStrategy::Simple,
            true,
            &pbfhogg::cat::CleanAttrs::default(),
            &forced(Compression::default()),
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
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
        members: vec![TestMember {
            id: MemberId::Way(1_000),
            role: "outer",
        }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }];
    write_test_pbf_sorted(&modified, &nodes, &ways, &relations);

    // derive_changes(base, modified) -> delta.osc.gz
    let stats = pbfhogg::diff::derive::derive_changes(
        &base,
        &modified,
        crate::common::out(&osc),
        false,
        true, // increment_version
        // Delete timestamp: any fixed instant stands in for "now" here.
        Some(1_700_000_000),
        pbfhogg::Parallelism::from_jobs(1),
        &crate::common::test_host(),
        crate::common::scratch_in(dir.path()),
    )
    .expect("derive_changes");
    assert!(
        stats.creates + stats.modifies + stats.deletes > 0,
        "derive must produce non-empty delta"
    );

    // apply_changes(base, delta) -> reconstructed
    let opts = pbfhogg::apply_changes::MergeOptions {
        write: forced(Compression::default()),
        locations_on_ways: false,
        ignore_missing_nodes: false,
        parallelism: pbfhogg::Parallelism::Auto,
        allow_version_regression: false,
        #[cfg(feature = "test-hooks")]
        panic_at_blob_seq: None,
    };
    pbfhogg::apply_changes::merge(
        &base,
        &osc,
        crate::common::out(&reconstructed),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("apply_changes");

    assert_elements_equivalent(&modified, &reconstructed);
}

// ---------------------------------------------------------------------------
// derive -> apply metadata preservation
// ---------------------------------------------------------------------------

/// The derive -> apply circle must preserve element metadata (version,
/// timestamp, changeset, uid, user), not just content.
/// `assert_elements_equivalent` compares `NormalizedMeta`, so any loss in
/// either half surfaces here. Regression test for the 2026-07-10 finding:
/// derive emitted only `version` and apply-changes dropped OSC metadata
/// entirely (every OSC-sourced element in merged output carried v0).
#[test]
#[allow(clippy::too_many_lines)]
fn derive_then_apply_preserves_metadata() {
    use crate::common::TestMeta;

    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let modified = dir.path().join("modified.osm.pbf");
    let osc = dir.path().join("delta.osc.gz");
    let reconstructed = dir.path().join("reconstructed.osm.pbf");

    let meta = |version: i32, user: &'static str| {
        Some(TestMeta {
            version,
            timestamp: 1_771_622_445, // 2026-02-20T21:20:45Z
            changeset: 123_456_789,
            uid: 4_242,
            user,
            visible: true,
        })
    };

    let base_nodes: Vec<TestNode> = (1..=6i32)
        .map(|i| TestNode {
            id: i64::from(i),
            lat: 550_000_000 + i * 1_000,
            lon: 120_000_000 + i * 1_000,
            tags: vec![],
            meta: meta(2, "base mapper"),
        })
        .collect();
    let base_ways = vec![TestWay {
        id: 1_000,
        refs: vec![1, 2, 3],
        tags: vec![("highway", "primary")],
        meta: meta(5, "way mapper"),
    }];
    let base_relations = vec![TestRelation {
        id: 100,
        members: vec![TestMember {
            id: MemberId::Way(1_000),
            role: "outer",
        }],
        tags: vec![("type", "multipolygon")],
        meta: meta(1, "rel mapper"),
    }];
    write_test_pbf_sorted(&base, &base_nodes, &base_ways, &base_relations);

    // Modified: bump one node (modify), add one node (create), touch the way.
    let mut mod_nodes = base_nodes;
    mod_nodes[1].tags = vec![("amenity", "cafe")];
    mod_nodes[1].meta = meta(3, "editing mapper");
    mod_nodes.push(TestNode {
        id: 50,
        lat: 556_000_000,
        lon: 126_000_000,
        tags: vec![("man_made", "flagpole")],
        meta: meta(1, "creator with spaces in name"),
    });
    mod_nodes.sort_by_key(|n| n.id);
    let mod_ways = vec![TestWay {
        id: 1_000,
        refs: vec![1, 2, 3, 4],
        tags: vec![("highway", "primary")],
        meta: meta(6, "way mapper"),
    }];
    // Genuinely modify the relation (tag change + version bump) so the
    // OSC carries it and the relation-modify write path is exercised -
    // an identical relation would pass through and prove nothing.
    let mod_relations = vec![TestRelation {
        id: 100,
        members: vec![TestMember {
            id: MemberId::Way(1_000),
            role: "outer",
        }],
        tags: vec![("type", "multipolygon"), ("landuse", "forest")],
        meta: meta(2, "rel editor"),
    }];
    write_test_pbf_sorted(&modified, &mod_nodes, &mod_ways, &mod_relations);

    let stats = pbfhogg::diff::derive::derive_changes(
        &base,
        &modified,
        crate::common::out(&osc),
        false,
        false, // increment_version: keep raw metadata
        None,  // delete timestamp: keep raw metadata
        pbfhogg::Parallelism::from_jobs(1),
        &crate::common::test_host(),
        crate::common::scratch_in(dir.path()),
    )
    .expect("derive_changes");
    assert!(
        stats.creates + stats.modifies > 0,
        "derive must produce creates/modifies"
    );

    let opts = pbfhogg::apply_changes::MergeOptions {
        write: forced(Compression::default()),
        locations_on_ways: false,
        ignore_missing_nodes: false,
        parallelism: pbfhogg::Parallelism::Auto,
        allow_version_regression: false,
        #[cfg(feature = "test-hooks")]
        panic_at_blob_seq: None,
    };
    pbfhogg::apply_changes::merge(
        &base,
        &osc,
        crate::common::out(&reconstructed),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("apply_changes");

    // Metadata-inclusive equivalence: NormalizedMeta is part of the compare.
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
        distrust_metadata: false,
        expression_strs: &exprs,
        omit_referenced: true,
        invert: false,
        remove_tags: false,
        write: forced(Compression::default()),
        parallelism: pbfhogg::Parallelism::Auto,
    };

    pbfhogg::tags_filter::tags_filter(
        &input,
        crate::common::out(&once),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("tags_filter once");
    pbfhogg::tags_filter::tags_filter(
        &once,
        crate::common::out(&twice),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
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
        distrust_metadata: false,
        expression_strs: &exprs,
        omit_referenced: false,
        invert: false,
        remove_tags: false,
        write: forced(Compression::default()),
        parallelism: pbfhogg::Parallelism::Auto,
    };
    pbfhogg::tags_filter::tags_filter(
        &b1,
        crate::common::out(&out_1),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("tags_filter bs=1");
    pbfhogg::tags_filter::tags_filter(
        &b5,
        crate::common::out(&out_5),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("tags_filter bs=5");
    pbfhogg::tags_filter::tags_filter(
        &b100,
        crate::common::out(&out_100),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
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
        parallelism: pbfhogg::Parallelism::from_jobs(1),
    };
    let diff_stats = |old: &Path, new: &Path| -> pbfhogg::diff::DiffStats {
        let mut sink: Vec<u8> = Vec::new();
        pbfhogg::diff::diff(
            old,
            new,
            &mut sink,
            &opts,
            false,
            &crate::common::test_host(),
            Some(crate::common::scratch_in(dir.path())),
        )
        .expect("diff")
    };

    let s_1_1 = diff_stats(&old_b1, &new_b1);
    let s_1_100 = diff_stats(&old_b1, &new_b100);
    let s_100_100 = diff_stats(&old_b100, &new_b100);

    // All four pairings compare the same logical content, so common
    // element count must match; differences must be zero.
    assert_eq!(
        s_1_1.common, s_100_100.common,
        "common stats diverge with layout"
    );
    assert_eq!(
        s_1_100.common, s_100_100.common,
        "cross-layout common stats diverge"
    );
    for (label, s) in [
        ("bs=1 vs bs=1", &s_1_1),
        ("bs=1 vs bs=100", &s_1_100),
        ("bs=100 vs bs=100", &s_100_100),
    ] {
        assert_eq!(
            s.created, 0,
            "{label}: same-content diff must have no created"
        );
        assert_eq!(
            s.modified, 0,
            "{label}: same-content diff must have no modified"
        );
        assert_eq!(
            s.deleted, 0,
            "{label}: same-content diff must have no deleted"
        );
    }
}

/// `diff_stats` is `diff` without the text: on every path (parallel shards,
/// the sequential block-pair merge, the unindexed element stream, the
/// negative-ID fallback) and every type filter, it either returns the same
/// four counts as an unsuppressed text diff of the same inputs, or fails
/// with the same error. An identical pair of corrupt files must fail both
/// ways, which a byte-equal shortcut in the count would not.
#[test]
fn diff_stats_matches_text_diff_on_every_path() {
    let dir = TempDir::new().expect("tempdir");
    let p = |name: &str| dir.path().join(name);
    write_diff_stats_fixtures(dir.path());

    let pairs = [
        ("indexed", "old.osm.pbf", "new.osm.pbf"),
        ("unindexed", "old-u.osm.pbf", "new-u.osm.pbf"),
        ("mixed", "old.osm.pbf", "new-u.osm.pbf"),
        ("negative ids", "old-neg.osm.pbf", "new-neg.osm.pbf"),
        ("identical", "old.osm.pbf", "old.osm.pbf"),
        ("identical corrupt", "corrupt.osm.pbf", "corrupt.osm.pbf"),
    ];
    let filters = [
        None,
        Some(pbfhogg::TypeFilter {
            nodes: true,
            ways: false,
            relations: false,
        }),
        Some(pbfhogg::TypeFilter {
            nodes: false,
            ways: true,
            relations: true,
        }),
    ];
    let counts = |s: pbfhogg::diff::DiffStats| (s.common, s.created, s.modified, s.deleted);
    let mut ok_runs = 0;
    for (label, old, new) in pairs {
        for type_filter in filters {
            for jobs in [1, 2, 4] {
                let parallelism = pbfhogg::Parallelism::from_jobs(jobs);
                let host = crate::common::test_host();
                let options = pbfhogg::diff::DiffOptions {
                    suppress_common: false,
                    verbose: false,
                    summary: false,
                    type_filter,
                    parallelism,
                };
                let text = pbfhogg::diff::diff(
                    &p(old),
                    &p(new),
                    &mut Vec::new(),
                    &options,
                    false,
                    &host,
                    Some(crate::common::scratch_in(dir.path())),
                )
                .map(counts)
                .map_err(|e| e.to_string());
                let stats = pbfhogg::diff::diff_stats(
                    &p(old),
                    &p(new),
                    type_filter,
                    parallelism,
                    false,
                    &host,
                )
                .map(counts)
                .map_err(|e| e.to_string());
                assert_eq!(
                    text, stats,
                    "{label}, filter {type_filter:?}, -j {jobs}: diff_stats disagrees with diff"
                );
                // The corrupt blob is the last one, a relation blob, so only
                // an unfiltered run is bound to read it.
                if label == "identical corrupt" && type_filter.is_none() {
                    assert!(
                        text.is_err(),
                        "{label}, -j {jobs}: a corrupt blob must fail"
                    );
                }
                ok_runs += usize::from(text.is_ok());
            }
        }
    }
    // The comparison above is vacuous if everything failed.
    assert!(ok_runs > 30, "only {ok_runs} runs succeeded");
}

/// The inputs of [`diff_stats_matches_text_diff_on_every_path`]: an indexed
/// pair with creates, deletes and modifies in every kind, the same content
/// unindexed, a mixed-sign indexed pair, and the old file with its last
/// blob's payload corrupted.
fn write_diff_stats_fixtures(dir: &Path) {
    use crate::common::{generate_nodes_with_negatives, write_test_pbf_non_indexed};
    let p = |name: &str| dir.join(name);

    let old_nodes = || generate_nodes(300, 1);
    let new_nodes = || {
        let mut nodes = generate_nodes(305, 1);
        nodes.retain(|n| n.id != 13);
        for n in &mut nodes {
            if n.id % 7 == 0 {
                n.lat = n.lat.saturating_add(1_000);
            }
        }
        nodes
    };
    let old_ways = || generate_ways(40, 1, 5, 1);
    let new_ways = || {
        let mut ways = generate_ways(40, 1, 5, 1);
        ways[2].tags.push(("highway", "residential"));
        ways.retain(|w| w.id != 40);
        ways
    };
    let rels = || generate_relations(5, 1, 2, 1);

    write_multi_block_test_pbf(&p("old.osm.pbf"), &old_nodes(), &old_ways(), &rels(), 25);
    write_multi_block_test_pbf(&p("new.osm.pbf"), &new_nodes(), &new_ways(), &rels(), 25);
    write_test_pbf_non_indexed(&p("old-u.osm.pbf"), &old_nodes(), &old_ways(), &rels());
    write_test_pbf_non_indexed(&p("new-u.osm.pbf"), &new_nodes(), &new_ways(), &rels());
    let mut neg_new = generate_nodes_with_negatives(20, 30);
    for n in neg_new.iter_mut().step_by(3) {
        n.lon = n.lon.saturating_add(1_000);
    }
    write_multi_block_test_pbf(
        &p("old-neg.osm.pbf"),
        &generate_nodes_with_negatives(20, 30),
        &[],
        &[],
        10,
    );
    write_multi_block_test_pbf(&p("new-neg.osm.pbf"), &neg_new, &[], &[], 10);
    let bytes = std::fs::read(p("old.osm.pbf")).expect("read");
    let last = crate::common::adversarial::locate_blobs(&bytes).len() - 1;
    let corrupt = crate::common::adversarial::mutate_blob_payload(&bytes, last, |payload| {
        *payload = vec![0xff; 16];
    });
    std::fs::write(p("corrupt.osm.pbf"), corrupt).expect("write corrupt");
}

/// A type filter that excludes a kind ahead of an enabled one answers the
/// same on every path: the sequential block-pair merge (`-j 1`, verbose,
/// and every negative-ID run) skips the excluded kind's blobs, the parallel
/// path never reads them, and the unindexed stream drains them by decoding.
/// The sequential merge used to start the later phase on the excluded
/// kind's first blob and refuse every such run as unsorted. The answer is
/// also pinned to the single-kind runs, which skip nothing.
#[test]
fn diff_type_filter_excluding_an_earlier_kind_agrees_on_every_path() {
    let dir = TempDir::new().expect("tempdir");
    write_diff_stats_fixtures(dir.path());
    write_negative_all_kinds_fixtures(dir.path());

    let filter = |nodes, ways, relations| pbfhogg::TypeFilter {
        nodes,
        ways,
        relations,
    };
    let run = |old: &str, new: &str, type_filter, jobs: usize, verbose: bool| {
        diff_counts(dir.path(), old, new, type_filter, jobs, verbose)
    };
    let add = |a: (u64, u64, u64, u64), b: (u64, u64, u64, u64)| {
        (a.0 + b.0, a.1 + b.1, a.2 + b.2, a.3 + b.3)
    };

    let pairs = [
        (
            "old.osm.pbf",
            "new.osm.pbf",
            "old-u.osm.pbf",
            "new-u.osm.pbf",
        ),
        (
            "old-negk.osm.pbf",
            "new-negk.osm.pbf",
            "old-negk-u.osm.pbf",
            "new-negk-u.osm.pbf",
        ),
    ];
    for (old, new, old_u, new_u) in pairs {
        let way = run(old, new, filter(false, true, false), 4, false);
        let relation = run(old, new, filter(false, false, true), 4, false);
        assert_ne!(
            way.1 + way.2 + way.3,
            0,
            "{old}: the way fixture must differ"
        );
        assert_ne!(
            relation.0 + relation.1 + relation.2 + relation.3,
            0,
            "{old}: the relation fixture must hold relations"
        );
        let expected = [
            (filter(false, true, false), way),
            (filter(false, false, true), relation),
            (filter(false, true, true), add(way, relation)),
            (
                filter(true, false, true),
                add(
                    run(old, new, filter(true, false, false), 4, false),
                    relation,
                ),
            ),
        ];
        for (type_filter, want) in expected {
            for (jobs, verbose) in [(1, false), (1, true), (2, false), (4, false)] {
                assert_eq!(
                    run(old, new, type_filter, jobs, verbose),
                    want,
                    "{old}, {type_filter:?}, -j {jobs}, verbose {verbose}"
                );
            }
            assert_eq!(
                run(old_u, new_u, type_filter, 1, false),
                want,
                "{old_u} (unindexed), {type_filter:?}"
            );
        }
    }
}

/// A mixed-sign pair holding every kind, indexed (`old-negk`, `new-negk`)
/// and unindexed (`-u`), so the negative-ID fallback (the sequential merge
/// at every -j) has an earlier kind to skip. Ways and relations differ.
fn write_negative_all_kinds_fixtures(dir: &Path) {
    use crate::common::{
        generate_nodes_with_negatives, generate_relations_with_negatives,
        generate_ways_with_negatives, write_test_pbf_non_indexed,
    };
    let p = |name: &str| dir.join(name);
    let nodes = generate_nodes_with_negatives(20, 30);
    let ways = generate_ways_with_negatives(10, 15, 3);
    let rels = generate_relations_with_negatives(3, 4, 2);
    // The fixture types are not `Clone`; the generators are deterministic.
    let mut new_ways = generate_ways_with_negatives(10, 15, 3);
    new_ways[1].tags.push(("highway", "service"));
    new_ways.retain(|w| w.id != 7);
    let mut new_rels = generate_relations_with_negatives(3, 4, 2);
    new_rels[0].tags.push(("type", "route"));
    write_multi_block_test_pbf(&p("old-negk.osm.pbf"), &nodes, &ways, &rels, 10);
    write_multi_block_test_pbf(&p("new-negk.osm.pbf"), &nodes, &new_ways, &new_rels, 10);
    write_test_pbf_non_indexed(&p("old-negk-u.osm.pbf"), &nodes, &ways, &rels);
    write_test_pbf_non_indexed(&p("new-negk-u.osm.pbf"), &nodes, &new_ways, &new_rels);
}

/// `(common, created, modified, deleted)` of a text diff of two files in
/// `dir`, panicking with the run's parameters on an error.
fn diff_counts(
    dir: &Path,
    old: &str,
    new: &str,
    type_filter: pbfhogg::TypeFilter,
    jobs: usize,
    verbose: bool,
) -> (u64, u64, u64, u64) {
    let options = pbfhogg::diff::DiffOptions {
        suppress_common: false,
        verbose,
        summary: false,
        type_filter: Some(type_filter),
        parallelism: pbfhogg::Parallelism::from_jobs(jobs),
    };
    let s = pbfhogg::diff::diff(
        &dir.join(old),
        &dir.join(new),
        &mut Vec::new(),
        &options,
        false,
        &crate::common::test_host(),
        Some(crate::common::scratch_in(dir)),
    )
    .unwrap_or_else(|e| {
        panic!("{old} vs {new}, {type_filter:?}, -j {jobs}, verbose {verbose}: {e}")
    });
    (s.common, s.created, s.modified, s.deleted)
}

/// Skipping an excluded kind does not wave a mis-ordered file through: a
/// node blob after a way blob is still refused as unsorted when nodes are
/// excluded, on the sequential path as on the parallel one.
#[test]
fn diff_type_filter_skip_still_refuses_an_earlier_kind_out_of_order() {
    let dir = TempDir::new().expect("tempdir");
    let p = |name: &str| dir.path().join(name);
    write_multi_block_test_pbf(
        &p("old.osm.pbf"),
        &generate_nodes(100, 1),
        &generate_ways(40, 1, 5, 1),
        &generate_relations(5, 1, 2, 1),
        25,
    );
    // Frames: the header, four node blobs, two way blobs, one relation blob.
    // Move the last node blob between the two way blobs.
    let bytes = std::fs::read(p("old.osm.pbf")).expect("read");
    let frames = crate::common::adversarial::locate_blobs(&bytes);
    assert_eq!(frames.len(), 8, "fixture layout");
    let frame = |i: usize| &bytes[frames[i].frame_start..frames[i].blob_end];
    let unsorted: Vec<u8> = [0, 1, 2, 3, 5, 4, 6, 7]
        .iter()
        .flat_map(|&i| frame(i).iter().copied())
        .collect();
    std::fs::write(p("unsorted.osm.pbf"), unsorted).expect("write unsorted");

    for type_filter in [
        pbfhogg::TypeFilter {
            nodes: false,
            ways: true,
            relations: false,
        },
        pbfhogg::TypeFilter {
            nodes: false,
            ways: false,
            relations: true,
        },
    ] {
        for (jobs, verbose) in [(1, false), (1, true), (4, false)] {
            let options = pbfhogg::diff::DiffOptions {
                suppress_common: false,
                verbose,
                summary: false,
                type_filter: Some(type_filter),
                parallelism: pbfhogg::Parallelism::from_jobs(jobs),
            };
            let err = pbfhogg::diff::diff(
                &p("unsorted.osm.pbf"),
                &p("old.osm.pbf"),
                &mut Vec::new(),
                &options,
                false,
                &crate::common::test_host(),
                Some(crate::common::scratch_in(dir.path())),
            )
            .expect_err("a node blob after a way blob must be refused");
            assert!(
                matches!(err.kind(), pbfhogg::ErrorKind::Unsorted { .. }),
                "{type_filter:?}, -j {jobs}, verbose {verbose}: {err}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Compression-level parity (Batch E)
// ---------------------------------------------------------------------------
//
// The compression layer (`none` / `zlib:N` / `zstd:N`) wraps blob
// payloads independently of element encoding. Changing the codec
// must not perturb the element set that a reader subsequently sees.
// These tests produce three outputs per command with identical
// logical input and distinct codecs, then assert all three outputs
// are element-equivalent.

#[test]
fn sort_compression_level_parity() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    sample_fixture(&input);

    let outputs = [
        ("none", Compression::None, dir.path().join("none.osm.pbf")),
        (
            "zlib6",
            Compression::Zlib(6),
            dir.path().join("zlib6.osm.pbf"),
        ),
        (
            "zstd3",
            Compression::Zstd(3),
            dir.path().join("zstd3.osm.pbf"),
        ),
    ];

    for (_label, compression, path) in &outputs {
        let opts = forced(*compression);
        pbfhogg::commands::sort::sort(
            &input,
            crate::common::out(path),
            &opts,
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
        )
        .expect("sort");
    }

    // All three outputs must round-trip to element-equivalent sets.
    assert_elements_equivalent(&outputs[0].2, &outputs[1].2);
    assert_elements_equivalent(&outputs[1].2, &outputs[2].2);
}

#[test]
fn tags_filter_compression_level_parity() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    sample_fixture(&input);

    let exprs = vec!["w/building=yes".to_string()];
    let outputs = [
        ("none", Compression::None, dir.path().join("none.osm.pbf")),
        (
            "zlib6",
            Compression::Zlib(6),
            dir.path().join("zlib6.osm.pbf"),
        ),
        (
            "zstd3",
            Compression::Zstd(3),
            dir.path().join("zstd3.osm.pbf"),
        ),
    ];

    for (_label, compression, path) in &outputs {
        let opts = pbfhogg::tags_filter::TagsFilterOptions {
            distrust_metadata: false,
            expression_strs: &exprs,
            omit_referenced: false,
            invert: false,
            remove_tags: false,
            write: forced(*compression),
            parallelism: pbfhogg::Parallelism::Auto,
        };
        pbfhogg::tags_filter::tags_filter(
            &input,
            crate::common::out(path),
            &opts,
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
        )
        .expect("tags_filter");
    }

    assert_elements_equivalent(&outputs[0].2, &outputs[1].2);
    assert_elements_equivalent(&outputs[1].2, &outputs[2].2);
}

/// `Compression::None` emits raw uncompressed blob payloads. That
/// code path is rarely exercised by test fixtures (the writer
/// default is Zlib(6)). Pin that a None-compressed output reads
/// back identically to a zlib-compressed twin.
#[test]
fn none_compression_reads_back_identical() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let out_none = dir.path().join("out_none.osm.pbf");
    let out_zlib = dir.path().join("out_zlib.osm.pbf");
    sample_fixture(&input);

    for (compression, output) in [
        (Compression::None, &out_none),
        (Compression::Zlib(6), &out_zlib),
    ] {
        let opts = forced(compression);
        pbfhogg::commands::sort::sort(
            &input,
            crate::common::out(output),
            &opts,
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
        )
        .expect("sort");
    }

    assert_elements_equivalent(&out_none, &out_zlib);

    // Byte-size comparison would be a nice sanity check but fixture
    // size is far below zlib's framing-overhead breakeven (2 KB-ish),
    // so on a 10-node fixture None often lands smaller than Zlib by
    // virtue of zlib's DEFLATE + wrapper bytes. Skip the size check
    // here; the element-equivalence assertion already pins that both
    // codecs serialize and deserialize the same logical content.
}
