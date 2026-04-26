//! Property-based tests for parser entry points and roundtrip
//! invariants.
//!
//! T07 in `notes/testing.md`. Same class of bugs as cargo-fuzz (parse
//! crashes, boundary violations, roundtrip asymmetries) but runs inside
//! `cargo test` in seconds with deterministic shrinking on failure. No
//! corpus directory committed (`proptest-regressions/` is gitignored).
//!
//! Properties pinned:
//!
//! - `PrimitiveBlock::new(arbitrary)` returns Ok or Err, never panics.
//! - `BlobReader::new(Cursor::new(arbitrary))` walks without panic.
//! - Truncating a known-good fixture at any byte offset never panics
//!   the reader.
//! - Generated node fixtures roundtrip through write -> read with
//!   id-set preserved.
//!
//! Test count is held to 64 cases per property (default proptest is
//! 256) so the file stays under a tier-1 wall-clock budget; the
//! property-coverage benefit comes from 64 × 4 = 256 distinct shapes
//! per `brokkr check` invocation.

#[path = "common/mod.rs"]
mod common;

use std::io::Cursor;

use std::collections::BTreeMap;

use bytes::Bytes;
use common::{
    generate_nodes, generate_nodes_with_negatives, generate_relations, generate_ways,
    read_normalized, write_test_pbf_sorted,
};
use pbfhogg::{BlobReader, PrimitiveBlock};
use proptest::prelude::*;
use tempfile::TempDir;

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    /// `PrimitiveBlock::new` must accept any byte buffer and return Ok
    /// or Err. Adversarial input must never trigger a panic, OOM, or
    /// stack overflow during the wire-format walk inside
    /// `WireBlock::parse_and_inline`.
    #[test]
    fn primitive_block_from_arbitrary_bytes_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        match PrimitiveBlock::new(Bytes::from(bytes)) {
            Ok(_) | Err(_) => {}
        }
    }

    /// `BlobReader::new` walks a byte stream and yields blobs. On
    /// arbitrary input it must produce a finite sequence of Ok / Err
    /// items (capped at 32 by this test) without panic.
    #[test]
    fn blob_reader_arbitrary_bytes_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..4096),
    ) {
        let mut reader = BlobReader::new(Cursor::new(bytes));
        for _ in 0..32 {
            match reader.next() {
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
                None => break,
            }
        }
    }

    /// Truncating a known-good PBF at any byte offset must never panic
    /// the reader. The fixture is small (~1 KB), so the truncation can
    /// land mid-frame, mid-blob-header, or mid-payload - every variant
    /// has to surface as Err, not panic.
    #[test]
    fn blob_reader_truncated_fixture_never_panics(seed in 0u64..1024) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("fixture.osm.pbf");
        let nodes = generate_nodes(20, 1);
        write_test_pbf_sorted(&path, &nodes, &[], &[]);
        let pbf = std::fs::read(&path).expect("read fixture");
        let len = if pbf.is_empty() {
            0
        } else {
            usize::try_from(seed).unwrap_or(0) % pbf.len()
        };
        let mut reader = BlobReader::new(Cursor::new(&pbf[..len]));
        for _ in 0..32 {
            match reader.next() {
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
                None => break,
            }
        }
    }

    /// Round-trip: write a generated node fixture, read it back, and
    /// assert ids AND coordinates AND tags survive the cycle.
    ///
    /// Tier A11 follow-up: previously this property compared id sets
    /// only. A coordinate-corruption regression in the writer or
    /// reader would have slipped past. The fixture's nodes have
    /// deterministic `(lat, lon)` derived from `id`, so we can
    /// recompute the expected coordinates from the original
    /// `TestNode` vector and compare.
    ///
    /// `start` is positive-only by design. Per `DEVIATIONS.md`
    /// ("Negative input IDs rejected project-wide") the *command*
    /// pipelines reject negative ids, so a proptest covering both
    /// signs would conflate the library-level read/write contract
    /// with the command-level invariant. Mixed-sign roundtrips are
    /// pinned separately by `negative_id_node_fixture_roundtrips`
    /// below; widening this property's range without thinking
    /// through that layering would re-merge the two contracts.
    #[test]
    fn node_fixture_roundtrips(
        count in 1usize..50,
        start in 1i64..1_000_000,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("fixture.osm.pbf");
        let input_nodes = generate_nodes(count, start);
        write_test_pbf_sorted(&path, &input_nodes, &[], &[]);

        let n = read_normalized(&path);
        prop_assert_eq!(n.nodes.len(), count);

        // Build a lookup by id so we don't depend on internal sort
        // order matching input vector order.
        let by_id: BTreeMap<i64, &common::NormalizedNode> =
            n.nodes.iter().map(|x| (x.id, x)).collect();
        for input in &input_nodes {
            let output = by_id
                .get(&input.id)
                .expect("input node must appear in output");
            prop_assert_eq!(output.lat, input.lat);
            prop_assert_eq!(output.lon, input.lon);
            // Tag-set equality. `generate_nodes` produces empty tags,
            // so this is trivially equal today, but pinning it here
            // catches a future generator change that adds tags
            // without updating this assertion.
            let expected: BTreeMap<String, String> = input
                .tags
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            prop_assert_eq!(&output.tags, &expected);
        }
    }

    /// Way fixture roundtrip - pins `BlockBuilder::add_way` and the
    /// `Way` parser by checking ids and refs survive intact. Tier A12
    /// follow-up; the previous batch covered nodes only.
    #[test]
    fn way_fixture_roundtrips(
        node_count in 5usize..30,
        way_count in 1usize..20,
        refs_per_way in 2usize..5,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("fixture.osm.pbf");
        let nodes = generate_nodes(node_count, 1);
        let ways = generate_ways(way_count, 1_000, refs_per_way, 1);
        write_test_pbf_sorted(&path, &nodes, &ways, &[]);

        let n = read_normalized(&path);
        prop_assert_eq!(n.ways.len(), way_count);

        let by_id: BTreeMap<i64, &common::NormalizedWay> =
            n.ways.iter().map(|x| (x.id, x)).collect();
        for input in &ways {
            let output = by_id
                .get(&input.id)
                .expect("input way must appear in output");
            prop_assert_eq!(&output.refs, &input.refs);
        }
    }

    /// Relation fixture roundtrip - pins `BlockBuilder::add_relation`
    /// and the `Relation` parser by checking ids and member shapes
    /// survive intact. Tier A12 follow-up.
    #[test]
    fn relation_fixture_roundtrips(
        node_count in 5usize..20,
        way_count in 2usize..10,
        rel_count in 1usize..10,
        members_per_rel in 1usize..4,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("fixture.osm.pbf");
        let nodes = generate_nodes(node_count, 1);
        let ways = generate_ways(way_count, 1_000, 2, 1);
        let relations = generate_relations(rel_count, 10_000, members_per_rel, 1_000);
        write_test_pbf_sorted(&path, &nodes, &ways, &relations);

        let n = read_normalized(&path);
        prop_assert_eq!(n.relations.len(), rel_count);

        let by_id: BTreeMap<i64, &common::NormalizedRelation> =
            n.relations.iter().map(|x| (x.id, x)).collect();
        for input in &relations {
            let output = by_id
                .get(&input.id)
                .expect("input relation must appear in output");
            prop_assert_eq!(output.members.len(), input.members.len());
        }
    }

    /// Library-level mixed-sign roundtrip: `BlockBuilder` /
    /// `PbfWriter` accept negative ids on input, the protobuf wire
    /// format encodes them via zigzag (sint64), and `ElementReader`
    /// decodes them back to the same i64 values. This holds even
    /// though no CLI command consumes such files - per
    /// `DEVIATIONS.md` the *commands* reject negatives, but the
    /// underlying library primitives don't.
    ///
    /// Tier A11 follow-up: now also pins coordinates, not just ids.
    #[test]
    fn negative_id_node_fixture_roundtrips(
        n_neg in 1usize..20,
        n_pos in 1usize..20,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("fixture.osm.pbf");
        let input_nodes = generate_nodes_with_negatives(n_neg, n_pos);
        write_test_pbf_sorted(&path, &input_nodes, &[], &[]);

        let n = read_normalized(&path);
        prop_assert_eq!(n.nodes.len(), n_neg + n_pos);
        let by_id: BTreeMap<i64, &common::NormalizedNode> =
            n.nodes.iter().map(|x| (x.id, x)).collect();
        for input in &input_nodes {
            let output = by_id
                .get(&input.id)
                .expect("input mixed-sign node must appear in output");
            prop_assert_eq!(output.lat, input.lat);
            prop_assert_eq!(output.lon, input.lon);
        }
    }
}
