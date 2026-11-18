//! Smoke tests for the shared fixture helpers in `crates/lib/tests/common`.
//!
//! Most integration tests rely on `write_test_pbf` / `write_test_pbf_sorted`
//! and their variants without ever verifying that the helpers themselves
//! produce well-formed output. These tests pin the observable contract so
//! a bug in the fixture layer fails here instead of showing up as a
//! confusing unrelated test failure in a consumer.

use crate::common::{
    TestMeta, TestNode, assert_indexed, generate_nodes, generate_relations, generate_ways,
    read_header, read_normalized, write_multi_block_test_pbf, write_test_pbf_sorted,
};
use pbfhogg::{BlobDecode, BlobReader, BlobType};

// ---------------------------------------------------------------------------
// Multi-block fixture
// ---------------------------------------------------------------------------

/// `write_multi_block_test_pbf` should produce one data blob per
/// `block_size` elements within each type section, plus the header.
#[test]
fn multi_block_fixture_splits_at_block_size() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("multi.osm.pbf");

    // 100 nodes + 20 ways + 5 relations at block_size=25 should produce:
    //   nodes: 4 blobs (25/25/25/25)
    //   ways:  1 blob  (20 fits in one)
    //   relations: 1 blob (5 fits in one)
    //   total data blobs: 6
    let nodes = generate_nodes(100, 1);
    let ways = generate_ways(20, 1_000, 3, 1);
    let relations = generate_relations(5, 10_000, 2, 1_000);

    write_multi_block_test_pbf(&path, &nodes, &ways, &relations, 25);

    let mut data_blobs = 0usize;
    for blob in BlobReader::from_path(&path).expect("open pbf") {
        let blob = blob.expect("read blob");
        if matches!(blob.get_type(), BlobType::OsmData) {
            data_blobs += 1;
        }
    }
    assert_eq!(
        data_blobs, 6,
        "expected 6 data blobs (4 node + 1 way + 1 rel), got {data_blobs}"
    );

    // Header must still be present and declare the sorted flag.
    let header = read_header(&path);
    assert!(
        header.is_sorted(),
        "write_multi_block_test_pbf must emit Sort.Type_then_ID"
    );

    // Every blob should carry indexdata (the writer adds it automatically).
    assert_indexed(&path);

    // Contents round-trip intact.
    let read_back = read_normalized(&path);
    assert_eq!(read_back.nodes.len(), 100);
    assert_eq!(read_back.ways.len(), 20);
    assert_eq!(read_back.relations.len(), 5);
}

/// The generators should produce element vectors that can be written
/// straight through `write_test_pbf_sorted` and read back with matching
/// ids, so consumers can rely on them as a one-line fixture source.
#[test]
fn generator_roundtrip() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("gen.osm.pbf");

    let nodes = generate_nodes(50, 1);
    let ways = generate_ways(10, 1_000, 5, 1);
    let relations = generate_relations(3, 10_000, 4, 1_000);

    write_test_pbf_sorted(&path, &nodes, &ways, &relations);

    let c = read_normalized(&path);
    assert_eq!(
        c.nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
        (1..=50).collect::<Vec<_>>()
    );
    assert_eq!(
        c.ways.iter().map(|w| w.id).collect::<Vec<_>>(),
        (1_000..1_010).collect::<Vec<_>>()
    );
    assert_eq!(
        c.relations.iter().map(|r| r.id).collect::<Vec<_>>(),
        (10_000..10_003).collect::<Vec<_>>()
    );

    // Each way should have 5 refs; each relation 4 members.
    for w in &c.ways {
        assert_eq!(w.refs.len(), 5, "way {} ref count", w.id);
    }
    for r in &c.relations {
        assert_eq!(r.members.len(), 4, "relation {} member count", r.id);
    }
}

// ---------------------------------------------------------------------------
// Metadata plumbing
// ---------------------------------------------------------------------------

