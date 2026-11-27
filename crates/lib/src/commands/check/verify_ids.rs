//! Validate PBF ID integrity: monotonicity, type ordering, and optional duplicate detection.

use crate::ElementKind;
use crate::error::Result;
use crate::idset::IdSet;
use crate::osm_id::osm_id_cmp;
use crate::owned::TypeFilter;
use crate::read::block::BlockBuffer;
use crate::read::input::{InputSource, PinnedInput};
use crate::read::project_ids::{IdScratch, KindIds};
use crate::scan::classify::KindSchedules;

/// True when `id` does not strictly follow `prev` in canonical OSM ID order.
///
/// Canonical order is `0`, then the negative IDs by ascending absolute value,
/// then the positives - the order [`crate::osm_id::osm_id_cmp`] defines and
/// the one `sort`, `scan_block_ids_checked` and the blob-overlap logic all
/// use. `check --ids` used raw `i64` comparison here, which is the same thing
/// only for positive-only files: on a file carrying negative IDs (JOSM
/// editor-local staging IDs are the everyday case) the two definitions
/// disagree in both directions at once. `sort`'s own correctly-ordered output
/// was reported non-monotonic, and a raw-`i64`-sorted file passed `check`
/// while `sort` considered it unsorted. A verifier that disagrees with the
/// definition it verifies is worse than no verifier, so this is the one place
/// that has to route through `osm_id_cmp`.
#[inline]
fn not_after(id: i64, prev: i64) -> bool {
    osm_id_cmp(id, prev) != std::cmp::Ordering::Greater
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Configuration for the `verify_ids` command.
pub struct VerifyIdsOptions {
    /// Maximum number of violations to retain in the report. Zero means
    /// unlimited. Scanning and exact counting continue past it. It also
    /// bounds the details retained while scanning (at most this many per
    /// blob and per kind), though not the rest of the check's working
    /// memory, such as the duplicate pass's ID sets.
    pub max_errors: usize,
    /// Read the input's front (header and first-blob probe) with `O_DIRECT`.
    /// The blob reads are parallel preads of unaligned blob ranges that
    /// ignore it.
    pub direct_io: bool,
    /// When true, use `IdSet` per type to detect duplicate IDs.
    /// Increases memory usage but catches duplicates that monotonicity alone cannot.
    pub full: bool,
    /// Restrict the check to these element types. `None` = all types.
    pub type_filter: Option<TypeFilter>,
}

pub(super) fn effective_error_limit(max_errors: usize) -> Option<usize> {
    if max_errors == 0 {
        None
    } else {
        Some(max_errors)
    }
}

fn should_store_violation(current_len: usize, limit: Option<usize>) -> bool {
    match limit {
        Some(limit) => current_len < limit,
        None => true,
    }
}

// ---------------------------------------------------------------------------
// Violation types
// ---------------------------------------------------------------------------

/// A single ID integrity violation.
pub enum IdViolation {
    /// An element ID is not strictly greater than the previous ID of the same type.
    NonMonotonic {
        elem_type: &'static str,
        id: i64,
        prev_id: i64,
    },
    /// An element ID appears more than once (only detected in full mode).
    Duplicate { elem_type: &'static str, id: i64 },
    /// Element types appear out of canonical order (nodes, then ways, then relations).
    TypeOrder {
        found: &'static str,
        after: &'static str,
    },
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// Result of an ID integrity verification pass.
pub struct VerifyIdsReport {
    /// Whether the PBF header declares Sort.Type_then_ID.
    pub header_sorted: bool,
    /// Whether the PBF has blob-level indexdata.
    pub indexed: bool,
    /// Whether full (duplicate detection) mode was used.
    pub full: bool,
    /// Number of nodes scanned.
    pub node_count: u64,
    /// Number of ways scanned.
    pub way_count: u64,
    /// Number of relations scanned.
    pub relation_count: u64,
    /// Stored violations, limited by `max_errors` unless it is zero.
    pub violations: Vec<IdViolation>,
    /// Total violation count (may exceed `violations.len()`).
    pub total_violations: u64,
    /// Whether some element ID of a selected kind is negative. Element IDs
    /// only, never a reference or member; independent of the verdict and of
    /// `max_errors`.
    pub has_negative_ids: bool,
    /// True when no violations were found.
    pub passed: bool,
}

impl VerifyIdsReport {
    /// Build the report as a JSON value.
    pub fn to_json_value(&self, file_name: &str) -> serde_json::Value {
        let violations_json: Vec<serde_json::Value> =
            self.violations.iter().map(violation_to_json).collect();

        serde_json::json!({
            "file": file_name,
            "header_sorted": self.header_sorted,
            "indexed": self.indexed,
            "counts": {
                "nodes": self.node_count,
                "ways": self.way_count,
                "relations": self.relation_count,
            },
            "has_negative_ids": self.has_negative_ids,
            "passed": self.passed,
            "total_violations": self.total_violations,
            "violations": violations_json,
        })
    }

    /// Serialize the report as a JSON string.
    pub fn to_json(&self, file_name: &str) -> Result<String> {
        serde_json::to_string_pretty(&self.to_json_value(file_name))
            .map_err(|e| crate::Error::internal_with("failed to serialize the report as JSON", e))
    }

    /// Write the human-readable summary to `out`.
    ///
    /// Takes a writer rather than printing so a closed stdout pipe surfaces
    /// as an `io::Error` the caller can handle instead of a `println!` panic.
    pub fn write_human<W: std::io::Write>(
        &self,
        out: &mut W,
        file_name: &str,
    ) -> std::io::Result<()> {
        writeln!(out, "Verify IDs: {file_name}")?;
        writeln!(out, "  Header sorted: {}", yes_no(self.header_sorted))?;
        writeln!(out, "  Indexed: {}", yes_no(self.indexed))?;
        write_mode_line(out, self.full)?;
        writeln!(out, "  Negative IDs: {}", yes_no(self.has_negative_ids))?;
        writeln!(out)?;

        writeln!(
            out,
            "Scanned {} nodes, {} ways, {} relations",
            fmt_count(self.node_count),
            fmt_count(self.way_count),
            fmt_count(self.relation_count),
        )?;
        writeln!(out)?;

        if self.passed {
            writeln!(out, "ID integrity: OK")?;
        } else {
            write_violations(out, self)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Human-output helpers (keep cognitive complexity out of write_human)
// ---------------------------------------------------------------------------

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// Write the mode line (streaming vs full).
fn write_mode_line(out: &mut impl std::io::Write, full: bool) -> std::io::Result<()> {
    if full {
        writeln!(out, "  Mode: full (duplicate detection)")
    } else {
        writeln!(out, "  Mode: streaming")
    }
}

/// Write the violation summary and individual violation lines.
fn write_violations(
    out: &mut impl std::io::Write,
    report: &VerifyIdsReport,
) -> std::io::Result<()> {
    let showing = report.violations.len();
    let total = report.total_violations;

    writeln!(
        out,
        "{total} violations (showing first {showing} of {total}):"
    )?;

    for v in &report.violations {
        write_single_violation(out, v)?;
    }

    writeln!(out)?;
    writeln!(out, "ID integrity: FAILED")
}

/// Write one violation line.
fn write_single_violation(out: &mut impl std::io::Write, v: &IdViolation) -> std::io::Result<()> {
    match v {
        IdViolation::NonMonotonic {
            elem_type,
            id,
            prev_id,
        } => writeln!(
            out,
            "  {elem_type} {id}: non-monotonic (previous: {prev_id})"
        ),
        IdViolation::Duplicate { elem_type, id } => {
            writeln!(out, "  {elem_type} {id}: duplicate")
        }
        IdViolation::TypeOrder { found, after } => {
            writeln!(out, "  type order: {found} after {after}")
        }
    }
}

/// Format a count with thousands separators.
fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

fn violation_to_json(v: &IdViolation) -> serde_json::Value {
    match v {
        IdViolation::NonMonotonic {
            elem_type,
            id,
            prev_id,
        } => serde_json::json!({
            "type": "non_monotonic",
            "elem_type": elem_type,
            "id": id,
            "prev_id": prev_id,
        }),
        IdViolation::Duplicate { elem_type, id } => serde_json::json!({
            "type": "duplicate",
            "elem_type": elem_type,
            "id": id,
        }),
        IdViolation::TypeOrder { found, after } => serde_json::json!({
            "type": "type_order",
            "found": found,
            "after": after,
        }),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Validate PBF ID integrity: monotonicity, type ordering, and optional duplicate detection.
///
/// Two internal paths depending on `opts.full`:
///
/// - **Streaming (default):** one parallel phase per kind via
///   `parallel_project_phase`, each blob read through the `KindIds`
///   projection on the block validator (no block built), no IdSet
///   population. Monotonicity + offset-based type-order only (no
///   duplicate detection).
/// - **Full:** the streaming scan first, which proves a kind with no order
///   violation duplicate-free (strictly increasing under `osm_id_cmp`), then
///   for each kind it found out of order only, a second read with a shared
///   `IdSet` populated via `try_insert_atomic_if_new` for cross-blob
///   duplicate detection. A negative ID is judged like any other: its order
///   by `osm_id_cmp`, its duplicates through the set's negative family,
///   which no pre-allocation covers, so every negative goes through the
///   phase's serial side set.
///
/// No kind's judgement depends on another's, so an unindexed file, whose
/// blobs' kinds the header walk cannot know, is read in one phase instead:
/// each blob decoded once, its IDs judged per kind and its type order from
/// the kinds the decode found (`scan_kinds`).
///
/// Both modes use the same per-blob worker shape to keep the planet-scale
/// memory floor bounded. Streaming mode previously walked
/// `for_each_pipelined` directly and reached about 25 GB anon at planet scale
/// before OOM at 29.2 GB in the 2026-04-26 overnight run. That was the former
/// decoder's cross-thread allocation retention: decoded blocks now use inline
/// string-table entries backed by pooled decompression buffers, and pipeline
/// admission bounds the number of in-flight blocks.
///
/// # Planet-scale memory
///
/// Streaming mode: per-blob `BlobVerifyResult` plus the schedule vec; no ID
/// set. `--full` on a file in order per kind costs the same.
///
/// `--full`'s duplicate pass: an `IdSet` per kind it reads, its bitset
/// pre-allocated over the range `check::id_alloc_bound` chooses (at least the
/// live-OSM range for the kind) for the workers' atomic setters; an ID above
/// it goes into a side set from the phase's serial merge, moved into the
/// kind's set after the phase. The planet node set alone is gigabytes, so an
/// out-of-order file pays that peak and a second read of those kinds.
///
/// # Input
///
/// The input must be a regular file (`InvalidArgument` otherwise), and is pinned for the run (see [`PinnedInput`]); a caller's
/// pin is used as it is. Where a later open cannot reach the pinned file, an
/// input replaced mid-run fails with `InputReplaced`.
///
/// # Kind claims
///
/// On an indexed file each kind's phase reads the blobs whose indexdata
/// claims that kind, and type order is judged from those claims (an empty
/// blob claiming a kind counts as that kind), so a decoded element of another
/// kind in a scheduled blob stops the check
/// ([`CheckOutcome::Stopped`](super::CheckOutcome)), discarding the type
/// order and every other finding. Under a `type_filter` only the selected
/// kinds' blobs are decoded, so a lie in another kind's blob goes unseen;
/// `check --index` is the verifier of every claim.
#[hotpath::measure]
pub fn verify_ids<'a>(
    input: impl Into<InputSource<'a>>,
    opts: &VerifyIdsOptions,
    host: &crate::host::Host,
) -> Result<super::CheckOutcome<VerifyIdsReport>> {
    let host = &host.enter()?;
    let input = input.into();
    host.invocation
        .decided(|| super::CheckOutcome::stop_on_finding(verify_ids_inner(input, opts, host)))
}

pub(super) fn verify_ids_inner(
    input: InputSource<'_>,
    opts: &VerifyIdsOptions,
    host: &crate::host::Entered<'_>,
) -> Result<VerifyIdsReport> {
    {
        // Both engines open the input twice, each through its own open file
        // description: the header and indexdata probe (O_DIRECT when asked),
        // then the schedule walk whose descriptor the classify pool preads.
        // Both are derived from this one pin, so a file renamed over the
        // input between them cannot be probed as one file and checked as the
        // other: where `/proc/self/fd` reopens the pin both opens reach the
        // pinned file, otherwise on Unix a replaced input fails the run
        // (`PinnedInput` says what it does off Unix). Taken before any other
        // open of the input.
        let pin = input.pin(host)?;
        if opts.full {
            return verify_ids_full_parallel(&pin, opts, host);
        }
        verify_ids_streaming_parallel(&pin, opts, host)
    }
}

// ---------------------------------------------------------------------------
// The scan both modes share
// ---------------------------------------------------------------------------
//
// Streaming mode and `--full`'s proof pass run without an IdSet: their
// workers report `BlobVerifyResult` with `duplicate_ids` and `overflow_ids`
// always empty.

pub(super) const KINDS: [ElementKind; 3] =
    [ElementKind::Node, ElementKind::Way, ElementKind::Relation];

/// The phase start and end markers of each kind, indexed by ordinal.
const KIND_MARKERS: [(&str, &str); 3] = [
    ("VERIFYIDS_NODES_START", "VERIFYIDS_NODES_END"),
    ("VERIFYIDS_WAYS_START", "VERIFYIDS_WAYS_END"),
    ("VERIFYIDS_RELATIONS_START", "VERIFYIDS_RELATIONS_END"),
];

/// Whether `filter` selects `kind`.
pub(super) fn selects(filter: &TypeFilter, kind: ElementKind) -> bool {
    match kind {
        ElementKind::Node => filter.nodes,
        ElementKind::Way => filter.ways,
        ElementKind::Relation => filter.relations,
    }
}

/// One selected kind's merged findings: its element count, its order
/// violations in file order (stored up to `max_errors`, all counted), its
/// duplicates (`--full`'s replay only), and the blobs that replay must read.
pub(super) struct KindScan {
    elem_type: &'static str,
    count: u64,
    violations: Vec<IdViolation>,
    /// The order violations, all counted.
    total: u64,
    /// Some element ID of the kind is negative.
    negative: bool,
    /// The canonically smallest duplicates, at least as many as the report
    /// can store, in no order; [`Scanned::into_report`] selects and sorts.
    duplicates: Vec<i64>,
    /// Every duplicate, one per extra copy, counted.
    duplicate_total: u64,
    /// The schedule positions of the blobs that may share an ID with
    /// another blob of the kind or within themselves, ascending: empty on a
    /// kind in strict order. See [`replay_plan`].
    replay: Vec<usize>,
}

/// What both modes scan: the type-order findings and each selected kind's,
/// assembled into report order by [`Scanned::into_report`].
pub(super) struct Scanned {
    pub(super) type_order: Vec<IdViolation>,
    pub(super) type_order_total: u64,
    pub(super) kinds: [Option<KindScan>; 3],
}

impl Scanned {
    /// The counts, and the violations in report order (type order, then
    /// nodes, ways and relations, each kind's order violations in file order
    /// followed by its duplicates canonically smallest first), stored up to
    /// `max_errors` and all counted. Each component already holds a prefix
    /// of at most `max_errors`, so truncating the concatenation once stores
    /// what a running remaining-limit would.
    pub(super) fn into_report(self, max_errors: usize) -> Findings {
        let limit = effective_error_limit(max_errors);
        let mut counts = [0; 3];
        let mut violations = self.type_order;
        let mut total = self.type_order_total;
        let mut negative = false;
        for (count, kind) in counts.iter_mut().zip(self.kinds) {
            if let Some(kind) = kind {
                *count = kind.count;
                total += kind.total + kind.duplicate_total;
                negative |= kind.negative;
                let mut duplicates = kind.duplicates;
                let room = limit.map_or(duplicates.len(), |limit| {
                    limit
                        .saturating_sub(kind.violations.len())
                        .min(duplicates.len())
                });
                keep_smallest(&mut duplicates, room);
                duplicates.sort_unstable_by(|a, b| osm_id_cmp(*a, *b));
                let elem_type = kind.elem_type;
                violations.extend(kind.violations);
                violations.extend(
                    duplicates
                        .into_iter()
                        .map(|id| IdViolation::Duplicate { elem_type, id }),
                );
            }
        }
        if let Some(limit) = effective_error_limit(max_errors) {
            violations.truncate(limit);
        }
        Findings {
            counts,
            violations,
            total,
            negative,
        }
    }
}

/// What [`Scanned::into_report`] assembles for the report.
pub(super) struct Findings {
    counts: [u64; 3],
    violations: Vec<IdViolation>,
    total: u64,
    negative: bool,
}

impl Findings {
    pub(super) fn into_report(
        self,
        header_sorted: bool,
        indexed: bool,
        full: bool,
    ) -> VerifyIdsReport {
        let [node_count, way_count, relation_count] = self.counts;
        VerifyIdsReport {
            header_sorted,
            indexed,
            full,
            node_count,
            way_count,
            relation_count,
            passed: self.total == 0,
            total_violations: self.total,
            has_negative_ids: self.negative,
            violations: self.violations,
        }
    }
}

/// Scan the selected kinds of a file sorted by kind. `sets` holds a
/// pre-allocated `IdSet` per kind under `--full`'s duplicate pass and none
/// otherwise.
///
/// An indexed file is read one phase per selected kind, each over the blobs
/// claiming that kind, and its type order judged from the claims (the
/// schedules' frame offsets). An unindexed one is read once: each blob's IDs
/// of every selected kind are judged from one validation pass, since no
/// kind's judgement depends on another's, and its type order from the kinds
/// the same pass decodes, by blob and group position ([`GroupKinds`]). Type
/// order covers every kind whatever `type_filter` selects, in both arms.
/// Either way it is judged first and each kind's results are then merged in
/// file order, the kinds in turn, so on a file whose indexdata is true the
/// report is the same.
fn scan_kinds(
    file: &crate::read::input::InputFile,
    schedules: &KindSchedules,
    type_filter: &TypeFilter,
    mut sets: [Option<&mut IdSet>; 3],
    max_errors: usize,
    host: &crate::host::Entered<'_>,
) -> Result<Scanned> {
    let limit = effective_error_limit(max_errors);
    let mut scanned = Scanned {
        type_order: Vec::new(),
        type_order_total: 0,
        kinds: [None, None, None],
    };
    match schedules {
        KindSchedules::Indexed {
            nodes,
            ways,
            relations,
        } => {
            check_type_order(
                &[
                    schedule_span(nodes),
                    schedule_span(ways),
                    schedule_span(relations),
                ],
                &mut scanned.type_order,
                &mut scanned.type_order_total,
                limit,
            );
            for (kind, schedule) in KINDS.into_iter().zip([nodes, ways, relations]) {
                if !selects(type_filter, kind) {
                    continue;
                }
                let index = usize::from(kind.ordinal());
                let (start, end) = KIND_MARKERS[index];
                host.invocation.marker(start);
                let per_blob = kind_phase(file, schedule, kind, sets[index].take(), limit, host)?;
                scanned.kinds[index] = Some(merge_kind(per_blob, kind, limit)?);
                host.invocation.marker(end);
            }
        }
        KindSchedules::Unindexed(blobs) => {
            host.invocation.marker("VERIFYIDS_UNINDEXED_START");
            let sets = sets.each_ref().map(|set| set.as_deref());
            let (per_kind, spans) = unindexed_pass(file, blobs, type_filter, sets, limit, host)?;
            check_type_order(
                &spans,
                &mut scanned.type_order,
                &mut scanned.type_order_total,
                limit,
            );
            for (kind, per_blob) in KINDS.into_iter().zip(per_kind) {
                if selects(type_filter, kind) {
                    scanned.kinds[usize::from(kind.ordinal())] =
                        Some(merge_kind(per_blob, kind, limit)?);
                }
            }
            host.invocation.marker("VERIFYIDS_UNINDEXED_END");
        }
    }
    Ok(scanned)
}

fn verify_ids_streaming_parallel(
    pin: &PinnedInput,
    opts: &VerifyIdsOptions,
    host: &crate::host::Entered<'_>,
) -> Result<VerifyIdsReport> {
    host.invocation.marker("VERIFYIDS_SCAN_START");

    let (header_sorted, indexed) =
        crate::commands::input_front::sorted_and_indexed(pin, opts.direct_io, host)?;

    let type_filter = TypeFilter::or_all(opts.type_filter);

    let (schedules, shared_file, _max_ids) = crate::scan::classify::build_classify_schedules_split(
        pin,
        crate::scan::classify::KindClaims::Refuse,
        host,
    )?;

    let scanned = scan_kinds(
        &shared_file,
        &schedules,
        &type_filter,
        [None, None, None],
        opts.max_errors,
        host,
    )?;

    host.invocation.marker("VERIFYIDS_SCAN_END");

    Ok(scanned
        .into_report(opts.max_errors)
        .into_report(header_sorted, indexed, false))
}

// ---------------------------------------------------------------------------
// The per-blob projection
// ---------------------------------------------------------------------------

/// One blob's judgement of the IDs of one kind, fed in element-walk order:
/// the blob's first and last ID, count and order violations, its canonical
/// interval once it is out of order, and with the kind's `IdSet` (`--full`)
/// its duplicates and the IDs outside the set's allocation, every negative
/// among them. Details are retained up to `limit` (`None`: all), every one
/// counted, since the merge stores no more than that of any one list.
pub(super) struct KindJudge {
    elem_type: &'static str,
    prev: Option<i64>,
    limit: Option<usize>,
    pub(super) r: BlobVerifyResult,
}

impl KindJudge {
    pub(super) fn new(kind: ElementKind, limit: Option<usize>) -> Self {
        Self {
            elem_type: kind.as_str(),
            prev: None,
            limit,
            r: BlobVerifyResult::empty(),
        }
    }

    #[inline]
    pub(super) fn push(&mut self, id: i64, atomic_ids: Option<&IdSet>) {
        let r = &mut self.r;
        r.count += 1;
        r.negative |= id < 0;
        if r.first_id.is_none() {
            r.first_id = Some(id);
        }
        r.last_id = Some(id);
        let unordered = self.prev.filter(|&p| not_after(id, p) || !r.strict);
        if let Some(p) = unordered {
            self.out_of_order(id, p);
        }
        self.prev = Some(id);
        // Retain the IDs the bounded atomic allocation does not cover (those
        // above it and every negative) for the phase's serial merge.
        if let Some(atomic_ids) = atomic_ids {
            match atomic_ids.try_insert_atomic_if_new(id) {
                Some(true) => {}
                Some(false) => self.r.record_duplicate(id, self.limit),
                None => self.r.overflow_ids.push(id),
            }
        }
    }

    /// `id`, after `p`, in a blob that is out of order: at or before `p`,
    /// or anywhere once an earlier ID was. Kept out of line, off the
    /// ordered input's element loop.
    #[cold]
    #[inline(never)]
    fn out_of_order(&mut self, id: i64, p: i64) {
        let r = &mut self.r;
        if not_after(id, p) {
            r.within_total += 1;
            if should_store_violation(r.within_violations.len(), self.limit) {
                r.within_violations.push(IdViolation::NonMonotonic {
                    elem_type: self.elem_type,
                    id,
                    prev_id: p,
                });
            }
            if r.strict {
                // Strict so far: the first ID is the least, the previous
                // the greatest.
                r.strict = false;
                r.lo = r.first_id.unwrap_or(p);
                r.hi = p;
            }
        }
        if not_after(id, r.lo) {
            r.lo = id;
        }
        if not_after(r.hi, id) {
            r.hi = id;
        }
    }
}

/// Keep the `room` canonically smallest of `ids`, in no order.
fn keep_smallest(ids: &mut Vec<i64>, room: usize) {
    if room == 0 {
        ids.clear();
    } else if room < ids.len() {
        ids.select_nth_unstable_by(room - 1, |a, b| osm_id_cmp(*a, *b));
        ids.truncate(room);
    }
}

/// Append `id` to a duplicate list retained up to `limit`: past twice the
/// limit the list is cut back to its `limit` canonically smallest, which
/// holds every one a report can store (equal IDs being equal details).
fn push_bounded(ids: &mut Vec<i64>, id: i64, limit: Option<usize>) {
    ids.push(id);
    if let Some(limit) = limit
        && ids.len() >= limit.saturating_mul(2).max(1)
    {
        keep_smallest(ids, limit);
    }
}

/// Check a blob's structure and kind claim through [`KindIds`], then judge
/// its IDs of `kind` ([`KindJudge`]). The projection picks the IDs by each
/// element's kind.
fn verify_blob(
    buffer: BlockBuffer<'_>,
    kind: ElementKind,
    atomic_ids: Option<&IdSet>,
    limit: Option<usize>,
    scratch: &mut IdScratch,
) -> Result<BlobVerifyResult> {
    let (KindIds { .. }, accepted) =
        buffer.project_ids_and_refs(|_, _| KindIds::of(kind, &mut *scratch))?;
    let mut judge = KindJudge::new(kind, limit);
    scratch.each(&accepted, |_, id| {
        judge.push(id, atomic_ids);
        Ok(())
    })?;
    // The bytes go back to their pool.
    drop(accepted);
    Ok(judge.r)
}

/// One phase of an indexed file: the blobs claiming `kind`, judged per blob
/// and returned by schedule position. Under `--full` each result's IDs
/// above the set's allocation are settled as it arrives and moved into the
/// set after the phase.
fn kind_phase(
    file: &crate::read::input::InputFile,
    schedule: &[crate::scan::classify::ScheduledBlob],
    kind: ElementKind,
    id_set: Option<&mut IdSet>,
    limit: Option<usize>,
    host: &crate::host::Entered<'_>,
) -> Result<Vec<Option<BlobVerifyResult>>> {
    let mut per_blob: Vec<Option<BlobVerifyResult>> = (0..schedule.len()).map(|_| None).collect();
    let mut overflow = IdSet::new();
    let atomic_ids = id_set.as_deref();
    crate::scan::classify::parallel_project_phase(
        file,
        schedule,
        None,
        IdScratch::default,
        |_, buffer, scratch| verify_blob(buffer, kind, atomic_ids, limit, scratch),
        |seq, mut r| {
            settle_overflow(&mut overflow, &mut r, limit, host);
            per_blob[seq] = Some(r);
        },
        host,
    )?;
    if let Some(id_set) = id_set {
        id_set.merge(overflow);
    }
    Ok(per_blob)
}

/// The one pass over an unindexed file: each blob validated once through
/// [`KindIds`] of the selected kinds, its IDs judged per kind as
/// [`verify_blob`] judges one kind's (the projection yields each kind's IDs
/// in element-walk order), the results returned per kind by blob position,
/// a kind not selected holding empty ones. Under `--full` each kind's
/// overflow is settled in its own side set. Beside them, where each kind's
/// elements first and last appear in the file, at `(blob position, group
/// index)`, for every kind whatever `type_filter` selects
/// ([`GroupKinds`]).
fn unindexed_pass(
    file: &crate::read::input::InputFile,
    blobs: &[crate::scan::classify::ScheduledBlob],
    type_filter: &TypeFilter,
    sets: [Option<&IdSet>; 3],
    limit: Option<usize>,
    host: &crate::host::Entered<'_>,
) -> Result<(PerKind, KindSpans<(usize, usize)>)> {
    let mut kinds = crate::element_kind::ElementKindSet::default();
    for kind in KINDS {
        if selects(type_filter, kind) {
            kinds.add(kind);
        }
    }
    let mut per_kind: PerKind = std::array::from_fn(|_| (0..blobs.len()).map(|_| None).collect());
    let mut overflow: [IdSet; 3] = std::array::from_fn(|_| IdSet::new());
    let mut spans: KindSpans<(usize, usize)> = [None; 3];
    crate::scan::classify::parallel_project_phase(
        file,
        blobs,
        None,
        IdScratch::default,
        |_, buffer, scratch| -> Result<([BlobVerifyResult; 3], KindSpans<usize>)> {
            let ((KindIds { .. }, groups), accepted) = buffer.project_ids_and_refs(|_, _| {
                (KindIds::new(kinds, &mut *scratch), GroupKinds::default())
            })?;
            let mut judges = KINDS.map(|kind| KindJudge::new(kind, limit));
            scratch.each(&accepted, |kind, id| {
                let index = usize::from(kind.ordinal());
                judges[index].push(id, sets[index]);
                Ok(())
            })?;
            drop(accepted);
            Ok((judges.map(|judge| judge.r), groups.spans))
        },
        |seq, (results, groups)| {
            for ((slots, overflow), mut r) in per_kind.iter_mut().zip(&mut overflow).zip(results) {
                settle_overflow(overflow, &mut r, limit, host);
                slots[seq] = Some(r);
            }
            fold_group_spans(&mut spans, seq, groups);
        },
        host,
    )?;
    Ok((per_kind, spans))
}

/// Each kind's per-blob results by blob position, indexed by ordinal.
type PerKind = [Vec<Option<BlobVerifyResult>>; 3];

/// Where each kind's elements first and last appear, as positions under some
/// order of the file (`None`: the kind is absent), indexed by ordinal.
pub(super) type KindSpans<K> = [Option<(K, K)>; 3];

/// Widen `span` to cover `first..=last`. Results arrive in completion order,
/// so the bounds are a minimum and a maximum, never the latest arrival.
fn widen<K: Ord + Copy>(span: &mut Option<(K, K)>, first: K, last: K) {
    *span = Some(match *span {
        None => (first, last),
        Some((lo, hi)) => (lo.min(first), hi.max(last)),
    });
}

/// Fold blob `seq`'s [`GroupKinds`] spans into the file's `(blob, group)`
/// spans, in whatever order the blobs complete.
pub(super) fn fold_group_spans(
    spans: &mut KindSpans<(usize, usize)>,
    seq: usize,
    groups: KindSpans<usize>,
) {
    for (span, group) in spans.iter_mut().zip(groups) {
        if let Some((first, last)) = group {
            widen(span, (seq, first), (seq, last));
        }
    }
}

/// The frame offsets of a claimed schedule's first and last blob.
pub(super) fn schedule_span(
    schedule: &[crate::scan::classify::ScheduledBlob],
) -> Option<(u64, u64)> {
    let mut span = None;
    for blob in schedule {
        let offset = blob.frame_start();
        widen(&mut span, offset, offset);
    }
    span
}

/// The type-order evidence one unindexed blob's validation yields: for each
/// kind, the index of the first and the last group holding it, from the kinds
/// the validator hands each group's end. The element walk reads a group's
/// nodes, then its ways, then its relations, whatever its field order (a
/// group holding several kinds already breaks the PBF format, which allows
/// one), so inside a group the kinds are in order and two kinds sharing a
/// group share a position: an inversion inside a blob is a later group
/// holding a kind below one an earlier group held, which these positions,
/// read under the blob's place in the file, expose to [`check_type_order`].
/// What a hook records is provisional, read only once the blob is accepted.
#[derive(Default)]
pub(super) struct GroupKinds {
    group: usize,
    pub(super) spans: KindSpans<usize>,
}

impl crate::read::block::GroupProjection for GroupKinds {
    fn group_end(&mut self, kinds: crate::element_kind::ElementKindSet) {
        for (kind, span) in KINDS.into_iter().zip(&mut self.spans) {
            if kinds.has(kind) {
                widen(span, self.group, self.group);
            }
        }
        self.group += 1;
    }
}

/// Merge one kind's per-blob results: counts summed, the order violations in
/// file order (each blob's step down from the previous blob's last ID, at its
/// first element, then the blob's own), stored up to `max_errors_remaining`
/// and all counted; the duplicates, which [`Scanned::into_report`] lists
/// after them in canonical ID order; and, on a kind out of order, the
/// [`replay_plan`] `--full`'s duplicate pass reads.
///
/// Which occurrence of a repeated ID a worker calls the duplicate follows
/// completion order: the atomic setters let whichever blob inserts second
/// lose, and `settle_overflow` takes spilled IDs as results arrive. The
/// multiset is fixed, though: an ID seen `m` times yields `m - 1` equal
/// `Duplicate` details whichever copies they came from. So duplicates take
/// no position from their blob; they follow the kind's order violations,
/// canonically smallest first, and the details a cap keeps do not depend on
/// the schedule. Attributing each to its true file position would need an
/// ordered replay of every ID, serialising the duplicate pass.
pub(super) fn merge_kind(
    per_blob: Vec<Option<BlobVerifyResult>>,
    kind: ElementKind,
    max_errors_remaining: Option<usize>,
) -> Result<KindScan> {
    let elem_type = kind.as_str();
    let mut count: u64 = 0;
    let mut violations: Vec<IdViolation> = Vec::new();
    let mut total_violations: u64 = 0;
    let mut non_monotonic: u64 = 0;
    let mut prev_last: Option<i64> = None;
    let mut negative = false;
    let mut duplicates: Vec<i64> = Vec::new();
    let mut duplicate_total: u64 = 0;
    // Each nonempty blob's position, canonical interval and strictness.
    let mut intervals: Vec<BlobInterval> = Vec::new();
    for (position, slot) in per_blob.into_iter().enumerate() {
        let Some(r) = slot else {
            return Err(crate::error::Error::internal(
                "id check: classify phase left a blob slot unfilled",
            ));
        };
        count += r.count;
        negative |= r.negative;
        if let (Some(pl), Some(fi)) = (prev_last, r.first_id)
            && not_after(fi, pl)
        {
            total_violations += 1;
            non_monotonic += 1;
            if should_store_violation(violations.len(), max_errors_remaining) {
                violations.push(IdViolation::NonMonotonic {
                    elem_type,
                    id: fi,
                    prev_id: pl,
                });
            }
        }
        total_violations += r.within_total;
        non_monotonic += r.within_total;
        for v in r.within_violations {
            if !should_store_violation(violations.len(), max_errors_remaining) {
                break;
            }
            violations.push(v);
        }
        duplicate_total += r.duplicate_total;
        for id in r.duplicate_ids {
            push_bounded(&mut duplicates, id, max_errors_remaining);
        }
        if let (Some(first), Some(last)) = (r.first_id, r.last_id) {
            let (lo, hi) = if r.strict {
                (first, last)
            } else {
                (r.lo, r.hi)
            };
            intervals.push(BlobInterval {
                position,
                lo,
                hi,
                strict: r.strict,
            });
            prev_last = Some(last);
        }
    }
    let replay = if non_monotonic > 0 {
        replay_plan(intervals)
    } else {
        Vec::new()
    };
    Ok(KindScan {
        elem_type,
        count,
        violations,
        total: total_violations,
        negative,
        duplicates,
        duplicate_total,
        replay,
    })
}

/// One nonempty blob of a kind: its schedule position, the canonically
/// least and greatest of its IDs, and whether they are in strict order.
struct BlobInterval {
    position: usize,
    lo: i64,
    hi: i64,
    strict: bool,
}

/// The blobs of a kind `--full`'s duplicate pass must read again, by
/// schedule position, ascending. Under canonical order (a strict total order
/// over every `i64`), two copies of one ID lie in one blob, which is then not
/// in strict order, or in two blobs whose closed intervals both hold it. So
/// a blob in strict order whose interval meets no other blob's holds no
/// duplicate, whatever its place in the file, and is skipped; every other
/// blob, sweeping the intervals by their least ID into overlapping
/// components, is read. Only observed IDs bound an interval, never a claim.
fn replay_plan(mut intervals: Vec<BlobInterval>) -> Vec<usize> {
    intervals.sort_unstable_by(|a, b| osm_id_cmp(a.lo, b.lo));
    let mut replay = Vec::new();
    let mut start = 0;
    while start < intervals.len() {
        let mut hi = intervals[start].hi;
        let mut end = start + 1;
        while end < intervals.len() && not_after(intervals[end].lo, hi) {
            if not_after(hi, intervals[end].hi) {
                hi = intervals[end].hi;
            }
            end += 1;
        }
        let component = &intervals[start..end];
        if component.len() > 1 || !component[0].strict {
            replay.extend(component.iter().map(|blob| blob.position));
        }
        start = end;
    }
    replay.sort_unstable();
    replay
}

// ---------------------------------------------------------------------------
// Parallel full-mode implementation
// ---------------------------------------------------------------------------

/// Per-blob classify result for the parallel full-mode verifier.
///
/// The main-thread merge sees one of these per blob and aggregates in
/// schedule order. Each result follows the contents of its input blob and
/// streams back over the parallel_classify_phase channel.
// Both detail lists are retained only up to the detail limit, with exact
// totals beside them: the merge stores the first `limit` order violations in
// file order, so no blob needs more than its own first `limit`, and the
// `limit` canonically smallest duplicates, which lie among each blob's own
// smallest `limit`. With `max_errors` 0 (unlimited) every detail is kept.
pub(super) struct BlobVerifyResult {
    first_id: Option<i64>,
    last_id: Option<i64>,
    count: u64,
    /// Every ID so far followed its predecessor in canonical order.
    strict: bool,
    /// Once not `strict`, the canonically least and greatest ID; while
    /// `strict` these are `first_id` and `last_id`, and these unset.
    lo: i64,
    hi: i64,
    /// The first non-monotonic violations observed *within* this blob, up
    /// to the detail limit.
    within_violations: Vec<IdViolation>,
    /// Every one of them, counted.
    within_total: u64,
    /// IDs that were already set in the shared IdSet when this blob tried
    /// to insert them, one per collision, the canonically smallest retained
    /// as [`push_bounded`] keeps them.
    duplicate_ids: Vec<i64>,
    /// Every collision, counted.
    duplicate_total: u64,
    /// IDs the parallel allocation does not cover: those above it and every
    /// negative. `settle_overflow` takes them into the phase's overflow set
    /// as the result arrives, for exact duplicate detection.
    overflow_ids: Vec<i64>,
    /// Some ID of the kind in this blob is negative.
    negative: bool,
}

impl BlobVerifyResult {
    fn empty() -> Self {
        Self {
            first_id: None,
            last_id: None,
            count: 0,
            strict: true,
            lo: 0,
            hi: 0,
            within_violations: Vec::new(),
            within_total: 0,
            duplicate_ids: Vec::new(),
            duplicate_total: 0,
            overflow_ids: Vec::new(),
            negative: false,
        }
    }

    fn record_duplicate(&mut self, id: i64, limit: Option<usize>) {
        self.duplicate_total += 1;
        push_bounded(&mut self.duplicate_ids, id, limit);
    }
}

/// Full-mode entry: `scan_kinds` with an ID set per selected kind. No
/// kind's judgement depends on another's (verify_ids only cares about
/// per-kind monotonicity and per-kind duplicates), so on an indexed file
/// phase ordering is purely for phase-wall attribution in the sidecar.
fn verify_ids_full_parallel(
    pin: &PinnedInput,
    opts: &VerifyIdsOptions,
    host: &crate::host::Entered<'_>,
) -> Result<VerifyIdsReport> {
    host.invocation.marker("VERIFYIDS_SCAN_START");

    // Cheap: one front read of the header and the first data blob's header.
    // parallel_classify_phase reads through its own descriptor, the
    // shared_file from build_classify_schedules_split.
    let (header_sorted, indexed) =
        crate::commands::input_front::sorted_and_indexed(pin, opts.direct_io, host)?;

    let type_filter = TypeFilter::or_all(opts.type_filter);

    let (schedules, shared_file, max_ids) = crate::scan::classify::build_classify_schedules_split(
        pin,
        crate::scan::classify::KindClaims::Refuse,
        host,
    )?;

    // The proof pass: the streaming scan. `osm_id_cmp` is a strict total
    // order over every `i64` (its key is injective), so a kind with no order violation, within a blob or across
    // blobs in file order, is strictly increasing and holds no duplicate:
    // its findings are final and it never allocates an ID set. Nothing a
    // header or indexdata claims is believed for this. Type order is judged
    // here, from the schedules' offsets on an indexed file (no extra I/O) and
    // from the decoded kinds on an unindexed one (see `scan_kinds`).
    let mut scanned = scan_kinds(
        &shared_file,
        &schedules,
        &type_filter,
        [None, None, None],
        opts.max_errors,
        host,
    )?;
    duplicate_pass(
        &mut scanned,
        &shared_file,
        &schedules,
        max_ids,
        pin,
        opts.max_errors,
        host,
    )?;

    host.invocation.marker("VERIFYIDS_SCAN_END");

    Ok(scanned
        .into_report(opts.max_errors)
        .into_report(header_sorted, indexed, true))
}

/// `--full`'s duplicate pass: of the kinds the proof pass in `scanned` found
/// out of order, the blobs each kind's replay plan names (those that may
/// share an ID, see [`replay_plan`]) are read again with the kind's ID set.
/// Only the duplicates are taken from that read; every other finding is the
/// proof pass's, which read every blob. On an unindexed file one read covers
/// the union of the kinds' plans, and IDs of a kind from a blob only another
/// kind's plan named are inserted too: they collide with nothing, a blob
/// left out of a plan holding no ID that occurs twice.
pub(super) fn duplicate_pass(
    scanned: &mut Scanned,
    shared_file: &crate::read::input::InputFile,
    schedules: &KindSchedules,
    max_ids: [i64; 3],
    pin: &PinnedInput,
    max_errors: usize,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let mut dup_filter = TypeFilter {
        nodes: false,
        ways: false,
        relations: false,
    };
    for kind in KINDS {
        let index = usize::from(kind.ordinal());
        if scanned.kinds[index]
            .as_ref()
            .is_some_and(|k| !k.replay.is_empty())
        {
            match kind {
                ElementKind::Node => dup_filter.nodes = true,
                ElementKind::Way => dup_filter.ways = true,
                ElementKind::Relation => dup_filter.relations = true,
            }
        }
    }
    if dup_filter.nodes || dup_filter.ways || dup_filter.relations {
        host.invocation.marker("VERIFYIDS_DUPLICATES_START");
        // Indexdata only sizes the bounded parallel allocation. Decoded IDs
        // outside it are inserted serially from each phase's merge, so
        // regional files with sparse global IDs need no moving ID ceiling.
        // The size is the pinned file's, the one both opens read.
        let file_len = pin.metadata()?.len();
        let floors = [
            crate::tunables::NODE_ID_FLOOR,
            crate::tunables::WAY_ID_FLOOR,
            crate::tunables::RELATION_ID_FLOOR,
        ];
        let mut sets: [IdSet; 3] = std::array::from_fn(|_| IdSet::new());
        for (index, (set, floor)) in sets.iter_mut().zip(floors).enumerate() {
            if selects(&dup_filter, KINDS[index]) {
                set.pre_allocate(super::id_alloc_bound(max_ids[index], floor, file_len));
            }
        }
        let replays = scanned
            .kinds
            .each_ref()
            .map(|kind| kind.as_ref().map_or(&[][..], |k| k.replay.as_slice()));
        let subset = |schedule: &[crate::scan::classify::ScheduledBlob], replay: &[usize]| {
            replay.iter().map(|&p| schedule[p]).collect::<Vec<_>>()
        };
        let replayed = match schedules {
            KindSchedules::Indexed {
                nodes,
                ways,
                relations,
            } => KindSchedules::Indexed {
                nodes: subset(nodes, replays[0]),
                ways: subset(ways, replays[1]),
                relations: subset(relations, replays[2]),
            },
            KindSchedules::Unindexed(blobs) => {
                let mut union: Vec<usize> = replays.concat();
                union.sort_unstable();
                union.dedup();
                KindSchedules::Unindexed(subset(blobs, &union))
            }
        };
        let [nodes, ways, relations] = &mut sets;
        let dup = scan_kinds(
            shared_file,
            &replayed,
            &dup_filter,
            [
                dup_filter.nodes.then_some(nodes),
                dup_filter.ways.then_some(ways),
                dup_filter.relations.then_some(relations),
            ],
            max_errors,
            host,
        )?;
        drop(sets);
        for (slot, kind) in scanned.kinds.iter_mut().zip(dup.kinds) {
            if let (Some(slot), Some(kind)) = (slot.as_mut(), kind) {
                slot.duplicates = kind.duplicates;
                slot.duplicate_total = kind.duplicate_total;
            }
        }
        host.invocation.marker("VERIFYIDS_DUPLICATES_END");
    }
    Ok(())
}

/// Settle the IDs one blob found outside the phase's pre-allocation (above
/// it, or negative), from the phase's serial merge as its result arrives.
/// Each goes into `overflow`, the side set the workers never see (see
/// `check::insert_overflow_ids` for why it is not retained until the phase
/// ends), and moves to `duplicate_ids` if already there. The spill vector is
/// taken and dropped here, so a saved result holds no allocation for it
/// until the ordered merge. Results arrive in
/// completion order, so which blob a duplicate is attributed to depends on
/// it, as it already does for duplicates the atomic setters find; neither the
/// totals nor the report do, since `merge_kind` orders duplicates by ID, not
/// by blob. Test builds count the insertions on the run's token.
pub(super) fn settle_overflow(
    overflow: &mut IdSet,
    r: &mut BlobVerifyResult,
    limit: Option<usize>,
    host: &crate::host::Entered<'_>,
) {
    let mut inserted: u64 = 0;
    for id in std::mem::take(&mut r.overflow_ids) {
        inserted += 1;
        if !overflow.insert_if_new(id) {
            r.record_duplicate(id, limit);
        }
    }
    #[cfg(pbfhogg_test_hooks)]
    host.invocation
        .faults()
        .check_overflow_ids
        .fetch_add(inserted, std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(pbfhogg_test_hooks))]
    let _ = (host, inserted);
}

/// Verify canonical kind order: every node before every way before every
/// relation. `spans` holds, per kind, its first and last position in the
/// file under one order: the claimed schedules' frame offsets on an indexed
/// file ([`schedule_span`]; no extra I/O), the decoded `(blob, group)`
/// positions on an unindexed one ([`GroupKinds`]). A pair is inverted when
/// the lower kind's last position lies past the higher kind's first; equal
/// positions (two kinds in one group) are in order.
///
/// Emits one `TypeOrder` violation per inversion found (bounded at three:
/// node-way, node-relation, way-relation), each counted whether or not it
/// is stored.
pub(super) fn check_type_order<K: Ord + Copy>(
    spans: &KindSpans<K>,
    violations: &mut Vec<IdViolation>,
    total_violations: &mut u64,
    max_errors: Option<usize>,
) {
    // `found` is the kind whose blob sits out of place, `after` the kind it
    // was found after: a node blob past a way blob is "node after way".
    let record = |found: &'static str,
                  after: &'static str,
                  violations: &mut Vec<IdViolation>,
                  total_violations: &mut u64| {
        *total_violations += 1;
        if should_store_violation(violations.len(), max_errors) {
            violations.push(IdViolation::TypeOrder { found, after });
        }
    };

    // Canonical order is nodes < ways < relations. The node-relation pair is
    // checked directly: node<way and way<rel imply it only when ways exist,
    // and a file with no ways (a relation, then a node) would otherwise
    // pass. On a correctly ordered file max_node <= min_rel, so the direct
    // check never fires there; on a disordered one it may fire beside the
    // other two, which is the bound the doc above states.
    let [nodes, ways, relations] = *spans;
    let max_node = nodes.map(|(_, last)| last);
    let min_way = ways.map(|(first, _)| first);
    let max_way = ways.map(|(_, last)| last);
    let min_rel = relations.map(|(first, _)| first);

    if let (Some(mn), Some(mw)) = (max_node, min_way)
        && mn > mw
    {
        record("node", "way", violations, total_violations);
    }
    if let (Some(mn), Some(mr)) = (max_node, min_rel)
        && mn > mr
    {
        record("node", "relation", violations, total_violations);
    }
    if let (Some(mw), Some(mr)) = (max_way, min_rel)
        && mw > mr
    {
        record("way", "relation", violations, total_violations);
    }
}

#[cfg(test)]
mod tests {
    use super::{IdViolation, VerifyIdsOptions, check_type_order, schedule_span, verify_ids_inner};
    use crate::block_builder::HeaderBuilder;
    use crate::read::wire_fixtures::{Group, block, dense, field_bytes, plain_node, relation, way};
    use crate::scan::classify::ScheduledBlob;
    use crate::writer::{Compression, PbfWriter};

    #[test]
    fn id_report_ignores_tag_values_and_unread_metadata() {
        let fixture = |tag, info: &[u8], dense_tags: &[u64]| {
            let mut node = plain_node(1, 0, 0, &[(1, tag)], None);
            field_bytes(&mut node, 4, info);
            let mut dense = dense(&[(3, 0, 0), (2, 0, 0), (3, 0, 0)], dense_tags);
            field_bytes(&mut dense, 5, info);
            block(
                &["", "k", "v"],
                &[Group {
                    nodes: vec![node],
                    dense: Some(dense),
                    ways: vec![way(10, &[1], (1, tag))],
                    relations: vec![relation(20, &[(0, 10, 1)])],
                }],
            )
        };
        let good = fixture(2, &[], &[1, 2, 0, 0, 0]);
        let bad = fixture(127, &[0x08], &[1, 127]);
        let full = crate::PrimitiveBlock::from_vec(bad.clone())
            .expect_err("full validation rejects the truncated node Info");
        assert!(
            matches!(
                full.kind(),
                crate::ErrorKind::WireFormat {
                    msg: "truncated varint"
                }
            ),
            "{full}"
        );
        let dir = crate::path_guard::tests::scratch();
        let host = crate::host::Host::for_tests();
        let entered = host.entered();
        let report = |name, payload: &[u8]| {
            let path = dir.path().join(name);
            let mut writer = PbfWriter::new(
                std::fs::File::create(&path).expect("fixture"),
                Compression::None,
            );
            writer
                .write_header(&HeaderBuilder::new().build().expect("header"))
                .expect("write header");
            writer
                .write_primitive_block_no_indexdata(payload)
                .expect("write fixture");
            writer.flush().expect("flush fixture");
            verify_ids_inner(
                path.as_path().into(),
                &VerifyIdsOptions {
                    max_errors: 0,
                    direct_io: false,
                    full: true,
                    type_filter: None,
                },
                &entered,
            )
            .expect("ID report")
            .to_json_value("fixture")
        };
        let expected = report("good.pbf", &good);
        assert_eq!(expected["counts"]["nodes"].as_u64(), Some(4));
        assert_eq!(expected["passed"].as_bool(), Some(false));
        assert_eq!(report("bad.pbf", &bad), expected);
    }

    /// One schedule entry per frame start, each a small frame.
    fn blobs(frames: &[u64]) -> Vec<ScheduledBlob> {
        frames
            .iter()
            .map(|&f| {
                ScheduledBlob::at_frame(
                    f,
                    f + 10,
                    1,
                    None,
                    crate::scan::classify::KindClaims::Unchecked,
                )
                .expect("valid test frame")
            })
            .collect()
    }

    fn type_order(nodes: &[u64], ways: &[u64], rels: &[u64]) -> Vec<(&'static str, &'static str)> {
        let mut violations = Vec::new();
        let mut total = 0u64;
        check_type_order(
            &[
                schedule_span(&blobs(nodes)),
                schedule_span(&blobs(ways)),
                schedule_span(&blobs(rels)),
            ],
            &mut violations,
            &mut total,
            None,
        );
        assert_eq!(usize::try_from(total).ok(), Some(violations.len()));
        violations
            .into_iter()
            .map(|v| match v {
                IdViolation::TypeOrder { found, after } => (found, after),
                _ => panic!("check_type_order reported a non-type-order violation"),
            })
            .collect()
    }

    #[test]
    fn a_node_blob_after_a_relation_blob_is_reported_without_ways() {
        assert_eq!(type_order(&[200], &[], &[100]), [("node", "relation")]);
    }

    #[test]
    fn a_correctly_ordered_file_reports_nothing_with_any_kind_absent() {
        assert!(type_order(&[100, 200], &[300], &[400]).is_empty());
        assert!(type_order(&[100], &[], &[400]).is_empty());
        assert!(type_order(&[], &[300], &[400]).is_empty());
        assert!(type_order(&[100], &[300], &[]).is_empty());
        assert!(type_order(&[], &[], &[400]).is_empty());
        assert!(type_order(&[100], &[], &[]).is_empty());
    }

    #[test]
    fn each_inverted_pair_is_reported_once() {
        assert_eq!(type_order(&[300], &[200], &[400]), [("node", "way")]);
        assert_eq!(type_order(&[100], &[400], &[300]), [("way", "relation")]);
        assert_eq!(
            type_order(&[500], &[300], &[400]),
            [("node", "way"), ("node", "relation")]
        );
    }
}
