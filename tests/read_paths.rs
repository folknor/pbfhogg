//! Tests for reading path equivalence: pipeline, par_map_reduce, and seek operations.
//!
//! Verifies that all reading modes produce identical results and that seek
//! operations work correctly on BlobReader.
#![allow(
    clippy::unwrap_used,
    clippy::cognitive_complexity,
    clippy::too_many_lines
)]

mod common;

use std::io::SeekFrom;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::Duration;
#[cfg(feature = "test-hooks")]
use std::time::Instant;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{
    BlobFilter, BlobReader, BlobType, ByteOffset, Element, ElementReader, IndexedReader, Info,
    MemberId,
};
use tempfile::TempDir;

/// Write a multi-block PBF to the given path.
/// Contains: header + 3 data blocks (3 nodes, 2 ways, 1 relation).
fn write_test_pbf(path: &Path) {
    write_test_pbf_with_compression(path, Compression::default());
}

/// Like [`write_test_pbf`] but selects the blob compression, so a test can
/// exercise the Raw / Zlib / Zstd `BlobData` variants of the decode path.
fn write_test_pbf_with_compression(path: &Path, compression: Compression) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(file, compression);

    let header = block_builder::HeaderBuilder::new()
        .bbox(9.0, 54.0, 13.0, 58.0)
        .build()
        .unwrap();
    writer.write_header(&header).unwrap();

    let mut bb = BlockBuilder::new();

    // Block 1: 3 nodes
    bb.add_node(100, 550_000_000, 120_000_000, [("name", "A")], None);
    bb.add_node(200, 560_000_000, 130_000_000, [("name", "B")], None);
    bb.add_node(
        300,
        -330_000_000,
        -580_000_000,
        std::iter::empty::<(&str, &str)>(),
        None,
    );
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    // Block 2: 2 ways
    bb.add_way(1000, [("highway", "primary")], &[100, 200, 300], None);
    bb.add_way(2000, [("building", "yes")], &[200, 300, 200], None);
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    // Block 3: 1 relation
    bb.add_relation(
        5000,
        [("type", "multipolygon")],
        &[MemberData {
            id: MemberId::Way(1000),
            role: "outer",
        }],
        None,
    );
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    writer.flush().unwrap();
}

/// Write a PBF with several differently-shaped consecutive blocks carrying full
/// element detail - tags, coordinates, per-element metadata, way references, and
/// relation members with roles - so a decode-path comparison can catch a
/// divergence in ANY materialized field, not just element kind and ID. Selects
/// the blob compression so Raw / Zlib / Zstd are all exercised.
fn write_materialization_pbf(path: &Path, compression: Compression) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(file, compression);

    let header = block_builder::HeaderBuilder::new()
        .bbox(9.0, 54.0, 13.0, 58.0)
        .build()
        .unwrap();
    writer.write_header(&header).unwrap();

    let meta_a = Metadata {
        version: 3,
        timestamp: 1_600_000_000,
        changeset: 42,
        uid: 7,
        user: "alice",
        visible: true,
    };
    let meta_b = Metadata {
        version: 1,
        timestamp: 1_500_000_500,
        changeset: 99,
        uid: 12,
        user: "bob",
        visible: true,
    };

    let mut bb = BlockBuilder::new();

    // Block 1: dense nodes, mixed tag shapes, mixed metadata presence.
    bb.add_node(
        100,
        550_000_000,
        120_000_000,
        [("name", "A"), ("place", "city")],
        Some(&meta_a),
    );
    bb.add_node(
        200,
        560_000_000,
        130_000_000,
        std::iter::empty::<(&str, &str)>(),
        Some(&meta_b),
    );
    bb.add_node(300, -330_000_000, -580_000_000, [("amenity", "cafe")], None);
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    // Block 2: more nodes, a deliberately different shape (no tags, no metadata).
    bb.add_node(
        400,
        570_000_000,
        140_000_000,
        std::iter::empty::<(&str, &str)>(),
        None,
    );
    bb.add_node(
        500,
        580_000_000,
        150_000_000,
        std::iter::empty::<(&str, &str)>(),
        None,
    );
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    // Block 3: ways with references, tags, and metadata.
    bb.add_way(
        1000,
        [("highway", "primary"), ("name", "Main")],
        &[100, 200, 300, 400],
        Some(&meta_a),
    );
    bb.add_way(2000, [("building", "yes")], &[200, 300, 200], None);
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    // Block 4: a relation with multiple typed members and distinct roles.
    bb.add_relation(
        5000,
        [("type", "multipolygon"), ("name", "Region")],
        &[
            MemberData {
                id: MemberId::Way(1000),
                role: "outer",
            },
            MemberData {
                id: MemberId::Way(2000),
                role: "inner",
            },
            MemberData {
                id: MemberId::Node(100),
                role: "label",
            },
        ],
        Some(&meta_b),
    );
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    writer.flush().unwrap();
}

