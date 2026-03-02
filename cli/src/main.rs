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

use clap::{Parser, Subcommand};
use pbfhogg::writer::Compression;

#[derive(Parser)]
#[command(name = "pbfhogg", about = "OpenStreetMap PBF toolkit")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Display PBF file metadata
    Fileinfo {
        /// Input PBF file
        file: PathBuf,
        /// Full scan: count blobs and elements
        #[arg(long)]
        extended: bool,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
    },
    /// Validate referential integrity
    CheckRefs {
        /// Input PBF file
        file: PathBuf,
        /// Also check relation member references
        #[arg(long)]
        check_relations: bool,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
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
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
        /// Proceed even if input lacks indexdata (slower: type filter becomes no-op)
        #[arg(long)]
        force: bool,
    },
    /// Concatenate PBF files with optional type filtering
    Cat {
        /// Input PBF files
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Output PBF file
        #[arg(short, long)]
        output: PathBuf,
        /// Filter by element type (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type")]
        type_filter: Option<String>,
        /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
        /// Proceed even if input lacks indexdata (slower: type filter becomes no-op)
        #[arg(long)]
        force: bool,
    },
    /// Sort PBF into standard order (nodes → ways → relations, by ID)
    Sort {
        /// Input PBF file
        file: PathBuf,
        /// Output PBF file
        #[arg(short, long)]
        output: PathBuf,
        /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
        /// Use io_uring with registered buffers (requires linux-io-uring feature)
        #[arg(long)]
        io_uring: bool,
        /// Enable SQ polling (requires --io-uring)
        #[arg(long, requires = "io_uring")]
        sqpoll: bool,
        /// Proceed even if input lacks indexdata (slower: full decode for ID scanning)
        #[arg(long)]
        force: bool,
    },
    /// Filter elements by tag expressions
    TagsFilter {
        /// Input PBF file
        file: PathBuf,
        /// Output PBF file
        #[arg(short, long)]
        output: PathBuf,
        /// Omit referenced objects (faster, single pass)
        #[arg(short = 'R', long = "omit-referenced")]
        omit_referenced: bool,
        /// Tag filter expressions (e.g. "highway=primary", "amenity", "w/building=yes")
        #[arg(required = true)]
        expressions: Vec<String>,
        /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
        /// Proceed even if input lacks indexdata (slower: type/tag filters become no-ops)
        #[arg(long)]
        force: bool,
    },
    /// Compare two PBF files and show differences
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
        /// Filter by element type (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type")]
        type_filter: Option<String>,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
    },
    /// Generate OSC diff from two PBF snapshots
    DeriveChanges {
        /// Old PBF file
        old: PathBuf,
        /// New PBF file
        new: PathBuf,
        /// Output OSC file (.osc.gz)
        #[arg(short, long)]
        output: PathBuf,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
    },
    /// Extract elements by ID
    Getid {
        /// Input PBF file
        file: PathBuf,
        /// Output PBF file
        #[arg(short, long)]
        output: PathBuf,
        /// Include referenced nodes of matching ways (two-pass)
        #[arg(short = 'r', long = "add-referenced")]
        add_referenced: bool,
        /// Read IDs from file instead of arguments
        #[arg(short = 'i', long = "id-file")]
        id_file: Option<PathBuf>,
        /// Element IDs (e.g. n123 w456 r789)
        ids: Vec<String>,
        /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
        /// Proceed even if input lacks indexdata (slower: type filter becomes no-op)
        #[arg(long)]
        force: bool,
    },
    /// Remove elements by ID
    Removeid {
        /// Input PBF file
        file: PathBuf,
        /// Output PBF file
        #[arg(short, long)]
        output: PathBuf,
        /// Read IDs from file instead of arguments
        #[arg(short = 'i', long = "id-file")]
        id_file: Option<PathBuf>,
        /// Element IDs (e.g. n123 w456 r789)
        ids: Vec<String>,
        /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
    },
    /// Extract elements within a geographic region (bbox or polygon)
    Extract {
        /// Input PBF file
        file: PathBuf,
        /// Output PBF file
        #[arg(short, long)]
        output: PathBuf,
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
        /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
        /// Proceed even without blob-level indexdata (slower for complete-ways/smart)
        #[arg(long)]
        force: bool,
    },
    /// Embed node coordinates in ways
    AddLocationsToWays {
        /// Input PBF file
        file: PathBuf,
        /// Output PBF file
        #[arg(short, long)]
        output: PathBuf,
        /// Keep untagged nodes in output (default: drop them)
        #[arg(long)]
        keep_untagged_nodes: bool,
        /// Node location index type: hash (default), dense (mmap, for planet-scale)
        #[arg(short = 'n', long, default_value = "hash")]
        index_type: String,
        /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
        /// Proceed even if input lacks indexdata (slower: full decode/re-encode)
        #[arg(long)]
        force: bool,
    },
    /// Analyze node coordinate statistics for FOR compression sizing
    NodeStats {
        /// Input PBF file
        file: PathBuf,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
        /// Proceed even if input lacks indexdata (slower: node filter becomes no-op)
        #[arg(long)]
        force: bool,
    },
    /// Apply OSC diffs to a PBF file
    Merge {
        /// Base PBF file
        base: PathBuf,
        /// OSC diff file (.osc.gz)
        changes: PathBuf,
        /// Output PBF file
        #[arg(short, long)]
        output: PathBuf,
        /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
        /// Use io_uring for output I/O (requires linux-io-uring feature)
        #[arg(long)]
        io_uring: bool,
        /// Use SQ polling for io_uring (eliminates io_uring_enter syscalls, requires --io-uring)
        #[arg(long, requires = "io_uring")]
        sqpoll: bool,
        /// Proceed even if base PBF lacks indexdata (slower: full decode for classification)
        #[arg(long)]
        force: bool,
    },
    /// Check if a PBF file has blob-level indexdata
    IsIndexed {
        /// Input PBF file
        file: PathBuf,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
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
        /// Output PBF file
        #[arg(short, long)]
        output: PathBuf,
        /// Compression spec (e.g. none, zlib, zstd:3)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// I/O mode: buffered, uring, uring-sqpoll
        #[arg(long, default_value = "buffered")]
        io_mode: String,
    },
}

