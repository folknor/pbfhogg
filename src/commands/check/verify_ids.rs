//! Validate PBF ID integrity: monotonicity, type ordering, and optional duplicate detection.

use crate::ElementKind;
use crate::error::Result;
use crate::idset::IdSet;
use crate::osm_id::osm_id_cmp;
use crate::owned::TypeFilter;
use crate::read::block::{BlockBuffer, GroupProjection};
use crate::read::elements::WayRefIter;
use crate::read::input::{InputSource, PinnedInput};
use crate::read::wire::{WireDenseInfo, WireDenseNodes, WireInfo, WireNode, WireRelation, WireWay};

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
/// - **Streaming (default):** three-phase parallel scan via
///   `parallel_project_phase`, each blob read through the `KindIds`
///   projection on the block validator (no block built), no IdSet
///   population. Monotonicity + offset-based type-order only (no
///   duplicate detection).
/// - **Full:** same shape but with a shared `IdSet` per kind populated
///   via `set_atomic_if_new` for cross-blob duplicate detection.
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
/// Each kind's phase reads the blobs whose indexdata claims that kind, and
/// type order is judged from those claims, so a decoded element of another
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
// Parallel streaming-mode implementation
// ---------------------------------------------------------------------------
//
// Mirrors `verify_ids_full_parallel` minus the IdSet allocation and
// duplicate-detection pass. Workers report `BlobVerifyResult` with
// `duplicate_ids` always empty; the serial merge skips the duplicate
// translation step.

/// Warn that an unindexed input costs three decompression passes.
///
/// `build_classify_schedules_split` cannot tell which kind an unindexed blob
/// holds, so it replicates that blob into all three per-kind schedules and each
/// one gets read and decompressed three times. The answer is correct - which is
/// why this went unnoticed - but it is three times the work, silently.
///
/// This is a warning rather than a refusal (the `require_indexdata` treatment
/// the transform commands get) because `check` is a validation command with no
/// `--force`: refusing would make it impossible to validate exactly the
/// third-party files most in need of validating.
fn warn_unindexed_triple_scan(indexed: bool, host: &crate::host::Entered<'_>) {
    if !indexed {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "check.ids.unindexed_triple_scan",
            "input has no blob-level indexdata, so every blob is \
             decompressed three times - once per element kind, because an \
             unindexed blob has to be scheduled as all three. An indexed copy \
             is checked in a single pass."
        );
        // The schedule walker would add its unindexed-filter note for the
        // same condition; this warning already says it.
        host.invocation
            .mark_reported("reader.filter_without_indexdata");
    }
}

