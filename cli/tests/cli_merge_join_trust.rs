//! The block-pair merge-join must not lose elements when a file's header
//! claims `Sort.Type_then_ID` and its blobs disagree.
//!
//! The streaming block-pair merge that `-j 1` once ran walked each element
//! kind in one pass, stashing the first blob of a later kind for the next
//! phase. A blob of an EARLIER kind turning up after that kind's phase had
//! already ended used to be `continue`d - discarded, along with every element
//! in it, with no error, no warning, and no anomaly in the reported counts.
//! A header can lie about
//! sortedness (`pbfhogg degrade --unsort` exists to produce such files), so
//! the merge cannot take the stamp as proof.
//!
//! Indexed inputs are compared by one block-pair engine at every `-j`: a
//! header walk of both whole files runs `BlobOrderGuard` in file order
//! before bucketing blobs by kind, and `-j` only changes how many ID-range
//! shards the merge runs. The unindexed element stream, which has no blob
//! ranges, classifies every block by its elements and checks element order
//! as it converts.
//!
//! Every rejection test here still runs at BOTH `-j 1` and a sharded `-j`.
//! That is history, not thoroughness: `-j 1` used to run a separate
//! streaming engine, and that divergence is how this bug hid. Before the
//! guard, `pbfhogg diff` returned exit 2 with "not in canonical order" at
//! `-j 1` and exit 0 with "Files are identical" at the default `-j`, on the
//! same pair of files - and the suite's one seq-vs-parallel parity test used
//! an honestly-sorted fixture, so it had no power against the only case
//! where the two paths disagreed.
//!
//! The fixtures here are written with `BlockBuilder` + `PbfWriter::to_path`
//! directly rather than through `tests/common`, because the point is to lay
//! blobs out in an order the shared helpers deliberately cannot produce.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use crate::invoker::CliInvoker;
use pbfhogg::MemberId;
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::writer::{Compression, PbfWriter};

