//! Multi-PBF merge correctness tests.

use crate::common::{
    TestMember, TestNode, TestRelation, TestWay, node_ids_id_only as node_ids,
    read_all_elements_id_only as read_all_elements, relation_ids_id_only as relation_ids,
    way_ids_id_only as way_ids, write_test_pbf_sorted,
};
use pbfhogg::MemberId;
use pbfhogg::cat::dedupe::merge_pbf;
use pbfhogg::write::WriteOptions;

fn default_opts() -> WriteOptions {
    WriteOptions {
        force: true,
        ..WriteOptions::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn merge_disjoint_node_files() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &a,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("name", "a")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("name", "c")],
                meta: None,
            },
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path()];
    let stats = merge_pbf(
        &inputs,
        crate::common::out(&output),
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge_pbf");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3, 4]);
    // Disjoint indexed inputs are copied through undecoded: their counts are
    // indexdata claims, not observations.
    assert_eq!(stats.nodes, 0);
    assert_eq!(stats.passthrough_claimed, 4);
    assert_eq!(stats.duplicates_removed, 0);
}

#[test]
fn merge_overlapping_nodes_dedup() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Both files contain nodes 2 and 3 (exact duplicates - same id, no metadata = version 0)
    write_test_pbf_sorted(
        &a,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "shared")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "shared")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path()];
    let stats = merge_pbf(
        &inputs,
        crate::common::out(&output),
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge_pbf");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3, 4]);
    assert_eq!(stats.nodes, 4);
    assert_eq!(stats.duplicates_removed, 2);
}

#[test]
fn merge_with_ways_and_relations() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &a,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[TestNode {
            id: 3,
            lat: 120_000_000,
            lon: 220_000_000,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 20,
            refs: vec![3],
            tags: vec![("highway", "secondary")],
            meta: None,
        }],
        &[TestRelation {
            id: 100,
            members: vec![
                TestMember {
                    id: MemberId::Way(10),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Way(20),
                    role: "inner",
                },
            ],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path()];
    let stats = merge_pbf(
        &inputs,
        crate::common::out(&output),
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge_pbf");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10, 20]);
    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(stats.nodes, 0);
    assert_eq!(stats.ways, 0);
    assert_eq!(stats.relations, 0);
    assert_eq!(stats.passthrough_claimed, 6);
}

#[test]
fn merge_single_file() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &a,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path()];
    let stats = merge_pbf(
        &inputs,
        crate::common::out(&output),
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge_pbf");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(stats.nodes, 0);
    assert_eq!(stats.passthrough_claimed, 2);
    assert_eq!(stats.duplicates_removed, 0);
}

#[test]
fn merge_three_files() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let c_file = dir.path().join("c.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &a,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[TestNode {
            id: 2,
            lat: 110_000_000,
            lon: 210_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &c_file,
        &[TestNode {
            id: 3,
            lat: 120_000_000,
            lon: 220_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path(), c_file.as_path()];
    let stats = merge_pbf(
        &inputs,
        crate::common::out(&output),
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge_pbf");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(stats.nodes, 0);
    assert_eq!(stats.passthrough_claimed, 3);
    assert_eq!(stats.duplicates_removed, 0);
}

#[test]
fn merge_empty_files() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&a, &[], &[], &[]);
    write_test_pbf_sorted(&b, &[], &[], &[]);

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path()];
    let stats = merge_pbf(
        &inputs,
        crate::common::out(&output),
        // No `force`: a header-only file has no blob lacking indexdata.
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge_pbf");

    assert_eq!(stats.nodes, 0);
    assert_eq!(stats.ways, 0);
    assert_eq!(stats.relations, 0);
    assert_eq!(stats.duplicates_removed, 0);
}

