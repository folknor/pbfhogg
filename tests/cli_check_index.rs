//! `pbfhogg check --index`: the verification half of the blob-metadata trust
//! boundary.
//!
//! Every fast path in the crate believes `BlobHeader.indexdata` and
//! `BlobHeader.tagdata` without checking them, and `degrade
//! --corrupt-indexdata` / `--corrupt-indexdata-kind` / `--corrupt-tagdata`
//! exist to manufacture files whose hints are well-formed and false.
//! `check --index` is the other side.
//!
//! Every detection test here is paired with an honest-file counterpart run
//! through the identical assertions, because a verifier that reports a
//! violation on everything would pass every detection test on its own. The
//! honest half is the one that fails when the check is over-eager - which is
//! exactly what happened during development: an earlier draft treated a v1
//! index's absent bbox as a violation and reported all 6562 node blobs of the
//! real Denmark extract as lying.
//!
//! Exit codes matter as much as the text: exit 1 is "I ran, and the answer is
//! no", exit 2 is "I could not run". A verifier that conflates them is useless
//! in a script.

use std::path::Path;

use crate::common::cli::CliInvoker;
use crate::common::{
    TestNode, TestRelation, TestWay, generate_nodes, generate_relations, generate_ways,
    write_indexed_pbf, write_multi_block_test_pbf,
};

/// A multi-blob fixture carrying tags on several distinct blobs across all
/// three element kinds, so both the indexdata and the tagdata checks have more
/// than one blob to disagree about.
fn write_fixture(path: &Path) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let mut nodes = generate_nodes(60, 1);
    nodes[0].tags = vec![("place", "city"), ("name", "Origo")];
    nodes[7].tags = vec![("amenity", "cafe")];
    nodes[42].tags = vec![("highway", "bus_stop")];
    let mut ways = generate_ways(12, 1, 3, 1);
    ways[0].tags = vec![("highway", "residential")];
    let mut rels = generate_relations(6, 1, 2, 1);
    rels[0].tags = vec![("type", "route")];
    write_multi_block_test_pbf(path, &nodes, &ways, &rels, 20);
    (nodes, ways, rels)
}

/// Exit code the CLI uses for "ran fine, answer is negative".
const EXIT_ANSWER_NEGATIVE: i32 = 1;

fn check_index(path: &Path) -> crate::common::cli::CliOutput {
    CliInvoker::new()
        .arg("check")
        .arg("--index")
        .arg(path)
        .run()
}

/// Run `degrade` with one corruption flag and return the corrupted file's path.
fn degrade_with(dir: &Path, input: &Path, flag: &str, name: &str) -> std::path::PathBuf {
    let out = dir.join(name);
    CliInvoker::new()
        .arg("degrade")
        .arg(input)
        .arg("-o")
        .arg(&out)
        .arg(flag)
        .assert_success();
    out
}

// ---------------------------------------------------------------------------
// The honest baseline
// ---------------------------------------------------------------------------

/// The single most important test in this file: an honest file must pass.
///
/// It is the counterpart to every detection below, and the one that catches a
/// check that has become over-eager. A `check --index` that reports a lie on a
/// file this crate itself just wrote is worse than no check at all.
#[test]
fn an_honest_file_passes_and_exits_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("honest.osm.pbf");
    write_fixture(&input);

    let out = check_index(&input);
    assert!(
        out.status.success(),
        "a file pbfhogg wrote itself must verify clean; stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );
    let stdout = out.stdout_str();
    assert!(
        stdout.contains("OK"),
        "the passing summary must say so: {stdout}"
    );
    // The fixture is multi-blob and indexed; a check that silently examined
    // zero blobs would also "pass".
    assert!(
        !stdout.contains("Blobs: 0 "),
        "the check must actually have looked at blobs: {stdout}"
    );
    assert!(
        stdout.contains("with indexdata"),
        "the summary must report how many blobs carried indexdata: {stdout}"
    );
}

