//! Count tag frequencies in a PBF file. Equivalent to `osmium tags-count`.

mod table;
mod tally;

use super::require_indexdata;
use crate::ElementKind;
use crate::owned::TypeFilter;
use crate::read::input::InputSource;
use crate::tag_expr::{Expression, parse_expressions, tag_matches};

use super::Result;

/// A single tag count entry: key, value, count.
pub struct TagCount {
    pub key: String,
    pub value: String,
    pub count: u64,
}

/// What [`tags_count`] returns: the rows, sorted, and what the run counted.
pub struct TagCounts {
    /// The pairs that passed the count thresholds, in the requested order.
    pub rows: Vec<TagCount>,
    /// Figures of the completed run. They are fixed when the run ends, so
    /// they still hold after a caller drains or edits `rows`.
    pub stats: TagCountStats,
}

/// What one [`tags_count`] run counted.
pub struct TagCountStats {
    /// Distinct key/value pairs the run selected and saw (after the type
    /// filter and the expressions, before `min_count` and `max_count`): not
    /// occurrences and not distinct keys.
    pub distinct_pairs: u64,
    /// Of `distinct_pairs`, how many passed `min_count` and `max_count`,
    /// which is how many rows the run returned.
    pub retained_pairs: u64,
}

impl TagCountStats {
    /// The closing summary line: both figures, always, in one shape. It
    /// carries no `=`: a harness scraping `key=value` lines from stderr
    /// would otherwise read it as a metric.
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "{} distinct key/value pairs counted, {} passed the count thresholds",
            self.distinct_pairs, self.retained_pairs,
        )
    }
}

/// Sort order for tag count results.
#[derive(Clone, Copy, Default)]
pub enum TagCountSort {
    /// Count descending (most frequent first) - default.
    #[default]
    CountDesc,
    /// Count ascending (least frequent first).
    CountAsc,
    /// Key name ascending, then value ascending.
    NameAsc,
    /// Key name descending, then value descending.
    NameDesc,
}

/// Options for `tags_count`.
pub struct TagCountOptions<'a> {
    pub min_count: u64,
    pub max_count: Option<u64>,
    pub sort: TagCountSort,
    pub expressions: &'a [String],
    pub type_filter: Option<TypeFilter>,
    pub direct_io: bool,
    pub force: bool,
    /// Size of the classify worker pool. `Auto` keeps the decode pool's own
    /// host-derived sizing.
    pub parallelism: crate::host::Parallelism,
}

/// Count tag key=value frequencies in a PBF file.
///
/// If `type_filter` is set, only count tags on elements of those types.
/// Results are sorted by count descending,
/// then by key, then by value. Entries below `min_count` are excluded. The
/// returned [`TagCountStats`] says how many distinct pairs were counted and
/// how many of them the thresholds kept.
///
/// If `expressions` is non-empty, only tags matching at least one
/// expression are counted (same syntax as `tags-filter`).
///
/// Parallel workers via `parallel_project_accumulate`: each worker reads a
/// blob through a validator projection (`tally`), counts its tags by string
/// table index, and merges the blob's distinct pairs into one table shared by
/// every worker, sharded by pair hash with a mutex per shard (`table`). No
/// per-blob map crosses a channel and no single thread does the merging: a
/// lone consumer merging per-blob maps was the whole wall at planet, with the
/// workers parked behind it. Per-worker whole-file maps are not kept either
/// (that shape held roughly the global cardinality once per worker).
/// Transient per-worker storage is one blob's distinct index pairs.
///
/// The table is not bounded independently of input: for D distinct selected
/// key/value pairs whose value bytes total S, and K distinct keys of total
/// length L, it needs O(D + S) for the pairs plus at most O(K + L) per shard
/// for the keys (a key is interned once in each shard holding a pair of
/// it), plus hash-table capacity overhead. `min_count` and `max_count` filter only after the
/// exact totals are known; neither limits the table. Early eviction can lose
/// a later threshold crossing, and a sketch would change the exact-count
/// contract. Use type/expression restrictions to reduce cardinality, rather
/// than treating thresholds as a memory budget. See docs/cli/commands.md,
/// "inspect tags".
///
/// The input must be a regular file (`InvalidArgument` otherwise), and is pinned for the run once the expressions are accepted
/// (see [`PinnedInput`](crate::read::input::PinnedInput)); a caller's pin is
/// used as it is. Where a later open cannot reach the pinned file, an input
/// replaced mid-run fails with `InputReplaced`.
#[hotpath::measure]
pub fn tags_count<'a>(
    input: impl Into<InputSource<'a>>,
    opts: &TagCountOptions<'_>,
    host: &crate::host::Host,
) -> Result<TagCounts> {
    let host = &host.enter()?;
    let input = input.into();
    host.invocation
        .decided(|| tags_count_inner(input, opts, host))
}

