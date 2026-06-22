//! Mixed-sign-id invariant sweep.
//!
//! pbfhogg rejects negative input ids project-wide (see
//! [`DEVIATIONS.md`] "Negative input IDs rejected project-wide" and
//! [`decisions/0002-negative-ids-rejected-project-wide.md`] for the
//! decision record). The named enforcement sites are:
//!
//! - `renumber` - **hard reject** at three entry points
//!   (`src/commands/renumber/wire_rewrite.rs::reframe_dense_with_new_ids`,
//!   `reframe_ways_with_new_ids`, `rewrite_relations_with_new_ids`).
//!   Returns an error naming the offending id; never panics, never
//!   silently passes through.
//! - `diff` / `derive-changes` parallel shard planners - `debug_assert!`
//!   only. Release builds rely on the upstream chain never producing
//!   mixed-sign input. **Not exercisable from CLI tests** because
//!   `brokkr check` runs in release mode.
//!
//! Other commands (`cat`, `sort`, `inspect`, `tags-filter`, `getid`)
//! are NOT named as enforcement sites in DEVIATIONS. The contract this
//! file pins for them is panic-freedom on mixed-sign input. Their
//! current behavior varies (cat/sort preserve, tags-filter silently
//! drops the negative-id ways through its parallel-classify path) -
//! see TODO.md for the open question of whether they should promote
//! to clean-error rejection. The tests below pin the *current* status
//! quo, not a forward-looking contract.
//!
//! Tests use only the stable allowlist (CliInvoker + the existing
//! fixture writers) so internal-module rewrites cannot break them.
//!
//! [`DEVIATIONS.md`]: ../../DEVIATIONS.md
//! [`decisions/0002-negative-ids-rejected-project-wide.md`]: ../../decisions/0002-negative-ids-rejected-project-wide.md

#[path = "common/mod.rs"]
mod common;

use std::path::Path;

use common::cli::CliInvoker;
use common::{
    generate_nodes_with_negatives, generate_relations_with_negatives, generate_ways_with_negatives,
    node_ids_id_only, read_all_elements_id_only, read_normalized, relation_ids_id_only,
    way_ids_id_only, write_test_pbf, write_test_pbf_sorted,
};
use tempfile::TempDir;

const N_NEG: usize = 3;
const N_POS: usize = 3;

/// Build a small mixed-sign fixture: 3 negative + 3 positive nodes, ways,
/// and relations. The order matches canonical OSM sort
/// (`-1, -2, -3, 1, 2, 3` per kind, see `osm_id_cmp` in
/// `src/osm_id.rs`). Header carries `Sort.Type_then_ID` and the file is
/// indexed - same shape production PBFs use.
fn build_fixture(path: &Path) {
    let nodes = generate_nodes_with_negatives(N_NEG, N_POS);
    let ways = generate_ways_with_negatives(N_NEG, N_POS, 3);
    let relations = generate_relations_with_negatives(N_NEG, N_POS, 2);
    write_test_pbf_sorted(path, &nodes, &ways, &relations);
}

/// Same shape as `build_fixture` but writes an unsorted header. Used by
/// the `sort` test below to exercise the sort code path on mixed-sign
/// input.
fn build_unsorted_fixture(path: &Path) {
    let nodes = generate_nodes_with_negatives(N_NEG, N_POS);
    let ways = generate_ways_with_negatives(N_NEG, N_POS, 3);
    let relations = generate_relations_with_negatives(N_NEG, N_POS, 2);
    write_test_pbf(path, &nodes, &ways, &relations);
}

