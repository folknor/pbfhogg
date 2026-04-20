//! Validate referential integrity in a PBF file. Equivalent to `osmium check-refs`.

use std::path::Path;
use std::time::Instant;

use crate::{Element, MemberId};

use crate::idset::IdSet;
use crate::scan::classify::{build_classify_schedules_split, parallel_classify_phase};
use crate::BoxResult as Result;

/// A single missing reference entry (populated when `show_ids` is true).
pub struct MissingRef {
    /// The missing element, e.g. "n123".
    pub missing_type: char,
    pub missing_id: i64,
    /// The element that references it, e.g. "w456".
    pub referencing_type: char,
    pub referencing_id: i64,
}

/// Result of a referential integrity check.
pub struct RefCheckResult {
    pub node_count: u64,
    pub way_count: u64,
    pub relation_count: u64,
    pub missing_node_refs: u64,
    pub missing_way_refs: u64,
    pub missing_node_members: u64,
    pub missing_relation_members: u64,
    /// Number of relation-to-relation references that point to missing IDs.
    /// May exceed `missing_relation_members` when multiple relations reference
    /// the same missing relation.
    pub missing_relation_member_occurrences: u64,
    /// Every missing reference occurrence (populated when `show_ids` is true).
    /// Not deduplicated - each occurrence is a separate entry.
    pub missing_refs: Vec<MissingRef>,
}

impl RefCheckResult {
    /// True if all references resolve.
    pub fn is_valid(&self) -> bool {
        self.missing_node_refs == 0
            && self.missing_way_refs == 0
            && self.missing_node_members == 0
            && self.missing_relation_members == 0
    }

    /// Total number of missing references.
    pub fn total_missing(&self) -> u64 {
        self.missing_node_refs
            + self.missing_way_refs
            + self.missing_node_members
            + self.missing_relation_members
    }
}

/// Per-way-blob classify result. Bounded by blob size (~8000 elements × typical
/// ~10 refs/way). Streamed back to the main thread for merge.
#[derive(Default)]
struct WayBlobResult {
    way_count: u64,
    way_refs_checked: u64,
    missing_node_refs: Vec<i64>,
    /// Populated only when `show_ids` is true.
    missing_refs: Vec<MissingRef>,
}

/// Per-relation-blob classify result.
#[derive(Default)]
struct RelBlobResult {
    relation_count: u64,
    rel_node_members_checked: u64,
    rel_way_members_checked: u64,
    rel_rel_members_deferred: u64,
    missing_node_members: Vec<i64>,
    missing_way_refs: Vec<i64>,
    deferred_relation_refs: Vec<i64>,
    /// Parallel to `deferred_relation_refs`, populated only when `show_ids` is true.
    deferred_relation_ref_sources: Vec<i64>,
    /// Populated only when `show_ids` is true.
    missing_refs: Vec<MissingRef>,
}