/// A `TestNode` with metadata should write through to the file and come
/// back via `read_normalized` with matching fields.
#[test]
fn test_meta_roundtrips_through_writer() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("meta.osm.pbf");

    let meta = TestMeta {
        version: 3,
        timestamp: 1_700_000_000,
        changeset: 424242,
        uid: 99,
        user: "tester",
        visible: true,
    };
    let nodes = vec![
        TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "a")],
            meta: Some(meta),
        },
        TestNode {
            id: 2,
            lat: 110_000_000,
            lon: 210_000_000,
            tags: vec![],
            meta: None,
        },
    ];

    write_test_pbf_sorted(&path, &nodes, &[], &[]);

    let c = read_normalized(&path);
    assert_eq!(c.nodes.len(), 2);

    let n1 = c.nodes.iter().find(|n| n.id == 1).expect("node 1");
    let m = n1.meta.as_ref().expect("node 1 should carry metadata");
    assert_eq!(
        *m,
        crate::common::NormalizedMeta {
            version: Some(3),
            milli_timestamp: Some(1_700_000_000_000),
            changeset: Some(424_242),
            uid: Some(99),
            user: Some("tester".to_owned()),
            visible: Some(true),
        }
    );

    // A block mixing metadata-bearing and metadata-less nodes should still
    // round-trip: the metadata-less node reads back with no metadata.
    let n2 = c.nodes.iter().find(|n| n.id == 2).expect("node 2");
    assert_eq!(n2.meta, None);
}

/// Smoke-check that `assert_indexed` passes for a file written with the
/// standard sorted writer. (`assert_non_indexed` is exercised by
/// `crates/lib/tests/non_indexed_parity.rs`, `crates/cli/tests/cli_diff.rs` and
/// `crates/cli/tests/cli_sort.rs`.)
#[test]
fn assert_indexed_passes_for_standard_fixture() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("indexed.osm.pbf");
    write_test_pbf_sorted(
        &path,
        &generate_nodes(3, 1),
        &generate_ways(1, 1_000, 3, 1),
        &[],
    );
    assert_indexed(&path);

    // And the file reads back as real PBF data (no blob corruption).
    let reader = BlobReader::from_path(&path).expect("open pbf");
    let mut saw_data = false;
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(_) = blob.decode().expect("decode blob") {
            saw_data = true;
        }
    }
    assert!(saw_data, "expected at least one decodeable data blob");
}

// ---------------------------------------------------------------------------
// Base fixture family
// ---------------------------------------------------------------------------

/// `write_base_pbf` is sorted and indexed; `write_changed_pbf` differs from
/// it by exactly the documented edits.
#[test]
fn base_and_changed_fixtures_differ_by_the_documented_edits() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let changed = dir.path().join("changed.osm.pbf");
    crate::common::write_base_pbf(&base);
    crate::common::write_changed_pbf(&changed);
    crate::common::assert_indexed(&base);
    crate::common::assert_sorted_file(&base);
    let mut expected = read_normalized(&base);
    expected.nodes.retain(|node| node.id != 7);
    expected.nodes.push(crate::common::NormalizedNode {
        id: 301,
        nano_lat: 30_100_000,
        nano_lon: 30_100_000,
        tags: Default::default(),
        meta: None,
    });
    expected.ways.retain(|way| way.id != 40);
    let actual = read_normalized(&changed);
    assert_eq!(actual.nodes, expected.nodes);
    assert_eq!(actual.ways, expected.ways);
    assert_eq!(actual.relations, expected.relations);
}

// ---------------------------------------------------------------------------
// Scratch and adversarial helpers
// ---------------------------------------------------------------------------

#[test]
fn bad_indexdata_fixture_contains_exactly_thirty_zero_bytes() {
    let dir = crate::common::test_scratch();
    let path = dir.child("bad-index.osm.pbf");
    crate::common::write_bad_indexdata_pbf(&path);
    let bytes = std::fs::read(path).expect("read fixture");
    let stream =
        crate::common::adversarial::locate_blobs_strict(&bytes).expect("complete fixture framing");
    let (blob, kind) = &stream.frames[1];
    assert_eq!(kind.as_slice(), b"OSMData");
    assert_eq!(
        crate::common::adversarial::blob_indexdata(&bytes[blob.header_start..blob.header_end]),
        Some([0_u8; 30].as_slice())
    );
}

#[test]
fn every_blob_index_guard_rejects_empty_and_incomplete_streams() {
    use crate::common::adversarial::locate_blobs;
    let dir = crate::common::test_scratch();
    let good = dir.child("good.osm.pbf");
    write_test_pbf_sorted(&good, &generate_nodes(3, 1), &[], &[]);
    crate::common::assert_every_data_blob_indexed(&good);
    let bytes = std::fs::read(&good).expect("read fixture");
    let frames = locate_blobs(&bytes);
    let mut trailing = bytes.clone();
    trailing.push(0);
    let mut malformed_header = bytes.clone();
    malformed_header[frames[1].header_start] = 0;
    let cases = [
        Vec::new(),
        vec![0],
        vec![0, 0, 0, 10, 0],
        bytes[..frames[0].blob_end].to_vec(),
        bytes[..bytes.len() - 1].to_vec(),
        trailing,
        malformed_header,
    ];
    for (i, input) in cases.iter().enumerate() {
        let path = dir.child(format!("invalid-{i}.osm.pbf"));
        std::fs::write(&path, input).expect("write invalid fixture");
        assert!(
            std::panic::catch_unwind(|| crate::common::assert_every_data_blob_indexed(&path))
                .is_err(),
            "invalid case {i} passed the index guard"
        );
    }
    let unindexed = dir.child("unindexed.osm.pbf");
    crate::common::write_test_pbf_non_indexed(&unindexed, &generate_nodes(3, 1), &[], &[]);
    assert!(
        std::panic::catch_unwind(|| crate::common::assert_every_data_blob_indexed(&unindexed))
            .is_err()
    );
}

