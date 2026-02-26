//! Benchmark: read all elements then write them back through BlockBuilder + PbfWriter.
//!
//! Usage: bench_write <file.osm.pbf> [runs] [--compression none,zlib:6,zstd:3]
//!
//! Measures full decode + write throughput (the nidhogg/elivagar write path).
//! Output goes to /dev/null so I/O cost is excluded.
//! The --compression flag accepts a comma-separated list of modes to benchmark.
//! Both sync and pipelined writer modes are benchmarked for each compression.

use pbfhogg::{
    BlobDecode, BlobReader, Element,
    block_builder::BlockBuilder,
    writer::{Compression, PbfWriter},
};
use std::io::{BufReader, Write};
use std::path::Path;
use std::time::Instant;

fn flush<W: Write>(bb: &mut BlockBuilder, writer: &mut PbfWriter<W>) {
    if let Some(bytes) = bb.take().expect("take block") {
        writer.write_primitive_block(bytes).expect("write block");
    }
}

#[derive(Default)]
struct Counts {
    nodes: u64,
    ways: u64,
    relations: u64,
}

impl Counts {
    fn total(&self) -> u64 {
        self.nodes + self.ways + self.relations
    }
}

const DEFAULT_COMPRESSIONS: &str = "none,zlib:6,zstd:3";

fn parse_compression(s: &str) -> Option<(String, Compression)> {
    let (name, level_str) = match s.split_once(':') {
        Some((n, l)) => (n, Some(l)),
        None => (s, None),
    };
    let comp = match name {
        "none" => Compression::None,
        "zlib" => Compression::Zlib(level_str.and_then(|l| l.parse().ok()).unwrap_or(6)),
        "zstd" => Compression::Zstd(level_str.and_then(|l| l.parse().ok()).unwrap_or(3)),
        _ => {
            eprintln!("Unknown compression: {s}");
            return None;
        }
    };
    let label = match comp {
        Compression::None => "none".to_string(),
        Compression::Zlib(l) => format!("zlib:{l}"),
        Compression::Zstd(l) => format!("zstd:{l}"),
        _ => s.to_string(),
    };
    Some((label, comp))
}

#[allow(clippy::too_many_lines)]
fn decode_and_write<W: Write>(path: &Path, writer: &mut PbfWriter<W>) -> Counts {
    let file = std::fs::File::open(path).expect("open pbf");
    let reader = BlobReader::new(BufReader::new(file));

    let mut bb = BlockBuilder::new();
    let mut counts = Counts::default();

    for blob_result in reader {
        let blob = blob_result.expect("read blob");
        match blob.decode().expect("decode blob") {
            BlobDecode::OsmHeader(_) => {}
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    match &element {
                        Element::DenseNode(dn) => {
                            if !bb.can_add_node() {
                                flush(&mut bb, writer);
                            }
                            let tags: Vec<(&str, &str)> = dn.tags().collect();
                            bb.add_node(
                                dn.id(),
                                dn.decimicro_lat(),
                                dn.decimicro_lon(),
                                &tags,
                                None,
                            );
                            counts.nodes += 1;
                        }
                        Element::Node(n) => {
                            if !bb.can_add_node() {
                                flush(&mut bb, writer);
                            }
                            let tags: Vec<(&str, &str)> = n.tags().collect();
                            bb.add_node(
                                n.id(),
                                n.decimicro_lat(),
                                n.decimicro_lon(),
                                &tags,
                                None,
                            );
                            counts.nodes += 1;
                        }
                        Element::Way(w) => {
                            if !bb.can_add_way() {
                                flush(&mut bb, writer);
                            }
                            let tags: Vec<(&str, &str)> = w.tags().collect();
                            let refs: Vec<i64> = w.refs().collect();
                            bb.add_way(w.id(), &tags, &refs, None);
                            counts.ways += 1;
                        }
                        Element::Relation(r) => {
                            if !bb.can_add_relation() {
                                flush(&mut bb, writer);
                            }
                            let tags: Vec<(&str, &str)> = r.tags().collect();
                            let members: Vec<pbfhogg::block_builder::MemberData<'_>> = r
                                .members()
                                .map(|m| pbfhogg::block_builder::MemberData {
                                    id: m.id,
                                    role: m.role().ok().unwrap_or(""),
                                })
                                .collect();
                            bb.add_relation(r.id(), &tags, &members, None);
                            counts.relations += 1;
                        }
                        _ => {}
                    }
                }
            }
            BlobDecode::Unknown(_) => {}
            _ => {}
        }
    }

    // Flush remaining
    if let Some(bytes) = bb.take().expect("take final block") {
        writer.write_primitive_block(bytes).expect("write final block");
    }

    counts
}

