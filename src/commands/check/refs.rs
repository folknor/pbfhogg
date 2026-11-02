//! Validate referential integrity in a PBF file. Equivalent to `osmium check-refs`.

use crate::counters::Stopwatch;

use crate::{ElementKind, MemberId};

use crate::error::Result;
use crate::idset::IdSet;
use crate::read::input::{InputFile, InputSource};
use crate::read::project_ids::{
    IdScratch, KindIds, RelationSpan, RelationSpans, WaySpan, WaySpans,
};
use crate::scan::classify::{
    KindSchedules, ScheduledBlob, build_classify_schedules_split, parallel_project_phase,
};
use crate::tunables::{NODE_ID_FLOOR, RELATION_ID_FLOOR, WAY_ID_FLOOR};

/// A single missing reference entry (populated under `show_ids`).
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
    /// Missing reference occurrences retained under
    /// [`CheckRefsOptions::show_ids`], at most [`ShowIds::max_errors`] unless
    /// it is zero. Empty when `show_ids` is `None`.
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
    /// Populated only under `show_ids`.
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
    /// Parallel to `deferred_relation_refs`, populated only under `show_ids`.
    deferred_relation_ref_sources: Vec<i64>,
    /// Populated only under `show_ids`.
    missing_refs: Vec<MissingRef>,
}

/// Options for [`check_refs`].
#[derive(Debug, Clone, Copy)]
pub struct CheckRefsOptions {
    /// Accepted and has no effect: the check reads only through the schedule
    /// walk and the parallel readers' unaligned blob preads.
    pub direct_io: bool,
    /// Also check relation member references.
    pub check_relations: bool,
    /// Retain each missing reference occurrence with the element referencing
    /// it in [`RefCheckResult::missing_refs`]. `None` retains none; the
    /// counts are the same either way.
    pub show_ids: Option<ShowIds>,
}