/// `cat` (no flags) is the canonical passthrough path. It re-runs
/// indexdata generation but otherwise touches the bytes the minimum
/// amount. Mixed-sign ids must survive end-to-end.
#[test]
fn cat_preserves_mixed_sign_ids() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    build_fixture(&input);

    CliInvoker::new()
        .arg("cat")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .assert_success();

    let contents = read_all_elements_id_only(&output);
    let nodes = node_ids_id_only(&contents);
    let ways = way_ids_id_only(&contents);
    let relations = relation_ids_id_only(&contents);
    assert_eq!(nodes.len(), N_NEG + N_POS, "node count must survive cat");
    assert_eq!(ways.len(), N_NEG + N_POS, "way count must survive cat");
    assert_eq!(
        relations.len(),
        N_NEG + N_POS,
        "relation count must survive cat",
    );
    let mut neg = nodes.iter().filter(|id| **id < 0).count();
    let mut pos = nodes.iter().filter(|id| **id > 0).count();
    assert_eq!(neg, N_NEG, "all negative node ids preserved");
    assert_eq!(pos, N_POS, "all positive node ids preserved");
    neg = ways.iter().filter(|id| **id < 0).count();
    pos = ways.iter().filter(|id| **id > 0).count();
    assert_eq!(neg, N_NEG, "all negative way ids preserved");
    assert_eq!(pos, N_POS, "all positive way ids preserved");
    // Tier A4 follow-up: relation passthrough was previously
    // unchecked; add the same neg/pos parity for relations so a
    // regression that drops mixed-sign relations doesn't slip past.
    neg = relations.iter().filter(|id| **id < 0).count();
    pos = relations.iter().filter(|id| **id > 0).count();
    assert_eq!(neg, N_NEG, "all negative relation ids preserved");
    assert_eq!(pos, N_POS, "all positive relation ids preserved");
}

/// `inspect` with no subcommand prints summary stats. It must not
/// panic on mixed-sign input. We don't check the report text - that's a
/// downstream assertion; pinning panic-freedom is enough.
#[test]
fn inspect_handles_mixed_sign_ids() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    build_fixture(&input);

    let out = CliInvoker::new().arg("inspect").arg(&input).run();
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("panicked at"),
        "inspect must not panic on mixed-sign ids; stderr:\n{stderr}",
    );
}

/// `sort` on unsorted mixed-sign input must produce a sorted output that
/// preserves every id. Order check is structural via `read_normalized`,
/// which sorts internally - so we only verify count+presence.
#[test]
fn sort_preserves_mixed_sign_ids() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    build_unsorted_fixture(&input);

    CliInvoker::new()
        .arg("sort")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .assert_success();

    let n = read_normalized(&output);
    assert_eq!(n.nodes.len(), N_NEG + N_POS, "all nodes survive sort");
    assert_eq!(n.ways.len(), N_NEG + N_POS, "all ways survive sort");
    assert_eq!(
        n.relations.len(),
        N_NEG + N_POS,
        "all relations survive sort"
    );
    // Tier A5 follow-up: previously only nodes had the neg/pos
    // filter check; ways and relations were count-only, so a
    // regression that loses the sign of every way/relation while
    // keeping counts intact passed silently.
    let assert_split = |kind: &str, ids: &[i64]| {
        let neg = ids.iter().filter(|id| **id < 0).count();
        let pos = ids.iter().filter(|id| **id > 0).count();
        assert_eq!(neg, N_NEG, "all negative {kind} ids preserved by sort");
        assert_eq!(pos, N_POS, "all positive {kind} ids preserved by sort");
    };
    assert_split("node", &n.nodes.iter().map(|x| x.id).collect::<Vec<_>>());
    assert_split("way", &n.ways.iter().map(|x| x.id).collect::<Vec<_>>());
    assert_split(
        "relation",
        &n.relations.iter().map(|x| x.id).collect::<Vec<_>>(),
    );
}

