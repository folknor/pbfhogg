mod host;
mod output;

use std::io::Write;
use std::path::PathBuf;
use std::process;

use clap::{Args, Parser, Subcommand, ValueEnum};
use pbfhogg::altw::IndexType;
use pbfhogg::cat::CleanAttrs;
use pbfhogg::tags_count::TagCountSort;
use pbfhogg::writer::Compression;
use pbfhogg::{HeaderOverrides, TypeFilter};

// hotpath 0.15 installed CountingAllocator internally under hotpath-alloc; as
// of 0.16 it is a plain generic type the consumer must declare - the crate no
// longer wires it up on its own. Without this, hotpath-alloc builds fall back
// to the system allocator and track_alloc/track_dealloc never fire, so every
// function silently reports 0 bytes.
#[cfg(feature = "hotpath-alloc")]
#[global_allocator]
static ALLOC: hotpath::CountingAllocator = hotpath::CountingAllocator::new();

/// The exit statuses `pbfhogg` guarantees to callers.
///
/// The split follows the `diff`/`grep` convention: a command that ran to
/// completion and answered "no" is not the same event as a command that
/// could not run at all. Scripts can therefore branch on the answer
/// (`1`) without swallowing failures (`2`), which the previous
/// everything-is-`1` scheme made impossible.
///
/// `2` also matches what clap already returns for a usage error, so the
/// "the invocation did not work" band is contiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitStatus {
    /// `1` - the command ran to completion and its answer is negative:
    /// `diff` found differences, `check` found violations,
    /// `inspect --indexed` found no index, `inspect --show` found no
    /// element. Not an error; nothing is printed to stderr for it.
    AnswerNegative,
    /// `2` - the command failed. Every error path lands here.
    Failed,
}

impl ExitStatus {
    const fn code(self) -> u8 {
        match self {
            Self::AnswerNegative => 1,
            Self::Failed => 2,
        }
    }
}

/// Non-fatal exit code carrier. Subcommands that need to signal a
/// specific non-zero exit status (e.g. `diff` with differences found)
/// return this via the error channel instead of calling `process::exit`.
/// `process::exit` skips destructors, which prevents the hotpath guard
/// in `main` from flushing its JSON report.
#[derive(Debug)]
struct ExitWithCode(ExitStatus);

impl std::fmt::Display for ExitWithCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit with code {}", self.0.code())
    }
}

impl std::error::Error for ExitWithCode {}

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

/// Every free-form flag is parsed exactly once, here at the argument
/// boundary, into the typed value the library consumes. Nothing
/// downstream re-parses a user string: `run_*` takes `Compression`,
/// `TypeFilter`, `IndexType` and friends, not `&str`.
fn parse_type_filter(value: &str) -> Result<TypeFilter, String> {
    value.parse::<TypeFilter>().map_err(|e| e.to_string())
}

fn parse_single_type(value: &str) -> Result<TypeFilter, String> {
    let filter = parse_type_filter(value)?;
    if filter.count() != 1 {
        return Err(format!(
            "invalid element type '{value}' (expected exactly one of node, way, or relation)"
        ));
    }
    Ok(filter)
}

fn parse_compression(value: &str) -> Result<Compression, String> {
    value.parse::<Compression>().map_err(|e| e.to_string())
}

#[derive(Args)]
struct CompressionArg {
    /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
    #[arg(long, default_value = "zlib", value_parser = parse_compression)]
    compression: Compression,
}

#[derive(Args)]
struct DirectIoArg {
    /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
    #[arg(long)]
    direct_io: bool,
}

