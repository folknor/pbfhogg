//! Benchmark: count all elements using each pbfhogg read mode.
//!
//! Usage: bench_read <file.osm.pbf> [runs]

use pbfhogg::{BlobDecode, BlobReader, Element, ElementReader, Mmap, MmapBlobReader};
use std::io::BufReader;
use std::path::Path;
use std::time::Instant;

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

fn bench_sequential(path: &Path) -> (u64, Counts) {
    let reader = ElementReader::from_path(path).expect("open pbf");
    let mut counts = Counts::default();
    let start = Instant::now();

    reader
        .for_each(|element| match element {
            Element::Node(_) | Element::DenseNode(_) => counts.nodes += 1,
            Element::Way(_) => counts.ways += 1,
            Element::Relation(_) => counts.relations += 1,
        })
        .expect("read pbf");

    (start.elapsed().as_millis() as u64, counts)
}

fn bench_parallel(path: &Path) -> (u64, Counts) {
    let reader = ElementReader::from_path(path).expect("open pbf");
    let start = Instant::now();

    let (nodes, ways, relations) = reader
        .par_map_reduce(
            |element| match element {
                Element::Node(_) | Element::DenseNode(_) => (1u64, 0u64, 0u64),
                Element::Way(_) => (0, 1, 0),
                Element::Relation(_) => (0, 0, 1),
            },
            || (0, 0, 0),
            |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2),
        )
        .expect("read pbf");

    (
        start.elapsed().as_millis() as u64,
        Counts {
            nodes,
            ways,
            relations,
        },
    )
}

fn bench_pipelined(path: &Path) -> (u64, Counts) {
    let reader = ElementReader::from_path(path).expect("open pbf");
    let mut counts = Counts::default();
    let start = Instant::now();

    reader
        .for_each_pipelined(|element| match element {
            Element::Node(_) | Element::DenseNode(_) => counts.nodes += 1,
            Element::Way(_) => counts.ways += 1,
            Element::Relation(_) => counts.relations += 1,
        })
        .expect("read pbf");

    (start.elapsed().as_millis() as u64, counts)
}

fn bench_mmap(path: &Path) -> (u64, Counts) {
    let mmap = unsafe { Mmap::from_path(path) }.expect("mmap pbf");
    let reader = MmapBlobReader::new(mmap);
    let mut counts = Counts::default();
    let start = Instant::now();

    for blob_result in reader {
        let blob = blob_result.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::Node(_) | Element::DenseNode(_) => counts.nodes += 1,
                    Element::Way(_) => counts.ways += 1,
                    Element::Relation(_) => counts.relations += 1,
                }
            }
        }
    }

    (start.elapsed().as_millis() as u64, counts)
}

fn bench_mmap_blobread(path: &Path) -> (u64, Counts) {
    let file = std::fs::File::open(path).expect("open pbf");
    let reader = BlobReader::new(BufReader::new(file));
    let mut counts = Counts::default();
    let start = Instant::now();

    for blob_result in reader {
        let blob = blob_result.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::Node(_) | Element::DenseNode(_) => counts.nodes += 1,
                    Element::Way(_) => counts.ways += 1,
                    Element::Relation(_) => counts.relations += 1,
                }
            }
        }
    }

    (start.elapsed().as_millis() as u64, counts)
}

fn emit(tool: &str, mode: &str, elapsed_ms: u64, counts: &Counts, file_mb: u64) {
    eprintln!("---");
    eprintln!("tool={tool}");
    eprintln!("mode={mode}");
    eprintln!("elapsed_ms={elapsed_ms}");
    eprintln!("nodes={}", counts.nodes);
    eprintln!("ways={}", counts.ways);
    eprintln!("relations={}", counts.relations);
    eprintln!("elements={}", counts.total());
    eprintln!("file_mb={file_mb}");
}

fn run_bench(
    name: &str,
    path: &Path,
    file_mb: u64,
    runs: usize,
    f: fn(&Path) -> (u64, Counts),
) {
    let mut best_ms = u64::MAX;
    let mut best_counts = Counts::default();

    for _ in 0..runs {
        let (ms, counts) = f(path);
        if ms < best_ms {
            best_ms = ms;
            best_counts = counts;
        }
    }

    emit("pbfhogg", name, best_ms, &best_counts, file_mb);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: bench_read <file.osm.pbf> [runs]");
        std::process::exit(1);
    }

    let path = Path::new(&args[1]);
    let runs: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);

    let file_mb = std::fs::metadata(path)
        .map(|m| m.len() / 1_000_000)
        .unwrap_or(0);

    eprintln!("=== pbfhogg read benchmark ===");
    eprintln!("file: {}", path.display());
    eprintln!("size: {file_mb} MB");
    eprintln!("runs: {runs} (best of)");
    eprintln!();

    run_bench("sequential", path, file_mb, runs, bench_sequential);
    run_bench("parallel", path, file_mb, runs, bench_parallel);
    run_bench("pipelined", path, file_mb, runs, bench_pipelined);
    run_bench("mmap", path, file_mb, runs, bench_mmap);
    run_bench("blobreader", path, file_mb, runs, bench_mmap_blobread);
}
