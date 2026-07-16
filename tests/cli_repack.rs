//! CLI-driven integration tests for `pbfhogg repack`.
//!
//! Fixture PBFs are written with the stable-allowlist writer helpers; the
//! repack command runs via the compiled `pbfhogg` binary through
//! `CliInvoker`; output is verified by reading the resulting PBF with the
//! stable-allowlist reader helpers. No imports from
//! `pbfhogg::commands::repack` or any other internal module - a rewrite of
//! `src/commands/repack/` cannot break these tests by type changes alone.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;

use common::cli::CliInvoker;
use common::{
    TestNode, TestRelation, TestWay, generate_nodes, generate_relations, generate_ways,
    read_header, read_normalized, write_multi_block_test_pbf,
};
use pbfhogg::{BlobDecode, BlobReader, Element};

/// Invoke `pbfhogg repack --elements-per-blob N -o <output> <input>`.
fn run_repack(input: &Path, output: &Path, elements_per_blob: usize) -> common::cli::CliOutput {
    CliInvoker::new()
        .arg("repack")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--elements-per-blob")
        .arg(elements_per_blob.to_string())
        .assert_success()
}

/// Count elements of each kind, per blob, in the output PBF.
///
/// Returns a vec of `(node_count, way_count, relation_count)` tuples in
/// blob order. Used to verify the per-blob element cap is respected and
/// that each blob is single-kind (the PBF interop convention).
fn elements_per_blob(path: &Path) -> Vec<(usize, usize, usize)> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut per_blob = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            let mut nodes = 0;
            let mut ways = 0;
            let mut rels = 0;
            for element in block.elements() {
                match element {
                    Element::Node(_) | Element::DenseNode(_) => nodes += 1,
                    Element::Way(_) => ways += 1,
                    Element::Relation(_) => rels += 1,
                    _ => {}
                }
            }
            per_blob.push((nodes, ways, rels));
        }
    }
    per_blob
}

/// Build a sorted, multi-blob fixture: 60 nodes, 12 ways, 3 relations,
/// packed at 20 elements/blob. Yields 3 node blobs + 1 way blob + 1
/// relation blob = 5 OsmData blobs in the input.
///
/// Sizes chosen so the multi-blob kind (nodes) has an input blob size
/// (20) that is an exact multiple of the shrink-test cap (10). Exact
/// division means each input blob's `M % cap` tail is zero, so the
/// merge-thread ordering guard (drain-before-direct-write) never fires
/// and never splits a tail into its own under-cap block. That is what
/// keeps the per-kind output blob count equal to `ceil(elements / cap)`
/// for this fixture (see `repack_blob_count_matches_prediction`). The
/// ceil identity is NOT general: on a shrink where the cap does not
/// divide the per-input-blob element count, the guard flushes each input
/// blob's tail as its own possibly-under-cap block, so the output blob
/// count depends on the input-blob boundaries and can exceed
/// `ceil(elements / cap)`. Ways (12) and relations (3) each occupy a
/// single input blob, so even their non-dividing tails cannot cross an
/// input-blob boundary and the ceil count still holds for them here.
fn write_repack_fixture(path: &Path) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let mut nodes = generate_nodes(60, 1);
    nodes[0].tags = vec![("place", "city"), ("name", "Origo")];
    nodes[7].tags = vec![("amenity", "cafe")];
    nodes[42].tags = vec![("highway", "bus_stop")];
    let ways = generate_ways(12, 1, 3, 1);
    let rels = generate_relations(3, 1, 2, 1);
    write_multi_block_test_pbf(path, &nodes, &ways, &rels, 20);
    (nodes, ways, rels)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Round-trip preserves element count, IDs, and tag multiset across a
