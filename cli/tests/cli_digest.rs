//! CLI-driven tests for `pbfhogg digest`, and for the `diff` behaviour its
//! contract depends on.
//!
//! The contract: on every input the digest accepts, two files get the same
//! digest exactly when `diff --suppress-common` reports them identical, on
//! every diff path; everything else is refused with no digest printed. The
//! agreement test below is the drift alarm - it fails if diff's notion of
//! element equality and the digest's canonical form ever disagree.

#![allow(clippy::unwrap_used)]
#![allow(clippy::too_many_lines)]

use std::path::Path;

use crate::common::adversarial::{locate_blobs, mutate_blob_header_indexdata, mutate_blob_payload};
use crate::common::{
    TestMember, TestMeta, TestNode, TestRelation, TestWay, generate_nodes,
    generate_nodes_with_negatives, write_multi_block_test_pbf, write_test_pbf,
    write_test_pbf_non_indexed, write_test_pbf_sorted,
};
use crate::invoker::{CliInvoker, CliOutput};
use pbfhogg::MemberId;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The digest report lines, or the failed run.
fn digest(path: &Path) -> Result<Vec<String>, CliOutput> {
    let out = CliInvoker::new().arg("digest").arg(path).run();
    if !out.status.success() {
        return Err(out);
    }
    let lines: Vec<String> = out
        .stdout_str()
        .lines()
        .filter(|l| {
            l.starts_with("xxh128 ")
                || l.starts_with("node ")
                || l.starts_with("way ")
                || l.starts_with("relation ")
        })
        .map(str::to_owned)
        .collect();
    assert_eq!(
        lines.len(),
        4,
        "digest report must be four lines; stdout:\n{}",
        out.stdout_str()
    );
    Ok(lines)
}

fn digest_ok(path: &Path) -> Vec<String> {
    digest(path).unwrap_or_else(|out| {
        panic!(
            "digest of {} failed (exit {:?}); stderr:\n{}",
            path.display(),
            out.status.code(),
            out.stderr_str()
        )
    })
}

/// Assert the digest refuses `path`: non-zero exit and no digest line.
fn assert_refused(path: &Path, why: &str) {
    let out = CliInvoker::new().arg("digest").arg(path).run();
    assert!(
        !out.status.success(),
        "digest must refuse {why}; it printed:\n{}",
        out.stdout_str()
    );
    assert!(
        !out.stdout_str().contains("xxh128 "),
        "a refused digest must print no digest line ({why}); stdout:\n{}",
        out.stdout_str()
    );
}

/// `diff --quiet` exit code: 0 identical, 1 different, 2 error. (The verdict
/// is the same with or without `--suppress-common`, which `--quiet` refuses
/// as an output-only flag.)
fn diff_code(old: &Path, new: &Path, jobs: Option<usize>) -> Option<i32> {
    let mut cli = CliInvoker::new()
        .arg("diff")
        .arg(old)
        .arg(new)
        .arg("--quiet");
    if let Some(j) = jobs {
        cli = cli.arg("-j").arg(j.to_string());
    }
    cli.run().status.code()
}

fn node(id: i64, lat: i32, lon: i32, tags: Vec<(&'static str, &'static str)>) -> TestNode {
    TestNode {
        id,
        lat,
        lon,
        tags,
        meta: None,
    }
}

fn way(id: i64, refs: Vec<i64>, tags: Vec<(&'static str, &'static str)>) -> TestWay {
    TestWay {
        id,
        refs,
        tags,
        meta: None,
    }
}

fn member(id: MemberId, role: &'static str) -> TestMember {
    TestMember { id, role }
}

fn relation(
    id: i64,
    members: Vec<TestMember>,
    tags: Vec<(&'static str, &'static str)>,
) -> TestRelation {
    TestRelation {
        id,
        members,
        tags,
        meta: None,
    }
}

struct Fixture {
    nodes: Vec<TestNode>,
    ways: Vec<TestWay>,
    relations: Vec<TestRelation>,
}

fn base_fixture() -> Fixture {
    Fixture {
        nodes: vec![
            node(
                1,
                100_000_000,
                200_000_000,
                vec![("name", "a"), ("shop", "bakery")],
            ),
            node(2, 110_000_000, 210_000_000, vec![]),
            node(3, 120_000_000, 220_000_000, vec![("barrier", "gate")]),
        ],
        ways: vec![way(
            10,
            vec![1, 2, 3],
            vec![("highway", "primary"), ("name", "x")],
        )],
        relations: vec![relation(
            20,
            vec![
                member(MemberId::Way(10), "outer"),
                member(MemberId::Node(1), "label"),
            ],
            vec![("type", "multipolygon")],
        )],
    }
}