fn tags_count_inner(
    input: InputSource<'_>,
    opts: &TagCountOptions<'_>,
    host: &crate::host::Entered<'_>,
) -> Result<TagCounts> {
    // Deliberately reuse tags-filter's expression grammar, including its
    // n/w/r type prefixes. osmium tags-count treats these strings as tag
    // matchers only, so `w/highway` has different meaning between the tools.
    // Parsed before the input is probed, so a malformed expression is refused
    // without touching the file.
    let expressions = if opts.expressions.is_empty() {
        None
    } else {
        Some(parse_expressions(opts.expressions)?)
    };

    // The input is opened twice, each through its own open file description:
    // the indexdata probe, then the schedule walk whose descriptor the
    // classify pool preads. Both are derived from this one pin, so a file
    // renamed over the input between them cannot be probed as one file and
    // counted as the other: where `/proc/self/fd` reopens the pin both opens
    // reach the pinned file, otherwise on Unix a replaced input fails the run
    // (`PinnedInput` says what it does off Unix). Taken before any other open
    // of the input.
    let pin = input.pin(host)?;

    // Need indexdata either for the type-filter schedule or (when there
    // is no type filter) simply for the all-kinds schedule the parallel
    // path uses. `require_indexdata` gracefully accepts `force`.
    require_indexdata(
        &pin,
        opts.direct_io,
        opts.force,
        "input PBF has no blob-level indexdata. Without indexdata, the type filter \
         is a no-op - all blobs are decompressed (significantly slower).",
        host,
    )?;

    let tf = TypeFilter::or_all(opts.type_filter);
    // Narrow the blob schedule only when the filter names exactly one
    // kind; a two-kind filter still has to visit both kinds' blobs.
    let kind_filter = tf.single_kind();

    host.invocation.marker("TAGSCOUNT_START");

    // Apply the same blob-level tag index `tags-filter` uses.
    //
    // `tags-count -e highway=*` counts only tags matching an expression, so a
    // blob whose tag index carries none of the required keys cannot contribute
    // a single count - and skipping it means never decompressing it. Without
    // this the command parsed the identical expressions and then decoded every
    // blob in the file anyway.
    //
    // The filter has to come from `blob_filter_from_expressions`, not be
    // rebuilt here: that helper clears the entire key set when any expression
    // carries a key matcher it cannot express as key-set membership, because
    // the filter's semantics are a union and a partial clear would silently
    // skip blobs the unconstrained expression could still match.
    let blob_filter = expressions
        .as_deref()
        .and_then(crate::commands::tags_filter::blob_filter_from_expressions);
    let (schedule, shared_file, blobs_skipped_by_tag) =
        crate::scan::classify::build_classify_schedule_blob_filtered(
            &pin,
            kind_filter,
            blob_filter.as_ref(),
            crate::scan::classify::KindClaims::Refuse,
            host,
        )?;
    if blobs_skipped_by_tag > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "tags_count.blobs_skipped_by_tag",
            "{blobs_skipped_by_tag} blobs skipped by tag index"
        );
    }
    let thread_override = opts.parallelism.fixed();
    let table = table::CountTable::new();
    // Distinct per worker, so the workers start their shard walks apart.
    let seeds = std::sync::atomic::AtomicUsize::new(0);

    // Each worker merges its own blobs into the shared table (see `table`),
    // so the accumulate pool's per-worker state is only scratch and nothing
    // is handed back at the end.
    crate::scan::classify::parallel_project_accumulate(
        &shared_file,
        &schedule,
        thread_override,
        || tally::BlobTally::new(seeds.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
        |buffer, tally| {
            tally.add_blob(buffer, tf, expressions.as_deref(), &table, &host.invocation)
        },
        drop,
        host,
    )?;

    host.invocation.marker("TAGSCOUNT_DRAIN_START");
    let (mut results, stats) = table.into_counts(
        |count| count >= opts.min_count && opts.max_count.is_none_or(|max| count <= max),
        &host.invocation,
    )?;

    results.sort_by(|a, b| match opts.sort {
        TagCountSort::CountDesc => b
            .count
            .cmp(&a.count)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.value.cmp(&b.value)),
        TagCountSort::CountAsc => a
            .count
            .cmp(&b.count)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.value.cmp(&b.value)),
        TagCountSort::NameAsc => a
            .key
            .cmp(&b.key)
            .then_with(|| a.value.cmp(&b.value))
            .then_with(|| b.count.cmp(&a.count)),
        TagCountSort::NameDesc => b
            .key
            .cmp(&a.key)
            .then_with(|| b.value.cmp(&a.value))
            .then_with(|| b.count.cmp(&a.count)),
    });

    host.invocation.marker("TAGSCOUNT_DRAIN_END");
    host.invocation.marker("TAGSCOUNT_END");
    Ok(TagCounts {
        rows: results,
        stats,
    })
}

/// Check if a tag matches any expression (respecting the element's type).
///
/// This predicate is per tag, not per element: counting every tag on an
/// element with one matching tag would inflate unrelated counts. Callers pass
/// the kind of the element carrying the tag. Keep this quantifier
/// separate from element filtering and use tag_matches for shared semantics.
fn matches_expression(
    expressions: &[Expression],
    key: &str,
    value: &str,
    kind: ElementKind,
) -> bool {
    for expr in expressions {
        if expr.type_filter.admits(kind) && tag_matches(&expr.matcher, key, value) {
            return true;
        }
    }
    false
}
