//! Validate PBF ID integrity: monotonicity, type ordering, and optional duplicate detection.

use std::path::Path;

use crate::BoxResult as Result;
use crate::ElementReader;
use crate::idset::IdSet;
use crate::owned::TypeFilter;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Configuration for the `verify_ids` command.
pub struct VerifyIdsOptions<'a> {
    /// When true, use `IdSet` per type to detect duplicate IDs.
    /// Increases memory usage but catches duplicates that monotonicity alone cannot.
    pub full: bool,
    /// Filter by element type (comma-separated: "node", "way", "relation").
    /// `None` means check all types.
    pub type_filter: Option<&'a str>,
    /// Maximum number of violations to store in the report (capped for memory).
    pub max_errors: usize,
    /// Whether to use O_DIRECT for reading.
    pub direct_io: bool,
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
    /// Stored violations (up to `max_errors`).
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

    /// Print a human-readable summary to stdout.
    pub fn print_human(&self, file_name: &str) {
        println!("Verify IDs: {file_name}");
        println!("  Header sorted: {}", yes_no(self.header_sorted));
        println!("  Indexed: {}", yes_no(self.indexed));
        print_mode_line(self.full);
        println!();

        println!(
            "Scanned {} nodes, {} ways, {} relations",
            fmt_count(self.node_count),
            fmt_count(self.way_count),
            fmt_count(self.relation_count),
        );
        println!();

        if self.passed {
            println!("ID integrity: OK");
        } else {
            print_violations(self);
        }
    }
}

// ---------------------------------------------------------------------------
// Human-output helpers (keep cognitive complexity out of print_human)
// ---------------------------------------------------------------------------

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// Print the mode line (streaming vs full).
fn print_mode_line(full: bool) {
    if full {
        println!("  Mode: full (duplicate detection)");
    } else {
        println!("  Mode: streaming");
    }
}

/// Print the violation summary and individual violation lines.
fn print_violations(report: &VerifyIdsReport) {
    let showing = report.violations.len();
    let total = report.total_violations;

    println!("{total} violations (showing first {showing} of {total}):");

    for v in &report.violations {
        print_single_violation(v);
    }

    println!();
    println!("ID integrity: FAILED");
}