/// Write `fixture` in the requested encoding.
fn write(path: &Path, f: &Fixture, indexed: bool) {
    if indexed {
        write_test_pbf_sorted(path, &f.nodes, &f.ways, &f.relations);
    } else {
        write_test_pbf_non_indexed(path, &f.nodes, &f.ways, &f.relations);
    }
}

// ---------------------------------------------------------------------------
// The contract: digest equality agrees with diff
// ---------------------------------------------------------------------------

/// For every variant of a base fixture, digest equality must match diff's
/// verdict, for both encodings and for the sequential and parallel diff
/// paths. Variants cover every content field (expected different) and
/// every non-content property (expected equal). Adding a field to diff's
/// comparison, or to the digest's canonical form, without the other turns
/// one of these red.
#[test]
fn digest_equality_agrees_with_diff_on_every_field() {
    type Mutate = fn(&mut Fixture);
    let variants: Vec<(&str, bool, Mutate)> = vec![
        ("identical", true, |_| {}),
        ("metadata only", true, |f| {
            let meta = Some(TestMeta {
                version: 7,
                timestamp: 1_700_000_000,
                changeset: 99,
                uid: 5,
                user: "someone",
                visible: true,
            });
            f.nodes[0].meta = meta;
            f.ways[0].meta = meta;
            f.relations[0].meta = meta;
        }),
        ("node latitude", false, |f| f.nodes[1].lat += 1),
        ("node longitude", false, |f| f.nodes[1].lon -= 1),
        ("node tag value", false, |f| {
            f.nodes[0].tags[1].1 = "butcher";
        }),
        ("node tag key", false, |f| f.nodes[0].tags[1].0 = "amenity"),
        ("node tag order", false, |f| f.nodes[0].tags.swap(0, 1)),
        ("node tag added", false, |f| {
            f.nodes[1].tags.push(("k", "v"));
        }),
        ("node tag removed", false, |f| {
            f.nodes[2].tags.clear();
        }),
        ("node id", false, |f| f.nodes[2].id = 4),
        ("node added", false, |f| f.nodes.push(node(5, 1, 1, vec![]))),
        ("way ref", false, |f| f.ways[0].refs[1] = 3),
        ("way ref order", false, |f| f.ways[0].refs.reverse()),
        ("way ref dropped", false, |f| {
            f.ways[0].refs.pop();
        }),
        ("way tag value", false, |f| {
            f.ways[0].tags[0].1 = "secondary";
        }),
        ("relation role", false, |f| {
            f.relations[0].members[0].role = "inner";
        }),
        ("relation member type", false, |f| {
            f.relations[0].members[1].id = MemberId::Way(1);
        }),
        ("relation member id", false, |f| {
            f.relations[0].members[0].id = MemberId::Way(11);
        }),
        ("relation member order", false, |f| {
            f.relations[0].members.swap(0, 1);
        }),
        ("relation tag", false, |f| {
            f.relations[0].tags[0].1 = "route";
        }),
        ("relation removed", false, |f| f.relations.clear()),
    ];

    let dir = TempDir::new().expect("tempdir");
    for indexed in [true, false] {
        let base = dir.path().join(format!("base-{indexed}.osm.pbf"));
        write(&base, &base_fixture(), indexed);
        let base_digest = digest_ok(&base);

        for (i, (name, expect_equal, mutate)) in variants.iter().enumerate() {
            let mut fixture = base_fixture();
            mutate(&mut fixture);
            let path = dir.path().join(format!("variant-{i}-{indexed}.osm.pbf"));
            write(&path, &fixture, indexed);

            let digest_equal = digest_ok(&path)[0] == base_digest[0];
            assert_eq!(
                digest_equal, *expect_equal,
                "{name} (indexed={indexed}): digest equality"
            );
            for jobs in [Some(1), None] {
                let code = diff_code(&base, &path, jobs);
                assert_eq!(
                    code,
                    Some(if digest_equal { 0 } else { 1 }),
                    "{name} (indexed={indexed}, jobs={jobs:?}): diff disagrees with digest"
                );
            }
        }
    }
}

