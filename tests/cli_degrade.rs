//! CLI-driven integration tests for `pbfhogg degrade`.
//!
//! Fixtures are built with the stable-allowlist writer helpers; the
//! degrade command runs via the compiled `pbfhogg` binary through
//! `CliInvoker`; output is verified by reading the resulting PBF with
//! the stable-allowlist reader helpers and (where useful) by piping the
//! output through `pbfhogg sort` to confirm it round-trips back to the
//! original element set. No imports from `pbfhogg::commands::degrade` -
//! a rewrite of `src/commands/degrade/` cannot break these tests by
//! type changes alone.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;

use common::cli::CliInvoker;
use common::{
    TestNode, TestRelation, TestWay, assert_has_tagdata, assert_indexed, assert_no_tagdata,
    assert_no_tagdata_all_blobs, assert_non_indexed, assert_sorted_file, count_tagdata_blobs,
    generate_nodes, generate_relations, generate_ways, read_header, read_normalized,
    write_multi_block_test_pbf,
};
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};

/// Build a sorted, multi-blob fixture: 60 nodes, 12 ways, 6 relations,
/// packed at 20 elements/blob. Yields 3 node blobs + 1 way blob + 1
/// relation blob = 5 OsmData blobs in the input.
fn write_degrade_fixture(path: &Path) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let mut nodes = generate_nodes(60, 1);
    // Tag elements in several distinct blobs (and across all three kinds) so
    // the fixture carries tagdata in more than one blob. With 20 elements per
    // blob the tagged nodes land in the first node blob (ids 1 and 8) and the
    // third node blob (id 43); tagging a way and a relation adds tagdata to
    // the way blob and the relation blob too. That makes the whole-file
    // `assert_no_tagdata_all_blobs` check on `--strip-tagdata` output
    // meaningful rather than a single-blob assertion in disguise.
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

/// The `HeaderBlock.bbox` the bbox fixture declares, in
/// `HeaderBuilder::bbox` order (left/bottom/right/top degrees). Chosen to
/// enclose every `generate_nodes` coordinate (node `n` sits at
/// `n * 1e-4` degrees, so `1..60` land in `[1e-4, 6e-3]`).
const FIXTURE_BBOX: (f64, f64, f64, f64) = (0.0, 0.0, 0.01, 0.01);

// Rich HeaderBlock metadata the bbox fixture carries beyond the bbox. Every
// one of these is a field that a `HeaderBuilder::from_header` rebuild would
// silently drop or replace (source and custom optional features have no
// `HeaderBuilder` encoder; the writingprogram would be reset to "pbfhogg"),
// so asserting they SURVIVE `--strip-bbox` pins that the passthrough path
// preserves the input HeaderBlock payload verbatim instead of rebuilding it.
const FIXTURE_WRITING_PROGRAM: &str = "degrade-test-writer/3.1";
const FIXTURE_SOURCE: &str = "survey-import-2019";
const FIXTURE_CUSTOM_FEATURE: &str = "Custom.Extension-v9";
const FIXTURE_REPL_TS: i64 = 1_700_000_000;
const FIXTURE_REPL_SEQ: i64 = 4242;
const FIXTURE_REPL_URL: &str = "https://example.org/replication";

/// Append a base-128 varint to `buf` (protobuf wire encoding).
fn push_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            buf.push(byte | 0x80);
        } else {
            buf.push(byte);
            break;
        }
    }
}

/// Append a length-delimited (wire type 2) field to a protobuf message.
fn push_len_field(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    push_varint(buf, (u64::from(field) << 3) | 2);
    push_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

/// Build the rich HeaderBlock protobuf bytes the bbox fixtures share: a bbox,
/// sorted flag, a non-default writingprogram, a custom optional feature, the
/// three osmosis replication fields, and a `source` (field 17). `HeaderBuilder`
/// cannot emit `source`, so it is appended at the wire level.
fn rich_bbox_header_bytes() -> Vec<u8> {
    let (left, bottom, right, top) = FIXTURE_BBOX;
    let mut header = HeaderBuilder::new()
        .sorted()
        .bbox(left, bottom, right, top)
        .writing_program(FIXTURE_WRITING_PROGRAM)
        .optional_feature(FIXTURE_CUSTOM_FEATURE)
        .replication_timestamp(FIXTURE_REPL_TS)
        .replication_sequence_number(FIXTURE_REPL_SEQ)
        .replication_base_url(FIXTURE_REPL_URL)
        .build()
        .expect("build header");
    // Field 17: source. Protobuf fields may appear in any order, so appending
    // is valid; the reader picks it up as HeaderBlock.source.
    push_len_field(&mut header, 17, FIXTURE_SOURCE.as_bytes());
    header
}

/// Write the three element kinds into `writer`, one block per kind.
fn write_fixture_elements(
    writer: &mut PbfWriter<impl std::io::Write>,
    nodes: &[TestNode],
    ways: &[TestWay],
    rels: &[TestRelation],
) {
    let no_meta: Option<&Metadata> = None;
    let mut bb = BlockBuilder::new();
    for n in nodes {
        bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), no_meta);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write nodes");
    }
    for w in ways {
        bb.add_way(w.id, w.tags.iter().copied(), &w.refs, no_meta);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write ways");
    }
    for r in rels {
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: m.role,
            })
            .collect();
        bb.add_relation(r.id, r.tags.iter().copied(), &members, no_meta);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer
            .write_primitive_block(bytes)
            .expect("write relations");
    }
}

/// Build a sorted, indexed, multi-blob fixture that carries a rich header:
/// a `HeaderBlock.bbox`, a non-default writingprogram, a custom optional
/// feature, replication metadata, and a `source`. Written via
/// `PbfWriter::to_path`, which embeds `indexdata`, so the output is both
/// indexed (extractable) and bbox-bearing.
fn write_bbox_fixture(path: &Path) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let nodes = generate_nodes(60, 1);
    let ways = generate_ways(12, 1, 3, 1);
    let rels = generate_relations(6, 1, 2, 1);

    let header = rich_bbox_header_bytes();
    let mut writer =
        PbfWriter::to_path(path, Compression::default(), &header).expect("create writer");
    write_fixture_elements(&mut writer, &nodes, &ways, &rels);
    writer.flush().expect("flush");
    (nodes, ways, rels)
}