fn main() {
    let _guard = hotpath::HotpathGuardBuilder::new("pbfhogg::main")
        .percentiles(&[50, 95, 99])
        .with_functions_limit(0)
        .build();

    let cli = Cli::parse();

    let result = match cli.command {
        Command::Fileinfo { file, extended, direct_io } => run_fileinfo(&file, extended, direct_io),
        Command::CheckRefs {
            file,
            check_relations,
            direct_io,
        } => run_check_refs(&file, check_relations, direct_io),
        Command::TagsCount {
            file,
            min_count,
            type_filter,
            direct_io,
            force,
        } => run_tags_count(&file, min_count, type_filter.as_deref(), direct_io, force),
        Command::Cat {
            files,
            output,
            type_filter,
            compression,
            direct_io,
            force,
        } => run_cat(&files, &output, type_filter.as_deref(), &compression, direct_io, force),
        Command::Sort { file, output, compression, direct_io, io_uring, sqpoll, force } => run_sort(&file, &output, &compression, direct_io, io_uring, sqpoll, force),
        Command::TagsFilter {
            file,
            output,
            omit_referenced,
            expressions,
            compression,
            direct_io,
            force,
        } => run_tags_filter(&file, &output, &expressions, omit_referenced, &compression, direct_io, force),
        Command::Diff {
            old,
            new,
            suppress_common,
            verbose,
            type_filter,
            direct_io,
        } => run_diff(&old, &new, suppress_common, verbose, type_filter.as_deref(), direct_io),
        Command::DeriveChanges {
            old,
            new,
            output,
            direct_io,
        } => run_derive_changes(&old, &new, &output, direct_io),
        Command::Getid {
            file,
            output,
            add_referenced,
            id_file,
            ids,
            compression,
            direct_io,
            force,
        } => run_getid(&file, &output, add_referenced, id_file.as_deref(), &ids, &compression, direct_io, force),
        Command::Removeid {
            file,
            output,
            id_file,
            ids,
            compression,
            direct_io,
        } => run_removeid(&file, &output, id_file.as_deref(), &ids, &compression, direct_io),
        Command::Extract {
            file,
            output,
            bbox,
            polygon,
            simple,
            smart,
            compression,
            direct_io,
            force,
        } => run_extract(
            &file, &output, bbox.as_deref(), polygon.as_deref(),
            extract_strategy(simple, smart), &compression, direct_io, force,
        ),
        Command::AddLocationsToWays {
            file,
            output,
            keep_untagged_nodes,
            index_type,
            compression,
            direct_io,
            force,
        } => run_add_locations_to_ways(&file, &output, keep_untagged_nodes, &index_type, &compression, direct_io, force),
        Command::NodeStats { file, direct_io, force } => run_node_stats(&file, direct_io, force),
        Command::Merge {
            base,
            changes,
            output,
            compression,
            direct_io,
            io_uring,
            sqpoll,
            force,
        } => run_merge(&base, &changes, &output, &compression, direct_io, io_uring, sqpoll, force),
        Command::IsIndexed { file, direct_io } => run_is_indexed(&file, direct_io),
        Command::BenchRead { file, mode } => run_bench_read(&file, &mode),
        Command::BenchWrite { file, compression, writer } => run_bench_write(&file, &compression, &writer),
        Command::BenchMerge { base, changes, output, compression, io_mode } => {
            run_bench_merge(&base, &changes, &output, &compression, &io_mode)
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn run_fileinfo(path: &std::path::Path, extended: bool, direct_io: bool) -> Result<(), Box<dyn std::error::Error>> {
    let info = pbfhogg::fileinfo::fileinfo(path, extended, direct_io)?;

    if let Some((left, bottom, right, top)) = info.bbox {
        println!("Bounding box: ({left}, {bottom}) - ({right}, {top})");
    }
    if let Some(ref prog) = info.writing_program {
        println!("Writing program: {prog}");
    }
    if !info.required_features.is_empty() {
        println!("Required features: {}", info.required_features.join(", "));
    }
    if !info.optional_features.is_empty() {
        println!("Optional features: {}", info.optional_features.join(", "));
    }
    if let Some(ts) = info.replication_timestamp {
        println!("Replication timestamp: {ts}");
    }
    if let Some(seq) = info.replication_sequence {
        println!("Replication sequence: {seq}");
    }
    if let Some(ref url) = info.replication_url {
        println!("Replication URL: {url}");
    }
    if let Some(indexed) = info.is_indexed {
        println!("Indexdata: {}", if indexed { "yes" } else { "no" });
    }

    if extended {
        println!();
        if let Some(n) = info.blob_count {
            println!("Data blobs: {n}");
        }
        if let Some(n) = info.node_count {
            println!("Nodes: {n}");
        }
        if let Some(n) = info.way_count {
            println!("Ways: {n}");
        }
        if let Some(n) = info.relation_count {
            println!("Relations: {n}");
        }
    }

    Ok(())
}

fn run_is_indexed(path: &std::path::Path, direct_io: bool) -> Result<(), Box<dyn std::error::Error>> {
    if pbfhogg::has_indexdata(path, direct_io)? {
        println!("indexed");
    } else {
        println!("not indexed");
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
            println!(
                "Missing way refs in relations: {}",
                result.missing_way_refs
            );
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

fn parse_compression(s: &str) -> Result<Compression, Box<dyn std::error::Error>> {
    match s {
        "none" => Ok(Compression::None),
        "zlib" => Ok(Compression::default()),
        "zstd" => Ok(Compression::Zstd(3)),
        _ if s.starts_with("zlib:") => {
            let level: u32 = s[5..].parse().map_err(|_| format!("invalid zlib level: {s}"))?;
            if level > 9 {
                return Err(format!("zlib level must be 0-9, got {level}").into());
            }
            Ok(Compression::Zlib(level))
        }
        _ if s.starts_with("zstd:") => {
            let level: i32 = s[5..].parse().map_err(|_| format!("invalid zstd level: {s}"))?;
            Ok(Compression::Zstd(level))
        }
        _ => Err(format!("unknown compression: {s} (expected none, zlib, zlib:LEVEL, zstd, zstd:LEVEL)").into()),
    }
}

fn run_cat(
    files: &[PathBuf],
    output: &std::path::Path,
    type_filter: Option<&str>,
    compression: &str,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
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
    sqpoll: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
    let opts = pbfhogg::sort::SortOptions { compression, direct_io, io_uring, sqpoll, force };
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
    let compression = parse_compression(compression)?;
    let stats = pbfhogg::tags_filter::tags_filter(file, output, expressions, omit_referenced, compression, direct_io, force)?;
    stats.print_summary();
    Ok(())
}

fn resolve_ids(
    id_file: Option<&std::path::Path>,
    ids: &[String],
) -> Result<pbfhogg::getid::IdSet, Box<dyn std::error::Error>> {
    match id_file {
        Some(path) => pbfhogg::getid::parse_ids_from_file(path),
        None => pbfhogg::getid::parse_ids(ids),
    }
}

fn run_diff(
    old: &std::path::Path,
    new: &std::path::Path,
    suppress_common: bool,
    verbose: bool,
    type_filter: Option<&str>,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let options = pbfhogg::diff::DiffOptions {
        suppress_common,
        verbose,
        type_filter: type_filter.map(String::from),
    };
    let mut stdout = std::io::stdout().lock();
    let stats = pbfhogg::diff::diff(old, new, &mut stdout, &options, direct_io)?;
    stats.print_summary();
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
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::derive_changes::derive_changes(old, new, output, direct_io)?;
    stats.print_summary();
    Ok(())
}

fn run_getid(
    file: &std::path::Path,
    output: &std::path::Path,
    add_referenced: bool,
    id_file: Option<&std::path::Path>,
    ids: &[String],
    compression: &str,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
    let id_set = resolve_ids(id_file, ids)?;
    let stats = pbfhogg::getid::getid(file, output, &id_set, add_referenced, compression, direct_io, force)?;
    stats.print_summary();
    Ok(())
}

fn run_removeid(
    file: &std::path::Path,
    output: &std::path::Path,
    id_file: Option<&std::path::Path>,
    ids: &[String],
    compression: &str,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
    let id_set = resolve_ids(id_file, ids)?;
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
    let compression = parse_compression(compression)?;
    let region = match (bbox_str, polygon_path) {
        (Some(s), None) => {
            let bbox = pbfhogg::extract::parse_bbox(s)?;
            pbfhogg::extract::Region::Bbox(bbox)
        }
        (None, Some(p)) => pbfhogg::extract::parse_geojson(p)?,
        (None, None) => return Err("one of --bbox or --polygon is required".into()),
        (Some(_), Some(_)) => return Err("--bbox and --polygon are mutually exclusive".into()),
    };
    let stats = pbfhogg::extract::extract(file, output, &region, strategy, compression, direct_io, force)?;
    stats.print_summary();
    Ok(())
}

fn run_add_locations_to_ways(
    file: &std::path::Path,
    output: &std::path::Path,
    keep_untagged_nodes: bool,
    index_type_str: &str,
    compression: &str,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
    let index_type = match index_type_str {
        "hash" => pbfhogg::add_locations_to_ways::IndexType::Hash,
        "dense" => pbfhogg::add_locations_to_ways::IndexType::Dense {
            capacity: pbfhogg::add_locations_to_ways::DENSE_INDEX_DEFAULT_CAPACITY,
        },
        other => return Err(format!("unknown index type: {other} (expected: hash, dense)").into()),
    };
    let stats = pbfhogg::add_locations_to_ways::add_locations_to_ways(
        file, output, keep_untagged_nodes, compression, direct_io, index_type, force,
    )?;
    stats.print_summary();
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
    sqpoll: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
    let opts = pbfhogg::merge::MergeOptions { compression, direct_io, io_uring, sqpoll, force };
    let stats = pbfhogg::merge::merge(base, changes, output, &opts)?;
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
                    pbfhogg::Element::Node(_) | pbfhogg::Element::DenseNode(_) => (1u64, 0u64, 0u64),
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
                            pbfhogg::Element::Node(_) | pbfhogg::Element::DenseNode(_) => nodes += 1,
                            pbfhogg::Element::Way(_) => ways += 1,
                            pbfhogg::Element::Relation(_) => rels += 1,
                            _ => {}
                        }
                    }
                }
            }
            (start.elapsed().as_millis(), nodes, ways, rels)
        }
        other => return Err(format!("unknown read mode: {other} (expected: sequential, parallel, pipelined, blobreader)").into()),
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
    use std::io::BufReader;
    use pbfhogg::block_builder::{BlockBuilder, MemberData};

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
                        let members: Vec<_> = r.members().map(|m| MemberData {
                            id: m.id,
                            role: m.role().ok().unwrap_or_default(),
                        }).collect();
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
    use std::time::Instant;
    use pbfhogg::block_builder::HeaderBuilder;

    let compression = parse_compression(compression)?;
    let header_bytes = HeaderBuilder::new().build()?;

    let (elapsed_ms, nodes, ways, rels) = match writer_mode {
        "sync" => {
            let mut writer = pbfhogg::writer::PbfWriter::to_path(
                std::path::Path::new("/dev/null"),
                compression,
            )?;
            writer.write_header(&header_bytes)?;
            let start = Instant::now();
            let (nodes, ways, rels) = bench_write_loop(path, &mut writer)?;
            let elapsed_ms = start.elapsed().as_millis();
            (elapsed_ms, nodes, ways, rels)
        }
        "pipelined" => {
            let mut writer = pbfhogg::writer::PbfWriter::to_path_pipelined(
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
        other => return Err(format!("unknown writer mode: {other} (expected: sync, pipelined)").into()),
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

    let compression = parse_compression(compression)?;
    let (io_uring, sqpoll) = match io_mode {
        "buffered" => (false, false),
        "uring" => (true, false),
        "uring-sqpoll" => (true, true),
        other => return Err(format!("unknown I/O mode: {other} (expected: buffered, uring, uring-sqpoll)").into()),
    };

    let _ = std::fs::remove_file(output);

    let start = Instant::now();
    let opts = pbfhogg::merge::MergeOptions { compression, direct_io: false, io_uring, sqpoll, force: true };
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
