//! The block-pair merge-join must not lose elements when a file's header
//! claims `Sort.Type_then_ID` and its blobs disagree.
//!
//! `block_pair_merge_phase` walks each element kind in one pass, stashing the
//! first blob of a later kind for the next phase. A blob of an EARLIER kind
//! turning up after that kind's phase had already ended used to be
//! `continue`d - discarded, along with every element in it, with no error, no
//! warning, and no anomaly in the reported counts. A header can lie about
//! sortedness (`pbfhogg degrade --unsort` exists to produce such files), so
//! the merge cannot take the stamp as proof.
//!
//! The two `diff` paths reach the same conclusion by different routes. The
//! SEQUENTIAL path (`-j 1`) discovers the disorder structurally, while
//! walking blobs in file order. The PARALLEL path buckets blobs by kind in
//! `walk_file` before any ordering is examined, so file order never reaches
//! its planner; it needs an explicit guard at the walk, which is what
//! `BlobOrderGuard` is.
//!
//! Every rejection test here therefore runs at BOTH `-j 1` and the default
//! `-j`. That is load-bearing, not thoroughness: this divergence is how the
//! bug hid. Before the guard, `pbfhogg diff` returned exit 2 with "not in
//! canonical order" at `-j 1` and exit 0 with "Files are identical" at the
//! default `-j`, on the same pair of files - and the suite's one
//! seq-vs-parallel parity test used an honestly-sorted fixture, so it had no
//! power against the only case where the two paths disagreed.
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
use tempfile::TempDir;

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
            writer.write_primitive_block(bytes).expect("write block");
        }
    }

    writer.flush().expect("flush");
}

/// Run `pbfhogg diff` at an explicit job count and return (exit code,
/// stderr). `jobs` is always spelled out: see the module doc for why a
/// default-`-j`-only test proved nothing here.
fn run_diff_jobs(old: &Path, new: &Path, jobs: &str) -> (Option<i32>, String) {
    let out = CliInvoker::new()
        .arg("diff")
        .arg(old)
        .arg(new)
        .arg("-j")
        .arg(jobs)
        .run();
    (out.status.code(), out.stderr_str())
}

/// Sequential path only, for the assertions that name a
/// sequential-specific phrase.
fn run_diff(old: &Path, new: &Path) -> (Option<i32>, String) {
    run_diff_jobs(old, new, "1")
}

/// Assert both `diff` paths refuse `old` vs `new` with an ordering
/// complaint. The two produce different wording because they detect the
/// disorder at different points; the contract is the verdict, not the
/// sentence.
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
    let dir = TempDir::new().expect("tempdir");
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
        stderr.contains("node blob during the way phase"),
        "error should name which kind turned up in which phase; got:\n{stderr}"
    );

    // And the same verdict from the parallel planner, which reaches it by a
    // different route. Before `BlobOrderGuard` this arm returned exit 0 and
    // "Files are identical" on exactly this input.
    assert_both_paths_refuse(&honest, &liar);
}

/// The same shape on the other side of the comparison, so the guard is not
/// accidentally one-sided (the two readers are separate `BlobReader`s with
/// separate stashes).
#[test]
fn out_of_order_blob_on_the_old_side_is_rejected_too() {
    let dir = TempDir::new().expect("tempdir");
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
    let dir = TempDir::new().expect("tempdir");
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

/// A same-kind ID regression, which is the layout `degrade --unsort`
/// actually produces: two adjacent node blobs whose ID spans go backwards.
/// The sequential merge sees only kind transitions, so it cannot notice
/// this one at all - the parallel planner is the path that must, because an
/// unsorted per-kind list breaks the binary searches its sharding is built
/// on.
#[test]
fn backwards_id_spans_within_one_kind_are_rejected() {
    let dir = TempDir::new().expect("tempdir");
    let honest = dir.path().join("honest.osm.pbf");
    let liar = dir.path().join("liar.osm.pbf");

    write_blobs(&honest, &[Blob::Nodes(&[1, 2]), Blob::Nodes(&[3, 4])]);
    // Same elements, blobs swapped: 3..4 then 1..2, still stamped sorted.
    write_blobs(&liar, &[Blob::Nodes(&[3, 4]), Blob::Nodes(&[1, 2])]);

    let (code, stderr) = run_diff_jobs(&honest, &liar, "4");
    assert_eq!(
        code,
        Some(2),
        "backwards ID spans within a kind must be refused; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("blobs are not in canonical order"),
        "error should name the ordering problem; got:\n{stderr}"
    );
}

/// Non-regression: honestly-sorted multi-blob files still diff cleanly, and -
/// load-bearing - the two sides use DIFFERENT blob boundaries, so the merge
/// takes its residual path and the "does this blob still hold elements past
/// the boundary" check actually runs on both outcomes. Without this the tests
/// above would pass just as well against a build that rejected every input,
/// and the residual bookkeeping would go unexercised.
#[test]
fn honestly_sorted_misaligned_blob_boundaries_still_diff_clean() {
    let dir = TempDir::new().expect("tempdir");
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