/// Render the metadata block of an element to a stable string.
fn info_string(info: &Info<'_>) -> String {
    let user = match info.user() {
        Some(Ok(u)) => u.to_string(),
        Some(Err(_)) => "<err>".to_string(),
        None => "<none>".to_string(),
    };
    format!(
        "v={:?} ts={:?} cs={:?} uid={:?} user={user} vis={}",
        info.version(),
        info.milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.visible(),
    )
}

/// Fully materialize an element into a stable string: kind, ID, coordinates,
/// sorted tags, metadata, way references, and relation members with roles. Two
/// decode paths that agree on every element's `materialize` output are
/// producing byte-for-byte identical decoded content, not merely the same
/// (kind, ID) pairs.
fn materialize(element: &Element<'_>) -> String {
    let sorted_tags = |mut v: Vec<(&str, &str)>| {
        v.sort_unstable();
        format!("{v:?}")
    };
    match element {
        Element::Node(n) => format!(
            "node id={} dlat={} dlon={} tags={} info=[{}]",
            n.id(),
            n.decimicro_lat(),
            n.decimicro_lon(),
            sorted_tags(n.tags().collect()),
            info_string(&n.info()),
        ),
        Element::DenseNode(dn) => {
            let info = match dn.info() {
                Some(i) => {
                    let user = match i.user() {
                        Ok(u) => u.to_string(),
                        Err(_) => "<err>".to_string(),
                    };
                    format!(
                        "v={:?} ts={:?} cs={:?} uid={:?} user={user} vis={}",
                        i.version(),
                        i.milli_timestamp(),
                        i.changeset(),
                        i.uid(),
                        i.visible(),
                    )
                }
                None => "<none>".to_string(),
            };
            format!(
                "node id={} dlat={} dlon={} tags={} info=[{info}]",
                dn.id(),
                dn.decimicro_lat(),
                dn.decimicro_lon(),
                sorted_tags(dn.tags().collect()),
            )
        }
        Element::Way(w) => {
            let refs: Vec<i64> = w.refs().collect();
            format!(
                "way id={} tags={} refs={refs:?} info=[{}]",
                w.id(),
                sorted_tags(w.tags().collect()),
                info_string(&w.info()),
            )
        }
        Element::Relation(r) => {
            let members: Vec<String> = r
                .members()
                .map(|m| {
                    let role = m.role().unwrap_or("<err>");
                    format!("{:?}:{}:{role}", m.id.member_type(), m.id.id())
                })
                .collect();
            format!(
                "rel id={} tags={} members={members:?} info=[{}]",
                r.id(),
                sorted_tags(r.tags().collect()),
                info_string(&r.info()),
            )
        }
        _ => "unknown".to_string(),
    }
}

/// Collect the full materialization of every element via sequential `for_each`.
fn collect_sequential_full(path: &Path) -> Vec<String> {
    let mut result = Vec::new();
    ElementReader::from_path(path)
        .unwrap()
        .for_each(|element| result.push(materialize(&element)))
        .unwrap();
    result
}

/// Write a larger multi-block PBF to exercise tiny pipeline caps.
fn write_many_block_pbf(path: &Path, node_blocks: usize) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(file, Compression::default());

    let header = block_builder::HeaderBuilder::new()
        .bbox(9.0, 54.0, 13.0, 58.0)
        .build()
        .unwrap();
    writer.write_header(&header).unwrap();

    let mut bb = BlockBuilder::new();
    let mut node_id = 100_i64;
    for _ in 0..node_blocks {
        for offset in 0..3 {
            bb.add_node(
                node_id,
                550_000_000 + offset * 10_000,
                120_000_000 + offset * 10_000,
                [("name", "node")],
                None,
            );
            node_id += 100;
        }
        writer
            .write_primitive_block(bb.take().unwrap().unwrap())
            .unwrap();
    }

    bb.add_way(1000, [("highway", "primary")], &[100, 200, 300], None);
    bb.add_way(2000, [("building", "yes")], &[200, 300, 200], None);
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    bb.add_relation(
        5000,
        [("type", "multipolygon")],
        &[MemberData {
            id: MemberId::Way(1000),
            role: "outer",
        }],
        None,
    );
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    writer.flush().unwrap();
}

#[derive(Clone)]
struct CountingRead {
    inner: Cursor<Vec<u8>>,
    bytes_read: Arc<AtomicUsize>,
}

impl CountingRead {
    fn new(bytes: Vec<u8>, bytes_read: Arc<AtomicUsize>) -> Self {
        Self {
            inner: Cursor::new(bytes),
            bytes_read,
        }
    }
}

impl Read for CountingRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes_read.fetch_add(n, Relaxed);
        Ok(n)
    }
}

/// Extract (type_char, id) from an element.
fn element_id(element: &Element<'_>) -> (char, i64) {
    match element {
        Element::Node(n) => ('n', n.id()),
        Element::DenseNode(dn) => ('n', dn.id()),
        Element::Way(w) => ('w', w.id()),
        Element::Relation(r) => ('r', r.id()),
        _ => ('?', 0),
    }
}