// Pins that `merge_pbf` with the same input file supplied twice
// preserves every element type. The earlier regression dropped ways
// and relations because `cat/dedupe.rs`'s pass-2 loop grouped
// consecutive `overlaps[i]=true` entries into one `write_overlap_run`
// call without checking that the run stayed within a single element
// kind. Two back-to-back same-kind overlap pairs (node|node, way|way)
// merged into one run, which then inherited `entries[0].index.kind`
// and dropped elements of every other kind. Fixed 2026-04-23 by
// requiring the overlap-run walker to also match `index.kind`.
#[test]
#[allow(clippy::too_many_lines)]
fn merge_same_input_preserves_ways_and_relations() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &a,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("name", "n1")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "n2")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("name", "n3")],
                meta: None,
            },
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![("name", "n4")],
                meta: None,
            },
            TestNode {
                id: 5,
                lat: 140_000_000,
                lon: 240_000_000,
                tags: vec![("name", "n5")],
                meta: None,
            },
            TestNode {
                id: 6,
                lat: 150_000_000,
                lon: 250_000_000,
                tags: vec![("name", "n6")],
                meta: None,
            },
            TestNode {
                id: 7,
                lat: 160_000_000,
                lon: 260_000_000,
                tags: vec![("name", "n7")],
                meta: None,
            },
            TestNode {
                id: 8,
                lat: 170_000_000,
                lon: 270_000_000,
                tags: vec![("name", "n8")],
                meta: None,
            },
            TestNode {
                id: 9,
                lat: 180_000_000,
                lon: 280_000_000,
                tags: vec![("name", "n9")],
                meta: None,
            },
            TestNode {
                id: 10,
                lat: 190_000_000,
                lon: 290_000_000,
                tags: vec![("name", "n10")],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 100,
                refs: vec![1, 2, 3],
                tags: vec![("highway", "residential")],
                meta: None,
            },
            TestWay {
                id: 101,
                refs: vec![3, 4, 5],
                tags: vec![("highway", "service")],
                meta: None,
            },
            TestWay {
                id: 102,
                refs: vec![5, 6, 7],
                tags: vec![("waterway", "stream")],
                meta: None,
            },
            TestWay {
                id: 103,
                refs: vec![7, 8, 9, 10],
                tags: vec![("landuse", "meadow")],
                meta: None,
            },
        ],
        &[TestRelation {
            id: 1000,
            members: vec![
                TestMember {
                    id: MemberId::Way(100),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Way(101),
                    role: "inner",
                },
                TestMember {
                    id: MemberId::Way(102),
                    role: "subarea",
                },
                TestMember {
                    id: MemberId::Way(103),
                    role: "label",
                },
            ],
            tags: vec![("type", "multipolygon"), ("name", "fixture")],
            meta: None,
        }],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), a.as_path()];
    let stats = merge_pbf(
        &inputs,
        crate::common::out(&output),
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge_pbf");
    let merged = read_all_elements(&output);

    assert_eq!(node_ids(&merged), vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    assert_eq!(way_ids(&merged), vec![100, 101, 102, 103]);
    assert_eq!(relation_ids(&merged), vec![1000]);
    assert_eq!(stats.nodes, 10);
    assert_eq!(stats.ways, 4);
    assert_eq!(stats.relations, 1);
}

/// F60: Three files with overlapping ID ranges - exercises 3-way heap merge.
#[test]
#[allow(clippy::too_many_lines)]
fn merge_three_files_overlapping_ids() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let c_file = dir.path().join("c.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Three files with overlapping node ID ranges:
    // A: 1, 3, 5    B: 2, 3, 4    C: 3, 5, 6
    // Node 3 appears in all three, node 5 in A+C - tests 3-way dedup
    write_test_pbf_sorted(
        &a,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("src", "a")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("src", "a")],
                meta: None,
            },
            TestNode {
                id: 5,
                lat: 140_000_000,
                lon: 240_000_000,
                tags: vec![("src", "a")],
                meta: None,
            },
        ],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("src", "b")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("src", "b")],
                meta: None,
            },
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![("src", "b")],
                meta: None,
            },
        ],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &c_file,
        &[
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("src", "c")],
                meta: None,
            },
            TestNode {
                id: 5,
                lat: 140_000_000,
                lon: 240_000_000,
                tags: vec![("src", "c")],
                meta: None,
            },
            TestNode {
                id: 6,
                lat: 150_000_000,
                lon: 250_000_000,
                tags: vec![("src", "c")],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path(), c_file.as_path()];
    let stats = merge_pbf(
        &inputs,
        crate::common::out(&output),
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge_pbf");
    let c = read_all_elements(&output);

    // Output should contain exactly 6 unique nodes (1-6), sorted
    assert_eq!(node_ids(&c), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(stats.nodes, 6);
    // Node 3 appears in 3 files (2 duplicates), node 5 in 2 files (1 duplicate)
    assert_eq!(
        stats.duplicates_removed, 3,
        "3 duplicates: node 3 ×2 + node 5 ×1"
    );
}

/// A corrupt 4-byte frame prefix in a merge input must be rejected by the size
/// guard, not honoured as an allocation request.
///
/// `cat --dedupe`'s pass-1 frame loop was the fifth hand-rolled copy of the
/// blob-frame decoder, and the only one that carried neither the
/// `MAX_BLOB_HEADER_SIZE` cap nor the zero-length rejection: it resized its
/// header buffer straight from the unvalidated prefix, so four bytes of
/// corrupt input asked for up to a 4 GiB allocation before anything inspected
/// them. The loop now shares `read::frame`'s decoder with the other four
/// sites.
///
/// A trailing `0xFFFFFFFF` prefix is the worst case expressible in four bytes.
/// The other arm of the same guard is checked alongside it: a zero-length
/// prefix advances four bytes while declaring no type and no payload, so
/// accepting it would let a run of zero bytes read as a clean, empty success.
///
/// Each arm asserts the *specific* guard's message rather than a shared
/// substring. That is load-bearing, not pedantry: with only a "header" check,
/// the zero-length arm passed with the zero-length guard disabled, because a
/// zero-length prefix then parses an empty `BlobHeader` and is caught one layer
/// down by `WireBlobHeader::parse`'s no-type-field rejection - whose message
/// also says "header". The arm tested that a corrupt file is rejected, which
/// was never in doubt, instead of testing the guard it was written for.
#[test]
fn merge_rejects_a_corrupt_frame_prefix_without_allocating() {
    use std::io::Write as _;

    for (label, prefix, expected) in [
        (
            "0xFFFFFFFF",
            [0xFFu8, 0xFF, 0xFF, 0xFF],
            "blob header is too big: 4294967295 bytes",
        ),
        ("zero-length", [0u8, 0, 0, 0], "blob header size is zero"),
    ] {
        let dir = crate::common::test_scratch();
        let a = dir.path().join("a.osm.pbf");
        let output = dir.path().join("output.osm.pbf");

        write_test_pbf_sorted(
            &a,
            &[TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            }],
            &[],
            &[],
        );
        // The file is valid up to here, so the corrupt prefix sits at a frame
        // boundary and the decoder reaches it having succeeded on every
        // preceding frame. Four bytes is a *complete* prefix, which the EOF
        // policy must treat as a committed frame rather than as a tolerable
        // 1-3 byte tail.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&a)
            .expect("reopen fixture");
        f.write_all(&prefix).expect("append corrupt prefix");
        drop(f);

        let inputs: Vec<&std::path::Path> = vec![a.as_path()];
        let err = merge_pbf(
            &inputs,
            crate::common::out(&output),
            &default_opts(),
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
        )
        .err()
        .unwrap_or_else(|| panic!("a {label} frame prefix must be rejected, not merged"));
        let msg = err.to_string().to_ascii_lowercase();
        assert!(
            msg.contains(expected),
            "a {label} prefix must fail on its own frame-prefix guard \
             (expected {expected:?}), got: {msg}"
        );
    }
}