/// Runs `snapshot_dir` on `root` and returns its panic message. The scratch
/// guard stays outside the caught closure: a guard dropped while unwinding
/// keeps its tree, so a `should_panic` test holding one would leave residue
/// on every passing run.
fn snapshot_panic_message(root: &std::path::Path) -> String {
    let payload = std::panic::catch_unwind(|| crate::common::snapshot_dir(root))
        .expect_err("snapshot_dir must panic");
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .expect("a string panic payload")
}

#[test]
fn missing_snapshot_root_is_an_error() {
    let dir = crate::common::test_scratch();
    let message = snapshot_panic_message(&dir.path().join("missing"));
    assert!(message.contains("read scratch directory"), "{message}");
}

#[test]
fn file_snapshot_root_is_an_error() {
    let dir = crate::common::test_scratch();
    let file = dir.path().join("file");
    std::fs::write(&file, b"").expect("create file");
    let message = snapshot_panic_message(&file);
    assert!(message.contains("read scratch directory"), "{message}");
}

#[test]
fn varint_roundtrip() {
    use crate::common::adversarial::{read_varint, write_varint};
    for &v in &[0_u64, 1, 127, 128, 255, 16_384, 1_u64 << 32, u64::MAX] {
        let mut buf = Vec::new();
        write_varint(&mut buf, v);
        let (got, n) = read_varint(&buf).expect("decode");
        assert_eq!(got, v);
        assert_eq!(n, buf.len());
    }
}

#[test]
fn truncate_to_caps_to_len() {
    use crate::common::adversarial::truncate_to;
    let bytes = vec![1, 2, 3, 4, 5];
    assert_eq!(truncate_to(&bytes, 3), vec![1, 2, 3]);
    assert_eq!(truncate_to(&bytes, 99), bytes);
    assert_eq!(truncate_to(&bytes, 0), Vec::<u8>::new());
}

#[cfg(unix)]
#[test]
fn scratch_snapshot_does_not_follow_symlink_cycles_or_escapes() {
    let dir = crate::common::test_scratch();
    let outside = crate::common::test_scratch();
    std::fs::write(outside.child("hidden"), b"x").expect("write");
    std::os::unix::fs::symlink(outside.path(), dir.child("escape")).expect("escape");
    std::os::unix::fs::symlink(dir.path(), dir.child("cycle")).expect("cycle");
    assert_eq!(
        crate::common::snapshot_dir(dir.path()),
        ["cycle", "escape"]
            .into_iter()
            .map(std::path::PathBuf::from)
            .collect()
    );
}

// ---------------------------------------------------------------------------
// Mixed-sign graph and its relabelled twin
// ---------------------------------------------------------------------------

mod mixed_sign {
    use crate::common::mixed_sign::{
        MIXED_SIGN_BBOX, MixedSignGraph, blob_ids, canonical_key, mixed_sign_graph,
    };
    use crate::common::{
        assert_every_data_blob_indexed, assert_non_indexed, assert_sorted_file,
        read_all_elements_with_coords,
    };
    use pbfhogg::MemberId;

    fn member_ids(g: &MixedSignGraph) -> Vec<(i64, MemberId)> {
        g.relations
            .iter()
            .flat_map(|r| r.members.iter().map(move |m| (r.id, m.id)))
            .collect()
    }