/// What one blob in a hand-laid fixture holds.
enum Blob {
    Nodes(&'static [i64]),
    Ways(&'static [i64]),
    Relations(&'static [i64]),
}

/// Write a PBF whose header stamps `Sort.Type_then_ID` and whose blobs are
/// laid out exactly as `blobs` says, honest or not. Uses `to_path` so every
/// OsmData blob carries indexdata, which `block_pair_merge_phase` requires.
fn write_blobs(path: &Path, blobs: &[Blob]) {
    let header = HeaderBuilder::new().sorted().build().expect("build header");
    let mut writer = PbfWriter::to_path(
        path,
        Compression::default(),
        &header,
        pbfhogg::host::Executor::Ambient,
    )
    .expect("create writer");
    fill_blobs(blobs, |bytes| {
        writer.write_primitive_block(bytes).expect("write block");
    });
    writer.flush().expect("flush");
}

/// [`write_blobs`] without indexdata on any data blob, so `diff` takes the
/// unindexed element stream.
fn write_blobs_unindexed(path: &Path, blobs: &[Blob]) {
    let header = HeaderBuilder::new().sorted().build().expect("build header");
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer.write_header(&header).expect("write header");
    fill_blobs(blobs, |bytes| {
        writer
            .write_primitive_block_no_indexdata(bytes)
            .expect("write block");
    });
    writer.flush().expect("flush");
}

/// Encode each entry of `blobs` as one block and hand it to `write`.
fn fill_blobs(blobs: &[Blob], mut write: impl FnMut(&[u8])) {
    let mut bb = BlockBuilder::new();

    for blob in blobs {
        match blob {
            Blob::Nodes(ids) => {
                for &id in *ids {
                    let lat = 100_000_000 + i32::try_from(id).expect("small test id");
                    bb.add_node(id, lat, 200_000_000, [], None);
                }
            }
            Blob::Ways(ids) => {
                for &id in *ids {
                    bb.add_way(id, [], &[1, 2], None);
                }
            }
            Blob::Relations(ids) => {
                for &id in *ids {
                    let members = [MemberData {
                        id: MemberId::Node(1),
                        role: "label",
                    }];
                    bb.add_relation(id, [], &members, None);
                }
            }
        }
        // One `take()` per entry in `blobs` - this is what puts each group
        // in its own blob, in the order given.
        if let Some(bytes) = bb.take().expect("take") {
            write(bytes);
        }
    }
}

/// Run `pbfhogg diff` at an explicit job count and return (exit code,
/// stderr). `jobs` is always spelled out: see the module doc for why a
/// default-`-j`-only test proved nothing here.
fn run_diff_jobs(old: &Path, new: &Path, jobs: &str) -> (Option<i32>, String) {
    run_diff_args(old, new, jobs, &[])
}

/// [`run_diff_jobs`] with extra arguments (a type filter).
fn run_diff_args(old: &Path, new: &Path, jobs: &str, args: &[&str]) -> (Option<i32>, String) {
    let mut invoker = CliInvoker::new()
        .arg("diff")
        .arg(old)
        .arg(new)
        .arg("-j")
        .arg(jobs);
    for arg in args {
        invoker = invoker.arg(arg);
    }
    let out = invoker.run();
    (out.status.code(), out.stderr_str())
}

/// Assert `diff` with `args` exits `code` at `-j 1` and `-j 4`, one shard
/// and up to four (an unindexed input runs the element stream at both, a
/// negative-ID one a single shard), with `needle` in stderr when given.
fn assert_both_paths(old: &Path, new: &Path, args: &[&str], code: i32, needle: Option<&str>) {
    for jobs in ["1", "4"] {
        let (got, stderr) = run_diff_args(old, new, jobs, args);
        assert_eq!(
            got,
            Some(code),
            "-j {jobs} {args:?}: expected exit {code}; stderr:\n{stderr}"
        );
        if let Some(needle) = needle {
            assert!(
                stderr.contains(needle),
                "-j {jobs} {args:?}: expected {needle:?} in stderr; got:\n{stderr}"
            );
        }
    }
}

/// One shard only, for the assertions that check a message in detail.
fn run_diff(old: &Path, new: &Path) -> (Option<i32>, String) {
    run_diff_jobs(old, new, "1")
}

/// Assert `diff` refuses `old` vs `new` with an ordering complaint at
/// `-j 1` and `-j 4`. The contract is the verdict, not the sentence.
fn assert_both_paths_refuse(old: &Path, new: &Path) {
    for jobs in ["1", "4"] {
        let (code, stderr) = run_diff_jobs(old, new, jobs);
        assert_eq!(
            code,
            Some(2),
            "-j {jobs} must refuse an out-of-order blob layout; stderr:\n{stderr}"
        );
        assert!(
            stderr.contains("blobs are not in canonical order"),
            "-j {jobs} error should name the ordering problem; got:\n{stderr}"
        );
    }
}

/// A node blob after the way blobs is silently dropped by the old
/// `continue`: the node phase has already ended, the way phase does not want
/// it, and no later phase ever sees it again.
#[test]
fn node_blob_after_way_blobs_is_rejected_not_dropped() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let liar = dir.path().join("liar.osm.pbf");

    write_blobs(&honest, &[Blob::Nodes(&[1, 2, 3]), Blob::Ways(&[100, 200])]);
    // Same elements, but node 3 sits in a blob after the ways. The header
    // still stamps Sort.Type_then_ID.
    write_blobs(
        &liar,
        &[
            Blob::Nodes(&[1, 2]),
            Blob::Ways(&[100, 200]),
            Blob::Nodes(&[3]),
        ],
    );

    let (code, stderr) = run_diff(&honest, &liar);
    assert_eq!(
        code,
        Some(2),
        "an out-of-order blob must fail, not produce an answer; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("blobs are not in canonical order"),
        "error should name the ordering problem; got:\n{stderr}"
    );
    assert!(
        stderr.contains(
            "a node blob holding id 3 follows way blobs already reaching id 200 \
             earlier in this file"
        ),
        "error should name which kind turned up after which; got:\n{stderr}"
    );

    // And the same verdict at the default shard count. Before
    // `BlobOrderGuard` the parallel arm returned exit 0 and "Files are
    // identical" on exactly this input.
    assert_both_paths_refuse(&honest, &liar);
}

/// The same shape on the other side of the comparison, so the guard is not
/// accidentally one-sided (each input is walked and guarded on its own).
#[test]
fn out_of_order_blob_on_the_old_side_is_rejected_too() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let liar = dir.path().join("liar.osm.pbf");

    write_blobs(&honest, &[Blob::Nodes(&[1, 2, 3]), Blob::Ways(&[100, 200])]);
    write_blobs(
        &liar,
        &[
            Blob::Nodes(&[1, 2]),
            Blob::Ways(&[100, 200]),
            Blob::Nodes(&[3]),
        ],
    );

    // liar first this time, on both paths.
    assert_both_paths_refuse(&liar, &honest);
}

/// A blob whose stored `max_id` UNDERSTATES what the block actually holds.
///
/// ADR-0013 says metadata is trusted by default but nothing load-bearing may
/// break when it is wrong. `merge_decoded_pair` picks its merge boundary
/// from the two blobs' claimed last IDs; when a claim is short, the boundary
/// lands mid-block and the side whose claim was short is dropped along with
/// every element past the boundary - silently, with no error and no count
/// anomaly, because the count that would have disagreed comes from the same
/// lying metadata.
///
/// `BlockBuilder` derives its index from what it actually wrote and cannot
/// emit this, which is why the fix pass recorded no test here. It does not
/// need a whole protobuf emitter though: `max_id` is bytes 10..18 of the
/// indexdata blob, and `mutate_blob_header_indexdata` already rewrites those
/// bytes without touching the payload. The result is precisely a blob whose
/// sidecar metadata no longer describes its own contents.
#[test]
fn understated_max_id_is_rejected_not_silently_truncated() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let honest_new = dir.path().join("new.osm.pbf");
    let lying_new = dir.path().join("lying.osm.pbf");