// ---------------------------------------------------------------------------
// An input that changes between the two passes
// ---------------------------------------------------------------------------

/// Runs `mutate` once, when the merge reports that pass 2 begins: after pass
/// 1 has indexed every input and before pass 2 opens any of them.
struct BetweenPasses {
    mutate: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl pbfhogg::host::DiagnosticSink for BetweenPasses {
    fn event(&self, event: &pbfhogg::host::Event<'_>) {
        if event.key == "cat.dedupe.pass2"
            && let Some(mutate) = self.mutate.lock().expect("sink lock").take()
        {
            mutate();
        }
    }
}

/// Input A: nodes 1..=3 and way 10; input B: nodes 2..=4, whose blob
/// overlaps A's (decoded in pass 2), and relation 100, which overlaps
/// nothing (copied through).
fn between_pass_fixtures(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let node = |id| TestNode {
        id,
        lat: 0,
        lon: 0,
        tags: vec![],
        meta: None,
    };
    let a = dir.join("a.osm.pbf");
    let b = dir.join("b.osm.pbf");
    write_test_pbf_sorted(
        &a,
        &[node(1), node(2), node(3)],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![],
            meta: None,
        }],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[node(2), node(3), node(4)],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![],
            tags: vec![],
            meta: None,
        }],
    );
    (a, b)
}

/// Every frame of `path`: (frame start, blob payload length), in file order.
fn frames(path: &std::path::Path) -> Vec<(u64, u64)> {
    let file = std::fs::File::open(path).expect("open fixture");
    let mut reader = pbfhogg::BlobReader::new_seekable(file).expect("blob reader");
    let mut frames = Vec::new();
    while let Some(next) = reader.next_header_skip_blob() {
        let (header, offset) = next.expect("frame header");
        let size = u64::try_from(header.get_blob_size()).expect("blob size");
        frames.push((offset.expect("offset").0, size));
    }
    frames
}

/// Merge A and B with `mutate` applied between the passes; the error.
fn merge_changed_between_passes(
    dir: &std::path::Path,
    a: &std::path::Path,
    b: &std::path::Path,
    mutate: impl FnOnce() + Send + 'static,
) -> pbfhogg::Error {
    let sink = std::sync::Arc::new(BetweenPasses {
        mutate: std::sync::Mutex::new(Some(Box::new(mutate))),
    });
    let mut host = crate::common::test_host();
    host.diagnostics =
        pbfhogg::host::Diagnostics::new(std::sync::Arc::<BetweenPasses>::clone(&sink));
    let result = merge_pbf(
        &[a, b],
        crate::common::out(dir.join("output.osm.pbf")),
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &host,
    );
    assert!(
        sink.mutate.lock().expect("sink lock").is_none(),
        "the input must have been changed between the passes"
    );
    match result {
        Ok(_) => panic!("an input changed between the passes must fail the merge"),
        Err(err) => err,
    }
}

