// Every file the CLI writes is a `pbfhogg::Output`: checked, staged and
// published by the library. clippy.toml lists the path-writing methods this
// refuses.
#![deny(clippy::disallowed_methods)]

mod exit_code;
mod host;
mod output;
mod signals;

use exit_code::{Hinted, error_report, is_cancelled, is_platform_unsupported, output_hint};

use std::io::Write;
use std::path::PathBuf;
use std::process;

use clap::{Args, Parser, Subcommand, ValueEnum};
use pbfhogg::ElementKind;
use pbfhogg::altw::IndexType;
use pbfhogg::cat::CleanAttrs;
use pbfhogg::geocode_index::builder::BuildConfig as GeocodeBuildConfig;
use pbfhogg::getid::ElementRef;
use pbfhogg::tags_count::TagCountSort;
use pbfhogg::write::{OutputBackend, WriteOptions};
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

/// The failure exit statuses `pbfhogg` guarantees to callers. `0` and `1`
/// are a completed command's answer and travel as [`Outcome`], never through
/// the error channel.
///
/// The split follows the `diff`/`grep` convention: a command that ran to
/// completion and answered "no" is not the same event as a command that
/// could not run at all. Scripts can therefore branch on the answer
/// (`1`) without swallowing failures (`2`), which the previous
/// everything-is-`1` scheme made impossible.
///
/// `2` also matches what clap already returns for a usage error, so the
/// "the invocation did not work" band is contiguous.
///
/// `3` splits one cause out of that band: the host cannot provide a
/// requested I/O feature (`--direct-io`, `--io-uring`) at setup. Retrying
/// without the flag is the fix, which no other failure promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitStatus {
    /// `2` - the command failed. Every error path but one lands here.
    Failed,
    /// `3` - the command failed because a requested platform I/O feature
    /// is unavailable at setup (`ErrorKind::PlatformUnsupported`). The
    /// error, naming the feature, goes to stderr as for `2`.
    PlatformUnsupported,
}

impl ExitStatus {
    const fn code(self) -> u8 {
        match self {
            Self::Failed => 2,
            Self::PlatformUnsupported => 3,
        }
    }
}

/// A command's completed answer, carried separately from errors so a
/// negative answer cannot be mistaken for a failed invocation: no wrapper
/// that re-boxes or maps an error can turn it into exit `2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// `0` - the command ran to completion and its answer is positive (or
    /// the command has no yes/no answer).
    Positive,
    /// `1` - the command ran to completion and its answer is negative:
    /// `diff` found differences, `check` found violations,
    /// `inspect --indexed` found no index, `inspect --show` found no
    /// element. Not an error; nothing is printed to stderr for it.
    Negative,
}

impl Outcome {
    const fn code(self) -> u8 {
        match self {
            Self::Positive => 0,
            Self::Negative => 1,
        }
    }
}

fn accept_outcome(
    overall: &mut Outcome,
    result: Result<Outcome, Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if result? == Outcome::Negative {
        *overall = Outcome::Negative;
    }
    Ok(())
}

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
    #[command(flatten)]
    overwrite: OverwriteArg,
}

impl OutputArg {
    /// The output, checked against the output rules and staged, before any
    /// work is done. Cleanup trouble after a successful publication goes to
    /// the host's diagnostics.
    fn open(
        &self,
        host: &pbfhogg::host::Host,
    ) -> Result<pbfhogg::Output, Box<dyn std::error::Error>> {
        Ok(pbfhogg::Output::new(
            &self.output,
            self.overwrite.intent(),
            &host.diagnostics,
        )?)
    }
}

/// Where a command that needs scratch space puts it.
#[derive(Args)]
struct TmpDirArg {
    /// Directory for this run's scratch files, which can be large (add-locations-to-ways
    /// needs up to hundreds of GB with `--index-type external`; parallel diff writes a copy of
    /// the changed text). The directory must exist and be writable; it is checked before any
    /// work starts. Each run makes one hidden `.pbfhogg-run-*` directory in it and removes it
    /// at the end. Default: the output's directory, or the current directory when there is
    /// no output file. Output staging files stay beside the output regardless.
    #[arg(long = "tmp-dir", value_name = "DIR")]
    tmp_dir: Option<PathBuf>,
}

#[derive(Args)]
struct OverwriteArg {
    /// Replace an output that already exists. Without it an existing output
    /// is refused before any work is done. A symlink is refused either way.
    #[arg(short = 'O', long)]
    overwrite: bool,
}

impl OverwriteArg {
    const fn intent(&self) -> pbfhogg::Overwrite {
        pbfhogg::Overwrite::from_flag(self.overwrite)
    }
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
    #[arg(
        long = "output-header",
        value_name = "KEY=VALUE",
        help = "Set output header fields.",
        long_help = pbfhogg::HeaderOverrides::output_header_help()
    )]
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

impl From<DefaultTypeArg> for ElementKind {
    fn from(arg: DefaultTypeArg) -> Self {
        match arg {
            DefaultTypeArg::Node => Self::Node,
            DefaultTypeArg::Way => Self::Way,
            DefaultTypeArg::Relation => Self::Relation,
        }
    }
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

impl From<ExportTypeArg> for pbfhogg::export::ExportTypes {
    fn from(arg: ExportTypeArg) -> Self {
        match arg {
            ExportTypeArg::Node => Self::NodesOnly,
            ExportTypeArg::Way => Self::WaysOnly,
        }
    }
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
/// at `sparse` instead of a bare "invalid value". Repository search cannot
/// establish whether downstream command lines still use it.
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
            Self::Dense => "dense".parse::<IndexType>().map_err(|error| {
                format!("{error} For this command, pass --index-type sparse.").into()
            }),
        }
    }
}