/// Assert the rich non-bbox header fields the bbox fixture declares are all
/// present. Used as an input precondition so the survival assertions after a
/// `--strip-bbox` are not vacuous.
fn assert_rich_header_fields_present(path: &Path) {
    let h = read_header(path);
    assert_eq!(h.writing_program(), Some(FIXTURE_WRITING_PROGRAM));
    assert_eq!(h.source(), Some(FIXTURE_SOURCE));
    assert_eq!(h.osmosis_replication_timestamp(), Some(FIXTURE_REPL_TS));
    assert_eq!(
        h.osmosis_replication_sequence_number(),
        Some(FIXTURE_REPL_SEQ)
    );
    assert_eq!(h.osmosis_replication_base_url(), Some(FIXTURE_REPL_URL));
    assert!(
        h.optional_features()
            .iter()
            .any(|f| f == FIXTURE_CUSTOM_FEATURE),
        "fixture must declare the custom optional feature"
    );
    assert!(h.is_sorted());
    assert!(h.bbox().is_some());
}

/// Assert every rich non-bbox header field survived a transform verbatim.
fn assert_rich_header_fields_survived(path: &Path) {
    let h = read_header(path);
    assert_eq!(
        h.writing_program(),
        Some(FIXTURE_WRITING_PROGRAM),
        "writingprogram must survive (a HeaderBuilder rebuild would reset it to pbfhogg)"
    );
    assert_eq!(
        h.source(),
        Some(FIXTURE_SOURCE),
        "source (field 17) must survive (HeaderBuilder cannot even emit it)"
    );
    assert_eq!(h.osmosis_replication_timestamp(), Some(FIXTURE_REPL_TS));
    assert_eq!(
        h.osmosis_replication_sequence_number(),
        Some(FIXTURE_REPL_SEQ)
    );
    assert_eq!(h.osmosis_replication_base_url(), Some(FIXTURE_REPL_URL));
    assert!(
        h.optional_features()
            .iter()
            .any(|f| f == FIXTURE_CUSTOM_FEATURE),
        "custom optional feature must survive (a HeaderBuilder rebuild drops it)"
    );
    assert!(h.is_sorted(), "Sort.Type_then_ID must survive");
}

/// Extract the raw byte frames of every `OSMData` blob in `path`, in file
/// order. Used to assert the passthrough path copies OsmData frames verbatim
/// (byte-for-byte) rather than re-encoding them. Walks the PBF wire envelope
/// `[4-byte BE header_len][BlobHeader][Blob]` directly and parses each
/// BlobHeader's type (field 1) and datasize (field 3).
fn osm_data_frames(path: &Path) -> Vec<Vec<u8>> {
    fn read_varint_at(buf: &[u8], pos: &mut usize) -> u64 {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = buf[*pos];
            *pos += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        result
    }

    let data = std::fs::read(path).expect("read pbf");
    let mut pos = 0usize;
    let mut frames = Vec::new();
    while pos < data.len() {
        let frame_start = pos;
        let header_len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        let header_end = pos + header_len;
        let mut hpos = pos;
        let mut blob_type = String::new();
        let mut datasize = 0usize;
        while hpos < header_end {
            let tag = read_varint_at(&data, &mut hpos);
            let field = tag >> 3;
            let wire = tag & 7;
            match (field, wire) {
                (1, 2) => {
                    let len = usize::try_from(read_varint_at(&data, &mut hpos)).expect("len fits");
                    blob_type = String::from_utf8(data[hpos..hpos + len].to_vec()).expect("utf8");
                    hpos += len;
                }
                (3, 0) => {
                    datasize =
                        usize::try_from(read_varint_at(&data, &mut hpos)).expect("datasize fits");
                }
                (_, 0) => {
                    read_varint_at(&data, &mut hpos);
                }
                (_, 2) => {
                    let len = usize::try_from(read_varint_at(&data, &mut hpos)).expect("len fits");
                    hpos += len;
                }
                _ => panic!("unexpected wire type {wire} in BlobHeader"),
            }
        }
        let frame_end = header_end + datasize;
        if blob_type == "OSMData" {
            frames.push(data[frame_start..frame_end].to_vec());
        }
        pos = frame_end;
    }
    frames
}

/// Build a non-indexed, bbox-bearing fixture via the sync `PbfWriter::new`
/// path (which does not embed `indexdata`). Carries the same rich header as
/// `write_bbox_fixture`. Used to prove `--strip-bbox` needs no indexdata
/// precondition: the passthrough path never calls `require_indexdata`, so the
/// run must succeed on a non-indexed input without `--force` (the decode path
/// would error out).
fn write_non_indexed_bbox_fixture(path: &Path) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let nodes = generate_nodes(60, 1);
    let ways = generate_ways(12, 1, 3, 1);
    let rels = generate_relations(6, 1, 2, 1);

    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::new(file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = rich_bbox_header_bytes();
    writer.write_header(&header).expect("write header");

    // write_primitive_block_no_indexdata keeps the OsmData BlobHeaders free of
    // the indexdata field, so the fixture is genuinely non-indexed.
    let no_meta: Option<&Metadata> = None;
    let mut bb = BlockBuilder::new();
    for n in &nodes {
        bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), no_meta);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer
            .write_primitive_block_no_indexdata(bytes)
            .expect("write nodes");
    }
    for w in &ways {
        bb.add_way(w.id, w.tags.iter().copied(), &w.refs, no_meta);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer
            .write_primitive_block_no_indexdata(bytes)
            .expect("write ways");
    }
    for r in &rels {
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: m.role,
            })
            .collect();
        bb.add_relation(r.id, r.tags.iter().copied(), &members, no_meta);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer
            .write_primitive_block_no_indexdata(bytes)
            .expect("write relations");
    }
    writer.flush().expect("flush");
    (nodes, ways, rels)
}

/// Build a sorted, indexed fixture whose ways carry inline `LocationsOnWays`
/// coordinates and whose header declares the `LocationsOnWays` optional
/// feature. Used to make the standalone `--strip-locations` assertion
/// non-vacuous: the input genuinely declares LOW, so clearing it is a real
/// change rather than a no-op on a header that never had it.
fn write_low_fixture(path: &Path) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let nodes = generate_nodes(60, 1);
    let ways = generate_ways(12, 1, 3, 1);
    let rels = generate_relations(6, 1, 2, 1);

    let header = HeaderBuilder::new()
        .sorted()
        .optional_feature("LocationsOnWays")
        .build()
        .expect("build header");
    let mut writer =
        PbfWriter::to_path(path, Compression::default(), &header).expect("create writer");

    let no_meta: Option<&Metadata> = None;
    let mut bb = BlockBuilder::new();
    for n in &nodes {
        bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), no_meta);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write nodes");
    }
    for w in &ways {
        // Give every ref an inline coordinate so the way genuinely carries
        // LocationsOnWays data (not just the header feature flag). Exact
        // values are irrelevant to the strip; a fixed decimicro pair suffices.
        let locations: Vec<(i32, i32)> = w.refs.iter().map(|_| (1_000_000, 2_000_000)).collect();
        bb.add_way_with_locations(w.id, w.tags.iter().copied(), &w.refs, &locations, no_meta);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write ways");
    }
    for r in &rels {
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: m.role,
            })
            .collect();
        bb.add_relation(r.id, r.tags.iter().copied(), &members, no_meta);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer
            .write_primitive_block(bytes)
            .expect("write relations");
    }
    writer.flush().expect("flush");
    (nodes, ways, rels)
}