/// `tags-filter` is a re-encode that walks every element through the
/// classifier. The contract this test pins is **panic-freedom** on
/// mixed-sign input - the classifier's parallel path is known to be
/// shaped for production-positive ids only, and silently dropping
/// negative-id elements is a documented current behavior, not a
/// regression. The reverse-direction "negative ids must survive
/// tags-filter" is out of scope: `DEVIATIONS.md` "Negative input IDs
/// rejected project-wide" names renumber, diff/derive shard planners,
/// and getid as the enforcement sites; tags-filter is not on that
/// list.
#[test]
fn tags_filter_handles_mixed_sign_ids() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    use common::{TestRelation, TestWay};
    let nodes = generate_nodes_with_negatives(N_NEG, N_POS);
    let mut ways: Vec<TestWay> = generate_ways_with_negatives(N_NEG, N_POS, 3);
    for w in &mut ways {
        w.tags = vec![("highway", "primary")];
    }
    let relations: Vec<TestRelation> = generate_relations_with_negatives(N_NEG, N_POS, 2);
    write_test_pbf_sorted(&input, &nodes, &ways, &relations);

    let out = CliInvoker::new()
        .arg("tags-filter")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("w/highway=primary")
        .run();

    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("panicked at"),
        "tags-filter must not panic on mixed-sign ids; stderr:\n{stderr}",
    );
    // Reviewer pass 2 follow-up: the assertions used to be guarded
    // by `if out.status.success()`, which meant a future clean
    // rejection of mixed-sign input would skip the silent-drop
    // assertion entirely (and pass silently). Lock both shape AND
    // outcome: tags-filter currently succeeds + drops negative-id
    // ways. A regression that flips either side fails this test.
    assert!(
        out.status.success(),
        "tags-filter currently succeeds on mixed-sign input. If it \
         has been promoted to clean rejection per TODO.md 'Promote \
         silent passthrough/drop to clean error', invert this \
         assertion deliberately and update the TODO entry. \
         stderr:\n{stderr}",
    );
    let contents = read_all_elements_id_only(&output);
    let way_ids = way_ids_id_only(&contents);
    let neg_ways = way_ids.iter().filter(|id| **id < 0).count();
    assert_eq!(
        neg_ways, 0,
        "tags-filter currently drops negative-id ways through its \
         parallel-classify path. If this assertion fails because \
         tags-filter now preserves negative ids, that is a deliberate \
         change - update both this test and the TODO entry.",
    );
}

/// `renumber` MUST hard-reject negative input ids per the documented
/// contract in `DEVIATIONS.md`. The three named entry points
/// (`reframe_dense_with_new_ids`, `reframe_ways_with_new_ids`,
/// `rewrite_relations_with_new_ids`) all return an error naming the
/// offending id. With `N_NEG = 3, N_POS = 3` the fixture's first
/// negative node id is `-1`; the dense-node entry point fires first
/// and the error message must contain both the error class string
/// (`non-negative`) and the specific id (`-1`).
#[test]
fn renumber_rejects_mixed_sign_ids_with_named_id() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    build_fixture(&input);

    let out = CliInvoker::new()
        .arg("renumber")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .run();

    assert!(
        !out.status.success(),
        "renumber must reject mixed-sign input; stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("panicked at"),
        "renumber must not panic on mixed-sign ids; stderr:\n{stderr}",
    );
    assert!(
        stderr.contains("non-negative"),
        "renumber error must mention the non-negative requirement \
         (DEVIATIONS contract); stderr:\n{stderr}",
    );
    assert!(
        stderr.contains("-1"),
        "renumber error must name the offending id (DEVIATIONS contract); \
         stderr:\n{stderr}",
    );
}