    write_blobs(&old, &[Blob::Nodes(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10])]);
    write_blobs(
        &honest_new,
        &[Blob::Nodes(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10])],
    );

    // Blob 1 is the single OsmData blob (blob 0 is the OsmHeader). Claim its
    // last node is 5 while the payload still holds 1..=10.
    let bytes = std::fs::read(&honest_new).expect("read fixture");
    let forged = crate::common::adversarial::mutate_blob_header_indexdata(&bytes, 1, |ix| {
        assert_eq!(ix.len(), 42, "fixture blob must carry a v3 index");
        ix[10..18].copy_from_slice(&5i64.to_le_bytes());
    });
    std::fs::write(&lying_new, forged).expect("write forged");

    // Non-regression first: the same files with honest metadata compare
    // identical, so the rejection below is about the forged claim and not
    // about the fixture shape.
    let (code, stderr) = run_diff(&old, &honest_new);
    assert_eq!(code, Some(0), "honest fixture must diff clean; {stderr}");

    let (code, stderr) = run_diff(&old, &lying_new);
    assert_eq!(
        code,
        Some(2),
        "an understated max_id must be refused, not used to truncate the \
         block; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("does not match the blob's contents"),
        "error should say the metadata contradicts the payload; got:\n{stderr}"
    );
}