/// Build a sorted fixture whose input blobs are deliberately *smaller*
/// than the `--block-cap` the unsort tests use (4 elements/blob vs cap
/// 10). This is the regime that distinguishes the two unsort modes and
/// reproduces the real-world bug: when input blobs are smaller than the
/// cap, the buggy per-input-blob boundary flush confines the swap to one
/// output blob (intra-blob inversion) instead of producing the documented
/// cross-blob overlap. Each kind has well over `cap + 1` elements so the
/// swap fires for all three kinds.
fn write_unsort_fixture(path: &Path) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let nodes = generate_nodes(60, 1);
    let ways = generate_ways(24, 1, 3, 1);
    let rels = generate_relations(24, 1, 2, 1);
    write_multi_block_test_pbf(path, &nodes, &ways, &rels, 4);
    (nodes, ways, rels)
}

/// The `--block-cap` the unsort tests pass. Larger than the unsort
/// fixture's 4-element input blobs so the two modes diverge.
const UNSORT_CAP: &str = "10";

/// Build a sorted fixture whose input blobs are deliberately *larger* than
/// the `--block-cap` the large-blob test uses (20 elements/blob vs cap 5).
/// This is the regime that exposed finding 1: one input blob carrying more
/// than `cap` same-kind elements. Keying the swap to the `cap` boundary
/// (the old shared logic) made `--unsort-intra` fill and flush an output
/// block here, producing the cross-blob overlap shape instead of an
/// intra-blob inversion. The fix keys `--unsort-intra`'s swap to the first
/// two elements, so it stays inside the first output block regardless of
/// input blob size. Each kind has well over `cap` elements.
fn write_large_blob_unsort_fixture(
    path: &Path,
) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let nodes = generate_nodes(60, 1);
    let ways = generate_ways(60, 1, 3, 1);
    let rels = generate_relations(60, 1, 2, 1);
    write_multi_block_test_pbf(path, &nodes, &ways, &rels, 20);
    (nodes, ways, rels)
}

/// The `--block-cap` the large-blob test passes. Smaller than the
/// large-blob fixture's 20-element input blobs so a single input blob
/// spans more than one output block.
const LARGE_BLOB_CAP: &str = "5";

/// Per-blob `(kind, ordered element ids)` from the output file. Element
/// ids are returned in stream order so callers can detect intra-blob
/// inversions (a descending step within one blob).
fn blob_elements(path: &Path) -> Vec<(BlobKindLabel, Vec<i64>)> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut out = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            let mut ids = Vec::new();
            let mut nodes = 0;
            let mut ways = 0;
            for element in block.elements() {
                match element {
                    Element::Node(n) => {
                        nodes += 1;
                        ids.push(n.id());
                    }
                    Element::DenseNode(dn) => {
                        nodes += 1;
                        ids.push(dn.id());
                    }
                    Element::Way(w) => {
                        ways += 1;
                        ids.push(w.id());
                    }
                    Element::Relation(r) => {
                        ids.push(r.id());
                    }
                    _ => {}
                }
            }
            let kind = if nodes > 0 {
                BlobKindLabel::Node
            } else if ways > 0 {
                BlobKindLabel::Way
            } else {
                BlobKindLabel::Relation
            };
            out.push((kind, ids));
        }
    }
    out
}

/// Count adjacent same-kind blob pairs with overlapping ID ranges
/// (`max_id` of one blob >= `min_id` of the next same-kind blob). The CLI
/// promises exactly one such overlap per eligible kind under `--unsort`,
/// so tests assert on the count, not just presence.
fn count_adjacent_overlaps(
    blobs: &[(BlobKindLabel, i64, i64, usize)],
    kind: BlobKindLabel,
) -> usize {
    let same: Vec<_> = blobs.iter().filter(|(k, ..)| *k == kind).collect();
    same.windows(2)
        .filter(|w| {
            let (_, _, a_max, _) = w[0];
            let (_, b_min, _, _) = w[1];
            a_max >= b_min
        })
        .count()
}

/// Count strictly-descending steps (internal ID inversions) across all
/// blobs of `kind`. Each unsort swap contributes exactly one, so
/// `--unsort-intra` tests assert this equals one per eligible kind and
/// `--unsort` tests assert it equals zero.
fn count_intra_blob_inversions(blobs: &[(BlobKindLabel, Vec<i64>)], kind: BlobKindLabel) -> usize {
    blobs
        .iter()
        .filter(|(k, _)| *k == kind)
        .map(|(_, ids)| ids.windows(2).filter(|w| w[0] > w[1]).count())
        .sum()
}

/// Per-blob `(kind, min_id, max_id, count)` from the output file. Used
/// to assert overlap structure after `--unsort`.
fn blob_index_summary(path: &Path) -> Vec<(BlobKindLabel, i64, i64, usize)> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut out = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            let mut min_id = i64::MAX;
            let mut max_id = i64::MIN;
            let mut nodes = 0;
            let mut ways = 0;
            let mut rels = 0;
            for element in block.elements() {
                match element {
                    Element::Node(n) => {
                        nodes += 1;
                        min_id = min_id.min(n.id());
                        max_id = max_id.max(n.id());
                    }
                    Element::DenseNode(dn) => {
                        nodes += 1;
                        min_id = min_id.min(dn.id());
                        max_id = max_id.max(dn.id());
                    }
                    Element::Way(w) => {
                        ways += 1;
                        min_id = min_id.min(w.id());
                        max_id = max_id.max(w.id());
                    }
                    Element::Relation(r) => {
                        rels += 1;
                        min_id = min_id.min(r.id());
                        max_id = max_id.max(r.id());
                    }
                    _ => {}
                }
            }
            let kind = if nodes > 0 {
                BlobKindLabel::Node
            } else if ways > 0 {
                BlobKindLabel::Way
            } else {
                BlobKindLabel::Relation
            };
            let count = nodes + ways + rels;
            out.push((kind, min_id, max_id, count));
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlobKindLabel {
    Node,
    Way,
    Relation,
}

// ---------------------------------------------------------------------------
// --strip-indexdata
// ---------------------------------------------------------------------------

