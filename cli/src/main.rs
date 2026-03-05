// Optional allocator overrides. Benchmarked on Denmark (483 MB): both jemalloc and
// mimalloc showed <1% wall time difference vs the system allocator. Kept as opt-in
// features for consumers who want lower RSS at planet scale.
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc_crate::MiMalloc = mimalloc_crate::MiMalloc;

use std::path::PathBuf;
use std::process;

use clap::{Args, Parser, Subcommand, ValueEnum};
use pbfhogg::writer::Compression;

#[derive(Parser)]
#[command(name = "pbfhogg", about = "OpenStreetMap PBF toolkit")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct OutputArg {
    /// Output file
    #[arg(short, long)]
    output: PathBuf,
}

#[derive(Args)]
struct CompressionArg {
    /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
    #[arg(long, default_value = "zlib")]
    compression: String,
}

#[derive(Args)]
struct DirectIoArg {
    /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
    #[arg(long)]
    direct_io: bool,
}

#[derive(Args)]
struct ForceArg {
    /// Proceed even if input lacks indexdata (slower fallback path)
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct UringArg {
    /// Use io_uring for output I/O (requires linux-io-uring feature)
    #[arg(long)]
    io_uring: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum DefaultTypeArg {
    Node,
    Way,
    Relation,
}

#[derive(Subcommand)]
enum Command {
    /// Validate referential integrity
    CheckRefs {
        /// Input PBF file
        file: PathBuf,
        /// Also check relation member references
        #[arg(long)]
        check_relations: bool,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Count tag key=value frequencies
    TagsCount {
        /// Input PBF file
        file: PathBuf,
        /// Only show tags with at least this many occurrences
        #[arg(long, default_value = "1")]
        min_count: u64,
        /// Filter by element type: node, way, or relation
        #[arg(short = 't', long = "type")]
        type_filter: Option<String>,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
    /// Concatenate PBF files with optional type filtering
    Cat {
        /// Input PBF files
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[command(flatten)]
        output: OutputArg,
        /// Filter by element type (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type")]
        type_filter: Option<String>,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
    /// Sort PBF into standard order (nodes → ways → relations, by ID)
    Sort {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        uring: UringArg,
        #[command(flatten)]
        force: ForceArg,
    },
    /// Filter elements by tag expressions.
    ///
    /// Default mode (without `-R`) resolves relation members transitively:
    /// matched relations pull in member ways, member nodes, nested member
    /// relations, and node refs of included ways.
    TagsFilter {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Omit referenced objects (faster, single pass, direct matches only)
        #[arg(short = 'R', long = "omit-referenced")]
        omit_referenced: bool,
        /// Tag filter expressions (e.g. "highway=primary", "amenity", "w/building=yes")
        #[arg(required = true)]
        expressions: Vec<String>,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
    /// Filter OSC changes by tag expressions; always preserve deletes.
    TagsFilterOsc {
        /// Input OSC change file (.osc.gz)
        changes: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Tag filter expressions (e.g. "highway=primary", "amenity", "w/building=yes")
        #[arg(required = true)]
        expressions: Vec<String>,
    },
    /// Compare two PBF files and show differences.
    ///
    /// Uses content equality (coordinates, tags, refs, members) — not version/timestamp
    /// ordering. This means diff output is deterministic regardless of whether metadata
    /// is present, partial, or absent in either input.
    Diff {
        /// Old PBF file
        old: PathBuf,
        /// New PBF file
        new: PathBuf,
        /// Hide unchanged elements (show only created/modified/deleted)
        #[arg(short = 'c', long = "suppress-common")]
        suppress_common: bool,
        /// Show detailed changes for modified elements
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Exit-code only, suppress diff output and summary
        #[arg(short = 'q', long, conflicts_with = "output")]
        quiet: bool,
        /// Write diff output to file instead of stdout
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,
        /// Filter by element type (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type")]
        type_filter: Option<String>,
        /// Ignore changeset metadata when diffing (already ignored by content-equality mode)
        #[arg(long)]
        ignore_changeset: bool,
        /// Ignore uid metadata when diffing (already ignored by content-equality mode)
        #[arg(long)]
        ignore_uid: bool,
        /// Ignore user metadata when diffing (already ignored by content-equality mode)
        #[arg(long)]
        ignore_user: bool,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Generate OSC diff from two PBF snapshots
    DeriveChanges {
        /// Old PBF file
        old: PathBuf,
        /// New PBF file
        new: PathBuf,
        /// Bump version of deleted elements by 1 in the output OSC
        #[arg(long)]
        increment_version: bool,
        #[command(flatten)]
        output: OutputArg,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Extract elements by ID
    Getid {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Include referenced nodes of matching ways (two-pass)
        #[arg(short = 'r', long = "add-referenced")]
        add_referenced: bool,
        /// Read IDs from file instead of arguments
        #[arg(short = 'i', long = "id-file")]
        id_file: Option<PathBuf>,
        /// Default type for bare numeric IDs: node, way, relation
        #[arg(long = "default-type", value_enum)]
        default_type: Option<DefaultTypeArg>,
        /// Element IDs (e.g. n123 w456 r789)
        ids: Vec<String>,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
    /// Remove elements by ID
    Removeid {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Read IDs from file instead of arguments
        #[arg(short = 'i', long = "id-file")]
        id_file: Option<PathBuf>,
        /// Default type for bare numeric IDs: node, way, relation
        #[arg(long = "default-type", value_enum)]
        default_type: Option<DefaultTypeArg>,
        /// Element IDs (e.g. n123 w456 r789)
        ids: Vec<String>,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Extract elements within a geographic region (bbox or polygon)
    Extract {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Bounding box: minlon,minlat,maxlon,maxlat
        #[arg(short = 'b', long, group = "area")]
        bbox: Option<String>,
        /// Polygon GeoJSON file
        #[arg(short = 'p', long, group = "area")]
        polygon: Option<PathBuf>,
        /// Simple strategy (single pass, may have dangling refs)
        #[arg(short = 's', long, conflicts_with = "smart")]
        simple: bool,
        /// Smart strategy (three passes, complete multipolygon/boundary relations)
        #[arg(long, conflicts_with = "simple")]
        smart: bool,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
    /// Embed node coordinates in ways
    AddLocationsToWays {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Keep all untagged nodes in output (default: drop untagged nodes unless referenced by a relation)
        #[arg(long)]
        keep_untagged_nodes: bool,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
    /// Filter history PBF to a snapshot at a timestamp
    TimeFilter {
        /// Input history PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Snapshot cutoff timestamp (UNIX seconds or RFC3339 UTC: YYYY-MM-DDTHH:MM:SSZ)
        timestamp: String,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Inspect PBF file: metadata, block breakdown, ordering analysis
    Inspect {
        /// Input PBF file
        file: PathBuf,
        /// Show per-block distribution stats and optional block listing
        #[arg(long, num_args = 0..=1, default_missing_value = "0")]
        blocks: Option<usize>,
        /// Show min/max element IDs per type and monotonicity
        #[arg(long)]
        id_ranges: bool,
        /// Show locations-on-ways diagnostics
        #[arg(long)]
        locations: bool,
        /// Show only anomalous blocks (<50% or >150% of median, plus mixed blocks)
        #[arg(long)]
        anomalies: bool,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Analyze node coordinate statistics for FOR compression sizing
    NodeStats {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
    /// Apply OSC diffs to a PBF file
    Merge {
        /// Base PBF file
        base: PathBuf,
        /// OSC diff file (.osc.gz)
        changes: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        uring: UringArg,
        #[command(flatten)]
        force: ForceArg,
    },
    /// Merge multiple OSC files into one OSC file
    MergeChanges {
        /// Input OSC files (.osc or .osc.gz)
        #[arg(required = true)]
        changes: Vec<PathBuf>,
        #[command(flatten)]
        output: OutputArg,
        /// Keep only the last change per object (type + id)
        #[arg(long)]
        simplify: bool,
    },
    /// Check if a PBF file has blob-level indexdata
    IsIndexed {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Verify PBF file integrity
    Verify {
        #[command(subcommand)]
        command: VerifyCommand,
    },
    /// Benchmark: count elements using a single read mode (emits kv to stderr)
    BenchRead {
        /// Input PBF file
        file: PathBuf,
        /// Read mode: sequential, parallel, pipelined, blobreader
        #[arg(long)]
        mode: String,
    },
    /// Benchmark: decode + write all elements through BlockBuilder (emits kv to stderr)
    BenchWrite {
        /// Input PBF file
        file: PathBuf,
        /// Compression spec (e.g. none, zlib:6, zstd:3)
        #[arg(long, default_value = "zlib:6")]
        compression: String,
        /// Writer mode: sync or pipelined
        #[arg(long, default_value = "sync")]
        writer: String,
    },
    /// Benchmark: merge base PBF with OSC diff (emits kv to stderr)
    BenchMerge {
        /// Base PBF file
        base: PathBuf,
        /// OSC diff file (.osc.gz)
        changes: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Compression spec (e.g. none, zlib, zstd:3)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// I/O mode: buffered, uring
        #[arg(long, default_value = "buffered")]
        io_mode: String,
    },
}

#[derive(Subcommand)]
enum VerifyCommand {
    /// Check ID uniqueness and ordering
    Ids {
        /// Input PBF file
        file: PathBuf,
        /// Full duplicate detection via bitmap (slower, more memory, works on unsorted files)
        #[arg(long)]
        full: bool,
        /// Filter by element type (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type")]
        type_filter: Option<String>,
        /// Stop after N violations (0 = unlimited)
        #[arg(long, default_value = "100")]
        max_errors: usize,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
        /// Exit-code only, no output
        #[arg(long, conflicts_with = "json")]
        quiet: bool,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Validate referential integrity (wraps check-refs)
    Refs {
        /// Input PBF file
        file: PathBuf,
        /// Also check relation member references
        #[arg(long)]
        check_relations: bool,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
        /// Exit-code only, no output
        #[arg(long, conflicts_with = "json")]
        quiet: bool,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Run all verification checks (IDs + referential integrity)
    All {
        /// Input PBF file
        file: PathBuf,
        /// Full duplicate detection for ID check
        #[arg(long)]
        full: bool,
        /// Filter by element type for ID check (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type")]
        type_filter: Option<String>,
        /// Stop after N violations per check
        #[arg(long, default_value = "100")]
        max_errors: usize,
        /// Also check relation member references
        #[arg(long)]
        check_relations: bool,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
        /// Exit-code only, no output
        #[arg(long, conflicts_with = "json")]
        quiet: bool,
        #[command(flatten)]
        io: DirectIoArg,
    },
}

fn main() {
    let _guard = hotpath::HotpathGuardBuilder::new("pbfhogg::main")
        .percentiles(&[50, 95, 99])
        .with_functions_limit(0)
        .build();

    let cli = Cli::parse();

    let result = match cli.command {
        Command::CheckRefs {
            file,
            check_relations,
            io,
        } => run_check_refs(&file, check_relations, io.direct_io),
        Command::TagsCount {
            file,
            min_count,
            type_filter,
            force,
            io,
        } => run_tags_count(
            &file,
            min_count,
            type_filter.as_deref(),
            io.direct_io,
            force.force,
        ),
        Command::Cat {
            files,
            output,
            type_filter,
            compression,
            force,
            io,
        } => run_cat(
            &files,
            &output.output,
            type_filter.as_deref(),
            &compression.compression,
            io.direct_io,
            force.force,
        ),
        Command::Sort {
            file,
            output,
            compression,
            io,
            uring,
            force,
        } => run_sort(
            &file,
            &output.output,
            &compression.compression,
            io.direct_io,
            uring.io_uring,
            force.force,
        ),
        Command::TagsFilter {
            file,
            output,
            omit_referenced,
            expressions,
            compression,
            force,
            io,
        } => run_tags_filter(
            &file,
            &output.output,
            &expressions,
            omit_referenced,
            &compression.compression,
            io.direct_io,
            force.force,
        ),
        Command::TagsFilterOsc {
            changes,
            output,
            expressions,
        } => run_tags_filter_osc(&changes, &output.output, &expressions),
        Command::Diff {
            old,
            new,
            suppress_common,
            verbose,
            quiet,
            output,
            type_filter,
            ignore_changeset,
            ignore_uid,
            ignore_user,
            io,
        } => run_diff(
            &old,
            &new,
            suppress_common,
            verbose,
            quiet,
            output.as_deref(),
            type_filter.as_deref(),
            ignore_changeset,
            ignore_uid,
            ignore_user,
            io.direct_io,
        ),
        Command::DeriveChanges {
            old,
            new,
            increment_version,
            output,
            io,
        } => run_derive_changes(&old, &new, &output.output, io.direct_io, increment_version),
        Command::Getid {
            file,
            output,
            add_referenced,
            id_file,
            default_type,
            ids,
            compression,
            force,
            io,
        } => run_getid(
            &file,
            &output.output,
            add_referenced,
            id_file.as_deref(),
            default_type,
            &ids,
            &compression.compression,
            io.direct_io,
            force.force,
        ),
        Command::Removeid {
            file,
            output,
            id_file,
            default_type,
            ids,
            compression,
            io,
        } => run_removeid(
            &file,
            &output.output,
            id_file.as_deref(),
            default_type,
            &ids,
            &compression.compression,
            io.direct_io,
        ),
        Command::Extract {
            file,
            output,
            bbox,
            polygon,
            simple,
            smart,
            compression,
            force,
            io,
        } => run_extract(
            &file,
            &output.output,
            bbox.as_deref(),
            polygon.as_deref(),
            extract_strategy(simple, smart),
            &compression.compression,
            io.direct_io,
            force.force,
        ),
        Command::AddLocationsToWays {
            file,
            output,
            keep_untagged_nodes,
            compression,
            force,
            io,
        } => run_add_locations_to_ways(
            &file,
            &output.output,
            keep_untagged_nodes,
            &compression.compression,
            io.direct_io,
            force.force,
        ),
        Command::TimeFilter {
            file,
            output,
            timestamp,
            compression,
            io,
        } => run_time_filter(
            &file,
            &output.output,
            &timestamp,
            &compression.compression,
            io.direct_io,
        ),
        Command::Inspect {
            file,
            blocks,
            id_ranges,
            locations,
            anomalies,
            json,
            io,
        } => run_inspect(
            &file,
            blocks,
            id_ranges,
            locations,
            anomalies,
            json,
            io.direct_io,
        ),
        Command::NodeStats { file, io, force } => run_node_stats(&file, io.direct_io, force.force),
        Command::Merge {
            base,
            changes,
            output,
            compression,
            force,
            io,
            uring,
        } => run_merge(
            &base,
            &changes,
            &output.output,
            &compression.compression,
            io.direct_io,
            uring.io_uring,
            force.force,
        ),
        Command::MergeChanges {
            changes,
            output,
            simplify,
        } => run_merge_changes(&changes, &output.output, simplify),
        Command::IsIndexed { file, io } => run_is_indexed(&file, io.direct_io),
        Command::Verify { command } => match command {
            VerifyCommand::Ids {
                file,
                full,
                type_filter,
                max_errors,
                json,
                quiet,
                io,
            } => run_verify_ids(
                &file,
                full,
                type_filter.as_deref(),
                max_errors,
                json,
                quiet,
                io.direct_io,
            ),
            VerifyCommand::Refs {
                file,
                check_relations,
                json,
                quiet,
                io,
            } => run_verify_refs(&file, check_relations, json, quiet, io.direct_io),
            VerifyCommand::All {
                file,
                full,
                type_filter,
                max_errors,
                check_relations,
                json,
                quiet,
                io,
            } => run_verify_all(
                &file,
                full,
                type_filter.as_deref(),
                max_errors,
                check_relations,
                json,
                quiet,
                io.direct_io,
            ),
        },
        Command::BenchRead { file, mode } => run_bench_read(&file, &mode),
        Command::BenchWrite {
            file,
            compression,
            writer,
        } => run_bench_write(&file, &compression, &writer),
        Command::BenchMerge {
            base,
            changes,
            output,
            compression,
            io_mode,
        } => run_bench_merge(&base, &changes, &output.output, &compression, &io_mode),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn run_is_indexed(
    path: &std::path::Path,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if pbfhogg::has_indexdata(path, direct_io)? {
        println!("indexed");
    } else {
        println!("not indexed");
        process::exit(1);
    }
    Ok(())
}

fn run_verify_ids(
    path: &std::path::Path,
    full: bool,
    type_filter: Option<&str>,
    max_errors: usize,
    json: bool,
    quiet: bool,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use pbfhogg::verify_ids::VerifyIdsOptions;

    let opts = VerifyIdsOptions {
        full,
        type_filter,
        max_errors,
        direct_io,
    };
    let report = pbfhogg::verify_ids::verify_ids(path, &opts)?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("input.osm.pbf");
    if json {
        println!("{}", report.to_json(file_name)?);
    } else if !quiet {
        report.print_human(file_name);
    }

    if !report.passed {
        process::exit(1);
    }
    Ok(())
}

fn run_verify_refs(
    path: &std::path::Path,
    check_relations: bool,
    json: bool,
    quiet: bool,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let result = pbfhogg::check_refs::check_refs(path, check_relations, direct_io)?;

    if json {
        let value = serde_json::json!({
            "node_count": result.node_count,
            "way_count": result.way_count,
            "relation_count": result.relation_count,
            "missing_node_refs": result.missing_node_refs,
            "missing_way_refs": result.missing_way_refs,
            "missing_node_members": result.missing_node_members,
            "missing_relation_members": result.missing_relation_members,
            "passed": result.is_valid(),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if !quiet {
        println!(
            "Elements: {} nodes, {} ways, {} relations",
            result.node_count, result.way_count, result.relation_count
        );
        if result.missing_node_refs > 0 {
            println!("Missing node refs in ways: {}", result.missing_node_refs);
        }
        if check_relations {
            if result.missing_way_refs > 0 {
                println!("Missing way refs in relations: {}", result.missing_way_refs);
            }
            if result.missing_node_members > 0 {
                println!(
                    "Missing node members in relations: {}",
                    result.missing_node_members
                );
            }
            if result.missing_relation_members > 0 {
                println!(
                    "Missing relation members: {}",
                    result.missing_relation_members
                );
            }
        }
        if result.is_valid() {
            println!("Referential integrity: OK");
        } else {
            println!(
                "Referential integrity: FAILED ({} missing references)",
                result.total_missing()
            );
        }
    }

    if !result.is_valid() {
        process::exit(1);
    }
    Ok(())
}

fn run_verify_all(
    path: &std::path::Path,
    full: bool,
    type_filter: Option<&str>,
    max_errors: usize,
    check_relations: bool,
    json: bool,
    quiet: bool,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use pbfhogg::verify_ids::VerifyIdsOptions;

    // Run ID check
    let opts = VerifyIdsOptions {
        full,
        type_filter,
        max_errors,
        direct_io,
    };
    let ids_report = pbfhogg::verify_ids::verify_ids(path, &opts)?;

    // Run ref check
    let refs_result = pbfhogg::check_refs::check_refs(path, check_relations, direct_io)?;

    let all_passed = ids_report.passed && refs_result.is_valid();
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("input.osm.pbf");

    if json {
        let value = serde_json::json!({
            "ids": serde_json::from_str::<serde_json::Value>(&ids_report.to_json(file_name)?)?,
            "refs": {
                "node_count": refs_result.node_count,
                "way_count": refs_result.way_count,
                "relation_count": refs_result.relation_count,
                "missing_node_refs": refs_result.missing_node_refs,
                "missing_way_refs": refs_result.missing_way_refs,
                "missing_node_members": refs_result.missing_node_members,
                "missing_relation_members": refs_result.missing_relation_members,
                "passed": refs_result.is_valid(),
            },
            "passed": all_passed,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if !quiet {
        ids_report.print_human(file_name);
        println!();
        println!("---");
        println!();
        println!(
            "Elements: {} nodes, {} ways, {} relations",
            refs_result.node_count, refs_result.way_count, refs_result.relation_count
        );
        if refs_result.is_valid() {
            println!("Referential integrity: OK");
        } else {
            println!(
                "Referential integrity: FAILED ({} missing references)",
                refs_result.total_missing()
            );
        }
        println!();
        if all_passed {
            println!("All checks: PASSED");
        } else {
            println!("All checks: FAILED");
        }
    }

    if !all_passed {
        process::exit(1);
    }
    Ok(())
}

fn run_check_refs(
    path: &std::path::Path,
    check_relations: bool,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let result = pbfhogg::check_refs::check_refs(path, check_relations, direct_io)?;

    println!(
        "Elements: {} nodes, {} ways, {} relations",
        result.node_count, result.way_count, result.relation_count
    );

    if result.missing_node_refs > 0 {
        println!("Missing node refs in ways: {}", result.missing_node_refs);
    }
    if check_relations {
        if result.missing_way_refs > 0 {
            println!("Missing way refs in relations: {}", result.missing_way_refs);
        }
        if result.missing_node_members > 0 {
            println!(
                "Missing node members in relations: {}",
                result.missing_node_members
            );
        }
        if result.missing_relation_members > 0 {
            println!(
                "Missing relation members: {}",
                result.missing_relation_members
            );
        }
    }

    if result.is_valid() {
        println!("Referential integrity: OK");
    } else {
        println!(
            "Referential integrity: FAILED ({} missing references)",
            result.total_missing()
        );
        process::exit(1);
    }

    Ok(())
}

fn run_tags_count(
    path: &std::path::Path,
    min_count: u64,
    type_filter: Option<&str>,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let results = pbfhogg::tags_count::tags_count(path, min_count, type_filter, direct_io, force)?;

    for entry in &results {
        println!("{}\t{}\t{}", entry.count, entry.key, entry.value);
    }

    eprintln!("{} distinct tag values", results.len());
    Ok(())
}

fn run_cat(
    files: &[PathBuf],
    output: &std::path::Path,
    type_filter: Option<&str>,
    compression: &str,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let paths: Vec<&std::path::Path> = files.iter().map(AsRef::as_ref).collect();
    let stats = pbfhogg::cat::cat(&paths, output, type_filter, compression, direct_io, force)?;
    stats.print_summary();
    Ok(())
}

fn run_sort(
    file: &std::path::Path,
    output: &std::path::Path,
    compression: &str,
    direct_io: bool,
    io_uring: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let opts = pbfhogg::sort::SortOptions {
        compression,
        direct_io,
        io_uring,
        force,
    };
    let stats = pbfhogg::sort::sort(file, output, &opts)?;
    stats.print_summary();
    Ok(())
}

fn run_tags_filter(
    file: &std::path::Path,
    output: &std::path::Path,
    expressions: &[String],
    omit_referenced: bool,
    compression: &str,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let stats = pbfhogg::tags_filter::tags_filter(
        file,
        output,
        expressions,
        omit_referenced,
        compression,
        direct_io,
        force,
    )?;
    stats.print_summary();
    Ok(())
}

fn run_tags_filter_osc(
    changes: &std::path::Path,
    output: &std::path::Path,
    expressions: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::tags_filter_osc::tags_filter_osc(changes, output, expressions)?;
    stats.print_summary();
    Ok(())
}

fn resolve_ids(
    id_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
) -> Result<pbfhogg::getid::IdSet, Box<dyn std::error::Error>> {
    let default_type = default_type.map(|kind| match kind {
        DefaultTypeArg::Node => pbfhogg::getid::DefaultType::Node,
        DefaultTypeArg::Way => pbfhogg::getid::DefaultType::Way,
        DefaultTypeArg::Relation => pbfhogg::getid::DefaultType::Relation,
    });
    match id_file {
        Some(path) => pbfhogg::getid::parse_ids_from_file_with_default_type(path, default_type),
        None => pbfhogg::getid::parse_ids_with_default_type(ids, default_type),
    }
}

fn run_diff(
    old: &std::path::Path,
    new: &std::path::Path,
    suppress_common: bool,
    verbose: bool,
    quiet: bool,
    output_path: Option<&std::path::Path>,
    type_filter: Option<&str>,
    ignore_changeset: bool,
    ignore_uid: bool,
    ignore_user: bool,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let options = pbfhogg::diff::DiffOptions {
        suppress_common,
        verbose,
        type_filter: type_filter.map(String::from),
        ignore_changeset,
        ignore_uid,
        ignore_user,
    };
    let stats = if quiet {
        let mut sink = std::io::sink();
        pbfhogg::diff::diff(old, new, &mut sink, &options, direct_io)?
    } else if let Some(path) = output_path {
        let file = std::fs::File::create(path)?;
        let mut out = std::io::BufWriter::new(file);
        let stats = pbfhogg::diff::diff(old, new, &mut out, &options, direct_io)?;
        out.flush()?;
        stats
    } else {
        let mut stdout = std::io::stdout().lock();
        pbfhogg::diff::diff(old, new, &mut stdout, &options, direct_io)?
    };
    if !quiet {
        stats.print_summary();
    }
    if stats.has_differences() {
        process::exit(1);
    }
    Ok(())
}

fn run_derive_changes(
    old: &std::path::Path,
    new: &std::path::Path,
    output: &std::path::Path,
    direct_io: bool,
    increment_version: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats =
        pbfhogg::derive_changes::derive_changes(old, new, output, direct_io, increment_version)?;
    stats.print_summary();
    Ok(())
}

fn run_getid(
    file: &std::path::Path,
    output: &std::path::Path,
    add_referenced: bool,
    id_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
    compression: &str,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let id_set = resolve_ids(id_file, default_type, ids)?;
    let stats = pbfhogg::getid::getid(
        file,
        output,
        &id_set,
        add_referenced,
        compression,
        direct_io,
        force,
    )?;
    stats.print_summary();
    Ok(())
}

fn run_removeid(
    file: &std::path::Path,
    output: &std::path::Path,
    id_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
    compression: &str,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let id_set = resolve_ids(id_file, default_type, ids)?;
    let stats = pbfhogg::getid::removeid(file, output, &id_set, compression, direct_io)?;
    stats.print_summary();
    Ok(())
}

fn extract_strategy(simple: bool, smart: bool) -> pbfhogg::extract::ExtractStrategy {
    if simple {
        pbfhogg::extract::ExtractStrategy::Simple
    } else if smart {
        pbfhogg::extract::ExtractStrategy::Smart
    } else {
        pbfhogg::extract::ExtractStrategy::CompleteWays
    }
}

fn run_extract(
    file: &std::path::Path,
    output: &std::path::Path,
    bbox_str: Option<&str>,
    polygon_path: Option<&std::path::Path>,
    strategy: pbfhogg::extract::ExtractStrategy,
    compression: &str,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let region = match (bbox_str, polygon_path) {
        (Some(s), None) => {
            let bbox = pbfhogg::extract::parse_bbox(s)?;
            pbfhogg::extract::Region::Bbox(bbox)
        }
        (None, Some(p)) => pbfhogg::extract::parse_geojson(p)?,
        (None, None) => return Err("one of --bbox or --polygon is required".into()),
        (Some(_), Some(_)) => return Err("--bbox and --polygon are mutually exclusive".into()),
    };
    let stats = pbfhogg::extract::extract(
        file,
        output,
        &region,
        strategy,
        compression,
        direct_io,
        force,
    )?;
    stats.print_summary();
    Ok(())
}

fn run_add_locations_to_ways(
    file: &std::path::Path,
    output: &std::path::Path,
    keep_untagged_nodes: bool,
    compression: &str,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let stats = pbfhogg::add_locations_to_ways::add_locations_to_ways(
        file,
        output,
        keep_untagged_nodes,
        compression,
        direct_io,
        force,
    )?;
    stats.print_summary();
    Ok(())
}

fn run_time_filter(
    file: &std::path::Path,
    output: &std::path::Path,
    timestamp: &str,
    compression: &str,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let cutoff = parse_timestamp(timestamp)?;
    let stats = pbfhogg::time_filter::time_filter(file, output, cutoff, compression, direct_io)?;
    stats.print_summary();
    Ok(())
}

fn parse_timestamp(input: &str) -> Result<i64, Box<dyn std::error::Error>> {
    if let Ok(value) = input.parse::<i64>() {
        return Ok(value);
    }
    parse_rfc3339_utc(input).map_err(Into::into)
}

fn parse_rfc3339_utc(input: &str) -> Result<i64, String> {
    if input.len() != 20 {
        return Err("timestamp must be UNIX seconds or YYYY-MM-DDTHH:MM:SSZ".to_owned());
    }

    let bytes = input.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return Err("timestamp must be UNIX seconds or YYYY-MM-DDTHH:MM:SSZ".to_owned());
    }

    let year = parse_i32(input, 0, 4)?;
    let month = parse_u32(input, 5, 7)?;
    let day = parse_u32(input, 8, 10)?;
    let hour = parse_u32(input, 11, 13)?;
    let minute = parse_u32(input, 14, 16)?;
    let second = parse_u32(input, 17, 19)?;

    if !(1..=12).contains(&month) {
        return Err("invalid month in timestamp".to_owned());
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err("invalid time in timestamp".to_owned());
    }

    let max_day = days_in_month(year, month);
    if day == 0 || day > max_day {
        return Err("invalid day in timestamp".to_owned());
    }

    let days = days_from_civil(year, month, day);
    Ok(days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second))
}

fn parse_i32(s: &str, start: usize, end: usize) -> Result<i32, String> {
    s[start..end]
        .parse::<i32>()
        .map_err(|_| "invalid numeric timestamp component".to_owned())
}

fn parse_u32(s: &str, start: usize, end: usize) -> Result<u32, String> {
    s[start..end]
        .parse::<u32>()
        .map_err(|_| "invalid numeric timestamp component".to_owned())
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

// Gregorian civil date to days since Unix epoch (1970-01-01).
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let mut y = i64::from(year);
    let m = i64::from(month);
    let d = i64::from(day);
    y -= if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn run_inspect(
    path: &std::path::Path,
    blocks: Option<usize>,
    id_ranges: bool,
    locations: bool,
    anomalies: bool,
    json: bool,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let show_blocks = blocks.is_some() || anomalies;
    let block_limit = if anomalies && blocks.is_none() {
        Some(0)
    } else {
        blocks
    };
    let mut report = pbfhogg::inspect::inspect(path, show_blocks, id_ranges, locations, direct_io)?;
    if json {
        let value = report.to_json_filtered(block_limit, anomalies);
        println!("{value}");
    } else {
        report.print_report_filtered(block_limit, anomalies);
    }
    Ok(())
}

fn run_node_stats(
    path: &std::path::Path,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let report = pbfhogg::node_stats::node_stats(path, direct_io, force)?;
    report.print_report();
    Ok(())
}

fn run_merge(
    base: &std::path::Path,
    changes: &std::path::Path,
    output: &std::path::Path,
    compression: &str,
    direct_io: bool,
    io_uring: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let opts = pbfhogg::merge::MergeOptions {
        compression,
        direct_io,
        io_uring,
        force,
    };
    let stats = pbfhogg::merge::merge(base, changes, output, &opts)?;
    stats.print_summary();
    Ok(())
}

fn run_merge_changes(
    changes: &[PathBuf],
    output: &std::path::Path,
    simplify: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let inputs: Vec<&std::path::Path> = changes.iter().map(AsRef::as_ref).collect();
    let stats = pbfhogg::merge_changes::merge_changes(&inputs, output, simplify)?;
    stats.print_summary();
    Ok(())
}

fn run_bench_read(path: &std::path::Path, mode: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Instant;

    let (elapsed_ms, nodes, ways, rels): (u128, u64, u64, u64) = match mode {
        "sequential" => {
            let reader = pbfhogg::ElementReader::from_path(path)?;
            let mut nodes = 0u64;
            let mut ways = 0u64;
            let mut rels = 0u64;
            let start = Instant::now();
            reader.for_each(|el| match el {
                pbfhogg::Element::Node(_) | pbfhogg::Element::DenseNode(_) => nodes += 1,
                pbfhogg::Element::Way(_) => ways += 1,
                pbfhogg::Element::Relation(_) => rels += 1,
                _ => {}
            })?;
            (start.elapsed().as_millis(), nodes, ways, rels)
        }
        "parallel" => {
            let reader = pbfhogg::ElementReader::from_path(path)?;
            let start = Instant::now();
            let (nodes, ways, rels) = reader.par_map_reduce(
                |el| match el {
                    pbfhogg::Element::Node(_) | pbfhogg::Element::DenseNode(_) => {
                        (1u64, 0u64, 0u64)
                    }
                    pbfhogg::Element::Way(_) => (0, 1, 0),
                    pbfhogg::Element::Relation(_) => (0, 0, 1),
                    _ => (0, 0, 0),
                },
                || (0, 0, 0),
                |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2),
            )?;
            (start.elapsed().as_millis(), nodes, ways, rels)
        }
        "pipelined" => {
            let reader = pbfhogg::ElementReader::from_path(path)?;
            let mut nodes = 0u64;
            let mut ways = 0u64;
            let mut rels = 0u64;
            let start = Instant::now();
            reader.for_each_pipelined(|el| match el {
                pbfhogg::Element::Node(_) | pbfhogg::Element::DenseNode(_) => nodes += 1,
                pbfhogg::Element::Way(_) => ways += 1,
                pbfhogg::Element::Relation(_) => rels += 1,
                _ => {}
            })?;
            (start.elapsed().as_millis(), nodes, ways, rels)
        }
        "blobreader" => {
            use std::io::BufReader;
            let reader = pbfhogg::BlobReader::new(BufReader::new(std::fs::File::open(path)?));
            let mut nodes = 0u64;
            let mut ways = 0u64;
            let mut rels = 0u64;
            let start = Instant::now();
            for blob_result in reader {
                let blob = blob_result?;
                if let pbfhogg::BlobDecode::OsmData(block) = blob.decode()? {
                    for el in block.elements() {
                        match el {
                            pbfhogg::Element::Node(_) | pbfhogg::Element::DenseNode(_) => {
                                nodes += 1
                            }
                            pbfhogg::Element::Way(_) => ways += 1,
                            pbfhogg::Element::Relation(_) => rels += 1,
                            _ => {}
                        }
                    }
                }
            }
            (start.elapsed().as_millis(), nodes, ways, rels)
        }
        other => {
            return Err(format!(
                "unknown read mode: {other} (expected: sequential, parallel, pipelined, blobreader)"
            )
            .into())
        }
    };

    eprintln!("elapsed_ms={elapsed_ms}");
    eprintln!("nodes={nodes}");
    eprintln!("ways={ways}");
    eprintln!("relations={rels}");
    Ok(())
}

fn bench_write_loop<W: std::io::Write>(
    path: &std::path::Path,
    writer: &mut pbfhogg::writer::PbfWriter<W>,
) -> Result<(u64, u64, u64), Box<dyn std::error::Error>> {
    use pbfhogg::block_builder::{BlockBuilder, MemberData};
    use std::io::BufReader;

    let reader = pbfhogg::BlobReader::new(BufReader::new(std::fs::File::open(path)?));
    let mut bb = BlockBuilder::new();
    let mut nodes = 0u64;
    let mut ways = 0u64;
    let mut rels = 0u64;

    for blob_result in reader {
        let blob = blob_result?;
        if let pbfhogg::BlobDecode::OsmData(block) = blob.decode()? {
            for el in block.elements() {
                match el {
                    pbfhogg::Element::DenseNode(dn) => {
                        if !bb.can_add_node() {
                            if let Some(bytes) = bb.take()? {
                                writer.write_primitive_block(bytes)?;
                            }
                        }
                        let tags: Vec<_> = dn.tags().collect();
                        bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &tags, None);
                        nodes += 1;
                    }
                    pbfhogg::Element::Node(n) => {
                        if !bb.can_add_node() {
                            if let Some(bytes) = bb.take()? {
                                writer.write_primitive_block(bytes)?;
                            }
                        }
                        let tags: Vec<_> = n.tags().collect();
                        bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), &tags, None);
                        nodes += 1;
                    }
                    pbfhogg::Element::Way(w) => {
                        if !bb.can_add_way() {
                            if let Some(bytes) = bb.take()? {
                                writer.write_primitive_block(bytes)?;
                            }
                        }
                        let tags: Vec<_> = w.tags().collect();
                        let refs: Vec<_> = w.refs().collect();
                        bb.add_way(w.id(), &tags, &refs, None);
                        ways += 1;
                    }
                    pbfhogg::Element::Relation(r) => {
                        if !bb.can_add_relation() {
                            if let Some(bytes) = bb.take()? {
                                writer.write_primitive_block(bytes)?;
                            }
                        }
                        let tags: Vec<_> = r.tags().collect();
                        let members: Vec<_> = r
                            .members()
                            .map(|m| MemberData {
                                id: m.id,
                                role: m.role().ok().unwrap_or_default(),
                            })
                            .collect();
                        bb.add_relation(r.id(), &tags, &members, None);
                        rels += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(bytes)?;
    }

    Ok((nodes, ways, rels))
}

fn run_bench_write(
    path: &std::path::Path,
    compression: &str,
    writer_mode: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use pbfhogg::block_builder::HeaderBuilder;
    use std::time::Instant;

    let compression: Compression = compression.parse()?;
    let header_bytes = HeaderBuilder::new().build()?;

    let (elapsed_ms, nodes, ways, rels) = match writer_mode {
        "sync" => {
            let file = std::fs::File::create("/dev/null")?;
            let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
            let mut writer = pbfhogg::writer::PbfWriter::new(buf, compression);
            writer.write_header(&header_bytes)?;
            let start = Instant::now();
            let (nodes, ways, rels) = bench_write_loop(path, &mut writer)?;
            let elapsed_ms = start.elapsed().as_millis();
            (elapsed_ms, nodes, ways, rels)
        }
        "pipelined" => {
            let mut writer = pbfhogg::writer::PbfWriter::to_path(
                std::path::Path::new("/dev/null"),
                compression,
                &header_bytes,
            )?;
            let start = Instant::now();
            let (nodes, ways, rels) = bench_write_loop(path, &mut writer)?;
            drop(writer);
            let elapsed_ms = start.elapsed().as_millis();
            (elapsed_ms, nodes, ways, rels)
        }
        other => {
            return Err(format!("unknown writer mode: {other} (expected: sync, pipelined)").into());
        }
    };

    eprintln!("elapsed_ms={elapsed_ms}");
    eprintln!("nodes={nodes}");
    eprintln!("ways={ways}");
    eprintln!("relations={rels}");
    Ok(())
}

/// Read peak resident set size (VmHWM) from `/proc/self/status`.
/// Returns `None` on non-Linux platforms or if parsing fails.
#[cfg(target_os = "linux")]
fn read_peak_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let trimmed = rest.trim();
            let kb_str = trimmed.strip_suffix("kB").unwrap_or(trimmed).trim();
            return kb_str.parse::<u64>().ok();
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn read_peak_rss_kb() -> Option<u64> {
    None
}

#[allow(clippy::cast_precision_loss)]
fn run_bench_merge(
    base: &std::path::Path,
    changes: &std::path::Path,
    output: &std::path::Path,
    compression: &str,
    io_mode: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Instant;

    let compression: Compression = compression.parse()?;
    let io_uring = match io_mode {
        "buffered" => false,
        "uring" => true,
        other => {
            return Err(format!("unknown I/O mode: {other} (expected: buffered, uring)").into());
        }
    };

    let _ = std::fs::remove_file(output);

    let start = Instant::now();
    let opts = pbfhogg::merge::MergeOptions {
        compression,
        direct_io: false,
        io_uring,
        force: true,
    };
    let stats = pbfhogg::merge::merge(base, changes, output, &opts)?;
    let elapsed_ms = start.elapsed().as_millis();

    let output_mb = std::fs::metadata(output)
        .map(|m| m.len() as f64 / 1_000_000.0)
        .unwrap_or(0.0);

    eprintln!("elapsed_ms={elapsed_ms}");
    eprintln!("base_nodes={}", stats.base_nodes);
    eprintln!("base_ways={}", stats.base_ways);
    eprintln!("base_relations={}", stats.base_relations);
    eprintln!("diff_nodes={}", stats.diff_nodes);
    eprintln!("diff_ways={}", stats.diff_ways);
    eprintln!("diff_relations={}", stats.diff_relations);
    eprintln!("blobs_passthrough={}", stats.blobs_passthrough);
    eprintln!("blobs_rewritten={}", stats.blobs_rewritten);
    eprintln!("bytes_passthrough={}", stats.bytes_passthrough);
    eprintln!("bytes_rewritten={}", stats.bytes_rewritten);
    eprintln!("diff_heap_bytes={}", stats.diff_heap_bytes);
    eprintln!("output_mb={output_mb:.2}");
    if let Some(peak_kb) = read_peak_rss_kb() {
        eprintln!("peak_rss_kb={peak_kb}");
    }
    Ok(())
}