fn verify_ids_streaming_parallel(
    pin: &PinnedInput,
    opts: &VerifyIdsOptions,
    host: &crate::host::Entered<'_>,
) -> Result<VerifyIdsReport> {
    host.invocation.marker("VERIFYIDS_SCAN_START");

    // Same arena cap as `--full`. The streaming workers do no per-element
    // allocation (just reads + push to a small per-blob within_violations
    // Vec), so the contention cost measured on the pre-migration
    // time-filter snapshot path (allocation-heavy BlockBuilder scratch
    // reused across blobs) doesn't apply here.
    host.process.cross_thread_alloc_pool();

    let (header_sorted, indexed) =
        crate::commands::input_front::sorted_and_indexed(pin, opts.check.direct_io, host)?;
    warn_unindexed_triple_scan(indexed, host);

    let type_filter = TypeFilter::or_all(opts.type_filter);

    let (node_schedule, way_schedule, rel_schedule, shared_file, _max_ids) =
        crate::scan::classify::build_classify_schedules_split(
            pin,
            crate::scan::classify::KindClaims::Refuse,
            host,
        )?;

    let mut node_count: u64 = 0;
    let mut way_count: u64 = 0;
    let mut relation_count: u64 = 0;
    let mut violations: Vec<IdViolation> = Vec::new();
    let mut total_violations: u64 = 0;

    // Same offset-based type-order check as --full; non-indexed inputs
    // skip it (build_classify_schedules_split replicates blobs into all
    // three schedules when indexdata is missing).
    if indexed {
        check_type_order(
            &node_schedule,
            &way_schedule,
            &rel_schedule,
            &mut violations,
            &mut total_violations,
            effective_error_limit(opts.check.max_errors),
        );
    }

    if type_filter.nodes {
        host.invocation.marker("VERIFYIDS_NODES_START");
        let (count, phase_violations, phase_total) = verify_single_kind_streaming(
            &shared_file,
            &node_schedule,
            ElementKind::Node,
            remaining_error_limit(opts.check.max_errors, violations.len()),
            host,
        )?;
        node_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        host.invocation.marker("VERIFYIDS_NODES_END");
    }

    if type_filter.ways {
        host.invocation.marker("VERIFYIDS_WAYS_START");
        let (count, phase_violations, phase_total) = verify_single_kind_streaming(
            &shared_file,
            &way_schedule,
            ElementKind::Way,
            remaining_error_limit(opts.check.max_errors, violations.len()),
            host,
        )?;
        way_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        host.invocation.marker("VERIFYIDS_WAYS_END");
    }

    if type_filter.relations {
        host.invocation.marker("VERIFYIDS_RELATIONS_START");
        let (count, phase_violations, phase_total) = verify_single_kind_streaming(
            &shared_file,
            &rel_schedule,
            ElementKind::Relation,
            remaining_error_limit(opts.check.max_errors, violations.len()),
            host,
        )?;
        relation_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        host.invocation.marker("VERIFYIDS_RELATIONS_END");
    }

    host.invocation.marker("VERIFYIDS_SCAN_END");

    Ok(VerifyIdsReport {
        header_sorted,
        indexed,
        full: false,
        node_count,
        way_count,
        relation_count,
        passed: total_violations == 0,
        total_violations,
        violations,
    })
}

// ---------------------------------------------------------------------------
// The per-blob projection
// ---------------------------------------------------------------------------

/// Where the IDs of one element kind sit in a block, in the order the
/// element walk yields them, recorded as the block validator passes each
/// element (`BlockBuffer::project`): no block is built, no offset table
/// appended. A dense group's IDs are not copied out: the projection keeps
/// the position of its validated `id` array, decoded once after the blob is
/// accepted, where the element walk stepped `lat`, `lon` and `keys_vals`
/// beside it. (A copy would hold a whole blob's node IDs per worker: planet
/// node blobs of a few hundred thousand nodes made that tens of megabytes
/// across the pool.) Way, relation and plain node IDs, taken from the
/// messages the validator already parsed, are copied.
///
/// Validation is block construction's own call, so a blob is refused here
/// exactly when building its block would refuse it, with the same error,
/// and a contradicted kind claim is refused after it as before. What a hook
/// records is provisional: every judgement on the IDs (order, duplicates,
/// the shared `IdSet`) runs after the blob has been accepted.
///
/// The element walk yields a group's dense nodes before its plain ones
/// while the validator hands over the plain ones first, so a group's plain
/// node IDs wait in `plain` until the group ends. IDs are selected by the
/// element's own kind, never by the blob's claim: an unindexed blob sits in
/// all three phases and each takes its own kind from it.
struct KindIds<'s> {
    kind: ElementKind,
    scratch: &'s mut IdScratch,
}

/// One stretch of a blob's IDs, in element order.
enum IdRun {
    /// A dense group's validated `id` array: its address and length in the
    /// blob's bytes, decoded after acceptance.
    Dense { at: usize, len: usize },
    /// The next `n` IDs of `IdScratch::ids`.
    Copied(usize),
}

impl KindIds<'_> {
    fn copy(&mut self, id: i64) {
        self.scratch.ids.push(id);
        if let Some(IdRun::Copied(n)) = self.scratch.runs.last_mut() {
            *n += 1;
        } else {
            self.scratch.runs.push(IdRun::Copied(1));
        }
    }
}