/// `--strip-indexdata` clears the BlobHeader.indexdata field on every
/// OsmData blob. Element semantics, sortedness, and `LocationsOnWays`
/// (when set) all pass through unchanged because the blob payload is
/// not touched.
#[test]
fn degrade_strip_indexdata_drops_indexdata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);
    assert_indexed(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-indexdata")
        .assert_success();

    assert_non_indexed(&output);

    // Sortedness preserved (the blob payload is unchanged; only the
    // BlobHeader.indexdata is cleared).
    assert!(
        read_header(&output).is_sorted(),
        "--strip-indexdata should not clear Sort.Type_then_ID"
    );

    // Element semantics preserved.
    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

// ---------------------------------------------------------------------------
// --strip-locations
// ---------------------------------------------------------------------------

/// `--strip-locations` clears the `LocationsOnWays` header feature. The
/// fixture genuinely declares LOW (its ways carry inline coordinates and the
/// header sets the optional feature), so the assertion below is non-vacuous:
/// the flag really removes a feature the input had, rather than confirming a
/// no-op on a header that never declared it. Element data (ids, tags, refs,
/// members) round-trips through the BlockBuilder re-encode.
#[test]
fn degrade_strip_locations_clears_low_and_preserves_elements() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_low_fixture(&input);
    // Precondition: the input actually declares LocationsOnWays, so clearing
    // it is a real change.
    assert!(
        read_header(&input).has_locations_on_ways(),
        "fixture must declare LocationsOnWays for the strip to be meaningful"
    );

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-locations")
        .assert_success();

    assert!(
        !read_header(&output).has_locations_on_ways(),
        "--strip-locations output must not declare LocationsOnWays"
    );

    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

// ---------------------------------------------------------------------------
// --strip-tagdata
// ---------------------------------------------------------------------------

/// `--strip-tagdata` clears the BlobHeader.tagdata field (the per-blob tag
/// key index) on every OsmData blob, forcing `tags-filter`'s no-hint
/// fallback path. Like `--strip-indexdata` it is a header-only passthrough:
/// indexdata, sortedness, and every element property pass through unchanged
/// because the blob payload is not touched. Crucially it leaves indexdata
/// intact - a tagdata-stripped file is still indexed.
#[test]
fn degrade_strip_tagdata_drops_tagdata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);
    // Precondition: the fixture carries tagdata in more than one blob (tagged
    // nodes in two node blobs plus a tagged way and relation), so a stripper
    // that only cleared the first blob would be caught by the whole-file walk
    // below rather than passing a single-blob assertion.
    assert_has_tagdata(&input);
    assert!(
        count_tagdata_blobs(&input) > 1,
        "fixture must carry tagdata in more than one blob to make the \
         whole-file strip assertion meaningful, got {}",
        count_tagdata_blobs(&input)
    );
    assert_indexed(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-tagdata")
        .assert_success();

    // Every output blob must be free of tagdata, not just the first.
    assert_no_tagdata_all_blobs(&output);

    // indexdata is preserved (only tagdata is targeted). The passthrough keeps
    // the original indexdata bytes verbatim, so the file is still indexed.
    assert_indexed(&output);

    // Sortedness preserved (the blob payload is unchanged; only the
    // BlobHeader.tagdata is cleared).
    assert!(
        read_header(&output).is_sorted(),
        "--strip-tagdata should not clear Sort.Type_then_ID"
    );

    // Element semantics preserved.
    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

// ---------------------------------------------------------------------------
// --strip-bbox
// ---------------------------------------------------------------------------

/// `--strip-bbox` clears the `HeaderBlock.bbox` while leaving every OsmData
/// blob untouched: it is a header-only passthrough, so indexdata, tagdata,
/// sortedness, and the element multiset all survive.
#[test]
fn degrade_strip_bbox_clears_header_bbox() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_bbox_fixture(&input);
    // Precondition: the fixture actually declares a bbox AND a full set of
    // other header fields, so the strip is meaningful and the survival
    // assertions below are not vacuous.
    assert!(
        read_header(&input).bbox().is_some(),
        "fixture must declare a HeaderBlock.bbox for the strip to be meaningful"
    );
    assert_rich_header_fields_present(&input);
    assert_indexed(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-bbox")
        .assert_success();

    assert!(
        read_header(&output).bbox().is_none(),
        "--strip-bbox output must not declare a HeaderBlock.bbox"
    );

    // The bbox is the ONLY header field that changes: source, writingprogram,
    // the custom optional feature, replication metadata, and sortedness all
    // survive verbatim. A HeaderBuilder rebuild would have dropped or reset
    // several of these, so this pins the verbatim-passthrough contract.
    assert_rich_header_fields_survived(&output);

    // Header-only passthrough: indexdata is untouched.
    assert_indexed(&output);

    // The OsmData blob frames are copied byte-for-byte - the strip touches
    // only the OSMHeader.
    assert_eq!(
        osm_data_frames(&output),
        osm_data_frames(&input),
        "--strip-bbox must leave every OsmData frame byte-identical"
    );

    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `--strip-bbox` on a *non-indexed* bbox-bearing input succeeds without
/// `--force`, proving it carries no indexdata precondition and does not
/// switch to the decode path. The decode path calls `require_indexdata` and
/// would error on a non-indexed input without `--force`; a clean success
/// therefore proves the run stayed on the header-only passthrough. The output
/// stays non-indexed, the bbox is gone, the other header fields survive, and
/// the OsmData frames are copied verbatim.
#[test]
fn degrade_strip_bbox_no_indexdata_uses_passthrough() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_non_indexed_bbox_fixture(&input);
    assert_non_indexed(&input);
    assert!(read_header(&input).bbox().is_some());
    assert_rich_header_fields_present(&input);

    // No --force: a decode-path dispatch would fail the indexdata precondition.
    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-bbox")
        .assert_success();

    assert!(read_header(&output).bbox().is_none());
    // Passthrough on a non-indexed input leaves it non-indexed.
    assert_non_indexed(&output);
    assert_rich_header_fields_survived(&output);
    assert_eq!(
        osm_data_frames(&output),
        osm_data_frames(&input),
        "non-indexed --strip-bbox must copy OsmData frames byte-for-byte"
    );

    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `--strip-bbox --strip-indexdata` composes on the passthrough path: the
/// bbox is dropped from the OSMHeader *and* indexdata is cleared from every
/// OsmData blob, while sortedness and the element multiset survive.
#[test]
fn degrade_strip_bbox_and_strip_indexdata_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_bbox_fixture(&input);
    assert!(read_header(&input).bbox().is_some());
    assert_indexed(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-bbox")
        .arg("--strip-indexdata")
        .assert_success();

    assert!(read_header(&output).bbox().is_none());
    assert_non_indexed(&output);
    assert!(read_header(&output).is_sorted());

    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `--strip-bbox --strip-locations` composes across the path boundary:
/// `--strip-locations` forces the decode path, and `--strip-bbox` must still
/// drop the bbox from the rebuilt output header. Confirms the bbox strip is
/// wired into the decode path's header construction, not just the
/// passthrough path's.
#[test]
fn degrade_strip_bbox_and_strip_locations_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_bbox_fixture(&input);
    assert!(read_header(&input).bbox().is_some());

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-bbox")
        .arg("--strip-locations")
        .assert_success();

    assert!(
        read_header(&output).bbox().is_none(),
        "--strip-bbox output must not declare a HeaderBlock.bbox (decode path)"
    );
    assert!(
        !read_header(&output).has_locations_on_ways(),
        "--strip-locations output must not declare LocationsOnWays"
    );

    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `extract --bbox` on the stripped output produces the same elements as
/// `extract --bbox` on the original bbox-bearing input. `extract` derives
/// its region purely from the CLI `--bbox` argument and prunes blobs with
/// the per-blob `indexdata` bboxes; it never consults the `HeaderBlock`
/// bbox, so removing it is inert for extract correctness. This pins that
/// invariant end-to-end: the degraded file stays a valid extract input.
#[test]
fn degrade_strip_bbox_extract_bbox_matches_original() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let stripped = dir.path().join("stripped.osm.pbf");
    let extract_orig = dir.path().join("extract_orig.osm.pbf");
    let extract_stripped = dir.path().join("extract_stripped.osm.pbf");

    write_bbox_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&stripped)
        .arg("--strip-bbox")
        .assert_success();
    assert!(read_header(&stripped).bbox().is_none());

    // A sub-region of the fixture's coordinate span so the extract is
    // non-empty but not the whole file. osmium order: minlon,minlat,maxlon,maxlat.
    let region = "0.0,0.0,0.00055,0.00055";
    for (src, out) in [(&input, &extract_orig), (&stripped, &extract_stripped)] {
        CliInvoker::new()
            .arg("extract")
            .arg(src)
            .arg("-o")
            .arg(out)
            .arg("--bbox")
            .arg(region)
            .assert_success();
    }

    let orig = read_normalized(&extract_orig);
    let strip = read_normalized(&extract_stripped);
    let full = read_normalized(&input);
    // Guard against a vacuous (empty) extract passing trivially.
    assert!(
        !orig.nodes.is_empty(),
        "extract region must select at least one node"
    );
    // Prove the spatial filter really ran: the sub-region must select
    // strictly fewer nodes than the whole file. If extract returned every
    // element the "matches" comparison below would be trivially satisfiable
    // by a no-op passthrough.
    assert!(
        orig.nodes.len() < full.nodes.len(),
        "extract must select strictly fewer than all {} nodes, got {}",
        full.nodes.len(),
        orig.nodes.len()
    );
    assert_eq!(orig.nodes, strip.nodes);
    assert_eq!(orig.ways, strip.ways);
    assert_eq!(orig.relations, strip.relations);
}

/// `--strip-bbox --generator <name>` on a passthrough-eligible bbox-bearing
/// input takes the *rebuild* branch of `passthrough_header_bytes`, not the
/// verbatim-forward branch: `--generator` is a `HeaderOverrides` override, so
/// `passthrough_header_bytes` rebuilds the header via `HeaderBuilder` (bbox
/// omitted under `--strip-bbox`) instead of forwarding the decompressed
/// `HeaderBlock` protobuf field-for-field. Two independent signals pin this:
/// the bbox is gone (proving the strip still applied) AND `writingprogram`
/// equals the override rather than the fixture's original value (proving the
/// override actually took effect, which only happens on the rebuild path -
/// the verbatim path has no override plumbing at all). Without this test the
/// rebuild branch in `passthrough_header_bytes` had no direct coverage; every
/// other `--strip-bbox` test above exercises only the verbatim branch.
#[test]
fn degrade_strip_bbox_with_generator_override_rebuilds_header() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_bbox_fixture(&input);
    assert!(
        read_header(&input).bbox().is_some(),
        "fixture must declare a HeaderBlock.bbox for the strip to be meaningful"
    );
    assert_eq!(
        read_header(&input).writing_program(),
        Some(FIXTURE_WRITING_PROGRAM),
        "fixture's original writingprogram must differ from the override below"
    );

    const OVERRIDE_GENERATOR: &str = "degrade-rebuild-override/9.0";

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-bbox")
        .arg("--generator")
        .arg(OVERRIDE_GENERATOR)
        .assert_success();

    let out_header = read_header(&output);
    assert!(
        out_header.bbox().is_none(),
        "--strip-bbox must still clear HeaderBlock.bbox on the override-rebuild path"
    );
    assert_eq!(
        out_header.writing_program(),
        Some(OVERRIDE_GENERATOR),
        "--generator must win on the rebuild path, proving passthrough_header_bytes \
         took the HeaderBuilder rebuild branch rather than the verbatim-forward branch"
    );

    // Element semantics preserved: only the header changed.
    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

// ---------------------------------------------------------------------------
// --unsort
// ---------------------------------------------------------------------------

/// `--unsort` clears `Sort.Type_then_ID` and produces genuine cross-blob
/// overlap: at least one adjacent same-kind blob pair whose indexdata ID
/// ranges overlap, per kind that has more than `block_cap + 1` elements.
///
/// The fixture packs input at 4 elements/blob and the run uses
/// `--block-cap 10`, so input blobs are smaller than the cap. This is the
/// regime that regressed before the fix (the per-input-blob boundary
/// flush confined the swap to one output blob and `detect_overlaps`
/// returned zero). The central builder must now pack continuously across
/// input blobs so the swap straddles a real output-blob boundary. The two
/// straddling blobs stay internally ID-monotone - the disorder lives at
/// the inter-blob seam, not inside a blob.
#[test]
fn degrade_unsort_creates_adjacent_overlap_per_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    assert_unsort_cross_blob_shape(&output, &input);
}