/// A blob of an overlap run that no longer decodes when pass 2 re-reads it
/// fails located at its input and frame - the second input, at a nonzero
/// offset, so first-input attribution cannot pass.
#[test]
fn a_blob_corrupted_between_passes_is_located() {
    let dir = crate::common::test_scratch();
    let (a, b) = between_pass_fixtures(dir.path());
    // Header, nodes, relation.
    let frames = frames(&b);
    assert_eq!(frames.len(), 3);
    let (node_frame, _) = frames[1];
    let node_end = frames[2].0;
    let payload = node_end - frames[1].1;

    let target = b.clone();
    let err = merge_changed_between_passes(dir.path(), &a, &b, move || {
        // Keep every length and the BlobHeader; make the Blob unparseable.
        let mut bytes = std::fs::read(&target).expect("read B");
        let range = usize::try_from(payload).expect("payload")
            ..usize::try_from(node_end).expect("frame end");
        bytes[range].fill(0xff);
        std::fs::write(&target, bytes).expect("corrupt B");
    });
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::WireFormat { .. }),
        "expected the Blob decode failure, got {}",
        err.report()
    );
    assert_eq!(err.source_path(), Some(b.as_path()), "{}", err.report());
    assert_eq!(
        err.byte_offset(),
        Some(pbfhogg::ByteOffset(node_frame)),
        "{}",
        err.report()
    );
}

/// A passthrough frame that is gone when pass 2 re-reads it fails located at
/// its input and frame.
#[test]
fn a_frame_truncated_between_passes_is_located() {
    let dir = crate::common::test_scratch();
    let (a, b) = between_pass_fixtures(dir.path());
    let frames = frames(&b);
    assert_eq!(frames.len(), 3);
    let (relation_frame, _) = frames[2];

    let target = b.clone();
    let err = merge_changed_between_passes(dir.path(), &a, &b, move || {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .expect("open B")
            .set_len(relation_frame)
            .expect("truncate B at the relation frame");
    });
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof),
        "expected the frame read to hit end of file, got {}",
        err.report()
    );
    assert_eq!(err.source_path(), Some(b.as_path()), "{}", err.report());
    assert_eq!(
        err.byte_offset(),
        Some(pbfhogg::ByteOffset(relation_frame)),
        "{}",
        err.report()
    );
}

// ---------------------------------------------------------------------------
// The overlap-run pipeline shared with sort
// ---------------------------------------------------------------------------

/// Overlap runs in the fixture below.
const RUNS: i64 = 8;
/// Blobs per run, their IDs interleaved so every blob of a run overlaps.
const BLOBS_PER_RUN: i64 = 6;
/// Nodes per blob: a run re-encodes into several output blocks. Without tags
/// or metadata they compress to one chunk, so no run here fills its channel;
/// the planet-density tests below are the ones whose runs do.
const NODES_PER_BLOB: i64 = 6_000;

/// A file claiming `Sort.Type_then_ID` whose node blobs nevertheless
/// overlap: `RUNS` runs of `BLOBS_PER_RUN` interleaved blobs. Indexed, so
/// pass 1 reads headers only and every payload is first decoded by an
/// overlap worker in pass 2. Every node sits at latitude `lat`. Returns its
/// frames.
fn write_interleaved_runs(path: &std::path::Path, lat: i32) -> Vec<(u64, u64)> {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(&HeaderBuilder::new().sorted().build().expect("header"))
        .expect("write header");
    let mut bb = BlockBuilder::new();
    for run in 0..RUNS {
        let base = 1 + run * BLOBS_PER_RUN * NODES_PER_BLOB;
        for blob in 0..BLOBS_PER_RUN {
            for i in 0..NODES_PER_BLOB {
                let id = base + blob + i * BLOBS_PER_RUN;
                bb.add_node(id, lat, 0, std::iter::empty::<(&str, &str)>(), None);
            }
            let bytes = bb.take().expect("encode").expect("non-empty block");
            writer.write_primitive_block(bytes).expect("write block");
        }
    }
    writer.flush().expect("flush fixture");
    frames(path)
}

fn host_with_cores(cores: usize) -> pbfhogg::host::Host {
    let mut host = crate::common::test_host();
    host.resources = std::sync::Arc::new(pbfhogg::host::Fixed {
        parallelism: std::num::NonZeroUsize::new(cores),
        memory_available: Some(8 << 30),
    });
    host
}

