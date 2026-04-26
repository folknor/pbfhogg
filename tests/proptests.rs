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

use bytes::Bytes;
use common::{
    generate_nodes, generate_nodes_with_negatives, read_normalized,
    write_test_pbf_sorted,
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
    /// assert every input id is present in the output and counts
    /// match. Pins the read/write surface against arbitrary `count`
    /// and `start_id` shapes.
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
        let mut input_ids: Vec<i64> = input_nodes.iter().map(|x| x.id).collect();
        input_ids.sort_unstable();
        let mut output_ids: Vec<i64> = n.nodes.iter().map(|x| x.id).collect();
        output_ids.sort_unstable();
        prop_assert_eq!(input_ids, output_ids);
    }

    /// Library-level mixed-sign roundtrip: `BlockBuilder` /
    /// `PbfWriter` accept negative ids on input, the protobuf wire
    /// format encodes them via zigzag (sint64), and `ElementReader`
    /// decodes them back to the same i64 values. This holds even
    /// though no CLI command consumes such files - per
    /// `DEVIATIONS.md` the *commands* reject negatives, but the
    /// underlying library primitives don't.
    ///
    /// Pinning this contract separately from
    /// `node_fixture_roundtrips` makes the layering explicit: a
    /// future change that breaks library-level mixed-sign support
    /// fails this test loudly, even if every command-level test
    /// still passes (because no CLI command exercises mixed-sign
    /// input).
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
        let mut input_ids: Vec<i64> = input_nodes.iter().map(|x| x.id).collect();
        input_ids.sort_unstable();
        let mut output_ids: Vec<i64> = n.nodes.iter().map(|x| x.id).collect();
        output_ids.sort_unstable();
        prop_assert_eq!(input_ids, output_ids);
    }
}
