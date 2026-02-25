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
        #[arg(short = 's', long)]
        simple: bool,
        /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
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
        /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
        #[arg(long, default_value = "zlib")]
        compression: String,
        /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
        #[arg(long)]
        direct_io: bool,
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
    },
}

fn main() {
    let _guard = hotpath::HotpathGuardBuilder::new("pbfhogg::main")
        .percentiles(&[50, 95, 99])
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
        } => run_tags_count(&file, min_count, type_filter.as_deref(), direct_io),
        Command::Cat {
            files,
            output,
            type_filter,
            compression,
            direct_io,
        } => run_cat(&files, &output, type_filter.as_deref(), &compression, direct_io),
        Command::Sort { file, output, compression, direct_io } => run_sort(&file, &output, &compression, direct_io),
        Command::TagsFilter {
            file,
            output,
            omit_referenced,
            expressions,
            compression,
            direct_io,
        } => run_tags_filter(&file, &output, &expressions, omit_referenced, &compression, direct_io),
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
        } => run_getid(&file, &output, add_referenced, id_file.as_deref(), &ids, &compression, direct_io),
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
            compression,
            direct_io,
        } => run_extract(&file, &output, bbox.as_deref(), polygon.as_deref(), simple, &compression, direct_io),
        Command::AddLocationsToWays {
            file,
            output,
            keep_untagged_nodes,
            compression,
            direct_io,
        } => run_add_locations_to_ways(&file, &output, keep_untagged_nodes, &compression, direct_io),
        Command::Merge {
            base,
            changes,
            output,
            compression,
            direct_io,
        } => run_merge(&base, &changes, &output, &compression, direct_io),
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
) -> Result<(), Box<dyn std::error::Error>> {
    let results = pbfhogg::tags_count::tags_count(path, min_count, type_filter, direct_io)?;

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
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
    let paths: Vec<&std::path::Path> = files.iter().map(AsRef::as_ref).collect();
    let stats = pbfhogg::cat::cat(&paths, output, type_filter, compression, direct_io)?;
    stats.print_summary();
    Ok(())
}

fn run_sort(
    file: &std::path::Path,
    output: &std::path::Path,
    compression: &str,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
    let stats = pbfhogg::sort::sort(file, output, compression, direct_io)?;
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
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
    let stats = pbfhogg::tags_filter::tags_filter(file, output, expressions, omit_referenced, compression, direct_io)?;
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
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
    let id_set = resolve_ids(id_file, ids)?;
    let stats = pbfhogg::getid::getid(file, output, &id_set, add_referenced, compression, direct_io)?;
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

fn run_extract(
    file: &std::path::Path,
    output: &std::path::Path,
    bbox_str: Option<&str>,
    polygon_path: Option<&std::path::Path>,
    simple: bool,
    compression: &str,
    direct_io: bool,
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
    let stats = pbfhogg::extract::extract(file, output, &region, simple, compression, direct_io)?;
    stats.print_summary();
    Ok(())
}

fn run_add_locations_to_ways(
    file: &std::path::Path,
    output: &std::path::Path,
    keep_untagged_nodes: bool,
    compression: &str,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
    let stats =
        pbfhogg::add_locations_to_ways::add_locations_to_ways(file, output, keep_untagged_nodes, compression, direct_io)?;
    stats.print_summary();
    Ok(())
}

fn run_merge(
    base: &std::path::Path,
    changes: &std::path::Path,
    output: &std::path::Path,
    compression: &str,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression = parse_compression(compression)?;
    let stats = pbfhogg::merge::merge(base, changes, output, compression, direct_io)?;
    stats.print_summary();
    Ok(())
}
