//! Validate referential integrity in a PBF file. Equivalent to `osmium check-refs`.

use crate::counters::Stopwatch;

use crate::{Element, MemberId};

use crate::error::Result;
use crate::idset::IdSet;
use crate::read::input::InputSource;
use crate::scan::classify::{build_classify_schedules_split, parallel_classify_phase};
use crate::tunables::{NODE_ID_FLOOR, RELATION_ID_FLOOR, WAY_ID_FLOOR};

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
    /// Missing reference occurrences retained for display when `show_ids` is
    /// true. Limited by the shared `max_errors` value unless it is zero.
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

/// Per-way-blob classify result. Its retained references follow the contents
/// of that input blob. Streamed back to the main thread for merge.
#[derive(Default)]
struct WayBlobResult {
    way_count: u64,
    overflow_ids: Vec<i64>,
    way_refs_checked: u64,
    /// Missing node refs in this blob, counted before `missing_node_refs`
    /// is deduplicated.
    missing_node_ref_occurrences: u64,
    /// The distinct missing node IDs, sorted (`sort_dedup`).
    missing_node_refs: Vec<i64>,
    /// Populated only when `show_ids` is true.
    missing_refs: Vec<MissingRef>,
}

/// Per-relation-blob classify result. The two missing-ID vectors are sorted
/// and deduplicated like `WayBlobResult::missing_node_refs`, their
/// occurrences counted first.
#[derive(Default)]
struct RelBlobResult {
    relation_count: u64,
    overflow_ids: Vec<i64>,
    rel_node_members_checked: u64,
    rel_way_members_checked: u64,
    rel_rel_members_deferred: u64,
    missing_node_member_occurrences: u64,
    missing_way_ref_occurrences: u64,
    missing_node_members: Vec<i64>,
    missing_way_refs: Vec<i64>,
    deferred_relation_refs: Vec<i64>,
    /// Parallel to `deferred_relation_refs`, populated only when `show_ids` is true.
    deferred_relation_ref_sources: Vec<i64>,
    /// Populated only when `show_ids` is true.
    missing_refs: Vec<MissingRef>,
}

/// Options for [`check_refs`].
#[derive(Debug, Clone, Copy)]
pub struct CheckRefsOptions {
    /// Options shared with the ID and index checks. `direct_io` is accepted
    /// and has no effect: the check reads only through the schedule walk and
    /// the parallel readers' unaligned blob preads.
    pub check: super::CheckOptions,
    /// Also check relation member references.
    pub check_relations: bool,
    /// Retain missing reference IDs up to `check.max_errors`, or all if zero.
    pub show_ids: bool,
}

/// Sort and deduplicate one blob's missing IDs in the worker, so the serial
/// merge inserts each distinct ID once and in order.
fn sort_dedup(ids: &mut Vec<i64>) {
    ids.sort_unstable();
    ids.dedup();
}

/// A negative element ID the check would have to track. Negative IDs are
/// invalid input (ADR-0002) and the tracking sets cannot hold them, so a
/// check that went on would report every reference to the element as
/// missing.
#[cold]
fn negative_id_error(kind: &str, id: i64) -> crate::Error {
    crate::Error::invalid_data(format!(
        "{kind} {id} has a negative ID; the reference check does not check \
         negative IDs, which no production PBF carries"
    ))
}

/// A checked reference to a negative ID (see [`negative_id_error`]).
#[cold]
fn negative_ref_error(from: char, from_id: i64, to: char, to_id: i64) -> crate::Error {
    crate::Error::invalid_data(format!(
        "{from}{from_id} references negative ID {to}{to_id}; the reference \
         check does not check negative IDs, which no production PBF carries"
    ))
}

/// A relation member of a type the check cannot resolve against any of its
/// sets. Passing it unchecked would certify a reference nobody looked at.
#[cold]
fn unknown_member_error(rid: i64, raw_type: i32, id: i64) -> crate::Error {
    crate::Error::invalid_data(format!(
        "r{rid} has member {id} of unsupported member type {raw_type}; the \
         reference check resolves only node, way and relation members"
    ))
}

