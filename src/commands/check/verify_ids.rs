//! Validate PBF ID integrity: monotonicity, type ordering, and optional duplicate detection.

use std::path::Path;

use crate::BoxResult as Result;
use crate::ElementReader;
use crate::idset::IdSet;
use crate::osm_id::osm_id_cmp;
use crate::owned::TypeFilter;

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
    /// An element ID lies outside the range `--full` can track for duplicates:
    /// negative, or above the pre-allocated ceiling for its type.
    ///
    /// Duplicate detection needs one bit per possible ID, and the bitset
    /// cannot grow once the parallel phase has started. Rather than panicking
    /// a worker - which is what used to happen, and which aborts exactly the
    /// diagnosis `check` exists to produce - the ID is reported. Negative IDs
    /// are legal OSM (JOSM editor-local staging IDs) but are not
    /// duplicate-checkable here; an ID above the ceiling means the file
    /// outgrew what the verifier sized from its own indexdata.
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
        Ok(serde_json::to_string_pretty(
            &self.to_json_value(file_name),
        )?)
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
///   `parallel_classify_phase`, no IdSet population. Monotonicity +
///   offset-based type-order only (no duplicate detection).
/// - **Full:** same shape but with a shared `IdSet` per kind populated
///   via `set_atomic_if_new` for cross-blob duplicate detection.
///
/// Both modes use the same per-blob worker shape to keep the planet-scale
/// memory floor bounded. Streaming mode previously walked
/// `for_each_pipelined` directly, which hit the documented cross-thread
/// `PrimitiveBlock` retention pattern (`src/read/pipeline.rs:66-89`,
/// ~25 GB at planet scale; OOM at 29.2 GB measured 2026-04-26 overnight).
///
/// # Planet-scale memory
///
/// Streaming mode: per-blob `BlobVerifyResult` plus the schedule vec;
/// peak anon RSS in the same ~2 GB range as `--full`.
///
/// `--full` mode: three `IdSet` pre-allocated to OSM ID ceilings
/// (~1.6 GB for nodes, 175 MB for ways, 3 MB for relations) when the
/// corresponding `type_filter` bit is set. Pre-allocation is mandatory
/// for `set_atomic_if_new` (workers panic on unallocated chunks). Total
/// full-mode RSS at planet: ~1.8 GB.
#[hotpath::measure]
pub fn verify_ids(
    path: &Path,
    opts: &VerifyIdsOptions,
    host: &crate::host::Host,
) -> Result<VerifyIdsReport> {
    if opts.full {
        return verify_ids_full_parallel(path, opts, host);
    }
    verify_ids_streaming_parallel(path, opts, host)
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
fn warn_unindexed_triple_scan(indexed: bool, host: &crate::host::Host) {
    if !indexed {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "check.ids.unindexed_triple_scan",
            "warning: input has no blob-level indexdata, so every blob is \
             decompressed three times - once per element kind, because an \
             unindexed blob has to be scheduled as all three. Generate an \
             indexed copy with `pbfhogg cat input.osm.pbf -o indexed.osm.pbf` \
             to check it in a single pass."
        );
    }
}

