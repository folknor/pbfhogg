//! Count tag frequencies in a PBF file. Equivalent to `osmium tags-count`.

use rustc_hash::FxHashMap;

use super::require_indexdata;
use crate::owned::TypeFilter;
use crate::read::input::InputSource;
use crate::tag_expr::{Expression, parse_expressions, tag_matches};
use crate::{Element, ElementKind, PrimitiveBlock};

use super::Result;
type CountMap = FxHashMap<String, FxHashMap<String, u64>>;

/// A single tag count entry: key, value, count.
pub struct TagCount {
    pub key: String,
    pub value: String,
    pub count: u64,
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
/// then by key, then by value. Entries below `min_count` are excluded.
///
/// If `expressions` is non-empty, only tags matching at least one
/// expression are counted (same syntax as `tags-filter`).
///
/// Parallel workers via `parallel_classify_phase`: each worker emits a
/// per-blob `CountMap` (sized to that blob's distinct tags); the
/// consumer thread merges per-blob maps into a single global. The
/// accumulate-style alternative (per-worker map held across every
/// blob a worker sees) hit 26.8 GB peak anon at planet because every
/// worker held roughly the global cardinality by the end. Per-blob
/// emission avoids retaining a whole-file map per worker. Transient map
/// storage depends on blob contents and the number of in-flight maps;
/// 8 000 elements is a writer default, not a bound on arbitrary input.
///
/// The global map is not bounded independently of input: for D distinct
/// selected key/value pairs, K distinct keys, and S bytes of stored key/value
/// strings, it needs O(D + K + S) space plus hash-table capacity overhead.
/// The blob schedule and in-flight decode/per-blob maps add separate storage.
/// `min_count` and `max_count` filter only after the exact totals are known;
/// neither limits this map. Early eviction can lose a later threshold crossing,
/// and a sketch would change the exact-count contract. Use type/expression
/// restrictions to reduce cardinality, rather than treating thresholds as a
/// memory budget. See reference/cli-reference.md, "inspect tags".
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
) -> Result<Vec<TagCount>> {
    let input = input.into();
    host.cancel.decided(|| tags_count_inner(input, opts, host))
}

fn tags_count_inner(
    input: InputSource<'_>,
    opts: &TagCountOptions<'_>,
    host: &crate::host::Host,
) -> Result<Vec<TagCount>> {
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

    crate::debug::emit_marker("TAGSCOUNT_START");

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
            crate::scan::classify::PartialIndex::Refuse,
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
    let mut counts: CountMap = FxHashMap::default();

    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &schedule,
        thread_override,
        || (),
        |block, _s| {
            let mut local: CountMap = FxHashMap::default();
            count_block_tags(&mut local, block, tf, &expressions);
            Ok(local)
        },
        |_seq, per_blob| {
            merge_counts(&mut counts, per_blob);
        },
        host,
    )?;

    // Reserve for all D pairs, even if thresholds retain fewer. While the map
    // drains, this vector coexists with its remaining allocations; each retained
    // pair also gets its own key String to satisfy the public TagCount shape.
    // Values move into the result. Thresholds therefore do not bound even the
    // result's reserved capacity, and key bytes may be repeated D times.
    let capacity: usize = counts.values().map(rustc_hash::FxHashMap::len).sum();
    let mut results: Vec<TagCount> = Vec::with_capacity(capacity);
    for (key, inner) in counts {
        for (value, count) in inner {
            if count >= opts.min_count && opts.max_count.is_none_or(|max| count <= max) {
                results.push(TagCount {
                    key: key.clone(),
                    value,
                    count,
                });
            }
        }
    }

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

    crate::debug::emit_marker("TAGSCOUNT_END");
    Ok(results)
}

/// Check if a tag matches any expression (respecting the element's type).
///
/// This predicate is per tag, not per element: counting every tag on an
/// element with one matching tag would inflate unrelated counts. Callers pass
/// one kind from the exhaustive Element match below. Keep this quantifier
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

/// Count tags from a single block into a local map.
fn count_block_tags(
    counts: &mut CountMap,
    block: &PrimitiveBlock,
    type_filter: TypeFilter,
    expressions: &Option<Vec<Expression>>,
) {
    for element in block.elements_skip_metadata() {
        let kind = match &element {
            Element::DenseNode(_) | Element::Node(_) => ElementKind::Node,
            Element::Way(_) => ElementKind::Way,
            Element::Relation(_) => ElementKind::Relation,
        };
        if !type_filter.admits(kind) {
            continue;
        }

        match &element {
            Element::DenseNode(dn) => {
                for (k, v) in dn.tags() {
                    if expressions
                        .as_ref()
                        .is_none_or(|e| matches_expression(e, k, v, kind))
                    {
                        increment_tag(counts, k, v);
                    }
                }
            }
            Element::Node(n) => {
                for (k, v) in n.tags() {
                    if expressions
                        .as_ref()
                        .is_none_or(|e| matches_expression(e, k, v, kind))
                    {
                        increment_tag(counts, k, v);
                    }
                }
            }
            Element::Way(w) => {
                for (k, v) in w.tags() {
                    if expressions
                        .as_ref()
                        .is_none_or(|e| matches_expression(e, k, v, kind))
                    {
                        increment_tag(counts, k, v);
                    }
                }
            }
            Element::Relation(r) => {
                for (k, v) in r.tags() {
                    if expressions
                        .as_ref()
                        .is_none_or(|e| matches_expression(e, k, v, kind))
                    {
                        increment_tag(counts, k, v);
                    }
                }
            }
        }
    }
}

/// Increment the count for a (key, value) pair, allocating only on first insert.
#[inline]
fn increment_tag(counts: &mut CountMap, k: &str, v: &str) {
    if let Some(inner) = counts.get_mut(k) {
        if let Some(count) = inner.get_mut(v) {
            *count += 1;
        } else {
            inner.insert(v.to_string(), 1);
        }
    } else {
        let mut inner = FxHashMap::default();
        inner.insert(v.to_string(), 1);
        counts.insert(k.to_string(), inner);
    }
}

/// Merge a per-worker CountMap into the global totals. Sums counts for
/// keys present in both; moves unique entries across. Move-based for
/// the inner maps to avoid re-hashing value strings.
fn merge_counts(global: &mut CountMap, worker: CountMap) {
    for (key, worker_inner) in worker {
        match global.get_mut(&key) {
            Some(global_inner) => {
                for (value, count) in worker_inner {
                    *global_inner.entry(value).or_insert(0) += count;
                }
            }
            None => {
                global.insert(key, worker_inner);
            }
        }
    }
}