/// Settings of [`CheckRefsOptions::show_ids`].
#[derive(Debug, Clone, Copy)]
pub struct ShowIds {
    /// Maximum number of missing reference occurrences to retain. Zero means
    /// unlimited. This bounds the detail list, not the check's working
    /// memory: scanning and exact counting continue past it, and every
    /// deferred relation-to-relation reference keeps its source until the
    /// relation phase has judged it.
    pub max_errors: usize,
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
/// Reports unique missing IDs for nodes and ways (every node and way is in
/// its set before a reference to it is finally judged, whatever the file's
/// order). Reports unique missing relation IDs for relation-to-relation
/// members (deferred to a post-pass to handle forward references, then
/// deduplicated).
///
/// # Errors
///
/// `InvalidData` for a negative ID the check would compare and for a
/// relation member of an unknown type (see "Negative IDs" below), besides
/// the read and decode errors of any scan.
///
/// # Architecture
///
/// One header walk sorts the blobs by kind ([`build_classify_schedules_split`])
/// and each phase dispatches through [`parallel_project_phase`]: workers pread
/// and decompress each blob and validate it through a projection on the block
/// validator instead of building a block (`read::project_ids`: the node IDs,
/// each way's ID and `refs` position, each relation's ID and member-array
/// positions). The validator and kind-claim refusal are block construction's
/// own call, so a blob is refused exactly as building its block would refuse
/// it, with the same error. Only after the blob is accepted does the worker
/// replay what the projection recorded, in element-walk order, through the
/// same checks the element walk ran (shared-set insertion, the refusals
/// below, missing references), so a blob's first refusal is the one the walk
/// found first. The main thread merges per-blob results in completion order.
///
/// On an indexed file, phase 1 reads the node blobs and writes `node_ids` via
/// `IdSet::set_atomic`. Phase 2 reads the way blobs, checks their refs
/// against `node_ids` and, under `check_relations`, writes `way_ids`. Phase 3
/// (only under `check_relations`) reads the relation blobs, checks their
/// members against both and populates `relation_ids`. Forward
/// relation-relation references are deferred to a post-pass when
/// `relation_ids` is complete.
///
/// # Unindexed input
///
/// A blob without indexdata could hold any kind, and a way cannot be judged
/// before every node is in `node_ids`. Rather than read such a blob in every
/// phase, an unindexed file is read once by a combined phase that records
/// node IDs, ways and (under `check_relations`) relations in one validation
/// pass, and replays each accepted blob's nodes, then its ways, then its
/// relations, each way's and relation's ID inserted as it is judged. A
/// reference found in its set is final, since the sets only grow and only
/// accepted elements enter them. A miss is not, as the node blob holding the
/// target may not have been replayed yet (or holds an ID above the
/// pre-allocation, which joins the set only after the phase), so a blob whose
/// way judgement found any missing reference has that judgement dropped
/// whole and is read again by phase 2, after `node_ids` is complete; one
/// whose relation judgement missed a node or way member likewise by phase 3.
/// The refusals do not depend on order, so they stand wherever they are met.
/// A sorted file re-reads only the blobs whose references raced the blobs
/// still being replayed and the blobs with references that are missing in
/// fact; no blob is read more than once per phase, so no order costs more
/// than reading every blob in every phase would.
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
/// them. And [`ShowIds::max_errors`] caps retained display details only when
/// non-zero: zero keeps one detail per missing occurrence.
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
/// # What a phase reads
///
/// Node IDs; each way's ID and node refs; each relation's ID and member IDs
/// and types. Never coordinates, tags, metadata or roles, which validation
/// still checks (a check that passed a corrupt file would vouch for it). An
/// earlier refusal of a selective parser here rested on a Europe hotpath
/// that timed only the offset tables; the element walk it left out was most
/// of the workers' CPU after decompression (`reference/performance.md`,
/// "check --refs and getid read through projections").
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
/// `check --index` is the verifier of every claim. An unindexed blob claims
/// nothing, and the combined phase takes each element by its own kind.
#[hotpath::measure]
pub fn check_refs<'a>(
    input: impl Into<InputSource<'a>>,
    opts: &CheckRefsOptions,
    host: &crate::host::Host,
) -> Result<super::CheckOutcome<RefCheckResult>> {
    let host = &host.enter()?;
    let input = input.into();
    host.invocation
        .decided(|| super::CheckOutcome::stop_on_contradiction(check_refs_inner(input, opts, host)))
}

/// What judging a blob's ways and relations needs besides the sets.
#[derive(Clone, Copy)]
struct Judge {
    check_relations: bool,
    show_ids: Option<ShowIds>,
}

impl Judge {
    /// Whether a missing reference found with `retained` details already
    /// held is retained as one more.
    fn keeps_detail(self, retained: usize) -> bool {
        self.show_ids
            .is_some_and(|show_ids| should_store_missing_ref(retained, show_ids.max_errors))
    }
}

/// The ID sets every judgement reads, and the phases write.
struct Sets {
    nodes: IdSet,
    ways: IdSet,
    relations: IdSet,
}

/// Insert one accepted blob's node IDs into `node_ids`, returning how many
/// there were and those above the pre-allocation, for the serial merge.
fn replay_nodes(scratch: &IdScratch, bytes: &[u8], node_ids: &IdSet) -> Result<(u64, Vec<i64>)> {
    let mut count: u64 = 0;
    let mut overflow = Vec::new();
    scratch.each(bytes, |_, id| {
        if !node_ids.set_atomic_checked(id) {
            if id < 0 {
                return Err(negative_id_error("node", id));
            }
            overflow.push(id);
        }
        count += 1;
        Ok(())
    })?;
    Ok((count, overflow))
}