/// Shared assertions for the `--unsort` cross-blob shape: header sortedness
/// cleared, exactly one adjacent cross-blob overlap per kind, zero
/// intra-blob inversions (each blob internally ID-monotone), element
/// multiset preserved.
fn assert_unsort_cross_blob_shape(output: &Path, input: &Path) {
    assert!(
        !read_header(output).is_sorted(),
        "--unsort output must not declare Sort.Type_then_ID"
    );

    let summary = blob_index_summary(output);
    let elements = blob_elements(output);
    for kind in [
        BlobKindLabel::Node,
        BlobKindLabel::Way,
        BlobKindLabel::Relation,
    ] {
        let same_count = summary.iter().filter(|(k, ..)| *k == kind).count();
        assert!(
            same_count >= 2,
            "kind {kind:?}: need at least 2 blobs to verify overlap, got {same_count}"
        );
        // The CLI promises exactly one adjacent cross-blob overlap per
        // eligible kind - the minimum perturbation that fires sort's
        // detect_overlaps. Count it, don't just check presence.
        assert_eq!(
            count_adjacent_overlaps(&summary, kind),
            1,
            "kind {kind:?}: expected exactly one adjacent cross-blob overlap, \
             blobs were {:?}",
            summary
                .iter()
                .filter(|(k, ..)| *k == kind)
                .collect::<Vec<_>>()
        );
        // The overlap is expressed cross-blob; each blob stays internally
        // ID-monotone (this is what separates --unsort from --unsort-intra).
        assert_eq!(
            count_intra_blob_inversions(&elements, kind),
            0,
            "kind {kind:?}: --unsort blobs must be internally ID-monotone, \
             blobs were {:?}",
            elements
                .iter()
                .filter(|(k, _)| *k == kind)
                .collect::<Vec<_>>()
        );
    }

    // Element multiset preserved (just reordered).
    let original = read_normalized(input);
    let degraded = read_normalized(output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `--unsort-intra` clears `Sort.Type_then_ID` and produces the intra-blob
/// adversarial shape: exactly one same-kind blob per kind has an internal
/// ID-order inversion, but no adjacent same-kind blob pair overlaps.
///
/// This is the shape that slips past a blob-range overlap check: `sort`
/// decides whether to rewrite by comparing adjacent same-kind blobs'
/// `(min_id, max_id)` ranges, and here every blob's range stays disjoint
/// from its neighbours even though one blob is internally unsorted. So the
/// stream is genuinely out of order while a range-only check sees nothing
/// to fix and the header no longer claims sortedness - a monotonicity
/// blind spot for any consumer that trusts declared sortedness plus
/// non-overlapping ranges.
#[test]
fn degrade_unsort_intra_creates_intra_blob_inversion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort-intra")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    assert_unsort_intra_shape(&output, &input);
}

/// `--unsort-intra` stays intra-blob even when a single input blob carries
/// more than `--block-cap` same-kind elements (finding 1's regime). The
/// fixture packs 20 elements/blob and the run caps output blocks at 5, so
/// each input blob spans four output blocks. The old shared swap keyed to
/// the cap boundary would have filled and flushed a block here, producing
/// the cross-blob overlap shape; the fix keys the swap to the first two
/// elements so it lands at the start of the first output block.
#[test]
fn degrade_unsort_intra_large_input_blobs_stay_intra_blob() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_large_blob_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort-intra")
        .arg("--block-cap")
        .arg(LARGE_BLOB_CAP)
        .assert_success();

    assert_unsort_intra_shape(&output, &input);
}