/// `count` and `name` were accepted as shorthands by the hand-written
/// parser this replaced. They stay as hidden aliases because repository
/// search cannot establish whether downstream command lines still use them.
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
        /// Per-blob element cap. The default matches the PBF interop
        /// convention; passing a small cap against a `planet.openstreetmap.org`-
        /// packed input (~228 k/blob) produces a Geofabrik-style packing.
        #[arg(
            long = "elements-per-blob",
            default_value_t = pbfhogg::block_builder::DEFAULT_ELEMENT_CAP
        )]
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
    /// At least one transformation flag is required. Flags compose, except
    /// `--unsort` and `--unsort-intra`, which are mutually exclusive.
    /// `--unsort` swaps elements across a block boundary when a kind has
    /// more than the block cap elements. `--unsort-intra` swaps the first
    /// two elements of each kind inside a block. Smaller kinds pass through.
    /// Used to produce inputs for benchmarking non-optimal code paths.
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
        /// without large fixtures per element kind.
        #[arg(
            long = "block-cap",
            default_value_t = pbfhogg::block_builder::DEFAULT_ELEMENT_CAP,
            hide = true
        )]
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
        // Deliberately keep the extension-default help contract alongside the
        // four-byte sniff. Promising stricter content detection misstates fallback.
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
        #[command(flatten)]
        overwrite: OverwriteArg,
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
        #[command(flatten)]
        overwrite: OverwriteArg,
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
        tmp: TmpDirArg,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Print a digest of a PBF's element content.
    ///
    /// Two files get the same digest exactly when `diff --suppress-common`
    /// reports them identical, whatever their encoding (compression, blob
    /// boundaries, dense or plain nodes, indexdata). Prints `xxh128 v1 HEX`,
    /// then `node|way|relation COUNT HEX` per kind. Refuses, printing
    /// nothing, any input on which diff's answer is not a function of
    /// element content: unsorted, out-of-order or duplicate IDs, mixed
    /// blocks, partial or inaccurate indexdata, unresolvable string
    /// references.
    Digest {
        /// Input PBF file
        file: PathBuf,
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
        /// Read IDs from text file (one per line, e.g. n123 or node/123)
        #[arg(short = 'i', long = "id-file")]
        id_file: Option<PathBuf>,
        /// Read IDs from an OSM/PBF file (all element IDs are collected)
        #[arg(short = 'I', long = "id-osm-file")]
        id_osm_file: Option<PathBuf>,
        /// Type for bare numeric IDs. Without this, each ID must name its type.
        #[arg(long = "default-type", value_enum)]
        default_type: Option<DefaultTypeArg>,
        /// Element IDs (e.g. n123 w456 r789, or node/123 way/456 relation/789)
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
        /// Read IDs from text file (one per line, e.g. n123 or node/123)
        #[arg(short = 'i', long = "id-file")]
        id_file: Option<PathBuf>,
        /// Read IDs from an OSM/PBF file (all element IDs are collected)
        #[arg(short = 'I', long = "id-osm-file")]
        id_osm_file: Option<PathBuf>,
        /// Type for bare numeric IDs. Without this, each ID must name its type.
        #[arg(long = "default-type", value_enum)]
        default_type: Option<DefaultTypeArg>,
        /// Element IDs (e.g. n123 w456 r789, or node/123 way/456 relation/789)
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
        /// Replace outputs that already exist, including every output a
        /// --config names. Without it any existing output is refused before
        /// any work is done. A symlink is refused either way.
        #[arg(short = 'O', long)]
        overwrite: bool,
        /// Bounding box: minlon,minlat,maxlon,maxlat
        #[arg(
            short = 'b',
            long,
            group = "area",
            required_unless_present_any = ["polygon", "config"],
            conflicts_with = "config"
        )]
        bbox: Option<String>,
        /// Polygon GeoJSON file
        #[arg(short = 'p', long, group = "area", conflicts_with = "config")]
        polygon: Option<PathBuf>,
        /// Multi-extract JSON config file
        #[arg(short = 'c', long, conflicts_with_all = ["bbox", "polygon", "output"])]
        config: Option<PathBuf>,
        /// Rebase config outputs under this directory (only with --config)
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
        ///   auto     - route by input eligibility and estimated node-store size:
        ///              unsorted or unindexed input uses sparse; sorted, indexed
        ///              input uses external when the estimate exceeds the
        ///              fixed share of the page-cache budget. If either
        ///              estimate is unavailable, sorted, indexed input uses
        ///              external. See reference/pipeline.md.
        ///   sparse   - rank-indexed flat mmap array, sized at 8 bytes per
        ///              referenced node. Works on any PBF and needs temporary
        ///              disk equal to referenced_nodes * 8 bytes. This is the default.
        ///   external - double radix join with bounded working memory and
        ///              sequential I/O. Requires sorted input with indexdata and
        ///              has high temporary-disk demand. See reference/performance.md.
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
        #[command(flatten)]
        tmp: TmpDirArg,
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
    #[command(subcommand_negates_reqs = true, args_conflicts_with_subcommands = true)]
    Inspect {
        #[command(subcommand)]
        subcommand: Option<InspectCommand>,
        /// Input PBF file
        #[arg(required = true)]
        file: Option<PathBuf>,
        /// Check if PBF has blob-level indexdata (exit code 0/1). Reads the
        /// first data blob only; later blobs are not checked.
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
        #[arg(
            long,
            help = "Show blocks outside inspect's per-type median thresholds, plus mixed blocks"
        )]
        anomalies: bool,
        /// Extended scan: timestamp range, data bbox, metadata coverage, ordering
        #[arg(short, long)]
        extended: bool,
        /// Show a single element by type/ID (e.g. "n123" or "node/123").
        ///
        /// A mode of its own: it returns one element and nothing else, so
        /// every other inspect flag is rejected alongside it rather than
        /// silently discarded (which is what used to happen).
        #[arg(
            long,
            value_name = "TYPE_ID",
            value_parser = clap::value_parser!(ElementRef),
            conflicts_with_all = [
                "indexed", "nodes", "jobs", "blocks", "id_ranges", "locations",
                "anomalies", "extended", "get", "json", "force",
                "distrust_metadata",
            ],
        )]
        show: Option<ElementRef>,
        /// Get a single value by key path (e.g. "header.bbox", "data.timestamp.first").
        /// Data and metadata paths enable extended stats automatically.
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
        /// Preserve way-node locations through the merge. Ways the OSC
        /// creates or modifies get locations from the merged nodes; base
        /// ways the OSC does not touch keep their existing locations, even
        /// if the OSC moves or deletes a node they reference. Rerun
        /// add-locations-to-ways on the output for a full refresh.
        /// Requires the base PBF to have LocationsOnWays and be sorted.
        #[arg(long)]
        locations_on_ways: bool,
        /// Permit unresolved references to use the legacy (0,0) fallback.
        #[arg(long, requires = "locations_on_ways")]
        ignore_missing_nodes: bool,
        /// Accept a change whose version is lower than the one already
        /// held for that object. Off by default: an out-of-order diff
        /// otherwise downgrades objects to an older state and reports
        /// success. An equal version is always accepted; the later
        /// change wins.
        #[arg(long)]
        allow_version_regression: bool,
        /// Worker-pool size for the descriptor-first pipeline.
        /// `0` (default) uses the `nproc - 2` heuristic (leaves two
        /// cores for the scanner + drain threads, minimum 2). Pass a
        /// specific N >= 2 to measure scaling or constrain CPU use; an
        /// explicit `-j 1` is refused (single-worker operation is not a
        /// validated configuration).
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
        /// Accept an incoming change whose version is lower than the
        /// highest version already claimed for that object, deletes
        /// included. An equal version is always accepted, and the later
        /// change wins. Off by default: --simplify keeps the LAST
        /// change per object by input position, so inputs listed out of
        /// sequence order otherwise downgrade objects to older versions
        /// and report success. Requires --simplify: without it every change
        /// is kept in input order and no version is compared.
        #[arg(long, requires = "simplify")]
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
        /// Retain at most N violation details per check (0 = unlimited);
        /// the scan still runs to the end and the counts stay exact
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
        #[arg(long, default_value = "zlib", value_parser = parse_compression)]
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
        /// S2 cell level for streets/addresses; must exceed --coarse-level
        /// and fit the builder limit
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().street_level
        )]
        street_level: u8,
        /// Fallback cell level for rural areas; must be below --street-level
        /// and fit the builder limit
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().coarse_level
        )]
        coarse_level: u8,
        /// S2 cell level for admin boundaries; the builder enforces its work limit
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().admin_level
        )]
        admin_level: u8,
        /// Douglas-Peucker cap for local admin polygons; levels 5, 4, 3, and 2
        /// retain up to 2x, 4x, 8x, and 16x this value
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().max_admin_vertices
        )]
        max_admin_vertices: u16,
        /// Fine-level max search distance in meters
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().fine_search_radius_m
        )]
        search_radius: f32,
        /// Coarse-level max search distance in meters
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().coarse_search_radius_m
        )]
        coarse_search_radius: f32,
        #[command(flatten)]
        io: DirectIoArg,
        /// Replace an existing index (or empty directory) at --output-dir
        /// after the new build succeeds. Without it an existing --output-dir
        /// is refused before any work is done.
        #[arg(short = 'O', long)]
        overwrite: bool,
        /// Accept input without blob-level indexdata (slower fallback path).
        #[arg(long)]
        allow_unindexed: bool,
        /// Legacy shorthand for both --overwrite and --allow-unindexed.
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        tmp: TmpDirArg,
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

/// `getid`'s three independent booleans: adjacent `bool`s that transpose
/// silently at a call site, so they are written by field name.
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

