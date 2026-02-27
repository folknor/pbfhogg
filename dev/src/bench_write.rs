//! Benchmark: read all elements then write them back through BlockBuilder + PbfWriter.
//!
//! Ports the logic from `examples/bench_write.rs` into the dev harness.
//! For each compression mode, runs two writer variants:
//! 1. sync — `PbfWriter::to_path` with header written before timing starts
//! 2. pipelined — `PbfWriter::to_path_pipelined` with header in constructor

use std::io::{BufReader, Write};
use std::path::Path;
use std::time::Instant;

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;

// ---------------------------------------------------------------------------
// Counts
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counts {
    nodes: u64,
    ways: u64,
    relations: u64,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a comma-separated list of compression specs into `(label, Compression)` pairs.
///
/// Accepted formats: `none`, `zlib`, `zlib:6`, `zstd`, `zstd:3`.
/// Defaults: zlib level 6, zstd level 3.
pub fn parse_compressions(
    input: &str,
) -> Result<Vec<(String, Compression)>, DevError> {
    let mut result = Vec::new();
    for token in input.split(',') {
        let trimmed = token.trim();
        let (name, level_str) = match trimmed.split_once(':') {
            Some((n, l)) => (n, Some(l)),
            None => (trimmed, None),
        };
        let comp = match name {
            "none" => Compression::None,
            "zlib" => {
                let level = parse_zlib_level(level_str, 6)?;
                Compression::Zlib(level)
            }
            "zstd" => {
                let level = parse_zstd_level(level_str, 3)?;
                Compression::Zstd(level)
            }
            _ => return Err(DevError::Config(format!("unknown compression: {trimmed}"))),
        };
        let label = match comp {
            Compression::None => "none".to_owned(),
            Compression::Zlib(l) => format!("zlib:{l}"),
            Compression::Zstd(l) => format!("zstd:{l}"),
            _ => trimmed.to_owned(),
        };
        result.push((label, comp));
    }
    Ok(result)
}

/// Run the write benchmark for each compression mode (sync + pipelined variants).
pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    compressions: &[(String, Compression)],
) -> Result<(), DevError> {
    let basename = pbf_basename(pbf_path);

    for (label, comp) in compressions {
        run_sync_variant(harness, pbf_path, &basename, file_mb, runs, label, *comp)?;
        run_pipe_variant(harness, pbf_path, &basename, file_mb, runs, label, *comp)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Variant runners
// ---------------------------------------------------------------------------

fn run_sync_variant(
    harness: &BenchHarness,
    pbf_path: &Path,
    basename: &str,
    file_mb: f64,
    runs: usize,
    label: &str,
    comp: Compression,
) -> Result<BenchResult, DevError> {
    let variant = format!("sync-{label}");
    output::bench_msg(&format!("variant: {variant}"));

    let config = build_config(&variant, basename, file_mb, runs);

    harness.run_internal(&config, |_i| {
        bench_sync(pbf_path, comp)
    })
}

fn run_pipe_variant(
    harness: &BenchHarness,
    pbf_path: &Path,
    basename: &str,
    file_mb: f64,
    runs: usize,
    label: &str,
    comp: Compression,
) -> Result<BenchResult, DevError> {
    let variant = format!("pipe-{label}");
    output::bench_msg(&format!("variant: {variant}"));

    let config = build_config(&variant, basename, file_mb, runs);

    harness.run_internal(&config, |_i| {
        bench_pipelined(pbf_path, comp)
    })
}

// ---------------------------------------------------------------------------
// Benchmark functions
// ---------------------------------------------------------------------------

fn bench_sync(path: &Path, comp: Compression) -> Result<BenchResult, DevError> {
    let header_bytes = HeaderBuilder::new().build()?;
    let mut writer = PbfWriter::to_path(Path::new("/dev/null"), comp)?;
    writer.write_header(&header_bytes)?;

    let start = Instant::now();
    let counts = decode_and_write(path, &mut writer)?;
    let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);

    Ok(counts_to_result(elapsed_ms, &counts))
}

fn bench_pipelined(path: &Path, comp: Compression) -> Result<BenchResult, DevError> {
    let header_bytes = HeaderBuilder::new().build()?;
    let mut writer = PbfWriter::to_path_pipelined(
        Path::new("/dev/null"),
        comp,
        &header_bytes,
    )?;

    let start = Instant::now();
    let counts = decode_and_write(path, &mut writer)?;
    drop(writer); // flush + join writer thread
    let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);

    Ok(counts_to_result(elapsed_ms, &counts))
}

// ---------------------------------------------------------------------------
// Decode + write loop
// ---------------------------------------------------------------------------