/// Shared assertions for the `--unsort-intra` shape: header sortedness
/// cleared, exactly one intra-blob inversion per kind, zero cross-blob
/// overlaps, element multiset preserved.
fn assert_unsort_intra_shape(output: &Path, input: &Path) {
    assert!(
        !read_header(output).is_sorted(),
        "--unsort-intra output must not declare Sort.Type_then_ID"
    );

    let summary = blob_index_summary(output);
    let elements = blob_elements(output);
    for kind in [
        BlobKindLabel::Node,
        BlobKindLabel::Way,
        BlobKindLabel::Relation,
    ] {
        assert_eq!(
            count_intra_blob_inversions(&elements, kind),
            1,
            "kind {kind:?}: expected exactly one intra-blob inversion, \
             blobs were {:?}",
            elements
                .iter()
                .filter(|(k, _)| *k == kind)
                .collect::<Vec<_>>()
        );
        // No cross-blob overlap: this is exactly the shape a blob-range
        // overlap check cannot see.
        assert_eq!(
            count_adjacent_overlaps(&summary, kind),
            0,
            "kind {kind:?}: --unsort-intra must not produce cross-blob overlap, \
             blobs were {:?}",
            summary
                .iter()
                .filter(|(k, ..)| *k == kind)
                .collect::<Vec<_>>()
        );
    }

    // Element multiset preserved (just reordered).
    let original = read_normalized(input);
    let degraded = read_normalized(output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `--unsort` and `--unsort-intra` are mutually exclusive.
#[test]
fn degrade_unsort_and_unsort_intra_are_mutually_exclusive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--unsort-intra")
        .assert_failure()
        .assert_stderr_contains("unsort-intra");
}

/// `--unsort` output piped through `pbfhogg sort` recovers the original
/// element set with `Sort.Type_then_ID` re-declared. Closes the loop on
/// the design's primary use case: the cross-blob overlap must actually
/// reach `sort`'s overlap-rewrite path.
#[test]
fn degrade_unsort_then_sort_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let unsorted = dir.path().join("unsorted.osm.pbf");
    let resorted = dir.path().join("resorted.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&unsorted)
        .arg("--unsort")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    let sort_out = CliInvoker::new()
        .arg("sort")
        .arg(&unsorted)
        .arg("-o")
        .arg(&resorted)
        .arg("--force")
        .assert_success();

    // Prove sort actually hit the overlap-rewrite path rather than passing
    // the file through untouched. Sort prints this line only when
    // detect_overlaps flags at least one blob run for decode + re-encode;
    // the cross-blob overlap --unsort injects is what makes it fire. (A
    // full passthrough would print nothing here.)
    sort_out.assert_stderr_contains("blobs in overlap runs");

    // Prove the output is genuinely sorted in file order, not merely
    // element-equivalent. read_normalized re-sorts every section before
    // comparison, so it would accept a stream that sort left disordered;
    // assert_sorted_file walks the file in blob order and checks the
    // header flag plus per-type monotonicity, catching a passthrough that
    // never repaired the overlap.
    assert_sorted_file(&resorted);

    let original = read_normalized(&input);
    let recovered = read_normalized(&resorted);
    assert_eq!(original.nodes, recovered.nodes);
    assert_eq!(original.ways, recovered.ways);
    assert_eq!(original.relations, recovered.relations);
}

// ---------------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------------

/// `--unsort --strip-indexdata` composes: output is unsorted *and*
/// unindexed.
#[test]
fn degrade_unsort_and_strip_indexdata_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--strip-indexdata")
        .arg("--block-cap")
        .arg("5")
        .assert_success();

    assert_non_indexed(&output);
    assert!(!read_header(&output).is_sorted());

    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `--unsort --strip-locations` composes: the cross-blob overlap shape is
/// preserved *and* `LocationsOnWays` is cleared. Confirms the swap logic
/// still fires when the ways go through the coordinate-dropping re-encode.
#[test]
fn degrade_unsort_and_strip_locations_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--strip-locations")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    assert!(
        !read_header(&output).has_locations_on_ways(),
        "--strip-locations output must not declare LocationsOnWays"
    );
    assert_unsort_cross_blob_shape(&output, &input);
}