/// `getid` MUST hard-reject negative input ids per `DEVIATIONS.md`
/// "Negative input IDs rejected project-wide". Mirrors osmium's getid
/// (`research/osmium-tool/src/id_file.cpp:31-37`, documented in
/// `research/osmium-tool/man/osmium-getid.md:62`: "This command will
/// not work with negative IDs."). The error must name the offending
/// id and its kind (node/way/relation) and contain the contract
/// substring "non-negative", same shape as renumber's reject.
#[test]
fn getid_rejects_negative_ids_with_named_id_and_kind() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    build_fixture(&input);

    let out = CliInvoker::new()
        .arg("getid")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        // `--` marks the end of options so `n-1`, `w-1` aren't
        // misparsed as short-flag arguments. (Without this clap
        // treats `n-1` as `-1` after stripping.)
        .arg("--")
        .arg("n-1")
        .arg("n-2")
        .arg("w-1")
        .run();

    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("panicked at"),
        "getid must not panic on negative-id queries; stderr:\n{stderr}",
    );
    assert!(
        !out.status.success(),
        "getid must reject negative-id queries (DEVIATIONS contract); \
         stdout:\n{}\nstderr:\n{stderr}",
        out.stdout_str(),
    );
    assert!(
        stderr.contains("non-negative"),
        "getid error must mention the non-negative requirement \
         (DEVIATIONS contract); stderr:\n{stderr}",
    );
    // First arg is `n-1`, so the parse-time reject fires on node id -1.
    assert!(
        stderr.contains("node id -1"),
        "getid error must name the offending id and its kind; \
         stderr:\n{stderr}",
    );
}

/// `getid --id-osm-file <pbf>` must NOT reject a source PBF that
/// contains negative ids. DEVIATIONS' "Negative input IDs rejected
/// project-wide" section explicitly carves out this path: ids
/// harvested from a separate PBF are treated as opaque payload, the
/// same stance as cat/sort/inspect (which preserve mixed-sign input
/// without validation). A future change that extends the
/// `parse_id_spec` parse-time reject into `parse_ids_from_pbf`
/// would break this test.
///
/// Source: `build_fixture` (mixed-sign 3 neg + 3 pos per kind).
/// Input: positives-only (`generate_*_with_negatives(0, N_POS)`).
/// Splitting the inputs this way keeps the test focused on the
/// source-PBF parse path. (`IdSet::any_in_range` now clamps negative
/// `min` to 0 so a straddling-zero indexdata range correctly screens
/// in its positive portion; see the doc comment on `any_in_range` and
/// the "Negative input IDs rejected project-wide" entry in
/// `DEVIATIONS.md`. The narrower input keeps this test orthogonal to
/// that fix even if the clamp ever changes.)
#[test]
fn getid_id_osm_file_passes_through_negative_source_ids() {
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("source.osm.pbf");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Source: mixed-sign fixture (would have triggered the parse-time
    // reject if the contract didn't carve out the PBF-source path).
    build_fixture(&source);

    // Input: positives only, so the input blob's indexdata range is
    // wholly positive and IdSet::any_in_range can match cleanly.
    let pos_nodes = generate_nodes_with_negatives(0, N_POS);
    let pos_ways = generate_ways_with_negatives(0, N_POS, 3);
    let pos_relations = generate_relations_with_negatives(0, N_POS, 2);
    write_test_pbf_sorted(&input, &pos_nodes, &pos_ways, &pos_relations);

    let out = CliInvoker::new()
        .arg("getid")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--id-osm-file")
        .arg(&source)
        .run();

    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("non-negative"),
        "getid --id-osm-file must NOT extend the parse-time reject \
         into the PBF-source path (DEVIATIONS contract). The source \
         PBF's negative ids should be treated as opaque payload. \
         stderr:\n{stderr}",
    );
    assert!(
        out.status.success(),
        "getid --id-osm-file must succeed when source PBF contains \
         negative ids; stdout:\n{}\nstderr:\n{stderr}",
        out.stdout_str(),
    );

    // The source's positive ids (1..=N_POS for each kind) survive the
    // IdSet population, match the input's positives, and pass through
    // to the output.
    let contents = read_all_elements_id_only(&output);
    let nodes = node_ids_id_only(&contents);
    let ways = way_ids_id_only(&contents);
    let relations = relation_ids_id_only(&contents);
    assert_eq!(
        nodes.len(),
        N_POS,
        "all positive nodes survive; ids: {nodes:?}"
    );
    assert_eq!(
        ways.len(),
        N_POS,
        "all positive ways survive; ids: {ways:?}"
    );
    assert_eq!(
        relations.len(),
        N_POS,
        "all positive relations survive; ids: {relations:?}",
    );
}
