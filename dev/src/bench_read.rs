//! Benchmark: count all elements using each pbfhogg read mode.
//!
//! Ports the logic from `examples/bench_read.rs` into the dev harness.

use std::io::BufReader;
use std::path::Path;
use std::time::Instant;

use pbfhogg::{BlobDecode, BlobReader as PbfBlobReader, Element, ElementReader, Mmap, MmapBlobReader};

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Read mode for the benchmark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    Sequential,
    Parallel,
    Pipelined,
    Mmap,
    BlobReader,
}

impl ReadMode {
    /// Lowercase name for use as the benchmark variant.
    pub fn name(self) -> &'static str {
        match self {
            ReadMode::Sequential => "sequential",
            ReadMode::Parallel => "parallel",
            ReadMode::Pipelined => "pipelined",
            ReadMode::Mmap => "mmap",
            ReadMode::BlobReader => "blobreader",
        }
    }
}

/// All five read modes in benchmark order.
pub const ALL_MODES: &[ReadMode] = &[
    ReadMode::Sequential,
    ReadMode::Parallel,
    ReadMode::Pipelined,
    ReadMode::Mmap,
    ReadMode::BlobReader,
];

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
// Parse helpers
// ---------------------------------------------------------------------------

/// Parse a comma-separated list of mode names into `ReadMode` values.
///
/// Accepted names (case-insensitive): sequential, parallel, pipelined, mmap, blobreader.
pub fn parse_modes(input: &str) -> Result<Vec<ReadMode>, DevError> {
    let mut modes = Vec::new();
    for token in input.split(',') {
        let trimmed = token.trim();
        let mode = match trimmed.to_ascii_lowercase().as_str() {
            "sequential" => ReadMode::Sequential,
            "parallel" => ReadMode::Parallel,
            "pipelined" => ReadMode::Pipelined,
            "mmap" => ReadMode::Mmap,
            "blobreader" => ReadMode::BlobReader,
            _ => return Err(DevError::Config(format!("unknown read mode: {trimmed}"))),
        };
        modes.push(mode);
    }
    Ok(modes)
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the read benchmark for each requested mode.
pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    modes: &[ReadMode],
) -> Result<(), DevError> {
    let basename = pbf_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    for &mode in modes {
        output::bench_msg(&format!("mode: {}", mode.name()));

        let config = BenchConfig {
            command: "bench read".into(),
            variant: Some(mode.name().into()),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: Some("zlib-ng".into()),
            cargo_profile: "release".into(),
            runs,
        };

        harness.run_internal(&config, |_i| run_single_mode(mode, pbf_path))?;
    }

    Ok(())
}

/// Dispatch a single benchmark iteration for the given mode.
fn run_single_mode(mode: ReadMode, path: &Path) -> Result<BenchResult, DevError> {
    let (elapsed_ms, counts) = match mode {
        ReadMode::Sequential => bench_sequential(path)?,
        ReadMode::Parallel => bench_parallel(path)?,
        ReadMode::Pipelined => bench_pipelined(path)?,
        ReadMode::Mmap => bench_mmap(path)?,
        ReadMode::BlobReader => bench_blobreader(path)?,
    };

    Ok(BenchResult {
        elapsed_ms,
        extra: Some(serde_json::json!({
            "nodes": counts.nodes,
            "ways": counts.ways,
            "relations": counts.relations,
        })),
    })
}

// ---------------------------------------------------------------------------
// Mode functions
// ---------------------------------------------------------------------------

fn bench_sequential(path: &Path) -> Result<(i64, Counts), DevError> {
    let reader = ElementReader::from_path(path)?;
    let mut counts = Counts::default();
    let start = Instant::now();

    reader.for_each(|element| match element {
        Element::Node(_) | Element::DenseNode(_) => counts.nodes += 1,
        Element::Way(_) => counts.ways += 1,
        Element::Relation(_) => counts.relations += 1,
        _ => {}
    })?;

    let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    Ok((elapsed_ms, counts))
}

fn bench_parallel(path: &Path) -> Result<(i64, Counts), DevError> {
    let reader = ElementReader::from_path(path)?;
    let start = Instant::now();

    let (nodes, ways, relations) = reader.par_map_reduce(
        |element| match element {
            Element::Node(_) | Element::DenseNode(_) => (1u64, 0u64, 0u64),
            Element::Way(_) => (0, 1, 0),
            Element::Relation(_) => (0, 0, 1),
            _ => (0, 0, 0),
        },
        || (0, 0, 0),
        |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2),
    )?;

    let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    Ok((elapsed_ms, Counts { nodes, ways, relations }))
}

fn bench_pipelined(path: &Path) -> Result<(i64, Counts), DevError> {
    let reader = ElementReader::from_path(path)?;
    let mut counts = Counts::default();
    let start = Instant::now();

    reader.for_each_pipelined(|element| match element {
        Element::Node(_) | Element::DenseNode(_) => counts.nodes += 1,
        Element::Way(_) => counts.ways += 1,
        Element::Relation(_) => counts.relations += 1,
        _ => {}
    })?;

    let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    Ok((elapsed_ms, counts))
}

fn bench_mmap(path: &Path) -> Result<(i64, Counts), DevError> {
    // SAFETY: file is read-only during benchmark
    let mmap = unsafe { Mmap::from_path(path) }?;
    let reader = MmapBlobReader::new(mmap);
    let mut counts = Counts::default();
    let start = Instant::now();

    for blob_result in reader {
        let blob = blob_result?;
        if let BlobDecode::OsmData(block) = blob.decode()? {
            for element in block.elements() {
                match element {
                    Element::Node(_) | Element::DenseNode(_) => counts.nodes += 1,
                    Element::Way(_) => counts.ways += 1,
                    Element::Relation(_) => counts.relations += 1,
                    _ => {}
                }
            }
        }
    }

    let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    Ok((elapsed_ms, counts))
}

fn bench_blobreader(path: &Path) -> Result<(i64, Counts), DevError> {
    let file = std::fs::File::open(path)?;
    let reader = PbfBlobReader::new(BufReader::new(file));
    let mut counts = Counts::default();
    let start = Instant::now();

    for blob_result in reader {
        let blob = blob_result?;
        if let BlobDecode::OsmData(block) = blob.decode()? {
            for element in block.elements() {
                match element {
                    Element::Node(_) | Element::DenseNode(_) => counts.nodes += 1,
                    Element::Way(_) => counts.ways += 1,
                    Element::Relation(_) => counts.relations += 1,
                    _ => {}
                }
            }
        }
    }

    let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    Ok((elapsed_ms, counts))
}