/// The sweeps run on worker threads but the output is written in run order:
/// one worker and many write the same bytes, each run being one overlap
/// component re-encoded into several blocks, with (on one worker) more runs
/// than the admission window. The two inputs hold the same IDs and versions
/// at different coordinates, so which of each duplicate pair survives shows
/// in the bytes: the earliest input's copy, whatever the worker count.
#[test]
fn overlap_runs_write_the_same_bytes_on_one_worker_and_many() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    write_interleaved_runs(&a, 0);
    write_interleaved_runs(&b, 1_000);
    let total = RUNS * BLOBS_PER_RUN * NODES_PER_BLOB;
    let mut outputs = Vec::new();
    // Three cores is one sweep worker; ten is eight, one per run.
    for cores in [3, 10] {
        let output = dir.path().join(format!("out-{cores}.osm.pbf"));
        let stats = merge_pbf(
            &[a.as_path(), b.as_path()],
            crate::common::out(&output),
            &default_opts(),
            &pbfhogg::HeaderOverrides::default(),
            &host_with_cores(cores),
        )
        .unwrap_or_else(|e| panic!("{cores} cores: {e}"));
        let total_u64 = u64::try_from(total).expect("count");
        assert_eq!(stats.nodes, total_u64, "{cores} cores");
        assert_eq!(stats.duplicates_removed, total_u64, "{cores} cores");
        assert_eq!(
            stats.blobs_rewritten,
            u64::try_from(2 * RUNS * BLOBS_PER_RUN).expect("count"),
            "{cores} cores"
        );
        let ids = node_ids(&read_all_elements(&output));
        assert!(
            ids.iter().copied().eq(1..=total),
            "{cores} cores: merged node IDs are not 1..=N"
        );
        outputs.push(std::fs::read(&output).expect("read output"));
    }
    assert!(
        outputs[0] == outputs[1],
        "the worker count changed the output bytes"
    );
}

/// A run that fails while later runs are in flight ends the merge with that
/// run's error, located at its input and frame, instead of hanging or
/// reporting a consequence of the shutdown. The admission window (four runs
/// on two workers) is smaller than the run count, so the dispatcher can be
/// waiting on credit when the error lands; the test does not force that
/// schedule, only allows it.
#[test]
fn a_failed_overlap_run_ends_the_pipeline_with_its_own_error() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let frames = write_interleaved_runs(&a, 0);
    // Frame 0 is the header, then the blobs in order: corrupt the payload of
    // run 0's fourth blob, keeping every length and its BlobHeader.
    let (frame, payload_len) = frames[4];
    let end = frames[5].0;
    let mut bytes = std::fs::read(&a).expect("read A");
    let start = usize::try_from(end - payload_len).expect("payload start");
    bytes[start..usize::try_from(end).expect("frame end")].fill(0xff);
    std::fs::write(&a, bytes).expect("corrupt A");

    // Four cores is two workers and a window of four runs, for eight runs.
    let err = merge_pbf(
        &[a.as_path()],
        crate::common::out(dir.path().join("out.osm.pbf")),
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &host_with_cores(4),
    )
    .err()
    .unwrap_or_else(|| panic!("a corrupt overlap blob must fail the merge"));
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::WireFormat { .. }),
        "expected the Blob decode failure, got {}",
        err.report()
    );
    assert_eq!(err.source_path(), Some(a.as_path()), "{}", err.report());
    assert_eq!(
        err.byte_offset(),
        Some(pbfhogg::ByteOffset(frame)),
        "{}",
        err.report()
    );
}

// ---------------------------------------------------------------------------
// What an overlap sweep holds, at planet blob density
// ---------------------------------------------------------------------------
//
// These assert the sweep's own resource counters (`overlap_run_*`, see
// `crates/lib/src/commands/blob_merge/sweep.rs`), never the host's resident memory.

/// The sweep constants these tests are sized against
/// (`OVERLAP_RUN_CHUNK_BYTES`, `OVERLAP_RUN_CHANNEL_DEPTH`,
/// `SWEEP_PINNED_BYTES`). They are crate-private, so a unit test beside the
/// sweep fails if one changes without these.
const CHUNK_BYTES: u64 = 2 << 20;
const CHANNEL_DEPTH: u64 = 4;
const PINNED_BYTES: u64 = 64 << 20;

/// Planet's elements per blob (reference/blob-density.md).
const PLANET_BLOB_ELEMENTS: i64 = 228_000;

/// Records every counter the run emits.
#[derive(Default)]
struct Counters(std::sync::Mutex<Vec<(String, i64)>>);

impl pbfhogg::debug::InstrumentSink for Counters {
    fn marker(&self, _name: &str) {}
    fn counter(&self, name: &str, value: i64) {
        self.0.lock().expect("sink").push((name.to_owned(), value));
    }
}

impl Counters {
    /// The value of the one emission of `name`.
    fn get(&self, name: &str) -> u64 {
        let all = self.0.lock().expect("sink");
        let mut values = all.iter().filter(|(n, _)| n == name).map(|(_, v)| *v);
        let value = values
            .next()
            .unwrap_or_else(|| panic!("no counter named {name}"));
        assert!(values.next().is_none(), "{name} emitted more than once");
        u64::try_from(value).expect("non-negative counter")
    }
}

