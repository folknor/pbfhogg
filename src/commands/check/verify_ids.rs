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
    /// Options shared with the index and reference checks.
    pub check: super::CheckOptions,
    /// When true, use `IdSet` per type to detect duplicate IDs.
    /// Increases memory usage but catches duplicates that monotonicity alone cannot.
    pub full: bool,
    /// Restrict the check to these element types. `None` = all types.
    pub type_filter: Option<TypeFilter>,
}

fn effective_error_limit(max_errors: usize) -> Option<usize> {
    if max_errors == 0 {
        None
    } else {
        Some(max_errors)
    }
}

fn remaining_error_limit(max_errors: usize, stored: usize) -> Option<usize> {
    effective_error_limit(max_errors).map(|limit| limit.saturating_sub(stored))
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
    /// A negative element ID cannot be tracked for duplicates. Positive IDs
    /// above the bounded parallel allocation are tracked in a side set filled
    /// from the phase's serial merge.
    IdOutOfRange { elem_type: &'static str, id: i64 },
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
        IdViolation::IdOutOfRange { elem_type, id } => writeln!(
            out,
            "  {elem_type} {id}: outside the duplicate-detection ID range"
        ),
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
        IdViolation::IdOutOfRange { elem_type, id } => serde_json::json!({
            "type": "id_out_of_range",
            "elem_type": elem_type,
            "id": id,
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
/// - **Full:** same shape but with a shared `IdSet` per kind populated
///   via `set_atomic_if_new` for cross-blob duplicate detection.
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
/// Streaming mode: per-blob `BlobVerifyResult` plus the schedule vec;
/// peak anon RSS in the same ~2 GB range as `--full`.
///
/// `--full` mode: an `IdSet` per kind selected by `type_filter`, its bitset
/// pre-allocated over the range `check::id_alloc_bound` chooses (at least the
/// live-OSM range for the kind) for the workers' atomic setters; an ID above
/// it goes into a side set from the phase's serial merge, moved into the
/// kind's set after the phase. The node set is about 1.6 GB;
/// total full-mode RSS at planet was about 1.8 GB.
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
        .decided(|| super::CheckOutcome::stop_on_contradiction(verify_ids_inner(input, opts, host)))
}

fn verify_ids_inner(
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
// Streaming mode is `--full` minus the IdSet allocation and duplicate
// detection: its workers report `BlobVerifyResult` with `duplicate_ids` and
// `out_of_range_ids` always empty, so the shared merge adds nothing for them.

const KINDS: [ElementKind; 3] = [ElementKind::Node, ElementKind::Way, ElementKind::Relation];

/// The phase start and end markers of each kind, indexed by ordinal.
const KIND_MARKERS: [(&str, &str); 3] = [
    ("VERIFYIDS_NODES_START", "VERIFYIDS_NODES_END"),
    ("VERIFYIDS_WAYS_START", "VERIFYIDS_WAYS_END"),
    ("VERIFYIDS_RELATIONS_START", "VERIFYIDS_RELATIONS_END"),
];

/// Whether `filter` selects `kind`.
fn selects(filter: &TypeFilter, kind: ElementKind) -> bool {
    match kind {
        ElementKind::Node => filter.nodes,
        ElementKind::Way => filter.ways,
        ElementKind::Relation => filter.relations,
    }
}

/// What both modes scan: the selected kinds' element counts, and the
/// violations in report order (type order, then nodes, ways and relations),
/// stored up to `max_errors` and all counted.
struct Scanned {
    counts: [u64; 3],
    violations: Vec<IdViolation>,
    total_violations: u64,
}

/// Scan the selected kinds of a file sorted by kind. `sets` holds a
/// pre-allocated `IdSet` per kind under `--full` (duplicate detection) and
/// none in streaming mode.
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
    schedules: KindSchedules,
    type_filter: &TypeFilter,
    mut sets: [Option<&mut IdSet>; 3],
    max_errors: usize,
    host: &crate::host::Entered<'_>,
) -> Result<Scanned> {
    let mut scanned = Scanned {
        counts: [0; 3],
        violations: Vec::new(),
        total_violations: 0,
    };
    let merge = |scanned: &mut Scanned,
                 kind: ElementKind,
                 per_blob: Vec<Option<BlobVerifyResult>>|
     -> Result<()> {
        let (count, violations, total) = merge_kind(
            per_blob,
            kind,
            remaining_error_limit(max_errors, scanned.violations.len()),
        )?;
        scanned.counts[usize::from(kind.ordinal())] = count;
        scanned.total_violations += total;
        scanned.violations.extend(violations);
        Ok(())
    };
    match schedules {
        KindSchedules::Indexed {
            nodes,
            ways,
            relations,
        } => {
            check_type_order(
                &[
                    schedule_span(&nodes),
                    schedule_span(&ways),
                    schedule_span(&relations),
                ],
                &mut scanned.violations,
                &mut scanned.total_violations,
                effective_error_limit(max_errors),
            );
            for (kind, schedule) in KINDS.into_iter().zip([nodes, ways, relations]) {
                if !selects(type_filter, kind) {
                    continue;
                }
                let index = usize::from(kind.ordinal());
                let (start, end) = KIND_MARKERS[index];
                host.invocation.marker(start);
                let per_blob = kind_phase(file, &schedule, kind, sets[index].take(), host)?;
                merge(&mut scanned, kind, per_blob)?;
                host.invocation.marker(end);
            }
        }
        KindSchedules::Unindexed(blobs) => {
            host.invocation.marker("VERIFYIDS_UNINDEXED_START");
            let sets = sets.each_ref().map(|set| set.as_deref());
            let (per_kind, spans) = unindexed_pass(file, &blobs, type_filter, sets, host)?;
            check_type_order(
                &spans,
                &mut scanned.violations,
                &mut scanned.total_violations,
                effective_error_limit(max_errors),
            );
            for (kind, per_blob) in KINDS.into_iter().zip(per_kind) {
                if selects(type_filter, kind) {
                    merge(&mut scanned, kind, per_blob)?;
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
        crate::commands::input_front::sorted_and_indexed(pin, opts.check.direct_io, host)?;

    let type_filter = TypeFilter::or_all(opts.type_filter);

    let (schedules, shared_file, _max_ids) = crate::scan::classify::build_classify_schedules_split(
        pin,
        crate::scan::classify::KindClaims::Refuse,
        host,
    )?;

    let scanned = scan_kinds(
        &shared_file,
        schedules,
        &type_filter,
        [None, None, None],
        opts.check.max_errors,
        host,
    )?;

    host.invocation.marker("VERIFYIDS_SCAN_END");

    let [node_count, way_count, relation_count] = scanned.counts;
    Ok(VerifyIdsReport {
        header_sorted,
        indexed,
        full: false,
        node_count,
        way_count,
        relation_count,
        passed: scanned.total_violations == 0,
        total_violations: scanned.total_violations,
        violations: scanned.violations,
    })
}

// ---------------------------------------------------------------------------
// The per-blob projection
// ---------------------------------------------------------------------------

/// One blob's judgement of the IDs of one kind, fed in element-walk order:
/// the blob's first and last ID, count and order violations, and with the
/// kind's `IdSet` (`--full`) its duplicates and the IDs outside the set's
/// allocation.
struct KindJudge {
    elem_type: &'static str,
    prev: Option<i64>,
    r: BlobVerifyResult,
}

impl KindJudge {
    fn new(kind: ElementKind) -> Self {
        Self {
            elem_type: kind.as_str(),
            prev: None,
            r: BlobVerifyResult::empty(),
        }
    }

    #[inline]
    fn push(&mut self, id: i64, atomic_ids: Option<&IdSet>) {
        let r = &mut self.r;
        r.count += 1;
        if r.first_id.is_none() {
            r.first_id = Some(id);
        }
        r.last_id = Some(id);
        if let Some(p) = self.prev
            && not_after(id, p)
        {
            r.within_violations.push(IdViolation::NonMonotonic {
                elem_type: self.elem_type,
                id,
                prev_id: p,
            });
        }
        self.prev = Some(id);
        // Retain IDs the bounded atomic allocation cannot cover. Positive
        // IDs are inserted after workers join; negatives remain invalid
        // under the command's existing policy.
        if let Some(atomic_ids) = atomic_ids {
            match atomic_ids.set_atomic_if_new_checked(id) {
                Some(true) => {}
                Some(false) => r.duplicate_ids.push(id),
                None => r.out_of_range_ids.push(id),
            }
        }
    }
}

/// Validate one blob through [`KindIds`] (the shared ID projection: block
/// construction's validator and kind-claim refusal, no block built) and
/// judge its IDs of `kind` once it is accepted ([`KindJudge`]). The
/// projection takes IDs by the element's own kind, never the blob's claim.
fn verify_blob(
    buffer: BlockBuffer<'_>,
    kind: ElementKind,
    atomic_ids: Option<&IdSet>,
    scratch: &mut IdScratch,
) -> Result<BlobVerifyResult> {
    let (KindIds { .. }, accepted) = buffer.project(|_, _| KindIds::of(kind, &mut *scratch))?;
    let mut judge = KindJudge::new(kind);
    scratch.each(accepted.bytes(), |_, id| {
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
        |_, buffer, scratch| verify_blob(buffer, kind, atomic_ids, scratch),
        |seq, mut r| {
            settle_overflow(&mut overflow, &mut r, host);
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
    host: &crate::host::Entered<'_>,
) -> Result<(PerKind, KindSpans<(usize, usize)>)> {
    let mut kinds = crate::element_kind::KindSet::default();
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
            let ((KindIds { .. }, groups), accepted) = buffer
                .project(|_, _| (KindIds::new(kinds, &mut *scratch), GroupKinds::default()))?;
            let mut judges = KINDS.map(KindJudge::new);
            scratch.each(accepted.bytes(), |kind, id| {
                let index = usize::from(kind.ordinal());
                judges[index].push(id, sets[index]);
                Ok(())
            })?;
            drop(accepted);
            Ok((judges.map(|judge| judge.r), groups.spans))
        },
        |seq, (results, groups)| {
            for ((slots, overflow), mut r) in per_kind.iter_mut().zip(&mut overflow).zip(results) {
                settle_overflow(overflow, &mut r, host);
                slots[seq] = Some(r);
            }
            for (span, group) in spans.iter_mut().zip(groups) {
                if let Some((first, last)) = group {
                    widen(span, (seq, first), (seq, last));
                }
            }
        },
        host,
    )?;
    Ok((per_kind, spans))
}

/// Each kind's per-blob results by blob position, indexed by ordinal.
type PerKind = [Vec<Option<BlobVerifyResult>>; 3];

/// Where each kind's elements first and last appear, as positions under some
/// order of the file (`None`: the kind is absent), indexed by ordinal.
type KindSpans<K> = [Option<(K, K)>; 3];

/// Widen `span` to cover `first..=last`. Results arrive in completion order,
/// so the bounds are a minimum and a maximum, never the latest arrival.
fn widen<K: Ord + Copy>(span: &mut Option<(K, K)>, first: K, last: K) {
    *span = Some(match *span {
        None => (first, last),
        Some((lo, hi)) => (lo.min(first), hi.max(last)),
    });
}

/// The frame offsets of a claimed schedule's first and last blob.
fn schedule_span(schedule: &[crate::scan::classify::ScheduledBlob]) -> Option<(u64, u64)> {
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
struct GroupKinds {
    group: usize,
    spans: KindSpans<usize>,
}

impl crate::read::block::GroupProjection for GroupKinds {
    fn group_end(&mut self, kinds: crate::element_kind::KindSet) {
        for (kind, span) in KINDS.into_iter().zip(&mut self.spans) {
            if kinds.has(kind) {
                widen(span, self.group, self.group);
            }
        }
        self.group += 1;
    }
}

/// Merge one kind's per-blob results in file order: counts summed, each
/// blob's own violations, then a step down from the previous blob's last ID,
/// its duplicates and its IDs outside the duplicate-detection range, stored
/// up to `max_errors_remaining` and all counted.
fn merge_kind(
    per_blob: Vec<Option<BlobVerifyResult>>,
    kind: ElementKind,
    max_errors_remaining: Option<usize>,
) -> Result<(u64, Vec<IdViolation>, u64)> {
    let elem_type = kind.as_str();
    let mut count: u64 = 0;
    let mut violations: Vec<IdViolation> = Vec::new();
    let mut total_violations: u64 = 0;
    let mut prev_last: Option<i64> = None;
    for slot in per_blob {
        let Some(r) = slot else {
            return Err(crate::error::Error::internal(
                "id check: classify phase left a blob slot unfilled",
            ));
        };
        count += r.count;
        for v in r.within_violations {
            total_violations += 1;
            if should_store_violation(violations.len(), max_errors_remaining) {
                violations.push(v);
            }
        }
        if let (Some(pl), Some(fi)) = (prev_last, r.first_id)
            && not_after(fi, pl)
        {
            total_violations += 1;
            if should_store_violation(violations.len(), max_errors_remaining) {
                violations.push(IdViolation::NonMonotonic {
                    elem_type,
                    id: fi,
                    prev_id: pl,
                });
            }
        }
        for id in r.duplicate_ids {
            total_violations += 1;
            if should_store_violation(violations.len(), max_errors_remaining) {
                violations.push(IdViolation::Duplicate { elem_type, id });
            }
        }
        for id in r.out_of_range_ids {
            total_violations += 1;
            if should_store_violation(violations.len(), max_errors_remaining) {
                violations.push(IdViolation::IdOutOfRange { elem_type, id });
            }
        }
        if r.last_id.is_some() {
            prev_last = r.last_id;
        }
    }
    Ok((count, violations, total_violations))
}

// ---------------------------------------------------------------------------
// Parallel full-mode implementation
// ---------------------------------------------------------------------------

/// Per-blob classify result for the parallel full-mode verifier.
///
/// The main-thread merge sees one of these per blob and aggregates in
/// schedule order. Each result follows the contents of its input blob and
/// streams back over the parallel_classify_phase channel.
// The detail limit is applied by the ordered merge, not by these three
// per-blob lists. On hostile input their retained storage grows with the
// number of violations across the schedule. Capping them alone would lose
// exact totals: a bounded redesign must carry separate totals for each list
// and budget retained examples across blobs while preserving merge order.
// Replacing the measured classify/merge path needs throughput validation;
// max_errors is a display-detail limit, not a whole-check memory guarantee.
struct BlobVerifyResult {
    first_id: Option<i64>,
    last_id: Option<i64>,
    count: u64,
    /// Non-monotonic violations observed *within* this blob.
    within_violations: Vec<IdViolation>,
    /// IDs that were already set in the shared IdSet when this blob
    /// tried to insert them. One entry per collision (not deduplicated -
    /// distinct duplicate occurrences count separately, matching the
    /// pre-swap RoaringTreemap::insert-returns-false semantics).
    duplicate_ids: Vec<i64>,
    /// IDs outside the parallel allocation. `settle_overflow` drains the
    /// positive ones into the phase's overflow set as the result arrives, for
    /// exact duplicate detection; the negatives left are reported as
    /// `IdOutOfRange` violations.
    out_of_range_ids: Vec<i64>,
}

impl BlobVerifyResult {
    fn empty() -> Self {
        Self {
            first_id: None,
            last_id: None,
            count: 0,
            within_violations: Vec::new(),
            duplicate_ids: Vec::new(),
            out_of_range_ids: Vec::new(),
        }
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
        crate::commands::input_front::sorted_and_indexed(pin, opts.check.direct_io, host)?;

    let type_filter = TypeFilter::or_all(opts.type_filter);

    let (schedules, shared_file, max_ids) = crate::scan::classify::build_classify_schedules_split(
        pin,
        crate::scan::classify::KindClaims::Refuse,
        host,
    )?;

    // Indexdata only sizes the bounded parallel allocation. Decoded IDs
    // outside it are inserted serially from each phase's merge, so
    // regional files with sparse global IDs need no moving ID ceiling. The
    // size is the pinned file's, the one both opens read.
    let file_len = pin.metadata()?.len();
    let mut node_ids = IdSet::new();
    let mut way_ids = IdSet::new();
    let mut relation_ids = IdSet::new();
    if type_filter.nodes {
        node_ids.pre_allocate(super::id_alloc_bound(
            max_ids[0],
            crate::tunables::NODE_ID_FLOOR,
            file_len,
        ));
    }
    if type_filter.ways {
        way_ids.pre_allocate(super::id_alloc_bound(
            max_ids[1],
            crate::tunables::WAY_ID_FLOOR,
            file_len,
        ));
    }
    if type_filter.relations {
        relation_ids.pre_allocate(super::id_alloc_bound(
            max_ids[2],
            crate::tunables::RELATION_ID_FLOOR,
            file_len,
        ));
    }

    // Type order is judged from the schedules' offsets on an indexed file
    // (no extra I/O) and from the decoded kinds on an unindexed one (see
    // `scan_kinds`).
    let scanned = scan_kinds(
        &shared_file,
        schedules,
        &type_filter,
        [
            type_filter.nodes.then_some(&mut node_ids),
            type_filter.ways.then_some(&mut way_ids),
            type_filter.relations.then_some(&mut relation_ids),
        ],
        opts.check.max_errors,
        host,
    )?;

    host.invocation.marker("VERIFYIDS_SCAN_END");

    let [node_count, way_count, relation_count] = scanned.counts;
    Ok(VerifyIdsReport {
        header_sorted,
        indexed,
        full: true,
        node_count,
        way_count,
        relation_count,
        passed: scanned.total_violations == 0,
        total_violations: scanned.total_violations,
        violations: scanned.violations,
    })
}

/// Settle the IDs one blob found outside the phase's pre-allocation, from the
/// phase's serial merge as its result arrives. A positive one goes into
/// `overflow`, the side set the workers never see (see
/// `check::insert_overflow_ids` for why it is not retained until the phase
/// ends), and moves to `duplicate_ids` if already there; a negative one stays
/// in `out_of_range_ids` for the ordered merge to report. Results arrive in
/// completion order, so which blob a duplicate is attributed to depends on
/// it, as it already does for duplicates the atomic setters find; the totals
/// do not. Test builds count the insertions on the run's token.
fn settle_overflow(
    overflow: &mut IdSet,
    r: &mut BlobVerifyResult,
    host: &crate::host::Entered<'_>,
) {
    let mut inserted: u64 = 0;
    let duplicates = &mut r.duplicate_ids;
    r.out_of_range_ids.retain(|&id| {
        if id < 0 {
            return true;
        }
        inserted += 1;
        if !overflow.set_if_new(id) {
            duplicates.push(id);
        }
        false
    });
    #[cfg(feature = "test-hooks")]
    host.invocation
        .faults()
        .check_overflow_ids
        .fetch_add(inserted, std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(feature = "test-hooks"))]
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
fn check_type_order<K: Ord + Copy>(
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
    use super::{IdViolation, check_type_order, schedule_span};
    use crate::scan::classify::ScheduledBlob;

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
