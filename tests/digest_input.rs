//! `digest` reads its input in one pass through one open, checking the blob
//! envelope and the indexdata claims while it decodes: a blob of unknown
//! type and a partially indexed file are refused there, located at the blob
//! that shows the fault, and a stream is digested like the file it carries.

use std::path::Path;

use pbfhogg::ErrorKind;
use pbfhogg::digest::ContentDigest;

use crate::common::adversarial::{locate_blobs, mutate_blob_header_indexdata};
use crate::common::{TestNode, write_multi_block_test_pbf};

/// Header blob, then one data blob per node: blobs 1 to 4.
const DATA_BLOBS: usize = 4;

fn write_fixture(path: &Path) {
    let nodes: Vec<TestNode> = (1..=4)
        .map(|id| TestNode {
            id,
            lat: 10 * i32::try_from(id).expect("small id"),
            lon: 20,
            tags: vec![("name", "x")],
            meta: None,
        })
        .collect();
    write_multi_block_test_pbf(path, &nodes, &[], &[], 1);
}

fn digest(path: &Path) -> pbfhogg::Result<ContentDigest> {
    pbfhogg::digest::digest(path, false, &crate::common::test_host())
}

/// The fixture with the indexdata of `stripped` blobs made unreadable (an
/// empty field, which every reader treats as absent).
fn stripped(bytes: &[u8], stripped: &[usize]) -> Vec<u8> {
    stripped.iter().fold(bytes.to_vec(), |acc, &blob| {
        mutate_blob_header_indexdata(&acc, blob, Vec::clear)
    })
}

/// A complete frame of a blob whose type no reader knows, carrying a small
/// raw payload.
fn unknown_frame() -> Vec<u8> {
    let body = [0x0A, 0x01, b'x']; // Blob.raw = "x"
    let mut header = vec![0x0A, 6]; // BlobHeader.type
    header.extend_from_slice(b"OSMFoo");
    header.extend_from_slice(&[0x18, u8::try_from(body.len()).expect("small body")]); // datasize
    let mut frame = u32::try_from(header.len())
        .expect("small header")
        .to_be_bytes()
        .to_vec();
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&body);
    frame
}

/// The refusal names the first data blob without indexdata, whether it
/// comes before or after the first indexed one, with the input and the
/// blob's frame start; a wholly unindexed file is digested, and equals the
/// indexed one.
#[test]
fn a_partially_indexed_file_is_refused_at_its_first_unindexed_blob() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let bytes = std::fs::read(&input).expect("read fixture");
    let indexed = digest(&input).expect("the indexed fixture");

    let path = dir.path().join("partial.osm.pbf");
    for (strip, first) in [
        (&[3][..], 3),
        (&[4][..], 4),
        (&[1][..], 1),
        (&[1, 2, 3][..], 1),
        (&[2, 4][..], 2),
    ] {
        let partial = stripped(&bytes, strip);
        std::fs::write(&path, &partial).expect("write partial");
        let err = digest(&path)
            .err()
            .unwrap_or_else(|| panic!("{strip:?}: a partially indexed file must be refused"));
        assert!(
            matches!(
                err.kind(),
                ErrorKind::MissingIndexdata { partial: true, .. }
            ),
            "{strip:?}: {err:?}"
        );
        assert_eq!(err.blob_index(), Some(first as u64), "{strip:?}: {err:?}");
        let frames = locate_blobs(&partial);
        assert_eq!(
            err.byte_offset().map(|o| o.0),
            Some(frames[first].frame_start as u64),
            "{strip:?}: {err:?}"
        );
        assert_eq!(err.source_path(), Some(path.as_path()), "{strip:?}");
    }

    let all: Vec<usize> = (1..=DATA_BLOBS).collect();
    std::fs::write(&path, stripped(&bytes, &all)).expect("write unindexed");
    assert_eq!(
        digest(&path).expect("a wholly unindexed file is digested"),
        indexed,
        "indexdata presence does not change the digest"
    );
}

/// A blob of unknown type is refused wherever it sits, in an indexed or an
/// unindexed file, located at that blob.
#[test]
fn a_blob_of_unknown_type_is_refused_at_that_blob() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let indexed = std::fs::read(&input).expect("read fixture");
    let all: Vec<usize> = (1..=DATA_BLOBS).collect();
    let unindexed = stripped(&indexed, &all);
    let path = dir.path().join("unknown.osm.pbf");
    for (label, bytes) in [("indexed", &indexed), ("unindexed", &unindexed)] {
        let frames = locate_blobs(bytes);
        // Before the first data blob, between two, and after the last.
        for at in [1, 3, frames.len()] {
            let offset = frames.get(at).map_or(bytes.len(), |f| f.frame_start);
            let with = [&bytes[..offset], &unknown_frame(), &bytes[offset..]].concat();
            std::fs::write(&path, with).expect("write");
            let err = digest(&path)
                .err()
                .unwrap_or_else(|| panic!("{label}, at {at}: must be refused"));
            assert!(
                err.to_string().contains("unknown type"),
                "{label}, at {at}: {err}"
            );
            assert_eq!(
                err.blob_index(),
                Some(at as u64),
                "{label}, at {at}: {err:?}"
            );
            assert_eq!(
                err.byte_offset().map(|o| o.0),
                Some(offset as u64),
                "{label}, at {at}: {err:?}"
            );
        }
    }
}

/// One open, read front to back: a FIFO carrying the file gets the file's
/// digest, and a FIFO carrying a partially indexed file is refused, as the
/// file is.
#[cfg(unix)]
#[test]
fn a_stream_is_digested_like_the_file_it_carries() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let bytes = std::fs::read(&input).expect("read fixture");
    let expected = digest(&input).expect("the file");

    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    for (label, payload, accepted) in [
        ("whole", bytes.clone(), true),
        ("partial", stripped(&bytes, &[2]), false),
    ] {
        let writer = {
            let fifo = fifo.clone();
            // A refusal may close the read end before every byte is taken;
            // the write's own outcome is not what this test checks.
            std::thread::spawn(move || drop(std::fs::write(&fifo, payload)))
        };
        let result = digest(&fifo);
        writer.join().expect("the writer thread");
        if accepted {
            assert_eq!(
                result.unwrap_or_else(|e| panic!("{label}: {e}")),
                expected,
                "{label}"
            );
        } else {
            let err = result
                .err()
                .unwrap_or_else(|| panic!("{label}: must be refused"));
            assert!(
                matches!(
                    err.kind(),
                    ErrorKind::MissingIndexdata { partial: true, .. }
                ),
                "{label}: {err:?}"
            );
            assert_eq!(err.blob_index(), Some(2), "{label}: {err:?}");
        }
    }
}