/// shrink (cap=10 vs input 25/blob).
#[test]
fn repack_round_trip_preserves_elements_on_shrink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    let (nodes, ways, rels) = write_repack_fixture(&input);
    run_repack(&input, &output, 10);

    let original = read_normalized(&input);
    let repacked = read_normalized(&output);

    assert_eq!(repacked.nodes.len(), nodes.len());
    assert_eq!(repacked.ways.len(), ways.len());
    assert_eq!(repacked.relations.len(), rels.len());
    assert_eq!(original.nodes, repacked.nodes);
    assert_eq!(original.ways, repacked.ways);
    assert_eq!(original.relations, repacked.relations);
}

/// Every output blob must hold no more than the configured cap, and each
/// blob must be single-kind (PBF interop convention).
#[test]
fn repack_respects_element_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    run_repack(&input, &output, 10);

    let per_blob = elements_per_blob(&output);
    assert!(!per_blob.is_empty(), "output has no OsmData blobs");
    for (i, (n, w, r)) in per_blob.iter().enumerate() {
        let total = n + w + r;
        assert!(total <= 10, "blob {i}: {total} elements exceeds cap 10");
        let kinds_present = u8::from(*n > 0) + u8::from(*w > 0) + u8::from(*r > 0);
        assert_eq!(kinds_present, 1, "blob {i} is mixed-kind: ({n}, {w}, {r})");
    }
}

/// Output blob count tracks the cap-prediction for each kind on shrinks.
/// 60 nodes / cap 10 = 6 node blobs. 12 ways / cap 10 = 2 blobs (1 full +
/// 1 partial of 2). 3 relations / cap 10 = 1 partial.
///
/// This test still passes under the merge-thread ordering guard only
/// because the fixture divides evenly: the multi-blob kind (nodes) has
/// input blobs of 20 and cap 10 divides 20, so every input blob's tail is
/// empty and the guard never fires - there is no per-input-blob
/// fragmentation to inflate the count. Ways and relations each fit in a
/// single input blob, so their (non-dividing) tails also cannot cross an
/// input-blob boundary. The `ceil(elements / cap)` identity therefore
/// holds here but is NOT general: whenever the cap fails to divide the
/// per-input-blob element count on a multi-blob shrink, the guard emits
/// each input blob's tail as its own under-cap block and the output blob
/// count grows with the number of input blobs (accepted trade: the
/// ordering guard that keeps output monotonic across coalesced blob
/// boundaries flushes the central builder before each input blob's
/// direct full blocks, which is what fragments non-dividing tails).
#[test]
fn repack_blob_count_matches_prediction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    run_repack(&input, &output, 10);

    let per_blob = elements_per_blob(&output);
    let node_blobs = per_blob.iter().filter(|(n, _, _)| *n > 0).count();
    let way_blobs = per_blob.iter().filter(|(_, w, _)| *w > 0).count();
    let rel_blobs = per_blob.iter().filter(|(_, _, r)| *r > 0).count();

    assert_eq!(node_blobs, 6, "expected 6 node blobs (60 nodes / cap 10)");
    assert_eq!(
        way_blobs, 2,
        "expected 2 way blobs (12 ways: 10 + 2 partial)"
    );
    assert_eq!(
        rel_blobs, 1,
        "expected 1 relation blob (3 relations partial)"
    );
}

/// Sort.Type_then_ID propagates from input to output. The input fixture
/// is built via `write_multi_block_test_pbf` which sets the sorted flag.
#[test]
fn repack_propagates_sorted_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    assert!(read_header(&input).is_sorted(), "fixture must be sorted");
    run_repack(&input, &output, 10);
    assert!(
        read_header(&output).is_sorted(),
        "output dropped Sort.Type_then_ID"
    );
}

/// `--elements-per-blob 0` is rejected up front.
#[test]
fn repack_rejects_zero_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    CliInvoker::new()
        .arg("repack")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--elements-per-blob")
        .arg("0")
        .assert_failure()
        .assert_stderr_contains("must be > 0");
}