// Deliberately sniff only four bytes, then use extensions with unknown = PBF.
// Gzip magic means OSC; no BOM/whitespace skipping or format refusal is added.
// Stricter sniffing would break accepted inputs and the extension fallback contract.
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
    // First, before any thread exists: SIGINT and SIGTERM become a
    // cancellation request (see `signals`). Every thread started from here on
    // inherits the blocked mask.
    let cancel = pbfhogg::host::Cancel::new();
    if let Err(e) = signals::install(cancel.clone()) {
        host::cli_warning(
            "cli.signals",
            format_args!(
                "cannot take SIGINT and SIGTERM ({e}); an interrupted run will not clean up \
                 its staging and scratch files"
            ),
        );
    }
    #[cfg(feature = "test-hooks")]
    signals::test_hooks::arm(&cancel);
    host::init_marker_fifo();
    let host = host::cli_host(cancel);
    let _guard = hotpath::HotpathGuardBuilder::new("pbfhogg::main")
        .percentiles(&[50.0, 95.0, 99.0])
        .functions_limit(0)
        .build();

    // `Cli::parse()` ends a parse error - and `--help` / `--version` - with
    // `clap::Error::exit`, which calls `process::exit` and so skips every
    // destructor, including the hotpath guard built two lines up. Use
    // `try_parse` and return instead so the guard is dropped normally.
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

    let mut outcome = Outcome::Positive;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        validate_input_output_aliases(&cli.command)?;
        let mut leftovers = LeftoverScanner::default();
        leftovers.command(&cli.command, &host);
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
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, uring.io_uring),
                    direct_reads: io.direct_io,
                    force: force.force,
                };
                if dedupe {
                    if type_filter.is_some() {
                        return Err("--type is not valid with --dedupe".into());
                    }
                    if !clean.is_empty() {
                        return Err("--clean is not valid with --dedupe".into());
                    }
                    run_merge_pbf(&files, output.open(&host)?, &write, &overrides, &host)
                } else {
                    if uring.io_uring {
                        return Err("--io-uring is only valid with --dedupe".into());
                    }
                    run_cat(
                        &files,
                        output.open(&host)?,
                        type_filter,
                        &clean_attrs(&clean),
                        &write,
                        &overrides,
                        &host,
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
            } => {
                let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, uring.io_uring),
                    direct_reads: io.direct_io,
                    force: force.force,
                };
                run_sort(&file, output.open(&host)?, &write, &overrides, &host)
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
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, uring.io_uring),
                    direct_reads: io.direct_io,
                    force: force.force,
                };
                run_repack(
                    &file,
                    output.open(&host)?,
                    elements_per_blob,
                    &write,
                    &overrides,
                    &host,
                )
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
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, uring.io_uring),
                    direct_reads: io.direct_io,
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
                run_degrade(
                    &file,
                    output.open(&host)?,
                    flags,
                    block_cap,
                    &write,
                    &overrides,
                    &host,
                )
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
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force: false,
                };
                run_renumber(
                    &file,
                    output.open(&host)?,
                    &start_id,
                    &write,
                    &overrides,
                    &host,
                )
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
                    // OSC output has no PBF header for these to land in.
                    if header.generator.is_some() || !header.output_headers.is_empty() {
                        return Err(
                            "--generator and --output-header are not valid in OSC mode (OSC output has no PBF header)".into(),
                        );
                    }
                    run_tags_filter_osc(&file, output.open(&host)?, expressions_file.as_deref(), &expressions, &host)
                }
                InputKind::Pbf if omit_referenced && jobs != 0 => {
                    Err("-j/--jobs has no effect with -R/--omit-referenced (single-pass mode uses the pipelined reader, not the parallel classify path). Drop -R or drop -j.".into())
                }
                InputKind::Pbf => {
                    let overrides =
                        HeaderOverrides::parse(header.generator, &header.output_headers)?;
                    let write = WriteOptions {
                        compression: compression.compression,
                        backend: OutputBackend::from_flags(io.direct_io, false),
                        direct_reads: io.direct_io,
                        force: force.force,
                    };
                    let flags = TagsFilterFlags {
                        omit_referenced,
                        invert_match,
                        remove_tags,
                        distrust_metadata: distrust.distrust_metadata,
                    };
                    run_tags_filter(
                        &file,
                        output.open(&host)?,
                        expressions_file.as_deref(),
                        &expressions,
                        flags,
                        &write,
                        jobs,
                        &overrides,
                     &host)
                }
            }
            }
            Command::Export {
                file,
                output,
                overwrite,
                format,
                type_filter,
                expressions_file,
                expressions,
                properties,
                bbox,
                metadata,
            } => run_export(
                &file,
                output
                    .as_deref()
                    .map(|path| pbfhogg::Output::new(path, overwrite.intent(), &host.diagnostics))
                    .transpose()?,
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
                overwrite,
                type_filter,
                jobs,
                format,
                increment_version,
                update_timestamp,
                tmp,
                io,
            } => match format {
                DiffFormat::Osc => {
                    // Deliberately require output before rejecting text-only flags.
                    // Even -q without -o must retain this historical missing-output
                    // error; moving checks changes the user-facing error contract.
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
                    // Deliberately reject quiet OSC after the output requirement.
                    // Allowing it changes the historical CLI error contract.
                    if quiet {
                        return Err("--quiet is not valid with --format osc".into());
                    }
                    if type_filter.is_some() {
                        return Err("--type is not valid with --format osc".into());
                    }
                    let scratch = run_scratch(&tmp, Some(&output), &host)?;
                    run_derive_changes(
                        &old,
                        &new,
                        pbfhogg::Output::new(&output, overwrite.intent(), &host.diagnostics)?,
                        io.direct_io,
                        increment_version,
                        update_timestamp,
                        jobs,
                        &host,
                        scratch,
                    )
                }
                DiffFormat::Text => {
                    if increment_version {
                        return Err("--increment-version is only valid with --format osc".into());
                    }
                    if update_timestamp {
                        return Err("--update-timestamp is only valid with --format osc".into());
                    }
                    // A quiet diff writes no output file, whatever -o says.
                    let scratch = run_scratch(&tmp, output.as_deref().filter(|_| !quiet), &host)?;
                    let flags = DiffFlags {
                        suppress_common,
                        verbose,
                        osmium_summary,
                        quiet,
                    };
                    let output = output
                        .as_deref()
                        .map(|path| {
                            pbfhogg::Output::new(path, overwrite.intent(), &host.diagnostics)
                        })
                        .transpose()?;
                    accept_outcome(
                        &mut outcome,
                        run_diff(
                            &old,
                            &new,
                            flags,
                            output,
                            type_filter,
                            jobs,
                            io.direct_io,
                            &host,
                            scratch,
                        ),
                    )
                }
            },
            Command::Digest { file, io } => {
                let digest = pbfhogg::digest::digest(&file, io.direct_io, &host)?;
                let mut out = output::buffered();
                digest.write_report(&mut out)?;
                out.flush()?;
                Ok(())
            }
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
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force: force.force,
                };
                if invert {
                    run_removeid(
                        &file,
                        output.open(&host)?,
                        id_file.as_deref(),
                        id_osm_file.as_deref(),
                        default_type,
                        &ids,
                        &write,
                        &overrides,
                        &host,
                    )
                } else {
                    let flags = GetidFlags {
                        add_referenced,
                        remove_tags,
                        verbose_ids,
                    };
                    run_getid(
                        &file,
                        output.open(&host)?,
                        flags,
                        id_file.as_deref(),
                        id_osm_file.as_deref(),
                        default_type,
                        &ids,
                        &write,
                        &overrides,
                        &host,
                    )
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
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force: false,
                };
                run_getparents(
                    &file,
                    output.open(&host)?,
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
            }
            Command::Extract {
                file,
                output,
                overwrite,
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
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force: force.force,
                };
                let overwrite = pbfhogg::Overwrite::from_flag(overwrite);
                if let Some(config_path) = config.as_deref() {
                    run_extract_config(
                        &file,
                        config_path,
                        directory.as_deref(),
                        overwrite,
                        extract_strategy(simple, smart),
                        set_bounds,
                        &clean_attrs(&clean),
                        &write,
                        &overrides,
                        &host,
                        &mut leftovers,
                    )
                } else if let Some(output) = output.as_ref() {
                    run_extract(
                        &file,
                        pbfhogg::Output::new(output, overwrite, &host.diagnostics)?,
                        bbox.as_deref(),
                        polygon.as_deref(),
                        extract_strategy(simple, smart),
                        set_bounds,
                        &clean_attrs(&clean),
                        &write,
                        &overrides,
                        &host,
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
                inject_prepass,
                ignore_missing_nodes,
                compression,
                force,
                io,
                header,
                tmp,
            } => {
                let scratch = run_scratch(&tmp, Some(&output.output), &host)?;
                let index_type = index_type.resolve()?;
                let overrides = HeaderOverrides::parse(header.generator, &header.output_headers)?;
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force: force.force,
                };
                let options = pbfhogg::altw::AltwOptions {
                    keep_untagged_nodes,
                    write,
                    index_type,
                    inject_prepass,
                    ignore_missing_nodes,
                };
                run_add_locations_to_ways(
                    &file,
                    output.open(&host)?,
                    &options,
                    &overrides,
                    &host,
                    scratch,
                )
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
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force: false,
                };
                run_time_filter(
                    &file,
                    output.open(&host)?,
                    &timestamp,
                    &write,
                    &overrides,
                    &host,
                )
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
                    let file = file.expect("clap requires a file without an inspect subcommand");
                    if let Some(show_id) = show {
                        return accept_outcome(
                            &mut outcome,
                            run_show_element(&file, show_id, io.direct_io, &host),
                        );
                    }
                    accept_outcome(
                        &mut outcome,
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
                        ),
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
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, uring.io_uring),
                    direct_reads: io.direct_io,
                    force: force.force,
                };
                let opts = pbfhogg::apply_changes::MergeOptions {
                    write,
                    locations_on_ways,
                    ignore_missing_nodes,
                    jobs: if jobs == 0 { None } else { Some(jobs) },
                    allow_version_regression,
                    #[cfg(feature = "test-hooks")]
                    panic_at_blob_seq: None,
                };
                run_apply_changes(
                    &base,
                    &changes,
                    output.open(&host)?,
                    &opts,
                    &overrides,
                    &host,
                )
            }
            Command::MergeChanges {
                changes,
                output,
                simplify,
                allow_version_regression,
                jobs,
            } => run_merge_changes(
                &changes,
                output.open(&host)?,
                simplify,
                jobs,
                allow_version_regression,
                &host,
            ),
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
            } => accept_outcome(
                &mut outcome,
                run_check(
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
                overwrite,
                allow_unindexed,
                force,
                tmp,
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
                overwrite || force,
                allow_unindexed || force,
                &tmp,
                &host,
            ),
            Command::BenchRead { file, mode } => run_bench_read(&file, mode, &host),
            Command::BenchWrite {
                file,
                compression,
                writer,
            } => run_bench_write(&file, compression, writer, &host),
            Command::BenchMerge {
                base,
                changes,
                output,
                compression,
                io_mode,
            } => run_bench_merge(
                &base,
                &changes,
                output.open(&host)?,
                compression,
                io_mode,
                &host,
            ),
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
    pbfhogg::read::emit_read_counters();
    pbfhogg::write::report_writer_index_loss(&host.diagnostics);
    #[cfg(feature = "test-hooks")]
    signals::test_hooks::write_count(&host.cancel);

    // From here on the outcome is decided; a "stopping" notice the signal
    // thread has not printed yet would be stale.
    signals::finish();
    let signal = signals::first_signal(&host.cancel);
    match result {
        Ok(()) => {
            if let Some(signo) = signal {
                // The work and any publication had completed (every early
                // stop is an error, and publication refuses once the token
                // is tripped before its decision): the result stands.
                output::cli_eprintln!(
                    "Note: {} arrived after the run had completed; its result stands",
                    signals::name(signo)
                );
            }
            process::ExitCode::from(outcome.code())
        }
        // Decided before the reader-gone override: an interrupted run whose
        // stdout reader also left must not report success.
        Err(e) if is_cancelled(e.as_ref()) => {
            output::cli_eprintln!(
                "Error: interrupted by {}: the run stopped before completing, removed its \
                 staging and scratch files and published nothing",
                signal.map_or("a cancellation request", signals::name)
            );
            process::ExitCode::from(signals::exit_status(signal))
        }
        Err(e) => {
            if output::reader_gone() {
                // Stdout's reader went away (`pbfhogg ... | head`) and the
                // command stopped writing. Nobody is left to read a report
                // and nothing failed, so end quietly with success - see
                // `output` for why this is keyed on the recorded event
                // rather than on the shape of `e`.
                process::ExitCode::SUCCESS
            } else {
                output::cli_eprintln!(
                    "Error: {}{}",
                    error_report(e.as_ref()),
                    output_hint(e.as_ref()).unwrap_or_default()
                );
                let status = if is_platform_unsupported(e.as_ref()) {
                    ExitStatus::PlatformUnsupported
                } else {
                    ExitStatus::Failed
                };
                process::ExitCode::from(status.code())
            }
        }
    }
    // `_guard` drops here, flushing the hotpath JSON report before the
    // returned `ExitCode` is converted to the process exit status.
}

