#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc_crate::MiMalloc = mimalloc_crate::MiMalloc;

use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};

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
    },
    /// Validate referential integrity
    CheckRefs {
        /// Input PBF file
        file: PathBuf,
        /// Also check relation member references
        #[arg(long)]
        check_relations: bool,
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
    },
    /// Sort PBF into standard order (nodes → ways → relations, by ID)
    Sort {
        /// Input PBF file
        file: PathBuf,
        /// Output PBF file
        #[arg(short, long)]
        output: PathBuf,
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
    },
}

fn main() {
    let _guard = hotpath::HotpathGuardBuilder::new("pbfhogg::main")
        .percentiles(&[50, 95, 99])
        .build();

    let cli = Cli::parse();

    let result = match cli.command {
        Command::Fileinfo { file, extended } => run_fileinfo(&file, extended),
        Command::CheckRefs {
            file,
            check_relations,
        } => run_check_refs(&file, check_relations),
        Command::TagsCount {
            file,
            min_count,
            type_filter,
        } => run_tags_count(&file, min_count, type_filter.as_deref()),
        Command::Cat {
            files,
            output,
            type_filter,
        } => run_cat(&files, &output, type_filter.as_deref()),
        Command::Sort { file, output } => run_sort(&file, &output),
        Command::TagsFilter {
            file,
            output,
            omit_referenced,
            expressions,
        } => run_tags_filter(&file, &output, &expressions, omit_referenced),
        Command::Diff {
            old,
            new,
            suppress_common,
            verbose,
            type_filter,
        } => run_diff(&old, &new, suppress_common, verbose, type_filter.as_deref()),
        Command::DeriveChanges {
            old,
            new,
            output,
        } => run_derive_changes(&old, &new, &output),
        Command::Getid {
            file,
            output,
            add_referenced,
            id_file,
            ids,
        } => run_getid(&file, &output, add_referenced, id_file.as_deref(), &ids),
        Command::Removeid {
            file,
            output,
            id_file,
            ids,
        } => run_removeid(&file, &output, id_file.as_deref(), &ids),
        Command::Extract {
            file,
            output,
            bbox,
            polygon,
            simple,
        } => run_extract(&file, &output, bbox.as_deref(), polygon.as_deref(), simple),
        Command::AddLocationsToWays {
            file,
            output,
            keep_untagged_nodes,
        } => run_add_locations_to_ways(&file, &output, keep_untagged_nodes),
        Command::Merge {
            base,
            changes,
            output,
        } => run_merge(&base, &changes, &output),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn run_fileinfo(path: &std::path::Path, extended: bool) -> Result<(), Box<dyn std::error::Error>> {
    let info = pbfhogg::fileinfo::fileinfo(path, extended)?;

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
) -> Result<(), Box<dyn std::error::Error>> {
    let result = pbfhogg::check_refs::check_refs(path, check_relations)?;

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
) -> Result<(), Box<dyn std::error::Error>> {
    let results = pbfhogg::tags_count::tags_count(path, min_count, type_filter)?;

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
) -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<&std::path::Path> = files.iter().map(AsRef::as_ref).collect();
    let stats = pbfhogg::cat::cat(&paths, output, type_filter)?;
    stats.print_summary();
    Ok(())
}

fn run_sort(
    file: &std::path::Path,
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::sort::sort(file, output)?;
    stats.print_summary();
    Ok(())
}

fn run_tags_filter(
    file: &std::path::Path,
    output: &std::path::Path,
    expressions: &[String],
    omit_referenced: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::tags_filter::tags_filter(file, output, expressions, omit_referenced)?;
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
) -> Result<(), Box<dyn std::error::Error>> {
    let options = pbfhogg::diff::DiffOptions {
        suppress_common,
        verbose,
        type_filter: type_filter.map(String::from),
    };
    let mut stdout = std::io::stdout().lock();
    let stats = pbfhogg::diff::diff(old, new, &mut stdout, &options)?;
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
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::derive_changes::derive_changes(old, new, output)?;
    stats.print_summary();
    Ok(())
}

fn run_getid(
    file: &std::path::Path,
    output: &std::path::Path,
    add_referenced: bool,
    id_file: Option<&std::path::Path>,
    ids: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let id_set = resolve_ids(id_file, ids)?;
    let stats = pbfhogg::getid::getid(file, output, &id_set, add_referenced)?;
    stats.print_summary();
    Ok(())
}

fn run_removeid(
    file: &std::path::Path,
    output: &std::path::Path,
    id_file: Option<&std::path::Path>,
    ids: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let id_set = resolve_ids(id_file, ids)?;
    let stats = pbfhogg::getid::removeid(file, output, &id_set)?;
    stats.print_summary();
    Ok(())
}

fn run_extract(
    file: &std::path::Path,
    output: &std::path::Path,
    bbox_str: Option<&str>,
    polygon_path: Option<&std::path::Path>,
    simple: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let region = match (bbox_str, polygon_path) {
        (Some(s), None) => {
            let bbox = pbfhogg::extract::parse_bbox(s)?;
            pbfhogg::extract::Region::Bbox(bbox)
        }
        (None, Some(p)) => pbfhogg::extract::parse_geojson(p)?,
        (None, None) => return Err("one of --bbox or --polygon is required".into()),
        (Some(_), Some(_)) => return Err("--bbox and --polygon are mutually exclusive".into()),
    };
    let stats = pbfhogg::extract::extract(file, output, &region, simple)?;
    stats.print_summary();
    Ok(())
}

fn run_add_locations_to_ways(
    file: &std::path::Path,
    output: &std::path::Path,
    keep_untagged_nodes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats =
        pbfhogg::add_locations_to_ways::add_locations_to_ways(file, output, keep_untagged_nodes)?;
    stats.print_summary();
    Ok(())
}

fn run_merge(
    base: &std::path::Path,
    changes: &std::path::Path,
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::merge::merge(base, changes, output)?;
    stats.print_summary();
    Ok(())
}