/// When the cap exceeds every kind's total element count, the central
/// builder never flushes mid-stream and each kind emits exactly one
/// output blob (genuine identity, not a per-input-blob silent
/// passthrough). The CLI emits the "never fired" warning so users running
/// `--elements-per-blob 64000` against a small input know the cap had no
/// effect.
#[test]
fn repack_grow_collapses_to_one_blob_per_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    // Cap >> per-kind totals (60 nodes, 12 ways, 3 rels): no mid-stream
    // flush, one output blob per kind.
    let out = run_repack(&input, &output, 8000);
    out.assert_stderr_contains("--elements-per-blob 8000 never fired");

    let original = read_normalized(&input);
    let repacked = read_normalized(&output);
    assert_eq!(original.nodes, repacked.nodes);
    assert_eq!(original.ways, repacked.ways);
    assert_eq!(original.relations, repacked.relations);

    let per_blob = elements_per_blob(&output);
    let node_blobs = per_blob.iter().filter(|(n, _, _)| *n > 0).count();
    let way_blobs = per_blob.iter().filter(|(_, w, _)| *w > 0).count();
    let rel_blobs = per_blob.iter().filter(|(_, _, r)| *r > 0).count();
    assert_eq!(
        node_blobs, 1,
        "expected 60 nodes to collapse to 1 blob (cap 8000)"
    );
    assert_eq!(
        way_blobs, 1,
        "expected 12 ways to collapse to 1 blob (cap 8000)"
    );
    assert_eq!(
        rel_blobs, 1,
        "expected 3 relations to collapse to 1 blob (cap 8000)"
    );
}

/// Round-trip preserves element multiset on a grow that also fires the
/// cap mid-stream: cap=30 vs input 20/blob means the central node
/// builder spans across input-blob boundaries (input blob 1's 20 nodes
/// plus 10 of input blob 2 = first output blob; remaining 10 of blob 2
/// plus all 20 of blob 3 = second output blob).
#[test]
fn repack_round_trip_preserves_elements_on_grow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    let (nodes, ways, rels) = write_repack_fixture(&input);
    run_repack(&input, &output, 30);

    let original = read_normalized(&input);
    let repacked = read_normalized(&output);

    assert_eq!(repacked.nodes.len(), nodes.len());
    assert_eq!(repacked.ways.len(), ways.len());
    assert_eq!(repacked.relations.len(), rels.len());
    assert_eq!(original.nodes, repacked.nodes);
    assert_eq!(original.ways, repacked.ways);
    assert_eq!(original.relations, repacked.relations);
}

/// Grow output blob count matches `ceil(elements / cap)`. cap=30: 60
/// nodes -> 2 blobs (cross-input-blob coalesce), 12 ways -> 1 partial,
/// 3 relations -> 1 partial.
#[test]
fn repack_grow_blob_count_matches_prediction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    run_repack(&input, &output, 30);

    let per_blob = elements_per_blob(&output);
    let node_blobs = per_blob.iter().filter(|(n, _, _)| *n > 0).count();
    let way_blobs = per_blob.iter().filter(|(_, w, _)| *w > 0).count();
    let rel_blobs = per_blob.iter().filter(|(_, _, r)| *r > 0).count();
    assert_eq!(node_blobs, 2, "expected 2 node blobs (60 / cap 30)");
    assert_eq!(way_blobs, 1, "expected 1 way blob (12 < cap 30)");
    assert_eq!(rel_blobs, 1, "expected 1 relation blob (3 < cap 30)");
}

/// On a grow that fires the cap mid-stream (cap=30 vs input 20/blob,
/// 60 total nodes) the no-op-cap warning must NOT appear: at least one
/// kind's central builder flushes mid-stream, which suppresses the
/// global warning.
#[test]
fn repack_grow_no_warning_when_cap_fires() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    let out = run_repack(&input, &output, 30);
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("never fired"),
        "no-op-cap warning should not fire when the cap fires mid-stream; stderr was:\n{stderr}"
    );
}