/// Digest equality also has to agree with diff across encodings: an indexed
/// file against a non-indexed one goes through diff's element-stream path.
#[test]
fn digest_is_independent_of_encoding() {
    let dir = TempDir::new().expect("tempdir");
    let f = base_fixture();
    let indexed = dir.path().join("indexed.osm.pbf");
    let plain = dir.path().join("plain.osm.pbf");
    let blocky = dir.path().join("blocky.osm.pbf");
    write(&indexed, &f, true);
    write(&plain, &f, false);
    // One element per block: completely different blob boundaries.
    write_multi_block_test_pbf(&blocky, &f.nodes, &f.ways, &f.relations, 1);

    let a = digest_ok(&indexed);
    assert_eq!(
        a,
        digest_ok(&plain),
        "indexdata presence changed the digest"
    );
    assert_eq!(a, digest_ok(&blocky), "blob boundaries changed the digest");
    assert_eq!(diff_code(&indexed, &plain, None), Some(0));
    assert_eq!(diff_code(&indexed, &blocky, None), Some(0));

    assert!(
        a[0].starts_with("xxh128 v1 "),
        "first line names algorithm and version: {}",
        a[0]
    );
    assert!(a[1].starts_with("node 3 "), "node line: {}", a[1]);
    assert!(a[2].starts_with("way 1 "), "way line: {}", a[2]);
    assert!(a[3].starts_with("relation 1 "), "relation line: {}", a[3]);
}

/// A change to one kind moves that kind's line and the overall line only.
#[test]
fn per_kind_lines_isolate_the_changed_kind() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let changed = dir.path().join("changed.osm.pbf");
    write(&base, &base_fixture(), true);
    let mut f = base_fixture();
    f.ways[0].tags[0].1 = "tertiary";
    write(&changed, &f, true);

    let a = digest_ok(&base);
    let b = digest_ok(&changed);
    assert_ne!(a[0], b[0], "overall digest must change");
    assert_eq!(a[1], b[1], "node line must not change");
    assert_ne!(a[2], b[2], "way line must change");
    assert_eq!(a[3], b[3], "relation line must not change");
}

/// A sorted file with no data blocks is valid and has the empty digest.
#[test]
fn header_only_file_has_the_empty_digest() {
    let dir = TempDir::new().expect("tempdir");
    let empty = dir.path().join("empty.osm.pbf");
    write_test_pbf_sorted(&empty, &[], &[], &[]);
    let d = digest_ok(&empty);
    assert!(d[1].starts_with("node 0 "), "{}", d[1]);
    assert!(d[2].starts_with("way 0 "), "{}", d[2]);
    assert!(d[3].starts_with("relation 0 "), "{}", d[3]);
}

// ---------------------------------------------------------------------------
// Refusals: inputs on which diff's answer is not a function of content
// ---------------------------------------------------------------------------

#[test]
fn refuses_unsorted_header() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("unsorted.osm.pbf");
    let f = base_fixture();
    write_test_pbf(&path, &f.nodes, &f.ways, &f.relations);
    assert_refused(
        &path,
        "a file whose header does not claim Sort.Type_then_ID",
    );
}

#[test]
fn refuses_empty_and_headerless_input() {
    let dir = TempDir::new().expect("tempdir");
    let zero = dir.path().join("zero.osm.pbf");
    std::fs::write(&zero, b"").expect("write");
    assert_refused(&zero, "a zero-byte file");

    let sorted = dir.path().join("sorted.osm.pbf");
    write(&sorted, &base_fixture(), true);
    let bytes = std::fs::read(&sorted).expect("read");
    let frames = locate_blobs(&bytes);
    let headerless = dir.path().join("headerless.osm.pbf");
    std::fs::write(&headerless, &bytes[frames[1].frame_start..]).expect("write");
    assert_refused(&headerless, "a file whose first blob is not an OsmHeader");
}

#[test]
fn refuses_a_second_header_blob() {
    let dir = TempDir::new().expect("tempdir");
    let sorted = dir.path().join("sorted.osm.pbf");
    write(&sorted, &base_fixture(), true);
    let bytes = std::fs::read(&sorted).expect("read");
    let frames = locate_blobs(&bytes);
    let header = &bytes[frames[0].frame_start..frames[0].blob_end];
    for insertion in [frames[0].blob_end, bytes.len()] {
        let doubled = [&bytes[..insertion], header, &bytes[insertion..]].concat();
        let path = dir.path().join(format!("two-headers-{insertion}.osm.pbf"));
        std::fs::write(&path, doubled).expect("write");
        assert_refused(&path, "a file with two OsmHeader blobs");
    }
}