#[derive(Args)]
struct DistrustMetadataArg {
    /// Ignore per-blob sidecar metadata (indexdata, tag index) and decode every
    /// blob. Slower, but the answer then depends only on element data.
    #[arg(long = "distrust-metadata")]
    distrust_metadata: bool,
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

#[derive(Clone, Copy, ValueEnum)]
enum ExportFormatArg {
    Geojsonseq,
    Geojson,
}

#[derive(Clone, Copy, ValueEnum)]
enum ExportTypeArg {
    Node,
    Way,
}

/// Metadata attribute accepted by `--clean`. The set is closed, so it is
/// a `ValueEnum` rather than a string matched in a helper: `--help`
/// enumerates it and shell completion knows it.
#[derive(Clone, Copy, ValueEnum)]
enum CleanAttrArg {
    Version,
    Changeset,
    Timestamp,
    Uid,
    User,
}

fn clean_attrs(attrs: &[CleanAttrArg]) -> CleanAttrs {
    let mut clean = CleanAttrs::default();
    for attr in attrs {
        match attr {
            CleanAttrArg::Version => clean.version = true,
            CleanAttrArg::Changeset => clean.changeset = true,
            CleanAttrArg::Timestamp => clean.timestamp = true,
            CleanAttrArg::Uid => clean.uid = true,
            CleanAttrArg::User => clean.user = true,
        }
    }
    clean
}

/// Node-index strategy for `add-locations-to-ways`.
///
/// `Dense` is hidden rather than absent: it was a real value until it was
/// removed, and a user carrying an old command line deserves the pointer
/// at `sparse` instead of a bare "invalid value".
#[derive(Clone, Copy, ValueEnum)]
enum IndexTypeArg {
    Auto,
    Sparse,
    External,
    #[value(hide = true)]
    Dense,
}

impl IndexTypeArg {
    fn resolve(self) -> Result<IndexType, Box<dyn std::error::Error>> {
        match self {
            Self::Auto => Ok(IndexType::Auto),
            Self::Sparse => Ok(IndexType::Sparse),
            Self::External => Ok(IndexType::External),
            Self::Dense => Err(
                "index type 'dense' was removed in favor of 'sparse'. Sparse \
                 (rank-indexed flat) is faster than dense at every measured \
                 scale and works in regimes dense doesn't. Use \
                 --index-type sparse instead."
                    .into(),
            ),
        }
    }
}

/// `count` and `name` were accepted as shorthands by the hand-written
/// parser this replaced; they stay, as hidden aliases.
#[derive(Clone, Copy, ValueEnum)]
enum TagSortArg {
    #[value(alias = "count")]
    CountDesc,
    CountAsc,
    #[value(alias = "name")]
    NameAsc,
    NameDesc,
}

impl From<TagSortArg> for TagCountSort {
    fn from(value: TagSortArg) -> Self {
        match value {
            TagSortArg::CountDesc => Self::CountDesc,
            TagSortArg::CountAsc => Self::CountAsc,
            TagSortArg::NameAsc => Self::NameAsc,
            TagSortArg::NameDesc => Self::NameDesc,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum BenchReadMode {
    Sequential,
    Parallel,
    Pipelined,
    Blobreader,
}

#[derive(Clone, Copy, ValueEnum)]
enum BenchWriterMode {
    Sync,
    Pipelined,
}

#[derive(Clone, Copy, ValueEnum)]
enum BenchIoMode {
    Buffered,
    Uring,
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
        clean: Vec<CleanAttrArg>,
        /// Filter by element type (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type", value_parser = parse_type_filter)]
        type_filter: Option<TypeFilter>,
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
    /// Re-encode a PBF with a different per-blob element cap.
    ///
    /// Element semantics, tags, refs, members, metadata, and DenseNodes
    /// encoding all round-trip. Output is type-sorted (nodes, ways,
    /// relations); the `Sort.Type_then_ID` flag is propagated when the
    /// input has it.
    ///
    /// Primary use case: producing same-corpus-different-encoding pairs
    /// for the blob-density measurement matrix. v1 only fires the cap
    /// on shrinks (cap < input blob size); growing across input blobs
    /// is deferred to v2.
    Repack {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Per-blob element cap. Default 8000 matches the PBF interop
        /// convention; passing 8000 against a `planet.openstreetmap.org`-
        /// packed input (~228 k/blob) produces a Geofabrik-style packing.
        #[arg(long = "elements-per-blob", default_value_t = 8000)]
        elements_per_blob: usize,
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
    /// Produce a valid-but-adversarial PBF by stripping properties or
    /// perturbing structure.
    ///
    /// At least one flag is required, and flags compose - except
    /// `--unsort` and `--unsort-intra`, which are mutually exclusive (they
    /// request opposite blob shapes). Supports `--unsort`, `--unsort-intra`,
    /// `--strip-locations`, `--strip-indexdata`, `--strip-tagdata`,
    /// `--strip-bbox`, and `--drop-ids`. Both
    /// unsort modes perturb
    /// exactly one same-kind pair per kind that has more than `block_cap`
    /// elements; kinds with fewer elements pass through unchanged. Used to
    /// produce inputs for benchmarking non-optimal code paths
    /// (`sort` overlap-rewrite, `add-locations-to-ways`, `--force`
    /// fallbacks).
    Degrade {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Clear `Sort.Type_then_ID`; pack blobs to the cap so exactly one
        /// adjacent same-kind blob pair per kind has overlapping ID ranges
        /// (cross-blob overlap). Fires `sort`'s overlap-rewrite path.
        #[arg(long, conflicts_with = "unsort_intra")]
        unsort: bool,
        /// Clear `Sort.Type_then_ID`; leave one same-kind blob per kind with
        /// an internal ID-order inversion but non-overlapping blob ranges
        /// (intra-blob shape for `sort`'s monotonicity blind spot).
        #[arg(long = "unsort-intra")]
        unsort_intra: bool,
        /// Drop inline way-node coordinates (`LocationsOnWays`).
        #[arg(long)]
        strip_locations: bool,
        /// Clear `BlobHeader.indexdata` on every OsmData blob.
        #[arg(long)]
        strip_indexdata: bool,
        /// Clear `BlobHeader.tagdata` (the per-blob tag key index) on every
        /// OsmData blob, forcing `tags-filter`'s no-hint fallback path.
        /// Leaves `indexdata` intact - a tagdata-stripped file is still
        /// indexed.
        #[arg(long)]
        strip_tagdata: bool,
        /// Replace `BlobHeader.tagdata` on every OsmData blob with a
        /// well-formed tag key index naming one key the blob does not carry.
        /// The result parses cleanly and lies, so `tags-filter` skips blobs
        /// whose elements match - the adversarial case `--strip-tagdata`
        /// cannot produce, since a missing index is decoded rather than
        /// trusted. Header-only, so it cannot be combined with a
        /// blob-re-encoding flag.
        #[arg(long = "corrupt-tagdata", conflicts_with = "strip_tagdata")]
        corrupt_tagdata: bool,
        /// Replace `BlobHeader.indexdata` on every OsmData blob with a
        /// well-formed index that overstates the blob: same element kind,
        /// inflated count, widened max ID. Produces the file `inspect`
        /// reports from without decoding anything, which is what
        /// `inspect --distrust-metadata` exists to see through.
        #[arg(long = "corrupt-indexdata", conflicts_with = "strip_indexdata")]
        corrupt_indexdata: bool,
        /// Rotate the element *kind* in every OsmData blob's
        /// `BlobHeader.indexdata` - node to way to relation to node - leaving
        /// count and ID range honest. The kind is what per-kind blob
        /// schedules select on, so this is the corruption that makes a
        /// consumer skip a blob entirely rather than misreport it. Header-only.
        #[arg(long = "corrupt-indexdata-kind", conflicts_with = "strip_indexdata")]
        corrupt_indexdata_kind: bool,
        /// Clear the `HeaderBlock.bbox` from the OSMHeader so the output
        /// declares no file-level bounding box. Header-only change; every
        /// OsmData blob passes through untouched.
        #[arg(long)]
        strip_bbox: bool,
        /// Drop exactly N elements chosen deterministically from their kind,
        /// ID, and SEED. Format: N:SEED (for example 5000:42).
        #[arg(long = "drop-ids", value_name = "N:SEED")]
        drop_ids: Option<String>,
        /// Per-block element cap on the decode path. Hidden; used by
        /// the test suite so `--unsort` can fire on small fixtures
        /// without 8000+ elements per kind.
        #[arg(long = "block-cap", default_value_t = pbfhogg::degrade::DEFAULT_BLOCK_CAP, hide = true)]
        block_cap: usize,
        /// Skip the indexdata precondition required by the decode path.
        /// The per-kind classify pipeline will fall back to scanning every
        /// blob for every kind, so runs are slower.
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        uring: UringArg,
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
        distrust: DistrustMetadataArg,
        /// Worker-pool size for the parallel classify phases.
        /// `0` (default) uses rayon's `available_parallelism()`.
        /// Pass a specific N to measure scaling or constrain CPU use.
        #[arg(short = 'j', long = "jobs", default_value_t = 0)]
        jobs: usize,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Export a PBF to GeoJSON or newline-delimited GeoJSONSeq.
    Export {
        /// Input PBF. LocationsOnWays is required for way export.
        file: PathBuf,
        /// Output file. Omit to write to stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Output format: geojsonseq or geojson.
        #[arg(long = "format", default_value = "geojsonseq")]
        format: ExportFormatArg,
        /// Only export this element type.
        #[arg(long = "type")]
        type_filter: Option<ExportTypeArg>,
        /// Read filter expressions from file, one per line.
        #[arg(short = 'e', long = "expressions")]
        expressions_file: Option<PathBuf>,
        /// Tag filter expressions, such as highway or w/building=yes.
        expressions: Vec<String>,
        /// Comma-separated whitelist of tag keys to retain.
        #[arg(long = "properties", value_delimiter = ',')]
        properties: Option<Vec<String>>,
        /// Spatial filter min_lon,min_lat,max_lon,max_lat. Way overlap is vertex-only and lossy.
        #[arg(long = "bbox")]
        bbox: Option<String>,
        /// Include OSM metadata properties.
        #[arg(long = "metadata")]
        metadata: bool,
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
        /// Print osmium-style summary (left/right/same/different
        /// counts) to stderr instead of the default pbfhogg-format
        /// summary. The pbfhogg-format summary always fires on
        /// stderr unless `--quiet` - this flag only swaps the
        /// format, it does not control whether a summary is shown.
        #[arg(short = 's', long = "osmium-summary")]
        osmium_summary: bool,
        /// Exit-code only, suppress diff output and summary
        #[arg(short = 'q', long, conflicts_with = "output")]
        quiet: bool,
        /// Write diff output to file instead of stdout
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,
        /// Filter by element type (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type", value_parser = parse_type_filter)]
        type_filter: Option<TypeFilter>,
        /// Number of parallel shards for the block-pair merge. `0` (default)
        /// auto-picks from available cores; `1` restores the sequential,
        /// scratch-free path; higher values partition the ID space across
        /// that many worker threads. Applies to both --format text and osc;
        /// parallel sharding requires both inputs to be indexed, and
        /// `-v/--verbose` always uses the sequential path (per-field detail
        /// lines are sequential-only). The parallel path writes shard temp
        /// files (planet scale: ~30 GB text, ~45 GB osc XML), removed on
        /// completion - pass `-j 1` if temp disk is scarce.
        ///
        /// Rejected with `-v/--verbose`: per-field detail lines are
        /// sequential-only, so an explicit `-j` there was accepted and
        /// discarded. `tags-filter` already rejects the equivalent
        /// `-R` + `-j`; the two now agree.
        #[arg(
            short = 'j',
            long = "jobs",
            default_value = "0",
            conflicts_with = "verbose"
        )]
        jobs: usize,
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
        /// Test instrument: override the ADR-0006 FullScan dispatch threshold.
        // Bounded at the CLI boundary rather than trusted raw into the
        // library entry point: 0 is not a threshold, it just pins the
        // FullScan arm unconditionally, and the upper bound keeps the
        // instrument inside the range a blob count can actually reach.
        #[arg(
            long,
            hide = true,
            value_parser = clap::value_parser!(u64).range(1..=1_000_000_000)
        )]
        full_scan_min_blobs: Option<u64>,
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
        clean: Vec<CleanAttrArg>,
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
        ///   auto     - route on scale (recommended): sparse unless the input is
        ///              sorted + indexed (external's precondition) AND the estimated
        ///              node store (8 bytes per node, from blob indexdata) exceeds
        ///              ~80% of this host's available RAM, where sparse's random
        ///              lookups start thrashing the page cache.
        ///   sparse   - rank-indexed flat mmap array (~8 bytes per referenced node).
        ///              Fast at small / medium scale; survives europe at ~6 minutes
        ///              on a 27 GB-RAM host. Works on any PBF. Needs temp disk equal
        ///              to referenced_nodes * 8 bytes.
        ///   external - double radix permutation. Bounded memory (~8.7 GB planet),
        ///              all sequential I/O. The only mode that survives at planet
        ///              on memory-constrained hosts. Requires sorted PBF and
        ///              indexdata. ~224 GB temp disk at planet.
        #[arg(long, default_value = "sparse")]
        index_type: IndexTypeArg,
        /// Emit pbfhogg's opt-in WayMembers-v1 and SharedNodePins-v1 metadata.
        #[arg(long)]
        inject_prepass: bool,
        /// Accept ways whose node references are missing from the input,
        /// writing those vertices as (0, 0). Without this the run fails:
        /// (0, 0) is Null Island, a real coordinate, so a placeholder vertex
        /// is indistinguishable from genuine data in the output.
        #[arg(long)]
        ignore_missing_nodes: bool,
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
        #[arg(required = true)]
        file: Option<PathBuf>,
        /// Check if PBF has blob-level indexdata (exit code 0/1)
        #[arg(long)]
        indexed: bool,
        /// Analyze node coordinate statistics for FOR compression sizing
        #[arg(long)]
        nodes: bool,
        /// Parallel worker threads for `--nodes` (only). `1` forces sequential,
        /// `0` auto-picks from `available_parallelism()`, higher values cap
        /// the worker pool. Rejected outside `--nodes` rather than ignored.
        #[arg(short = 'j', long = "jobs", default_value = "0", requires = "nodes")]
        jobs: usize,
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
        /// Show a single element by type/ID (e.g. "n123", "w456", "r789").
        ///
        /// A mode of its own: it returns one element and nothing else, so
        /// every other inspect flag is rejected alongside it rather than
        /// silently discarded (which is what used to happen).
        #[arg(
            long,
            value_name = "TYPE_ID",
            conflicts_with_all = [
                "indexed", "nodes", "jobs", "blocks", "id_ranges", "locations",
                "anomalies", "extended", "get", "json", "force",
                "distrust_metadata",
            ],
        )]
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
        distrust: DistrustMetadataArg,
        /// Proceed even if the input lacks indexdata (slower fallback path).
        /// Only the `--nodes` scan has such a fallback, so the flag is
        /// rejected outside `--nodes` rather than ignored.
        #[arg(long, requires = "nodes")]
        force: bool,
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
        /// Permit unresolved references to use the legacy (0,0) fallback.
        #[arg(long, requires = "locations_on_ways")]
        ignore_missing_nodes: bool,
        /// Accept a change whose version does not advance past the one
        /// already held for that object. Off by default: a repeated or
        /// out-of-order diff otherwise downgrades objects to an older
        /// state and reports success.
        #[arg(long)]
        allow_version_regression: bool,
        /// Worker-pool size for the descriptor-first pipeline.
        /// `0` (default) uses the `nproc - 2` heuristic (leaves two
        /// cores for the scanner + drain threads, min 1). Pass a
        /// specific N to measure scaling or constrain CPU use.
        #[arg(short = 'j', long = "jobs", default_value_t = 0)]
        jobs: usize,
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
        /// Accept an incoming change whose version does not advance past
        /// the one it replaces. Off by default: --simplify keeps the LAST
        /// change per object by input position, so inputs listed out of
        /// sequence order otherwise downgrade objects to older versions
        /// and report success.
        #[arg(long)]
        allow_version_regression: bool,
        /// Parallel worker threads. `1` forces sequential, `0` auto-picks
        /// from `available_parallelism()`, higher values cap the worker
        /// pool. Affects both the per-input parse fan-out (capped by
        /// input count) and the simplify path's `write_simplified` chunk
        /// fan-out.
        #[arg(short = 'j', long = "jobs", default_value_t = 0)]
        jobs: usize,
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
        /// Verify blob-level metadata (BlobHeader indexdata + tagdata) against
        /// the blobs themselves. Decompresses every blob. Opt-in: not part of
        /// the default --ids + --refs pair.
        #[arg(long)]
        index: bool,
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
        #[arg(short = 't', long = "type", value_parser = parse_type_filter)]
        type_filter: Option<TypeFilter>,
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
        mode: BenchReadMode,
    },
    /// Benchmark: decode + write all elements through BlockBuilder (emits kv to stderr)
    BenchWrite {
        /// Input PBF file
        file: PathBuf,
        /// Compression spec (e.g. none, zlib:6, zstd:3)
        #[arg(long, default_value = "zlib:6", value_parser = parse_compression)]
        compression: Compression,
        /// Writer mode: sync or pipelined
        #[arg(long, default_value = "sync")]
        writer: BenchWriterMode,
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
        #[arg(long, default_value = "zlib", value_parser = parse_compression)]
        compression: Compression,
        /// I/O mode: buffered, uring
        #[arg(long, default_value = "buffered")]
        io_mode: BenchIoMode,
    },
    /// Build a reverse geocoding index from a PBF file
    BuildGeocodeIndex {
        /// Input PBF file
        file: PathBuf,
        /// Output directory for index files
        #[arg(long)]
        output_dir: PathBuf,
        /// S2 cell level for streets/addresses (0-19, must exceed --coarse-level)
        #[arg(long, default_value = "17")]
        street_level: u8,
        /// Fallback cell level for rural areas (0-19, must be below --street-level)
        #[arg(long, default_value = "14")]
        coarse_level: u8,
        /// S2 cell level for admin boundaries (0-19)
        #[arg(long, default_value = "10")]
        admin_level: u8,
        /// Douglas-Peucker cap for local admin polygons; levels 5, 4, 3, and 2 retain up to 2x, 4x, 8x, and 16x this value
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
        sort: TagSortArg,
        /// Read tag expressions from file (one per line, # comments)
        #[arg(short = 'e', long = "expressions")]
        expressions_file: Option<PathBuf>,
        /// Tag filter expressions (e.g. "highway", "amenity", "w/building=yes")
        expressions: Vec<String>,
        /// Filter by element type: node, way, or relation
        #[arg(short = 't', long = "type", value_parser = parse_single_type)]
        type_filter: Option<TypeFilter>,
        /// Parallel worker threads. `1` forces sequential, `0` auto-picks
        /// from `available_parallelism()`, higher values cap the worker
        /// pool.
        #[arg(short = 'j', long = "jobs", default_value = "0")]
        jobs: usize,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
}

/// Write-side flags shared by every command that produces an output file.
///
/// These four travelled as four positional parameters through twenty `run_*`
/// functions, three of them adjacent `bool`s: transposing `direct_io` and
/// `force` at a call site compiled silently and changed what the command
/// did. As a struct they are written by field name at the one place that
/// knows their values, and the compiler catches a missing one.
///
/// Commands whose clap surface has no `--io-uring` or `--force` set the
/// corresponding field to `false` explicitly at the dispatch site.
#[derive(Clone, Copy)]
struct WriteOpts {
    compression: Compression,
    direct_io: bool,
    io_uring: bool,
    force: bool,
}

/// `getid`'s three independent booleans, same hazard as [`WriteOpts`].
#[derive(Clone, Copy)]
struct GetidFlags {
    add_referenced: bool,
    remove_tags: bool,
    verbose_ids: bool,
}

/// `tags-filter`'s three matching-mode booleans.
#[derive(Clone, Copy)]
struct TagsFilterFlags {
    omit_referenced: bool,
    invert_match: bool,
    remove_tags: bool,
    distrust_metadata: bool,
}

/// `check`'s selectors and reporting booleans.
#[derive(Clone, Copy)]
struct CheckFlags {
    ids: bool,
    refs: bool,
    index: bool,
    check_relations: bool,
    show_ids: bool,
    full: bool,
    json: bool,
    quiet: bool,
}

/// `inspect`'s section selectors. `blocks` and `get` stay out: they carry
/// values rather than being part of the interchangeable-boolean run.
#[derive(Clone, Copy)]
struct InspectFlags {
    indexed: bool,
    nodes: bool,
    id_ranges: bool,
    locations: bool,
    anomalies: bool,
    extended: bool,
    json: bool,
    force: bool,
    distrust_metadata: bool,
}

/// `diff`'s output-shape booleans.
#[derive(Clone, Copy)]
struct DiffFlags {
    suppress_common: bool,
    verbose: bool,
    osmium_summary: bool,
    quiet: bool,
}

/// The geocode index's S2 levels and search radii. The three `u8` levels
/// were adjacent positional parameters, as were the two `f32` radii - the
/// same silent-transposition hazard as the boolean runs, and harder to
/// notice because a wrong level still builds a valid-looking index.
#[derive(Clone, Copy)]
struct GeocodeLevels {
    street_level: u8,
    coarse_level: u8,
    admin_level: u8,
    max_admin_vertices: u16,
    search_radius: f32,
    coarse_search_radius: f32,
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
fn main() -> process::ExitCode {
    pbfhogg::debug::enable_marker_fifo_from_env();
    let host = host::cli_host();
    let _guard = hotpath::HotpathGuardBuilder::new("pbfhogg::main")
        .percentiles(&[50.0, 95.0, 99.0])
        .functions_limit(0)
        .build();

    // `Cli::parse()` ends a parse error - and `--help` / `--version` - with
    // `clap::Error::exit`, which calls `process::exit` and so skips every
    // destructor, including the hotpath guard built two lines up. That is
    // exactly the hazard `ExitWithCode` exists to avoid on the command
    // paths, so the argument path uses `try_parse` and returns instead.
    // `Error::print` writes to the same stream clap would have used
    // (stdout for help and version, stderr for a usage error) and
    // `Error::exit_code` is clap's own code for the case, so neither the
    // text nor the status a caller sees changes.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let code = err.exit_code();
            drop(err.print());
            return process::ExitCode::from(
                u8::try_from(code).unwrap_or(ExitStatus::Failed.code()),
            );
        }
    };

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        validate_input_output_aliases(&cli.command)?;
        match cli.command {
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
                let write = WriteOpts {
                    compression: compression.compression,
                    direct_io: io.direct_io,
                    io_uring: uring.io_uring,
                    force: force.force,
                };
                if dedupe {
                    if type_filter.is_some() {
                        return Err("--type is not valid with --dedupe".into());
                    }
                    if !clean.is_empty() {
                        return Err("--clean is not valid with --dedupe".into());
                    }
                    guarded_output(&output.output, |out| {
                        run_merge_pbf(&files, out, &write, &overrides, &host)
                    })
                } else {
                    if uring.io_uring {
                        return Err("--io-uring is only valid with --dedupe".into());
                    }
                    guarded_output(&output.output, |out| {
                        run_cat(
                            &files,
                            out,
                            type_filter,
                            &clean_attrs(&clean),
                            &write,
                            &overrides,
                            &host,
                        )
                    })
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
            } => {
                let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
                let write = WriteOpts {
                    compression: compression.compression,
                    direct_io: io.direct_io,
                    io_uring: uring.io_uring,
                    force: force.force,
                };
                guarded_output(&output.output, |out| {
                    run_sort(&file, out, &write, &overrides, &host)
                })
            }
            Command::Repack {
                file,
                output,
                elements_per_blob,
                compression,
                io,
                uring,
                force,
                header,
            } => {
                let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
                let write = WriteOpts {
                    compression: compression.compression,
                    direct_io: io.direct_io,
                    io_uring: uring.io_uring,
                    force: force.force,
                };
                guarded_output(&output.output, |out| {
                    run_repack(&file, out, elements_per_blob, &write, &overrides, &host)
                })
            }
            Command::Degrade {
                file,
                output,
                unsort,
                unsort_intra,
                strip_locations,
                strip_indexdata,
                strip_tagdata,
                corrupt_tagdata,
                corrupt_indexdata,
                corrupt_indexdata_kind,
                strip_bbox,
                drop_ids,
                block_cap,
                force,
                compression,
                io,
                uring,
                header,
            } => {
                let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
                let write = WriteOpts {
                    compression: compression.compression,
                    direct_io: io.direct_io,
                    io_uring: uring.io_uring,
                    force,
                };
                // Six adjacent `bool`s used to be six positional arguments
                // here; the library's own `DegradeFlags` already names them,
                // so build it where the flags are read.
                let flags = pbfhogg::degrade::DegradeFlags {
                    unsort,
                    unsort_intra,
                    strip_locations,
                    strip_indexdata,
                    strip_tagdata,
                    corrupt_tagdata,
                    corrupt_indexdata,
                    corrupt_indexdata_kind,
                    strip_bbox,
                    drop_ids: drop_ids
                        .as_deref()
                        .map(pbfhogg::degrade::DropSpec::parse)
                        .transpose()?,
                };
                guarded_output(&output.output, |out| {
                    run_degrade(&file, out, flags, block_cap, &write, &overrides, &host)
                })
            }
            Command::Renumber {
                file,
                output,
                start_id,
                compression,
                io,
                header,
            } => {
                let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
                let write = WriteOpts {
                    compression: compression.compression,
                    direct_io: io.direct_io,
                    io_uring: false,
                    force: false,
                };
                guarded_output(&output.output, |out| {
                    run_renumber(&file, out, &start_id, &write, &overrides, &host)
                })
            }
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
                distrust,
                io,
                jobs,
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
                    // OSC is XML: there are no blobs and no sidecar metadata,
                    // so nothing to distrust. Rejecting says that; accepting
                    // would imply the OSC path had a trust boundary it does
                    // not have.
                    if distrust.distrust_metadata {
                        return Err(
                            "--distrust-metadata is not valid in OSC mode (OSC input carries no blob metadata)".into(),
                        );
                    }
                    if jobs != 0 {
                        return Err("-j/--jobs is not valid in OSC mode (OSC parse is single-threaded)".into());
                    }
                    guarded_output(&output.output, |out| {
                        run_tags_filter_osc(&file, out, expressions_file.as_deref(), &expressions, &host)
                    })
                }
                InputKind::Pbf if omit_referenced && jobs != 0 => {
                    Err("-j/--jobs has no effect with -R/--omit-referenced (single-pass mode uses the pipelined reader, not the parallel classify path). Drop -R or drop -j.".into())
                }
                InputKind::Pbf => {
                    let overrides =
                        HeaderOverrides::parse(header.generator, &header.output_headers)?;
                    let write = WriteOpts {
                        compression: compression.compression,
                        direct_io: io.direct_io,
                        io_uring: false,
                        force: force.force,
                    };
                    let flags = TagsFilterFlags {
                        omit_referenced,
                        invert_match,
                        remove_tags,
                        distrust_metadata: distrust.distrust_metadata,
                    };
                    guarded_output(&output.output, |out| {
                        run_tags_filter(
                            &file,
                            out,
                            expressions_file.as_deref(),
                            &expressions,
                            flags,
                            &write,
                            jobs,
                            &overrides,
                         &host)
                    })
                }
            }
            }
            Command::Export {
                file,
                output,
                format,
                type_filter,
                expressions_file,
                expressions,
                properties,
                bbox,
                metadata,
            } => run_export(
                &file,
                output.as_deref(),
                format,
                type_filter,
                expressions_file.as_deref(),
                &expressions,
                properties,
                bbox.as_deref(),
                metadata,
                &host,
            ),
            Command::Diff {
                old,
                new,
                suppress_common,
                verbose,
                osmium_summary,
                quiet,
                output,
                type_filter,
                jobs,
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
                    if osmium_summary {
                        return Err("--osmium-summary is not valid with --format osc".into());
                    }
                    if quiet {
                        return Err("--quiet is not valid with --format osc".into());
                    }
                    if type_filter.is_some() {
                        return Err("--type is not valid with --format osc".into());
                    }
                    guarded_output(&output, |out| {
                        run_derive_changes(
                            &old,
                            &new,
                            out,
                            io.direct_io,
                            increment_version,
                            update_timestamp,
                            jobs,
                            &host,
                        )
                    })
                }
                DiffFormat::Text => {
                    if increment_version {
                        return Err("--increment-version is only valid with --format osc".into());
                    }
                    if update_timestamp {
                        return Err("--update-timestamp is only valid with --format osc".into());
                    }
                    let flags = DiffFlags {
                        suppress_common,
                        verbose,
                        osmium_summary,
                        quiet,
                    };
                    match output.as_deref() {
                        Some(path) => guarded_output(path, |out| {
                            run_diff(
                                &old,
                                &new,
                                flags,
                                Some(out),
                                type_filter,
                                jobs,
                                io.direct_io,
                                &host,
                            )
                        }),
                        None => run_diff(
                            &old,
                            &new,
                            flags,
                            None,
                            type_filter,
                            jobs,
                            io.direct_io,
                            &host,
                        ),
                    }
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
                let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
                let write = WriteOpts {
                    compression: compression.compression,
                    direct_io: io.direct_io,
                    io_uring: false,
                    force: force.force,
                };
                if invert {
                    guarded_output(&output.output, |out| {
                        run_removeid(
                            &file,
                            out,
                            id_file.as_deref(),
                            id_osm_file.as_deref(),
                            default_type,
                            &ids,
                            &write,
                            &overrides,
                            &host,
                        )
                    })
                } else {
                    let flags = GetidFlags {
                        add_referenced,
                        remove_tags,
                        verbose_ids,
                    };
                    guarded_output(&output.output, |out| {
                        run_getid(
                            &file,
                            out,
                            flags,
                            id_file.as_deref(),
                            id_osm_file.as_deref(),
                            default_type,
                            &ids,
                            &write,
                            &overrides,
                            &host,
                        )
                    })
                }
            }
            Command::Getparents {
                file,
                output,
                add_self,
                full_scan_min_blobs,
                id_file,
                id_osm_file,
                default_type,
                ids,
                compression,
                io,
                header,
            } => {
                let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
                let write = WriteOpts {
                    compression: compression.compression,
                    direct_io: io.direct_io,
                    io_uring: false,
                    force: false,
                };
                guarded_output(&output.output, |out| {
                    run_getparents(
                        &file,
                        out,
                        add_self,
                        id_file.as_deref(),
                        id_osm_file.as_deref(),
                        default_type,
                        &ids,
                        &write,
                        &overrides,
                        full_scan_min_blobs,
                        &host,
                    )
                })
            }
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
                let write = WriteOpts {
                    compression: compression.compression,
                    direct_io: io.direct_io,
                    io_uring: false,
                    force: force.force,
                };
                if let Some(config_path) = config.as_deref() {
                    run_extract_config(
                        &file,
                        config_path,
                        directory.as_deref(),
                        extract_strategy(simple, smart),
                        set_bounds,
                        &clean_attrs(&clean),
                        &write,
                        &overrides,
                        &host,
                    )
                } else if let Some(output) = output.as_ref() {
                    guarded_output(output, |out| {
                        run_extract(
                            &file,
                            out,
                            bbox.as_deref(),
                            polygon.as_deref(),
                            extract_strategy(simple, smart),
                            set_bounds,
                            &clean_attrs(&clean),
                            &write,
                            &overrides,
                            &host,
                        )
                    })
                } else {
                    Err("--output is required without --config".into())
                }
            }
            Command::AddLocationsToWays {
                file,
                output,
                keep_untagged_nodes,
                index_type,
                inject_prepass,
                ignore_missing_nodes,
                compression,
                force,
                io,
                header,
            } => {
                let index_type = index_type.resolve()?;
                let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
                let write = WriteOpts {
                    compression: compression.compression,
                    direct_io: io.direct_io,
                    io_uring: false,
                    force: force.force,
                };
                let options = pbfhogg::altw::AltwOptions {
                    keep_untagged_nodes,
                    compression: write.compression,
                    direct_io: write.direct_io,
                    force: write.force,
                    index_type,
                    inject_prepass,
                    ignore_missing_nodes,
                };
                guarded_output(&output.output, |out| {
                    run_add_locations_to_ways(&file, out, &options, &overrides, &host)
                })
            }
            Command::TimeFilter {
                file,
                output,
                timestamp,
                compression,
                io,
                header,
            } => {
                let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
                let write = WriteOpts {
                    compression: compression.compression,
                    direct_io: io.direct_io,
                    io_uring: false,
                    force: false,
                };
                guarded_output(&output.output, |out| {
                    run_time_filter(&file, out, &timestamp, &write, &overrides, &host)
                })
            }
            Command::Inspect {
                subcommand,
                file,
                indexed,
                nodes,
                jobs,
                blocks,
                id_ranges,
                locations,
                anomalies,
                extended,
                show,
                get,
                json,
                io,
                distrust,
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
                    jobs: tags_jobs,
                    io: tags_io,
                    force: tags_force,
                }) = subcommand
                {
                    run_tags_count(
                        &tags_file,
                        min_count,
                        max_count,
                        sort.into(),
                        expressions_file.as_deref(),
                        &expressions,
                        type_filter,
                        tags_jobs,
                        tags_io.direct_io,
                        tags_force.force,
                        &host,
                    )
                } else {
                    let file = file.ok_or("Input PBF file is required")?;
                    if let Some(ref show_id) = show {
                        return run_show_element(&file, show_id, io.direct_io, &host);
                    }
                    run_inspect(
                        &file,
                        InspectFlags {
                            indexed,
                            nodes,
                            id_ranges,
                            locations,
                            anomalies,
                            extended,
                            json,
                            force,
                            distrust_metadata: distrust.distrust_metadata,
                        },
                        jobs,
                        blocks,
                        get.as_deref(),
                        io.direct_io,
                        &host,
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
                ignore_missing_nodes,
                allow_version_regression,
                jobs,
                header,
            } => {
                let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
                let write = WriteOpts {
                    compression: compression.compression,
                    direct_io: io.direct_io,
                    io_uring: uring.io_uring,
                    force: force.force,
                };
                let opts = pbfhogg::apply_changes::MergeOptions {
                    compression: write.compression,
                    direct_io: write.direct_io,
                    io_uring: write.io_uring,
                    force: write.force,
                    locations_on_ways,
                    ignore_missing_nodes,
                    jobs: if jobs == 0 { None } else { Some(jobs) },
                    allow_version_regression,
                    #[cfg(feature = "test-hooks")]
                    panic_at_blob_seq: None,
                };
                guarded_output(&output.output, |out| {
                    run_apply_changes(&base, &changes, out, &opts, &overrides, &host)
                })
            }
            Command::MergeChanges {
                changes,
                output,
                simplify,
                allow_version_regression,
                jobs,
            } => guarded_output(&output.output, |out| {
                run_merge_changes(
                    &changes,
                    out,
                    simplify,
                    jobs,
                    allow_version_regression,
                    &host,
                )
            }),
            Command::Check {
                file,
                ids,
                refs,
                index,
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
                CheckFlags {
                    ids,
                    refs,
                    index,
                    check_relations,
                    show_ids,
                    full,
                    json,
                    quiet,
                },
                type_filter,
                max_errors,
                io.direct_io,
                &host,
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
                GeocodeLevels {
                    street_level,
                    coarse_level,
                    admin_level,
                    max_admin_vertices,
                    search_radius,
                    coarse_search_radius,
                },
                io.direct_io,
                force.force,
                &host,
            ),
            Command::BenchRead { file, mode } => run_bench_read(&file, mode),
            Command::BenchWrite {
                file,
                compression,
                writer,
            } => run_bench_write(&file, compression, writer),
            Command::BenchMerge {
                base,
                changes,
                output,
                compression,
                io_mode,
            } => run_bench_merge(&base, &changes, &output.output, compression, io_mode, &host),
        }
    })();

    // Emit the `writer_*` sidecar counters exactly once, here rather than
    // from `PbfWriter::flush`. The counters are a process-global static, so
    // flush-time emission published the same running totals once per writer -
    // once per region for `multi-extract` - which reads as a per-region
    // breakdown and is not one. This is the single point every command path
    // passes through, success or failure, and it runs before the hotpath
    // guard drops.
    pbfhogg::write::emit_writer_counters();

    match result {
        Ok(()) => process::ExitCode::SUCCESS,
        Err(e) => {
            if let Some(exit) = e.downcast_ref::<ExitWithCode>() {
                // Silent exit with a specific code (e.g. `diff` found
                // differences). Not an error worth printing.
                process::ExitCode::from(exit.0.code())
            } else if output::reader_gone() {
                // Stdout's reader went away (`pbfhogg ... | head`) and the
                // command stopped writing. Nobody is left to read a report
                // and nothing failed, so end quietly with success - see
                // `output` for why this is keyed on the recorded event
                // rather than on the shape of `e`.
                process::ExitCode::SUCCESS
            } else {
                eprintln!("Error: {e}");
                process::ExitCode::from(ExitStatus::Failed.code())
            }
        }
    }
    // `_guard` drops here, flushing the hotpath JSON report before the
    // returned `ExitCode` is converted to the process exit status.
}