impl GroupProjection for KindIds<'_> {
    fn group_begin(&mut self) {
        self.scratch.plain.clear();
    }

    fn node(&mut self, node: &WireNode<'_>, _info: Option<&WireInfo>) {
        if self.kind == ElementKind::Node {
            self.scratch.plain.push(node.id);
        }
    }

    fn way(&mut self, way: &WireWay<'_>, _info: Option<&WireInfo>) {
        if self.kind == ElementKind::Way {
            self.copy(way.id);
        }
    }

    fn relation(&mut self, relation: &WireRelation<'_>, _info: Option<&WireInfo>) {
        if self.kind == ElementKind::Relation {
            self.copy(relation.id);
        }
    }

    fn dense(
        &mut self,
        dense: &WireDenseNodes<'_>,
        _ids: usize,
        _info: Option<&WireDenseInfo<'_>>,
    ) {
        if self.kind == ElementKind::Node {
            self.scratch.runs.push(IdRun::Dense {
                at: dense.id_data.as_ptr() as usize,
                len: dense.id_data.len(),
            });
        }
    }

    fn group_end(&mut self) {
        let plain = std::mem::take(&mut self.scratch.plain);
        for &id in &plain {
            self.copy(id);
        }
        self.scratch.plain = plain;
    }
}

/// A phase worker's scratch, reused across blobs: the current blob's runs
/// and copied IDs, and the current group's plain node IDs.
#[derive(Default)]
struct IdScratch {
    runs: Vec<IdRun>,
    ids: Vec<i64>,
    plain: Vec<i64>,
}

impl IdScratch {
    /// The IDs the runs describe, in element order, read from `bytes`, the
    /// accepted blob the dense runs point into.
    fn each(&self, bytes: &[u8], mut f: impl FnMut(i64)) -> Result<()> {
        let base = bytes.as_ptr() as usize;
        let mut copied = self.ids.iter();
        for run in &self.runs {
            match *run {
                IdRun::Dense { at, len } => {
                    let ids = at
                        .checked_sub(base)
                        .and_then(|start| bytes.get(start..start.checked_add(len)?))
                        .ok_or_else(|| {
                            crate::error::Error::internal(
                                "id check: a dense id array lies outside its blob",
                            )
                        })?;
                    WayRefIter::delta_decoded(ids).for_each(&mut f);
                }
                IdRun::Copied(n) => copied.by_ref().take(n).copied().for_each(&mut f),
            }
        }
        Ok(())
    }
}

/// Validate one blob through [`KindIds`] and judge its IDs of `kind`: the
/// blob's first and last ID, count and order violations, and with
/// `atomic_ids` (`--full`) its duplicates and the IDs outside the set's
/// allocation.
fn verify_blob(
    buffer: BlockBuffer<'_>,
    kind: ElementKind,
    atomic_ids: Option<&IdSet>,
    scratch: &mut IdScratch,
) -> Result<BlobVerifyResult> {
    let elem_type = kind.as_str();
    scratch.runs.clear();
    scratch.ids.clear();
    let (KindIds { .. }, accepted) = buffer.project(|_, _| KindIds {
        kind,
        scratch: &mut *scratch,
    })?;

    let mut r = BlobVerifyResult::empty();
    let mut prev: Option<i64> = None;
    scratch.each(accepted.bytes(), |id| {
        r.count += 1;
        if r.first_id.is_none() {
            r.first_id = Some(id);
        }
        r.last_id = Some(id);
        if let Some(p) = prev
            && not_after(id, p)
        {
            r.within_violations.push(IdViolation::NonMonotonic {
                elem_type,
                id,
                prev_id: p,
            });
        }
        prev = Some(id);
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
    })?;
    // The bytes go back to their pool.
    drop(accepted);
    Ok(r)
}