#[test]
fn refuses_duplicate_and_descending_ids() {
    let dir = TempDir::new().expect("tempdir");

    // Within one block.
    let dup = dir.path().join("dup.osm.pbf");
    write_test_pbf_sorted(
        &dup,
        &[node(1, 0, 0, vec![]), node(1, 0, 0, vec![])],
        &[],
        &[],
    );
    assert_refused(&dup, "a duplicate node id inside one block");

    // Across a block boundary.
    let desc = dir.path().join("desc.osm.pbf");
    write_multi_block_test_pbf(
        &desc,
        &[node(5, 0, 0, vec![]), node(4, 0, 0, vec![])],
        &[],
        &[],
        1,
    );
    assert_refused(&desc, "a node id descending across a block boundary");
}

#[test]
fn refuses_partial_indexdata() {
    let dir = TempDir::new().expect("tempdir");
    let sorted = dir.path().join("sorted.osm.pbf");
    write(&sorted, &base_fixture(), true);
    let bytes = std::fs::read(&sorted).expect("read");
    // Blob 2 is the way blob; an empty indexdata field is unreadable, which
    // every reader treats as absent.
    let partial = mutate_blob_header_indexdata(&bytes, 2, Vec::clear);
    let path = dir.path().join("partial.osm.pbf");
    std::fs::write(&path, partial).expect("write");
    assert_refused(&path, "indexdata on some data blobs and not others");
}

/// Diff's block-pair engine trusts claimed ranges: a node claimed as id 2
/// is decided "entirely after" a real id 1 and reported deleted plus
/// created. The digest refuses any claim that disagrees with the block.
#[test]
fn refuses_indexdata_that_misdescribes_its_block() {
    let dir = TempDir::new().expect("tempdir");
    let honest = dir.path().join("honest.osm.pbf");
    write_test_pbf_sorted(&honest, &[node(1, 0, 0, vec![])], &[], &[]);
    let bytes = std::fs::read(&honest).expect("read");
    let lying = mutate_blob_header_indexdata(&bytes, 1, |ix| {
        assert_eq!(ix.len(), 42, "fixture blob must carry a v3 index");
        ix[2..10].copy_from_slice(&2i64.to_le_bytes());
        ix[10..18].copy_from_slice(&2i64.to_le_bytes());
    });
    let path = dir.path().join("lying.osm.pbf");
    std::fs::write(&path, lying).expect("write");

    digest_ok(&honest);
    assert_refused(&path, "an index claiming ids the block does not hold");
}

/// Drop the last string-table entry of a PrimitiveBlock payload, so the
/// highest string index in the block points past the table.
fn drop_last_string(payload: &mut Vec<u8>) {
    fn read_varint(buf: &[u8], pos: &mut usize) -> usize {
        let mut value = 0usize;
        let mut shift = 0;
        loop {
            let b = buf[*pos];
            *pos += 1;
            value |= usize::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return value;
            }
            shift += 7;
        }
    }
    fn write_varint(out: &mut Vec<u8>, mut v: usize) {
        while v >= 0x80 {
            out.push(u8::try_from(v & 0x7f).unwrap() | 0x80);
            v >>= 7;
        }
        out.push(u8::try_from(v).unwrap());
    }

    assert_eq!(
        payload[0], 0x0A,
        "PrimitiveBlock must start with its string table"
    );
    let mut pos = 1;
    let table_len = read_varint(payload, &mut pos);
    let table_start = pos;
    let table_end = table_start + table_len;
    let mut last_entry = table_start;
    let mut cursor = table_start;
    while cursor < table_end {
        last_entry = cursor;
        assert_eq!(payload[cursor], 0x0A, "string table entry tag");
        cursor += 1;
        let len = read_varint(payload, &mut cursor);
        cursor += len;
    }
    let mut rebuilt = vec![0x0A];
    write_varint(&mut rebuilt, last_entry - table_start);
    rebuilt.extend_from_slice(&payload[table_start..last_entry]);
    rebuilt.extend_from_slice(&payload[table_end..]);
    *payload = rebuilt;
}