fn run_build_geocode_index(
    file: &std::path::Path,
    output_dir: &std::path::Path,
    levels: GeocodeLevels,
    direct_io: bool,
    force: bool,
    host: &pbfhogg::host::Host,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let config = pbfhogg::geocode_index::builder::BuildConfig {
        input_path: file.to_path_buf(),
        output_dir: output_dir.to_path_buf(),
        force,
        direct_io,
        street_level: levels.street_level,
        coarse_level: levels.coarse_level,
        admin_level: levels.admin_level,
        max_admin_vertices: levels.max_admin_vertices,
        fine_search_radius_m: levels.search_radius,
        coarse_search_radius_m: levels.coarse_search_radius,
    };
    let stats = pbfhogg::geocode_index::builder::build_geocode_index(&config, host)?;
    eprintln!(
        "Index built: {} addr, {} streets, {} interp, {} admin ({} relations dropped), {} fine cells, {} coarse cells, {} admin cells",
        stats.addr_points,
        stats.street_ways,
        stats.interp_ways,
        stats.admin_polygons,
        stats.dropped_admin_relations,
        stats.fine_cells,
        stats.coarse_cells,
        stats.admin_cells,
    );
    Ok(())
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn run_check(
    path: &std::path::Path,
    flags: CheckFlags,
    type_filter: Option<TypeFilter>,
    max_errors: usize,
    direct_io: bool,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let CheckFlags {
        ids,
        refs,
        index,
        check_relations,
        show_ids,
        full,
        json,
        quiet,
    } = flags;
    // Default: run the --ids + --refs pair when no selector is given.
    //
    // `--index` is a selector like the other two, but it is NOT part of the
    // default set: it decompresses the entire file to re-derive every blob's
    // metadata, so folding it into a bare `check` would silently multiply the
    // cost of the command everyone already runs. It does suppress the default,
    // so `check --index` runs only the metadata verification.
    let any_selector = ids || refs || index;
    let run_ids = ids || !any_selector;
    let run_refs = refs || !any_selector;
    let run_index = index;

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
            return Err(
                "--check-relations requires --refs (or omit --ids to run both checks)".into(),
            );
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
        use pbfhogg::check::verify_ids::VerifyIdsOptions;
        let opts = VerifyIdsOptions {
            full,
            type_filter,
            max_errors,
            direct_io,
        };
        let report = pbfhogg::check::verify_ids::verify_ids(path, &opts, host)?;
        if !report.passed {
            failed = true;
        }
        Some(report)
    } else {
        None
    };

    let index_report = if run_index {
        use pbfhogg::check::index::CheckIndexOptions;
        let report = pbfhogg::check::index::check_index(
            path,
            &CheckIndexOptions {
                max_errors,
                direct_io,
            },
            host,
        )?;
        if !report.passed() {
            failed = true;
        }
        Some(report)
    } else {
        None
    };

    let refs_result = if run_refs {
        let result =
            pbfhogg::check::refs::check_refs(path, check_relations, show_ids, direct_io, host)?;
        if !result.is_valid() {
            failed = true;
        }
        Some(result)
    } else {
        None
    };

    // Output. The verdict is decided above, before a byte is written, so a
    // reader that stops early (`check x | head -1`) must not cost the exit
    // status that carries it.
    let printed = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut out = output::buffered();
        if json {
            let mut combined = serde_json::Map::new();
            if let Some(ref report) = ids_report {
                combined.insert("ids".to_string(), report.to_json_value(file_name));
            }
            if let Some(ref report) = index_report {
                combined.insert("index".to_string(), report.to_json_value(file_name));
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
                    "untrackable_node_ids": result.untrackable_ids[0],
                    "untrackable_way_ids": result.untrackable_ids[1],
                    "untrackable_relation_ids": result.untrackable_ids[2],
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
                        m.insert(
                            "missing_details".to_string(),
                            serde_json::Value::Array(details),
                        );
                    }
                }
                combined.insert("refs".to_string(), refs_obj);
            }
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&serde_json::Value::Object(combined))?
            )?;
        } else if !quiet {
            if let Some(ref report) = ids_report {
                report.write_human(&mut out, file_name)?;
            }
            if let Some(ref report) = index_report {
                if ids_report.is_some() {
                    writeln!(out, "\n---\n")?;
                }
                report.write_human(&mut out, file_name)?;
            }
            if let Some(ref result) = refs_result {
                if show_ids {
                    for mref in &result.missing_refs {
                        writeln!(
                            out,
                            "{}{} in {}{}",
                            mref.missing_type,
                            mref.missing_id,
                            mref.referencing_type,
                            mref.referencing_id,
                        )?;
                    }
                }
                if ids_report.is_some() || index_report.is_some() {
                    writeln!(out, "\n---\n")?;
                }
                writeln!(
                    out,
                    "Elements: {} nodes, {} ways, {} relations",
                    result.node_count, result.way_count, result.relation_count
                )?;
                if result.missing_node_refs > 0 {
                    writeln!(
                        out,
                        "Missing node refs in ways: {}",
                        result.missing_node_refs
                    )?;
                }
                if check_relations {
                    if result.missing_way_refs > 0 {
                        writeln!(
                            out,
                            "Missing way refs in relations: {}",
                            result.missing_way_refs
                        )?;
                    }
                    if result.missing_node_members > 0 {
                        writeln!(
                            out,
                            "Missing node members in relations: {}",
                            result.missing_node_members
                        )?;
                    }
                    if result.missing_relation_members > 0 {
                        if result.missing_relation_member_occurrences
                            > result.missing_relation_members
                        {
                            writeln!(
                                out,
                                "Missing relation members: {} ({} references)",
                                result.missing_relation_members,
                                result.missing_relation_member_occurrences,
                            )?;
                        } else {
                            writeln!(
                                out,
                                "Missing relation members: {}",
                                result.missing_relation_members
                            )?;
                        }
                    }
                }
                // An untrackable ID is not itself a missing reference, but every
                // reference to one is counted as missing above. Say so, or the
                // over-report reads as a genuinely dangling ref.
                if result.total_untrackable() > 0 {
                    writeln!(
                        out,
                        "Untrackable element IDs: {} nodes, {} ways, {} relations \
                     - references to these are counted as missing",
                        result.untrackable_ids[0],
                        result.untrackable_ids[1],
                        result.untrackable_ids[2],
                    )?;
                }
                if result.is_valid() {
                    writeln!(out, "Referential integrity: OK")?;
                } else {
                    writeln!(
                        out,
                        "Referential integrity: FAILED ({} missing references)",
                        result.total_missing()
                    )?;
                }
            }
        }
        out.flush()?;
        Ok(())
    })();
    output::tolerate_reader_gone(printed)?;

    if failed {
        return Err(Box::new(ExitWithCode(ExitStatus::AnswerNegative)));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_tags_count(
    path: &std::path::Path,
    min_count: u64,
    max_count: Option<u64>,
    sort: TagCountSort,
    expressions_file: Option<&std::path::Path>,
    expressions: &[String],
    type_filter: Option<TypeFilter>,
    jobs: usize,
    direct_io: bool,
    force: bool,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let all_expressions = combine_expressions(expressions_file, expressions)?;
    let opts = pbfhogg::tags_count::TagCountOptions {
        min_count,
        max_count,
        sort,
        expressions: &all_expressions,
        type_filter,
        direct_io,
        force,
        jobs,
    };
    let results = pbfhogg::tags_count::tags_count(path, &opts, host)?;

    let mut out = output::buffered();
    for entry in &results {
        writeln!(out, "{}\t{}\t{}", entry.count, entry.key, entry.value)?;
    }
    out.flush()?;

    eprintln!("{} distinct tag values", results.len());
    Ok(())
}

fn run_cat(
    files: &[PathBuf],
    output: &std::path::Path,
    type_filter: Option<TypeFilter>,
    clean: &CleanAttrs,
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<&std::path::Path> = files.iter().map(AsRef::as_ref).collect();
    let stats = pbfhogg::cat::cat(
        &paths,
        output,
        type_filter,
        clean,
        write.compression,
        write.direct_io,
        write.force,
        overrides,
        host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("cat.summary"))?;
    Ok(())
}

fn run_sort(
    file: &std::path::Path,
    output: &std::path::Path,
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let opts = pbfhogg::sort::SortOptions {
        compression: write.compression,
        direct_io: write.direct_io,
        io_uring: write.io_uring,
        force: write.force,
    };
    let stats = pbfhogg::sort::sort(file, output, &opts, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("sort.summary"))?;
    Ok(())
}

fn run_repack(
    file: &std::path::Path,
    output: &std::path::Path,
    elements_per_blob: usize,
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::repack::repack(
        file,
        output,
        elements_per_blob,
        write.compression,
        write.direct_io,
        write.io_uring,
        write.force,
        overrides,
        host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("repack.summary"))?;
    Ok(())
}

fn run_degrade(
    file: &std::path::Path,
    output: &std::path::Path,
    flags: pbfhogg::degrade::DegradeFlags,
    block_cap: usize,
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::degrade::degrade(
        file,
        output,
        flags,
        block_cap,
        write.compression,
        write.direct_io,
        write.io_uring,
        write.force,
        overrides,
        host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("degrade.summary"))?;
    Ok(())
}

fn run_renumber(
    file: &std::path::Path,
    output: &std::path::Path,
    start_id: &str,
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let parts: Vec<&str> = start_id.split(',').collect();
    let opts =
        match parts.len() {
            1 => {
                let id: i64 = parts[0]
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid start ID: {}", parts[0]))?;
                pbfhogg::renumber::RenumberOptions {
                    start_node_id: id,
                    start_way_id: id,
                    start_relation_id: id,
                }
            }
            3 => {
                let node_id: i64 = parts[0]
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid node start ID: {}", parts[0]))?;
                let way_id: i64 = parts[1]
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid way start ID: {}", parts[1]))?;
                let rel_id: i64 = parts[2]
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid relation start ID: {}", parts[2]))?;
                pbfhogg::renumber::RenumberOptions {
                    start_node_id: node_id,
                    start_way_id: way_id,
                    start_relation_id: rel_id,
                }
            }
            _ => return Err(
                "--start-id must be a single value or 3 comma-separated values (node,way,relation)"
                    .into(),
            ),
        };
    let stats = pbfhogg::renumber::renumber_external(
        file,
        output,
        &opts,
        write.compression,
        write.direct_io,
        overrides,
        host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("renumber.summary"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_tags_filter(
    file: &std::path::Path,
    output: &std::path::Path,
    expressions_file: Option<&std::path::Path>,
    expressions: &[String],
    flags: TagsFilterFlags,
    write: &WriteOpts,
    jobs: usize,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let TagsFilterFlags {
        omit_referenced,
        invert_match,
        remove_tags,
        distrust_metadata,
    } = flags;
    let all_expressions = combine_expressions(expressions_file, expressions)?;
    if all_expressions.is_empty() {
        return Err("no filter expressions provided (use positional args or -e FILE)".into());
    }
    if remove_tags && omit_referenced {
        eprintln!(
            "Warning! With -R/--omit-referenced use of -t/--remove-tags isn't doing anything."
        );
    }
    let opts = pbfhogg::tags_filter::TagsFilterOptions {
        expression_strs: &all_expressions,
        omit_referenced,
        invert: invert_match,
        remove_tags,
        compression: write.compression,
        direct_io: write.direct_io,
        force: write.force,
        jobs: if jobs == 0 { None } else { Some(jobs) },
        distrust_metadata,
    };
    let stats = pbfhogg::tags_filter::tags_filter(file, output, &opts, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("tags_filter.summary"))?;
    Ok(())
}

fn run_tags_filter_osc(
    changes: &std::path::Path,
    output: &std::path::Path,
    expressions_file: Option<&std::path::Path>,
    expressions: &[String],
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let all_expressions = combine_expressions(expressions_file, expressions)?;
    if all_expressions.is_empty() {
        return Err("no filter expressions provided (use positional args or -e FILE)".into());
    }
    let stats =
        pbfhogg::tags_filter::osc::tags_filter_osc(changes, output, &all_expressions, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("tags_filter.osc.summary"))?;
    Ok(())
}

fn paths_alias(input: &std::path::Path, output: &std::path::Path) -> std::io::Result<bool> {
    // An input that does not exist cannot alias anything, and reporting it
    // from here would replace the command's own "failed to open <path>"
    // with a bare aliasing-check IO error that never names the file. The
    // check runs before every command, so it must stay silent about
    // problems that are not aliasing and let the command speak first.
    let input_canonical = match std::fs::canonicalize(input) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if output.exists() {
        let output_canonical = std::fs::canonicalize(output)?;
        if input_canonical == output_canonical {
            return Ok(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let input_meta = std::fs::metadata(&input_canonical)?;
            let output_meta = std::fs::metadata(&output_canonical)?;
            return Ok(
                input_meta.dev() == output_meta.dev() && input_meta.ino() == output_meta.ino()
            );
        }
        #[cfg(not(unix))]
        return Ok(false);
    }

    let Some(planned) = planned_path(output)? else {
        return Ok(false);
    };
    Ok(input_canonical == planned)
}

/// Resolve the location a not-yet-created file will occupy: the canonical
/// parent directory joined with the final component. Returns `None` when the
/// path has no file name (e.g. a bare `..`), which can never be a valid output.
fn planned_path(path: &std::path::Path) -> std::io::Result<Option<PathBuf>> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let parent = std::fs::canonicalize(parent)?;
    Ok(path.file_name().map(|name| parent.join(name)))
}

/// Alias check between two *outputs*, where neither side is required to exist
/// yet. Whenever at least one side is already present on disk this delegates to
/// `paths_alias`, so symlink and hardlink identity is decided in exactly one
/// place; only when both are still absent does it fall back to comparing the
/// planned destinations.
fn outputs_alias(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
    if a.exists() {
        return paths_alias(a, b);
    }
    if b.exists() {
        return paths_alias(b, a);
    }
    match (planned_path(a)?, planned_path(b)?) {
        (Some(a), Some(b)) => Ok(a == b),
        _ => Ok(false),
    }
}

fn reject_aliases(
    inputs: &[&std::path::Path],
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    for input in inputs {
        if paths_alias(input, output)? {
            return Err(format!(
                "output '{}' aliases input '{}'",
                output.display(),
                input.display()
            )
            .into());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_input_output_aliases(command: &Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Cat { files, output, .. } => {
            let inputs: Vec<&std::path::Path> = files.iter().map(PathBuf::as_path).collect();
            reject_aliases(&inputs, &output.output)
        }
        Command::Sort { file, output, .. }
        | Command::Repack { file, output, .. }
        | Command::Degrade { file, output, .. }
        | Command::Renumber { file, output, .. }
        | Command::TagsFilter { file, output, .. }
        | Command::Getid { file, output, .. }
        | Command::Getparents { file, output, .. }
        | Command::AddLocationsToWays { file, output, .. }
        | Command::TimeFilter { file, output, .. } => reject_aliases(&[file], &output.output),
        Command::Export {
            file,
            output: Some(output),
            ..
        } => reject_aliases(&[file], output),
        Command::Diff {
            old,
            new,
            output: Some(output),
            ..
        } => reject_aliases(&[old, new], output),
        Command::Extract {
            file,
            output: Some(output),
            ..
        } => reject_aliases(&[file], output),
        Command::ApplyChanges {
            base,
            changes,
            output,
            ..
        } => reject_aliases(&[base, changes], &output.output),
        Command::MergeChanges {
            changes, output, ..
        } => {
            let inputs: Vec<&std::path::Path> = changes.iter().map(PathBuf::as_path).collect();
            reject_aliases(&inputs, &output.output)
        }
        Command::BenchMerge {
            base,
            changes,
            output,
            ..
        } => reject_aliases(&[base, changes], &output.output),
        Command::Export { output: None, .. }
        | Command::Diff { output: None, .. }
        | Command::Extract { output: None, .. }
        | Command::Inspect { .. }
        | Command::Check { .. }
        | Command::BenchRead { .. }
        | Command::BenchWrite { .. }
        | Command::BuildGeocodeIndex { .. } => Ok(()),
    }
}

/// Run a command that writes one final output file, through a scratch
/// sibling that is renamed into place only once the command succeeded.
///
/// Every user-named `--output` goes through here. Without it a mid-run
/// failure leaves a truncated partial file under the name the user chose -
/// and, worse, `File::create` truncates a pre-existing file of that name
/// before the first byte of new output exists. The closure receives the
/// scratch path, so the command never learns the destination at all.
///
/// The scratch file is a dot-prefixed sibling in the destination directory
/// (so `commit`'s rename never crosses a filesystem), and keeps the
/// destination's suffix so extension-sniffing writers behave identically.
fn guarded_output<F>(output: &std::path::Path, run: F) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&std::path::Path) -> Result<(), Box<dyn std::error::Error>>,
{
    let out = pbfhogg::path_guard::OutputFile::new(output)?;
    match run(out.path()) {
        Ok(()) => {
            out.commit()?;
            Ok(())
        }
        // `ExitWithCode` is not a failure: the command ran to completion and
        // its answer is merely negative (`diff` found differences). Its
        // output is complete and must be published, or a `diff -o` that
        // found differences - the ordinary case - would delete its own
        // result on the way out.
        Err(e) if e.downcast_ref::<ExitWithCode>().is_some() => {
            out.commit()?;
            Err(e)
        }
        Err(e) => Err(e),
    }
}

fn run_export_to<W: std::io::Write>(
    file: &std::path::Path,
    writer: W,
    options: &pbfhogg::export::ExportOptions,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::export::export(file, writer, options, host).map_err(export_error)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("export_to.summary"))?;
    Ok(())
}

/// The library reports a missing header feature without naming any CLI
/// flag - a `Display` impl in a library has no idea which front end is
/// reading it. Naming `--type node` is this binary's job, so the advice
/// is attached here, where the flag actually exists.
fn export_error(err: pbfhogg::Error) -> Box<dyn std::error::Error> {
    if matches!(err.kind(), pbfhogg::ErrorKind::MissingFeature(f) if *f == "LocationsOnWays") {
        return format!("{err}; use --type node to export nodes only").into();
    }
    Box::new(err)
}

#[allow(clippy::too_many_arguments)]
fn run_export(
    file: &std::path::Path,
    output: Option<&std::path::Path>,
    format: ExportFormatArg,
    type_filter: Option<ExportTypeArg>,
    expressions_file: Option<&std::path::Path>,
    expressions: &[String],
    properties: Option<Vec<String>>,
    bbox: Option<&str>,
    metadata: bool,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let expressions = combine_expressions(expressions_file, expressions)?;
    let format = match format {
        ExportFormatArg::Geojsonseq => pbfhogg::export::ExportFormat::GeoJsonSeq,
        ExportFormatArg::Geojson => pbfhogg::export::ExportFormat::GeoJson,
    };
    let types = match type_filter {
        None => pbfhogg::export::ExportTypes::All,
        Some(ExportTypeArg::Node) => pbfhogg::export::ExportTypes::NodesOnly,
        Some(ExportTypeArg::Way) => pbfhogg::export::ExportTypes::WaysOnly,
    };
    let options = pbfhogg::export::ExportOptions::new(
        format,
        types,
        &expressions,
        properties,
        bbox,
        metadata,
    )?;

    if let Some(output) = output {
        if paths_alias(file, output)? {
            return Err(format!(
                "export output '{}' aliases input '{}'",
                output.display(),
                file.display()
            )
            .into());
        }
        // Write through a scratch sibling: a mid-run failure must not
        // truncate or delete a pre-existing file the user named as output.
        let out = pbfhogg::path_guard::OutputFile::new(output)?;
        let file_out = std::fs::File::create(out.path())?;
        run_export_to(file, std::io::BufWriter::new(file_out), &options, host)?;
        out.commit()?;
    } else {
        // `export` flushes its writer when it finishes, so a failed final
        // flush surfaces as an error rather than vanishing in a drop.
        run_export_to(file, output::buffered(), &options, host)?;
    }
    Ok(())
}

fn resolve_ids(
    id_file: Option<&std::path::Path>,
    id_osm_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
    direct_io: bool,
    host: &pbfhogg::host::Host,
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
        let pbf_ids = pbfhogg::getid::parse_ids_from_pbf(path, direct_io, host)?;
        pbfhogg::getid::merge_id_sets(&mut id_set, &pbf_ids);
    }
    if id_set.is_empty() {
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

/// Report the requested IDs that never made it into the output.
///
/// `found` comes from `getid`'s own write path. This used to decode the
/// finished output PBF a second time, which roughly doubled the
/// command's runtime at planet scale for what is only a diagnostic.
fn print_missing_ids(requested: &pbfhogg::getid::ElementIds, found: &pbfhogg::getid::ElementIds) {
    let missing_nodes: Vec<_> = requested
        .node_ids
        .iter()
        .filter(|&id| !found.node_ids.get(id))
        .collect();
    let missing_ways: Vec<_> = requested
        .way_ids
        .iter()
        .filter(|&id| !found.way_ids.get(id))
        .collect();
    let missing_rels: Vec<_> = requested
        .relation_ids
        .iter()
        .filter(|&id| !found.relation_ids.get(id))
        .collect();

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
}

#[allow(clippy::too_many_arguments)]
fn run_diff(
    old: &std::path::Path,
    new: &std::path::Path,
    flags: DiffFlags,
    output_path: Option<&std::path::Path>,
    type_filter: Option<TypeFilter>,
    jobs: usize,
    direct_io: bool,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let DiffFlags {
        suppress_common,
        verbose,
        osmium_summary,
        quiet,
    } = flags;

    let jobs_resolved = if jobs == 0 {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
    } else {
        jobs
    };

    let options = pbfhogg::diff::DiffOptions {
        suppress_common,
        verbose,
        summary: osmium_summary,
        type_filter,
        jobs: jobs_resolved,
    };
    let stats = if quiet {
        let mut sink = std::io::sink();
        pbfhogg::diff::diff(old, new, &mut sink, &options, direct_io, host)?
    } else if let Some(path) = output_path {
        let file = std::fs::File::create(path)?;
        let mut out = std::io::BufWriter::new(file);
        let stats = pbfhogg::diff::diff(old, new, &mut out, &options, direct_io, host)?;
        out.flush()?;
        stats
    } else {
        // A reader that leaves early stops the diff here, before its verdict
        // exists, so the run ends with success rather than with the
        // differences status `main` cannot know.
        let mut out = output::buffered();
        let stats = pbfhogg::diff::diff(old, new, &mut out, &options, direct_io, host)?;
        out.flush()?;
        stats
    };
    if !quiet {
        if osmium_summary {
            stats.write_osmium_summary(
                &mut host.diagnostics.summary_writer("diff.osmium_summary"),
            )?;
        } else {
            stats.write_summary(&mut host.diagnostics.summary_writer("diff.summary"))?;
        }
    }
    if stats.has_differences() {
        return Err(Box::new(ExitWithCode(ExitStatus::AnswerNegative)));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_derive_changes(
    old: &std::path::Path,
    new: &std::path::Path,
    output: &std::path::Path,
    direct_io: bool,
    increment_version: bool,
    update_timestamp: bool,
    jobs: usize,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let jobs_resolved = if jobs == 0 {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
    } else {
        jobs
    };
    let stats = pbfhogg::diff::derive::derive_changes(
        old,
        new,
        output,
        direct_io,
        increment_version,
        update_timestamp,
        jobs_resolved,
        host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("derive.summary"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_getid(
    file: &std::path::Path,
    output: &std::path::Path,
    flags: GetidFlags,
    id_file: Option<&std::path::Path>,
    id_osm_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let GetidFlags {
        add_referenced,
        remove_tags,
        verbose_ids,
    } = flags;
    let id_set = resolve_ids(
        id_file,
        id_osm_file,
        default_type,
        ids,
        write.direct_io,
        host,
    )?;

    if remove_tags && !add_referenced {
        eprintln!(
            "Warning! Without -r/--add-referenced use of -t/--remove-tags isn't doing anything."
        );
    }

    if verbose_ids {
        print_requested_ids(&id_set);
    }

    let opts = pbfhogg::getid::GetidOptions {
        add_referenced,
        remove_tags,
        report_found: verbose_ids,
    };
    let stats = pbfhogg::getid::getid(
        file,
        output,
        &id_set,
        &opts,
        write.compression,
        write.direct_io,
        write.force,
        overrides,
        host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("getid.summary"))?;

    if let Some(found) = stats.found.as_ref() {
        print_missing_ids(&id_set, found);
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
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    full_scan_min_blobs: Option<u64>,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let id_set = resolve_ids(
        id_file,
        id_osm_file,
        default_type,
        ids,
        write.direct_io,
        host,
    )?;
    let opts = pbfhogg::getparents::GetparentsOptions { add_self };
    let stats = if let Some(min_blobs) = full_scan_min_blobs {
        pbfhogg::getparents::getparents_with_min_blobs(
            file,
            output,
            &id_set,
            &opts,
            write.compression,
            write.direct_io,
            overrides,
            min_blobs,
            host,
        )?
    } else {
        pbfhogg::getparents::getparents(
            file,
            output,
            &id_set,
            &opts,
            write.compression,
            write.direct_io,
            overrides,
            host,
        )?
    };
    stats.write_summary(&mut host.diagnostics.summary_writer("getparents.summary"))?;
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
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let id_set = resolve_ids(
        id_file,
        id_osm_file,
        default_type,
        ids,
        write.direct_io,
        host,
    )?;
    let stats = pbfhogg::getid::removeid(
        file,
        output,
        &id_set,
        write.compression,
        write.direct_io,
        write.force,
        overrides,
        host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("getid.summary"))?;
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
    clean: &CleanAttrs,
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
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
    let stats = pbfhogg::extract::extract(
        file,
        output,
        &region,
        strategy,
        set_bounds,
        clean,
        write.compression,
        write.direct_io,
        write.force,
        overrides,
        host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("extract.summary"))?;
    Ok(())
}

/// Check every destination a multi-output command is about to publish
/// *before* the first of them is published.
///
/// `OutputFile::commit` renames a scratch sibling over the destination.
/// Renaming over a directory fails, as does renaming into a directory that
/// no longer exists - and once one rename has succeeded, a later failure
/// leaves the output set half replaced. Running these checks over the whole
/// set first turns the common causes into a clean refusal that publishes
/// nothing. It is a narrowing, not a guarantee: see the call site.
///
/// Write access to the parent is not probed here; it is already proven by
/// the scratch file the run wrote into that same directory.
fn preflight_publishable(paths: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    for path in paths {
        let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let parent = if parent.as_os_str().is_empty() {
            std::path::Path::new(".")
        } else {
            parent
        };
        if !parent.is_dir() {
            return Err(format!(
                "cannot publish '{}': directory '{}' does not exist",
                path.display(),
                parent.display()
            )
            .into());
        }
        if path.is_dir() {
            return Err(format!(
                "cannot publish '{}': it is an existing directory",
                path.display()
            )
            .into());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_extract_config(
    file: &std::path::Path,
    config_path: &std::path::Path,
    directory_override: Option<&std::path::Path>,
    strategy: pbfhogg::extract::ExtractStrategy,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let (config_dir, mut slots) = pbfhogg::extract::parse_extract_config(config_path)?;

    // If -d/--directory is given on CLI, override the config's directory
    if let Some(dir) = directory_override {
        let config_parent = config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let old_dir = config_dir.as_deref().unwrap_or(config_parent);
        for slot in &mut slots {
            // Re-resolve output relative to new directory
            if let Ok(relative) = slot.output.strip_prefix(old_dir) {
                slot.output = dir.join(relative);
            }
        }
    }

    for slot in &slots {
        reject_aliases(&[file], &slot.output)?;
    }
    // Two config slots resolving to the same file (identical path, a symlink,
    // or a hardlink) would open two independent writers on it: the second
    // File::create truncates what the first is producing and the two streams
    // interleave. Reject the collision before any writer is opened.
    for (i, slot) in slots.iter().enumerate() {
        for other in &slots[i + 1..] {
            if outputs_alias(&slot.output, &other.output)? {
                return Err(format!(
                    "extract outputs '{}' and '{}' resolve to the same file",
                    slot.output.display(),
                    other.output.display()
                )
                .into());
            }
        }
    }

    // Guard each destination and redirect the slot at its scratch sibling.
    // Order matters: both alias checks above ran on the *final* paths, which
    // is the only place the user's collision can be seen - two scratch names
    // never collide, so checking them would silently pass a config that
    // writes two extracts to one file.
    let final_paths: Vec<PathBuf> = slots.iter().map(|slot| slot.output.clone()).collect();
    preflight_publishable(&final_paths)?;
    let guards: Vec<pbfhogg::path_guard::OutputFile> = final_paths
        .iter()
        .map(|path| pbfhogg::path_guard::OutputFile::new(path))
        .collect::<Result<_, _>>()?;
    for (slot, guard) in slots.iter_mut().zip(&guards) {
        slot.output = guard.path().to_path_buf();
    }

    eprintln!("Multi-extract: {} extracts from config", slots.len());
    let all_stats = pbfhogg::extract::extract_multi(
        file,
        &slots,
        strategy,
        set_bounds,
        clean,
        write.compression,
        write.direct_io,
        write.force,
        overrides,
        host,
    )?;
    // Extraction is all-or-nothing: a failure above drops every guard and no
    // destination was ever opened, so nothing is published at all. What
    // follows is *not* a transaction. Publication is a preflighted sequence
    // of renames: the preflight is repeated here, immediately before the
    // first rename, so the ordinary reasons a rename fails - a destination
    // that is a directory, a parent directory that vanished - become a
    // refusal with nothing published rather than a partial set. It cannot be
    // made airtight: another process may still change a destination between
    // this check and the rename that reaches it, and if a rename fails after
    // an earlier one succeeded the output set is partial. POSIX offers no
    // N-way atomic rename, and the rollback log that would close the gap is
    // out of proportion to this command.
    preflight_publishable(&final_paths)?;
    for guard in guards {
        guard.commit()?;
    }
    for (i, stats) in all_stats.iter().enumerate() {
        eprint!("  [{}] {} - ", i + 1, final_paths[i].display());
        stats.write_summary(&mut host.diagnostics.summary_writer("extract.summary"))?;
    }
    Ok(())
}

fn run_add_locations_to_ways(
    file: &std::path::Path,
    output: &std::path::Path,
    options: &pbfhogg::altw::AltwOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::altw::add_locations_to_ways(file, output, options, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("altw.summary"))?;
    Ok(())
}

fn run_time_filter(
    file: &std::path::Path,
    output: &std::path::Path,
    timestamp: &str,
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let cutoff = parse_timestamp(timestamp)?;
    let stats = pbfhogg::time_filter::time_filter(
        file,
        output,
        cutoff,
        write.compression,
        write.direct_io,
        overrides,
        host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("time_filter.summary"))?;
    Ok(())
}

fn parse_timestamp(input: &str) -> Result<i64, Box<dyn std::error::Error>> {
    if let Ok(value) = input.parse::<i64>() {
        return Ok(value);
    }
    pbfhogg::commands::parse_rfc3339_utc(input).map_err(|e| format!("{e} (or UNIX seconds)").into())
}

fn run_show_element(
    path: &std::path::Path,
    spec: &str,
    direct_io: bool,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let (elem_type, id) = parse_element_spec(spec)?;
    let mut out = output::buffered();
    let found = pbfhogg::inspect::show_element_to(path, elem_type, id, direct_io, &mut out, host)?;
    out.flush()?;
    if !found {
        eprintln!("Element not found: {spec}");
        return Err(Box::new(ExitWithCode(ExitStatus::AnswerNegative)));
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

/// Which of `run_inspect`'s four execution shapes a flag combination
/// resolves to, decided once up front.
///
/// The distinction is *alone vs combined*, and it is a live one: clap
/// rejects `--show` alongside anything and `-j`/`--force` outside
/// `--nodes`, but `--indexed --extended` and friends remain legal - a
/// combined run does the full scan and folds the extra reports in, while
/// a flag alone gets its fast path that skips the scan entirely.
enum InspectMode<'a> {
    /// `--indexed` alone: header probe only, no full inspect.
    IndexedOnly,
    /// `--nodes` alone: node stats only, no full inspect.
    NodesOnly,
    /// `--get` with a header/file key and nothing else: read just the
    /// header blob.
    HeaderGet(&'a str),
    /// Everything else: full scan, with any requested extras folded in.
    Full,
}

impl<'a> InspectMode<'a> {
    fn resolve(flags: &InspectFlags, blocks: Option<usize>, get: Option<&'a str>) -> Self {
        let scan_extras = flags.extended
            || flags.id_ranges
            || flags.locations
            || flags.anomalies
            || blocks.is_some();
        if flags.indexed && !scan_extras && !flags.nodes && get.is_none() {
            return Self::IndexedOnly;
        }
        if flags.nodes && !scan_extras && !flags.indexed && get.is_none() {
            return Self::NodesOnly;
        }
        if let Some(key) = get {
            let needs_full_scan = key.starts_with("data.")
                || key.starts_with("metadata.")
                || key.starts_with("elements.")
                || key.starts_with("blocks.")
                || key == "indexed";
            if !needs_full_scan && !scan_extras && !flags.nodes && !flags.indexed {
                return Self::HeaderGet(key);
            }
        }
        Self::Full
    }
}

#[allow(clippy::too_many_lines)]
fn run_inspect(
    path: &std::path::Path,
    flags: InspectFlags,
    jobs: usize,
    blocks: Option<usize>,
    get: Option<&str>,
    direct_io: bool,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let mode = InspectMode::resolve(&flags, blocks, get);
    let InspectFlags {
        indexed,
        nodes,
        id_ranges,
        locations,
        anomalies,
        extended,
        json,
        force,
        distrust_metadata,
    } = flags;

    // `--distrust-metadata` only means something for the scanning report:
    // `--indexed` asks whether indexdata is present (distrusting its contents
    // does not change the answer), `--nodes` decodes every node already, and
    // `--get header.*` reads the header blob directly. Rejecting says so
    // rather than accepting a flag that would do nothing.
    if distrust_metadata && !matches!(mode, InspectMode::Full) {
        return Err(
            "--distrust-metadata applies to the scanning report only, not to --indexed, --nodes or a header --get key"
                .into(),
        );
    }

    // JSON pairings that have no output shape, rejected up front for every
    // mode. (`--indexed --json` alone is fine and handled below.)
    if nodes && json {
        return Err(
            "--nodes and --json cannot be used together (node stats have no JSON output)".into(),
        );
    }
    if get.is_some() && json {
        return Err("--get and --json cannot be used together".into());
    }

    match mode {
        InspectMode::IndexedOnly => {
            let has_index = pbfhogg::has_indexdata(path, direct_io, &host.diagnostics)?;
            let mut out = output::buffered();
            let printed = if json {
                writeln!(out, "{}", serde_json::json!({"indexed": has_index}))
            } else if has_index {
                writeln!(out, "Indexed: yes")
            } else {
                writeln!(out, "Indexed: no")
            };
            // The answer is the exit status; a reader that left early must
            // not cost it.
            output::tolerate_reader_gone(printed.and_then(|()| out.flush()))?;
            if !has_index {
                return Err(Box::new(ExitWithCode(ExitStatus::AnswerNegative)));
            }
            return Ok(());
        }
        InspectMode::NodesOnly => {
            let report =
                pbfhogg::inspect::node_stats::node_stats(path, direct_io, force, jobs, host)?;
            let mut out = output::buffered();
            report.write_report(&mut out)?;
            out.flush()?;
            return Ok(());
        }
        InspectMode::HeaderGet(key) => {
            let mut reader =
                pbfhogg::BlobReader::open(path, direct_io)?.diagnostics(host.diagnostics.clone());
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
                "header.bbox" => header
                    .bbox()
                    .map(|bb| format!("{} {} {} {}", bb.left, bb.bottom, bb.right, bb.top)),
                "header.writing_program" => header.writing_program().map(String::from),
                "header.replication.url" => header.osmosis_replication_base_url().map(String::from),
                "header.replication.sequence" => header
                    .osmosis_replication_sequence_number()
                    .map(|s| s.to_string()),
                "header.replication.timestamp" => header
                    .osmosis_replication_timestamp()
                    .map(|t| t.to_string()),
                _ => return Err(format!("unknown key: {key}").into()),
            };
            let Some(v) = val else {
                return Err(format!("key {key} has no value in this file").into());
            };
            let mut out = output::buffered();
            writeln!(out, "{v}")?;
            out.flush()?;
            return Ok(());
        }
        InspectMode::Full => {}
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
    let mut report = pbfhogg::inspect::inspect(
        path,
        &pbfhogg::inspect::InspectOptions {
            show_blocks,
            show_id_ranges: id_ranges,
            show_locations: locations,
            extended,
            distrust_metadata,
            direct_io,
        },
        host,
    )?;

    // --indexed combined with other flags: check and report, set exit code
    let has_index = if indexed {
        Some(pbfhogg::has_indexdata(path, direct_io, &host.diagnostics)?)
    } else {
        None
    };
    let exit_not_indexed = has_index == Some(false);

    // --nodes combined with other flags: run node stats, printed below
    let node_report = if nodes {
        Some(pbfhogg::inspect::node_stats::node_stats(
            path, direct_io, force, jobs, host,
        )?)
    } else {
        None
    };

    // Everything is computed; only output remains. `--indexed` makes the
    // exit status an answer, and a reader that left early must not cost it.
    let printed = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut out = output::buffered();
        if let Some(has_index) = has_index
            && !json
        {
            writeln!(out, "Indexed: {}", if has_index { "yes" } else { "no" })?;
        }
        if let Some(ref node_report) = node_report {
            node_report.write_report(&mut out)?;
        }
        if let Some(key) = get {
            match report.get_value(key) {
                Some(val) => writeln!(out, "{val}")?,
                None => return Err(format!("unknown key: {key}").into()),
            }
        } else if json {
            let mut value = report.to_json_filtered(block_limit, anomalies);
            if indexed && let serde_json::Value::Object(ref mut map) = value {
                map.insert(
                    "indexed".to_string(),
                    serde_json::Value::Bool(!exit_not_indexed),
                );
            }
            writeln!(out, "{value}")?;
        } else {
            report.write_report_filtered(&mut out, block_limit, anomalies)?;
        }
        out.flush()?;
        Ok(())
    })();
    output::tolerate_reader_gone(printed)?;

    if exit_not_indexed {
        return Err(Box::new(ExitWithCode(ExitStatus::AnswerNegative)));
    }
    Ok(())
}

fn run_apply_changes(
    base: &std::path::Path,
    changes: &std::path::Path,
    output: &std::path::Path,
    opts: &pbfhogg::apply_changes::MergeOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::apply_changes::merge(base, changes, output, opts, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("apply_changes.summary"))?;
    Ok(())
}

fn run_merge_pbf(
    inputs: &[PathBuf],
    output: &std::path::Path,
    write: &WriteOpts,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<&std::path::Path> = inputs.iter().map(AsRef::as_ref).collect();
    let opts = pbfhogg::cat::dedupe::MergePbfOptions {
        compression: write.compression,
        direct_io: write.direct_io,
        io_uring: write.io_uring,
        force: write.force,
    };
    let stats = pbfhogg::cat::dedupe::merge_pbf(&paths, output, &opts, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("cat.dedupe.summary"))?;
    Ok(())
}

fn run_merge_changes(
    changes: &[PathBuf],
    output: &std::path::Path,
    simplify: bool,
    jobs: usize,
    allow_version_regression: bool,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let inputs: Vec<&std::path::Path> = changes.iter().map(AsRef::as_ref).collect();
    let jobs = if jobs == 0 { None } else { Some(jobs) };
    let stats = pbfhogg::merge_changes::merge_changes(
        &inputs,
        output,
        simplify,
        jobs,
        allow_version_regression,
        host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("merge_changes.summary"))?;
    Ok(())
}

fn run_bench_read(
    path: &std::path::Path,
    mode: BenchReadMode,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Instant;

    let (elapsed_ms, nodes, ways, rels): (u128, u64, u64, u64) = match mode {
        BenchReadMode::Sequential => {
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
        BenchReadMode::Parallel => {
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
        BenchReadMode::Pipelined => {
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
        BenchReadMode::Blobreader => {
            // Route through the library's buffered path (256 KiB FileReader)
            // so this arm measures decode architecture, not the syscall
            // overhead of a default 8 KiB BufReader.
            let reader = pbfhogg::BlobReader::from_path(path)?;
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
                        if !bb.can_add_node()
                            && let Some(bytes) = bb.take()?
                        {
                            writer.write_primitive_block(bytes)?;
                        }
                        bb.add_node(
                            dn.id(),
                            dn.decimicro_lat(),
                            dn.decimicro_lon(),
                            dn.tags(),
                            None,
                        );
                        nodes += 1;
                    }
                    pbfhogg::Element::Node(n) => {
                        if !bb.can_add_node()
                            && let Some(bytes) = bb.take()?
                        {
                            writer.write_primitive_block(bytes)?;
                        }
                        bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), None);
                        nodes += 1;
                    }
                    pbfhogg::Element::Way(w) => {
                        if !bb.can_add_way()
                            && let Some(bytes) = bb.take()?
                        {
                            writer.write_primitive_block(bytes)?;
                        }
                        let refs: Vec<_> = w.refs().collect();
                        bb.add_way(w.id(), w.tags(), &refs, None);
                        ways += 1;
                    }
                    pbfhogg::Element::Relation(r) => {
                        if !bb.can_add_relation()
                            && let Some(bytes) = bb.take()?
                        {
                            writer.write_primitive_block(bytes)?;
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
    compression: Compression,
    writer_mode: BenchWriterMode,
) -> Result<(), Box<dyn std::error::Error>> {
    use pbfhogg::block_builder::HeaderBuilder;
    use std::time::Instant;

    let header_bytes = HeaderBuilder::new().build()?;

    let (elapsed_ms, nodes, ways, rels) = match writer_mode {
        BenchWriterMode::Sync => {
            let file = std::fs::File::create("/dev/null")?;
            let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
            let mut writer = pbfhogg::writer::PbfWriter::new(buf, compression);
            writer.write_header(&header_bytes)?;
            let start = Instant::now();
            let (nodes, ways, rels) = bench_write_loop(path, &mut writer)?;
            let elapsed_ms = start.elapsed().as_millis();
            (elapsed_ms, nodes, ways, rels)
        }
        BenchWriterMode::Pipelined => {
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
    compression: Compression,
    io_mode: BenchIoMode,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Instant;

    let io_uring = matches!(io_mode, BenchIoMode::Uring);

    drop(std::fs::remove_file(output));

    let start = Instant::now();
    let opts = pbfhogg::apply_changes::MergeOptions {
        compression,
        direct_io: false,
        io_uring,
        force: true,
        locations_on_ways: false,
        ignore_missing_nodes: false,
        jobs: None,
        allow_version_regression: false,
        #[cfg(feature = "test-hooks")]
        panic_at_blob_seq: None,
    };
    let stats = pbfhogg::apply_changes::merge(
        base,
        changes,
        output,
        &opts,
        &HeaderOverrides::default(),
        host,
    )?;
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