/// Collect all element IDs using sequential for_each.
fn collect_sequential(path: &Path) -> Vec<(char, i64)> {
    let mut result = Vec::new();
    let reader = ElementReader::from_path(path).unwrap();
    reader
        .for_each(|element| {
            result.push(element_id(&element));
        })
        .unwrap();
    result
}

// ---------------------------------------------------------------------------
// Pipeline tests (via ElementReader::for_each_pipelined)
// ---------------------------------------------------------------------------

/// Pipelined reading produces elements in the same order as sequential reading.
#[test]
fn pipelined_matches_sequential() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let sequential = collect_sequential(&path);

    let mut pipelined = Vec::new();
    let reader = ElementReader::from_path(&path).unwrap();
    reader
        .for_each_pipelined(|element| {
            pipelined.push(element_id(&element));
        })
        .unwrap();

    assert_eq!(sequential, pipelined);
}

/// The sequential `for_each` decode route (`decompress_into` +
/// `from_vec_with_scratch`) must yield elements identical to the pipelined
/// route for every blob compression kind. Raw, Zlib, and Zstd exercise the
/// three `BlobData` variants the copy-free decode path handles. The comparison
/// is over the FULL materialization of each element - kind, ID, coordinates,
/// sorted tags, metadata, way references, and relation members with roles -
/// across several differently-shaped consecutive blocks, so a stale
/// string-table range, a wrong coordinate, dropped metadata, or a mangled
/// member role surfaces here as a mismatch rather than slipping past a
/// kind-and-ID-only check. The pipelined path decodes through a genuinely
/// different constructor and reuses per-thread scratch, so a scratch-reuse
/// regression in either route would diverge. (The exact `MAX_BLOB_MESSAGE_SIZE`
/// Raw/Zlib/Zstd boundary is pinned separately, at the decompression-helper
/// level, by `decompress_helpers_agree_at_message_size_boundary` in
/// `src/read/decompress.rs`.)
#[test]
fn for_each_matches_pipelined_across_compressions() {
    for compression in [
        Compression::None,
        Compression::Zlib(6),
        Compression::Zstd(3),
    ] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.osm.pbf");
        write_materialization_pbf(&path, compression);

        let sequential = collect_sequential_full(&path);

        let mut pipelined = Vec::new();
        ElementReader::from_path(&path)
            .unwrap()
            .for_each_pipelined(|element| pipelined.push(materialize(&element)))
            .unwrap();

        // Sanity: the fixture must actually produce each element kind, or a
        // materialization bug in one branch could hide behind an empty set.
        assert!(
            sequential.iter().any(|s| s.starts_with("node ")),
            "fixture must produce nodes for {compression:?}"
        );
        assert!(
            sequential.iter().any(|s| s.starts_with("way ")),
            "fixture must produce ways for {compression:?}"
        );
        assert!(
            sequential.iter().any(|s| s.starts_with("rel ")),
            "fixture must produce relations for {compression:?}"
        );
        assert_eq!(
            sequential, pipelined,
            "sequential vs pipelined element mismatch for {compression:?}"
        );
    }
}

/// into_blocks_pipelined yields the same elements as for_each_pipelined.
#[test]
fn block_iterator_matches_pipelined() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let sequential = collect_sequential(&path);

    let mut from_iter = Vec::new();
    let reader = ElementReader::from_path(&path).unwrap();
    for block_result in reader.into_blocks_pipelined() {
        let block = block_result.unwrap();
        for element in block.elements() {
            from_iter.push(element_id(&element));
        }
    }

    assert_eq!(sequential, from_iter);
}

/// into_blocks_pipelined handles early drop without hanging.
#[test]
fn block_iterator_early_drop() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    let mut blocks = reader.into_blocks_pipelined();
    // Take just the first block and drop the iterator
    let _first = blocks.next();
    drop(blocks);
    // If we get here without hanging, the test passes.
}

#[test]
fn block_iterator_early_drop_under_pressure() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("many.osm.pbf");
    write_many_block_pbf(&path, 64);

    let (done_tx, done_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let reader = ElementReader::from_path(&path)
            .unwrap()
            .read_ahead(1)
            .decode_ahead(1);
        let mut blocks = reader.into_blocks_pipelined();
        let _first = blocks.next();
        drop(blocks);
        done_tx.send(()).unwrap();
    });

    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("pipeline did not stop after iterator drop");
}

#[test]
fn block_fn_error_stops_pipeline() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("many.osm.pbf");
    write_many_block_pbf(&path, 64);

    let (done_tx, done_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = ElementReader::from_path(&path)
            .unwrap()
            .read_ahead(1)
            .decode_ahead(1)
            .for_each_block_pipelined(|_| Err(std::io::Error::other("stop").into()));
        done_tx.send(result.is_err()).unwrap();
    });

    assert!(
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("pipeline did not stop after block closure error")
    );
}