#[test]
fn refuses_an_unresolvable_string_reference() {
    let dir = TempDir::new().expect("tempdir");
    let sorted = dir.path().join("sorted.osm.pbf");
    // A way-only file: blob 1 is the way block, whose last-interned string
    // is the value of its last tag.
    write_test_pbf_non_indexed(
        &sorted,
        &[],
        &[way(10, vec![1, 2], vec![("highway", "primary")])],
        &[],
    );
    let bytes = std::fs::read(&sorted).expect("read");
    let broken = mutate_blob_payload(&bytes, 1, drop_last_string);
    let path = dir.path().join("broken.osm.pbf");
    std::fs::write(&path, broken).expect("write");
    assert_refused(&path, "a tag value index past the string table");

    // Diff no longer calls the corrupt file identical to itself.
    assert_ne!(diff_code(&path, &path, None), Some(0));
}

// ---------------------------------------------------------------------------
// Diff fixes the contract depends on
// ---------------------------------------------------------------------------

/// An empty PrimitiveGroup next to a way group used to make the block
/// "mixed", which the streaming merge refuses. Empty groups are neutral.
#[test]
fn empty_group_beside_ways_is_not_a_mixed_block() {
    let dir = TempDir::new().expect("tempdir");
    let sorted = dir.path().join("sorted.osm.pbf");
    write_test_pbf_non_indexed(
        &sorted,
        &[],
        &[way(10, vec![1, 2], vec![("highway", "primary")])],
        &[],
    );
    let bytes = std::fs::read(&sorted).expect("read");
    let with_empty_group = mutate_blob_payload(&bytes, 1, |p| p.extend_from_slice(&[0x12, 0x00]));
    let path = dir.path().join("empty-group.osm.pbf");
    std::fs::write(&path, with_empty_group).expect("write");

    assert_eq!(diff_code(&path, &sorted, None), Some(0));
    assert_eq!(digest_ok(&path), digest_ok(&sorted));
}

/// An empty `DenseNodes` block between the ways and the relations classifies
/// as a node block. The streaming merge used to stash it during the way
/// phase, then stop the relation phase at it, losing every relation after
/// it - while the digest, rightly, saw the same elements on both sides.
#[test]
fn empty_dense_block_between_kinds_loses_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let without = dir.path().join("without.osm.pbf");
    write_test_pbf_non_indexed(
        &without,
        &[],
        &[way(1, vec![1, 2], vec![])],
        &[relation(1, vec![member(MemberId::Way(1), "")], vec![])],
    );
    let bytes = std::fs::read(&without).expect("read");
    let frames = locate_blobs(&bytes);
    assert_eq!(frames.len(), 3, "header, way block, relation block");

    // A raw blob whose PrimitiveBlock is a one-entry string table and one
    // group holding an empty DenseNodes message.
    let empty_dense = mutate_blob_payload(&bytes, 1, |p| {
        *p = vec![0x0A, 0x02, 0x0A, 0x00, 0x12, 0x02, 0x12, 0x00];
    });
    let empty_frames = locate_blobs(&empty_dense);
    let empty_frame = &empty_dense[empty_frames[1].frame_start..empty_frames[1].blob_end];

    let mut spliced = bytes[..frames[2].frame_start].to_vec();
    spliced.extend_from_slice(empty_frame);
    spliced.extend_from_slice(&bytes[frames[2].frame_start..]);
    let with = dir.path().join("with.osm.pbf");
    std::fs::write(&with, spliced).expect("write");

    assert_eq!(diff_code(&with, &without, Some(1)), Some(0));
    assert_eq!(digest_ok(&with), digest_ok(&without));
}

/// The negative-ID fallback covers the whole comparison: an order-guard
/// error on one side must not preempt a negative ID on the other, or `-j`
/// changes the answer.
#[test]
fn negative_fallback_is_decided_before_either_order_check() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    write_multi_block_test_pbf(
        &old,
        &[node(2, 0, 0, vec![]), node(1, 0, 0, vec![])],
        &[],
        &[],
        1,
    );
    write_test_pbf_sorted(&new, &[node(-1, 0, 0, vec![])], &[], &[]);

    assert_eq!(diff_code(&old, &new, Some(1)), Some(1));
    assert_eq!(diff_code(&old, &new, Some(2)), Some(1));
}

