//! The history time-filter on blobs `BlockBuilder` never writes: one blob
//! holding several element kinds.
//!
//! The history path encodes a blob's last open block twice when the blob's
//! last object has a visible version at or before the cutoff: once without
//! that candidate, and once on a clone of the builder with it appended, the
//! stitcher choosing between the two once the next blob shows whether the
//! object continues. When the candidate is of another kind than the open
//! block, the alternative ending closes the open block and starts a block of
//! the candidate's kind. The fixtures here put nodes and the candidate way
//! in one blob, so that crossing is what the stitcher writes or drops.

use std::path::Path;

use pbfhogg::block_builder::HeaderBuilder;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{Element, ElementReader};

use crate::common::groups::{Versioned, versioned_block};

const CUTOFF: i64 = 250;

/// An output element: kind, ID, version.
type Written = (char, i64, i32);

/// A history file (sorted, `HistoricalInformation`) of `blobs`, each one
/// block of its groups, no blob carrying indexdata.
fn write_history(path: &Path, blobs: &[&[&[Versioned]]]) {
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(
            &HeaderBuilder::new()
                .sorted()
                .historical()
                .build()
                .expect("build header"),
        )
        .expect("write header");
    for groups in blobs {
        writer
            .write_primitive_block_no_indexdata(&versioned_block(groups))
            .expect("write block");
    }
    writer.flush().expect("flush fixture");
}

/// Filter `input` at [`CUTOFF`]: the written elements in file order, and
/// the element kinds of each output block.
fn run(input: &Path, dir: &Path) -> (Vec<Written>, Vec<Vec<char>>) {
    let output = dir.join("out.osm.pbf");
    pbfhogg::time_filter::time_filter(
        input,
        crate::common::out(&output),
        CUTOFF,
        &Default::default(),
        &Default::default(),
        &crate::common::test_host(),
    )
    .expect("time-filter runs");
    let mut written = Vec::new();
    let mut blocks = Vec::new();
    ElementReader::open(&output, false)
        .expect("open output")
        .for_each_block_pipelined(|block| {
            let mut kinds = Vec::new();
            for element in block.elements() {
                let (kind, id, version) = match element {
                    Element::DenseNode(n) => ('n', n.id(), n.info().expect("info").version()),
                    Element::Node(n) => ('n', n.id(), n.info().version().expect("version")),
                    Element::Way(w) => ('w', w.id(), w.info().version().expect("version")),
                    Element::Relation(r) => ('r', r.id(), r.info().version().expect("version")),
                    other => panic!("unexpected element {other:?}"),
                };
                if kinds.last() != Some(&kind) {
                    kinds.push(kind);
                }
                written.push((kind, id, version));
            }
            blocks.push(kinds);
            Ok(())
        })
        .expect("read output");
    (written, blocks)
}

/// The first blob holds nodes and then way 10, whose version 1 is the
/// blob's candidate. The next blob's version 2 of way 10 is past the
/// cutoff, so the candidate's ending is chosen: the node block closed and
/// way 10's version 1 in a way block of its own.
#[test]
fn a_held_ending_crossing_a_kind_boundary_is_written_when_its_candidate_stands() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("in.osh.pbf");
    write_history(
        &path,
        &[
            &[
                &[('n', 1, 1, 100, true), ('n', 2, 1, 100, true)],
                &[('w', 10, 1, 100, true)],
            ],
            &[&[('w', 10, 2, 300, true), ('w', 11, 1, 100, true)]],
        ],
    );
    let (written, blocks) = run(&path, dir.path());
    assert_eq!(
        written,
        [('n', 1, 1), ('n', 2, 1), ('w', 10, 1), ('w', 11, 1)]
    );
    assert_eq!(blocks, [vec!['n'], vec!['w'], vec!['w']]);
}

/// The same first blob, but the next blob's version 2 of way 10 is at or
/// before the cutoff and supersedes the candidate: the held blob's ending
/// without it is written, the node block alone.
#[test]
fn a_held_ending_crossing_a_kind_boundary_is_dropped_when_its_candidate_is_superseded() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("in.osh.pbf");
    write_history(
        &path,
        &[
            &[
                &[('n', 1, 1, 100, true), ('n', 2, 1, 100, true)],
                &[('w', 10, 1, 100, true)],
            ],
            &[&[('w', 10, 2, 200, true), ('w', 11, 1, 100, true)]],
        ],
    );
    let (written, blocks) = run(&path, dir.path());
    assert_eq!(
        written,
        [('n', 1, 1), ('n', 2, 1), ('w', 10, 2), ('w', 11, 1)]
    );
    assert_eq!(blocks, [vec!['n'], vec!['w']]);
}

/// The input's one blob runs nodes, ways and then relation 100, whose
/// version 1 is the candidate (its version 2 is past the cutoff). The node
/// block closes when the ways start, so it is written unconditionally; way
/// 10's deleted version drops it; and with no blob after it the input's end
/// keeps the candidate, so the ending written closes the way block and adds
/// a relation block.
#[test]
fn a_last_blob_ending_crossing_two_kind_boundaries_keeps_its_candidate() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("in.osh.pbf");
    write_history(
        &path,
        &[&[
            &[('n', 1, 1, 100, true), ('n', 1, 2, 300, true)],
            &[('w', 10, 1, 100, true), ('w', 10, 2, 200, false)],
            &[('w', 11, 1, 200, true)],
            &[('r', 100, 1, 100, true), ('r', 100, 2, 300, true)],
        ]],
    );
    let (written, blocks) = run(&path, dir.path());
    assert_eq!(written, [('n', 1, 1), ('w', 11, 1), ('r', 100, 1)]);
    assert_eq!(blocks, [vec!['n'], vec!['w'], vec!['r']]);
}
