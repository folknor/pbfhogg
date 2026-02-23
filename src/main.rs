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

fn run_merge(
    base: &std::path::Path,
    changes: &std::path::Path,
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::merge::merge(base, changes, output)?;
    stats.print_summary();
    Ok(())
}