/// `i64::MIN` and `-i64::MAX` used to share a sort key, so the merge-join
/// paired them as one object and, ignoring the id, reported them common.
#[test]
fn extreme_negative_ids_are_distinct_objects() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    write_test_pbf_non_indexed(&a, &[node(i64::MIN, 5, 5, vec![])], &[], &[]);
    write_test_pbf_non_indexed(&b, &[node(-i64::MAX, 5, 5, vec![])], &[], &[]);

    assert_eq!(diff_code(&a, &b, None), Some(1));
    assert_ne!(digest_ok(&a)[0], digest_ok(&b)[0]);
}

/// Indexed inputs with negative IDs take the sequential block-pair merge
/// even at the default `-j`: the parallel planner's raw-i64 thresholds do
/// not order negative IDs the way the merge does.
#[test]
fn negative_ids_diff_correctly_at_default_jobs() {
    let dir = TempDir::new().expect("tempdir");
    let nodes = generate_nodes_with_negatives(6, 6);
    let base = dir.path().join("base.osm.pbf");
    let same = dir.path().join("same.osm.pbf");
    let plain = dir.path().join("plain.osm.pbf");
    write_multi_block_test_pbf(&base, &nodes, &[], &[], 2);
    write_multi_block_test_pbf(&same, &nodes, &[], &[], 3);
    write_test_pbf_non_indexed(&plain, &nodes, &[], &[]);

    assert_eq!(diff_code(&base, &same, None), Some(0));
    assert_eq!(digest_ok(&base), digest_ok(&same));
    assert_eq!(digest_ok(&base), digest_ok(&plain));

    let mut changed_nodes = generate_nodes_with_negatives(6, 6);
    changed_nodes[3].lat += 1;
    let changed = dir.path().join("changed.osm.pbf");
    write_multi_block_test_pbf(&changed, &changed_nodes, &[], &[], 2);
    assert_eq!(diff_code(&base, &changed, None), Some(1));
    assert_ne!(digest_ok(&base)[0], digest_ok(&changed)[0]);
}

/// Large enough to give the parallel planner several shards.
#[test]
fn many_blocks_agree_with_parallel_diff() {
    let dir = TempDir::new().expect("tempdir");
    let nodes = generate_nodes(400, 1);
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    write_multi_block_test_pbf(&a, &nodes, &[], &[], 7);
    write_multi_block_test_pbf(&b, &nodes, &[], &[], 50);
    assert_eq!(diff_code(&a, &b, Some(4)), Some(0));
    assert_eq!(digest_ok(&a), digest_ok(&b));
}

// ---------------------------------------------------------------------------
// External cross-check
// ---------------------------------------------------------------------------

/// osmium's own re-encodings - plain (non-dense) nodes, no metadata, no
/// compression, no indexdata - must digest equal to the file they came from.
///
/// `#[ignore = "external"]`: external cross-validation belongs to
/// `brokkr verify` (see `reference/testing.md` "External cross-validation");
/// this fixture pins the encodings a real dataset run does not vary.
#[test]
#[ignore = "external"]
fn osmium_reencodings_digest_equal() {
    if std::process::Command::new("osmium")
        .arg("--version")
        .output()
        .is_err()
    {
        crate::common::skip("osmium not found in PATH");
        return;
    }
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("source.osm.pbf");
    let mut f = base_fixture();
    f.nodes[0].meta = Some(TestMeta::default());
    write(&source, &f, true);
    let expected = digest_ok(&source);

    for (i, format) in [
        "pbf,pbf_dense_nodes=false",
        "pbf,add_metadata=false,pbf_compression=none",
        "pbf,pbf_compression=zlib,pbf_dense_nodes=false,add_metadata=false",
    ]
    .iter()
    .enumerate()
    {
        let out = dir.path().join(format!("osmium-{i}.osm.pbf"));
        let status = std::process::Command::new("osmium")
            .arg("cat")
            .arg(&source)
            .arg("-f")
            .arg(format)
            .arg("-o")
            .arg(&out)
            .arg("--overwrite")
            .status()
            .expect("run osmium");
        assert!(status.success(), "osmium cat -f {format} failed");
        assert_eq!(digest_ok(&out), expected, "osmium -f {format}");
        assert_eq!(
            diff_code(&source, &out, None),
            Some(0),
            "diff vs osmium -f {format}"
        );
    }
}