/// A mid-file decode failure surfaces from the plain `run_pipeline` in its
/// file-order position: blocks before the corrupt blob are delivered first,
/// then the error returns. Pins the surviving default engine now that the
/// gated batched twin that also covered this was removed.
#[test]
fn decode_error_surfaces_after_prior_blocks() {
    let dir = TempDir::new().unwrap();
    let good = dir.path().join("good.osm.pbf");
    let broken = dir.path().join("broken.osm.pbf");
    write_test_pbf(&good);
    let bytes = std::fs::read(&good).unwrap();
    // Frame 0 is the header, frame 1 the first node block; corrupt the
    // following way block (frame 2) so the reader must deliver the preceding
    // block before returning the decode failure.
    std::fs::write(
        &broken,
        common::adversarial::mutate_blob_payload(&bytes, 2, |payload| {
            payload.clear();
            payload.push(0xFF);
        }),
    )
    .unwrap();
    let mut blocks = 0;
    let result = ElementReader::from_path(&broken)
        .unwrap()
        .for_each_block_pipelined(|_| {
            blocks += 1;
            Ok(())
        });
    assert!(result.is_err(), "decode error must surface");
    assert!(
        blocks > 0,
        "preceding block must be delivered before the error"
    );
}

#[test]
fn pipelined_matches_sequential_tiny_caps() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("many.osm.pbf");
    write_many_block_pbf(&path, 16);

    let sequential = collect_sequential(&path);

    let mut pipelined = Vec::new();
    ElementReader::from_path(&path)
        .unwrap()
        .read_ahead(1)
        .decode_ahead(1)
        .for_each_pipelined(|element| {
            pipelined.push(element_id(&element));
        })
        .unwrap();

    assert_eq!(sequential, pipelined);
}

#[test]
fn pipelined_block_iterator_matches_sequential_with_tiny_count_bounds() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("many.osm.pbf");
    write_many_block_pbf(&path, 16);

    let sequential = collect_sequential(&path);
    let mut pipelined = Vec::new();
    for block in ElementReader::from_path(&path)
        .unwrap()
        .read_ahead(1)
        .decode_ahead(1)
        .into_blocks_pipelined()
    {
        block
            .unwrap()
            .for_each_element(|element| pipelined.push(element_id(&element)));
    }

    assert_eq!(sequential, pipelined);
}

#[cfg(feature = "test-hooks")]
#[test]
fn admission_high_water_bounded_under_slow_first_decode() {
    use pbfhogg::read::pipeline_test_hooks::{
        BLOCK_DECODE_SEQ, BLOCKED_DECODE_READY, RELEASE_BLOCKED_DECODE, REORDER_FILLED_HIGH_WATER,
        reset,
    };

    reset();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("many.osm.pbf");
    write_many_block_pbf(&path, 16);

    BLOCK_DECODE_SEQ.store(1, Relaxed);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = ElementReader::from_path(&path)
            .unwrap()
            .read_ahead(1)
            .decode_ahead(2)
            .for_each_block_pipelined(|_| Ok(()));
        done_tx.send(result).unwrap();
    });

    let start = Instant::now();
    while !BLOCKED_DECODE_READY.load(Relaxed) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "decode hook was not reached"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    let start = Instant::now();
    while REORDER_FILLED_HIGH_WATER.load(Relaxed) < 1 {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "later decode did not reach the reorder buffer"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    RELEASE_BLOCKED_DECODE.store(true, Relaxed);
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("pipeline did not finish after releasing decode hook")
        .unwrap();

    assert!(REORDER_FILLED_HIGH_WATER.load(Relaxed) <= 2);
    reset();
}

#[test]
fn early_exit_does_not_read_whole_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("many.osm.pbf");
    write_many_block_pbf(&path, 256);
    let bytes = std::fs::read(&path).unwrap();
    let full_len = bytes.len();
    let bytes_read = Arc::new(AtomicUsize::new(0));

    // Dropping `PipelinedBlocks` joins the background pipeline thread, so a
    // shutdown regression would hang here. Run it on a spawned thread and
    // fail via `recv_timeout` rather than hanging the whole suite.
    let reader_count = Arc::clone(&bytes_read);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let reader = CountingRead::new(bytes, reader_count);
        let reader = ElementReader::new(reader)
            .unwrap()
            .read_ahead(1)
            .decode_ahead(1);
        let mut blocks = reader.into_blocks_pipelined();
        let _first = blocks.next();
        drop(blocks);
        done_tx.send(()).unwrap();
    });

    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("early-drop pipeline did not stop after iterator drop");

    let read = bytes_read.load(Relaxed);
    assert!(
        read < full_len,
        "early drop read the whole file: read {read}, full {full_len}"
    );
}