/// The same refusal inside a sharded run: the understated blob sits in the
/// middle of a multi-shard kind, with its claim inside its shard's window,
/// so the shard sees what the block holds past the claim. The sharded merge
/// used to decide a block's tail from the claim alone and silently report
/// node 6 as deleted here.
#[test]
fn understated_max_id_is_rejected_at_every_shard_count() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let honest_new = dir.path().join("new.osm.pbf");
    let lying_new = dir.path().join("lying.osm.pbf");

    let layout = [
        Blob::Nodes(&[1, 2, 3]),
        Blob::Nodes(&[4, 5, 6]),
        Blob::Nodes(&[7, 8, 9]),
        Blob::Nodes(&[10, 11, 12]),
    ];
    write_blobs(&old, &layout);
    write_blobs(&honest_new, &layout);
    assert_both_paths(&old, &honest_new, &[], 0, None);

    // Blob 2 is the second node blob, `[4, 5, 6]`: claim its last node is 5.
    let bytes = std::fs::read(&honest_new).expect("read fixture");
    let forged = crate::common::adversarial::mutate_blob_header_indexdata(&bytes, 2, |ix| {
        assert_eq!(ix.len(), 42, "fixture blob must carry a v3 index");
        ix[10..18].copy_from_slice(&5i64.to_le_bytes());
    });
    std::fs::write(&lying_new, forged).expect("write forged");

    assert_both_paths(
        &old,
        &lying_new,
        &[],
        2,
        Some("does not match the blob's contents"),
    );
}

/// A same-kind ID regression, which is the layout `degrade --unsort`
/// actually produces: two adjacent node blobs whose ID spans go backwards.
/// The sequential merge used to see only kind transitions and answered
/// wrongly here - against a correctly ordered file with the same elements it
/// reported the backwards blob's elements as created and deleted - while the
/// parallel planner refused the file. The one header walk every `-j` runs
/// applies the blob order guard now.
#[test]
fn backwards_id_spans_within_one_kind_are_rejected() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let liar = dir.path().join("liar.osm.pbf");

    write_blobs(&honest, &[Blob::Nodes(&[1, 2]), Blob::Nodes(&[3, 4])]);
    // Same elements, blobs swapped: 3..4 then 1..2, still stamped sorted.
    write_blobs(&liar, &[Blob::Nodes(&[3, 4]), Blob::Nodes(&[1, 2])]);

    assert_both_paths_refuse(&honest, &liar);
    assert_both_paths_refuse(&liar, &honest);
}

/// The same regression inside a kind the type filter excludes: the header
/// walk checks every blob's order, whatever the filter selects.
#[test]
fn backwards_id_spans_in_an_excluded_kind_are_rejected() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let liar = dir.path().join("liar.osm.pbf");

    write_blobs(
        &honest,
        &[
            Blob::Nodes(&[1, 2]),
            Blob::Nodes(&[3, 4]),
            Blob::Ways(&[100]),
        ],
    );
    write_blobs(
        &liar,
        &[
            Blob::Nodes(&[3, 4]),
            Blob::Nodes(&[1, 2]),
            Blob::Ways(&[100]),
        ],
    );

    assert_both_paths(
        &liar,
        &honest,
        &["-t", "way"],
        2,
        Some("blobs are not in canonical order"),
    );
}

/// Negative IDs order 0, -1, -2, ..., then 1, 2, ..., so a blob holding -2
/// and 1 legitimately follows a blob holding -1. Its numeric range
/// `[-2, 1]` proves only that it holds -2 and 1; an order check that took
/// 0 as its first element refused this valid layout.
#[test]
fn valid_mixed_sign_blob_layouts_are_accepted() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_blobs(&old, &[Blob::Nodes(&[-1]), Blob::Nodes(&[-2, 1])]);
    write_blobs(&new, &[Blob::Nodes(&[-1, -2]), Blob::Nodes(&[1])]);

    assert_both_paths(&old, &new, &[], 0, Some("Files are identical"));
    assert_both_paths(&new, &old, &[], 0, Some("Files are identical"));
}