/// A host of `cores` cores whose invocation records every counter.
fn recording_host(cores: usize) -> (pbfhogg::host::Host, std::sync::Arc<Counters>) {
    let counters = std::sync::Arc::new(Counters::default());
    let mut host = host_with_cores(cores);
    host.invocation = pbfhogg::host::Invocation::with_sink(
        std::sync::Arc::clone(&counters) as std::sync::Arc<dyn pbfhogg::debug::InstrumentSink>
    );
    (host, counters)
}

/// Uncompressed output: a frame is its payload, so these debug-build tests
/// spend their time in the sweep, not in zlib.
fn uncompressed_opts() -> WriteOptions {
    WriteOptions {
        compression: pbfhogg::writer::Compression::None,
        ..default_opts()
    }
}

/// Write one uncompressed node blob per entry of `blobs`, each node tagged
/// `v=value(id)`, under a header claiming `Sort.Type_then_ID` when `sorted`.
/// Returns each blob's payload length: what the sweep charges for holding
/// its decoded block.
fn write_node_blobs(
    path: &std::path::Path,
    sorted: bool,
    blobs: &[Vec<i64>],
    value: impl Fn(i64) -> String,
) -> Vec<u64> {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::None);
    let header = if sorted {
        HeaderBuilder::new().sorted()
    } else {
        HeaderBuilder::new()
    };
    writer
        .write_header(&header.build().expect("header"))
        .expect("write header");
    let mut payloads = Vec::new();
    for ids in blobs {
        // One block per entry, however large: the element cap is the
        // entry's own length.
        let cap = std::num::NonZeroUsize::new(ids.len()).expect("non-empty blob");
        let mut bb = BlockBuilder::with_element_cap_prealloc(cap, 8_000);
        for &id in ids {
            let v = value(id);
            bb.add_node(id, 0, 0, [("v", v.as_str())], None);
        }
        let bytes = bb.take().expect("encode").expect("non-empty block");
        payloads.push(u64::try_from(bytes.len()).expect("length"));
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush fixture");
    payloads
}

/// Every node of `path` in file order: (id, its `v` tag).
fn nodes_with_value(path: &std::path::Path) -> Vec<(i64, String)> {
    read_all_elements(path)
        .nodes
        .into_iter()
        .map(|(id, tags)| {
            let v = tags
                .into_iter()
                .find(|(k, _)| k == "v")
                .map(|(_, v)| v)
                .unwrap_or_else(|| panic!("node {id} lost its tag"));
            (id, v)
        })
        .collect()
}

/// The length of the largest frame of `path`, prefix and header included.
fn largest_frame(path: &std::path::Path) -> u64 {
    let len = std::fs::metadata(path).expect("stat output").len();
    let starts: Vec<u64> = frames(path).into_iter().map(|(start, _)| start).collect();
    starts
        .iter()
        .zip(starts.iter().skip(1).chain([&len]))
        .map(|(start, end)| end - start)
        .max()
        .expect("a frame")
}

/// Planet merged with itself, at planet density: two inputs, each one node
/// blob of planet's elements per blob, the same nodes in both. They are one
/// overlap run whose two cursors open before any element is emitted, so the
/// run holds both blocks for its whole length (a dense overlap is not
/// demoted), while its output, about thirty blocks, streams out in more
/// chunks than the run's channel holds. Every chunk stays within the chunk
/// budget, and completed output waits on the sweep's side for one block at
/// most: shipping only at the end of the run, or keeping filled blocks
/// across groups, would hold the whole run there.
///
/// One run is swept by one worker, whatever the host; the worker-count
/// comparison is `overlap_runs_write_the_same_bytes_on_one_worker_and_many`.
#[test]
fn a_planet_density_self_merge_streams_its_run_in_bounded_chunks() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    let ids: Vec<i64> = (1..=PLANET_BLOB_ELEMENTS).collect();
    let value = |id: i64| format!("{id:040}");
    let payload_a = write_node_blobs(&a, true, std::slice::from_ref(&ids), value);
    let payload_b = write_node_blobs(&b, true, std::slice::from_ref(&ids), value);

    let (host, counters) = recording_host(4);
    let stats = merge_pbf(
        &[a.as_path(), b.as_path()],
        crate::common::out(&output),
        &uncompressed_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &host,
    )
    .unwrap_or_else(|e| panic!("merge: {}", e.report()));
    let n = u64::try_from(PLANET_BLOB_ELEMENTS).expect("count");
    assert_eq!(stats.nodes, n);
    assert_eq!(stats.duplicates_removed, n);
    assert_eq!(stats.blobs_rewritten, 2);
    let expected: Vec<(i64, String)> = ids.iter().map(|&id| (id, value(id))).collect();
    assert!(
        nodes_with_value(&output) == expected,
        "the merge is not the input's nodes in order"
    );

    // Every frame is far below a chunk, so none travels alone and every
    // chunk is a packed one; and the run's output is more than its channel
    // holds in budget-sized chunks.
    let largest = largest_frame(&output);
    assert!(
        largest * 2 < CHUNK_BYTES,
        "fixture: an output frame of {largest} bytes is near a chunk"
    );
    let output_len = std::fs::metadata(&output).expect("stat output").len();
    assert!(
        output_len > (CHANNEL_DEPTH + 1) * CHUNK_BYTES,
        "fixture: {output_len} bytes of output fit the run's channel"
    );
    let chunk_high = counters.get("overlap_run_chunk_high_water_bytes");
    assert!(
        chunk_high <= CHUNK_BYTES,
        "a chunk of {chunk_high} bytes exceeds the chunk budget"
    );
    let chunks = counters.get("overlap_run_chunks");
    assert!(
        chunks > CHANNEL_DEPTH,
        "the run's output went out in {chunks} chunks"
    );
    let staged = counters.get("overlap_run_staged_high_water_bytes");
    assert!(
        staged <= CHUNK_BYTES + largest,
        "{staged} bytes of completed output were held before shipping"
    );
    assert_eq!(
        counters.get("overlap_run_pinned_high_water_bytes"),
        payload_a[0] + payload_b[0],
        "the run's charge is both blocks"
    );
    assert_eq!(counters.get("overlap_run_demoted_cursors"), 0);
}