fn verify_ids_streaming_parallel(
    path: &Path,
    opts: &VerifyIdsOptions,
    host: &crate::host::Host,
) -> Result<VerifyIdsReport> {
    use crate::Element;

    crate::debug::emit_marker("VERIFYIDS_SCAN_START");

    // Same arena cap as `--full`. The streaming workers do no per-element
    // allocation (just reads + push to a small per-blob within_violations
    // Vec), so the contention cost measured on the pre-migration
    // time-filter snapshot path (allocation-heavy BlockBuilder scratch
    // reused across blobs) doesn't apply here.
    host.process.cross_thread_alloc_pool();

    let header_sorted = ElementReader::open_in(path, opts.check.direct_io, host)?
        .header()
        .is_sorted();
    let indexed = crate::commands::has_indexdata(path, opts.check.direct_io, &host.diagnostics)?;
    warn_unindexed_triple_scan(indexed, host);

    let type_filter = TypeFilter::or_all(opts.type_filter);

    let (node_schedule, way_schedule, rel_schedule, shared_file, _max_ids) =
        crate::scan::classify::build_classify_schedules_split(path, &host.diagnostics)?;

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
        crate::debug::emit_marker("VERIFYIDS_NODES_START");
        let (count, phase_violations, phase_total) = verify_single_kind_streaming(
            &shared_file,
            &node_schedule,
            "node",
            remaining_error_limit(opts.check.max_errors, violations.len()),
            |el| match el {
                Element::DenseNode(dn) => Some(dn.id()),
                Element::Node(n) => Some(n.id()),
                _ => None,
            },
            host,
        )?;
        node_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        crate::debug::emit_marker("VERIFYIDS_NODES_END");
    }

    if type_filter.ways {
        crate::debug::emit_marker("VERIFYIDS_WAYS_START");
        let (count, phase_violations, phase_total) = verify_single_kind_streaming(
            &shared_file,
            &way_schedule,
            "way",
            remaining_error_limit(opts.check.max_errors, violations.len()),
            |el| match el {
                Element::Way(w) => Some(w.id()),
                _ => None,
            },
            host,
        )?;
        way_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        crate::debug::emit_marker("VERIFYIDS_WAYS_END");
    }

    if type_filter.relations {
        crate::debug::emit_marker("VERIFYIDS_RELATIONS_START");
        let (count, phase_violations, phase_total) = verify_single_kind_streaming(
            &shared_file,
            &rel_schedule,
            "relation",
            remaining_error_limit(opts.check.max_errors, violations.len()),
            |el| match el {
                Element::Relation(r) => Some(r.id()),
                _ => None,
            },
            host,
        )?;
        relation_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        crate::debug::emit_marker("VERIFYIDS_RELATIONS_END");
    }

    crate::debug::emit_marker("VERIFYIDS_SCAN_END");

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