fn should_store_missing_ref(current_len: usize, max_errors: usize) -> bool {
    max_errors == 0 || current_len < max_errors
}

fn append_capped_missing_refs(
    target: &mut Vec<MissingRef>,
    entries: impl IntoIterator<Item = MissingRef>,
    max_errors: usize,
) {
    let remaining = if max_errors == 0 {
        usize::MAX
    } else {
        max_errors.saturating_sub(target.len())
    };
    target.extend(entries.into_iter().take(remaining));
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
/// # Errors
///
/// `InvalidData` for a negative ID the check would compare and for a
/// relation member of an unknown type (see "Negative IDs" below), besides
/// the read and decode errors of any scan.
///
/// # Architecture
///
/// Three-phase parallel scan following the renumber_external pattern.
/// [`crate::host::ProcessActions::cross_thread_alloc_pool`] caps glibc arenas to prevent
/// cross-thread alloc/free fragmentation from the pread worker pool. Each
/// phase builds a per-kind schedule via [`build_classify_schedule`] and
/// dispatches through [`parallel_classify_phase`]: workers pread +
/// decompress + decode + classify in parallel; the main thread merges
/// per-blob results.
///
/// Phase 1 (nodes) writes to `node_ids` via `IdSet::set_atomic`. Phase 2
/// (ways) reads `node_ids` to check refs, optionally writes to `way_ids`.
/// Phase 3 (relations, only when `check_relations`) reads both and populates
/// `relation_ids`. Forward relation-relation references are deferred to a
/// post-pass when `relation_ids` is complete.
///
/// # Planet-scale memory usage
///
/// Uses [`IdSet`] for all three ID sets, each pre-allocated as a bitset over
/// the ID range `check::id_alloc_bound` chooses (at least the live-OSM range
/// for the kind, whatever the file's size) and grown after each phase by the
/// IDs above it, which the phase's merge collects into a side set as results
/// arrive. The node set is about 1.6 GB, ways about 175 MB and relations
/// about 3 MB.
///
/// Missing node and way references go into one `IdSet` per reported
/// category, so their cost follows the range of missing IDs, not the number
/// of missing references: a ways-only planet, every ref missing, costs about
/// one node-range bitset. Workers keep the `Vec` push in their hot loop and
/// sort and deduplicate each blob's misses; the serial merge inserts them
/// (the push-then-drain shape `reference/performance-history.md` records,
/// never a set insertion per element). A file with no missing references
/// allocates nothing for them.
///
/// Two stores still grow with occurrences. Relation-to-relation members are
/// deferred until every relation ID is known, one `i64` each (plus their
/// source under `show_ids`); a valid file holds few, but nothing bounds
/// them. And `max_errors` caps retained display details only when non-zero:
/// `max_errors == 0` keeps one detail per missing occurrence.
///
/// # Negative IDs
///
/// Negative IDs are invalid input (ADR-0002) and the tracking sets cannot
/// hold them, so a negative ID the check would compare is refused: a node,
/// a way under `check_relations` (the only mode that tracks way IDs), a
/// relation, and every checked reference. The refusals sit where a set
/// lookup or insertion has already failed, except for relation members,
/// which are deferred unlooked-up and take one sign compare. A member of an
/// unknown type is refused too, since no set can resolve it.
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
///
/// # Input
///
/// Opened once (the schedule walk, whose descriptor the worker pools
/// preread), so this takes no pin of its own; a caller that composes it with
/// other checks on the same input pins once and hands the pin here, and the
/// walk opens through it. That walk reads by position, so the input has to be
/// seekable: a FIFO is not checked.
///
/// # Kind claims
///
/// Each phase reads the blobs whose indexdata claims its kind, and a decoded
/// element of another kind in one of them stops the check
/// ([`CheckOutcome::Stopped`](super::CheckOutcome)): the phase that needed
/// that element never reads the blob. A blob claiming relations is decoded
/// only under `check_relations`, so without it a lie there goes unseen;
/// `check --index` is the verifier of every claim.
#[hotpath::measure]
pub fn check_refs<'a>(
    input: impl Into<InputSource<'a>>,
    opts: &CheckRefsOptions,
    host: &crate::host::Host,
) -> Result<super::CheckOutcome<RefCheckResult>> {
    let host = &host.enter();
    let input = input.into();
    host.invocation
        .decided(|| super::CheckOutcome::stop_on_contradiction(check_refs_inner(input, opts, host)))
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn check_refs_inner(
    input: InputSource<'_>,
    opts: &CheckRefsOptions,
    host: &crate::host::Entered<'_>,
) -> Result<RefCheckResult> {
    let check_relations = opts.check_relations;
    let show_ids = opts.show_ids;
    let max_errors = opts.check.max_errors;
    // `opts.check.direct_io` is accepted and has no effect: every read here
    // is the schedule walk or a `parallel_classify_phase` pread of an
    // unaligned blob range, neither of which takes O_DIRECT. The CLI says so
    // when the flag is given.

    host.invocation.marker("CHECKREFS_SCAN_START");

    // Pread worker pool allocates decoded blocks; the main thread frees
    // them after the merge.
    host.process.cross_thread_alloc_pool();

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

    // One set per reported category - `total_missing` sums the categories,
    // so a node missing from a way and from a relation counts in both. The
    // merge inserts each blob's distinct missing IDs, so memory follows the
    // missing ID range rather than the reference count: a ways-only file,
    // where every ref is missing, used to retain 8 bytes per reference.
    let mut missing_nodes_in_ways = IdSet::new();
    let mut missing_ways_in_relations = IdSet::new();
    let mut missing_nodes_in_relations = IdSet::new();
    let mut missing_node_refs_occurrences: u64 = 0;
    let mut missing_way_refs_occurrences: u64 = 0;
    let mut missing_node_members_occurrences: u64 = 0;
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
    let (node_schedule, way_schedule, rel_schedule, shared_file, max_ids) =
        build_classify_schedules_split(input, crate::scan::classify::KindClaims::Refuse, host)?;

    // Bound indexdata-driven allocation, then insert observed positive IDs
    // above that allocation after each parallel phase joins. A regional
    // extract retains global IDs; its file size is not a validity ceiling.
    // The size is an fstat of the walk's own descriptor, the one every phase
    // reads, so it describes that file even when `input` is a bare path a
    // stat would resolve again.
    let file_len = shared_file
        .file()
        .metadata()
        .map_err(|e| {
            crate::Error::from(e).context(format!("failed to stat {}", input.path().display()))
        })?
        .len();
    let mut node_ids = IdSet::new();
    node_ids.pre_allocate(super::id_alloc_bound(max_ids[0], NODE_ID_FLOOR, file_len));
    let mut way_ids = IdSet::new();
    let mut relation_ids = IdSet::new();
    if check_relations {
        way_ids.pre_allocate(super::id_alloc_bound(max_ids[1], WAY_ID_FLOOR, file_len));
        relation_ids.pre_allocate(super::id_alloc_bound(
            max_ids[2],
            RELATION_ID_FLOOR,
            file_len,
        ));
    }

    let node_blobs = node_schedule.len() as u64;
    let way_blobs = way_schedule.len() as u64;
    let relation_blobs_total = rel_schedule.len() as u64;

    // ------------------------------------------------------------------
    // Phase 1 - node scan. Workers populate `node_ids` via `set_atomic`.
    // ------------------------------------------------------------------
    // Each phase inserts IDs above its pre-allocation into a side set from
    // its serial merge and moves it into the phase's own set after the join
    // (see `check::insert_overflow_ids`).
    let mut overflow = IdSet::new();
    host.invocation.marker("CHECKREFS_NODES_START");
    {
        let node_ids_ref = &node_ids;
        parallel_classify_phase(
            &shared_file,
            &node_schedule,
            None,
            || (),
            |block, &mut ()| -> crate::Result<(u64, Vec<i64>)> {
                let mut count: u64 = 0;
                let mut overflow = Vec::new();
                for el in block.elements_skip_metadata() {
                    let id = match el {
                        Element::DenseNode(dn) => dn.id(),
                        Element::Node(n) => n.id(),
                        _ => continue,
                    };
                    if !node_ids_ref.set_atomic_checked(id) {
                        if id < 0 {
                            return Err(negative_id_error("node", id));
                        }
                        overflow.push(id);
                    }
                    count += 1;
                }
                Ok((count, overflow))
            },
            |_seq, (count, overflow_ids)| {
                super::insert_overflow_ids(&mut overflow, overflow_ids, host);
                result.node_count += count;
            },
            host,
        )?;
    }
    node_ids.merge(std::mem::replace(&mut overflow, IdSet::new()));
    host.invocation.marker("CHECKREFS_NODES_END");
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; this sample is inert unless the invocation has a sink.
    crate::debug::emit_mallinfo2(&host.invocation, "checkrefs_after_nodes");

    // ------------------------------------------------------------------
    // Phase 2 - way scan. Workers read `node_ids` (via `get`, no atomic
    // needed now that phase 1 joined), optionally write `way_ids`, and
    // collect per-blob missing-ref vecs.
    // ------------------------------------------------------------------
    host.invocation.marker("CHECKREFS_WAYS_START");
    {
        let node_ids_ref = &node_ids;
        let way_ids_ref = &way_ids;
        parallel_classify_phase(
            &shared_file,
            &way_schedule,
            None,
            || (),
            |block, &mut ()| -> crate::Result<WayBlobResult> {
                let mut r = WayBlobResult::default();
                for el in block.elements_skip_metadata() {
                    if let Element::Way(w) = el {
                        let wid = w.id();
                        // Way IDs are tracked only for the relation phase;
                        // without it no comparison reads them, so a negative
                        // one is refused only here.
                        if check_relations && !way_ids_ref.set_atomic_checked(wid) {
                            if wid < 0 {
                                return Err(negative_id_error("way", wid));
                            }
                            r.overflow_ids.push(wid);
                        }
                        r.way_count += 1;
                        for nref in w.refs() {
                            r.way_refs_checked += 1;
                            if !node_ids_ref.get(nref) {
                                if nref < 0 {
                                    return Err(negative_ref_error('w', wid, 'n', nref));
                                }
                                r.missing_node_refs.push(nref);
                                if show_ids
                                    && should_store_missing_ref(r.missing_refs.len(), max_errors)
                                {
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
                r.missing_node_ref_occurrences = r.missing_node_refs.len() as u64;
                sort_dedup(&mut r.missing_node_refs);
                Ok(r)
            },
            |_seq, r| {
                super::insert_overflow_ids(&mut overflow, r.overflow_ids, host);
                result.way_count += r.way_count;
                way_refs_checked += r.way_refs_checked;
                missing_node_refs_occurrences += r.missing_node_ref_occurrences;
                for id in r.missing_node_refs {
                    missing_nodes_in_ways.set(id);
                }
                if show_ids {
                    append_capped_missing_refs(&mut missing_refs, r.missing_refs, max_errors);
                }
            },
            host,
        )?;
    }
    way_ids.merge(std::mem::replace(&mut overflow, IdSet::new()));
    host.invocation.marker("CHECKREFS_WAYS_END");
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; this sample is inert unless the invocation has a sink.
    crate::debug::emit_mallinfo2(&host.invocation, "checkrefs_after_ways");

    // ------------------------------------------------------------------
    // Phase 3 - relation scan (only when `check_relations`). Workers read
    // both `node_ids` and `way_ids`, write `relation_ids`, and collect
    // per-blob missing/deferred vecs.
    // ------------------------------------------------------------------
    let relation_blobs: u64 = if check_relations {
        relation_blobs_total
    } else {
        0
    };
    if check_relations {
        host.invocation.marker("CHECKREFS_RELATIONS_START");
        {
            let node_ids_ref = &node_ids;
            let way_ids_ref = &way_ids;
            let relation_ids_ref = &relation_ids;
            parallel_classify_phase(
                &shared_file,
                &rel_schedule,
                None,
                || (),
                |block, &mut ()| -> crate::Result<RelBlobResult> {
                    let mut r = RelBlobResult::default();
                    for el in block.elements_skip_metadata() {
                        if let Element::Relation(rel) = el {
                            let rid = rel.id();
                            if !relation_ids_ref.set_atomic_checked(rid) {
                                if rid < 0 {
                                    return Err(negative_id_error("relation", rid));
                                }
                                r.overflow_ids.push(rid);
                            }
                            r.relation_count += 1;
                            for mem in rel.members() {
                                match mem.id {
                                    MemberId::Node(id) => {
                                        r.rel_node_members_checked += 1;
                                        if !node_ids_ref.get(id) {
                                            if id < 0 {
                                                return Err(negative_ref_error('r', rid, 'n', id));
                                            }
                                            r.missing_node_members.push(id);
                                            if show_ids
                                                && should_store_missing_ref(
                                                    r.missing_refs.len(),
                                                    max_errors,
                                                )
                                            {
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
                                            if id < 0 {
                                                return Err(negative_ref_error('r', rid, 'w', id));
                                            }
                                            r.missing_way_refs.push(id);
                                            if show_ids
                                                && should_store_missing_ref(
                                                    r.missing_refs.len(),
                                                    max_errors,
                                                )
                                            {
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
                                        // Deferred without a lookup, so the
                                        // sign is checked here rather than in
                                        // a failed `get`.
                                        if id < 0 {
                                            return Err(negative_ref_error('r', rid, 'r', id));
                                        }
                                        r.rel_rel_members_deferred += 1;
                                        r.deferred_relation_refs.push(id);
                                        if show_ids {
                                            r.deferred_relation_ref_sources.push(rid);
                                        }
                                    }
                                    // A member type this check has no set to
                                    // resolve against (a newer producer's, or
                                    // a corrupt one). Skipping it would pass
                                    // a reference nobody checked.
                                    MemberId::Unknown(raw_type, id) => {
                                        return Err(unknown_member_error(rid, raw_type, id));
                                    }
                                }
                            }
                        }
                    }
                    r.missing_node_member_occurrences = r.missing_node_members.len() as u64;
                    r.missing_way_ref_occurrences = r.missing_way_refs.len() as u64;
                    sort_dedup(&mut r.missing_node_members);
                    sort_dedup(&mut r.missing_way_refs);
                    Ok(r)
                },
                |_seq, r| {
                    super::insert_overflow_ids(&mut overflow, r.overflow_ids, host);
                    result.relation_count += r.relation_count;
                    rel_node_members_checked += r.rel_node_members_checked;
                    rel_way_members_checked += r.rel_way_members_checked;
                    rel_rel_members_deferred += r.rel_rel_members_deferred;
                    missing_node_members_occurrences += r.missing_node_member_occurrences;
                    missing_way_refs_occurrences += r.missing_way_ref_occurrences;
                    for id in r.missing_node_members {
                        missing_nodes_in_relations.set(id);
                    }
                    for id in r.missing_way_refs {
                        missing_ways_in_relations.set(id);
                    }
                    deferred_relation_refs.extend(r.deferred_relation_refs);
                    if show_ids {
                        deferred_relation_ref_sources.extend(r.deferred_relation_ref_sources);
                        append_capped_missing_refs(&mut missing_refs, r.missing_refs, max_errors);
                    }
                },
                host,
            )?;
        }
        relation_ids.merge(std::mem::replace(&mut overflow, IdSet::new()));
        host.invocation.marker("CHECKREFS_RELATIONS_END");
        // Brokkr stores named counters generically and reports changes in sidecar
        // comparisons; this sample is inert unless the invocation has a sink.
        crate::debug::emit_mallinfo2(&host.invocation, "checkrefs_after_relations");
    }

    // Unique-missing counts are the sets' cardinalities. The timer keeps its
    // `missing_dedup` name from the sort-and-dedup it replaced.
    let t_dedup = Stopwatch::start();
    result.missing_node_refs = missing_nodes_in_ways.len();
    result.missing_way_refs = missing_ways_in_relations.len();
    result.missing_node_members = missing_nodes_in_relations.len();
    let missing_dedup_ms = t_dedup.elapsed_ms();

    // Resolve deferred relation refs against the complete relation_ids set.
    host.invocation.marker("CHECKREFS_DEFERRED_RESOLVE_START");
    let t_deferred = Stopwatch::start();
    if check_relations {
        let mut missing_relation_members = IdSet::new();
        let mut occurrences: u64 = 0;
        // `deferred_relation_ref_sources` is parallel to
        // `deferred_relation_refs` when `show_ids` is set, and empty otherwise.
        for (i, &id) in deferred_relation_refs.iter().enumerate() {
            if !relation_ids.get(id) {
                missing_relation_members.set(id);
                occurrences += 1;
                if show_ids && should_store_missing_ref(missing_refs.len(), max_errors) {
                    missing_refs.push(MissingRef {
                        missing_type: 'r',
                        missing_id: id,
                        referencing_type: 'r',
                        referencing_id: deferred_relation_ref_sources[i],
                    });
                }
            }
        }
        result.missing_relation_members = missing_relation_members.len();
        result.missing_relation_member_occurrences = occurrences;
    }
    let deferred_resolve_ms = t_deferred.elapsed_ms();
    host.invocation.marker("CHECKREFS_DEFERRED_RESOLVE_END");

    result.missing_refs = missing_refs;

    host.invocation.marker("CHECKREFS_SCAN_END");
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; this sample is inert unless the invocation has a sink.
    crate::debug::emit_mallinfo2(&host.invocation, "checkrefs_final");
    host.invocation
        .counter("checkrefs_node_count", result.node_count);
    host.invocation
        .counter("checkrefs_way_count", result.way_count);
    host.invocation
        .counter("checkrefs_relation_count", result.relation_count);
    host.invocation
        .counter("checkrefs_missing_node_refs", result.missing_node_refs);
    host.invocation
        .counter("checkrefs_missing_way_refs", result.missing_way_refs);
    host.invocation.counter(
        "checkrefs_missing_node_members",
        result.missing_node_members,
    );
    host.invocation.counter(
        "checkrefs_missing_relation_members",
        result.missing_relation_members,
    );

    // One-shot post-pass timers, sampled where each step ended.
    host.invocation
        .counter("checkrefs_missing_dedup_ms", missing_dedup_ms);
    host.invocation
        .counter("checkrefs_deferred_resolve_ms", deferred_resolve_ms);

    // Structural counters.
    host.invocation.counter("checkrefs_node_blobs", node_blobs);
    host.invocation.counter("checkrefs_way_blobs", way_blobs);
    host.invocation
        .counter("checkrefs_relation_blobs", relation_blobs);
    host.invocation
        .counter("checkrefs_way_refs_checked", way_refs_checked);
    host.invocation.counter(
        "checkrefs_rel_node_members_checked",
        rel_node_members_checked,
    );
    host.invocation
        .counter("checkrefs_rel_way_members_checked", rel_way_members_checked);
    host.invocation.counter(
        "checkrefs_rel_rel_members_deferred",
        rel_rel_members_deferred,
    );
    host.invocation.counter(
        "checkrefs_missing_node_refs_occurrences",
        missing_node_refs_occurrences,
    );
    host.invocation.counter(
        "checkrefs_missing_way_refs_occurrences",
        missing_way_refs_occurrences,
    );
    host.invocation.counter(
        "checkrefs_missing_node_members_occurrences",
        missing_node_members_occurrences,
    );

    Ok(result)
}