/// The same order check guards what `apply-changes` writes under its sorted
/// header. It took 0 as the first element of the `[-2, 1]` blob and refused
/// to write this valid base back out, even with an empty change file.
#[test]
fn apply_changes_writes_a_valid_mixed_sign_layout() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("empty.osc.gz");
    let out = dir.path().join("out.osm.pbf");
    write_blobs(&base, &[Blob::Nodes(&[-1]), Blob::Nodes(&[-2, 1])]);
    crate::cli_apply_changes::write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6"></osmChange>"#,
    );

    let applied = CliInvoker::new()
        .arg("apply-changes")
        .arg(&base)
        .arg(&osc)
        .arg("-o")
        .arg(&out)
        .run();
    assert!(
        applied.status.success(),
        "apply-changes must write a valid mixed-sign layout; stderr:\n{}",
        applied.stderr_str()
    );
    assert_both_paths(&out, &base, &[], 0, Some("Files are identical"));
}

/// Frame `idx` of `bytes` with every byte of its Blob body set to 0xff,
/// which no envelope parse accepts; the frame and its header stay intact.
fn with_garbage_body(bytes: &[u8], idx: usize) -> Vec<u8> {
    let frames = crate::common::adversarial::locate_blobs(bytes);
    let frame = frames[idx];
    let mut out = bytes.to_vec();
    out[frame.blob_start..frame.blob_end].fill(0xff);
    out
}

/// The structure of the tail after the last selected kind is checked at
/// every `-j`: the header walk reads every header up front. `diff -t node`
/// used to refuse these files only at `-j` above one, when `-j 1` ran a
/// streaming merge whose phases stopped at the last selected kind.
#[test]
fn tail_after_the_last_selected_kind_is_checked_on_both_paths() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let layout = [
        Blob::Nodes(&[1, 2]),
        Blob::Ways(&[10]),
        Blob::Ways(&[20]),
        Blob::Relations(&[100]),
    ];
    write_blobs(&base, &layout);
    let bytes = std::fs::read(&base).expect("read base");
    // Frames: the header, a node blob, two way blobs, one relation blob.
    assert_eq!(crate::common::adversarial::locate_blobs(&bytes).len(), 5);

    let truncated = dir.path().join("truncated.osm.pbf");
    std::fs::write(&truncated, &bytes[..bytes.len() - 5]).expect("write truncated");
    let unindexed_tail = dir.path().join("unindexed-tail.osm.pbf");
    std::fs::write(
        &unindexed_tail,
        crate::common::adversarial::mutate_blob_header_indexdata(&bytes, 3, Vec::clear),
    )
    .expect("write unindexed tail");
    let backwards_tail = dir.path().join("backwards-tail.osm.pbf");
    write_blobs(
        &backwards_tail,
        &[
            Blob::Nodes(&[1, 2]),
            Blob::Ways(&[20]),
            Blob::Ways(&[10]),
            Blob::Relations(&[100]),
        ],
    );

    // Control: the intact file diffs clean against itself at every filter.
    assert_both_paths(&base, &base, &["-t", "node"], 0, None);
    assert_both_paths(&truncated, &base, &["-t", "node"], 2, Some("truncated"));
    assert_both_paths(
        &unindexed_tail,
        &base,
        &["-t", "node"],
        2,
        Some("indexdata"),
    );
    assert_both_paths(
        &backwards_tail,
        &base,
        &["-t", "node"],
        2,
        Some("blobs are not in canonical order"),
    );
}