/// Repack uses BlockBuilder's precomputed tagdata instead of the writer's
/// fallback payload scan, so this verifies the producer path in question.
#[test]
fn repacked_empty_tag_key_passes_index_check() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let repacked = dir.path().join("repacked.osm.pbf");
    let nodes = [TestNode {
        id: 1,
        lat: 0,
        lon: 0,
        tags: vec![("", "empty-key")],
        meta: None,
    }];
    write_indexed_pbf(&input, &nodes, &[], &[]);

    CliInvoker::new()
        .arg("repack")
        .arg(&input)
        .arg("-o")
        .arg(&repacked)
        .assert_success();

    let out = check_index(&repacked);
    assert!(
        out.status.success(),
        "repack must retain the empty key in its precomputed tagdata; stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );
}

/// A file with no indexdata at all makes no claims, so it cannot lie.
///
/// Every third-party PBF is this file. Reporting it would make `check --index`
/// useless on exactly the inputs most worth checking.
#[test]
fn a_file_that_makes_no_claims_passes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let stripped = dir.path().join("stripped.osm.pbf");
    write_fixture(&input);
    let stripped = {
        CliInvoker::new()
            .arg("degrade")
            .arg(&input)
            .arg("-o")
            .arg(&stripped)
            .arg("--strip-indexdata")
            .arg("--strip-tagdata")
            .assert_success();
        stripped
    };

    let out = check_index(&stripped);
    assert!(
        out.status.success(),
        "a PBF carrying no sidecar metadata has nothing to be wrong about; \
         stdout:\n{}",
        out.stdout_str(),
    );
    assert!(
        out.stdout_str().contains("0 with indexdata"),
        "and the summary should say the claims are absent: {}",
        out.stdout_str()
    );
}

// ---------------------------------------------------------------------------
// The three corruption modes
// ---------------------------------------------------------------------------

/// `--corrupt-indexdata` inflates `count` and `max_id` by 1 000 000 on every
/// blob, through the real encoder, so what lands on the wire is a valid index
/// that lies. `check --index` must catch both fields.
#[test]
fn an_inflated_count_and_id_range_are_detected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let corrupt = degrade_with(dir.path(), &input, "--corrupt-indexdata", "count.osm.pbf");

    let out = check_index(&corrupt);
    assert_eq!(
        out.status.code(),
        Some(EXIT_ANSWER_NEGATIVE),
        "a detected lie is a negative answer (exit 1), not a failure to run \
         (exit 2); stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );
    let stdout = out.stdout_str();
    assert!(
        stdout.contains("elements, blob holds"),
        "the inflated count must be reported: {stdout}"
    );
    assert!(
        stdout.contains("max_id"),
        "the widened max_id must be reported: {stdout}"
    );
    assert!(
        stdout.contains("FAILED"),
        "the summary must say the file failed: {stdout}"
    );
}

/// `--corrupt-indexdata-kind` rotates node -> way -> relation -> node and
/// leaves every other field honest. The kind is what per-kind blob schedules
/// select on, so this is the corruption that makes a consumer skip a blob
/// outright rather than merely misreport it.
#[test]
fn a_rotated_element_kind_is_detected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let corrupt = degrade_with(
        dir.path(),
        &input,
        "--corrupt-indexdata-kind",
        "kind.osm.pbf",
    );

    let out = check_index(&corrupt);
    assert_eq!(out.status.code(), Some(EXIT_ANSWER_NEGATIVE));
    let stdout = out.stdout_str();
    assert!(
        stdout.contains("claims kind"),
        "the rotated kind must be reported as such: {stdout}"
    );
    // The rotation leaves count and ID range honest, so a check that reported
    // those too would be reporting something it did not observe.
    assert!(
        !stdout.contains("elements, blob holds"),
        "--corrupt-indexdata-kind leaves the count honest; reporting a count \
         violation means the comparison is not field-specific: {stdout}"
    );
}

