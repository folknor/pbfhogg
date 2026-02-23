//! Real-data round-trip: read Denmark PBF → write back → read again → compare.
//!
//! Skipped if `data/denmark-latest.osm.pbf` doesn't exist.

use pbfhogg::block_builder::{self, BlockBuilder, MemberData, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};
use std::io::BufReader;
use std::path::Path;

fn denmark_path() -> std::path::PathBuf {
    // Use PBFHOGG_TEST_PBF env var if set, otherwise look next to the crate.
    if let Ok(p) = std::env::var("PBFHOGG_TEST_PBF") {
        return std::path::PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("data/denmark-latest.osm.pbf")
}

fn output_path() -> std::path::PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    std::fs::create_dir_all(&dir).ok();
    dir.join("denmark-roundtrip.osm.pbf")
}

#[derive(Default, Debug)]
struct Counts {
    nodes: u64,
    ways: u64,
    relations: u64,
}

/// Count all elements in a PBF file.
fn count_elements(path: &Path) -> Counts {
    let file = std::fs::File::open(path).expect("open pbf");
    let reader = BlobReader::new(BufReader::new(file));
    let mut counts = Counts::default();

    for blob_result in reader {
        let blob = blob_result.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(_) | Element::Node(_) => counts.nodes += 1,
                    Element::Way(_) => counts.ways += 1,
                    Element::Relation(_) => counts.relations += 1,
                }
            }
        }
    }

    counts
}

#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
fn write_pbf_copy(input: &Path, output: &Path) {
    let file = std::fs::File::open(input).expect("open input");
    let reader = BlobReader::new(BufReader::new(file));
    let mut writer = PbfWriter::to_path(output, Compression::default()).expect("create output");

    // Write a minimal header
    let header_bytes = block_builder::build_header(None, None, None, None).expect("build header");
    writer.write_header(&header_bytes).expect("write header");

    let mut bb = BlockBuilder::new();
    let mut last_type: Option<&str> = None;

    for blob_result in reader {
        let blob = blob_result.expect("read blob");
        let block = match blob.decode().expect("decode") {
            BlobDecode::OsmData(b) => b,
            _ => continue,
        };

        for element in block.elements() {
            match element {
                Element::DenseNode(dn) => {
                    if last_type != Some("node") {
                        if let Some(bytes) = bb.take().expect("take") {
                            writer.write_primitive_block(&bytes).expect("write");
                        }
                        last_type = Some("node");
                    }
                    if !bb.can_add_node() {
                        if let Some(bytes) = bb.take().expect("take") {
                            writer.write_primitive_block(&bytes).expect("write");
                        }
                    }

                    let tags: Vec<(&str, &str)> = dn.tags().collect();
                    let meta = dn.info().map(|info| Metadata {
                        version: info.version(),
                        timestamp: info.milli_timestamp() / 1000,
                        changeset: info.changeset(),
                        uid: info.uid(),
                        user: "", // skip user strings to keep it simple
                        visible: info.visible(),
                    });
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        &tags,
                        meta.as_ref(),
                    );
                }
                Element::Node(_) => {
                    // Rare non-dense nodes — skip for this test
                }
                Element::Way(w) => {
                    if last_type != Some("way") {
                        if let Some(bytes) = bb.take().expect("take") {
                            writer.write_primitive_block(&bytes).expect("write");
                        }
                        last_type = Some("way");
                    }
                    if !bb.can_add_way() {
                        if let Some(bytes) = bb.take().expect("take") {
                            writer.write_primitive_block(&bytes).expect("write");
                        }
                    }

                    let tags: Vec<(&str, &str)> = w.tags().collect();
                    let refs: Vec<i64> = w.refs().collect();
                    let info = w.info();
                    let meta = info.version().map(|v| Metadata {
                        version: v,
                        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                        changeset: info.changeset().unwrap_or(0),
                        uid: info.uid().unwrap_or(0),
                        user: "",
                        visible: info.visible(),
                    });
                    bb.add_way(w.id(), &tags, &refs, meta.as_ref());
                }
                Element::Relation(r) => {
                    if last_type != Some("relation") {
                        if let Some(bytes) = bb.take().expect("take") {
                            writer.write_primitive_block(&bytes).expect("write");
                        }
                        last_type = Some("relation");
                    }
                    if !bb.can_add_relation() {
                        if let Some(bytes) = bb.take().expect("take") {
                            writer.write_primitive_block(&bytes).expect("write");
                        }
                    }

                    let tags: Vec<(&str, &str)> = r.tags().collect();
                    let members: Vec<MemberData<'_>> = r
                        .members()
                        .map(|m| MemberData {
                            id: m.id,
                            role: m.role().unwrap_or(""),
                        })
                        .collect();
                    let info = r.info();
                    let meta = info.version().map(|v| Metadata {
                        version: v,
                        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                        changeset: info.changeset().unwrap_or(0),
                        uid: info.uid().unwrap_or(0),
                        user: "",
                        visible: info.visible(),
                    });
                    bb.add_relation(r.id(), &tags, &members, meta.as_ref());
                }
            }
        }
    }

    // Flush remaining
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(&bytes).expect("write");
    }
    writer.flush().expect("flush");
}

#[test]
#[ignore] // 54s on Denmark — run with: cargo test -- --ignored
fn roundtrip_denmark() {
    let dk = denmark_path();
    let out = output_path();

    if !dk.exists() {
        eprintln!("Skipping: {} not found", dk.display());
        return;
    }

    eprintln!("Counting elements in source...");
    let source_counts = count_elements(&dk);
    eprintln!(
        "  Source: {} nodes, {} ways, {} relations",
        source_counts.nodes, source_counts.ways, source_counts.relations
    );

    eprintln!("Writing round-trip copy...");
    write_pbf_copy(&dk, &out);

    let output_size = std::fs::metadata(&out)
        .expect("stat output")
        .len();
    eprintln!("  Output size: {} MB", output_size / 1_000_000);

    eprintln!("Counting elements in output...");
    let output_counts = count_elements(&out);
    eprintln!(
        "  Output: {} nodes, {} ways, {} relations",
        output_counts.nodes, output_counts.ways, output_counts.relations
    );

    assert_eq!(source_counts.nodes, output_counts.nodes, "node count mismatch");
    assert_eq!(source_counts.ways, output_counts.ways, "way count mismatch");
    assert_eq!(
        source_counts.relations, output_counts.relations,
        "relation count mismatch"
    );

    // Cleanup
    let _ = std::fs::remove_file(&out);
}