/// No `-j` interprets the body of an excluded kind's blob: the header walk
/// never reads it. The streaming merge `-j 1` once ran read the first blob
/// after the last selected kind to find where that kind ended and parsed
/// it, so `-j 1` refused a malformed envelope there that `-j 4` never saw.
#[test]
fn excluded_kind_bodies_are_never_interpreted() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    write_blobs(
        &base,
        &[
            Blob::Nodes(&[1, 2]),
            Blob::Ways(&[10]),
            Blob::Relations(&[100]),
        ],
    );
    let bytes = std::fs::read(&base).expect("read base");
    let bad_way = dir.path().join("bad-way.osm.pbf");
    std::fs::write(&bad_way, with_garbage_body(&bytes, 2)).expect("write bad way");
    let bad_node = dir.path().join("bad-node.osm.pbf");
    std::fs::write(&bad_node, with_garbage_body(&bytes, 1)).expect("write bad node");

    assert_both_paths(
        &bad_way,
        &base,
        &["-t", "node"],
        0,
        Some("Files are identical"),
    );
    assert_both_paths(
        &bad_way,
        &base,
        &["-t", "relation"],
        0,
        Some("Files are identical"),
    );
    assert_both_paths(
        &bad_node,
        &base,
        &["-t", "way"],
        0,
        Some("Files are identical"),
    );
    // Selected, the same blob is refused.
    assert_both_paths(&bad_way, &base, &["-t", "way"], 2, None);
}

/// Without indexdata a block's kind is known only once it is decoded. A
/// node block after the ways used to be stashed by the way phase, stashed
/// again by the relation phase, and dropped at the end: the diff answered
/// without node 3. It is refused now, in every phase and whatever the type
/// filter, because excluded kinds are classified to the end of the file
/// rather than left unread.
#[test]
fn unindexed_block_of_an_earlier_kind_is_refused_not_dropped() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let liar = dir.path().join("liar.osm.pbf");
    write_blobs_unindexed(
        &honest,
        &[
            Blob::Nodes(&[1, 2, 3]),
            Blob::Ways(&[100]),
            Blob::Relations(&[7]),
        ],
    );
    write_blobs_unindexed(
        &liar,
        &[
            Blob::Nodes(&[1, 2]),
            Blob::Ways(&[100]),
            Blob::Relations(&[7]),
            Blob::Nodes(&[3]),
        ],
    );

    for args in [
        &[][..],
        &["-t", "node"],
        &["-t", "way"],
        &["-t", "relation"],
    ] {
        assert_both_paths(
            &liar,
            &honest,
            args,
            2,
            Some("blobs are not in canonical order"),
        );
    }
    // Control: the honest file against itself.
    assert_both_paths(
        &honest,
        &honest,
        &["-t", "node"],
        0,
        Some("Files are identical"),
    );
}

/// The unindexed stream has no blob ranges to check, so it checks element
/// order as it converts: node blocks going backwards used to give the same
/// wrong answer as the indexed sequential merge did.
#[test]
fn unindexed_elements_out_of_order_are_refused() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let liar = dir.path().join("liar.osm.pbf");
    write_blobs_unindexed(&honest, &[Blob::Nodes(&[1, 2]), Blob::Nodes(&[3, 4])]);
    write_blobs_unindexed(&liar, &[Blob::Nodes(&[3, 4]), Blob::Nodes(&[1, 2])]);

    assert_both_paths(
        &liar,
        &honest,
        &[],
        2,
        Some("elements are not in canonical order"),
    );
    assert_both_paths(
        &honest,
        &liar,
        &[],
        2,
        Some("elements are not in canonical order"),
    );
}

/// The protobuf fields of `buf` as `(field number, wire type, whole field,
/// value)` byte ranges; only the varint and length-delimited wire types,
/// which is all a `PrimitiveBlock` written by `BlockBuilder` holds.
fn proto_fields(buf: &[u8]) -> Vec<(u64, u64, std::ops::Range<usize>, std::ops::Range<usize>)> {
    use crate::common::adversarial::read_varint;
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let start = pos;
        let (tag, n) = read_varint(&buf[pos..]).expect("tag");
        pos += n;
        let (field, wire) = (tag >> 3, tag & 7);
        let value = match wire {
            0 => {
                let (_, n) = read_varint(&buf[pos..]).expect("varint");
                pos..pos + n
            }
            2 => {
                let (len, n) = read_varint(&buf[pos..]).expect("length");
                let begin = pos + n;
                begin..begin + usize::try_from(len).expect("length fits")
            }
            other => panic!("unexpected wire type {other} in a fixture block"),
        };
        pos = value.end;
        out.push((field, wire, start..pos, value));
    }
    out
}