/// On a shrink (cap=10 vs input 20/blob) the cap fires on every input
/// blob, so the no-op-cap warning must NOT appear. Regression sentinel
/// against false positives on the cap-fires path.
#[test]
fn repack_no_warning_when_cap_fires() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    let out = run_repack(&input, &output, 10);
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("never fired"),
        "no-op-cap warning should not fire on a real shrink; stderr was:\n{stderr}"
    );
}

/// Collect element IDs of one kind in output order: blob by blob (as
/// written), and within each blob in stored element order. This preserves
/// the physical order a streaming consumer sees, so a monotonicity check
/// over it detects the Sort.Type_then_ID lie that a reordered output
/// commits. `read_normalized` deliberately canonicalizes order and so
/// cannot see the violation; this raw walk can.
fn relation_ids_in_output_order(path: &Path) -> Vec<i64> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut ids = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                if let Element::Relation(r) = element {
                    ids.push(r.id());
                }
            }
        }
    }
    ids
}

/// Regression for the cross-blob non-monotonicity bug (TODO, 2026-07-12):
/// repack wrote worker "full" blocks directly in seq order while the
/// central builder's coalesced tail-blocks were emitted through a delayed
/// buffer, so an earlier input blob's low-ID tail could land AFTER a later
/// blob's higher-ID full block - producing output that violates the
/// Sort.Type_then_ID ordering its own header still claims.
///
/// The repro needs relation input blobs LARGER than the target cap so the
/// worker splits each blob into a direct full block (low IDs) plus a
/// trailing tail (high IDs) that the central builder must carry across the
/// input-blob boundary. Fixture: 40 relations, IDs 1..=40, packed 20 per
/// input blob (2 relation blobs). Cap 12: each input blob yields one full
/// block of 12 plus a tail of 8.
///
/// Before the fix, blob A's direct full block [1..=12] and blob B's direct
/// full block [21..=32] were written first, and the coalesced tails
/// ([13..=20] merged with [33..=36], then [37..=40]) were flushed only at
/// the end - so ID 13 followed ID 32 in the output stream. After the fix
/// the drain guard flushes blob A's tail before blob B's full block, so
/// the output is globally [1..=12], [13..=20], [21..=32], [33..=40] and
/// strictly increasing. The element multiset is preserved either way, so
/// this ordering assertion is what pins the bug.
#[test]
fn repack_output_is_monotonic_across_coalesced_blob_boundaries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    // A few nodes/ways keep the fixture a well-formed multi-kind PBF; the
    // 40 relations across two 20-element input blobs are what force the
    // cross-boundary coalescing with interleaved full blocks.
    let nodes = generate_nodes(5, 1);
    let ways = generate_ways(5, 1, 3, 1);
    let rels = generate_relations(40, 1, 2, 1);
    write_multi_block_test_pbf(&input, &nodes, &ways, &rels, 20);
    assert!(read_header(&input).is_sorted(), "fixture must be sorted");

    // Cap 12 < input relation blob size 20: every relation input blob
    // splits into a direct full block plus a coalescing tail.
    run_repack(&input, &output, 12);

    // Header still claims Sort.Type_then_ID; the body must not lie about it.
    assert!(
        read_header(&output).is_sorted(),
        "output dropped Sort.Type_then_ID"
    );

    // Element multiset is preserved (guards against the ordering fix
    // dropping or duplicating relations).
    let original = read_normalized(&input);
    let repacked = read_normalized(&output);
    assert_eq!(original.relations, repacked.relations);

    let ids = relation_ids_in_output_order(&output);
    assert_eq!(ids.len(), 40, "expected all 40 relations in the output");
    for pair in ids.windows(2) {
        assert!(
            pair[1] > pair[0],
            "relation IDs are non-monotonic in output order: {} follows {} \
             (output violates the Sort.Type_then_ID it claims). full ID \
             sequence: {ids:?}",
            pair[1],
            pair[0],
        );
    }
}