/// `--corrupt-tagdata` replaces every blob's tag key set with a single key no
/// real OSM tag can collide with. The keys the blob really carries therefore
/// vanish from the index, which is the shape that makes `tags-filter` skip a
/// blob holding matches.
#[test]
fn a_replaced_tag_key_set_is_detected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let corrupt = degrade_with(dir.path(), &input, "--corrupt-tagdata", "tags.osm.pbf");

    let out = check_index(&corrupt);
    assert_eq!(out.status.code(), Some(EXIT_ANSWER_NEGATIVE));
    let stdout = out.stdout_str();
    assert!(
        stdout.contains("tagdata omits"),
        "the keys the blob really carries are missing from the index, and that \
         is the half that costs matches: {stdout}"
    );
    assert!(
        stdout.contains("tagdata lists"),
        "the bogus key the blob does not carry must also be reported: {stdout}"
    );
    // The payload is untouched by --corrupt-tagdata, so nothing about the
    // element index should be wrong.
    assert!(
        !stdout.contains("claims kind"),
        "--corrupt-tagdata does not touch indexdata: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Claims the verifier cannot read
// ---------------------------------------------------------------------------

/// Indexdata that is present but unreadable must be reported, not read as
/// absent.
///
/// `read_raw_frame` deserializes the raw field into an `Option<BlobIndex>`, so
/// unreadable bytes arrive at the comparison as `None` - indistinguishable,
/// there, from a file that carries no indexdata at all. The verifier therefore
/// silently passed the one input it most obviously has to reject, and did so in
/// both directions: a bad version byte (right length, unreadable content) and
/// an unrecognised length (dropped by the wire parser before the index code
/// ever sees it).
#[test]
fn indexdata_that_cannot_be_read_is_reported_rather_than_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let pbf = std::fs::read(&input).expect("read fixture");

    // Shape 1: a 42-byte index with a version byte no reader knows.
    let bad_version = dir.path().join("badversion.osm.pbf");
    std::fs::write(
        &bad_version,
        crate::common::adversarial::mutate_blob_header_indexdata(&pbf, 1, |ix| {
            assert!(!ix.is_empty(), "fixture blob 1 must carry indexdata");
            ix[0] = 0xEE;
        }),
    )
    .expect("write");
    let out = check_index(&bad_version);
    assert_eq!(
        out.status.code(),
        Some(EXIT_ANSWER_NEGATIVE),
        "an unreadable indexdata is a false claim, not an absent one; \
         stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );
    assert!(
        out.stdout_str().contains("not a readable blob index"),
        "and it must say so: {}",
        out.stdout_str()
    );

    // Shape 2: a length neither 26 nor 42. This one never reaches the index
    // code at all - `WireBlobHeader::parse` drops it and warns once - so it
    // needs the presence flag rather than the parsed value to be caught.
    let bad_len = dir.path().join("badlen.osm.pbf");
    std::fs::write(
        &bad_len,
        crate::common::adversarial::mutate_blob_header_indexdata(&pbf, 1, |ix| {
            ix.truncate(7);
        }),
    )
    .expect("write");
    assert_eq!(
        check_index(&bad_len).status.code(),
        Some(EXIT_ANSWER_NEGATIVE),
        "an indexdata of unrecognised length is still a claim the file \
         cannot back up"
    );

    // Paired counterpart: the same rewrite machinery, leaving the bytes
    // alone, must still verify clean. Without this, "report everything that
    // went through the mutator" would pass both assertions above.
    let untouched = dir.path().join("untouched.osm.pbf");
    std::fs::write(
        &untouched,
        crate::common::adversarial::mutate_blob_header_indexdata(&pbf, 1, |_ix| {}),
    )
    .expect("write");
    let out = check_index(&untouched);
    assert!(
        out.status.success(),
        "a byte-identical rewrite must verify clean; stdout:\n{}",
        out.stdout_str()
    );
}

/// A valid tag index over a blob that cannot be indexed is a lie with a
/// consumer, and must be reported even when indexdata is absent.
///
/// A group mixing element kinds makes `scan_block_tags` return `None`, because
/// the scanner would otherwise see only the first element field and emit a key
/// set missing the rest. The verifier used to return quietly on that `None`, on
/// the reasoning that the indexdata check would have fired anyway - which is
/// false whenever tagdata is present alone, and which left the exact
/// silently-wrong-output path this command was built to close: a mixed-kind
/// blob carrying `amenity`, a valid-looking tag index that omits it,
/// `check --index` saying OK, and `tags-filter -e amenity=cafe` skipping the
/// blob on the strength of that index.
#[test]
fn a_tag_index_over_an_unindexable_blob_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let pbf = std::fs::read(&input).expect("read fixture");
    // The relation blob is last, and the fixture gives its first relation a
    // tag, so pbfhogg wrote it a real tag index.
    let last = crate::common::adversarial::locate_blobs(&pbf).len() - 1;

    // Replace the payload with a group holding both a Way and a Relation,
    // preserving the header's tagdata, then drop the indexdata so the tagdata
    // claim is the only one left standing.
    let mixed = crate::common::adversarial::mutate_blob_payload(&pbf, last, |payload| {
        *payload = mixed_kind_block();
    });
    let mixed = crate::common::adversarial::mutate_blob_header_indexdata(&mixed, last, Vec::clear);
    let mixed_path = dir.path().join("mixed.osm.pbf");
    std::fs::write(&mixed_path, &mixed).expect("write");

    let out = check_index(&mixed_path);
    let stdout = out.stdout_str();
    assert_eq!(
        out.status.code(),
        Some(EXIT_ANSWER_NEGATIVE),
        "a tag index over a blob no tag index can describe must fail the \
         check; stdout:\n{stdout}\nstderr:\n{}",
        out.stderr_str(),
    );
    assert!(
        stdout.contains("tagdata is present for a blob that cannot be indexed"),
        "and the report must name the tagdata, since the indexdata was \
         removed and cannot be what fired: {stdout}"
    );

    // Paired counterpart: the identical rewrite path, with the payload left
    // alone, must verify clean. Dropping the indexdata is legal - it is what
    // every third-party PBF looks like - so nothing here should be reported.
    let stripped = crate::common::adversarial::mutate_blob_header_indexdata(&pbf, last, Vec::clear);
    let stripped_path = dir.path().join("stripped-index.osm.pbf");
    std::fs::write(&stripped_path, &stripped).expect("write");
    let out = check_index(&stripped_path);
    assert!(
        out.status.success(),
        "removing an index is not a lie about the blob; stdout:\n{}",
        out.stdout_str()
    );
}