/// Print one violation line.
fn print_single_violation(v: &IdViolation) {
    match v {
        IdViolation::NonMonotonic {
            elem_type,
            id,
            prev_id,
        } => {
            println!("  {elem_type} {id}: non-monotonic (previous: {prev_id})");
        }
        IdViolation::Duplicate { elem_type, id } => {
            println!("  {elem_type} {id}: duplicate");
        }
        IdViolation::TypeOrder { found, after } => {
            println!("  type order: {found} after {after}");
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
pub fn verify_ids(path: &Path, opts: &VerifyIdsOptions<'_>) -> Result<VerifyIdsReport> {
    if opts.full {
        return verify_ids_full_parallel(path, opts);
    }
    verify_ids_streaming_parallel(path, opts)
}

// ---------------------------------------------------------------------------
// Parallel streaming-mode implementation
// ---------------------------------------------------------------------------
//
// Mirrors `verify_ids_full_parallel` minus the IdSet allocation and
// duplicate-detection pass. Workers report `BlobVerifyResult` with
// `duplicate_ids` always empty; the serial merge skips the duplicate
// translation step.

#[allow(clippy::too_many_lines)]
fn verify_ids_streaming_parallel(
    path: &Path,
    opts: &VerifyIdsOptions<'_>,
) -> Result<VerifyIdsReport> {
    use crate::Element;

    crate::debug::emit_marker("VERIFYIDS_SCAN_START");

    // Same arena cap as `--full`. The streaming workers do no per-element
    // allocation (just reads + push to a small per-blob within_violations
    // Vec), so the regression measured on time-filter (where workers do
    // allocation-heavy BlockBuilder re-encode) doesn't apply here.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    let header_sorted = ElementReader::open(path, opts.direct_io)?
        .header()
        .is_sorted();
    let indexed = crate::commands::has_indexdata(path, opts.direct_io)?;

    let type_filter = opts
        .type_filter
        .map_or_else(TypeFilter::all, TypeFilter::parse);

    let (node_schedule, way_schedule, rel_schedule, shared_file) =
        crate::scan::classify::build_classify_schedules_split(path)?;

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
            opts.max_errors,
        );
    }

    if type_filter.nodes {
        crate::debug::emit_marker("VERIFYIDS_NODES_START");
        let (count, phase_violations, phase_total) = verify_single_kind_streaming(
            &shared_file,
            &node_schedule,
            "node",
            opts.max_errors.saturating_sub(violations.len()),
            |el| match el {
                Element::DenseNode(dn) => Some(dn.id()),
                Element::Node(n) => Some(n.id()),
                _ => None,
            },
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
            opts.max_errors.saturating_sub(violations.len()),
            |el| match el {
                Element::Way(w) => Some(w.id()),
                _ => None,
            },
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
            opts.max_errors.saturating_sub(violations.len()),
            |el| match el {
                Element::Relation(r) => Some(r.id()),
                _ => None,
            },
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
    max_errors_remaining: usize,
    extract_id: impl Fn(&crate::Element) -> Option<i64> + Send + Sync,
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
                        && id <= p
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
            if violations.len() < max_errors_remaining {
                violations.push(v);
            }
        }
        if let (Some(pl), Some(fi)) = (prev_last, r.first_id)
            && fi <= pl
        {
            total_violations += 1;
            if violations.len() < max_errors_remaining {
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
/// Kept small: the main-thread merge sees one of these per blob and aggregates
/// in schedule-order. Bounded by blob size (~8000 elements) so it streams
/// cheaply back over the parallel_classify_phase channel.
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
}

impl BlobVerifyResult {
    fn empty() -> Self {
        Self {
            first_id: None,
            last_id: None,
            count: 0,
            within_violations: Vec::new(),
            duplicate_ids: Vec::new(),
        }
    }
}

/// Full-mode entry. Three-phase parallel scan mirroring the check_refs step #2
/// shape. Each phase is independent of the others' results (no cross-kind
/// dependency - verify_ids only cares about per-kind monotonicity and
/// per-kind duplicates), so phase ordering is purely for phase-wall
/// attribution in the sidecar.
#[allow(clippy::too_many_lines)]
fn verify_ids_full_parallel(path: &Path, opts: &VerifyIdsOptions<'_>) -> Result<VerifyIdsReport> {
    use crate::Element;

    crate::debug::emit_marker("VERIFYIDS_SCAN_START");

    // mallopt prelude - same motivation as check_refs step #2: cap glibc
    // arenas at 2 to prevent cross-thread alloc/free fragmentation in the
    // pread worker pool.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    // Cheap: ElementReader::open reads the header and stops. Drop it
    // immediately; parallel_classify_phase reopens the file internally via
    // the shared_file from build_classify_schedules_split.
    let header_sorted = ElementReader::open(path, opts.direct_io)?
        .header()
        .is_sorted();
    let indexed = crate::commands::has_indexdata(path, opts.direct_io)?;

    let type_filter = opts
        .type_filter
        .map_or_else(TypeFilter::all, TypeFilter::parse);

    // Pre-allocate IdSets for the kinds we intend to verify.
    let mut node_ids = IdSet::new();
    let mut way_ids = IdSet::new();
    let mut relation_ids = IdSet::new();
    if type_filter.nodes {
        node_ids.pre_allocate(14_000_000_000);
    }
    if type_filter.ways {
        way_ids.pre_allocate(1_500_000_000);
    }
    if type_filter.relations {
        relation_ids.pre_allocate(25_000_000);
    }

    let (node_schedule, way_schedule, rel_schedule, shared_file) =
        crate::scan::classify::build_classify_schedules_split(path)?;

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
            opts.max_errors,
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
            opts.max_errors.saturating_sub(violations.len()),
            |el| match el {
                Element::DenseNode(dn) => Some(dn.id()),
                Element::Node(n) => Some(n.id()),
                _ => None,
            },
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
            opts.max_errors.saturating_sub(violations.len()),
            |el| match el {
                Element::Way(w) => Some(w.id()),
                _ => None,
            },
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
            opts.max_errors.saturating_sub(violations.len()),
            |el| match el {
                Element::Relation(r) => Some(r.id()),
                _ => None,
            },
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
    max_errors_remaining: usize,
    extract_id: impl Fn(&crate::Element) -> Option<i64> + Send + Sync,
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
                        && id <= p
                    {
                        r.within_violations.push(IdViolation::NonMonotonic {
                            elem_type,
                            id,
                            prev_id: p,
                        });
                    }
                    prev = Some(id);
                    if !id_set.set_atomic_if_new(id) {
                        r.duplicate_ids.push(id);
                    }
                }
            }
            r
        },
        |seq, r| {
            per_blob[seq] = Some(r);
        },
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
            if violations.len() < max_errors_remaining {
                violations.push(v);
            }
        }
        if let (Some(pl), Some(fi)) = (prev_last, r.first_id)
            && fi <= pl
        {
            total_violations += 1;
            if violations.len() < max_errors_remaining {
                violations.push(IdViolation::NonMonotonic {
                    elem_type,
                    id: fi,
                    prev_id: pl,
                });
            }
        }
        for id in r.duplicate_ids {
            total_violations += 1;
            if violations.len() < max_errors_remaining {
                violations.push(IdViolation::Duplicate { elem_type, id });
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
    max_errors: usize,
) {
    let record = |after: &'static str,
                  found: &'static str,
                  violations: &mut Vec<IdViolation>,
                  total_violations: &mut u64| {
        *total_violations += 1;
        if violations.len() < max_errors {
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