/// Collect node, way, and relation IDs in physical output order (blob by
/// blob, element order within blob) as three separate vecs. Generalizes
/// `relation_ids_in_output_order` to the other two kinds so the
/// cross-kind monotonicity tests can walk the raw stream a consumer sees;
/// `read_normalized` canonicalizes order and cannot detect the ordering
/// lie these tests target.
fn ids_in_output_order(path: &Path) -> (Vec<i64>, Vec<i64>, Vec<i64>) {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut nodes = Vec::new();
    let mut ways = Vec::new();
    let mut rels = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::Node(n) => nodes.push(n.id()),
                    Element::DenseNode(dn) => nodes.push(dn.id()),
                    Element::Way(w) => ways.push(w.id()),
                    Element::Relation(r) => rels.push(r.id()),
                    _ => {}
                }
            }
        }
    }
    (nodes, ways, rels)
}

/// Assert `ids` is strictly increasing, panicking with the offending pair
/// and the full sequence on the first inversion. `kind` names the element
/// type for the message.
fn assert_strictly_increasing(ids: &[i64], kind: &str) {
    for pair in ids.windows(2) {
        assert!(
            pair[1] > pair[0],
            "{kind} IDs are non-monotonic in output order: {} follows {} \
             (output violates the Sort.Type_then_ID it claims). full ID \
             sequence: {ids:?}",
            pair[1],
            pair[0],
        );
    }
}

/// Cross-kind coverage for the ordering fix (companion to the relation-only
/// `repack_output_is_monotonic_across_coalesced_blob_boundaries`). Nodes
/// (dense) and ways run through the same two-stream merge path - worker full
/// blocks written directly, coalesced tails routed through the central
/// builder - so they were equally exposed to the pre-fix reordering.
///
/// Fixture: 40 nodes and 40 ways, each laid out as two 20-element input
/// blobs, cap 12. Every input blob splits into a direct full block of 12
/// (low IDs) plus an 8-element coalescing tail (high IDs), so blob B's
/// direct full block would land before blob A's tail without the drain
/// guard - non-monotonic for both kinds. The relations are incidental (one
/// small blob) and only keep the fixture a well-formed multi-kind PBF.
#[test]
fn repack_output_is_monotonic_for_nodes_and_ways() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    let nodes = generate_nodes(40, 1);
    let ways = generate_ways(40, 1, 3, 1);
    let rels = generate_relations(3, 1, 2, 1);
    write_multi_block_test_pbf(&input, &nodes, &ways, &rels, 20);
    assert!(read_header(&input).is_sorted(), "fixture must be sorted");

    run_repack(&input, &output, 12);

    assert!(
        read_header(&output).is_sorted(),
        "output dropped Sort.Type_then_ID"
    );

    // Multiset preserved (guards against the ordering path dropping or
    // duplicating elements of either kind).
    let original = read_normalized(&input);
    let repacked = read_normalized(&output);
    assert_eq!(original.nodes, repacked.nodes);
    assert_eq!(original.ways, repacked.ways);

    let (node_ids, way_ids, _) = ids_in_output_order(&output);
    assert_eq!(node_ids.len(), 40, "expected all 40 nodes in the output");
    assert_eq!(way_ids.len(), 40, "expected all 40 ways in the output");
    assert_strictly_increasing(&node_ids, "node");
    assert_strictly_increasing(&way_ids, "way");
}