/// block_type() correctly classifies each block in a sorted PBF.
#[test]
fn block_type_classification() {
    use pbfhogg::BlockType;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    let mut types = Vec::new();
    for block_result in reader.into_blocks_pipelined() {
        let block = block_result.unwrap();
        types.push(block.block_type());
    }

    // write_test_pbf creates 3 blocks: dense nodes, ways, relations
    assert_eq!(
        types,
        vec![BlockType::DenseNodes, BlockType::Ways, BlockType::Relations]
    );

    // Convenience methods
    assert!(BlockType::DenseNodes.is_nodes());
    assert!(BlockType::Nodes.is_nodes());
    assert!(!BlockType::Ways.is_nodes());
    assert!(BlockType::Ways.is_ways());
    assert!(BlockType::Relations.is_relations());
    assert!(!BlockType::Mixed.is_nodes());
}

// ---------------------------------------------------------------------------
// par_map_reduce tests
// ---------------------------------------------------------------------------

/// par_map_reduce counts match sequential counts.
#[test]
fn par_map_reduce_count() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let sequential = collect_sequential(&path);
    let expected_nodes = sequential.iter().filter(|(t, _)| *t == 'n').count() as u64;
    let expected_ways = sequential.iter().filter(|(t, _)| *t == 'w').count() as u64;
    let expected_relations = sequential.iter().filter(|(t, _)| *t == 'r').count() as u64;

    let reader = ElementReader::from_path(&path).unwrap();
    let (nodes, ways, relations) = reader
        .par_map_reduce(
            |element| match element {
                Element::Node(_) | Element::DenseNode(_) => (1u64, 0u64, 0u64),
                Element::Way(_) => (0, 1, 0),
                Element::Relation(_) => (0, 0, 1),
                _ => (0, 0, 0),
            },
            || (0, 0, 0),
            |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2),
        )
        .unwrap();

    assert_eq!(nodes, expected_nodes);
    assert_eq!(ways, expected_ways);
    assert_eq!(relations, expected_relations);
}

/// par_map_reduce collects the same set of element IDs as sequential (order may differ).
#[test]
fn par_map_reduce_collect_ids() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let mut expected = collect_sequential(&path);
    expected.sort();

    let reader = ElementReader::from_path(&path).unwrap();
    let mut actual: Vec<(char, i64)> = reader
        .par_map_reduce(
            |element| vec![element_id(&element)],
            Vec::new,
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .unwrap();
    actual.sort();

    assert_eq!(expected, actual);
}

// ---------------------------------------------------------------------------
// BlobReader seek tests
// ---------------------------------------------------------------------------

/// Seeking back to the start re-reads the first blob.
#[test]
fn blobreader_seek_to_start() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let mut reader = BlobReader::seekable_from_path(&path).unwrap();
    let first = reader.next().unwrap().unwrap();
    assert_eq!(first.get_type(), BlobType::OsmHeader);
    assert_eq!(first.offset(), Some(ByteOffset(0)));

    // Seek back to start
    reader.seek(ByteOffset(0)).unwrap();
    let first_again = reader.next().unwrap().unwrap();
    assert_eq!(first_again.get_type(), BlobType::OsmHeader);
    assert_eq!(first_again.offset(), Some(ByteOffset(0)));
}

/// blob_from_offset can random-access any blob by its recorded offset.
#[test]
fn blobreader_blob_from_offset() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    // First pass: collect all blob types (as strings) and offsets
    let mut reader = BlobReader::seekable_from_path(&path).unwrap();
    let mut blobs_info: Vec<(String, ByteOffset)> = Vec::new();
    for blob in reader.by_ref() {
        let blob = blob.unwrap();
        blobs_info.push((blob.get_type().as_str().to_string(), blob.offset().unwrap()));
    }

    // Random access each blob by its offset
    for (expected_type, offset) in &blobs_info {
        let blob = reader.blob_from_offset(*offset).unwrap();
        assert_eq!(blob.get_type().as_str(), expected_type.as_str());
        assert_eq!(blob.offset(), Some(*offset));
    }
}

/// seek_raw with SeekFrom::Start(0) restarts; SeekFrom::End(0) reaches EOF.
#[test]
fn blobreader_seek_raw() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let mut reader = BlobReader::seekable_from_path(&path).unwrap();

    // Read first blob
    let _ = reader.next().unwrap().unwrap();

    // Seek back to start
    let pos = reader.seek_raw(SeekFrom::Start(0)).unwrap();
    assert_eq!(pos, 0);
    let blob = reader.next().unwrap().unwrap();
    assert_eq!(blob.get_type(), BlobType::OsmHeader);

    // Seek to end - next should return None (clean EOF)
    let end_pos = reader.seek_raw(SeekFrom::End(0)).unwrap();
    assert!(end_pos > 0);
    assert!(reader.next().is_none());
}