#[allow(clippy::too_many_arguments)]
fn run_build_geocode_index(
    file: &std::path::Path,
    output_dir: &std::path::Path,
    levels: GeocodeLevels,
    direct_io: bool,
    overwrite: bool,
    allow_unindexed: bool,
    tmp: &TmpDirArg,
    host: &pbfhogg::host::Host,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    // The caller has already expanded the legacy `--force` into both
    // permissions. Refusals happen here, before any work.
    let output = pbfhogg::geocode_index::builder::index_output(
        output_dir,
        pbfhogg::Overwrite::from_flag(overwrite),
        &host.diagnostics,
    )?;
    // After the output, so an existing index directory is judged before the
    // run's scratch directory could be made inside it.
    let scratch = run_scratch(tmp, Some(output_dir), host)?;
    // Exhaustive on purpose: a new `BuildConfig` field must be wired here
    // rather than silently taking its default. The clap defaults already
    // come from `BuildConfig::default()`.
    let config = GeocodeBuildConfig {
        input_path: file.to_path_buf(),
        allow_unindexed,
        direct_io,
        street_level: levels.street_level,
        coarse_level: levels.coarse_level,
        admin_level: levels.admin_level,
        max_admin_vertices: levels.max_admin_vertices,
        fine_search_radius_m: levels.search_radius,
        coarse_search_radius_m: levels.coarse_search_radius,
    };
    // The library reports the counts itself, ahead of its closing "Done" line.
    pbfhogg::geocode_index::builder::build_geocode_index(&config, output, host, scratch)?;
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
) -> Result<Outcome, Box<dyn std::error::Error>> {
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
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let check_options = pbfhogg::check::CheckOptions {
        max_errors,
        direct_io,
    };

    // Run checks and collect results
    let ids_report = if run_ids {
        use pbfhogg::check::verify_ids::VerifyIdsOptions;
        let opts = VerifyIdsOptions {
            check: check_options,
            full,
            type_filter,
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
        let report = pbfhogg::check::index::check_index(path, &check_options, host)?;
        if !report.passed() {
            failed = true;
        }
        Some(report)
    } else {
        None
    };

    let refs_result = if run_refs {
        let opts = pbfhogg::check::refs::CheckRefsOptions {
            check: check_options,
            check_relations,
            show_ids,
        };
        let result = pbfhogg::check::refs::check_refs(path, &opts, host)?;
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
                combined.insert("ids".to_string(), report.to_json_value(&file_name));
            }
            if let Some(ref report) = index_report {
                combined.insert("index".to_string(), report.to_json_value(&file_name));
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
                report.write_human(&mut out, &file_name)?;
            }
            if let Some(ref report) = index_report {
                if ids_report.is_some() {
                    writeln!(out, "\n---\n")?;
                }
                report.write_human(&mut out, &file_name)?;
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

    Ok(if failed {
        Outcome::Negative
    } else {
        Outcome::Positive
    })
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

    output::cli_eprintln!("{} distinct tag values", results.len());
    Ok(())
}

fn run_cat(
    files: &[PathBuf],
    output: pbfhogg::Output,
    type_filter: Option<TypeFilter>,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<&std::path::Path> = files.iter().map(AsRef::as_ref).collect();
    let stats = pbfhogg::cat::cat(&paths, output, type_filter, clean, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("cat.summary"))?;
    Ok(())
}

fn run_sort(
    file: &std::path::Path,
    output: pbfhogg::Output,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::sort::sort(file, output, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("sort.summary"))?;
    Ok(())
}

fn run_repack(
    file: &std::path::Path,
    output: pbfhogg::Output,
    elements_per_blob: usize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::repack::repack(file, output, elements_per_blob, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("repack.summary"))?;
    Ok(())
}

fn run_degrade(
    file: &std::path::Path,
    output: pbfhogg::Output,
    flags: pbfhogg::degrade::DegradeFlags,
    block_cap: usize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::degrade::degrade(file, output, flags, block_cap, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("degrade.summary"))?;
    Ok(())
}

fn run_renumber(
    file: &std::path::Path,
    output: pbfhogg::Output,
    start_id: &str,
    write: &WriteOptions,
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
    let stats = pbfhogg::renumber::renumber_external(file, output, &opts, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("renumber.summary"))?;
    stats.report_warnings(&host.diagnostics);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_tags_filter(
    file: &std::path::Path,
    output: pbfhogg::Output,
    expressions_file: Option<&std::path::Path>,
    expressions: &[String],
    flags: TagsFilterFlags,
    write: &WriteOptions,
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
        host::cli_warning(
            "tags_filter.remove_tags_ignored",
            format_args!("With -R/--omit-referenced use of -t/--remove-tags isn't doing anything."),
        );
    }
    let opts = pbfhogg::tags_filter::TagsFilterOptions {
        expression_strs: &all_expressions,
        omit_referenced,
        invert: invert_match,
        remove_tags,
        write: *write,
        jobs: if jobs == 0 { None } else { Some(jobs) },
        distrust_metadata,
    };
    let stats = pbfhogg::tags_filter::tags_filter(file, output, &opts, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("tags_filter.summary"))?;
    Ok(())
}

fn run_tags_filter_osc(
    changes: &std::path::Path,
    output: pbfhogg::Output,
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
    // `same_file` answers false for a missing path on either side.
    //
    // File identity (symlinks, hard links) is the library's test, shared
    // with the library entry points that refuse an aliased output; only the
    // not-yet-created planned-path case below is CLI-specific.
    if pbfhogg::path_guard::same_file(input, output)? {
        return Ok(true);
    }
    if output.exists() {
        return Ok(false);
    }
    let input_canonical = match std::fs::canonicalize(input) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };

    let Some(planned) = planned_path(output)? else {
        return Ok(false);
    };
    Ok(input_canonical == planned)
}

/// Resolve the location a not-yet-created file will occupy: the canonical
/// parent directory joined with the final component. Returns `None` when the
/// path has no file name (e.g. a bare `..`), which can never be a valid output.
fn planned_path(path: &std::path::Path) -> std::io::Result<Option<PathBuf>> {
    // A bare relative filename has an empty parent, which means the current directory.
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let parent = std::fs::canonicalize(parent)?;
    Ok(path.file_name().map(|name| parent.join(name)))
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

/// The run's scratch directory, made before any input is opened so that a
/// missing or unwritable `--tmp-dir` fails first. `output` is the file the
/// command writes, when it writes one: the default scratch location is its
/// directory.
fn run_scratch(
    tmp: &TmpDirArg,
    output: Option<&std::path::Path>,
    host: &pbfhogg::host::Host,
) -> Result<pbfhogg::scratch::RunScratch, Box<dyn std::error::Error>> {
    if let Some(dir) = &tmp.tmp_dir {
        return pbfhogg::scratch::RunScratch::create(dir, host)
            .map_err(|e| format!("--tmp-dir {}: {}", dir.display(), error_report(&e)).into());
    }
    let parent = output
        .and_then(std::path::Path::parent)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    Ok(pbfhogg::scratch::RunScratch::create(parent, host)?)
}

fn parent_of(path: &std::path::Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The directories a command's leftovers would be in: the directory of each
/// output it writes (staging and backups sit beside it), the scratch
/// location, and where older versions of the same command put their scratch
/// (`diff` shards beside the old input).
fn leftover_dirs(command: &Command) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match command {
        Command::Cat { output, .. }
        | Command::Sort { output, .. }
        | Command::Repack { output, .. }
        | Command::Degrade { output, .. }
        | Command::Renumber { output, .. }
        | Command::TagsFilter { output, .. }
        | Command::Getid { output, .. }
        | Command::Getparents { output, .. }
        | Command::TimeFilter { output, .. }
        | Command::ApplyChanges { output, .. }
        | Command::MergeChanges { output, .. }
        | Command::BenchMerge { output, .. } => dirs.push(parent_of(&output.output)),
        Command::AddLocationsToWays { output, tmp, .. } => {
            dirs.push(parent_of(&output.output));
            dirs.extend(tmp.tmp_dir.clone());
        }
        Command::Export {
            output: Some(output),
            ..
        }
        | Command::Extract {
            output: Some(output),
            ..
        } => dirs.push(parent_of(output)),
        Command::Diff {
            old,
            output,
            quiet,
            tmp,
            ..
        } => {
            // Older versions wrote `diff-par-*` shards beside the old input.
            dirs.push(parent_of(old));
            dirs.extend(output.as_deref().map(parent_of));
            // Where `run_scratch` puts this run's own directory: `--tmp-dir`,
            // else the output's directory, else (text to stdout, or `--quiet`,
            // which writes no output) the current directory.
            match &tmp.tmp_dir {
                Some(dir) => dirs.push(dir.clone()),
                None if output.is_none() || *quiet => dirs.push(PathBuf::from(".")),
                None => {}
            }
        }
        Command::BuildGeocodeIndex {
            output_dir, tmp, ..
        } => {
            dirs.push(parent_of(output_dir));
            dirs.extend(tmp.tmp_dir.clone());
        }
        _ => {}
    }
    dirs
}

/// Reports what earlier runs that never finished left behind, before the run
/// reserves any output of its own. Nothing is removed. Each directory is
/// scanned at most once per kind of scan, however many ways a command reaches
/// it (two spellings, a `--tmp-dir` that is the output's directory, several
/// `extract --config` outputs in one directory).
#[derive(Default)]
struct LeftoverScanner {
    /// Directories already scanned beside outputs and for scratch.
    beside: std::collections::HashSet<(u64, u64)>,
    /// Directories already scanned from the inside, as an index to replace.
    inside_index: std::collections::HashSet<(u64, u64)>,
}

impl LeftoverScanner {
    /// `true` the first time `dir` is seen in `seen`. A directory that does
    /// not exist has no leftovers; the command's own checks say what is wrong
    /// with it.
    fn first_visit(
        seen: &mut std::collections::HashSet<(u64, u64)>,
        dir: &std::path::Path,
    ) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(dir).is_ok_and(|meta| seen.insert((meta.dev(), meta.ino())))
    }

    fn dir(&mut self, dir: &std::path::Path, host: &pbfhogg::host::Host) {
        if Self::first_visit(&mut self.beside, dir) {
            let scan = pbfhogg::scratch::find_stale(dir);
            pbfhogg::scratch::report_stale(&host.diagnostics, &scan);
        }
    }

    /// An existing index directory about to be replaced.
    fn index(&mut self, dir: &std::path::Path, host: &pbfhogg::host::Host) {
        if Self::first_visit(&mut self.inside_index, dir) {
            let scan = pbfhogg::scratch::find_stale_in_index(dir);
            pbfhogg::scratch::report_stale(&host.diagnostics, &scan);
        }
    }

    fn command(&mut self, command: &Command, host: &pbfhogg::host::Host) {
        for dir in leftover_dirs(command) {
            self.dir(&dir, host);
        }
        if let Command::BuildGeocodeIndex { output_dir, .. } = command {
            self.index(output_dir, host);
        }
    }
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
        | Command::AddLocationsToWays { file, output, .. }
        | Command::TimeFilter { file, output, .. } => reject_aliases(&[file], &output.output),
        Command::TagsFilter {
            file,
            output,
            expressions_file,
            ..
        } => {
            let mut inputs: Vec<&std::path::Path> = vec![file];
            inputs.extend(expressions_file.as_deref());
            reject_aliases(&inputs, &output.output)
        }
        Command::Getid {
            file,
            output,
            id_file,
            id_osm_file,
            ..
        }
        | Command::Getparents {
            file,
            output,
            id_file,
            id_osm_file,
            ..
        } => {
            let mut inputs: Vec<&std::path::Path> = vec![file];
            inputs.extend(id_file.as_deref());
            inputs.extend(id_osm_file.as_deref());
            reject_aliases(&inputs, &output.output)
        }
        Command::Export {
            file,
            output: Some(output),
            expressions_file,
            ..
        } => {
            let mut inputs: Vec<&std::path::Path> = vec![file];
            inputs.extend(expressions_file.as_deref());
            reject_aliases(&inputs, output)
        }
        Command::Diff {
            old,
            new,
            output: Some(output),
            ..
        } => reject_aliases(&[old, new], output),
        Command::Extract {
            file,
            output: Some(output),
            polygon,
            ..
        } => {
            let mut inputs: Vec<&std::path::Path> = vec![file];
            inputs.extend(polygon.as_deref());
            reject_aliases(&inputs, output)
        }
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
        | Command::Digest { .. }
        | Command::Check { .. }
        | Command::BenchRead { .. }
        | Command::BenchWrite { .. }
        | Command::BuildGeocodeIndex { .. } => Ok(()),
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
    if matches!(
        err.kind(),
        pbfhogg::ErrorKind::MissingFeature(f) if *f == pbfhogg::HeaderBlock::LOCATIONS_ON_WAYS
    ) {
        return Box::new(Hinted::new(err, "; use --type node to export nodes only"));
    }
    Box::new(err)
}

#[allow(clippy::too_many_arguments)]
fn run_export(
    file: &std::path::Path,
    output: Option<pbfhogg::Output>,
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
    let types = type_filter.map_or(
        pbfhogg::export::ExportTypes::All,
        pbfhogg::export::ExportTypes::from,
    );
    let options = pbfhogg::export::ExportOptions::new(
        format,
        types,
        &expressions,
        properties,
        bbox,
        metadata,
    )?;

    if let Some(output) = output {
        // The input/output alias refusal already ran in
        // `validate_input_output_aliases`, against the destination.
        let mut buf = output.writer(&host.cancel)?;
        run_export_to(file, &mut buf, &options, host)?;
        // The checked flush that lets the output be published.
        buf.finish()?;
        output.publish(&host.cancel)?;
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
    let default_type = default_type.map(ElementKind::from);
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

// `--verbose-ids` is a deliberately exhaustive diagnostic: no output cap,
// and `print_missing_ids` collects the missing IDs per kind before printing.
// With `-I` every input ID participates, so output and storage scale with
// the input. Both CLI references document this under getid; capping it
// would make the listing incomplete without saying which IDs were cut.
fn print_requested_ids(ids: &pbfhogg::getid::ElementIds) {
    output::cli_eprintln!("Requested IDs:");
    if ids.node_ids.has_any() {
        output::cli_eprint!("  nodes:");
        for id in ids.node_ids.iter() {
            output::cli_eprint!(" {id}");
        }
        output::cli_eprintln!();
    }
    if ids.way_ids.has_any() {
        output::cli_eprint!("  ways:");
        for id in ids.way_ids.iter() {
            output::cli_eprint!(" {id}");
        }
        output::cli_eprintln!();
    }
    if ids.relation_ids.has_any() {
        output::cli_eprint!("  relations:");
        for id in ids.relation_ids.iter() {
            output::cli_eprint!(" {id}");
        }
        output::cli_eprintln!();
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
        output::cli_eprintln!("Found all requested objects.");
    } else {
        output::cli_eprintln!("Did not find {total_missing} object(s):");
        for id in &missing_nodes {
            output::cli_eprintln!("  n{id}");
        }
        for id in &missing_ways {
            output::cli_eprintln!("  w{id}");
        }
        for id in &missing_rels {
            output::cli_eprintln!("  r{id}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_diff(
    old: &std::path::Path,
    new: &std::path::Path,
    flags: DiffFlags,
    output: Option<pbfhogg::Output>,
    type_filter: Option<TypeFilter>,
    jobs: usize,
    direct_io: bool,
    host: &pbfhogg::host::Host,
    scratch: pbfhogg::scratch::RunScratch,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let DiffFlags {
        suppress_common,
        verbose,
        osmium_summary,
        quiet,
    } = flags;

    let jobs_resolved = if jobs == 0 {
        host.resources
            .parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
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
        pbfhogg::diff::diff(old, new, &mut sink, &options, direct_io, host, scratch)?
    } else if let Some(output) = output {
        let mut out = output.writer(&host.cancel)?;
        let stats = pbfhogg::diff::diff(old, new, &mut out, &options, direct_io, host, scratch)?;
        out.finish()?;
        // A diff that found differences is a completed answer, not a
        // failure: its output is published like any other.
        output.publish(&host.cancel)?;
        stats
    } else {
        // A reader that leaves early stops the diff here, before its verdict
        // exists, so the run ends with success rather than with the
        // differences status `main` cannot know.
        let mut out = output::buffered();
        let stats = pbfhogg::diff::diff(old, new, &mut out, &options, direct_io, host, scratch)?;
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
    Ok(if stats.has_differences() {
        Outcome::Negative
    } else {
        Outcome::Positive
    })
}

#[allow(clippy::too_many_arguments)]
fn run_derive_changes(
    old: &std::path::Path,
    new: &std::path::Path,
    output: pbfhogg::Output,
    direct_io: bool,
    increment_version: bool,
    update_timestamp: bool,
    jobs: usize,
    host: &pbfhogg::host::Host,
    scratch: pbfhogg::scratch::RunScratch,
) -> Result<(), Box<dyn std::error::Error>> {
    let jobs_resolved = if jobs == 0 {
        host.resources
            .parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
    } else {
        jobs
    };
    let timestamp = if update_timestamp {
        Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        )
    } else {
        None
    };
    let stats = pbfhogg::diff::derive::derive_changes(
        old,
        new,
        output,
        direct_io,
        increment_version,
        timestamp,
        jobs_resolved,
        host,
        scratch,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("derive.summary"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_getid(
    file: &std::path::Path,
    output: pbfhogg::Output,
    flags: GetidFlags,
    id_file: Option<&std::path::Path>,
    id_osm_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
    write: &WriteOptions,
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
        write.direct_reads,
        host,
    )?;

    if remove_tags && !add_referenced {
        host::cli_warning(
            "getid.remove_tags_ignored",
            format_args!(
                "Without -r/--add-referenced use of -t/--remove-tags isn't doing anything."
            ),
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
    let stats = pbfhogg::getid::getid(file, output, &id_set, &opts, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("getid.summary"))?;

    if let Some(found) = stats.found.as_ref() {
        print_missing_ids(&id_set, found);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_getparents(
    file: &std::path::Path,
    output: pbfhogg::Output,
    add_self: bool,
    id_file: Option<&std::path::Path>,
    id_osm_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    full_scan_min_blobs: Option<u64>,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let id_set = resolve_ids(
        id_file,
        id_osm_file,
        default_type,
        ids,
        write.direct_reads,
        host,
    )?;
    let opts = pbfhogg::getparents::GetparentsOptions { add_self };
    let stats = if let Some(min_blobs) = full_scan_min_blobs {
        pbfhogg::getparents::getparents_with_min_blobs(
            file, output, &id_set, &opts, write, overrides, min_blobs, host,
        )?
    } else {
        pbfhogg::getparents::getparents(file, output, &id_set, &opts, write, overrides, host)?
    };
    stats.write_summary(&mut host.diagnostics.summary_writer("getparents.summary"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_removeid(
    file: &std::path::Path,
    output: pbfhogg::Output,
    id_file: Option<&std::path::Path>,
    id_osm_file: Option<&std::path::Path>,
    default_type: Option<DefaultTypeArg>,
    ids: &[String],
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let id_set = resolve_ids(
        id_file,
        id_osm_file,
        default_type,
        ids,
        write.direct_reads,
        host,
    )?;
    let stats = pbfhogg::getid::removeid(file, output, &id_set, write, overrides, host)?;
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
    output: pbfhogg::Output,
    bbox_str: Option<&str>,
    polygon_path: Option<&std::path::Path>,
    strategy: pbfhogg::extract::ExtractStrategy,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
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
        file, output, &region, strategy, set_bounds, clean, write, overrides, host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("extract.summary"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_extract_config(
    file: &std::path::Path,
    config_path: &std::path::Path,
    directory_override: Option<&std::path::Path>,
    overwrite: pbfhogg::Overwrite,
    strategy: pbfhogg::extract::ExtractStrategy,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
    leftovers: &mut LeftoverScanner,
) -> Result<(), Box<dyn std::error::Error>> {
    let (_, slots) = pbfhogg::extract::parse_extract_config_with_output_directory(
        config_path,
        directory_override,
    )?;

    // Every input the run reads: the PBF, the config itself, and each
    // `polygon_file` the config names.
    for slot in &slots {
        let mut inputs: Vec<&std::path::Path> = vec![file, config_path];
        inputs.extend(slots.iter().filter_map(|s| s.polygon_file.as_deref()));
        reject_aliases(&inputs, &slot.output)?;
    }
    // The config names the outputs, so only now are their directories known.
    // Scanned before any output is reserved, each directory once.
    for slot in &slots {
        leftovers.dir(&parent_of(&slot.output), host);
    }
    // Every output the config names is checked here, before any extraction:
    // an existing one (without --overwrite), a symlink, two slots resolving
    // to one file. A config with many regions never fails on a late one
    // after the early ones were extracted.
    let outputs = pbfhogg::OutputSet::files(
        slots.iter().map(|slot| &slot.output),
        overwrite,
        &host.diagnostics,
    )?;
    let destinations: Vec<PathBuf> = outputs
        .iter()
        .map(|output| output.destination().to_path_buf())
        .collect();
    let regions: Vec<pbfhogg::extract::Region> =
        slots.into_iter().map(|slot| slot.region).collect();

    output::cli_eprintln!("Multi-extract: {} extracts from config", regions.len());
    // Extraction is all-or-nothing, and so, as far as failures allow, is
    // publication: the set is published as a group, rolling back members
    // already installed if a later one fails.
    let all_stats = pbfhogg::extract::extract_multi(
        file, &regions, outputs, strategy, set_bounds, clean, write, overrides, host,
    )?;
    for (i, stats) in all_stats.iter().enumerate() {
        output::cli_eprint!("  [{}] {} - ", i + 1, destinations[i].display());
        stats.write_summary(&mut host.diagnostics.summary_writer("extract.summary"))?;
    }
    Ok(())
}

fn run_add_locations_to_ways(
    file: &std::path::Path,
    output: pbfhogg::Output,
    options: &pbfhogg::altw::AltwOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
    scratch: pbfhogg::scratch::RunScratch,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats =
        pbfhogg::altw::add_locations_to_ways(file, output, options, overrides, host, scratch)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("altw.summary"))?;
    Ok(())
}

fn run_time_filter(
    file: &std::path::Path,
    output: pbfhogg::Output,
    timestamp: &str,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let cutoff = parse_timestamp(timestamp)?;
    let stats = pbfhogg::time_filter::time_filter(file, output, cutoff, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("time_filter.summary"))?;
    Ok(())
}

fn parse_timestamp(input: &str) -> Result<i64, Box<dyn std::error::Error>> {
    if let Ok(value) = input.parse::<i64>() {
        return Ok(value);
    }
    pbfhogg::commands::parse_rfc3339_utc(input)
        .map_err(|e| Box::new(Hinted::new(e, " (or UNIX seconds)")).into())
}

fn run_show_element(
    path: &std::path::Path,
    element_ref: ElementRef,
    direct_io: bool,
    host: &pbfhogg::host::Host,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let mut out = output::buffered();
    let found = pbfhogg::inspect::show_element_to(
        path,
        element_ref.kind,
        element_ref.id,
        direct_io,
        &mut out,
        host,
    )?;
    out.flush()?;
    Ok(if found {
        Outcome::Positive
    } else {
        Outcome::Negative
    })
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
    HeaderGet {
        key: &'a str,
        known: Option<pbfhogg::inspect::InspectGetKeyKind>,
    },
    /// Everything else: full scan, with any requested extras folded in.
    Full,
}

impl<'a> InspectMode<'a> {
    fn resolve(
        flags: &InspectFlags,
        blocks: Option<usize>,
        get: Option<&'a str>,
        get_lookup: Option<pbfhogg::inspect::InspectGetLookup>,
    ) -> Self {
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
        if let Some(key) = get
            && get_lookup
                .is_some_and(|lookup| lookup.need() == pbfhogg::inspect::InspectGetNeed::Header)
            && !scan_extras
            && !flags.nodes
            && !flags.indexed
        {
            return Self::HeaderGet {
                key,
                known: get_lookup.and_then(pbfhogg::inspect::InspectGetLookup::kind),
            };
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
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let get_lookup = get.map(pbfhogg::inspect::inspect_get_lookup);
    let mode = InspectMode::resolve(&flags, blocks, get, get_lookup);
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
    // Header and file get paths read without the inspect scan. Rejecting says so
    // rather than accepting a flag that would do nothing.
    if distrust_metadata && !matches!(mode, InspectMode::Full) {
        return Err(
            "--distrust-metadata applies to the scanning report only, not to --indexed, --nodes \
             or a header/file --get key"
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
            // Reads a header and at most one data blob: no checkpoint inside,
            // so it is decided before and after like every read-only command.
            host.cancel.check()?;
            let has_index = pbfhogg::has_indexdata(path, direct_io, &host.diagnostics)?;
            host.cancel.check()?;
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
            return Ok(if has_index {
                Outcome::Positive
            } else {
                Outcome::Negative
            });
        }
        InspectMode::NodesOnly => {
            let report =
                pbfhogg::inspect::node_stats::node_stats(path, direct_io, force, jobs, host)?;
            let mut out = output::buffered();
            report.write_report(&mut out)?;
            out.flush()?;
            return Ok(Outcome::Positive);
        }
        InspectMode::HeaderGet { key, known } => {
            // The reader parses the header eagerly, and reports a file
            // without one through the library's single missing-header error.
            host.cancel.check()?;
            let reader = pbfhogg::ElementReader::open(path, direct_io)?;
            let header = reader.header();
            let val = match known {
                Some(pbfhogg::inspect::InspectGetKeyKind::FileName) => Some(
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string()),
                ),
                Some(pbfhogg::inspect::InspectGetKeyKind::FileSize) => {
                    Some(std::fs::metadata(path)?.len().to_string())
                }
                Some(pbfhogg::inspect::InspectGetKeyKind::FileFormat) => Some("PBF".to_string()),
                Some(pbfhogg::inspect::InspectGetKeyKind::HeaderBbox) => header
                    .bbox()
                    .map(|bb| format!("{} {} {} {}", bb.left, bb.bottom, bb.right, bb.top)),
                Some(pbfhogg::inspect::InspectGetKeyKind::HeaderWritingProgram) => {
                    header.writing_program().map(String::from)
                }
                Some(pbfhogg::inspect::InspectGetKeyKind::HeaderReplicationUrl) => {
                    header.osmosis_replication_base_url().map(String::from)
                }
                Some(pbfhogg::inspect::InspectGetKeyKind::HeaderReplicationSequence) => header
                    .osmosis_replication_sequence_number()
                    .map(|s| s.to_string()),
                Some(pbfhogg::inspect::InspectGetKeyKind::HeaderReplicationTimestamp) => header
                    .osmosis_replication_timestamp()
                    .map(|t| t.to_string()),
                _ => return Err(format!("unknown key: {key}").into()),
            };
            let Some(v) = val else {
                return Err(format!("key {key} has no value in this file").into());
            };
            // The completion decision, as for `--indexed`.
            host.cancel.check()?;
            let mut out = output::buffered();
            writeln!(out, "{v}")?;
            out.flush()?;
            return Ok(Outcome::Positive);
        }
        InspectMode::Full => {}
    }

    // The get-key table marks data and metadata paths as requiring extended stats.
    let extended = extended
        || get_lookup
            .is_some_and(|lookup| lookup.need() == pbfhogg::inspect::InspectGetNeed::Extended);
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

    Ok(if exit_not_indexed {
        Outcome::Negative
    } else {
        Outcome::Positive
    })
}

fn run_apply_changes(
    base: &std::path::Path,
    changes: &std::path::Path,
    output: pbfhogg::Output,
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
    output: pbfhogg::Output,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<&std::path::Path> = inputs.iter().map(AsRef::as_ref).collect();
    let stats = pbfhogg::cat::dedupe::merge_pbf(&paths, output, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("cat.dedupe.summary"))?;
    Ok(())
}

fn run_merge_changes(
    changes: &[PathBuf],
    output: pbfhogg::Output,
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
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Instant;

    let (elapsed_ms, nodes, ways, rels): (u128, u64, u64, u64) = match mode {
        BenchReadMode::Sequential => {
            let reader = pbfhogg::ElementReader::from_path_with_eviction(path, true)?
                .diagnostics(host.diagnostics.clone())
                .cancel(host.cancel.clone());
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
            let reader = pbfhogg::ElementReader::from_path_with_eviction(path, true)?
                .diagnostics(host.diagnostics.clone())
                .cancel(host.cancel.clone());
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
            let reader = pbfhogg::ElementReader::from_path_with_eviction(path, true)?
                .diagnostics(host.diagnostics.clone())
                .cancel(host.cancel.clone());
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
            let reader = pbfhogg::BlobReader::from_path(path)?
                .evict_page_cache(true)
                .diagnostics(host.diagnostics.clone())
                .cancel(host.cancel.clone());
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

    output::cli_eprintln!("elapsed_ms={elapsed_ms}");
    output::cli_eprintln!("nodes={nodes}");
    output::cli_eprintln!("ways={ways}");
    output::cli_eprintln!("relations={rels}");
    Ok(())
}

fn bench_write_loop<W: std::io::Write>(
    path: &std::path::Path,
    writer: &mut pbfhogg::writer::PbfWriter<W>,
    host: &pbfhogg::host::Host,
) -> Result<(u64, u64, u64), Box<dyn std::error::Error>> {
    use pbfhogg::block_builder::{BlockBuilder, MemberData};
    use std::io::BufReader;

    // The leading OsmHeader is required like everywhere else: a zero-byte or
    // header-less file is the one missing-header error, not an empty run.
    drop(pbfhogg::ElementReader::open(path, false)?);
    let reader = pbfhogg::BlobReader::new(BufReader::new(std::fs::File::open(path)?))
        .diagnostics(host.diagnostics.clone())
        .cancel(host.cancel.clone());
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
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    use pbfhogg::block_builder::HeaderBuilder;
    use std::time::Instant;

    let header_bytes = HeaderBuilder::new().build()?;

    let (elapsed_ms, nodes, ways, rels) = match writer_mode {
        BenchWriterMode::Sync => {
            // A sink, not an output: the benchmark measures encoding and
            // discards the bytes.
            #[expect(
                clippy::disallowed_methods,
                reason = "/dev/null is a byte sink, not a published output"
            )]
            let file = std::fs::File::create("/dev/null")?;
            let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
            let mut writer = pbfhogg::writer::PbfWriter::new(buf, compression);
            writer.write_header(&header_bytes)?;
            let start = Instant::now();
            let (nodes, ways, rels) = bench_write_loop(path, &mut writer, host)?;
            // Flush in the timed window: the tail of the BufWriter is part of
            // the write, and its error is the run's error. Teardown stays
            // outside the window, as it always was for this arm.
            writer.flush()?;
            let elapsed_ms = start.elapsed().as_millis();
            (elapsed_ms, nodes, ways, rels)
        }
        BenchWriterMode::Pipelined => {
            let mut writer = pbfhogg::writer::PbfWriter::to_path(
                std::path::Path::new("/dev/null"),
                compression,
                &header_bytes,
                pbfhogg::host::Executor::Ambient,
            )?;
            let start = Instant::now();
            let (nodes, ways, rels) = bench_write_loop(path, &mut writer, host)?;
            // `flush`, not just `drop`: Drop joins the writer thread too, but
            // discards its result and skips the end-of-stream completeness
            // check. The drop stays inside the window so tearing the writer
            // down is still timed as it always was.
            writer.flush()?;
            drop(writer);
            let elapsed_ms = start.elapsed().as_millis();
            (elapsed_ms, nodes, ways, rels)
        }
    };

    output::cli_eprintln!("elapsed_ms={elapsed_ms}");
    output::cli_eprintln!("nodes={nodes}");
    output::cli_eprintln!("ways={ways}");
    output::cli_eprintln!("relations={rels}");
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn run_bench_merge(
    base: &std::path::Path,
    changes: &std::path::Path,
    output: pbfhogg::Output,
    compression: Compression,
    io_mode: BenchIoMode,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Instant;

    let backend = OutputBackend::from_flags(false, matches!(io_mode, BenchIoMode::Uring));
    let destination = output.destination().to_path_buf();

    // The timer stops where the merge, its write and its flush end: the
    // library publishes the output (fsync, rename, directory sync) inside
    // `merge`, and that is not part of the measured work, so the output stops
    // the clock itself when publication begins.
    let start = Instant::now();
    let work_elapsed = std::sync::Arc::new(std::sync::OnceLock::new());
    let output = {
        let work_elapsed = std::sync::Arc::clone(&work_elapsed);
        output.before_publish(move || {
            work_elapsed.set(start.elapsed()).ok();
        })
    };
    let opts = pbfhogg::apply_changes::MergeOptions {
        write: WriteOptions {
            compression,
            backend,
            direct_reads: false,
            force: true,
        },
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
    let elapsed_ms = work_elapsed
        .get()
        .ok_or("bench-merge: the output was published without stopping the timer")?
        .as_millis();

    let output_mb = std::fs::metadata(&destination)
        .map(|m| m.len() as f64 / 1_000_000.0)
        .unwrap_or(0.0);

    output::cli_eprintln!("elapsed_ms={elapsed_ms}");
    output::cli_eprintln!("base_nodes={}", stats.base_nodes);
    output::cli_eprintln!("base_ways={}", stats.base_ways);
    output::cli_eprintln!("base_relations={}", stats.base_relations);
    output::cli_eprintln!("diff_nodes={}", stats.diff_nodes);
    output::cli_eprintln!("diff_ways={}", stats.diff_ways);
    output::cli_eprintln!("diff_relations={}", stats.diff_relations);
    output::cli_eprintln!("blobs_passthrough={}", stats.blobs_passthrough);
    output::cli_eprintln!("blobs_rewritten={}", stats.blobs_rewritten);
    output::cli_eprintln!("bytes_passthrough={}", stats.bytes_passthrough);
    output::cli_eprintln!("bytes_rewritten={}", stats.bytes_rewritten);
    output::cli_eprintln!("diff_heap_bytes={}", stats.diff_heap_bytes);
    output::cli_eprintln!("output_mb={output_mb:.2}");
    // Peak RSS is a best-effort diagnostic; omit the field when the probe
    // cannot provide a reading instead of printing a fabricated zero.
    if let Some(peak_kb) = pbfhogg::process::read_peak_rss_kb() {
        output::cli_eprintln!("peak_rss_kb={peak_kb}");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod scratch_location_tests {
    use super::{TmpDirArg, host, run_scratch};

    #[test]
    fn scratch_goes_to_tmp_dir_else_the_output_directory_else_dot() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("tmp");
        let out_dir = dir.path().join("out");
        std::fs::create_dir(&tmp).unwrap();
        std::fs::create_dir(&out_dir).unwrap();
        let output = out_dir.join("result.pbf");
        let host = host::cli_host(pbfhogg::host::Cancel::new());

        let explicit = TmpDirArg {
            tmp_dir: Some(tmp.clone()),
        };
        let scratch = run_scratch(&explicit, Some(&output), &host).unwrap();
        assert_eq!(scratch.path().parent(), Some(tmp.as_path()));
        drop(scratch);

        let default = TmpDirArg { tmp_dir: None };
        let scratch = run_scratch(&default, Some(&output), &host).unwrap();
        assert_eq!(scratch.path().parent(), Some(out_dir.as_path()));
        drop(scratch);

        assert!(std::fs::read_dir(&tmp).unwrap().next().is_none());
        assert!(std::fs::read_dir(&out_dir).unwrap().next().is_none());
    }

    #[test]
    fn an_explicit_tmp_dir_error_names_the_flag_and_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let host = host::cli_host(pbfhogg::host::Cancel::new());
        let tmp = TmpDirArg {
            tmp_dir: Some(missing.clone()),
        };
        let err = run_scratch(&tmp, None, &host).err().unwrap().to_string();
        assert!(err.contains("--tmp-dir"), "{err}");
        assert!(err.contains(&missing.display().to_string()), "{err}");
    }
}