/// Write a sorted, indexed multi-blob PBF whose relation section is split
/// into consecutive input blobs of the given sizes (in order); nodes and
/// ways each occupy a single blob. `write_multi_block_test_pbf` applies one
/// uniform block size to every kind and so cannot place several small
/// all-tail relation blobs ahead of a larger full-block-bearing one - the
/// layout the pending-prepopulated guard test needs. Sync-writer blobs
/// carry indexdata (`write_primitive_block` scans block IDs), so repack's
/// indexdata requirement is satisfied. Allowlist-only (block_builder +
/// writer).
fn write_pbf_with_relation_blob_sizes(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
    rel_blob_sizes: &[usize],
) {
    use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
    use pbfhogg::writer::{Compression, PbfWriter};

    let total: usize = rel_blob_sizes.iter().sum();
    assert_eq!(
        total,
        relations.len(),
        "rel_blob_sizes must sum to the relation count"
    );

    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    // Nodes: one blob.
    for n in nodes {
        bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Ways: one blob.
    for w in ways {
        bb.add_way(w.id, w.tags.iter().copied(), &w.refs, None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Relations: one blob per requested size, in order.
    let mut idx = 0usize;
    for &sz in rel_blob_sizes {
        for r in &relations[idx..idx + sz] {
            let members: Vec<MemberData<'_>> = r
                .members
                .iter()
                .map(|m| MemberData {
                    id: m.id,
                    role: m.role,
                })
                .collect();
            bb.add_relation(r.id, r.tags.iter().copied(), &members, None);
        }
        idx += sz;
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(bytes).expect("write block");
        }
    }

    writer.flush().expect("flush");
}

/// Pins the ordering guard firing while the central-builder `pending`
/// buffer is ALREADY non-empty at guard entry - the path the relation-only
/// regression test never reaches (there the guard always enters with
/// `pending` empty and fires solely because `bb` holds the prior blob's
/// tail).
///
/// Layout: relation input blobs of sizes 5, 5, 5, 15 (30 relations, IDs
/// 1..=30), cap 12. The three 5-element blobs are all-tail (5 < 12), so
/// they feed the central builder without producing any direct full block
/// or firing the guard. Their cumulative 15 relations overflow the cap
/// once: the builder flushes a completed block (IDs 1..=12) into `pending`
/// and retains IDs 13..=15. `pending` is now non-empty and stays that way
/// (well under the FRAME_BATCH=32 drain threshold) when the fourth blob
/// arrives. That blob (15 relations, IDs 16..=30) carries a direct full
/// block of 12, so the guard fires with `pending` already holding the
/// coalesced 1..=12 block - and must drain it (plus the retained 13..=15
/// tail) before the direct 16..=27 write to stay ordered.
///
/// Before the fix the direct block 16..=27 was written first and the
/// buffered low-ID blocks were flushed only at phase end, so ID 16
/// preceded ID 1 in the output - non-monotonic. After the fix the guard
/// drains 1..=12 and 13..=15 first, yielding 1..=30 in order.
///
/// Note: the guard's `!pending.is_empty()` disjunct here co-occurs with a
/// non-empty `bb` (IDs 13..=15), so `bb` short-circuits the OR. The strict
/// `bb`-empty-`pending`-non-empty state is not reachable: the block builder
/// flushes lazily (only on overflow before an add, or an explicit flush),
/// so every tail-consume that populates `pending` leaves `bb` holding a
/// non-empty remainder, and the only thing that empties `bb` mid-stream is
/// the guard itself, which then drains all of `pending`. The disjunct is
/// therefore defensive; this test exercises the reachable pending-non-empty
/// entry, which the existing regression does not.
#[test]
fn repack_output_is_monotonic_with_pending_prepopulated_at_guard() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    let nodes = generate_nodes(5, 1);
    let ways = generate_ways(5, 1, 3, 1);
    let rels = generate_relations(30, 1, 2, 1);
    // Three small all-tail relation blobs (5 each) accumulate a completed
    // block into `pending`, then a 15-element blob fires the guard.
    write_pbf_with_relation_blob_sizes(&input, &nodes, &ways, &rels, &[5, 5, 5, 15]);
    assert!(read_header(&input).is_sorted(), "fixture must be sorted");

    run_repack(&input, &output, 12);

    assert!(
        read_header(&output).is_sorted(),
        "output dropped Sort.Type_then_ID"
    );

    let original = read_normalized(&input);
    let repacked = read_normalized(&output);
    assert_eq!(original.relations, repacked.relations);

    let (_, _, rel_ids) = ids_in_output_order(&output);
    assert_eq!(rel_ids.len(), 30, "expected all 30 relations in the output");
    assert_strictly_increasing(&rel_ids, "relation");
}

// ---------------------------------------------------------------------------
// LocationsOnWays preservation (v2.2)
// ---------------------------------------------------------------------------

/// A way with an id, node refs, and one inline `(decimicro_lat, decimicro_lon)`
/// coordinate per ref. The stable `write_multi_block_test_pbf` helper cannot
/// emit LocationsOnWays, so these tests build the fixture directly with the
/// allowlisted `BlockBuilder`/`PbfWriter` surface.
type LowWay = (
    i64,
    Vec<i64>,
    Vec<(i32, i32)>,
    Vec<(&'static str, &'static str)>,
);

/// Write a sorted, indexed PBF whose header declares `LocationsOnWays` and
/// whose ways carry inline node coordinates (via
/// `BlockBuilder::add_way_with_locations`). Nodes occupy one blob, ways a
/// second. Sync-writer blobs carry indexdata (`write_primitive_block` scans
/// block IDs), so repack's indexdata requirement is satisfied without
/// `--force`. Allowlist-only (block_builder + writer).
fn write_low_pbf(path: &Path, nodes: &[TestNode], ways: &[LowWay]) {
    write_way_coords_pbf(path, nodes, ways, true);
}

/// Like [`write_low_pbf`], but the ways always carry inline
/// `(decimicro_lat, decimicro_lon)` coordinates via
/// `BlockBuilder::add_way_with_locations` regardless of `declare_low`; only
/// whether the header advertises the `LocationsOnWays` feature is
/// parameterized. Used to pin the reverse no-implicit-conversion direction:
/// coordinates present in the wire data but the feature flag absent from the
/// header must not cause repack to preserve (or otherwise expose) them.
fn write_way_coords_pbf(path: &Path, nodes: &[TestNode], ways: &[LowWay], declare_low: bool) {
    use pbfhogg::block_builder::{self, BlockBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};

    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let mut header_builder = block_builder::HeaderBuilder::new().sorted();
    if declare_low {
        header_builder = header_builder.optional_feature("LocationsOnWays");
    }
    let header = header_builder.build().expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    // Nodes: one blob.
    for n in nodes {
        bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Ways: one blob, each carrying inline coordinates.
    for (id, refs, locs, tags) in ways {
        assert_eq!(refs.len(), locs.len(), "refs and locations must match");
        bb.add_way_with_locations(*id, tags.iter().copied(), refs, locs, None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

/// Collect `(way_id, inline coordinates)` for every way in the output, in
/// physical order. Coordinates come from `Way::node_locations()`, which is
/// empty unless the way actually embeds LocationsOnWays data.
fn way_locations_in_output(path: &Path) -> Vec<(i64, Vec<(i32, i32)>)> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut out = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                if let Element::Way(w) = element {
                    let locs: Vec<(i32, i32)> = w
                        .node_locations()
                        .map(|l| (l.decimicro_lat(), l.decimicro_lon()))
                        .collect();
                    out.push((w.id(), locs));
                }
            }
        }
    }
    out
}

/// When the input header declares `LocationsOnWays`, repack must re-advertise
/// the feature AND round-trip every way-ref coordinate exactly. Cap 2 against
/// 5 ways in one input blob forces the split: the worker frames two full
/// blocks (4 ways) and ships one way as the trailing merge slice, so both the
/// per-worker `add_way_with_locations` path and the central-builder
/// (Owned-way) path carry coordinates.
#[test]
fn repack_preserves_locations_on_ways() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    let nodes = generate_nodes(6, 1);
    let ways: Vec<LowWay> = (1..=5i64)
        .map(|w| {
            let refs: Vec<i64> = (0..3i64).map(|r| w * 10 + r).collect();
            let locs: Vec<(i32, i32)> = (0..3i64)
                .map(|r| {
                    let base = i32::try_from(w * 1_000_000 + r * 1000).expect("coord fits in i32");
                    (base, base + 7)
                })
                .collect();
            (w, refs, locs, vec![("highway", "residential")])
        })
        .collect();
    write_low_pbf(&input, &nodes, &ways);

    assert!(
        read_header(&input).has_locations_on_ways(),
        "fixture must declare LocationsOnWays"
    );

    // Cap 2 < 5 ways/blob: worker full blocks (4 ways) plus a 1-way tail.
    run_repack(&input, &output, 2);

    assert!(
        read_header(&output).has_locations_on_ways(),
        "output dropped LocationsOnWays"
    );

    let mut got = way_locations_in_output(&output);
    got.sort_by_key(|(id, _)| *id);
    let mut expected: Vec<(i64, Vec<(i32, i32)>)> = ways
        .iter()
        .map(|(id, _, locs, _)| (*id, locs.clone()))
        .collect();
    expected.sort_by_key(|(id, _)| *id);

    assert_eq!(
        got, expected,
        "way-ref coordinates did not round-trip exactly"
    );
}

