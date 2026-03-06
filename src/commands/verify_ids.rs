//! Validate PBF ID integrity: monotonicity, type ordering, and optional duplicate detection.

use std::path::Path;

use roaring::RoaringTreemap;

use super::{Result, TypeFilter};
use crate::ElementReader;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Configuration for the `verify_ids` command.
pub struct VerifyIdsOptions<'a> {
    /// When true, use `RoaringTreemap` per type to detect duplicate IDs.
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
    Duplicate {
        elem_type: &'static str,
        id: i64,
    },
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
        Ok(serde_json::to_string_pretty(&self.to_json_value(file_name))?)
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
// Scan state
// ---------------------------------------------------------------------------

/// Mutable state for the streaming ID scan pass.
///
/// Extracted as a struct to keep `process_element` and per-type check methods
/// small enough to satisfy `cognitive_complexity = "deny"`.
struct ScanState {
    prev_node_id: i64,
    prev_way_id: i64,
    prev_rel_id: i64,
    /// 0 = node, 1 = way, 2 = relation. Tracks canonical ordering.
    last_type_rank: u8,
    node_count: u64,
    way_count: u64,
    relation_count: u64,
    violations: Vec<IdViolation>,
    total_violations: u64,
    max_errors: usize,
    type_filter: TypeFilter,
    node_ids: Option<RoaringTreemap>,
    way_ids: Option<RoaringTreemap>,
    relation_ids: Option<RoaringTreemap>,
}

impl ScanState {
    fn new(opts: &VerifyIdsOptions<'_>) -> Self {
        let type_filter = opts.type_filter.map_or_else(TypeFilter::all, TypeFilter::parse);
        Self {
            prev_node_id: i64::MIN,
            prev_way_id: i64::MIN,
            prev_rel_id: i64::MIN,
            last_type_rank: 0,
            node_count: 0,
            way_count: 0,
            relation_count: 0,
            violations: Vec::new(),
            total_violations: 0,
            max_errors: opts.max_errors,
            type_filter,
            node_ids: if opts.full { Some(RoaringTreemap::new()) } else { None },
            way_ids: if opts.full { Some(RoaringTreemap::new()) } else { None },
            relation_ids: if opts.full { Some(RoaringTreemap::new()) } else { None },
        }
    }

    /// Record a violation, incrementing the total count and optionally storing it.
    fn record_violation(&mut self, v: IdViolation) {
        self.total_violations += 1;
        if self.violations.len() < self.max_errors {
            self.violations.push(v);
        }
    }

    /// Dispatch an element to the appropriate per-type checker.
    fn process_element(&mut self, element: &crate::Element) {
        match element {
            crate::Element::DenseNode(dn) => {
                if self.type_filter.nodes {
                    self.check_node(dn.id());
                }
            }
            crate::Element::Node(n) => {
                if self.type_filter.nodes {
                    self.check_node(n.id());
                }
            }
            crate::Element::Way(w) => {
                if self.type_filter.ways {
                    self.check_way(w.id());
                }
            }
            crate::Element::Relation(r) => {
                if self.type_filter.relations {
                    self.check_relation(r.id());
                }
            }
        }
    }

    /// Check a node ID for monotonicity, type ordering, and (optionally) duplicates.
    fn check_node(&mut self, id: i64) {
        self.node_count += 1;
        self.check_type_order(0, "node");
        self.check_monotonic("node", id, self.prev_node_id);
        self.prev_node_id = id;
        check_duplicate(&mut self.node_ids, &mut self.violations, &mut self.total_violations, self.max_errors, "node", id);
    }

    /// Check a way ID for monotonicity, type ordering, and (optionally) duplicates.
    fn check_way(&mut self, id: i64) {
        self.way_count += 1;
        self.check_type_order(1, "way");
        self.check_monotonic("way", id, self.prev_way_id);
        self.prev_way_id = id;
        check_duplicate(&mut self.way_ids, &mut self.violations, &mut self.total_violations, self.max_errors, "way", id);
    }

    /// Check a relation ID for monotonicity, type ordering, and (optionally) duplicates.
    fn check_relation(&mut self, id: i64) {
        self.relation_count += 1;
        self.check_type_order(2, "relation");
        self.check_monotonic("relation", id, self.prev_rel_id);
        self.prev_rel_id = id;
        check_duplicate(&mut self.relation_ids, &mut self.violations, &mut self.total_violations, self.max_errors, "relation", id);
    }

    /// Verify that element types appear in canonical order (nodes < ways < relations).
    fn check_type_order(&mut self, rank: u8, type_name: &'static str) {
        if rank < self.last_type_rank {
            let after = rank_to_name(self.last_type_rank);
            self.record_violation(IdViolation::TypeOrder {
                found: type_name,
                after,
            });
        }
        self.last_type_rank = rank;
    }

    /// Verify that the current ID is strictly greater than the previous ID of the same type.
    fn check_monotonic(&mut self, elem_type: &'static str, id: i64, prev_id: i64) {
        if id <= prev_id && prev_id != i64::MIN {
            self.record_violation(IdViolation::NonMonotonic {
                elem_type,
                id,
                prev_id,
            });
        }
    }
}

/// Check for duplicate ID in the given treemap (free function to avoid borrow conflicts).
///
/// When the treemap is `None` (streaming mode), this is a no-op. When `Some`,
/// inserts the ID and records a violation if it was already present.
#[allow(clippy::cast_sign_loss)]
fn check_duplicate(
    set: &mut Option<RoaringTreemap>,
    violations: &mut Vec<IdViolation>,
    total_violations: &mut u64,
    max_errors: usize,
    elem_type: &'static str,
    id: i64,
) {
    if let Some(set) = set.as_mut()
        && !set.insert(id.cast_unsigned())
    {
        *total_violations += 1;
        if violations.len() < max_errors {
            violations.push(IdViolation::Duplicate { elem_type, id });
        }
    }
}

fn rank_to_name(rank: u8) -> &'static str {
    match rank {
        0 => "node",
        1 => "way",
        _ => "relation",
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Validate PBF ID integrity: monotonicity, type ordering, and optional duplicate detection.
///
/// Streams through all elements in a single pipelined pass. When `full` is true,
/// maintains per-type `RoaringTreemap` sets to detect duplicate IDs (increases
/// memory usage from O(1) to O(n) in the number of unique IDs). Without `full`,
/// only monotonicity and type ordering are checked in constant memory.
///
/// # Planet-scale memory (full mode)
///
/// Uses `RoaringTreemap` (same as `check_refs`) for ID storage. For the full
/// planet (~10B nodes, ~1B ways, ~17M relations), the treemaps consume ~2-3 GB.
/// Streaming mode (default) uses constant memory.
#[hotpath::measure]
pub fn verify_ids(path: &Path, opts: &VerifyIdsOptions<'_>) -> Result<VerifyIdsReport> {
    let reader = ElementReader::open(path, opts.direct_io)?;
    let header_sorted = reader.header().is_sorted();
    let indexed = crate::commands::has_indexdata(path, opts.direct_io)?;

    let mut state = ScanState::new(opts);

    reader.for_each_pipelined(|element| {
        state.process_element(&element);
    })?;

    Ok(VerifyIdsReport {
        header_sorted,
        indexed,
        full: opts.full,
        node_count: state.node_count,
        way_count: state.way_count,
        relation_count: state.relation_count,
        passed: state.total_violations == 0,
        total_violations: state.total_violations,
        violations: state.violations,
    })
}