    /// The graph has the edges the negative-ID cases need: way refs and
    /// relation members of every kind crossing zero in both directions,
    /// magnitude twins in every kind, and nothing dangling.
    #[test]
    fn the_graph_crosses_zero_both_ways_and_is_complete() {
        let g = mixed_sign_graph();
        let refs: Vec<(i64, i64)> = g
            .ways
            .iter()
            .flat_map(|w| w.refs.iter().map(move |&r| (w.id, r)))
            .collect();
        assert!(refs.iter().any(|&(w, n)| w < 0 && n > 0));
        assert!(refs.iter().any(|&(w, n)| w > 0 && n < 0));
        assert!(refs.iter().any(|&(w, n)| w < 0 && n < 0));
        let members = member_ids(&g);
        for kind in 0..3 {
            let of_kind = |m: &MemberId| match (kind, m) {
                (0, MemberId::Node(id)) | (1, MemberId::Way(id)) | (2, MemberId::Relation(id)) => {
                    Some(*id)
                }
                _ => None,
            };
            assert!(
                members
                    .iter()
                    .any(|(r, m)| *r < 0 && of_kind(m).is_some_and(|id| id > 0)),
                "kind {kind}: negative relation over a positive member"
            );
            assert!(
                members
                    .iter()
                    .any(|(r, m)| *r > 0 && of_kind(m).is_some_and(|id| id < 0)),
                "kind {kind}: positive relation over a negative member"
            );
        }
        for ids in [
            g.nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
            g.ways.iter().map(|w| w.id).collect(),
            g.relations.iter().map(|r| r.id).collect(),
        ] {
            assert!(ids.contains(&7) && ids.contains(&-7), "magnitude twins");
            assert!(
                ids.is_sorted_by_key(|&id| canonical_key(id)),
                "canonical order"
            );
        }
        let node_ids: Vec<i64> = g.nodes.iter().map(|n| n.id).collect();
        let way_ids: Vec<i64> = g.ways.iter().map(|w| w.id).collect();
        let rel_ids: Vec<i64> = g.relations.iter().map(|r| r.id).collect();
        assert!(refs.iter().all(|(_, n)| node_ids.contains(n)));
        assert!(members.iter().all(|(_, m)| match m {
            MemberId::Node(id) => node_ids.contains(id),
            MemberId::Way(id) => way_ids.contains(id),
            MemberId::Relation(id) => rel_ids.contains(id),
            _ => false,
        }));
        let (min_lat, min_lon, max_lat, max_lon) = MIXED_SIGN_BBOX;
        let inside = |id: i64| {
            g.nodes.iter().find(|n| n.id == id).is_some_and(|n| {
                (min_lat..=max_lat).contains(&n.lat) && (min_lon..=max_lon).contains(&n.lon)
            })
        };
        assert!(inside(-7) && inside(7) && !inside(-11) && !inside(11));
        // The boundary's outer ring is closed.
        let ring = g.ways.iter().find(|w| w.id == 7).expect("ring");
        assert_eq!(ring.refs.first(), ring.refs.last());
    }

    /// The twin is the same graph under an order-preserving bijection onto
    /// positive IDs, per kind.
    #[test]
    fn the_twin_is_an_order_preserving_positive_relabelling() {
        let g = mixed_sign_graph();
        let (twin, relabel) = g.relabelled_twin();
        for map in [&relabel.nodes, &relabel.ways, &relabel.relations] {
            let mut pairs: Vec<(i64, i64)> = map.iter().map(|(&a, &b)| (a, b)).collect();
            pairs.sort_by_key(|&(a, _)| canonical_key(a));
            let images: Vec<i64> = pairs.iter().map(|&(_, b)| b).collect();
            let want: Vec<i64> = (1..=i64::try_from(images.len()).expect("fits")).collect();
            assert_eq!(images, want);
        }
        assert_eq!(twin.nodes.len(), g.nodes.len());
        for (a, b) in g.nodes.iter().zip(&twin.nodes) {
            assert_eq!(relabel.node(a.id), b.id);
            assert_eq!((a.lat, a.lon, &a.tags), (b.lat, b.lon, &b.tags));
        }
        for (a, b) in g.ways.iter().zip(&twin.ways) {
            assert_eq!(relabel.way(a.id), b.id);
            let refs: Vec<i64> = a.refs.iter().map(|&r| relabel.node(r)).collect();
            assert_eq!(refs, b.refs);
        }
        let mapped: Vec<(i64, MemberId)> = member_ids(&g)
            .into_iter()
            .map(|(r, m)| (relabel.relation(r), relabel.member(m)))
            .collect();
        assert_eq!(mapped, member_ids(&twin));
        // The magnitude twins land apart.
        assert_ne!(relabel.node(7), relabel.node(-7));
    }