/// Sentinel for the no-implicit-conversion constraint: when the input header
/// lacks `LocationsOnWays`, repack must NOT emit the feature and must NOT
/// synthesize inline way coordinates. The standard fixture has no LOW.
#[test]
fn repack_strips_no_low_when_input_has_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    assert!(
        !read_header(&input).has_locations_on_ways(),
        "fixture must not declare LocationsOnWays"
    );

    run_repack(&input, &output, 10);

    assert!(
        !read_header(&output).has_locations_on_ways(),
        "output emitted LocationsOnWays when the input lacked it"
    );

    let got = way_locations_in_output(&output);
    assert!(!got.is_empty(), "expected ways in the output");
    for (id, locs) in &got {
        assert!(
            locs.is_empty(),
            "way {id} unexpectedly carries inline coordinates"
        );
    }
}

/// Reverse of the constraint above: the input header omits the
/// `LocationsOnWays` feature flag, but the way blob still carries inline
/// `(decimicro_lat, decimicro_lon)` coordinates in the wire data (a
/// malformed-but-decodable input, or one hand-built to probe repack's
/// gating). `repack_strips_no_low_when_input_has_none` cannot catch
/// accidental preservation because its fixture has no coordinate fields at
/// all; this test puts real coordinates in the input and pins that repack's
/// gate on `header.has_locations_on_ways()` - not on whether the way payload
/// happens to carry locations - controls the output.
#[test]
fn repack_strips_coords_when_input_flag_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    let nodes = generate_nodes(6, 1);
    let ways: Vec<LowWay> = (1..=5i64)
        .map(|w| {
            let refs: Vec<i64> = (0..3i64).map(|r| w * 10 + r).collect();
            let locs: Vec<(i32, i32)> = (0..3i64)
                .map(|r| {
                    let base = i32::try_from(w * 1_000_000 + r * 1000).expect("coord fits in i32");
                    (base, base + 7)
                })
                .collect();
            (w, refs, locs, vec![("highway", "residential")])
        })
        .collect();
    write_way_coords_pbf(&input, &nodes, &ways, false);

    assert!(
        !read_header(&input).has_locations_on_ways(),
        "fixture must not declare LocationsOnWays"
    );

    // Cap 2 < 5 ways/blob: exercises both the worker full-block path and the
    // trailing Owned-way merge path, same split as
    // `repack_preserves_locations_on_ways`.
    run_repack(&input, &output, 2);

    assert!(
        !read_header(&output).has_locations_on_ways(),
        "output emitted LocationsOnWays when the input lacked the flag"
    );

    let got = way_locations_in_output(&output);
    assert_eq!(got.len(), ways.len(), "expected all ways in the output");
    for (id, locs) in &got {
        assert!(
            locs.is_empty(),
            "way {id} exposed inline coordinates despite absent LocationsOnWays flag"
        );
    }
}