/// seek_raw success clears the sticky error state left by a previous failing
/// `next()`, so callers that recover by seeking past bad bytes can resume.
#[test]
fn blobreader_seek_raw_clears_error_state() {
    // Bytes layout: [oversized blob header length | good PBF].
    //  - The first 4 bytes claim a blob header of ~4 GB, tripping the
    //    MAX_BLOB_HEADER_SIZE guard on the very first `next()` call.
    //  - After the failure, `last_blob_ok = false` makes the reader sticky.
    //  - Seeking past the 4 sentinel bytes should reset the state and let
    //    iteration resume on the good PBF that follows.
    let dir = TempDir::new().unwrap();
    let good_path = dir.path().join("good.osm.pbf");
    write_test_pbf(&good_path);
    let good_bytes = std::fs::read(&good_path).unwrap();

    let mut bytes = vec![0xFFu8, 0xFF, 0xFF, 0xFF]; // claimed header length = 0xFFFFFFFF
    bytes.extend_from_slice(&good_bytes);

    let mut reader = BlobReader::new(std::io::Cursor::new(bytes));

    // First call: HeaderTooBig error; reader becomes sticky.
    assert!(reader.next().unwrap().is_err());
    assert!(reader.next().is_none(), "reader must stay dead until seek");

    // Seek past the sentinel; state must clear.
    reader.seek_raw(SeekFrom::Start(4)).unwrap();

    // Iteration resumes on the good PBF.
    let blob = reader.next().unwrap().unwrap();
    assert_eq!(blob.get_type(), BlobType::OsmHeader);
}

/// next_header_skip_blob scans all headers without decoding blob content.
#[test]
fn blobreader_next_header_skip_blob() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    // Normal iteration: collect types (as strings) and offsets
    let reader = BlobReader::from_path(&path).unwrap();
    let mut expected: Vec<(String, Option<ByteOffset>)> = Vec::new();
    for blob in reader {
        let blob = blob.unwrap();
        expected.push((blob.get_type().as_str().to_string(), blob.offset()));
    }

    // Header-skip iteration: should match types and offsets without decoding
    let mut reader = BlobReader::seekable_from_path(&path).unwrap();
    let mut actual: Vec<(String, Option<ByteOffset>)> = Vec::new();
    while let Some(result) = reader.next_header_skip_blob() {
        let (header, offset) = result.unwrap();
        actual.push((header.blob_type().as_str().to_string(), offset));
    }

    assert_eq!(expected.len(), actual.len());
    for (e, a) in expected.iter().zip(actual.iter()) {
        assert_eq!(e.0, a.0, "blob types must match");
        assert_eq!(e.1, a.1, "offsets must match");
    }
}

// ---------------------------------------------------------------------------
// Header accessor tests
// ---------------------------------------------------------------------------

/// Write a PBF with Sort.Type_then_ID and verify header().is_sorted().
fn write_sorted_pbf(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(file, Compression::default());

    let header = block_builder::HeaderBuilder::new()
        .bbox(9.0, 54.0, 13.0, 58.0)
        .sorted()
        .build()
        .unwrap();
    writer.write_header(&header).unwrap();

    let mut bb = BlockBuilder::new();
    bb.add_node(
        1,
        550_000_000,
        120_000_000,
        std::iter::empty::<(&str, &str)>(),
        None,
    );
    bb.add_node(
        2,
        560_000_000,
        130_000_000,
        std::iter::empty::<(&str, &str)>(),
        None,
    );
    bb.add_node(
        3,
        570_000_000,
        140_000_000,
        std::iter::empty::<(&str, &str)>(),
        None,
    );
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    writer.flush().unwrap();
}

/// ElementReader exposes the parsed header via header().
#[test]
fn header_accessor() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    let header = reader.header();

    // write_test_pbf sets bbox to (9.0, 54.0, 13.0, 58.0)
    let bbox = header.bbox().unwrap();
    assert!((bbox.left - 9.0).abs() < 1e-6);
    assert!((bbox.bottom - 54.0).abs() < 1e-6);

    // writing_program is "pbfhogg"
    assert_eq!(header.writing_program(), Some("pbfhogg"));
}

/// header().is_sorted() returns true when Sort.Type_then_ID is set.
#[test]
fn header_is_sorted_true() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("sorted.osm.pbf");
    write_sorted_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    assert!(reader.header().is_sorted());
}

/// header().is_sorted() returns false when Sort.Type_then_ID is absent.
#[test]
fn header_is_sorted_false() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("unsorted.osm.pbf");
    write_test_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    assert!(!reader.header().is_sorted());
}

/// Elements are still delivered correctly after header is consumed at construction.
#[test]
fn header_consumed_elements_still_work() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_sorted_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    assert!(reader.header().is_sorted());

    let mut count = 0u64;
    reader
        .for_each(|_element| {
            count += 1;
        })
        .unwrap();

    assert_eq!(count, 3); // 3 nodes from write_sorted_pbf
}

/// Sorted PBF iterates without assertion failure (nodes in ascending ID order).
#[test]
fn sorted_pbf_no_assertion_failure() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("sorted.osm.pbf");
    write_sorted_pbf(&path);

    // for_each path
    let reader = ElementReader::from_path(&path).unwrap();
    reader.for_each(|_| {}).unwrap();

    // for_each_pipelined path
    let reader = ElementReader::from_path(&path).unwrap();
    reader.for_each_pipelined(|_| {}).unwrap();
}