/// Output frames larger than a chunk: long tag values, written uncompressed,
/// make every output block a few chunks' worth. Such a frame is sent in
/// pieces within the chunk budget, never as one message of its own size,
/// and the pieces rejoin into the same valid file.
#[test]
fn output_frames_larger_than_a_chunk_travel_in_budget_sized_pieces() {
    const NODES: i64 = 16_000;
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    let ids: Vec<i64> = (1..=NODES).collect();
    let value = |id: i64| format!("{id:0600}");
    write_node_blobs(&a, true, std::slice::from_ref(&ids), value);
    write_node_blobs(&b, true, std::slice::from_ref(&ids), value);

    let (host, counters) = recording_host(4);
    let stats = merge_pbf(
        &[a.as_path(), b.as_path()],
        crate::common::out(&output),
        &uncompressed_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &host,
    )
    .unwrap_or_else(|e| panic!("merge: {}", e.report()));
    let n = u64::try_from(NODES).expect("count");
    assert_eq!(stats.nodes, n);
    assert_eq!(stats.duplicates_removed, n);
    let expected: Vec<(i64, String)> = ids.iter().map(|&id| (id, value(id))).collect();
    assert!(
        nodes_with_value(&output) == expected,
        "the merge is not the input's nodes in order"
    );

    let largest = largest_frame(&output);
    assert!(
        largest > 2 * CHUNK_BYTES,
        "fixture: the largest output frame, {largest} bytes, spans under two chunks"
    );
    let chunk_high = counters.get("overlap_run_chunk_high_water_bytes");
    assert!(
        chunk_high <= CHUNK_BYTES,
        "a chunk of {chunk_high} bytes exceeds the chunk budget"
    );
}