/// Read all elements from a PBF file and write them through BlockBuilder.
fn decode_and_write<W: Write>(
    path: &Path,
    writer: &mut PbfWriter<W>,
) -> Result<Counts, DevError> {
    let file = std::fs::File::open(path)?;
    let reader = BlobReader::new(BufReader::new(file));

    let mut bb = BlockBuilder::new();
    let mut counts = Counts::default();

    for blob_result in reader {
        let blob = blob_result?;
        if let BlobDecode::OsmData(block) = blob.decode()? {
            process_block(&block, &mut bb, writer, &mut counts)?;
        }
    }

    // Flush remaining
    flush_block(&mut bb, writer)?;

    Ok(counts)
}

/// Process all elements in a single decoded block.
fn process_block<W: Write>(
    block: &pbfhogg::block::PrimitiveBlock,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<W>,
    counts: &mut Counts,
) -> Result<(), DevError> {
    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                write_node(bb, writer, dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &dn.tags().collect::<Vec<_>>())?;
                counts.nodes += 1;
            }
            Element::Node(n) => {
                write_node(bb, writer, n.id(), n.decimicro_lat(), n.decimicro_lon(), &n.tags().collect::<Vec<_>>())?;
                counts.nodes += 1;
            }
            Element::Way(w) => {
                write_way(bb, writer, w)?;
                counts.ways += 1;
            }
            Element::Relation(r) => {
                write_relation(bb, writer, r)?;
                counts.relations += 1;
            }
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Element write helpers
// ---------------------------------------------------------------------------

fn write_node<W: Write>(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<W>,
    id: i64,
    lat: i32,
    lon: i32,
    tags: &[(&str, &str)],
) -> Result<(), DevError> {
    if !bb.can_add_node() {
        flush_block(bb, writer)?;
    }
    bb.add_node(id, lat, lon, tags, None);
    Ok(())
}

fn write_way<W: Write>(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<W>,
    way: &pbfhogg::elements::Way<'_>,
) -> Result<(), DevError> {
    if !bb.can_add_way() {
        flush_block(bb, writer)?;
    }
    let tags: Vec<(&str, &str)> = way.tags().collect();
    let refs: Vec<i64> = way.refs().collect();
    bb.add_way(way.id(), &tags, &refs, None);
    Ok(())
}

fn write_relation<W: Write>(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<W>,
    rel: &pbfhogg::elements::Relation<'_>,
) -> Result<(), DevError> {
    if !bb.can_add_relation() {
        flush_block(bb, writer)?;
    }
    let tags: Vec<(&str, &str)> = rel.tags().collect();
    let members: Vec<MemberData<'_>> = rel
        .members()
        .map(|m| MemberData {
            id: m.id,
            role: m.role().ok().unwrap_or_default(),
        })
        .collect();
    bb.add_relation(rel.id(), &tags, &members, None);
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Flush the current block (if any) to the writer.
fn flush_block<W: Write>(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<W>,
) -> Result<(), DevError> {
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(bytes)?;
    }
    Ok(())
}

/// Build a `BenchConfig` for a write benchmark variant.
fn build_config(variant: &str, basename: &str, file_mb: f64, runs: usize) -> BenchConfig {
    BenchConfig {
        command: "bench write".into(),
        variant: Some(variant.into()),
        input_file: Some(basename.into()),
        input_mb: Some(file_mb),
        cargo_features: Some("zlib-ng".into()),
        cargo_profile: "release".into(),
        runs,
    }
}

/// Convert counts into a `BenchResult` with JSON extra data.
fn counts_to_result(elapsed_ms: i64, counts: &Counts) -> BenchResult {
    BenchResult {
        elapsed_ms,
        extra: Some(serde_json::json!({
            "nodes": counts.nodes,
            "ways": counts.ways,
            "relations": counts.relations,
        })),
    }
}

/// Extract the file basename from a path.
fn pbf_basename(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned()
}

/// Parse an optional zlib level string, falling back to a default.
fn parse_zlib_level(level_str: Option<&str>, default: u32) -> Result<u32, DevError> {
    match level_str {
        Some(s) => s
            .parse()
            .map_err(|_| DevError::Config(format!("invalid compression level: {s}"))),
        None => Ok(default),
    }
}

/// Parse an optional zstd level string, falling back to a default.
fn parse_zstd_level(level_str: Option<&str>, default: i32) -> Result<i32, DevError> {
    match level_str {
        Some(s) => s
            .parse()
            .map_err(|_| DevError::Config(format!("invalid compression level: {s}"))),
        None => Ok(default),
    }
}
