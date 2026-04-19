use std::path::PathBuf;
use std::process;

use clap::{Args, Parser, Subcommand, ValueEnum};
use pbfhogg::HeaderOverrides;
use pbfhogg::writer::Compression;

#[derive(Parser)]
#[command(name = "pbfhogg", about = "OpenStreetMap PBF toolkit", version)]
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

#[derive(Args)]
struct HeaderOverrideArg {
    /// Override the writing program name in the output header
    #[arg(long)]
    generator: Option<String>,
    /// Set output header fields (repeatable, format: key=value).
    /// Supported keys: osmosis_replication_timestamp, osmosis_replication_sequence_number,
    /// osmosis_replication_base_url
    #[arg(long = "output-header", value_name = "KEY=VALUE")]
    output_headers: Vec<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum DiffFormat {
    Text,
    Osc,
}

#[derive(Clone, Copy, ValueEnum)]
enum InputKind {
    Pbf,
    Osc,
}

#[derive(Clone, Copy, ValueEnum)]
enum DefaultTypeArg {
    Node,
    Way,
    Relation,
}


#[derive(Subcommand)]
enum Command {
    /// Concatenate PBF files with optional type filtering
    Cat {
        /// Input PBF files
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[command(flatten)]
        output: OutputArg,
        /// Strip metadata attribute (version, timestamp, changeset, uid, user).
        /// Can be specified multiple times, e.g. -c changeset -c uid -c user
        #[arg(short = 'c', long = "clean", value_name = "ATTR")]
        clean: Vec<String>,
        /// Filter by element type (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type")]
        type_filter: Option<String>,
        /// Sorted k-way merge with dedup by (type, id). Requires sorted inputs.
        #[arg(long)]
        dedupe: bool,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        uring: UringArg,
        #[command(flatten)]
        force: ForceArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
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
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Renumber all element IDs sequentially, remapping cross-references.
    ///
    /// Input must be sorted. Negative IDs (JOSM editor-local staging
    /// identifiers) are rejected - they must be resolved before renumbering.
    Renumber {
        /// Input PBF file (must be sorted)
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Starting ID(s): single value or comma-separated node,way,relation
        #[arg(short = 's', long, default_value = "1")]
        start_id: String,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Filter elements by tag expressions.
    ///
    /// Default mode (without `-R`) resolves relation members transitively:
    /// matched relations pull in member ways, member nodes, nested member
    /// relations, and node refs of included ways.
    ///
    /// With `--input-kind osc`, filters an OSC change file instead, always
    /// preserving deletes. PBF-only flags (`-R`, `-i`, `-t`) are not valid
    /// in OSC mode.
    TagsFilter {
        /// Input file (PBF or OSC)
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Input kind override: pbf or osc (autodetect from extension by default)
        #[arg(long = "input-kind")]
        input_kind: Option<InputKind>,
        /// Omit referenced objects (faster, single pass, direct matches only)
        #[arg(short = 'R', long = "omit-referenced")]
        omit_referenced: bool,
        /// Invert match: exclude matching objects, keep non-matching
        #[arg(short = 'i', long = "invert-match")]
        invert_match: bool,
        /// Remove tags from referenced objects not directly matched (use without -R)
        #[arg(short = 't', long = "remove-tags")]
        remove_tags: bool,
        /// Read filter expressions from file (one per line, # comments)
        #[arg(short = 'e', long = "expressions")]
        expressions_file: Option<PathBuf>,
        /// Tag filter expressions (e.g. "highway=primary", "amenity", "w/building=yes")
        expressions: Vec<String>,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Compare two PBF files and show differences.
    ///
    /// Uses content equality (coordinates, tags, refs, members) - not version/timestamp
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
        /// Show summary on stderr (left/right/same/different counts)
        #[arg(short = 's', long)]
        summary: bool,
        /// Exit-code only, suppress diff output and summary
        #[arg(short = 'q', long, conflicts_with = "output")]
        quiet: bool,
        /// Write diff output to file instead of stdout
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,
        /// Filter by element type (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type")]
        type_filter: Option<String>,
        /// Output format: text (default) or osc
        #[arg(long, default_value = "text")]
        format: DiffFormat,
        /// Bump version of deleted elements by 1 (--format osc only)
        #[arg(long)]
        increment_version: bool,
        /// Set delete timestamp to current time (--format osc only)
        #[arg(long)]
        update_timestamp: bool,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Extract or remove elements by ID
    ///
    /// By default, keeps only the listed IDs. With `--invert`, removes the
    /// listed IDs and keeps everything else.
    Getid {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Invert selection: remove listed IDs instead of keeping them
        #[arg(long)]
        invert: bool,
        /// Include referenced nodes of matching ways (two-pass)
        #[arg(short = 'r', long = "add-referenced", conflicts_with = "invert")]
        add_referenced: bool,
        /// Remove tags from referenced objects not explicitly requested (use with -r)
        #[arg(short = 't', long = "remove-tags", conflicts_with = "invert")]
        remove_tags: bool,
        /// Print requested IDs and report which were not found
        #[arg(long, conflicts_with = "invert")]
        verbose_ids: bool,
        /// Read IDs from text file (one per line, e.g. n123)
        #[arg(short = 'i', long = "id-file")]
        id_file: Option<PathBuf>,
        /// Read IDs from an OSM/PBF file (all element IDs are collected)
        #[arg(short = 'I', long = "id-osm-file")]
        id_osm_file: Option<PathBuf>,
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
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Find ways/relations referencing given IDs (reverse lookup)
    Getparents {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Also include the queried objects themselves in the output
        #[arg(short = 's', long = "add-self")]
        add_self: bool,
        /// Read IDs from text file (one per line, e.g. n123)
        #[arg(short = 'i', long = "id-file")]
        id_file: Option<PathBuf>,
        /// Read IDs from an OSM/PBF file (all element IDs are collected)
        #[arg(short = 'I', long = "id-osm-file")]
        id_osm_file: Option<PathBuf>,
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
        header: HeaderOverrideArg,
    },
    /// Extract elements within a geographic region (bbox or polygon)
    Extract {
        /// Input PBF file
        file: PathBuf,
        /// Output file (required for single extract, omit with --config)
        #[arg(short, long, required_unless_present = "config")]
        output: Option<PathBuf>,
        /// Bounding box: minlon,minlat,maxlon,maxlat
        #[arg(short = 'b', long, group = "area", conflicts_with = "config")]
        bbox: Option<String>,
        /// Polygon GeoJSON file
        #[arg(short = 'p', long, group = "area", conflicts_with = "config")]
        polygon: Option<PathBuf>,
        /// Multi-extract JSON config file
        #[arg(short = 'c', long, conflicts_with_all = ["bbox", "polygon", "output"])]
        config: Option<PathBuf>,
        /// Output directory override (only with --config)
        #[arg(short = 'd', long, requires = "config")]
        directory: Option<PathBuf>,
        /// Simple strategy (single pass, may have dangling refs)
        #[arg(short = 's', long, conflicts_with = "smart")]
        simple: bool,
        /// Smart strategy (three passes, complete multipolygon/boundary relations)
        #[arg(long, conflicts_with = "simple")]
        smart: bool,
        /// Write the extract region bounding box to the output header
        #[arg(long)]
        set_bounds: bool,
        /// Strip metadata attribute (version, timestamp, changeset, uid, user).
        /// Can be specified multiple times, e.g. --clean changeset --clean uid
        #[arg(long = "clean", value_name = "ATTR")]
        clean: Vec<String>,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
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
        /// Node coordinate index type:
        ///   auto     - external if sorted + indexed, dense otherwise (recommended).
        ///   dense    - direct-mapped mmap array. Fastest when working set fits in RAM.
        ///              Works on any PBF (sorted or unsorted).
        ///   sparse   - chunk-indexed sparse array. Bounded memory (~540 MB), slower.
        ///              Works on any PBF. No temp disk needed.
        ///   external - double radix permutation. Bounded memory (~17 GB planet),
        ///              all sequential I/O. 3.9x faster than dense at planet scale.
        ///              Requires sorted PBF and indexdata. ~300 GB temp disk at planet.
        #[arg(long, default_value = "dense")]
        index_type: String,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
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
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Inspect PBF file: metadata, block breakdown, ordering analysis
    #[command(subcommand_negates_reqs = true)]
    Inspect {
        #[command(subcommand)]
        subcommand: Option<InspectCommand>,
        /// Input PBF file
        #[arg(required_unless_present = "subcommand")]
        file: Option<PathBuf>,
        /// Check if PBF has blob-level indexdata (exit code 0/1)
        #[arg(long)]
        indexed: bool,
        /// Analyze node coordinate statistics for FOR compression sizing
        #[arg(long)]
        nodes: bool,
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
        /// Extended scan: timestamp range, data bbox, metadata coverage, ordering
        #[arg(short, long)]
        extended: bool,
        /// Show a single element by type/ID (e.g. "n123", "w456", "r789")
        #[arg(long, value_name = "TYPE_ID")]
        show: Option<String>,
        /// Get a single value by key path (e.g. "header.bbox", "data.timestamp.first")
        #[arg(short, long, value_name = "KEY")]
        get: Option<String>,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
    /// Apply OSC diffs to a PBF file
    ApplyChanges {
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
        /// Preserve and update way-node locations through the merge.
        /// Requires the base PBF to have LocationsOnWays and be sorted.
        #[arg(long)]
        locations_on_ways: bool,
        #[command(flatten)]
        header: HeaderOverrideArg,
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
    /// Validate PBF file integrity (IDs + referential integrity)
    Check {
        /// Input PBF file
        file: PathBuf,
        /// Check ID uniqueness and ordering
        #[arg(long)]
        ids: bool,
        /// Check referential integrity
        #[arg(long)]
        refs: bool,
        /// Also check relation member references (use with --refs)
        #[arg(long)]
        check_relations: bool,
        /// Show IDs of missing objects (use with --refs)
        #[arg(long)]
        show_ids: bool,
        /// Full duplicate detection via bitmap (use with --ids)
        #[arg(long)]
        full: bool,
        /// Filter by element type for ID check (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type")]
        type_filter: Option<String>,
        /// Stop after N violations per check (0 = unlimited)
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
    /// Build a reverse geocoding index from a PBF file
    BuildGeocodeIndex {
        /// Input PBF file
        file: PathBuf,
        /// Output directory for index files
        #[arg(long)]
        output_dir: PathBuf,
        /// S2 cell level for streets/addresses
        #[arg(long, default_value = "17")]
        street_level: u8,
        /// Fallback cell level for rural areas
        #[arg(long, default_value = "14")]
        coarse_level: u8,
        /// S2 cell level for admin boundaries
        #[arg(long, default_value = "10")]
        admin_level: u8,
        /// Douglas-Peucker vertex cap per admin polygon
        #[arg(long, default_value = "500")]
        max_admin_vertices: u16,
        /// Fine-level max search distance in meters
        #[arg(long, default_value = "75")]
        search_radius: f32,
        /// Coarse-level max search distance in meters
        #[arg(long, default_value = "1000")]
        coarse_search_radius: f32,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
}


#[derive(Subcommand)]
enum InspectCommand {
    /// Count tag key=value frequencies
    Tags {
        /// Input PBF file
        file: PathBuf,
        /// Only show tags with at least this many occurrences
        #[arg(long, default_value = "1")]
        min_count: u64,
        /// Only show tags with at most this many occurrences
        #[arg(short = 'M', long)]
        max_count: Option<u64>,
        /// Sort order: count-desc (default), count-asc, name-asc, name-desc
        #[arg(short = 's', long, default_value = "count-desc")]
        sort: String,
        /// Read tag expressions from file (one per line, # comments)
        #[arg(short = 'e', long = "expressions")]
        expressions_file: Option<PathBuf>,
        /// Tag filter expressions (e.g. "highway", "amenity", "w/building=yes")
        expressions: Vec<String>,
        /// Filter by element type: node, way, or relation
        #[arg(short = 't', long = "type")]
        type_filter: Option<String>,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
}

fn detect_input_kind(path: &std::path::Path) -> InputKind {
    // Content sniffing: read first bytes to distinguish PBF from OSC
    if let Ok(kind) = sniff_input_kind(path) {
        return kind;
    }
    // Fall back to extension
    let name = path.to_string_lossy();
    if name.ends_with(".osc") || name.ends_with(".osc.gz") || name.ends_with(".osc.bz2") {
        InputKind::Osc
    } else {
        InputKind::Pbf
    }
}

fn sniff_input_kind(path: &std::path::Path) -> std::io::Result<InputKind> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = [0u8; 4];
    match file.read_exact(&mut buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(std::io::Error::other("too small"));
        }
        Err(e) => return Err(e),
    }
    // Gzip magic (1f 8b) - PBF is never gzip-wrapped, so this is OSC (.osc.gz)
    if buf[0] == 0x1f && buf[1] == 0x8b {
        return Ok(InputKind::Osc);
    }
    // XML starts with '<' - this is OSC (.osc)
    if buf[0] == b'<' {
        return Ok(InputKind::Osc);
    }
    // PBF: first 4 bytes are big-endian u32 blob header size (typically 13-50)
    let size = u32::from_be_bytes(buf);
    if size > 0 && size < 1000 {
        return Ok(InputKind::Pbf);
    }
    Err(std::io::Error::other("ambiguous"))
}

/// Combine CLI positional expressions with expressions read from a file.
/// CLI expressions come first, then file expressions (additive, matching osmium).
fn combine_expressions(
    file: Option<&std::path::Path>,
    cli_args: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut all = cli_args.to_vec();
    if let Some(path) = file {
        let from_file = pbfhogg::tag_expr::read_expressions_file(path)?;
        all.extend(from_file);
    }
    Ok(all)
}

#[allow(clippy::too_many_lines)]
fn main() {
    let _guard = hotpath::HotpathGuardBuilder::new("pbfhogg::main")
        .percentiles(&[50.0, 95.0, 99.0])
        .functions_limit(0)
        .build();

    let cli = Cli::parse();

    let result = (|| -> Result<(), Box<dyn std::error::Error>> { match cli.command {
        Command::Cat {
            files,
            output,
            clean,
            type_filter,
            dedupe,
            compression,
            io,
            uring,
            force,
            header,
        } => {
            let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
            if dedupe {
                if type_filter.is_some() {
                    return Err("--type is not valid with --dedupe".into());
                }
                if !clean.is_empty() {
                    return Err("--clean is not valid with --dedupe".into());
                }
                run_merge_pbf(
                    &files,
                    &output.output,
                    &compression.compression,
                    io.direct_io,
                    uring.io_uring,
                    force.force,
                    &overrides,
                )
            } else {
                if uring.io_uring {
                    return Err("--io-uring is only valid with --dedupe".into());
                }
                run_cat(
                    &files,
                    &output.output,
                    type_filter.as_deref(),
                    &clean,
                    &compression.compression,
                    io.direct_io,
                    force.force,
                    &overrides,
                )
            }
        }
        Command::Sort {
            file,
            output,
            compression,
            io,
            uring,
            force,
            header,
        } => run_sort(
            &file,
            &output.output,
            &compression.compression,
            io.direct_io,
            uring.io_uring,
            force.force,
            &HeaderOverrides::parse(header.generator, &header.output_headers)?,
        ),
        Command::Renumber {
            file,
            output,
            start_id,
            compression,
            io,
            header,
        } => run_renumber(
            &file,
            &output.output,
            &start_id,
            &compression.compression,
            io.direct_io,
            &HeaderOverrides::parse(header.generator, &header.output_headers)?,
        ),
        Command::TagsFilter {
            file,
            output,
            input_kind,
            omit_referenced,
            invert_match,
            remove_tags,
            expressions_file,
            expressions,
            compression,
            force,
            io,
            header,
        } => {
            let kind = input_kind.unwrap_or_else(|| detect_input_kind(&file));
            match kind {
                InputKind::Osc => {
                    if omit_referenced {
                        return Err("-R/--omit-referenced is not valid in OSC mode".into());
                    }
                    if invert_match {
                        return Err("-i/--invert-match is not valid in OSC mode".into());
                    }
                    if remove_tags {
                        return Err("-t/--remove-tags is not valid in OSC mode".into());
                    }
                    run_tags_filter_osc(&file, &output.output, expressions_file.as_deref(), &expressions)
                }
                InputKind::Pbf => run_tags_filter(
                    &file,
                    &output.output,
                    expressions_file.as_deref(),
                    &expressions,
                    omit_referenced,
                    invert_match,
                    remove_tags,
                    &compression.compression,
                    io.direct_io,
                    force.force,
                    &HeaderOverrides::parse(header.generator, &header.output_headers)?,
                ),
            }
        }
        Command::Diff {
            old,
            new,
            suppress_common,
            verbose,
            summary,
            quiet,
            output,
            type_filter,
            format,
            increment_version,
            update_timestamp,
            io,
        } => match format {
            DiffFormat::Osc => {
                let output = output.ok_or("--output is required with --format osc")?;
                if suppress_common {
                    return Err("--suppress-common is not valid with --format osc".into());
                }
                if verbose {
                    return Err("--verbose is not valid with --format osc".into());
                }
                if summary {
                    return Err("--summary is not valid with --format osc".into());
                }
                if quiet {
                    return Err("--quiet is not valid with --format osc".into());
                }
                if type_filter.is_some() {
                    return Err("--type is not valid with --format osc".into());
                }
                run_derive_changes(&old, &new, &output, io.direct_io, increment_version, update_timestamp)
            }
            DiffFormat::Text => {
                if increment_version {
                    return Err("--increment-version is only valid with --format osc".into());
                }
                if update_timestamp {
                    return Err("--update-timestamp is only valid with --format osc".into());
                }
                run_diff(
                    &old,
                    &new,
                    suppress_common,
                    verbose,
                    summary,
                    quiet,
                    output.as_deref(),
                    type_filter.as_deref(),
                    io.direct_io,
                )
            }
        },
        Command::Getid {
            file,
            output,
            invert,
            add_referenced,
            remove_tags,
            verbose_ids,
            id_file,
            id_osm_file,
            default_type,
            ids,
            compression,
            force,
            io,
            header,
        } => {
            if invert {
                if force.force {
                    eprintln!("Warning: --force has no effect with --invert (removeid does not use indexdata)");
                }
                run_removeid(
                    &file,
                    &output.output,
                    id_file.as_deref(),
                    id_osm_file.as_deref(),
                    default_type,
                    &ids,
                    &compression.compression,
                    io.direct_io,
                    &HeaderOverrides::parse(header.generator, &header.output_headers)?,
                )
            } else {
                run_getid(
                    &file,
                    &output.output,
                    add_referenced,
                    remove_tags,
                    verbose_ids,
                    id_file.as_deref(),
                    id_osm_file.as_deref(),
                    default_type,
                    &ids,
                    &compression.compression,
                    io.direct_io,
                    force.force,
                    &HeaderOverrides::parse(header.generator, &header.output_headers)?,
                )
            }
        }
        Command::Getparents {
            file,
            output,
            add_self,
            id_file,
            id_osm_file,
            default_type,
            ids,
            compression,
            io,
            header,
        } => run_getparents(
            &file,
            &output.output,
            add_self,
            id_file.as_deref(),
            id_osm_file.as_deref(),
            default_type,
            &ids,
            &compression.compression,
            io.direct_io,
            &HeaderOverrides::parse(header.generator, &header.output_headers)?,
        ),
        Command::Extract {
            file,
            output,
            bbox,
            polygon,
            config,
            directory,
            simple,
            smart,
            set_bounds,
            clean,
            compression,
            force,
            io,
            header,
        } => {
            let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
            if let Some(config_path) = config.as_deref() {
                run_extract_config(
                    &file,
                    config_path,
                    directory.as_deref(),
                    extract_strategy(simple, smart),
                    set_bounds,
                    &clean,
                    &compression.compression,
                    io.direct_io,
                    force.force,
                    &overrides,
                )
            } else if let Some(output) = output.as_ref() {
                run_extract(
                    &file,
                    output,
                    bbox.as_deref(),
                    polygon.as_deref(),
                    extract_strategy(simple, smart),
                    set_bounds,
                    &clean,
                    &compression.compression,
                    io.direct_io,
                    force.force,
                    &overrides,
                )
            } else {
                Err("--output is required without --config".into())
            }
        }
        Command::AddLocationsToWays {
            file,
            output,
            keep_untagged_nodes,
            index_type,
            compression,
            force,
            io,
            header,
        } => run_add_locations_to_ways(
            &file,
            &output.output,
            keep_untagged_nodes,
            &index_type,
            &compression.compression,
            io.direct_io,
            force.force,
            &HeaderOverrides::parse(header.generator, &header.output_headers)?,
        ),
        Command::TimeFilter {
            file,
            output,
            timestamp,
            compression,
            io,
            header,
        } => run_time_filter(
            &file,
            &output.output,
            &timestamp,
            &compression.compression,
            io.direct_io,
            &HeaderOverrides::parse(header.generator, &header.output_headers)?,
        ),
        Command::Inspect {
            subcommand,
            file,
            indexed,
            nodes,
            blocks,
            id_ranges,
            locations,
            anomalies,
            extended,
            show,
            get,
            json,
            io,
            force,
        } => {
            if let Some(InspectCommand::Tags {
                file: tags_file,
                min_count,
                max_count,
                sort,
                expressions_file,
                expressions,
                type_filter,
                io: tags_io,
                force: tags_force,
            }) = subcommand
            {
                run_tags_count(
                    &tags_file,
                    min_count,
                    max_count,
                    &sort,
                    expressions_file.as_deref(),
                    &expressions,
                    type_filter.as_deref(),
                    tags_io.direct_io,
                    tags_force.force,
                )
            } else {
                let file = file.ok_or("Input PBF file is required")?;
                if let Some(ref show_id) = show {
                    return run_show_element(&file, show_id, io.direct_io);
                }
                run_inspect(
                    &file,
                    indexed,
                    nodes,
                    blocks,
                    id_ranges,
                    locations,
                    anomalies,
                    extended,
                    get.as_deref(),
                    json,
                    io.direct_io,
                    force.force,
                )
            }
        }
        Command::ApplyChanges {
            base,
            changes,
            output,
            compression,
            force,
            io,
            uring,
            locations_on_ways,
            header,
        } => run_apply_changes(
            &base,
            &changes,
            &output.output,
            &compression.compression,
            io.direct_io,
            uring.io_uring,
            force.force,
            locations_on_ways,
            &HeaderOverrides::parse(header.generator, &header.output_headers)?,
        ),
        Command::MergeChanges {
            changes,
            output,
            simplify,
        } => run_merge_changes(&changes, &output.output, simplify),
        Command::Check {
            file,
            ids,
            refs,
            check_relations,
            show_ids,
            full,
            type_filter,
            max_errors,
            json,
            quiet,
            io,
        } => run_check(
            &file,
            ids,
            refs,
            check_relations,
            show_ids,
            full,
            type_filter.as_deref(),
            max_errors,
            json,
            quiet,
            io.direct_io,
        ),
        Command::BuildGeocodeIndex {
            file,
            output_dir,
            street_level,
            coarse_level,
            admin_level,
            max_admin_vertices,
            search_radius,
            coarse_search_radius,
            io,
            force,
        } => run_build_geocode_index(
            &file,
            &output_dir,
            street_level,
            coarse_level,
            admin_level,
            max_admin_vertices,
            search_radius,
            coarse_search_radius,
            io.direct_io,
            force.force,
        ),
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
    } })();

    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_build_geocode_index(
    file: &std::path::Path,
    output_dir: &std::path::Path,
    street_level: u8,
    coarse_level: u8,
    admin_level: u8,
    max_admin_vertices: u16,
    search_radius: f32,
    coarse_search_radius: f32,
    direct_io: bool,
    force: bool,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let config = pbfhogg::geocode_index::builder::BuildConfig {
        input_path: file.to_path_buf(),
        output_dir: output_dir.to_path_buf(),
        force,
        direct_io,
        street_level,
        coarse_level,
        admin_level,
        max_admin_vertices,
        fine_search_radius_m: search_radius,
        coarse_search_radius_m: coarse_search_radius,
    };
    let stats = pbfhogg::geocode_index::builder::build_geocode_index(&config)?;
    eprintln!(
        "Index built: {} addr, {} streets, {} interp, {} admin, {} fine cells, {} coarse cells, {} admin cells",
        stats.addr_points, stats.street_ways, stats.interp_ways, stats.admin_polygons,
        stats.fine_cells, stats.coarse_cells, stats.admin_cells,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::cognitive_complexity)]
fn run_check(
    path: &std::path::Path,
    ids: bool,
    refs: bool,
    check_relations: bool,
    show_ids: bool,
    full: bool,
    type_filter: Option<&str>,
    max_errors: usize,
    json: bool,
    quiet: bool,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Default: run both checks when neither --ids nor --refs specified
    let run_ids = ids || !refs;
    let run_refs = refs || !ids;

    // Validate flags that only apply to one check mode
    if !run_ids {
        if full {
            return Err("--full requires --ids (or omit --refs to run both checks)".into());
        }
        if type_filter.is_some() {
            return Err("--type requires --ids (or omit --refs to run both checks)".into());
        }
    }
    if !run_refs {
        if check_relations {
            return Err("--check-relations requires --refs (or omit --ids to run both checks)".into());
        }
        if show_ids {
            return Err("--show-ids requires --refs (or omit --ids to run both checks)".into());
        }
    }

    let mut failed = false;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("input.osm.pbf");

    // Run checks and collect results
    let ids_report = if run_ids {
        use pbfhogg::verify_ids::VerifyIdsOptions;
        let opts = VerifyIdsOptions {
            full,
            type_filter,
            max_errors,
            direct_io,
        };
        let report = pbfhogg::verify_ids::verify_ids(path, &opts)?;
        if !report.passed {
            failed = true;
        }
        Some(report)
    } else {
        None
    };

    let refs_result = if run_refs {
        let result = pbfhogg::check_refs::check_refs(path, check_relations, show_ids, direct_io)?;
        if !result.is_valid() {
            failed = true;
        }
        Some(result)
    } else {
        None
    };

    // Output
    if json {
        let mut combined = serde_json::Map::new();
        if let Some(ref report) = ids_report {
            combined.insert("ids".to_string(), report.to_json_value(file_name));
        }
        if let Some(ref result) = refs_result {
            let mut refs_obj = serde_json::json!({
                "node_count": result.node_count,
                "way_count": result.way_count,
                "relation_count": result.relation_count,
                "missing_node_refs": result.missing_node_refs,
                "missing_way_refs": result.missing_way_refs,
                "missing_node_members": result.missing_node_members,
                "missing_relation_members": result.missing_relation_members,
                "missing_relation_member_occurrences": result.missing_relation_member_occurrences,
                "passed": result.is_valid(),
            });
            if show_ids {
                let details: Vec<_> = result.missing_refs.iter().map(|mref| {
                    serde_json::json!({
                        "missing": format!("{}{}", mref.missing_type, mref.missing_id),
                        "referenced_by": format!("{}{}", mref.referencing_type, mref.referencing_id),
                    })
                }).collect();
                if let Some(m) = refs_obj.as_object_mut() {
                    m.insert("missing_details".to_string(), serde_json::Value::Array(details));
                }
            }
            combined.insert("refs".to_string(), refs_obj);
        }
        println!("{}", serde_json::to_string_pretty(&serde_json::Value::Object(combined))?);
    } else if !quiet {
        if let Some(ref report) = ids_report {
            report.print_human(file_name);
        }
        if let Some(ref result) = refs_result {
            if show_ids {
                for mref in &result.missing_refs {
                    println!(
                        "{}{} in {}{}",
                        mref.missing_type, mref.missing_id,
                        mref.referencing_type, mref.referencing_id,
                    );
                }
            }
            if ids_report.is_some() {
                println!();
                println!("---");
                println!();
            }
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
                    println!("Missing node members in relations: {}", result.missing_node_members);
                }
                if result.missing_relation_members > 0 {
                    if result.missing_relation_member_occurrences > result.missing_relation_members {
                        println!(
                            "Missing relation members: {} ({} references)",
                            result.missing_relation_members,
                            result.missing_relation_member_occurrences,
                        );
                    } else {
                        println!("Missing relation members: {}", result.missing_relation_members);
                    }
                }
            }
            if result.is_valid() {
                println!("Referential integrity: OK");
            } else {
                println!("Referential integrity: FAILED ({} missing references)", result.total_missing());
            }
        }
    }

    if failed {
        process::exit(1);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_tags_count(
    path: &std::path::Path,
    min_count: u64,
    max_count: Option<u64>,
    sort_str: &str,
    expressions_file: Option<&std::path::Path>,
    expressions: &[String],
    type_filter: Option<&str>,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let sort = match sort_str {
        "count-desc" | "count" => pbfhogg::tags_count::TagCountSort::CountDesc,
        "count-asc" => pbfhogg::tags_count::TagCountSort::CountAsc,
        "name-asc" | "name" => pbfhogg::tags_count::TagCountSort::NameAsc,
        "name-desc" => pbfhogg::tags_count::TagCountSort::NameDesc,
        _ => return Err(format!("unknown sort order: {sort_str} (expected count-desc, count-asc, name-asc, name-desc)").into()),
    };
    let all_expressions = combine_expressions(expressions_file, expressions)?;
    let opts = pbfhogg::tags_count::TagCountOptions {
        min_count,
        max_count,
        sort,
        expressions: &all_expressions,
        type_filter,
        direct_io,
        force,
    };
    let results = pbfhogg::tags_count::tags_count(path, &opts)?;

    for entry in &results {
        println!("{}\t{}\t{}", entry.count, entry.key, entry.value);
    }

    eprintln!("{} distinct tag values", results.len());
    Ok(())
}

fn parse_clean_attrs(attrs: &[String]) -> Result<pbfhogg::cat::CleanAttrs, Box<dyn std::error::Error>> {
    let mut clean = pbfhogg::cat::CleanAttrs::default();
    for attr in attrs {
        match attr.as_str() {
            "version" => clean.version = true,
            "changeset" => clean.changeset = true,
            "timestamp" => clean.timestamp = true,
            "uid" => clean.uid = true,
            "user" => clean.user = true,
            other => return Err(format!(
                "unknown clean attribute: {other} (expected version, changeset, timestamp, uid, user)"
            ).into()),
        }
    }
    Ok(clean)
}

#[allow(clippy::too_many_arguments)]
fn run_cat(
    files: &[PathBuf],
    output: &std::path::Path,
    type_filter: Option<&str>,
    clean_attrs: &[String],
    compression: &str,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let clean = parse_clean_attrs(clean_attrs)?;
    let paths: Vec<&std::path::Path> = files.iter().map(AsRef::as_ref).collect();
    let stats = pbfhogg::cat::cat(&paths, output, type_filter, &clean, compression, direct_io, force, overrides)?;
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
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let opts = pbfhogg::sort::SortOptions {
        compression,
        direct_io,
        io_uring,
        force,
    };
    let stats = pbfhogg::sort::sort(file, output, &opts, overrides)?;
    stats.print_summary();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_renumber(
    file: &std::path::Path,
    output: &std::path::Path,
    start_id: &str,
    compression: &str,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let parts: Vec<&str> = start_id.split(',').collect();
    let opts = match parts.len() {
        1 => {
            let id: i64 = parts[0].trim().parse()
                .map_err(|_| format!("invalid start ID: {}", parts[0]))?;
            pbfhogg::renumber::RenumberOptions {
                start_node_id: id,
                start_way_id: id,
                start_relation_id: id,
            }
        }
        3 => {
            let node_id: i64 = parts[0].trim().parse()
                .map_err(|_| format!("invalid node start ID: {}", parts[0]))?;
            let way_id: i64 = parts[1].trim().parse()
                .map_err(|_| format!("invalid way start ID: {}", parts[1]))?;
            let rel_id: i64 = parts[2].trim().parse()
                .map_err(|_| format!("invalid relation start ID: {}", parts[2]))?;
            pbfhogg::renumber::RenumberOptions {
                start_node_id: node_id,
                start_way_id: way_id,
                start_relation_id: rel_id,
            }
        }
        _ => return Err("--start-id must be a single value or 3 comma-separated values (node,way,relation)".into()),
    };
    let stats = pbfhogg::renumber_external::renumber_external(
        file, output, &opts, compression, direct_io, overrides,
    )?;
    stats.print_summary();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_tags_filter(
    file: &std::path::Path,
    output: &std::path::Path,
    expressions_file: Option<&std::path::Path>,
    expressions: &[String],
    omit_referenced: bool,
    invert_match: bool,
    remove_tags: bool,
    compression: &str,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let all_expressions = combine_expressions(expressions_file, expressions)?;
    if all_expressions.is_empty() {
        return Err("no filter expressions provided (use positional args or -e FILE)".into());
    }
    if remove_tags && omit_referenced {
        eprintln!("Warning! With -R/--omit-referenced use of -t/--remove-tags isn't doing anything.");
    }
    let opts = pbfhogg::tags_filter::TagsFilterOptions {
        expression_strs: &all_expressions,
        omit_referenced,
        invert: invert_match,
        remove_tags,
        compression,
        direct_io,
        force,
    };
    let stats = pbfhogg::tags_filter::tags_filter(file, output, &opts, overrides)?;
    stats.print_summary();
    Ok(())
}

fn run_tags_filter_osc(
    changes: &std::path::Path,
    output: &std::path::Path,
    expressions_file: Option<&std::path::Path>,
    expressions: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let all_expressions = combine_expressions(expressions_file, expressions)?;
    if all_expressions.is_empty() {
        return Err("no filter expressions provided (use positional args or -e FILE)".into());
    }
    let stats = pbfhogg::tags_filter_osc::tags_filter_osc(changes, output, &all_expressions)?;
    stats.print_summary();
    Ok(())
}

fn resolve_ids(
    id_file: Option<&std::path::Path>,
    id_osm_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
    direct_io: bool,
) -> Result<pbfhogg::getid::ElementIds, Box<dyn std::error::Error>> {
    let default_type = default_type.map(|kind| match kind {
        DefaultTypeArg::Node => pbfhogg::getid::DefaultType::Node,
        DefaultTypeArg::Way => pbfhogg::getid::DefaultType::Way,
        DefaultTypeArg::Relation => pbfhogg::getid::DefaultType::Relation,
    });
    // Start with CLI positional IDs
    let mut id_set = pbfhogg::getid::parse_ids_with_default_type(ids, default_type)?;
    // Merge IDs from text file
    if let Some(path) = id_file {
        let file_ids = pbfhogg::getid::parse_ids_from_file_with_default_type(path, default_type)?;
        pbfhogg::getid::merge_id_sets(&mut id_set, &file_ids);
    }
    // Merge IDs from OSM/PBF file
    if let Some(path) = id_osm_file {
        let pbf_ids = pbfhogg::getid::parse_ids_from_pbf(path, direct_io)?;
        pbfhogg::getid::merge_id_sets(&mut id_set, &pbf_ids);
    }
    if !id_set.node_ids.has_any() && !id_set.way_ids.has_any() && !id_set.relation_ids.has_any() {
        return Err("no IDs specified (use positional args, -i FILE, or -I OSM-FILE)".into());
    }
    Ok(id_set)
}

fn print_requested_ids(ids: &pbfhogg::getid::ElementIds) {
    eprintln!("Requested IDs:");
    if ids.node_ids.has_any() {
        eprint!("  nodes:");
        for id in ids.node_ids.iter() {
            eprint!(" {id}");
        }
        eprintln!();
    }
    if ids.way_ids.has_any() {
        eprint!("  ways:");
        for id in ids.way_ids.iter() {
            eprint!(" {id}");
        }
        eprintln!();
    }
    if ids.relation_ids.has_any() {
        eprint!("  relations:");
        for id in ids.relation_ids.iter() {
            eprint!(" {id}");
        }
        eprintln!();
    }
}

fn print_missing_ids(
    output: &std::path::Path,
    requested: &pbfhogg::getid::ElementIds,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Scan the output PBF to find which requested IDs are present.
    let found = pbfhogg::getid::parse_ids_from_pbf(output, direct_io)?;

    let missing_nodes: Vec<_> = requested.node_ids.iter().filter(|&id| !found.node_ids.get(id)).collect();
    let missing_ways: Vec<_> = requested.way_ids.iter().filter(|&id| !found.way_ids.get(id)).collect();
    let missing_rels: Vec<_> = requested.relation_ids.iter().filter(|&id| !found.relation_ids.get(id)).collect();

    let total_missing = missing_nodes.len() + missing_ways.len() + missing_rels.len();
    if total_missing == 0 {
        eprintln!("Found all requested objects.");
    } else {
        eprintln!("Did not find {total_missing} object(s):");
        for id in &missing_nodes {
            eprintln!("  n{id}");
        }
        for id in &missing_ways {
            eprintln!("  w{id}");
        }
        for id in &missing_rels {
            eprintln!("  r{id}");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_diff(
    old: &std::path::Path,
    new: &std::path::Path,
    suppress_common: bool,
    verbose: bool,
    summary: bool,
    quiet: bool,
    output_path: Option<&std::path::Path>,
    type_filter: Option<&str>,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let options = pbfhogg::diff::DiffOptions {
        suppress_common,
        verbose,
        summary,
        type_filter: type_filter.map(String::from),
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
        if summary {
            stats.print_osmium_summary();
        } else {
            stats.print_summary();
        }
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
    update_timestamp: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats =
        pbfhogg::derive_changes::derive_changes(old, new, output, direct_io, increment_version, update_timestamp)?;
    stats.print_summary();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_getid(
    file: &std::path::Path,
    output: &std::path::Path,
    add_referenced: bool,
    remove_tags: bool,
    verbose_ids: bool,
    id_file: Option<&std::path::Path>,
    id_osm_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
    compression: &str,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let id_set = resolve_ids(id_file, id_osm_file, default_type, ids, direct_io)?;

    if remove_tags && !add_referenced {
        eprintln!("Warning! Without -r/--add-referenced use of -t/--remove-tags isn't doing anything.");
    }

    if verbose_ids {
        print_requested_ids(&id_set);
    }

    let opts = pbfhogg::getid::GetidOptions { add_referenced, remove_tags };
    let stats = pbfhogg::getid::getid(
        file,
        output,
        &id_set,
        &opts,
        compression,
        direct_io,
        force,
        overrides,
    )?;
    stats.print_summary();

    if verbose_ids {
        print_missing_ids(output, &id_set, direct_io)?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_getparents(
    file: &std::path::Path,
    output: &std::path::Path,
    add_self: bool,
    id_file: Option<&std::path::Path>,
    id_osm_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
    compression: &str,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let id_set = resolve_ids(id_file, id_osm_file, default_type, ids, direct_io)?;
    let opts = pbfhogg::getparents::GetparentsOptions { add_self };
    let stats = pbfhogg::getparents::getparents(file, output, &id_set, &opts, compression, direct_io, overrides)?;
    stats.print_summary();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_removeid(
    file: &std::path::Path,
    output: &std::path::Path,
    id_file: Option<&std::path::Path>,
    id_osm_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
    compression: &str,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let id_set = resolve_ids(id_file, id_osm_file, default_type, ids, direct_io)?;
    let stats = pbfhogg::getid::removeid(file, output, &id_set, compression, direct_io, overrides)?;
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

#[allow(clippy::too_many_arguments)]
fn run_extract(
    file: &std::path::Path,
    output: &std::path::Path,
    bbox_str: Option<&str>,
    polygon_path: Option<&std::path::Path>,
    strategy: pbfhogg::extract::ExtractStrategy,
    set_bounds: bool,
    clean_attrs: &[String],
    compression: &str,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let clean = parse_clean_attrs(clean_attrs)?;
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
        set_bounds,
        &clean,
        compression,
        direct_io,
        force,
        overrides,
    )?;
    stats.print_summary();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_extract_config(
    file: &std::path::Path,
    config_path: &std::path::Path,
    directory_override: Option<&std::path::Path>,
    strategy: pbfhogg::extract::ExtractStrategy,
    set_bounds: bool,
    clean_attrs: &[String],
    compression: &str,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let clean = parse_clean_attrs(clean_attrs)?;
    let (config_dir, mut slots) = pbfhogg::extract::parse_extract_config(config_path)?;

    // If -d/--directory is given on CLI, override the config's directory
    if let Some(dir) = directory_override {
        let config_parent = config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let old_dir = config_dir
            .as_deref()
            .unwrap_or(config_parent);
        for slot in &mut slots {
            // Re-resolve output relative to new directory
            if let Ok(relative) = slot.output.strip_prefix(old_dir) {
                slot.output = dir.join(relative);
            }
        }
    }

    eprintln!("Multi-extract: {} extracts from config", slots.len());
    let all_stats = pbfhogg::extract::extract_multi(
        file,
        &slots,
        strategy,
        set_bounds,
        &clean,
        compression,
        direct_io,
        force,
        overrides,
    )?;
    for (i, stats) in all_stats.iter().enumerate() {
        eprint!("  [{}] {} - ", i + 1, slots[i].output.display());
        stats.print_summary();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn run_add_locations_to_ways(
    file: &std::path::Path,
    output: &std::path::Path,
    keep_untagged_nodes: bool,
    index_type: &str,
    compression: &str,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let index_type: pbfhogg::add_locations_to_ways::IndexType = index_type.parse()?;
    let stats = pbfhogg::add_locations_to_ways::add_locations_to_ways(
        file,
        output,
        keep_untagged_nodes,
        compression,
        direct_io,
        force,
        overrides,
        index_type,
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
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let cutoff = parse_timestamp(timestamp)?;
    let stats = pbfhogg::time_filter::time_filter(file, output, cutoff, compression, direct_io, overrides)?;
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_show_element(
    path: &std::path::Path,
    spec: &str,
    direct_io: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (elem_type, id) = parse_element_spec(spec)?;
    let found = pbfhogg::inspect::show_element(path, elem_type, id, direct_io)?;
    if !found {
        eprintln!("Element not found: {spec}");
        process::exit(1);
    }
    Ok(())
}

/// Parse "n123", "w456", "r789", "node/123", "way/456", "relation/789".
fn parse_element_spec(
    spec: &str,
) -> Result<(pbfhogg::inspect::ShowElementType, i64), Box<dyn std::error::Error>> {
    use pbfhogg::inspect::ShowElementType;

    let (type_str, id_str) = if let Some(rest) = spec.strip_prefix("node/") {
        ("n", rest)
    } else if let Some(rest) = spec.strip_prefix("way/") {
        ("w", rest)
    } else if let Some(rest) = spec.strip_prefix("relation/") {
        ("r", rest)
    } else if spec.starts_with('n') || spec.starts_with('w') || spec.starts_with('r') {
        spec.split_at(1)
    } else {
        return Err(format!(
            "invalid element spec '{spec}': expected n<id>, w<id>, r<id>, \
             node/<id>, way/<id>, or relation/<id>"
        )
        .into());
    };

    let id: i64 = id_str
        .parse()
        .map_err(|_| format!("invalid ID in '{spec}': '{id_str}' is not an integer"))?;

    let elem_type = match type_str {
        "n" => ShowElementType::Node,
        "w" => ShowElementType::Way,
        "r" => ShowElementType::Relation,
        _ => unreachable!(),
    };

    Ok((elem_type, id))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_inspect(
    path: &std::path::Path,
    indexed: bool,
    nodes: bool,
    blocks: Option<usize>,
    id_ranges: bool,
    locations: bool,
    anomalies: bool,
    extended: bool,
    get: Option<&str>,
    json: bool,
    direct_io: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // --indexed only: quick check without full inspect
    let has_other_flags = extended
        || id_ranges
        || locations
        || anomalies
        || nodes
        || blocks.is_some()
        || get.is_some();
    if indexed && !has_other_flags {
        let has_index = pbfhogg::has_indexdata(path, direct_io)?;
        if json {
            println!("{}", serde_json::json!({"indexed": has_index}));
        } else if has_index {
            println!("Indexed: yes");
        } else {
            println!("Indexed: no");
        }
        if !has_index {
            process::exit(1);
        }
        return Ok(());
    }

    if nodes && json {
        return Err("--nodes and --json cannot be used together (node stats have no JSON output)".into());
    }

    // --nodes only: run node stats without full inspect
    if nodes && !indexed && !extended && !id_ranges && !locations && !anomalies && blocks.is_none() && get.is_none() {
        let report = pbfhogg::node_stats::node_stats(path, direct_io, force)?;
        report.print_report();
        return Ok(());
    }

    if get.is_some() && json {
        return Err("--get and --json cannot be used together".into());
    }

    // --get header.* or file.* only: read just the header blob, skip full scan
    if let Some(key) = get {
        let needs_full_scan = key.starts_with("data.")
            || key.starts_with("metadata.")
            || key.starts_with("elements.")
            || key.starts_with("blocks.")
            || key == "indexed";
        let has_other_flags = extended || id_ranges || locations || anomalies || nodes || indexed || blocks.is_some();
        if !needs_full_scan && !has_other_flags {
            let mut reader = pbfhogg::BlobReader::open(path, direct_io)?;
            let header = match reader.next() {
                Some(Ok(blob)) => match blob.decode()? {
                    pbfhogg::blob::BlobDecode::OsmHeader(h) => *h,
                    _ => return Err("first blob is not an OsmHeader".into()),
                },
                Some(Err(e)) => return Err(e.into()),
                None => return Err("empty PBF file".into()),
            };
            let val = match key {
                "file.name" => Some(
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string()),
                ),
                "file.size" => Some(std::fs::metadata(path)?.len().to_string()),
                "file.format" => Some("PBF".to_string()),
                "header.bbox" => header.bbox().map(|bb| format!("{} {} {} {}", bb.left, bb.bottom, bb.right, bb.top)),
                "header.writing_program" => header.writing_program().map(String::from),
                "header.replication.url" => header.osmosis_replication_base_url().map(String::from),
                "header.replication.sequence" => header.osmosis_replication_sequence_number().map(|s| s.to_string()),
                "header.replication.timestamp" => header.osmosis_replication_timestamp().map(|t| t.to_string()),
                _ => return Err(format!("unknown key: {key}").into()),
            };
            match val {
                Some(v) => println!("{v}"),
                None => return Err(format!("key {key} has no value in this file").into()),
            }
            return Ok(());
        }
    }

    // --get with data.* keys requires --extended
    let extended = extended
        || get
            .as_ref()
            .is_some_and(|k| k.starts_with("data.") || k.starts_with("metadata."));
    let show_blocks = blocks.is_some() || anomalies;
    let block_limit = if anomalies && blocks.is_none() {
        Some(0)
    } else {
        blocks
    };
    let mut report =
        pbfhogg::inspect::inspect(path, show_blocks, id_ranges, locations, extended, direct_io)?;

    // --indexed combined with other flags: check and report, set exit code
    let mut exit_not_indexed = false;
    if indexed {
        let has_index = pbfhogg::has_indexdata(path, direct_io)?;
        if !has_index {
            exit_not_indexed = true;
        }
        if !json {
            println!("Indexed: {}", if has_index { "yes" } else { "no" });
        }
    }

    // --nodes combined with other flags: run node stats and print
    if nodes {
        let node_report = pbfhogg::node_stats::node_stats(path, direct_io, force)?;
        node_report.print_report();
    }

    if let Some(key) = get {
        match report.get_value(key) {
            Some(val) => println!("{val}"),
            None => return Err(format!("unknown key: {key}").into()),
        }
    } else if json {
        let mut value = report.to_json_filtered(block_limit, anomalies);
        if indexed {
            if let serde_json::Value::Object(ref mut map) = value {
                map.insert("indexed".to_string(), serde_json::Value::Bool(!exit_not_indexed));
            }
        }
        println!("{value}");
    } else {
        report.print_report_filtered(block_limit, anomalies);
    }

    if exit_not_indexed {
        process::exit(1);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_apply_changes(
    base: &std::path::Path,
    changes: &std::path::Path,
    output: &std::path::Path,
    compression: &str,
    direct_io: bool,
    io_uring: bool,
    force: bool,
    locations_on_ways: bool,
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let opts = pbfhogg::merge::MergeOptions {
        compression,
        direct_io,
        io_uring,
        force,
        locations_on_ways,
    };
    let stats = pbfhogg::merge::merge(base, changes, output, &opts, overrides)?;
    stats.print_summary();
    Ok(())
}

fn run_merge_pbf(
    inputs: &[PathBuf],
    output: &std::path::Path,
    compression: &str,
    direct_io: bool,
    io_uring: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let compression: Compression = compression.parse()?;
    let paths: Vec<&std::path::Path> = inputs.iter().map(AsRef::as_ref).collect();
    let opts = pbfhogg::merge_pbf::MergePbfOptions {
        compression,
        direct_io,
        io_uring,
        force,
    };
    let stats = pbfhogg::merge_pbf::merge_pbf(&paths, output, &opts, overrides)?;
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
                                nodes += 1;
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
                        bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), None);
                        nodes += 1;
                    }
                    pbfhogg::Element::Node(n) => {
                        if !bb.can_add_node() {
                            if let Some(bytes) = bb.take()? {
                                writer.write_primitive_block(bytes)?;
                            }
                        }
                        bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), None);
                        nodes += 1;
                    }
                    pbfhogg::Element::Way(w) => {
                        if !bb.can_add_way() {
                            if let Some(bytes) = bb.take()? {
                                writer.write_primitive_block(bytes)?;
                            }
                        }
                        let refs: Vec<_> = w.refs().collect();
                        bb.add_way(w.id(), w.tags(), &refs, None);
                        ways += 1;
                    }
                    pbfhogg::Element::Relation(r) => {
                        if !bb.can_add_relation() {
                            if let Some(bytes) = bb.take()? {
                                writer.write_primitive_block(bytes)?;
                            }
                        }
                        let members: Vec<_> = r
                            .members()
                            .map(|m| MemberData {
                                id: m.id,
                                role: m.role().ok().unwrap_or_default(),
                            })
                            .collect();
                        bb.add_relation(r.id(), r.tags(), &members, None);
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

    drop(std::fs::remove_file(output));

    let start = Instant::now();
    let opts = pbfhogg::merge::MergeOptions {
        compression,
        direct_io: false,
        io_uring,
        force: true,
        locations_on_ways: false,
    };
    let stats = pbfhogg::merge::merge(base, changes, output, &opts, &HeaderOverrides::default())?;
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