/// Sparse tails: one input whose blobs each hold a dense run of IDs plus one
/// ID far past every other blob's, so all of them chain into one overlap run
/// and each cursor is down to its tail when the next blob opens. Long tag
/// values make each block a few MiB, so keeping every mostly consumed block
/// would pin well past the sweep's budget; demotion keeps the charge within
/// one block of it.
#[test]
fn sparse_tails_are_demoted_within_the_pinned_budget() {
    const BLOBS: i64 = 12;
    const DENSE: i64 = 8_000;
    const TAIL: i64 = 1 << 40;
    let dir = crate::common::test_scratch();
    let input = dir.path().join("tails.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    let blobs: Vec<Vec<i64>> = (0..BLOBS)
        .map(|blob| {
            let base = 1 + blob * DENSE;
            (base..base + DENSE).chain([TAIL + blob]).collect()
        })
        .collect();
    let payloads = write_node_blobs(&input, true, &blobs, |id| format!("{id:0>1000}"));
    let largest = *payloads.iter().max().expect("a blob");
    // The bound's preconditions: each block is under half the budget, so a
    // scan that demotes every older cursor leaves only the newest pinned and
    // the next threshold is the budget again; and the blocks together exceed
    // the bound, so a run that never demoted would break it.
    assert!(
        largest < PINNED_BYTES / 2,
        "fixture: a {largest}-byte block"
    );
    assert!(
        payloads.iter().sum::<u64>() > PINNED_BYTES + largest,
        "fixture: the blocks fit the budget without demotion"
    );

    let (host, counters) = recording_host(4);
    let stats = merge_pbf(
        &[input.as_path()],
        crate::common::out(&output),
        &uncompressed_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &host,
    )
    .unwrap_or_else(|e| panic!("merge: {}", e.report()));
    let expected: Vec<i64> = (1..=BLOBS * DENSE).chain(TAIL..TAIL + BLOBS).collect();
    assert_eq!(stats.nodes, u64::try_from(expected.len()).expect("count"));
    assert_eq!(stats.duplicates_removed, 0);
    assert_eq!(node_ids(&read_all_elements(&output)), expected);

    assert!(
        counters.get("overlap_run_demoted_cursors") > 0,
        "no cursor was demoted"
    );
    let pinned = counters.get("overlap_run_pinned_high_water_bytes");
    assert!(
        pinned <= PINNED_BYTES + largest,
        "the run's charge reached {pinned} bytes"
    );
}

/// An internally unsorted blob is merged through views sorted by ID, one
/// `Element` per element, which at planet density outweigh the decoded block
/// itself; the run's charge counts them.
#[test]
fn sorted_views_are_charged_to_the_run() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("shuffled.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    // A fixed permutation of 1..=N (7919 is prime and does not divide N),
    // so the blob descends; no header claim, so `sort` scans it and finds
    // it out of order.
    let n = PLANET_BLOB_ELEMENTS;
    let ids: Vec<i64> = (0..n).map(|i| (i * 7_919) % n + 1).collect();
    let payload = write_node_blobs(&input, false, std::slice::from_ref(&ids), |id| {
        id.to_string()
    })[0];
    let views =
        u64::try_from(ids.len() * std::mem::size_of::<pbfhogg::Element<'static>>()).expect("size");
    assert!(
        payload < views,
        "fixture: the {payload}-byte block outweighs its views"
    );

    let (host, counters) = recording_host(4);
    let stats = pbfhogg::commands::sort::sort(
        input.as_path(),
        crate::common::out(&output),
        &uncompressed_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &host,
    )
    .unwrap_or_else(|e| panic!("sort: {}", e.report()));
    assert_eq!(stats.nodes, u64::try_from(n).expect("count"));
    assert!(
        node_ids(&read_all_elements(&output)).into_iter().eq(1..=n),
        "the sort is not 1..=N"
    );
    let pinned = counters.get("overlap_run_pinned_high_water_bytes");
    assert!(
        pinned >= payload + views,
        "the run's charge of {pinned} bytes leaves out its sorted views"
    );
}

/// One node in very many versions, the same versions in both inputs: in
/// reverse order in the first, ascending in the second (a blob may hold one
/// ID's versions in any order). The merge writes every version once, in
/// version order, the first input's copy surviving each; the group spans
/// many output blocks and ships them one at a time, so completed output
/// waits for one block at most, and the dedupe lookup is not quadratic in
/// the versions (a scan of the group per copy would not finish here).
#[test]
fn a_many_version_group_ships_block_by_block() {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, Metadata};
    use pbfhogg::writer::{Compression, PbfWriter};
    const VERSIONS: i32 = 100_000;
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    let write = |path: &std::path::Path, input: &str, versions: &[i32]| {
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::None);
        writer
            .write_header(&HeaderBuilder::new().sorted().build().expect("header"))
            .expect("write header");
        let cap = std::num::NonZeroUsize::new(versions.len()).expect("versions");
        let mut bb = BlockBuilder::with_element_cap_prealloc(cap, 8_000);
        for &version in versions {
            let v = format!("{input}{version:040}");
            let meta = Metadata {
                version,
                timestamp: 1,
                changeset: 1,
                uid: 1,
                user: "u",
                visible: true,
            };
            bb.add_node(1, 0, 0, [("v", v.as_str())], Some(&meta));
        }
        let bytes = bb.take().expect("encode").expect("non-empty block");
        writer.write_primitive_block(bytes).expect("write block");
        writer.flush().expect("flush fixture");
    };
    let ascending: Vec<i32> = (1..=VERSIONS).collect();
    let descending: Vec<i32> = ascending.iter().rev().copied().collect();
    write(&a, "a", &descending);
    write(&b, "b", &ascending);

    let (host, counters) = recording_host(4);
    let stats = merge_pbf(
        &[a.as_path(), b.as_path()],
        crate::common::out(&output),
        &uncompressed_opts(),
        &pbfhogg::HeaderOverrides::default(),
        &host,
    )
    .unwrap_or_else(|e| panic!("merge: {}", e.report()));
    let n = u64::try_from(VERSIONS).expect("count");
    assert_eq!(stats.nodes, n);
    assert_eq!(stats.duplicates_removed, n);
    let expected: Vec<(i64, String)> = ascending
        .iter()
        .map(|version| (1, format!("a{version:040}")))
        .collect();
    assert!(
        nodes_with_value(&output) == expected,
        "the merge is not every version once, in order, from the first input"
    );
    let largest = largest_frame(&output);
    let output_len = std::fs::metadata(&output).expect("stat output").len();
    assert!(
        output_len > 2 * (CHUNK_BYTES + largest),
        "fixture: the group's {output_len} bytes of output fit one ship"
    );
    let staged = counters.get("overlap_run_staged_high_water_bytes");
    assert!(
        staged <= CHUNK_BYTES + largest,
        "{staged} bytes of completed output were held before shipping"
    );
}