/// A `PrimitiveBlock` whose one group carries both a Way (field 3) and a
/// Relation (field 4), with `amenity` on the relation the scanner cannot see.
fn mixed_kind_block() -> Vec<u8> {
    let mut st = Vec::new();
    for s in [b"".as_slice(), b"highway".as_slice(), b"amenity".as_slice()] {
        st.push(0x0A);
        st.push(u8::try_from(s.len()).expect("short string"));
        st.extend_from_slice(s);
    }
    let way = [0x12u8, 0x01, 0x01];
    let rel = [0x12u8, 0x01, 0x02];
    let mut group = Vec::new();
    group.push(0x1A);
    group.push(u8::try_from(way.len()).expect("short message"));
    group.extend_from_slice(&way);
    group.push(0x22);
    group.push(u8::try_from(rel.len()).expect("short message"));
    group.extend_from_slice(&rel);

    let mut block = Vec::new();
    block.push(0x0A);
    block.push(u8::try_from(st.len()).expect("short string table"));
    block.extend_from_slice(&st);
    block.push(0x12);
    block.push(u8::try_from(group.len()).expect("short group"));
    block.extend_from_slice(&group);
    block
}

// ---------------------------------------------------------------------------
// Selector plumbing
// ---------------------------------------------------------------------------

/// `--index` is a selector like `--ids` and `--refs`, and it is deliberately
/// NOT in the default set: a bare `check` must not silently start decompressing
/// the entire file.
#[test]
fn index_is_opt_in_and_a_bare_check_does_not_run_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let corrupt = degrade_with(dir.path(), &input, "--corrupt-indexdata", "count.osm.pbf");

    // A bare `check` on the corrupted file must not report the metadata lie -
    // it does not look. (It also must not fail for some other reason: the
    // payload is byte-identical to the honest fixture.)
    let bare = CliInvoker::new().arg("check").arg(&corrupt).run();
    assert!(
        bare.status.success(),
        "a bare check runs --ids and --refs only, and the payload is honest; \
         stdout:\n{}\nstderr:\n{}",
        bare.stdout_str(),
        bare.stderr_str(),
    );
    assert!(
        !bare.stdout_str().contains("Blob metadata"),
        "the index check must not run unless asked: {}",
        bare.stdout_str()
    );

    // And the same file with --index does report it, so the assertion above is
    // about the selector rather than about the fixture being clean.
    assert_eq!(
        check_index(&corrupt).status.code(),
        Some(EXIT_ANSWER_NEGATIVE)
    );
}