/// Per-kind streaming-parallel scan. Same shape as
/// `verify_single_kind_parallel` but workers do not consult an `IdSet` and
/// the merge skips the duplicate translation step.
#[allow(clippy::type_complexity)]
fn verify_single_kind_streaming(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    elem_type: &'static str,
    max_errors_remaining: Option<usize>,
    extract_id: impl Fn(&crate::Element) -> Option<i64> + Send + Sync,
    host: &crate::host::Host,
) -> Result<(u64, Vec<IdViolation>, u64)> {
    if schedule.is_empty() {
        return Ok((0, Vec::new(), 0));
    }

    let mut per_blob: Vec<Option<BlobVerifyResult>> = (0..schedule.len()).map(|_| None).collect();
    let extract_ref = &extract_id;

    crate::scan::classify::parallel_classify_phase(
        shared_file,
        schedule,
        None,
        || (),
        |block, _state| -> BlobVerifyResult {
            let mut r = BlobVerifyResult::empty();
            let mut prev: Option<i64> = None;
            for el in block.elements_skip_metadata() {
                if let Some(id) = extract_ref(&el) {
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
                }
            }
            r
        },
        |seq, r| {
            per_blob[seq] = Some(r);
        },
        &*host.resources,
    )?;

    // Serial merge in schedule (= file) order. duplicate_ids is always
    // empty in streaming results, so the `for id in r.duplicate_ids` loop
    // from the --full merge is dropped.
    let mut count: u64 = 0;
    let mut violations: Vec<IdViolation> = Vec::new();
    let mut total_violations: u64 = 0;
    let mut prev_last: Option<i64> = None;
    for slot in per_blob {
        let r = slot.expect("parallel_classify_phase must deliver every blob");
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
    /// IDs the shared IdSet could not represent at all - negative, or above
    /// the pre-allocated ceiling. Reported as `IdOutOfRange` violations.
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
    path: &Path,
    opts: &VerifyIdsOptions,
    host: &crate::host::Host,
) -> Result<VerifyIdsReport> {
    use crate::Element;

    crate::debug::emit_marker("VERIFYIDS_SCAN_START");

    // Pread worker pool; same motivation as check_refs step #2.
    host.process.cross_thread_alloc_pool();

    // Cheap: ElementReader::open reads the header and stops. Drop it
    // immediately; parallel_classify_phase reopens the file internally via
    // the shared_file from build_classify_schedules_split.
    let header_sorted = ElementReader::open_in(path, opts.check.direct_io, host)?
        .header()
        .is_sorted();
    let indexed = crate::commands::has_indexdata(path, opts.check.direct_io, &host.diagnostics)?;
    warn_unindexed_triple_scan(indexed, host);

    let type_filter = TypeFilter::or_all(opts.type_filter);

    let (node_schedule, way_schedule, rel_schedule, shared_file, max_ids) =
        crate::scan::classify::build_classify_schedules_split(path, &host.diagnostics)?;

    // Pre-allocate IdSets for the kinds we intend to verify, sized from the
    // file's own indexdata and floored at the live OSM ID space. The floors
    // were previously fixed ceilings, which is wrong in both directions on a
    // command whose job is to diagnose bad files: a near-future planet
    // (way IDs are already ~1.4e9 against the old 1.5e9 cap) or any
    // out-of-range ID in a degraded file panicked a worker instead of
    // producing the violation report the user asked for. Population now uses
    // `set_atomic_if_new_checked`, which reports rather than panics - see
    // `IdViolation::IdOutOfRange`.
    //
    // `id_alloc_bound` caps the opposite direction. Indexdata is unverified,
    // so sizing straight from it let a blob claiming `max_id = i64::MAX` ask
    // `pre_allocate` for an exabyte-scale bitset and take the process down
    // before the checked setters could report anything - a denial of service
    // introduced by the same change that removed the hard-coded ceilings.
    let file_len = std::fs::metadata(path).map_or(0, |m| m.len());
    let mut node_ids = IdSet::new();
    let mut way_ids = IdSet::new();
    let mut relation_ids = IdSet::new();
    if type_filter.nodes {
        node_ids.pre_allocate(super::id_alloc_bound(
            max_ids[0],
            super::NODE_ID_FLOOR,
            file_len,
        ));
    }
    if type_filter.ways {
        way_ids.pre_allocate(super::id_alloc_bound(
            max_ids[1],
            super::WAY_ID_FLOOR,
            file_len,
        ));
    }
    if type_filter.relations {
        relation_ids.pre_allocate(super::id_alloc_bound(
            max_ids[2],
            super::RELATION_ID_FLOOR,
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
    // violations on correctly-ordered files. The sequential
    // (non-full) path's element-level `check_type_order` still runs
    // when users need actual type-ordering verification on
    // non-indexed input; `--full` under `--force` just loses the
    // offset-based pre-check.
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
        crate::debug::emit_marker("VERIFYIDS_NODES_START");
        let node_ids_ref = &node_ids;
        let (count, phase_violations, phase_total) = verify_single_kind_parallel(
            &shared_file,
            &node_schedule,
            node_ids_ref,
            "node",
            remaining_error_limit(opts.check.max_errors, violations.len()),
            |el| match el {
                Element::DenseNode(dn) => Some(dn.id()),
                Element::Node(n) => Some(n.id()),
                _ => None,
            },
            host,
        )?;
        node_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        crate::debug::emit_marker("VERIFYIDS_NODES_END");
    }

    // Phase 2 - ways
    if type_filter.ways {
        crate::debug::emit_marker("VERIFYIDS_WAYS_START");
        let way_ids_ref = &way_ids;
        let (count, phase_violations, phase_total) = verify_single_kind_parallel(
            &shared_file,
            &way_schedule,
            way_ids_ref,
            "way",
            remaining_error_limit(opts.check.max_errors, violations.len()),
            |el| match el {
                Element::Way(w) => Some(w.id()),
                _ => None,
            },
            host,
        )?;
        way_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        crate::debug::emit_marker("VERIFYIDS_WAYS_END");
    }

    // Phase 3 - relations
    if type_filter.relations {
        crate::debug::emit_marker("VERIFYIDS_RELATIONS_START");
        let relation_ids_ref = &relation_ids;
        let (count, phase_violations, phase_total) = verify_single_kind_parallel(
            &shared_file,
            &rel_schedule,
            relation_ids_ref,
            "relation",
            remaining_error_limit(opts.check.max_errors, violations.len()),
            |el| match el {
                Element::Relation(r) => Some(r.id()),
                _ => None,
            },
            host,
        )?;
        relation_count = count;
        total_violations += phase_total;
        violations.extend(phase_violations);
        crate::debug::emit_marker("VERIFYIDS_RELATIONS_END");
    }

    crate::debug::emit_marker("VERIFYIDS_SCAN_END");

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
#[allow(clippy::type_complexity)]
fn verify_single_kind_parallel(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    id_set: &IdSet,
    elem_type: &'static str,
    max_errors_remaining: Option<usize>,
    extract_id: impl Fn(&crate::Element) -> Option<i64> + Send + Sync,
    host: &crate::host::Host,
) -> Result<(u64, Vec<IdViolation>, u64)> {
    if schedule.is_empty() {
        return Ok((0, Vec::new(), 0));
    }

    let mut per_blob: Vec<Option<BlobVerifyResult>> = (0..schedule.len()).map(|_| None).collect();
    let extract_ref = &extract_id;

    crate::scan::classify::parallel_classify_phase(
        shared_file,
        schedule,
        None,
        || (),
        |block, _state| -> BlobVerifyResult {
            let mut r = BlobVerifyResult::empty();
            let mut prev: Option<i64> = None;
            for el in block.elements_skip_metadata() {
                if let Some(id) = extract_ref(&el) {
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
                    // `None` = the ID is negative or above the pre-allocated
                    // ceiling, so duplicate detection cannot cover it. Report
                    // it as its own violation instead of panicking the worker
                    // (which is what `set_atomic_if_new` does, and which
                    // aborts the whole check on exactly the malformed input
                    // it exists to diagnose).
                    match id_set.set_atomic_if_new_checked(id) {
                        Some(true) => {}
                        Some(false) => r.duplicate_ids.push(id),
                        None => r.out_of_range_ids.push(id),
                    }
                }
            }
            r
        },
        |seq, r| {
            per_blob[seq] = Some(r);
        },
        &*host.resources,
    )?;

    // Serial merge in schedule (= file) order.
    let mut count: u64 = 0;
    let mut violations: Vec<IdViolation> = Vec::new();
    let mut total_violations: u64 = 0;
    let mut prev_last: Option<i64> = None;
    for slot in per_blob {
        let r = slot.expect("parallel_classify_phase must deliver every blob");
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

/// Verify that schedules appear in canonical file order: all node blobs
/// before all way blobs before all relation blobs. Uses data offsets from
/// the schedules directly (cheap; no extra I/O).
///
/// Emits one `TypeOrder` violation per inversion found (bounded at three:
/// node→way, node→relation, way→relation).
fn check_type_order(
    node_sched: &[(usize, u64, usize)],
    way_sched: &[(usize, u64, usize)],
    rel_sched: &[(usize, u64, usize)],
    violations: &mut Vec<IdViolation>,
    total_violations: &mut u64,
    max_errors: Option<usize>,
) {
    let record = |after: &'static str,
                  found: &'static str,
                  violations: &mut Vec<IdViolation>,
                  total_violations: &mut u64| {
        *total_violations += 1;
        if should_store_violation(violations.len(), max_errors) {
            violations.push(IdViolation::TypeOrder { found, after });
        }
    };

    // Canonical order is nodes < ways < relations (by file offset). The two
    // pairwise checks below are sufficient: node<way and way<rel give
    // node<rel transitively. A third (max_rel vs min_node) would fire on
    // every correctly-ordered file.
    let max_node = node_sched.iter().map(|(_, o, _)| *o).max();
    let min_way = way_sched.iter().map(|(_, o, _)| *o).min();
    let max_way = way_sched.iter().map(|(_, o, _)| *o).max();
    let min_rel = rel_sched.iter().map(|(_, o, _)| *o).min();

    if let (Some(mn), Some(mw)) = (max_node, min_way)
        && mn > mw
    {
        record("node", "way", violations, total_violations);
    }
    if let (Some(mw), Some(mr)) = (max_way, min_rel)
        && mw > mr
    {
        record("way", "relation", violations, total_violations);
    }
}