    /// The layout writer the negative-ID registry uses: its plain layout
    /// cuts the blobs as `write` does, its unsorted layout reverses every
    /// kind under a header claiming no order, and its locations layout
    /// embeds each ref's own node coordinates under `LocationsOnWays`.
    #[test]
    fn the_layout_writer_cuts_reverses_and_embeds_as_documented() {
        use crate::common::mixed_sign::Layout;
        let dir = crate::common::test_scratch();
        let g = mixed_sign_graph();
        let written = dir.path().join("written.osm.pbf");
        g.write(&written, true);
        for indexdata in [true, false] {
            let plain = dir.path().join(format!("plain-{indexdata}.osm.pbf"));
            g.write_layout(&plain, Layout::plain(indexdata));
            assert_eq!(blob_ids(&plain), blob_ids(&written));
        }

        let reversed = dir.path().join("reversed.osm.pbf");
        g.write_layout(
            &reversed,
            Layout {
                indexdata: true,
                sorted: false,
                locations: false,
            },
        );
        assert!(!crate::common::read_header(&reversed).is_sorted());
        let ids: Vec<i64> = blob_ids(&reversed).into_iter().flatten().collect();
        let mut want: Vec<i64> = g.nodes.iter().rev().map(|n| n.id).collect();
        want.extend(g.ways.iter().rev().map(|w| w.id));
        want.extend(g.relations.iter().rev().map(|r| r.id));
        assert_eq!(ids, want);

        let located = dir.path().join("located.osm.pbf");
        g.write_layout(
            &located,
            Layout {
                locations: true,
                ..Layout::plain(true)
            },
        );
        let header = crate::common::read_header(&located);
        assert!(
            header
                .optional_features()
                .iter()
                .any(|f| f == "LocationsOnWays")
        );
        let back = crate::common::read_normalized(&located);
        for way in &back.ways {
            let locations = way.locations.as_ref().expect("inline locations");
            for (r, (lat, lon)) in way.refs.iter().zip(locations) {
                let n = g.node(*r);
                assert_eq!((lat / 100, lon / 100), (i64::from(n.lat), i64::from(n.lon)));
            }
        }
    }

    /// Written, the graph has the blob layouts the negative-ID cases rely
    /// on, indexed and not, and reads back as written; the twin has the same
    /// cuts.
    #[test]
    fn the_written_graph_has_the_zero_straddling_blob_layouts() {
        let dir = crate::common::test_scratch();
        let g = mixed_sign_graph();
        let (twin, relabel) = g.relabelled_twin();
        let indexed = dir.path().join("mixed.osm.pbf");
        let plain = dir.path().join("mixed-plain.osm.pbf");
        let twin_path = dir.path().join("twin.osm.pbf");
        g.write(&indexed, true);
        g.write(&plain, false);
        twin.write(&twin_path, true);
        assert_every_data_blob_indexed(&indexed);
        assert_non_indexed(&plain);
        assert_sorted_file(&indexed);
        assert_sorted_file(&twin_path);

        let blobs = blob_ids(&indexed);
        assert_eq!(blobs, blob_ids(&plain));
        assert_eq!(
            &blobs[..3],
            &[vec![0, -1, -2], vec![-5, -7, -9], vec![-11, 1, 2]],
            "zero beside negatives, a negative-only blob, then one straddling zero"
        );
        let straddles = |ids: &[i64]| ids.iter().any(|&i| i < 0) && ids.iter().any(|&i| i > 0);
        assert_eq!(
            blobs.iter().filter(|ids| straddles(ids)).count(),
            3,
            "one per kind"
        );
        let twin_blobs = blob_ids(&twin_path);
        assert_eq!(
            twin_blobs.iter().map(Vec::len).collect::<Vec<_>>(),
            blobs.iter().map(Vec::len).collect::<Vec<_>>()
        );
        assert!(twin_blobs.iter().flatten().all(|&id| id > 0));

        let back = read_all_elements_with_coords(&indexed);
        let nodes: Vec<(i64, i32, i32)> = back.nodes.iter().map(|n| (n.0, n.1, n.2)).collect();
        let want: Vec<(i64, i32, i32)> = g.nodes.iter().map(|n| (n.id, n.lat, n.lon)).collect();
        assert_eq!(nodes, want);
        let ways: Vec<(i64, Vec<i64>)> = back.ways.iter().map(|w| (w.0, w.1.clone())).collect();
        let want: Vec<(i64, Vec<i64>)> = g.ways.iter().map(|w| (w.id, w.refs.clone())).collect();
        assert_eq!(ways, want);
        let twin_back = read_all_elements_with_coords(&twin_path);
        assert_eq!(
            twin_back
                .ways
                .iter()
                .map(|w| w.1.clone())
                .collect::<Vec<_>>(),
            g.ways
                .iter()
                .map(|w| w.refs.iter().map(|&r| relabel.node(r)).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
    }
}