/// Check referential integrity of a PBF file.
///
/// Verifies that all node references in ways point to existing nodes.
/// If `check_relations` is true, also verifies that relation members
/// (node, way, relation) point to existing elements.
///
/// Relies on PBF sort order: nodes before ways before relations. Reports
/// unique missing IDs for nodes and ways (since they precede relations and
/// are fully indexed before relation processing). Reports unique missing
/// relation IDs for relation-to-relation members (deferred to post-pass to
/// handle forward references, then deduplicated).
///
/// # Architecture
///
/// Three-phase parallel scan following the renumber_external pattern.
/// `mallopt(M_ARENA_MAX, 2)` caps glibc arenas at 2 to prevent cross-thread
/// alloc/free fragmentation from the pread worker pool. Each phase builds
/// a per-kind schedule via [`build_classify_schedule`] and dispatches
/// through [`parallel_classify_phase`]: workers pread + decompress +
/// decode + classify in parallel; the main thread merges per-blob results.
///
/// Phase 1 (nodes) writes to `node_ids` via `IdSet::set_atomic`. Phase 2
/// (ways) reads `node_ids` to check refs, optionally writes to `way_ids`.
/// Phase 3 (relations, only when `check_relations`) reads both and populates
/// `relation_ids`. Forward relation-relation references are deferred to a
/// post-pass when `relation_ids` is complete.
///
/// # Planet-scale memory usage
///
/// Uses [`IdSet`] for all three ID sets, pre-allocated to OSM ID
/// ceilings. Memory is bounded by the ID space, not the population:
/// ~1.6 GB for nodes (pre-allocated to 14 B), ~175 MB for ways (1.5 B),
/// ~3 MB for relations (25 M).
///
/// Missing-ID sets are small by construction (thousands to low millions
/// even on a broken input) and are stored as `Vec<i64>` with a final
/// `sort_unstable` + `dedup` to count unique missing IDs.
///
/// # Why this is NOT an ID-only consumer
///
/// Despite appearances, check_refs needs more than just element IDs:
/// way node refs (`w.refs()`) and relation member IDs and types
/// (`r.members()`). A selective wire-format parser that skips
/// stringtable/tags/coords/metadata but keeps IDs + refs + members
/// was on the original plan as step #3, predicated on a post-parallel
/// landing with decompression and parse roughly co-equal. The actual
/// landing put decompression overwhelmingly in front of parse at
/// planet (Europe hotpath after step #2: `decompress_blob_raw` 162 s
/// cumulative vs `parse_and_inline` 2.1 s), so a selective parser's
/// measured ceiling is a fraction of a second at Europe and a few
/// seconds at planet. The lever to pull for further gains is
/// decompression throughput (zstd input format, io_uring, direct I/O),
/// not selective parse. Revisit step #3 only if a future change makes
/// parse a meaningful share of wall again.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
#[hotpath::measure]
pub fn check_refs(path: &Path, check_relations: bool, show_ids: bool, direct_io: bool) -> Result<RefCheckResult> {
    // `direct_io` is not plumbed through the parallel pread workers yet
    // (parallel_classify_phase opens the input via a shared Arc<File>
    // without O_DIRECT). Accept silently for now; if this matters for a
    // workload it can be added to build_classify_schedule's contract.
    let _ = direct_io;

    crate::debug::emit_marker("CHECKREFS_SCAN_START");

    // Prelude: cap glibc arenas at 2 to prevent cross-thread alloc/free
    // fragmentation from the pread worker pool. Scoped to this command.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    // IdSet pre-allocated to the current OSM ID ceilings. Required
    // before any set_atomic call - set_atomic panics on unallocated chunks.
    let mut node_ids = IdSet::new();
    node_ids.pre_allocate(14_000_000_000);
    let mut way_ids = IdSet::new();
    let mut relation_ids = IdSet::new();
    if check_relations {
        way_ids.pre_allocate(1_500_000_000);
        relation_ids.pre_allocate(25_000_000);
    }

    let mut result = RefCheckResult {
        node_count: 0,
        way_count: 0,
        relation_count: 0,
        missing_node_refs: 0,
        missing_way_refs: 0,
        missing_node_members: 0,
        missing_relation_members: 0,
        missing_relation_member_occurrences: 0,
        missing_refs: Vec::new(),
    };

    let mut missing_node_refs_vec: Vec<i64> = Vec::new();
    let mut missing_way_refs_vec: Vec<i64> = Vec::new();
    let mut missing_node_members_vec: Vec<i64> = Vec::new();
    let mut deferred_relation_refs: Vec<i64> = Vec::new();
    let mut deferred_relation_ref_sources: Vec<i64> = Vec::new();
    let mut missing_refs: Vec<MissingRef> = Vec::new();

    let mut way_refs_checked: u64 = 0;
    let mut rel_node_members_checked: u64 = 0;
    let mut rel_way_members_checked: u64 = 0;
    let mut rel_rel_members_deferred: u64 = 0;

    // Single header-chain walk building all three per-kind schedules at
    // once. At Europe scale the header walk is ~15 s; three separate
    // `build_classify_schedule` calls would triple that cost.
    let (node_schedule, way_schedule, rel_schedule, shared_file) =
        build_classify_schedules_split(path)?;
    let node_blobs = node_schedule.len() as u64;
    let way_blobs = way_schedule.len() as u64;
    let relation_blobs_total = rel_schedule.len() as u64;

    // ------------------------------------------------------------------
    // Phase 1 - node scan. Workers populate `node_ids` via `set_atomic`.
    // ------------------------------------------------------------------
    crate::debug::emit_marker("CHECKREFS_NODES_START");
    {
        let node_ids_ref = &node_ids;
        parallel_classify_phase(
            &shared_file,
            &node_schedule,
            None,
            || (),
            |block, &mut ()| -> u64 {
                let mut count: u64 = 0;
                for el in block.elements_skip_metadata() {
                    match el {
                        Element::DenseNode(dn) => {
                            node_ids_ref.set_atomic(dn.id());
                            count += 1;
                        }
                        Element::Node(n) => {
                            node_ids_ref.set_atomic(n.id());
                            count += 1;
                        }
                        _ => {}
                    }
                }
                count
            },
            |_seq, count| {
                result.node_count += count;
            },
        )?;
    }
    crate::debug::emit_marker("CHECKREFS_NODES_END");
    crate::debug::emit_mallinfo2("checkrefs_after_nodes");

    // ------------------------------------------------------------------
    // Phase 2 - way scan. Workers read `node_ids` (via `get`, no atomic
    // needed now that phase 1 joined), optionally write `way_ids`, and
    // collect per-blob missing-ref vecs.
    // ------------------------------------------------------------------
    crate::debug::emit_marker("CHECKREFS_WAYS_START");
    {
        let node_ids_ref = &node_ids;
        let way_ids_ref = &way_ids;
        parallel_classify_phase(
            &shared_file,
            &way_schedule,
            None,
            || (),
            |block, &mut ()| -> WayBlobResult {
                let mut r = WayBlobResult::default();
                for el in block.elements_skip_metadata() {
                    if let Element::Way(w) = el {
                        let wid = w.id();
                        if check_relations {
                            way_ids_ref.set_atomic(wid);
                        }
                        r.way_count += 1;
                        for nref in w.refs() {
                            r.way_refs_checked += 1;
                            if !node_ids_ref.get(nref) {
                                r.missing_node_refs.push(nref);
                                if show_ids {
                                    r.missing_refs.push(MissingRef {
                                        missing_type: 'n',
                                        missing_id: nref,
                                        referencing_type: 'w',
                                        referencing_id: wid,
                                    });
                                }
                            }
                        }
                    }
                }
                r
            },
            |_seq, r| {
                result.way_count += r.way_count;
                way_refs_checked += r.way_refs_checked;
                missing_node_refs_vec.extend(r.missing_node_refs);
                if show_ids {
                    missing_refs.extend(r.missing_refs);
                }
            },
        )?;
    }
    crate::debug::emit_marker("CHECKREFS_WAYS_END");
    crate::debug::emit_mallinfo2("checkrefs_after_ways");

    // ------------------------------------------------------------------
    // Phase 3 - relation scan (only when `check_relations`). Workers read
    // both `node_ids` and `way_ids`, write `relation_ids`, and collect
    // per-blob missing/deferred vecs.
    // ------------------------------------------------------------------
    let relation_blobs: u64 = if check_relations { relation_blobs_total } else { 0 };
    if check_relations {
        crate::debug::emit_marker("CHECKREFS_RELATIONS_START");
        {
            let node_ids_ref = &node_ids;
            let way_ids_ref = &way_ids;
            let relation_ids_ref = &relation_ids;
            parallel_classify_phase(
                &shared_file,
                &rel_schedule,
                None,
                || (),
                |block, &mut ()| -> RelBlobResult {
                    let mut r = RelBlobResult::default();
                    for el in block.elements_skip_metadata() {
                        if let Element::Relation(rel) = el {
                            let rid = rel.id();
                            relation_ids_ref.set_atomic(rid);
                            r.relation_count += 1;
                            for mem in rel.members() {
                                match mem.id {
                                    MemberId::Node(id) => {
                                        r.rel_node_members_checked += 1;
                                        if !node_ids_ref.get(id) {
                                            r.missing_node_members.push(id);
                                            if show_ids {
                                                r.missing_refs.push(MissingRef {
                                                    missing_type: 'n',
                                                    missing_id: id,
                                                    referencing_type: 'r',
                                                    referencing_id: rid,
                                                });
                                            }
                                        }
                                    }
                                    MemberId::Way(id) => {
                                        r.rel_way_members_checked += 1;
                                        if !way_ids_ref.get(id) {
                                            r.missing_way_refs.push(id);
                                            if show_ids {
                                                r.missing_refs.push(MissingRef {
                                                    missing_type: 'w',
                                                    missing_id: id,
                                                    referencing_type: 'r',
                                                    referencing_id: rid,
                                                });
                                            }
                                        }
                                    }
                                    MemberId::Relation(id) => {
                                        r.rel_rel_members_deferred += 1;
                                        r.deferred_relation_refs.push(id);
                                        if show_ids {
                                            r.deferred_relation_ref_sources.push(rid);
                                        }
                                    }
                                    // Unknown member types from newer PBF producers -
                                    // skip for ref-checking since we don't know what
                                    // collection to check against.
                                    MemberId::Unknown(_, _) => {}
                                }
                            }
                        }
                    }
                    r
                },
                |_seq, r| {
                    result.relation_count += r.relation_count;
                    rel_node_members_checked += r.rel_node_members_checked;
                    rel_way_members_checked += r.rel_way_members_checked;
                    rel_rel_members_deferred += r.rel_rel_members_deferred;
                    missing_node_members_vec.extend(r.missing_node_members);
                    missing_way_refs_vec.extend(r.missing_way_refs);
                    deferred_relation_refs.extend(r.deferred_relation_refs);
                    if show_ids {
                        deferred_relation_ref_sources.extend(r.deferred_relation_ref_sources);
                        missing_refs.extend(r.missing_refs);
                    }
                },
            )?;
        }
        crate::debug::emit_marker("CHECKREFS_RELATIONS_END");
        crate::debug::emit_mallinfo2("checkrefs_after_relations");
    }

    // Occurrence counts = pre-dedup vec lengths.
    let missing_node_refs_occurrences = missing_node_refs_vec.len() as u64;
    let missing_way_refs_occurrences = missing_way_refs_vec.len() as u64;
    let missing_node_members_occurrences = missing_node_members_vec.len() as u64;

    // Dedup the per-kind missing-ref vecs to get the unique-missing counts.
    let t_dedup = Instant::now();
    let unique_len = |v: &mut Vec<i64>| -> u64 {
        v.sort_unstable();
        v.dedup();
        v.len() as u64
    };
    result.missing_node_refs = unique_len(&mut missing_node_refs_vec);
    result.missing_way_refs = unique_len(&mut missing_way_refs_vec);
    result.missing_node_members = unique_len(&mut missing_node_members_vec);
    let missing_dedup_ns = t_dedup.elapsed().as_nanos();

    // Resolve deferred relation refs against the complete relation_ids set.
    crate::debug::emit_marker("CHECKREFS_DEFERRED_RESOLVE_START");
    let t_deferred = Instant::now();
    if check_relations {
        let mut missing_relation_members_vec: Vec<i64> = Vec::new();
        let mut occurrences: u64 = 0;
        for (i, &id) in deferred_relation_refs.iter().enumerate() {
            if !relation_ids.get(id) {
                missing_relation_members_vec.push(id);
                occurrences += 1;
                if show_ids {
                    missing_refs.push(MissingRef {
                        missing_type: 'r',
                        missing_id: id,
                        referencing_type: 'r',
                        referencing_id: deferred_relation_ref_sources[i],
                    });
                }
            }
        }
        result.missing_relation_members = unique_len(&mut missing_relation_members_vec);
        result.missing_relation_member_occurrences = occurrences;
    }
    let deferred_resolve_ns = t_deferred.elapsed().as_nanos();
    crate::debug::emit_marker("CHECKREFS_DEFERRED_RESOLVE_END");

    result.missing_refs = missing_refs;

    crate::debug::emit_marker("CHECKREFS_SCAN_END");
    crate::debug::emit_mallinfo2("checkrefs_final");
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter("checkrefs_node_count", result.node_count as i64);
        crate::debug::emit_counter("checkrefs_way_count", result.way_count as i64);
        crate::debug::emit_counter("checkrefs_relation_count", result.relation_count as i64);
        crate::debug::emit_counter("checkrefs_missing_node_refs", result.missing_node_refs as i64);
        crate::debug::emit_counter("checkrefs_missing_way_refs", result.missing_way_refs as i64);
        crate::debug::emit_counter("checkrefs_missing_node_members", result.missing_node_members as i64);
        crate::debug::emit_counter("checkrefs_missing_relation_members", result.missing_relation_members as i64);

        // One-shot post-pass timers.
        let ns_to_ms = |ns: u128| (ns / 1_000_000) as i64;
        crate::debug::emit_counter("checkrefs_missing_dedup_ms", ns_to_ms(missing_dedup_ns));
        crate::debug::emit_counter("checkrefs_deferred_resolve_ms", ns_to_ms(deferred_resolve_ns));

        // Structural counters.
        crate::debug::emit_counter("checkrefs_node_blobs", node_blobs as i64);
        crate::debug::emit_counter("checkrefs_way_blobs", way_blobs as i64);
        crate::debug::emit_counter("checkrefs_relation_blobs", relation_blobs as i64);
        crate::debug::emit_counter("checkrefs_way_refs_checked", way_refs_checked as i64);
        crate::debug::emit_counter("checkrefs_rel_node_members_checked", rel_node_members_checked as i64);
        crate::debug::emit_counter("checkrefs_rel_way_members_checked", rel_way_members_checked as i64);
        crate::debug::emit_counter("checkrefs_rel_rel_members_deferred", rel_rel_members_deferred as i64);
        crate::debug::emit_counter("checkrefs_missing_node_refs_occurrences", missing_node_refs_occurrences as i64);
        crate::debug::emit_counter("checkrefs_missing_way_refs_occurrences", missing_way_refs_occurrences as i64);
        crate::debug::emit_counter("checkrefs_missing_node_members_occurrences", missing_node_members_occurrences as i64);
    }

    Ok(result)
}