/// Judge one accepted blob's ways, in element-walk order: each way's ID into
/// `way_ids` (under `check_relations`), then its refs against `node_ids`.
fn judge_ways(ways: &[WaySpan], bytes: &[u8], sets: &Sets, judge: Judge) -> Result<WayBlobResult> {
    let mut r = WayBlobResult::default();
    for way in ways {
        let wid = way.id;
        // Way IDs are tracked only for the relation phase; without it no
        // comparison reads them, so a negative one is refused only here.
        if judge.check_relations && !sets.ways.set_atomic_checked(wid) {
            if wid < 0 {
                return Err(negative_id_error("way", wid));
            }
            r.overflow_ids.push(wid);
        }
        r.way_count += 1;
        for nref in way.refs(bytes)? {
            r.way_refs_checked += 1;
            if !sets.nodes.get(nref) {
                if nref < 0 {
                    return Err(negative_ref_error('w', wid, 'n', nref));
                }
                r.missing_node_refs.push(nref);
                if judge.keeps_detail(r.missing_refs.len()) {
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
    r.missing_node_ref_occurrences = r.missing_node_refs.len() as u64;
    sort_dedup(&mut r.missing_node_refs);
    Ok(r)
}

/// Judge one accepted blob's relations, in element-walk order: each
/// relation's ID into `relation_ids`, then its node and way members against
/// their sets, its relation members deferred.
fn judge_relations(
    relations: &[RelationSpan],
    bytes: &[u8],
    sets: &Sets,
    judge: Judge,
) -> Result<RelBlobResult> {
    let mut r = RelBlobResult::default();
    for rel in relations {
        let rid = rel.id;
        if !sets.relations.set_atomic_checked(rid) {
            if rid < 0 {
                return Err(negative_id_error("relation", rid));
            }
            r.overflow_ids.push(rid);
        }
        r.relation_count += 1;
        for member in rel.members(bytes)? {
            match member {
                MemberId::Node(id) => {
                    r.rel_node_members_checked += 1;
                    if !sets.nodes.get(id) {
                        if id < 0 {
                            return Err(negative_ref_error('r', rid, 'n', id));
                        }
                        r.missing_node_members.push(id);
                        if judge.keeps_detail(r.missing_refs.len()) {
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
                    if !sets.ways.get(id) {
                        if id < 0 {
                            return Err(negative_ref_error('r', rid, 'w', id));
                        }
                        r.missing_way_refs.push(id);
                        if judge.keeps_detail(r.missing_refs.len()) {
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
                    // Deferred without a lookup, so the sign is checked
                    // here rather than in a failed `get`.
                    if id < 0 {
                        return Err(negative_ref_error('r', rid, 'r', id));
                    }
                    r.rel_rel_members_deferred += 1;
                    r.deferred_relation_refs.push(id);
                    // Every deferred reference keeps its source, uncapped:
                    // which of them turn out missing is known only after
                    // the relation phase.
                    if judge.show_ids.is_some() {
                        r.deferred_relation_ref_sources.push(rid);
                    }
                }
                // A member type this check has no set to resolve against (a
                // newer producer's, or a corrupt one). Skipping it would pass
                // a reference nobody checked.
                MemberId::Unknown(raw_type, id) => {
                    return Err(unknown_member_error(rid, raw_type, id));
                }
            }
        }
    }
    r.missing_node_member_occurrences = r.missing_node_members.len() as u64;
    r.missing_way_ref_occurrences = r.missing_way_refs.len() as u64;
    sort_dedup(&mut r.missing_node_members);
    sort_dedup(&mut r.missing_way_refs);
    Ok(r)
}

/// Everything the serial merges accumulate, and the side sets holding each
/// kind's IDs above its pre-allocation until its phase ends.
#[derive(Default)]
struct Tally {
    node_count: u64,
    way_count: u64,
    relation_count: u64,
    // One set per reported category - `total_missing` sums the categories,
    // so a node missing from a way and from a relation counts in both. The
    // merge inserts each blob's distinct missing IDs, so memory follows the
    // missing ID range rather than the reference count: a ways-only file,
    // where every ref is missing, used to retain 8 bytes per reference.
    missing_nodes_in_ways: IdSet,
    missing_ways_in_relations: IdSet,
    missing_nodes_in_relations: IdSet,
    missing_node_refs_occurrences: u64,
    missing_way_refs_occurrences: u64,
    missing_node_members_occurrences: u64,
    deferred_relation_refs: Vec<i64>,
    deferred_relation_ref_sources: Vec<i64>,
    missing_refs: Vec<MissingRef>,
    way_refs_checked: u64,
    rel_node_members_checked: u64,
    rel_way_members_checked: u64,
    rel_rel_members_deferred: u64,
    node_overflow: IdSet,
    way_overflow: IdSet,
    relation_overflow: IdSet,
}

impl Tally {
    // Each phase inserts IDs above its pre-allocation into a side set from
    // its serial merge and moves it into the kind's own set after the join
    // (see `check::insert_overflow_ids`).
    fn merge_nodes(&mut self, count: u64, overflow_ids: Vec<i64>, host: &crate::host::Entered<'_>) {
        super::insert_overflow_ids(&mut self.node_overflow, overflow_ids, host);
        self.node_count += count;
    }

    fn merge_ways(&mut self, r: WayBlobResult, judge: Judge, host: &crate::host::Entered<'_>) {
        super::insert_overflow_ids(&mut self.way_overflow, r.overflow_ids, host);
        self.way_count += r.way_count;
        self.way_refs_checked += r.way_refs_checked;
        self.missing_node_refs_occurrences += r.missing_node_ref_occurrences;
        for id in r.missing_node_refs {
            self.missing_nodes_in_ways.set(id);
        }
        if let Some(show_ids) = judge.show_ids {
            append_capped_missing_refs(&mut self.missing_refs, r.missing_refs, show_ids.max_errors);
        }
    }

    fn merge_relations(&mut self, r: RelBlobResult, judge: Judge, host: &crate::host::Entered<'_>) {
        super::insert_overflow_ids(&mut self.relation_overflow, r.overflow_ids, host);
        self.relation_count += r.relation_count;
        self.rel_node_members_checked += r.rel_node_members_checked;
        self.rel_way_members_checked += r.rel_way_members_checked;
        self.rel_rel_members_deferred += r.rel_rel_members_deferred;
        self.missing_node_members_occurrences += r.missing_node_member_occurrences;
        self.missing_way_refs_occurrences += r.missing_way_ref_occurrences;
        for id in r.missing_node_members {
            self.missing_nodes_in_relations.set(id);
        }
        for id in r.missing_way_refs {
            self.missing_ways_in_relations.set(id);
        }
        self.deferred_relation_refs.extend(r.deferred_relation_refs);
        if let Some(show_ids) = judge.show_ids {
            self.deferred_relation_ref_sources
                .extend(r.deferred_relation_ref_sources);
            append_capped_missing_refs(&mut self.missing_refs, r.missing_refs, show_ids.max_errors);
        }
    }
}

/// Phase 1 of an indexed file: the node blobs into `node_ids`.
fn node_phase(
    file: &InputFile,
    schedule: &[ScheduledBlob],
    sets: &Sets,
    tally: &mut Tally,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    parallel_project_phase(
        file,
        schedule,
        None,
        IdScratch::default,
        |_, buffer, scratch| -> Result<(u64, Vec<i64>)> {
            let (KindIds { .. }, accepted) =
                buffer.project(|_, _| KindIds::of(ElementKind::Node, &mut *scratch))?;
            replay_nodes(scratch, accepted.bytes(), &sets.nodes)
        },
        |_seq, (count, overflow_ids)| tally.merge_nodes(count, overflow_ids, host),
        host,
    )
}

/// Phase 2: the way blobs (an indexed file's, or the unindexed blobs the
/// combined phase could not judge), after `node_ids` is complete.
fn way_phase(
    file: &InputFile,
    schedule: &[ScheduledBlob],
    sets: &Sets,
    judge: Judge,
    tally: &mut Tally,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    parallel_project_phase(
        file,
        schedule,
        None,
        Vec::new,
        |_, buffer, ways| -> Result<WayBlobResult> {
            let (WaySpans { .. }, accepted) =
                buffer.project(|_, _| WaySpans::new(None, &mut *ways))?;
            judge_ways(ways, accepted.bytes(), sets, judge)
        },
        |_seq, r| tally.merge_ways(r, judge, host),
        host,
    )
}

/// Phase 3: the relation blobs (an indexed file's, or the unindexed blobs
/// the combined phase could not judge), after `node_ids` and `way_ids` are
/// complete.
fn relation_phase(
    file: &InputFile,
    schedule: &[ScheduledBlob],
    sets: &Sets,
    judge: Judge,
    tally: &mut Tally,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    parallel_project_phase(
        file,
        schedule,
        None,
        Vec::new,
        |_, buffer, relations| -> Result<RelBlobResult> {
            let (RelationSpans { .. }, accepted) =
                buffer.project(|_, _| RelationSpans::new(&mut *relations))?;
            judge_relations(relations, accepted.bytes(), sets, judge)
        },
        |_seq, r| tally.merge_relations(r, judge, host),
        host,
    )
}

/// A worker's scratch for the combined phase: one projection's worth each.
#[derive(Default)]
struct UnindexedScratch {
    ids: IdScratch,
    ways: Vec<WaySpan>,
    relations: Vec<RelationSpan>,
}

/// One kind's judgement of an unindexed blob in the combined phase.
enum Judged<R> {
    /// The blob holds none of the kind (or the run does not judge it).
    Absent,
    /// Every reference resolved, so the judgement is final.
    Final(R),
    /// A reference missed while its target may not yet have been in its
    /// set: the judgement is dropped and the blob read again by the kind's
    /// phase.
    Again,
}

/// What the combined phase found in one unindexed blob.
struct UnindexedBlobResult {
    node_count: u64,
    node_overflow: Vec<i64>,
    ways: Judged<WayBlobResult>,
    relations: Judged<RelBlobResult>,
}

/// The combined phase of an unindexed file (see `check_refs`, "Unindexed
/// input"): every blob read once, its nodes inserted and its ways and
/// relations judged against the sets as they stand. Returns the blobs whose
/// way and whose relation judgements have to be made again, in file order.
fn unindexed_phase(
    file: &InputFile,
    blobs: &[ScheduledBlob],
    sets: &Sets,
    judge: Judge,
    tally: &mut Tally,
    host: &crate::host::Entered<'_>,
) -> Result<(Vec<ScheduledBlob>, Vec<ScheduledBlob>)> {
    let mut ways_again: Vec<usize> = Vec::new();
    let mut relations_again: Vec<usize> = Vec::new();
    parallel_project_phase(
        file,
        blobs,
        None,
        UnindexedScratch::default,
        |_, buffer, scratch| -> Result<UnindexedBlobResult> {
            let UnindexedScratch {
                ids,
                ways,
                relations,
            } = scratch;
            relations.clear();
            let (_, accepted) = buffer.project(|_, _| {
                (
                    (
                        KindIds::of(ElementKind::Node, &mut *ids),
                        WaySpans::new(None, &mut *ways),
                    ),
                    judge
                        .check_relations
                        .then(|| RelationSpans::new(&mut *relations)),
                )
            })?;
            let bytes = accepted.bytes();
            let (node_count, node_overflow) = replay_nodes(ids, bytes, &sets.nodes)?;
            let ways = if ways.is_empty() {
                Judged::Absent
            } else {
                let r = judge_ways(ways, bytes, sets, judge)?;
                if r.missing_node_ref_occurrences > 0 {
                    Judged::Again
                } else {
                    Judged::Final(r)
                }
            };
            let relations = if relations.is_empty() {
                Judged::Absent
            } else {
                let r = judge_relations(relations, bytes, sets, judge)?;
                if r.missing_node_member_occurrences + r.missing_way_ref_occurrences > 0 {
                    Judged::Again
                } else {
                    Judged::Final(r)
                }
            };
            Ok(UnindexedBlobResult {
                node_count,
                node_overflow,
                ways,
                relations,
            })
        },
        |seq, r| {
            tally.merge_nodes(r.node_count, r.node_overflow, host);
            match r.ways {
                Judged::Absent => {}
                Judged::Final(w) => tally.merge_ways(w, judge, host),
                Judged::Again => ways_again.push(seq),
            }
            match r.relations {
                Judged::Absent => {}
                Judged::Final(rel) => tally.merge_relations(rel, judge, host),
                Judged::Again => relations_again.push(seq),
            }
        },
        host,
    )?;
    // Completion order back to file order, which the next phases' read-ahead
    // advice coalesces.
    let again = |mut positions: Vec<usize>| -> Vec<ScheduledBlob> {
        positions.sort_unstable();
        positions.into_iter().map(|p| blobs[p]).collect()
    };
    Ok((again(ways_again), again(relations_again)))
}

#[allow(clippy::too_many_lines)]
fn check_refs_inner(
    input: InputSource<'_>,
    opts: &CheckRefsOptions,
    host: &crate::host::Entered<'_>,
) -> Result<RefCheckResult> {
    let judge = Judge {
        check_relations: opts.check_relations,
        show_ids: opts.show_ids,
    };
    // `opts.direct_io` is accepted and has no effect: every read here
    // is the schedule walk or a `parallel_project_phase` pread of an
    // unaligned blob range, neither of which takes O_DIRECT. The CLI says so
    // when the flag is given.

    host.invocation.marker("CHECKREFS_SCAN_START");

    // Single header-chain walk sorting every blob by kind at once. At
    // Europe scale the header walk is ~15 s; one walk per kind would triple
    // that cost.
    let (schedules, shared_file, max_ids) =
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
    let mut sets = Sets {
        nodes: IdSet::new(),
        ways: IdSet::new(),
        relations: IdSet::new(),
    };
    sets.nodes
        .pre_allocate(super::id_alloc_bound(max_ids[0], NODE_ID_FLOOR, file_len));
    if judge.check_relations {
        sets.ways
            .pre_allocate(super::id_alloc_bound(max_ids[1], WAY_ID_FLOOR, file_len));
        sets.relations.pre_allocate(super::id_alloc_bound(
            max_ids[2],
            RELATION_ID_FLOOR,
            file_len,
        ));
    }

    let mut tally = Tally::default();

    // ------------------------------------------------------------------
    // Phase 1 - the node blobs of an indexed file, or every blob of an
    // unindexed one, judging what it can. Afterwards `node_ids` is complete.
    // ------------------------------------------------------------------
    let mut unindexed_blobs: u64 = 0;
    let (node_blobs, way_schedule, rel_schedule) = match schedules {
        KindSchedules::Indexed {
            nodes,
            ways,
            relations,
        } => {
            host.invocation.marker("CHECKREFS_NODES_START");
            node_phase(&shared_file, &nodes, &sets, &mut tally, host)?;
            sets.nodes.merge(std::mem::take(&mut tally.node_overflow));
            host.invocation.marker("CHECKREFS_NODES_END");
            (nodes.len() as u64, ways, relations)
        }
        KindSchedules::Unindexed(blobs) => {
            unindexed_blobs = blobs.len() as u64;
            host.invocation.marker("CHECKREFS_UNINDEXED_START");
            let (ways, relations) =
                unindexed_phase(&shared_file, &blobs, &sets, judge, &mut tally, host)?;
            sets.nodes.merge(std::mem::take(&mut tally.node_overflow));
            host.invocation.marker("CHECKREFS_UNINDEXED_END");
            (0, ways, relations)
        }
    };
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; this sample is inert unless the invocation has a sink.
    crate::debug::emit_mallinfo2(&host.invocation, "checkrefs_after_nodes");

    // ------------------------------------------------------------------
    // Phase 2 - way scan. Workers read the complete `node_ids`, optionally
    // write `way_ids`, and collect per-blob missing-ref vecs.
    // ------------------------------------------------------------------
    host.invocation.marker("CHECKREFS_WAYS_START");
    way_phase(&shared_file, &way_schedule, &sets, judge, &mut tally, host)?;
    sets.ways.merge(std::mem::take(&mut tally.way_overflow));
    host.invocation.marker("CHECKREFS_WAYS_END");
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; this sample is inert unless the invocation has a sink.
    crate::debug::emit_mallinfo2(&host.invocation, "checkrefs_after_ways");

    // ------------------------------------------------------------------
    // Phase 3 - relation scan (only when `check_relations`). Workers read
    // both complete sets, write `relation_ids`, and collect per-blob
    // missing/deferred vecs.
    // ------------------------------------------------------------------
    let relation_blobs: u64 = if judge.check_relations {
        rel_schedule.len() as u64
    } else {
        0
    };
    if judge.check_relations {
        host.invocation.marker("CHECKREFS_RELATIONS_START");
        relation_phase(&shared_file, &rel_schedule, &sets, judge, &mut tally, host)?;
        sets.relations
            .merge(std::mem::take(&mut tally.relation_overflow));
        host.invocation.marker("CHECKREFS_RELATIONS_END");
        // Brokkr stores named counters generically and reports changes in sidecar
        // comparisons; this sample is inert unless the invocation has a sink.
        crate::debug::emit_mallinfo2(&host.invocation, "checkrefs_after_relations");
    }

    let mut result = RefCheckResult {
        node_count: tally.node_count,
        way_count: tally.way_count,
        relation_count: tally.relation_count,
        missing_node_refs: 0,
        missing_way_refs: 0,
        missing_node_members: 0,
        missing_relation_members: 0,
        missing_relation_member_occurrences: 0,
        missing_refs: Vec::new(),
    };

    // Unique-missing counts are the sets' cardinalities. The timer keeps its
    // `missing_dedup` name from the sort-and-dedup it replaced.
    let t_dedup = Stopwatch::start();
    result.missing_node_refs = tally.missing_nodes_in_ways.len();
    result.missing_way_refs = tally.missing_ways_in_relations.len();
    result.missing_node_members = tally.missing_nodes_in_relations.len();
    let missing_dedup_ms = t_dedup.elapsed_ms();

    // Resolve deferred relation refs against the complete relation_ids set.
    host.invocation.marker("CHECKREFS_DEFERRED_RESOLVE_START");
    let t_deferred = Stopwatch::start();
    let mut missing_refs = std::mem::take(&mut tally.missing_refs);
    if judge.check_relations {
        let mut missing_relation_members = IdSet::new();
        let mut occurrences: u64 = 0;
        // `deferred_relation_ref_sources` is parallel to
        // `deferred_relation_refs` under `show_ids`, and empty otherwise.
        for (i, &id) in tally.deferred_relation_refs.iter().enumerate() {
            if !sets.relations.get(id) {
                missing_relation_members.set(id);
                occurrences += 1;
                if judge.keeps_detail(missing_refs.len()) {
                    missing_refs.push(MissingRef {
                        missing_type: 'r',
                        missing_id: id,
                        referencing_type: 'r',
                        referencing_id: tally.deferred_relation_ref_sources[i],
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

    // Structural counters: the blobs each phase read. On an unindexed file
    // the combined phase read every blob once and the way and relation
    // phases only the blobs it could not judge.
    host.invocation.counter("checkrefs_node_blobs", node_blobs);
    host.invocation
        .counter("checkrefs_unindexed_blobs", unindexed_blobs);
    host.invocation
        .counter("checkrefs_way_blobs", way_schedule.len() as u64);
    host.invocation
        .counter("checkrefs_relation_blobs", relation_blobs);
    host.invocation
        .counter("checkrefs_way_refs_checked", tally.way_refs_checked);
    host.invocation.counter(
        "checkrefs_rel_node_members_checked",
        tally.rel_node_members_checked,
    );
    host.invocation.counter(
        "checkrefs_rel_way_members_checked",
        tally.rel_way_members_checked,
    );
    host.invocation.counter(
        "checkrefs_rel_rel_members_deferred",
        tally.rel_rel_members_deferred,
    );
    host.invocation.counter(
        "checkrefs_missing_node_refs_occurrences",
        tally.missing_node_refs_occurrences,
    );
    host.invocation.counter(
        "checkrefs_missing_way_refs_occurrences",
        tally.missing_way_refs_occurrences,
    );
    host.invocation.counter(
        "checkrefs_missing_node_members_occurrences",
        tally.missing_node_members_occurrences,
    );

    Ok(result)
}