/// `--unsort-intra --strip-locations` composes: the intra-blob inversion
/// shape is preserved *and* `LocationsOnWays` is cleared.
#[test]
fn degrade_unsort_intra_and_strip_locations_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort-intra")
        .arg("--strip-locations")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    assert!(
        !read_header(&output).has_locations_on_ways(),
        "--strip-locations output must not declare LocationsOnWays"
    );
    assert_unsort_intra_shape(&output, &input);
}

/// `--unsort-intra --strip-indexdata` composes: output is intra-blob
/// unsorted *and* unindexed.
#[test]
fn degrade_unsort_intra_and_strip_indexdata_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort-intra")
        .arg("--strip-indexdata")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    assert_non_indexed(&output);
    assert_unsort_intra_shape(&output, &input);
}

/// `--strip-tagdata --strip-indexdata` composes on the passthrough path:
/// both header fields are cleared while the blob payload (and sortedness)
/// pass through untouched.
#[test]
fn degrade_strip_tagdata_and_strip_indexdata_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);
    assert_has_tagdata(&input);
    assert_indexed(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-tagdata")
        .arg("--strip-indexdata")
        .assert_success();

    assert_no_tagdata_all_blobs(&output);
    assert_non_indexed(&output);
    // Passthrough leaves the payload alone, so sortedness survives.
    assert!(read_header(&output).is_sorted());

    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `--unsort --strip-tagdata` composes on the decode path: the elements are
/// re-encoded (unsorted) *and* every output blob is emitted without tagdata.
/// This exercises the `frame_and_write_batch` tagdata=None path that the
/// merge thread uses under either unsort mode.
#[test]
fn degrade_unsort_and_strip_tagdata_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);
    assert_has_tagdata(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--strip-tagdata")
        .arg("--block-cap")
        .arg("5")
        .assert_success();

    assert_no_tagdata(&output);
    assert!(!read_header(&output).is_sorted());

    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `--strip-locations --strip-tagdata` composes on the decode path's
/// non-unsort shape: with `--block-cap 10` against the fixture's 20-element
/// input blobs, workers pre-frame full cap-blocks via `frame_owned`, so this
/// exercises the `frame_owned` tagdata=None path (distinct from the merge
/// thread's batch path above). `LocationsOnWays` is cleared and no output
/// blob carries tagdata.
#[test]
fn degrade_strip_locations_and_strip_tagdata_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);
    assert_has_tagdata(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-locations")
        .arg("--strip-tagdata")
        .arg("--block-cap")
        .arg("10")
        .assert_success();

    assert_no_tagdata(&output);
    assert!(
        !read_header(&output).has_locations_on_ways(),
        "--strip-locations output must not declare LocationsOnWays"
    );

    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Running `degrade` with no transformation flags is rejected.
#[test]
fn degrade_requires_at_least_one_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .assert_failure()
        .assert_stderr_contains("at least one transformation flag");
}

// ---------------------------------------------------------------------------
// --drop-ids
// ---------------------------------------------------------------------------

fn normalized_count(path: &Path) -> usize {
    let pbf = read_normalized(path);
    pbf.nodes.len() + pbf.ways.len() + pbf.relations.len()
}

/// Extract an unsigned-integer field from the `refs` object of a
/// `check --refs --json` document. The output is pretty-printed one field
/// per line as `"name": value,`; the closing quote in the needle makes the
/// match exact (so `missing_relation_members` never matches
/// `missing_relation_member_occurrences`). Kept dependency-free because the
/// integration-test crate has no `serde_json` dev-dependency.
fn refs_field(stdout: &str, field: &str) -> u64 {
    let needle = format!("\"{field}\"");
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with(&needle))
        .unwrap_or_else(|| panic!("field {field:?} not found in check --refs json:\n{stdout}"));
    let value = line
        .split_once(':')
        .expect("json field line has a colon")
        .1
        .trim()
        .trim_end_matches(',');
    value
        .parse()
        .unwrap_or_else(|_| panic!("field {field:?} value {value:?} is not an integer"))
}

/// Compute the four unique dangling-reference counts hash-independently from
/// the degrade *output* alone, per the spec's Section 7.3 definition: a
/// reference dangles iff its target `(kind, id)` is absent from the output.
/// Returns `(missing_node_refs, missing_way_refs, missing_node_members,
/// missing_relation_members)`.
fn expected_dangles(out: &common::NormalizedPbf) -> (u64, u64, u64, u64) {
    use std::collections::BTreeSet;
    let node_ids: BTreeSet<i64> = out.nodes.iter().map(|n| n.id).collect();
    let way_ids: BTreeSet<i64> = out.ways.iter().map(|w| w.id).collect();
    let rel_ids: BTreeSet<i64> = out.relations.iter().map(|r| r.id).collect();

    let mut missing_node_refs = BTreeSet::new();
    for w in &out.ways {
        for r in &w.refs {
            if !node_ids.contains(r) {
                missing_node_refs.insert(*r);
            }
        }
    }

    let mut missing_node_members = BTreeSet::new();
    let mut missing_way_refs = BTreeSet::new();
    let mut missing_relation_members = BTreeSet::new();
    for rel in &out.relations {
        for m in &rel.members {
            match m.member_type.as_str() {
                "node" if !node_ids.contains(&m.ref_id) => {
                    missing_node_members.insert(m.ref_id);
                }
                "way" if !way_ids.contains(&m.ref_id) => {
                    missing_way_refs.insert(m.ref_id);
                }
                "relation" if !rel_ids.contains(&m.ref_id) => {
                    missing_relation_members.insert(m.ref_id);
                }
                _ => {}
            }
        }
    }
    (
        missing_node_refs.len() as u64,
        missing_way_refs.len() as u64,
        missing_node_members.len() as u64,
        missing_relation_members.len() as u64,
    )
}