#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn bench_sync(path: &Path, compression: Compression) -> (u64, Counts) {
    let header_bytes = pbfhogg::block_builder::build_header(None, None, None, None, &[])
        .expect("build header");

    let mut writer = PbfWriter::to_path(Path::new("/dev/null"), compression)
        .expect("open writer");
    writer.write_header(&header_bytes).expect("write header");

    let start = Instant::now();
    let counts = decode_and_write(path, &mut writer);
    (start.elapsed().as_millis() as u64, counts)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn bench_pipelined(path: &Path, compression: Compression) -> (u64, Counts) {
    let header_bytes = pbfhogg::block_builder::build_header(None, None, None, None, &[])
        .expect("build header");

    let mut writer = PbfWriter::to_path_pipelined(
        Path::new("/dev/null"),
        compression,
        &header_bytes,
    )
    .expect("open pipelined writer");

    let start = Instant::now();
    let counts = decode_and_write(path, &mut writer);
    drop(writer); // flush + join writer thread
    (start.elapsed().as_millis() as u64, counts)
}

fn emit(mode: &str, elapsed_ms: u64, counts: &Counts, file_mb: u64) {
    eprintln!("---");
    eprintln!("tool=pbfhogg");
    eprintln!("mode={mode}");
    eprintln!("elapsed_ms={elapsed_ms}");
    eprintln!("nodes={}", counts.nodes);
    eprintln!("ways={}", counts.ways);
    eprintln!("relations={}", counts.relations);
    eprintln!("elements={}", counts.total());
    eprintln!("file_mb={file_mb}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: bench_write <file.osm.pbf> [runs] [--compression none,zlib:6,zstd:3]");
        std::process::exit(1);
    }

    let path = Path::new(&args[1]);
    let runs: usize = args
        .iter()
        .skip(2)
        .find(|a| !a.starts_with('-'))
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);

    let comp_str = args
        .iter()
        .position(|a| a == "--compression")
        .and_then(|i| args.get(i + 1))
        .map_or(DEFAULT_COMPRESSIONS, |s| s.as_str());

    let compressions: Vec<(String, Compression)> = comp_str
        .split(',')
        .filter_map(|s| parse_compression(s.trim()))
        .collect();

    if compressions.is_empty() {
        eprintln!("No valid compression modes specified");
        std::process::exit(1);
    }

    let file_mb = std::fs::metadata(path)
        .map(|m| m.len() / 1_000_000)
        .unwrap_or(0);

    eprintln!("=== pbfhogg write benchmark ===");
    eprintln!("file: {}", path.display());
    eprintln!("size: {file_mb} MB");
    eprintln!("runs: {runs} (best of)");
    eprintln!();

    for (comp_label, comp) in &compressions {
        // Sync writer
        let mode = format!("write-{comp_label}");
        let mut best_ms = u64::MAX;
        let mut best_counts = Counts::default();
        for _ in 0..runs {
            let (ms, counts) = bench_sync(path, *comp);
            if ms < best_ms {
                best_ms = ms;
                best_counts = counts;
            }
        }
        emit(&mode, best_ms, &best_counts, file_mb);

        // Pipelined writer
        let mode = format!("write-pipe-{comp_label}");
        let mut best_ms = u64::MAX;
        let mut best_counts = Counts::default();
        for _ in 0..runs {
            let (ms, counts) = bench_pipelined(path, *comp);
            if ms < best_ms {
                best_ms = ms;
                best_counts = counts;
            }
        }
        emit(&mode, best_ms, &best_counts, file_mb);
    }
}
