//! pbfhogg's benchmark instruments, over the library's public API, kept out
//! of the user-facing `pbfhogg` binary. The script benches drive them
//! (`geocode-query`, `merge-workload` in `brokkr.d/benches.toml`); each
//! prints a key=value block on stderr for its script to parse.
//!
//! The lifecycle, in order: SIGINT and SIGTERM become a cancellation
//! request before any thread exists (`signals`); the host is assembled
//! (`host`); the hotpath guard starts only when brokkr asks for a report;
//! the instrument runs; then, whatever it returned, the invocation's writer
//! and read counters and the degradation reports are emitted, and the
//! guard drops on the ordinary return path, so a stopped profiled run still
//! writes its report. Exit statuses: 0 done, 1 failed, 2 usage, 130 or 143
//! when SIGINT or SIGTERM cancelled the run.

mod geocode;
mod host;
mod merge;
mod signals;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

// hotpath counts allocations only through this allocator, which the
// executable declares itself.
#[cfg(feature = "hotpath-alloc")]
#[global_allocator]
static ALLOC: hotpath::CountingAllocator = hotpath::CountingAllocator::new();

#[derive(Parser)]
#[command(name = "pbfhogg-bench", about = "pbfhogg benchmark instruments")]
struct Cli {
    #[command(subcommand)]
    instrument: Instrument,
}

#[derive(Subcommand)]
enum Instrument {
    /// Build a geocode index for geocode-query (untimed setup): the
    /// builder's defaults, an existing index replaced, an unindexed input
    /// accepted
    GeocodeBuild {
        /// Input PBF file
        input: PathBuf,
        /// Index directory to write
        #[arg(long)]
        output_dir: PathBuf,
        /// Directory the run's scratch directory is made in
        #[arg(long)]
        tmp_dir: PathBuf,
    },
    /// Time reverse geocoding queries over fixed pseudo-random points in a
    /// bbox against a built index
    GeocodeQuery {
        /// Geocode index directory
        index_dir: PathBuf,
        /// Points are drawn uniformly from this box:
        /// min_lon,min_lat,max_lon,max_lat
        #[arg(long, value_parser = pbfhogg::extract::parse_bbox)]
        bbox: pbfhogg::extract::Bbox,
        /// Points per round
        #[arg(long, default_value = "100000")]
        queries: u32,
        /// Seed of the point sequence
        #[arg(long, default_value = "1")]
        seed: u64,
        /// Timed rounds after the untimed warm-up round
        #[arg(long, default_value = "3")]
        rounds: u32,
    },
    /// Merge a base PBF with an OSC into an output and report its timing
    /// and statistics
    Merge {
        /// Base PBF file
        base: PathBuf,
        /// OSC file (.osc or .osc.gz)
        changes: PathBuf,
        /// Output file
        #[arg(short, long)]
        output: PathBuf,
        /// Replace the output if it already exists
        #[arg(short = 'O', long)]
        overwrite: bool,
        /// Stage the output in this directory instead of beside it
        #[arg(long)]
        staging_dir: Option<PathBuf>,
        /// Blob compression: none, zlib or zstd, each optionally `:LEVEL`
        #[arg(long, default_value = "zlib")]
        compression: pbfhogg::writer::Compression,
        /// Output backend
        #[arg(long, value_enum, default_value = "buffered")]
        io_mode: merge::IoMode,
    },
}

/// Why an instrument failed.
pub(crate) enum Failure {
    Library(pbfhogg::Error),
    Message(String),
    /// The run's invocation was cancelled outside a library call.
    Cancelled,
}

impl From<pbfhogg::Error> for Failure {
    fn from(error: pbfhogg::Error) -> Self {
        Self::Library(error)
    }
}

fn main() -> ExitCode {
    let (invocation, fifo) = host::invocation();
    if let Err(e) = signals::install(invocation.clone()) {
        eprintln!(
            "Warning: cannot take SIGINT and SIGTERM ({e}); a stopped profiled run will write no report"
        );
    }
    let host = host::host(invocation);
    // Only when brokkr asks for a report: without the request an
    // instrumented build would start hotpath's metrics server.
    #[cfg(feature = "hotpath")]
    let _guard = std::env::var_os("HOTPATH_OUTPUT_PATH").is_some().then(|| {
        hotpath::HotpathGuardBuilder::new("pbfhogg_bench::main")
            .percentiles(&[50.0, 95.0, 99.0])
            .functions_limit(0)
            .build()
    });

    // `try_parse`, never `parse`: clap's own exit would skip the guard.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            drop(e.print());
            return ExitCode::from(if e.use_stderr() { 2 } else { 0 });
        }
    };

    let result = run(cli.instrument, &host, host::Markers(fifo.as_ref()));
    pbfhogg::write::emit_writer_counters(&host.invocation);
    pbfhogg::read::emit_read_counters(&host.invocation);
    pbfhogg::write::report_writer_degradations(&host);
    host.report_io_degradations();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Cancelled) => {
            eprintln!("Error: interrupted: the run stopped before completing");
            ExitCode::from(signals::interrupted_status().unwrap_or(130))
        }
        Err(Failure::Library(e)) if e.is_cancelled() => {
            eprintln!("Error: interrupted: the run stopped before completing");
            ExitCode::from(signals::interrupted_status().unwrap_or(130))
        }
        Err(Failure::Library(e)) => {
            eprintln!("Error: {}", e.report());
            ExitCode::FAILURE
        }
        Err(Failure::Message(message)) => {
            eprintln!("Error: {message}");
            ExitCode::FAILURE
        }
    }
    // `_guard` drops here, writing the hotpath report.
}

fn run(
    instrument: Instrument,
    host: &pbfhogg::host::Host,
    markers: host::Markers<'_>,
) -> Result<(), Failure> {
    match instrument {
        Instrument::GeocodeBuild {
            input,
            output_dir,
            tmp_dir,
        } => geocode::build(&input, &output_dir, &tmp_dir, host).map_err(Failure::from),
        Instrument::GeocodeQuery {
            index_dir,
            bbox,
            queries,
            seed,
            rounds,
        } => geocode::query(
            &index_dir,
            &geocode::Workload {
                bbox,
                queries,
                seed,
                rounds,
            },
            host,
            markers,
        ),
        Instrument::Merge {
            base,
            changes,
            output,
            overwrite,
            staging_dir,
            compression,
            io_mode,
        } => merge::merge(
            &base,
            &changes,
            &merge::Target {
                output: &output,
                overwrite,
                staging_dir: staging_dir.as_deref(),
                compression,
                io_mode,
            },
            host,
        ),
    }
}