/// Per-kind streaming-parallel scan. Same shape as
/// `verify_single_kind_parallel` but workers do not consult an `IdSet` and
/// the merge skips the duplicate translation step.
fn verify_single_kind_streaming(
    shared_file: &crate::read::input::InputFile,
    schedule: &[crate::scan::classify::ScheduledBlob],
    kind: ElementKind,
    max_errors_remaining: Option<usize>,
    host: &crate::host::Entered<'_>,
) -> Result<(u64, Vec<IdViolation>, u64)> {
    if schedule.is_empty() {
        return Ok((0, Vec::new(), 0));
    }
    let elem_type = kind.as_str();

    let mut per_blob: Vec<Option<BlobVerifyResult>> = (0..schedule.len()).map(|_| None).collect();

    crate::scan::classify::parallel_project_phase(
        shared_file,
        schedule,
        None,
        IdScratch::default,
        |buffer, scratch| verify_blob(buffer, kind, None, scratch),
        |seq, r| {
            per_blob[seq] = Some(r);
        },
        host,
    )?;

    // Serial merge in schedule (= file) order. duplicate_ids is always
    // empty in streaming results, so the `for id in r.duplicate_ids` loop
    // from the --full merge is dropped.
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

/// Full-mode entry. Three-phase parallel scan mirroring the check_refs step #2
/// shape. Each phase is independent of the others' results (no cross-kind
/// dependency - verify_ids only cares about per-kind monotonicity and
/// per-kind duplicates), so phase ordering is purely for phase-wall
/// attribution in the sidecar.
// This pass owns all three ID sets and their ordered phase merges.
#[allow(clippy::too_many_lines)]
fn verify_ids_full_parallel(
    pin: &PinnedInput,
    opts: &VerifyIdsOptions,
    host: &crate::host::Entered<'_>,
) -> Result<VerifyIdsReport> {
    host.invocation.marker("VERIFYIDS_SCAN_START");

    // Pread worker pool; same motivation as check_refs step #2.
    host.process.cross_thread_alloc_pool();

    // Cheap: one front read of the header and the first data blob's header.
    // parallel_classify_phase reads through its own descriptor, the
    // shared_file from build_classify_schedules_split.
    let (header_sorted, indexed) =
        crate::commands::input_front::sorted_and_indexed(pin, opts.check.direct_io, host)?;
    warn_unindexed_triple_scan(indexed, host);

    let type_filter = TypeFilter::or_all(opts.type_filter);

    let (node_schedule, way_schedule, rel_schedule, shared_file, max_ids) =
        crate::scan::classify::build_classify_schedules_split(
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

    // Accumulators for the report.
    let mut node_count: u64 = 0;
    let mut way_count: u64 = 0;
    let mut relation_count: u64 = 0;
    let mut violations: Vec<IdViolation> = Vec::new();
    let mut total_violations: u64 = 0;

    // Type-ordering check: track max data_offset per kind. After all phases
    // are dispatched, validate that max(node_offsets) < min(way_offsets) and
    // max(way_offsets) < min(relation_offsets). Uses the schedule directly
    // (no extra I/O); relies on build_classify_schedules_split delivering
    // offsets in file order, which it does.
    //
    // Non-indexed inputs: skip. `build_classify_schedules_split`
    // replicates every blob into all three per-kind schedules when
    // indexdata is missing (there's no cheap way to know a blob's
    // element kind from headers alone), so the offset comparisons
    // above span the same offset set three times and produce spurious
    // violations on correctly-ordered files. The streaming arm skips it
    // the same way, so neither arm checks type order on unindexed input:
    // that is a coverage gap of `check --ids`, recorded in
    // reference/indexdata-trust.md.
    if indexed {
        check_type_order(
            &node_schedule,
            &way_schedule,
            &rel_schedule,
            &mut violations,
            &mut total_violations,
            effective_error_limit(opts.check.max_errors),
        );
    }

    // Phase 1 - nodes
    if type_filter.nodes {
        host.invocation.marker("VERIFYIDS_NODES_START");
        let node_ids_ref = &mut node_ids;
        let (count, phase_violations, phase_total) = verify_single_kind_parallel(
            &shared_file,
            &node_schedule,
            node_ids_ref,
            ElementKind::Node,
            remaining_error_limit(opts.check.max_errors, violations.len()),
            host,
        )?;
        node_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        host.invocation.marker("VERIFYIDS_NODES_END");
    }

    // Phase 2 - ways
    if type_filter.ways {
        host.invocation.marker("VERIFYIDS_WAYS_START");
        let way_ids_ref = &mut way_ids;
        let (count, phase_violations, phase_total) = verify_single_kind_parallel(
            &shared_file,
            &way_schedule,
            way_ids_ref,
            ElementKind::Way,
            remaining_error_limit(opts.check.max_errors, violations.len()),
            host,
        )?;
        way_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        host.invocation.marker("VERIFYIDS_WAYS_END");
    }

    // Phase 3 - relations
    if type_filter.relations {
        host.invocation.marker("VERIFYIDS_RELATIONS_START");
        let relation_ids_ref = &mut relation_ids;
        let (count, phase_violations, phase_total) = verify_single_kind_parallel(
            &shared_file,
            &rel_schedule,
            relation_ids_ref,
            ElementKind::Relation,
            remaining_error_limit(opts.check.max_errors, violations.len()),
            host,
        )?;
        relation_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        host.invocation.marker("VERIFYIDS_RELATIONS_END");
    }

    host.invocation.marker("VERIFYIDS_SCAN_END");

    Ok(VerifyIdsReport {
        header_sorted,
        indexed,
        full: true,
        node_count,
        way_count,
        relation_count,
        passed: total_violations == 0,
        total_violations,
        violations,
    })
}

/// Run a single parallel phase for one element kind. Each blob returns a
/// `BlobVerifyResult`; the main thread collects them in schedule-order (via
/// a `Vec<Option<_>>` indexed by seq) and does the sequential merge:
/// cross-blob monotonicity, duplicate violation translation, count sum.
fn verify_single_kind_parallel(
    shared_file: &crate::read::input::InputFile,
    schedule: &[crate::scan::classify::ScheduledBlob],
    id_set: &mut IdSet,
    kind: ElementKind,
    max_errors_remaining: Option<usize>,
    host: &crate::host::Entered<'_>,
) -> Result<(u64, Vec<IdViolation>, u64)> {
    if schedule.is_empty() {
        return Ok((0, Vec::new(), 0));
    }
    let elem_type = kind.as_str();

    let mut per_blob: Vec<Option<BlobVerifyResult>> = (0..schedule.len()).map(|_| None).collect();
    let atomic_ids = &*id_set;
    let mut overflow = IdSet::new();

    crate::scan::classify::parallel_project_phase(
        shared_file,
        schedule,
        None,
        IdScratch::default,
        |buffer, scratch| verify_blob(buffer, kind, Some(atomic_ids), scratch),
        |seq, mut r| {
            settle_overflow(&mut overflow, &mut r, host);
            per_blob[seq] = Some(r);
        },
        host,
    )?;
    id_set.merge(overflow);

    // Serial merge in schedule (= file) order.
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

/// Verify that schedules appear in canonical file order: all node blobs
/// before all way blobs before all relation blobs. Uses data offsets from
/// the schedules directly (cheap; no extra I/O).
///
/// Emits one `TypeOrder` violation per inversion found (bounded at three:
/// node→way, node→relation, way→relation).
fn check_type_order(
    node_sched: &[crate::scan::classify::ScheduledBlob],
    way_sched: &[crate::scan::classify::ScheduledBlob],
    rel_sched: &[crate::scan::classify::ScheduledBlob],
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

    // Canonical order is nodes < ways < relations (by file offset). The
    // node-relation pair is checked directly: node<way and way<rel imply it
    // only when ways exist, and a file with no ways (a relation blob, then a
    // node blob) would otherwise pass. On a correctly ordered file
    // max_node < min_rel, so the direct check never fires there; on a
    // disordered one it may fire beside the other two, which is the bound
    // the doc above states.
    let offset = crate::scan::classify::ScheduledBlob::frame_start;
    let max_node = node_sched.iter().map(offset).max();
    let min_way = way_sched.iter().map(offset).min();
    let max_way = way_sched.iter().map(offset).max();
    let min_rel = rel_sched.iter().map(offset).min();

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
    use super::{IdViolation, check_type_order};
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
            &blobs(nodes),
            &blobs(ways),
            &blobs(rels),
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

    /// The IDs of `kind` the element walk yields, in its order: what the
    /// per-blob loop read before the projection replaced it.
    fn walked_ids(block: &crate::PrimitiveBlock, kind: crate::ElementKind) -> Vec<i64> {
        use crate::{Element, ElementKind};
        block
            .elements_skip_metadata()
            .filter_map(|el| match (&el, kind) {
                (Element::DenseNode(dn), ElementKind::Node) => Some(dn.id()),
                (Element::Node(n), ElementKind::Node) => Some(n.id()),
                (Element::Way(w), ElementKind::Way) => Some(w.id()),
                (Element::Relation(r), ElementKind::Relation) => Some(r.id()),
                _ => None,
            })
            .collect()
    }

    /// For every fixture, every truncation and every probed byte value, and
    /// every kind: the projection refuses exactly what block construction
    /// refuses, with the same message, and where it accepts it yields the
    /// element walk's IDs of that kind in the walk's order (dense nodes
    /// before plain ones within a group, groups in order).
    #[test]
    fn kind_ids_refuse_as_construction_and_match_the_element_walk() {
        use crate::ElementKind;
        use crate::read::wire_fixtures::{mutations, projection_fixtures};

        for (what, bytes) in projection_fixtures() {
            assert!(
                crate::PrimitiveBlock::from_vec(bytes.clone()).is_ok(),
                "{what}: fixture must be valid"
            );
            // One worker's scratch across every case, as a phase reuses it.
            let mut scratch = super::IdScratch::default();
            let (mut st, mut gr) = (Vec::new(), Vec::new());
            let cases =
                std::iter::once(("as built".to_owned(), bytes.clone())).chain(mutations(&bytes));
            for (how, input) in cases {
                let oracle = crate::PrimitiveBlock::from_vec(input.clone());
                for kind in [ElementKind::Node, ElementKind::Way, ElementKind::Relation] {
                    scratch.runs.clear();
                    scratch.ids.clear();
                    let buffer = crate::read::block::BlockBuffer::new(
                        input.clone(),
                        None,
                        &mut st,
                        &mut gr,
                        None,
                    );
                    let projected = match buffer.project(|_, _| super::KindIds {
                        kind,
                        scratch: &mut scratch,
                    }) {
                        Ok((super::KindIds { .. }, accepted)) => {
                            let mut ids = Vec::new();
                            scratch
                                .each(accepted.bytes(), |id| ids.push(id))
                                .map(|()| ids)
                        }
                        Err(e) => Err(e),
                    };
                    match (&oracle, projected) {
                        (Err(a), Err(b)) => {
                            assert_eq!(a.to_string(), b.to_string(), "{what}, {how}, {kind}");
                        }
                        (Ok(block), Ok(ids)) => {
                            assert_eq!(ids, walked_ids(block, kind), "{what}, {how}, {kind}");
                        }
                        (Ok(_), Err(e)) => {
                            panic!("{what}, {how}, {kind}: refused a valid block: {e}")
                        }
                        (Err(e), Ok(_)) => {
                            panic!(
                                "{what}, {how}, {kind}: accepted a block construction refuses: {e}"
                            )
                        }
                    }
                }
            }
        }
    }

    /// A blob whose claim its elements contradict is refused after
    /// validation, as the block path refuses it, and a malformed one for its
    /// malformation first.
    #[test]
    fn a_contradicted_claim_is_refused_like_construction() {
        use crate::ElementKind;
        use crate::read::wire_fixtures::projection_fixtures;

        let (_, bytes) = projection_fixtures().swap_remove(0);
        let claim = Some(ElementKind::Way);
        let project = |input: Vec<u8>| {
            let mut scratch = super::IdScratch::default();
            let (mut st, mut gr) = (Vec::new(), Vec::new());
            crate::read::block::BlockBuffer::new(input, None, &mut st, &mut gr, claim)
                .project(|_, _| super::KindIds {
                    kind: ElementKind::Node,
                    scratch: &mut scratch,
                })
                .map(drop)
        };
        let build = |input: Vec<u8>| {
            let (mut st, mut gr) = (Vec::new(), Vec::new());
            crate::read::block::BlockBuffer::new(input, None, &mut st, &mut gr, claim)
                .block()
                .map(drop)
        };
        let projected = project(bytes.clone()).expect_err("a node blob claiming ways");
        assert!(projected.kind_contradiction().is_some(), "{projected}");
        assert_eq!(
            projected.to_string(),
            build(bytes.clone())
                .expect_err("the block path refuses it")
                .to_string()
        );

        let truncated = bytes[..bytes.len() - 1].to_vec();
        let malformed = project(truncated.clone()).expect_err("truncated");
        assert!(malformed.kind_contradiction().is_none(), "{malformed}");
        assert_eq!(
            malformed.to_string(),
            build(truncated)
                .expect_err("the block path refuses it")
                .to_string()
        );
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