/// `--index` selects, so it suppresses the default `--ids` + `--refs` pair.
#[test]
fn index_alone_runs_only_the_index_check() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);

    let out = check_index(&input).stdout_str();
    assert!(
        out.contains("Blob metadata"),
        "the index summary must be present: {out}"
    );
    assert!(
        !out.contains("Verify IDs"),
        "--index must suppress the default --ids check: {out}"
    );
    assert!(
        !out.contains("Referential integrity"),
        "--index must suppress the default --refs check: {out}"
    );

    // Counterpart: asking for both runs both, so the suppression above is the
    // selector working rather than the index check having replaced them.
    let both = CliInvoker::new()
        .arg("check")
        .arg("--index")
        .arg("--refs")
        .arg(&input)
        .assert_success()
        .stdout_str();
    assert!(both.contains("Blob metadata"), "{both}");
    assert!(both.contains("Referential integrity"), "{both}");
}

/// The JSON surface carries the same verdict as the human one.
#[test]
fn json_output_reports_the_verdict_and_the_violations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let corrupt = degrade_with(dir.path(), &input, "--corrupt-indexdata", "count.osm.pbf");

    let honest = CliInvoker::new()
        .arg("check")
        .arg("--index")
        .arg("--json")
        .arg(&input)
        .assert_success()
        .stdout_str();
    let honest: serde_json::Value = serde_json::from_str(&honest).expect("valid JSON");
    assert_eq!(honest["index"]["passed"], serde_json::Value::Bool(true));
    assert_eq!(honest["index"]["total_violations"], 0);

    let out = CliInvoker::new()
        .arg("check")
        .arg("--index")
        .arg("--json")
        .arg(&corrupt)
        .run();
    assert_eq!(out.status.code(), Some(EXIT_ANSWER_NEGATIVE));
    let bad: serde_json::Value = serde_json::from_str(&out.stdout_str()).expect("valid JSON");
    assert_eq!(bad["index"]["passed"], serde_json::Value::Bool(false));
    assert!(
        bad["index"]["total_violations"].as_u64().unwrap_or(0) > 0,
        "a corrupted file must report a positive violation count: {bad}"
    );
    assert!(
        bad["index"]["violations"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "and the individual violations must be listed: {bad}"
    );
}

/// A missing input is a failure to run (exit 2), not a negative answer.
#[test]
fn an_unreadable_input_is_exit_two_not_exit_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = check_index(&dir.path().join("nope.osm.pbf"));
    assert_eq!(
        out.status.code(),
        Some(2),
        "a verifier that cannot distinguish 'the answer is no' from 'I could \
         not look' is useless in a script; stderr:\n{}",
        out.stderr_str(),
    );
}

#[test]
fn malformed_payload_without_metadata_is_an_error() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("malformed.pbf");
    let file = std::fs::File::create(&input).expect("create input");
    let mut writer = pbfhogg::writer::PbfWriter::new(file, pbfhogg::writer::Compression::None);
    let header = pbfhogg::block_builder::HeaderBuilder::new()
        .build()
        .expect("header");
    writer.write_header(&header).expect("write header");
    writer
        .write_primitive_block_no_indexdata(&[0x80])
        .expect("write adversarial payload");
    writer.flush().expect("flush fixture");
    drop(writer);

    let out = check_index(&input);
    assert_eq!(out.status.code(), Some(2), "{}", out.stderr_str());
}