/// A block's group classification reads only the first field of each
/// group, so a group holding a way and then a node is a "way block" by its
/// type while its elements are a node and a way. Skipping an excluded kind
/// by that classification would drop the node with the way; the skip
/// classifies by the elements instead and refuses the block.
#[test]
fn unindexed_group_hiding_another_kind_is_refused() {
    use crate::common::adversarial::{locate_blobs, mutate_blob_payload, write_varint};
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    write_blobs_unindexed(&base, &[Blob::Nodes(&[2]), Blob::Ways(&[10])]);
    let bytes = std::fs::read(&base).expect("read base");
    assert_eq!(
        locate_blobs(&bytes).len(),
        3,
        "header, node block, way block"
    );

    // The node block's group content (one DenseNodes field).
    let mut node_group = Vec::new();
    mutate_blob_payload(&bytes, 1, |payload| {
        let group = proto_fields(payload)
            .into_iter()
            .find(|f| f.0 == 2)
            .expect("node block group");
        node_group = payload[group.3].to_vec();
    });
    // The way block with that content appended to its group, after the way.
    let hybrid = mutate_blob_payload(&bytes, 2, |payload| {
        let mut out = Vec::new();
        for (field, _, whole, value) in proto_fields(payload) {
            if field == 2 {
                let mut group = payload[value].to_vec();
                group.extend_from_slice(&node_group);
                out.push(0x12);
                write_varint(&mut out, u64::try_from(group.len()).expect("length fits"));
                out.extend_from_slice(&group);
            } else {
                out.extend_from_slice(&payload[whole]);
            }
        }
        *payload = out;
    });
    let frames = locate_blobs(&hybrid);
    let mut spliced = bytes[..frames[1].frame_start].to_vec();
    spliced.extend_from_slice(&hybrid[frames[2].frame_start..frames[2].blob_end]);
    let hidden = dir.path().join("hidden.osm.pbf");
    std::fs::write(&hidden, spliced).expect("write hidden");

    let honest = dir.path().join("honest.osm.pbf");
    write_blobs_unindexed(&honest, &[Blob::Nodes(&[2]), Blob::Ways(&[10])]);
    for args in [&["-t", "node"][..], &["-t", "way"], &["-t", "relation"]] {
        assert_both_paths(&hidden, &honest, args, 2, Some("mixed-type block"));
    }
}

/// Non-regression: honestly-sorted multi-blob files still diff cleanly, and -
/// load-bearing - the two sides use DIFFERENT blob boundaries, so the merge
/// takes its residual path and the "does this blob still hold elements past
/// the boundary" check actually runs on both outcomes. Without this the tests
/// above would pass just as well against a build that rejected every input,
/// and the residual bookkeeping would go unexercised.
#[test]
fn honestly_sorted_misaligned_blob_boundaries_still_diff_clean() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_blobs(
        &old,
        &[
            Blob::Nodes(&[1, 2, 3, 4, 5]),
            Blob::Nodes(&[6, 7]),
            Blob::Ways(&[100, 200, 300]),
            Blob::Relations(&[1000]),
        ],
    );
    write_blobs(
        &new,
        &[
            Blob::Nodes(&[1, 2]),
            Blob::Nodes(&[3, 4]),
            Blob::Nodes(&[5, 6, 7]),
            Blob::Ways(&[100]),
            Blob::Ways(&[200, 300]),
            Blob::Relations(&[1000]),
        ],
    );

    let (code, stderr) = run_diff(&old, &new);
    assert_eq!(
        code,
        Some(0),
        "same elements in different blob layouts must compare identical; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Files are identical"),
        "expected an identical verdict, not merely a zero exit; got:\n{stderr}"
    );
}