/// Run `check --refs --check-relations --json` on `path` and assert its four
/// `missing_*` fields equal the hash-independent expectation derived from the
/// output. `check` exits 1 when integrity fails while still printing the JSON,
/// so this uses `run()` (not `assert_success`) and reads stdout. Returns the
/// four-field sum so callers can guard against a vacuous (zero-dangle) run.
fn assert_check_refs_matches_output(path: &Path) -> u64 {
    let out = read_normalized(path);
    let (mnr, mwr, mnm, mrm) = expected_dangles(&out);
    let check = CliInvoker::new()
        .arg("check")
        .arg(path)
        .arg("--refs")
        .arg("--check-relations")
        .arg("--json")
        .run();
    let stdout = check.stdout_str();
    assert_eq!(
        refs_field(&stdout, "missing_node_refs"),
        mnr,
        "missing_node_refs"
    );
    assert_eq!(
        refs_field(&stdout, "missing_way_refs"),
        mwr,
        "missing_way_refs"
    );
    assert_eq!(
        refs_field(&stdout, "missing_node_members"),
        mnm,
        "missing_node_members"
    );
    assert_eq!(
        refs_field(&stdout, "missing_relation_members"),
        mrm,
        "missing_relation_members"
    );
    mnr + mwr + mnm + mrm
}

/// The consumer contract: dropping referenced elements makes surviving
/// ways/relations dangle, and `check --refs` reports exactly the dangles the
/// output structure implies. Expectations are computed hash-independently from
/// the output (spec Section 7.3), so the test survives a fixture tweak; the
/// pinned `10:16` is verified to drop nodes 2 and 3 (referenced by every way)
/// and way 2 (a member of every relation), so the four-field sum is > 0.
#[test]
fn degrade_drop_ids_dangling_refs_match_check_refs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_degrade_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--drop-ids")
        .arg("10:16")
        .assert_success();

    assert_eq!(normalized_count(&output), normalized_count(&input) - 10);
    let sum = assert_check_refs_matches_output(&output);
    assert!(
        sum > 0,
        "10:16 must drop referenced elements so dangles are produced, got sum 0"
    );
}

#[test]
fn degrade_drop_ids_removes_exactly_n() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_degrade_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--drop-ids")
        .arg("10:1")
        .assert_success();

    assert_eq!(normalized_count(&output), normalized_count(&input) - 10);
    assert!(read_header(&output).is_sorted());
    assert_sorted_file(&output);
}

#[test]
fn degrade_drop_ids_is_reproducible_and_seed_changes_selection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let first = dir.path().join("first.osm.pbf");
    let second = dir.path().join("second.osm.pbf");
    let third = dir.path().join("third.osm.pbf");
    write_degrade_fixture(&input);
    for (output, spec) in [(&first, "10:7"), (&second, "10:7"), (&third, "10:8")] {
        CliInvoker::new()
            .arg("degrade")
            .arg(&input)
            .arg("-o")
            .arg(output)
            .arg("--drop-ids")
            .arg(spec)
            .assert_success();
    }
    assert_eq!(
        std::fs::read(&first).expect("first"),
        std::fs::read(&second).expect("second")
    );
    assert_ne!(
        std::fs::read(&first).expect("first"),
        std::fs::read(&third).expect("third")
    );
}

#[test]
fn degrade_drop_ids_validates_arguments_and_total() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_degrade_fixture(&input);
    for (spec, message) in [
        ("0:1", "N must be >= 1"),
        ("10", "N:SEED"),
        ("1000000:1", "input has only"),
    ] {
        CliInvoker::new()
            .arg("degrade")
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .arg("--drop-ids")
            .arg(spec)
            .assert_failure()
            .assert_stderr_contains(message);
    }
}

/// `--block-cap 0` is rejected up front.
#[test]
fn degrade_rejects_zero_block_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--block-cap")
        .arg("0")
        .assert_failure()
        .assert_stderr_contains("must be > 0");
}

/// `--unsort-intra --block-cap 1` is rejected: an intra-blob inversion
/// needs two same-kind elements in one output block, which a cap of 1
/// cannot hold. Rejecting up front avoids a silent no-op that would still
/// clear Sort.Type_then_ID.
#[test]
fn degrade_unsort_intra_rejects_block_cap_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort-intra")
        .arg("--block-cap")
        .arg("1")
        .assert_failure()
        .assert_stderr_contains("block-cap >= 2");
}

/// `--unsort --block-cap 1` is supported (not a silent no-op): each output
/// blob holds one element, and swapping the first two adjacent
/// single-element blobs produces exactly one descending cross-blob step -
/// the same overlap shape sort's detect_overlaps fires on.
#[test]
fn degrade_unsort_accepts_block_cap_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--block-cap")
        .arg("1")
        .assert_success();

    assert_unsort_cross_blob_shape(&output, &input);
}

mod tier2 {
    use super::*;

    #[test]
    fn degrade_drop_ids_and_strip_locations_compose() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("in.osm.pbf");
        let output = dir.path().join("out.osm.pbf");
        write_degrade_fixture(&input);
        let input_count = normalized_count(&input);
        CliInvoker::new()
            .arg("degrade")
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .args(["--drop-ids", "10:16", "--strip-locations"])
            .assert_success();
        assert_eq!(normalized_count(&output), input_count - 10);
        assert!(!read_header(&output).has_locations_on_ways());
        assert!(read_header(&output).is_sorted());
    }

    #[test]
    fn degrade_drop_ids_and_strip_indexdata_compose() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("in.osm.pbf");
        let output = dir.path().join("out.osm.pbf");
        write_degrade_fixture(&input);
        let input_count = normalized_count(&input);
        CliInvoker::new()
            .arg("degrade")
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .args(["--drop-ids", "10:16", "--strip-indexdata"])
            .assert_success();
        assert_eq!(normalized_count(&output), input_count - 10);
        assert_non_indexed(&output);
    }

    #[test]
    fn degrade_drop_ids_and_unsort_compose() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("in.osm.pbf");
        let output = dir.path().join("out.osm.pbf");
        write_unsort_fixture(&input);
        let input_count = normalized_count(&input);
        CliInvoker::new()
            .arg("degrade")
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .args(["--drop-ids", "10:16", "--unsort", "--block-cap", UNSORT_CAP])
            .assert_success();
        assert_eq!(normalized_count(&output), input_count - 10);
        assert!(!read_header(&output).is_sorted());
        for kind in [
            BlobKindLabel::Node,
            BlobKindLabel::Way,
            BlobKindLabel::Relation,
        ] {
            assert_eq!(
                count_adjacent_overlaps(&blob_index_summary(&output), kind),
                1
            );
            assert_eq!(
                count_intra_blob_inversions(&blob_elements(&output), kind),
                0
            );
        }
        // The consumer contract must still hold on an unsorted-but-kind-
        // separated file: check --refs reports exactly the dangles the output
        // structure implies (spec Section 8.2 #9). Not merely a cleared flag.
        let sum = assert_check_refs_matches_output(&output);
        assert!(sum > 0, "10:16 on the unsort fixture must produce dangles");
    }
}