/// Debug assertion fires on unsorted nodes when Sort.Type_then_ID is declared.
///
/// Requires `debug_assertions` to be enabled in the test profile.
/// Nightly 1.95 (2026-02-25) has a regression where `debug_assertions` is
/// off in test builds, so the test compiles to nothing in our environment.
/// `cfg(debug_assertions)` is the correct gate: the test is only
/// meaningful when the runtime assertion can fire, and `include_ignored`
/// can't resurrect a compile-excluded item (which `#[ignore]` alone left
/// vulnerable - tier 3 / `--profile full` in brokkr.toml runs ignored
/// tests and hit the unfireable-panic case).
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "Sort.Type_then_ID violated")]
fn sorted_flag_but_unsorted_nodes_panics() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("liar.osm.pbf");

    // Write a PBF that declares Sort.Type_then_ID but has nodes out of order
    let file = std::fs::File::create(&path).unwrap();
    let mut writer = PbfWriter::new(file, Compression::default());

    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .unwrap();
    writer.write_header(&header).unwrap();

    let mut bb = BlockBuilder::new();
    bb.add_node(
        100,
        550_000_000,
        120_000_000,
        std::iter::empty::<(&str, &str)>(),
        None,
    );
    bb.add_node(
        50,
        560_000_000,
        130_000_000,
        std::iter::empty::<(&str, &str)>(),
        None,
    ); // out of order!
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    writer.flush().unwrap();

    let reader = ElementReader::from_path(&path).unwrap();
    reader.for_each(|_| {}).unwrap();
}

// ---------------------------------------------------------------------------
// BlobFilter conservative pass-through on non-indexed PBFs
// ---------------------------------------------------------------------------
//
// `should_skip_blob` in `src/read/pipeline.rs:20-33` short-circuits to
// `false` (do not skip) when `blob.index()` is `None`. The doc comment
// calls this out: "Blobs without indexdata or tagdata always pass
// through (conservative)." The consequence is that a filter like
// `BlobFilter::only_ways()` skips node blobs on an indexed PBF but
// does NOT on a non-indexed one - every blob is decompressed and every
// element is delivered to the caller's closure.
//
// The element-level delivery path does not apply any element-type
// filter downstream of the pipeline, so on non-indexed input an
// only_ways filter will silently hand the caller every element type.
// That's the contract these tests pin.

#[test]
fn blobfilter_only_ways_skips_node_blobs_on_indexed_input() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("indexed.osm.pbf");
    common::write_test_pbf_sorted(
        &path,
        &common::generate_nodes(10, 1),
        &common::generate_ways(5, 1_000, 2, 1),
        &[],
    );
    common::assert_indexed(&path);

    let reader = ElementReader::from_path(&path)
        .unwrap()
        .with_blob_filter(BlobFilter::only_ways());

    let mut saw_nodes = 0u64;
    let mut saw_ways = 0u64;
    reader
        .for_each_pipelined(|element| match element {
            Element::Node(_) | Element::DenseNode(_) => saw_nodes += 1,
            Element::Way(_) => saw_ways += 1,
            _ => {}
        })
        .unwrap();

    assert_eq!(
        saw_nodes, 0,
        "only_ways filter must skip node blobs on indexed input"
    );
    assert_eq!(
        saw_ways, 5,
        "only_ways filter must deliver all ways on indexed input"
    );
}

#[test]
fn blobfilter_only_ways_passes_through_on_non_indexed_input() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("non_indexed.osm.pbf");
    common::write_test_pbf_non_indexed(
        &path,
        &common::generate_nodes(10, 1),
        &common::generate_ways(5, 1_000, 2, 1),
        &[],
    );
    common::assert_non_indexed(&path);

    let reader = ElementReader::from_path(&path)
        .unwrap()
        .with_blob_filter(BlobFilter::only_ways());

    let mut saw_nodes = 0u64;
    let mut saw_ways = 0u64;
    reader
        .for_each_pipelined(|element| match element {
            Element::Node(_) | Element::DenseNode(_) => saw_nodes += 1,
            Element::Way(_) => saw_ways += 1,
            _ => {}
        })
        .unwrap();

    // Node blobs are NOT skipped because the filter's blob-level
    // decision requires indexdata. All 10 nodes reach the closure.
    assert_eq!(
        saw_nodes, 10,
        "BlobFilter on non-indexed input must NOT drop node blobs - callers get every element"
    );
    assert_eq!(saw_ways, 5, "ways still delivered");
}

// ---------------------------------------------------------------------------
// IndexedReader on non-indexed input
// ---------------------------------------------------------------------------
//
// `IndexedReader::create_index` walks only blob headers (not bodies),
// so it does not itself depend on `BlobHeader.indexdata`. The per-blob
// `id_ranges` used by `ways_available` / `node_range_included` are
// populated lazily from decoded blocks via `update_element_id_ranges`
// (src/read/indexed.rs:184) - the same code path runs whether or not
// the input carries indexdata. This test pins that contract: the
// output of `read_ways_and_deps` on a non-indexed PBF must match the
// output on its indexed twin.

#[test]
fn indexed_reader_output_matches_on_indexed_and_non_indexed_twins() {
    let dir = TempDir::new().unwrap();
    let indexed = dir.path().join("indexed.osm.pbf");
    let non_indexed = dir.path().join("non_indexed.osm.pbf");

    // 8 nodes + 4 ways; each way refs two consecutive nodes. "building"
    // tag on odd-numbered ways so read_ways_and_deps has a meaningful
    // filter and node-dependency resolution.
    let nodes = common::generate_nodes(8, 1);
    let mut ways = common::generate_ways(4, 1_000, 2, 1);
    for (i, w) in ways.iter_mut().enumerate() {
        if i % 2 == 0 {
            w.tags = vec![("building", "yes")];
        }
    }

    common::write_test_pbf_sorted(&indexed, &nodes, &ways, &[]);
    common::write_test_pbf_non_indexed(&non_indexed, &nodes, &ways, &[]);
    common::assert_indexed(&indexed);
    common::assert_non_indexed(&non_indexed);

    let collect = |path: &Path| -> (Vec<i64>, Vec<i64>) {
        let mut reader = IndexedReader::from_path(path).unwrap();
        let mut way_ids = Vec::new();
        let mut node_ids = Vec::new();
        reader
            .read_ways_and_deps(
                |w| w.tags().any(|(k, v)| k == "building" && v == "yes"),
                |element| match element {
                    Element::Way(w) => way_ids.push(w.id()),
                    Element::Node(n) => node_ids.push(n.id()),
                    Element::DenseNode(n) => node_ids.push(n.id()),
                    _ => {}
                },
            )
            .unwrap();
        way_ids.sort_unstable();
        node_ids.sort_unstable();
        (way_ids, node_ids)
    };

    let (ways_idx, nodes_idx) = collect(&indexed);
    let (ways_non, nodes_non) = collect(&non_indexed);

    assert_eq!(ways_idx, ways_non, "way set diverges on non-indexed input");
    assert_eq!(
        nodes_idx, nodes_non,
        "node dep set diverges on non-indexed input"
    );
    assert!(!ways_idx.is_empty(), "filter must match at least one way");
}

// ---------------------------------------------------------------------------
// Reader-level tolerance contract (`reference/truncation-handling.md`):
// 0-3 leftover bytes of an incomplete next-frame length prefix after a
// complete previous frame is shape 1 - the reader returns `Ok(None)`,
// equivalent to a clean cut at the frame boundary. The `cli_truncation_sweep`
// integration test only pins no-panic at the command level for shape 1
// because some commands (sort) may legitimately reject a partial-input
// file even when the reader's tolerance contract holds. These unit tests
// pin the reader contract directly.
// ---------------------------------------------------------------------------

#[test]
fn trailing_partial_length_prefix_returns_ok_none() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("good.osm.pbf");
    write_test_pbf(&path);
    let good_bytes = std::fs::read(&path).unwrap();

    // For each tail byte count in 0..=3, append that many garbage
    // bytes to the complete file and expect the reader to iterate
    // every original blob then return Ok(None). 0 bytes (the
    // unmodified file) establishes the baseline; 1-3 bytes pin the
    // documented tolerance.
    for tail in 0..=3 {
        let mut bytes = good_bytes.clone();
        bytes.extend(std::iter::repeat_n(0xAAu8, tail));

        let mut reader = BlobReader::new(std::io::Cursor::new(bytes));
        let mut blob_count = 0;
        loop {
            match reader.next() {
                Some(Ok(_)) => blob_count += 1,
                Some(Err(e)) => panic!(
                    "reader must tolerate {tail} trailing bytes per the \
                     truncation reference doc; got Err: {e:?}"
                ),
                None => break,
            }
        }
        assert!(
            blob_count >= 1,
            "fixture must contain at least one blob (got {blob_count})"
        );
    }
}

#[test]
fn trailing_partial_length_prefix_4_bytes_is_committed_frame() {
    // 4 bytes is exactly a complete length prefix; that's NOT shape 1
    // anymore - the reader is committed to a frame and must hard-error
    // because the declared header bytes don't follow.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("good.osm.pbf");
    write_test_pbf(&path);
    let good_bytes = std::fs::read(&path).unwrap();

    let mut bytes = good_bytes.clone();
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x10]); // claims 16-byte header

    let mut reader = BlobReader::new(std::io::Cursor::new(bytes));
    let mut errored = false;
    loop {
        match reader.next() {
            Some(Ok(_)) => {}
            Some(Err(_)) => {
                errored = true;
                break;
            }
            None => break,
        }
    }
    assert!(
        errored,
        "4 trailing bytes (a complete length prefix declaring N>0 \
         header bytes that don't follow) is shape 2, must hard-error"
    );
}
