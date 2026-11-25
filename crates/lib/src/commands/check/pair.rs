//! The default pair of `check`: the ID check and the reference check over
//! one read of the file.
//!
//! Run as two calls ([`verify_ids`](super::verify_ids::verify_ids) then
//! [`check_refs`](super::refs::check_refs)), the pair walks the header chain
//! twice and preads, decompresses and validates every node and way blob
//! twice. Here one walk schedules the blobs and each kind's phase reads its
//! blobs once, for every check that reads that kind: the node phase for both
//! (the ID check only where its type filter selects nodes), the way phase
//! likewise, the relation phase for the ID check where it selects relations
//! and for the reference check under `check_relations`. Validation is block
//! construction's own call whichever projection rides along
//! (`read::project_ids`), so a blob is refused or contradicts its claim
//! exactly as it would in either call alone.
//!
//! # Each check decides alone
//!
//! The scan is shared; the verdicts are not. A failure of the read itself
//! (pread, decompression, validation, a contradicted kind claim) belongs to
//! every check still consuming the phase it ended, and ends that phase. A
//! failure of one check's judgement of an accepted blob - the reference
//! check's refusal of a member type no set can resolve, the one such
//! judgement an accepted blob can fail - belongs to that check alone: the
//! phase goes on for the other, and the failed check consumes nothing more.
//! Each check keeps the first terminal event it receives. A check not
//! reading a phase is untouched by it and runs on into the later phases, so
//! `-t r` with a lie in a node blob stops the reference check while the ID
//! check completes, as the two calls would. A read failure that is not a
//! finding (`CheckOutcome::stop_on_finding`) ends the whole call when the ID
//! check is still consuming the phase, since the ID check runs first and
//! its error is what the two calls would have reported; otherwise it is the
//! reference check's error, returned beside a complete ID report.
//!
//! Internal invariant failures (a span read against the wrong blob, an
//! unfilled result slot) end the call whichever check met them.
//!
//! # Memory
//!
//! The reference check's sets live from its first phase to its last (the
//! way phase unless `check_relations`), and are dropped before any later
//! phase and before `--full`'s duplicate pass allocates its own sets, so
//! those never coexist. What does coexist with the reference sets is the ID
//! check's per-blob results, one slot per scheduled blob of the phase; on a
//! file with many order violations their retained vectors grow with the
//! violation count, as they do in the ID check alone.
//!
//! # Unindexed input
//!
//! An unindexed file is read once for both checks: every blob validated
//! once through one projection carrying the node IDs (and, without
//! `check_relations`, the relation IDs the ID check selects), the way spans,
//! the relation spans under `check_relations` and the group kinds the ID
//! check's type order is judged from. Each accepted blob is judged by the ID
//! check as its own unindexed pass judges it and by the reference check
//! through the rule its combined phase applies (`refs::judge_unindexed`);
//! the reference check then reads again only the blobs whose judgement it
//! dropped, and finishes, before `--full`'s duplicate pass. Failures are
//! attributed as in the indexed phases. What the two checks see of
//! completion order (which failure a check meets first, which capped
//! `--show-ids` details it retains) is a draw some run of the two calls
//! could also make.
//!
//! # Why `--index` is not in the pair
//!
//! The index check reads through another engine on purpose: front to back
//! through one open (so alone it reads a FIFO), accepting a partially
//! indexed file and reporting its coverage instead of stopping, never
//! routing a blob by its claim, and decoding with the raw scanners rather
//! than the block validator. Sharing this scan would make it inherit the
//! claim routing and the partial-index stop it exists to judge, so it keeps
//! its own read.

use std::sync::atomic::{AtomicBool, Ordering};

use super::refs::{
    self, CheckRefsOptions, Judge, PhaseBlobs, RefCheckResult, RelBlobResult, Sets, Tally,
    WayBlobResult,
};
use super::verify_ids::{
    self, BlobVerifyResult, GroupKinds, KINDS, KindJudge, KindSpans, Scanned, VerifyIdsOptions,
    VerifyIdsReport,
};
use super::{CheckOutcome, StoppedCheck};
use crate::ElementKind;
use crate::error::Result;
use crate::owned::TypeFilter;
use crate::read::input::{InputSource, PinnedInput};
use crate::read::project_ids::{
    IdScratch, KindIds, RelationSpan, RelationSpans, WaySpan, WaySpans,
};
use crate::scan::classify::{KindSchedules, ScheduledBlob, parallel_project_phase};

/// The outcomes of [`check_ids_and_refs`]: each check's own, as the two
/// separate calls would have returned them. The ID check's error ends the
/// call, as it ends a run of the two calls before the reference check
/// starts; the reference check's error is carried here.
pub struct PairOutcome {
    /// The ID check's outcome.
    pub ids: CheckOutcome<VerifyIdsReport>,
    /// The reference check's outcome, or its error.
    pub refs: Result<CheckOutcome<RefCheckResult>>,
}

/// Run the ID check and the reference check over one read of `input`.
///
/// Reports as [`verify_ids`](super::verify_ids::verify_ids) and
/// [`check_refs`](super::refs::check_refs) report, with the same options;
/// see the module documentation for how failures are attributed.
///
/// # Errors
///
/// What [`verify_ids`](super::verify_ids::verify_ids) returns as an error;
/// the reference check's errors are in [`PairOutcome::refs`].
#[hotpath::measure]
pub fn check_ids_and_refs<'a>(
    input: impl Into<InputSource<'a>>,
    ids: &VerifyIdsOptions,
    refs: &CheckRefsOptions,
    host: &crate::host::Host,
) -> Result<PairOutcome> {
    let host = &host.enter()?;
    let input = input.into();
    host.invocation
        .decided(|| pair_inner(input, ids, refs, host))
}

/// The reference check's state through the pair: still consuming phases
/// (its sets and tally), or terminal.
#[expect(
    clippy::large_enum_variant,
    reason = "one value per call, never moved in a loop or stored in bulk"
)]
enum RefsState {
    Live { sets: Sets, tally: Tally },
    Done(Result<CheckOutcome<RefCheckResult>>),
}

impl RefsState {
    fn is_live(&self) -> bool {
        matches!(self, Self::Live { .. })
    }
}

/// One blob's contribution to the reference check.
enum RefsBlob {
    Nodes(u64, Vec<i64>),
    Ways(WayBlobResult),
    Relations(RelBlobResult),
}

/// What one blob of a shared phase yields: the ID check's judgement where
/// it reads the phase, the reference check's (or its judgement failure)
/// where it reads it and has not failed.
struct PairBlob {
    ids: Option<BlobVerifyResult>,
    refs: Option<Result<RefsBlob>>,
}

/// A worker's scratch: one projection's worth of each.
#[derive(Default)]
struct PairScratch {
    ids: IdScratch,
    ways: Vec<WaySpan>,
    relations: Vec<RelationSpan>,
}

#[allow(clippy::too_many_lines)]
fn pair_inner(
    input: InputSource<'_>,
    ids_opts: &VerifyIdsOptions,
    refs_opts: &CheckRefsOptions,
    host: &crate::host::Entered<'_>,
) -> Result<PairOutcome> {
    let judge = Judge {
        check_relations: refs_opts.check_relations,
        show_ids: refs_opts.show_ids,
    };
    // The ID check opens its input twice (front probe, schedule walk), so
    // it pins; the reference check reads through the same walk.
    let pin = input.pin(host)?;
    host.invocation.marker("CHECKPAIR_SCAN_START");

    // The front probe is the ID check's alone: its finding stops only that
    // check, its error ends the call as it ends the ID check's run.
    let mut ids_stopped: Option<StoppedCheck> = None;
    let mut front = None;
    match CheckOutcome::stop_on_finding(crate::commands::input_front::sorted_and_indexed(
        &pin,
        ids_opts.direct_io,
        host,
    ))? {
        CheckOutcome::Complete(probe) => front = Some(probe),
        CheckOutcome::Stopped(stopped) => ids_stopped = Some(stopped),
    }

    // One walk, which both checks would have made identically: a finding
    // stops both, an error is the ID check's where it is live.
    let walked =
        CheckOutcome::stop_on_finding(crate::scan::classify::build_classify_schedules_split(
            &pin,
            crate::scan::classify::KindClaims::Refuse,
            host,
        ));
    let (schedules, file, max_ids) = match walked {
        Ok(CheckOutcome::Complete(walk)) => walk,
        Ok(CheckOutcome::Stopped(stopped)) => {
            host.invocation.marker("CHECKPAIR_SCAN_END");
            return Ok(PairOutcome {
                ids: CheckOutcome::Stopped(ids_stopped.unwrap_or_else(|| stopped.clone())),
                refs: Ok(CheckOutcome::Stopped(stopped)),
            });
        }
        Err(error) => {
            if let Some(stopped) = ids_stopped {
                host.invocation.marker("CHECKPAIR_SCAN_END");
                return Ok(PairOutcome {
                    ids: CheckOutcome::Stopped(stopped),
                    refs: Err(error),
                });
            }
            return Err(error);
        }
    };

    if let KindSchedules::Unindexed(blobs) = &schedules {
        let (scanned, refs) = unindexed_pass(
            &file,
            blobs,
            max_ids,
            &mut ids_stopped,
            ids_opts,
            judge,
            host,
        )?;
        let ids = ids_outcome(
            ids_stopped,
            front,
            scanned,
            &file,
            &schedules,
            max_ids,
            &pin,
            ids_opts,
            host,
        )?;
        host.invocation.marker("CHECKPAIR_SCAN_END");
        return Ok(PairOutcome { ids, refs });
    }
    let KindSchedules::Indexed {
        nodes,
        ways,
        relations,
    } = &schedules
    else {
        return Err(crate::Error::internal("check pair: schedules changed kind"));
    };

    let type_filter = TypeFilter::or_all(ids_opts.type_filter);
    let limit = verify_ids::effective_error_limit(ids_opts.max_errors);
    let mut scanned = Scanned {
        type_order: Vec::new(),
        type_order_total: 0,
        kinds: [None, None, None],
    };
    verify_ids::check_type_order(
        &[
            verify_ids::schedule_span(nodes),
            verify_ids::schedule_span(ways),
            verify_ids::schedule_span(relations),
        ],
        &mut scanned.type_order,
        &mut scanned.type_order_total,
        limit,
    );

    let mut refs = match Sets::allocate(&file, max_ids, judge) {
        Ok(sets) => RefsState::Live {
            sets,
            tally: Tally::default(),
        },
        Err(error) => RefsState::Done(Err(error)),
    };
    let refs_last = if judge.check_relations {
        ElementKind::Relation
    } else {
        ElementKind::Way
    };
    let mut physical_blobs: u64 = 0;
    let refs_blobs = PhaseBlobs {
        node: nodes.len() as u64,
        unindexed: 0,
        way: ways.len() as u64,
        relation: if judge.check_relations {
            relations.len() as u64
        } else {
            0
        },
    };

    for (kind, schedule) in KINDS.into_iter().zip([nodes, ways, relations]) {
        let ids_reads = ids_stopped.is_none() && verify_ids::selects(&type_filter, kind);
        let refs_reads = refs.is_live() && (kind != ElementKind::Relation || judge.check_relations);
        if !ids_reads && !refs_reads {
            continue;
        }
        let (start, end) = PHASE_MARKERS[usize::from(kind.ordinal())];
        host.invocation.marker(start);
        physical_blobs += schedule.len() as u64;
        let mut per_blob: Vec<Option<BlobVerifyResult>> = if ids_reads {
            (0..schedule.len()).map(|_| None).collect()
        } else {
            Vec::new()
        };
        let mut refs_failed: Option<crate::Error> = None;
        let refs_dead = AtomicBool::new(false);
        let phase = {
            let (sets, mut tally) = match &mut refs {
                RefsState::Live { sets, tally } if refs_reads => (Some(&*sets), Some(tally)),
                _ => (None, None),
            };
            let refs_dead = &refs_dead;
            parallel_project_phase(
                &file,
                schedule,
                None,
                PairScratch::default,
                |_, buffer, scratch| {
                    let sets = sets.filter(|_| !refs_dead.load(Ordering::Relaxed));
                    judge_blob(kind, buffer, scratch, ids_reads, sets, judge)
                },
                |seq, blob| {
                    if let Some(r) = blob.ids {
                        per_blob[seq] = Some(r);
                    }
                    let (Some(tally), Some(result)) = (tally.as_deref_mut(), blob.refs) else {
                        return;
                    };
                    if refs_failed.is_some() {
                        return;
                    }
                    match result {
                        Ok(RefsBlob::Nodes(count, overflow)) => {
                            tally.merge_nodes(count, overflow, host);
                        }
                        Ok(RefsBlob::Ways(r)) => tally.merge_ways(r, judge, host),
                        Ok(RefsBlob::Relations(r)) => tally.merge_relations(r, judge, host),
                        Err(error) => {
                            refs_dead.store(true, Ordering::Relaxed);
                            refs_failed = Some(file.locate_blob(&schedule[seq], error));
                        }
                    }
                },
                host,
            )
        };
        host.invocation.marker(end);

        // The reference check's own failure came first, whatever ended
        // the phase after it.
        if let Some(error) = refs_failed {
            refs = RefsState::Done(Err(error));
        }
        if let Err(error) = phase {
            match StoppedCheck::from_error(&error) {
                Some(stopped) => {
                    if refs_reads && refs.is_live() {
                        refs = RefsState::Done(Ok(CheckOutcome::Stopped(stopped.clone())));
                    }
                    if ids_reads {
                        ids_stopped = Some(stopped);
                    }
                }
                None if ids_reads => return Err(error),
                None => {
                    if refs.is_live() {
                        refs = RefsState::Done(Err(error));
                    }
                }
            }
            continue;
        }
        if ids_reads {
            scanned.kinds[usize::from(kind.ordinal())] =
                Some(verify_ids::merge_kind(per_blob, kind, limit)?);
        }
        if refs_reads && let RefsState::Live { sets, tally } = &mut refs {
            match kind {
                ElementKind::Node => sets.nodes.merge(std::mem::take(&mut tally.node_overflow)),
                ElementKind::Way => sets.ways.merge(std::mem::take(&mut tally.way_overflow)),
                ElementKind::Relation => sets
                    .relations
                    .merge(std::mem::take(&mut tally.relation_overflow)),
            }
            if kind == refs_last {
                // Finished here, so its sets are gone before any later
                // phase and before the duplicate pass allocates.
                let RefsState::Live { sets, tally } = std::mem::replace(
                    &mut refs,
                    RefsState::Done(Err(crate::Error::internal(
                        "check pair: the reference check was not finished",
                    ))),
                ) else {
                    return Err(crate::Error::internal(
                        "check pair: reference state changed",
                    ));
                };
                refs = RefsState::Done(Ok(CheckOutcome::Complete(refs::finish(
                    tally,
                    &sets,
                    judge,
                    &refs_blobs,
                    host,
                ))));
            }
        }
    }
    host.invocation
        .counter("checkpair_physical_blobs", physical_blobs);

    let refs = match refs {
        RefsState::Done(outcome) => outcome,
        RefsState::Live { .. } => {
            return Err(crate::Error::internal(
                "check pair: the reference check never reached its last phase",
            ));
        }
    };

    let ids = ids_outcome(
        ids_stopped,
        front,
        scanned,
        &file,
        &schedules,
        max_ids,
        &pin,
        ids_opts,
        host,
    )?;
    host.invocation.marker("CHECKPAIR_SCAN_END");
    Ok(PairOutcome { ids, refs })
}

/// The ID check's outcome once its scan is merged into `scanned` and the
/// reference check's sets are gone: stopped, or `--full`'s duplicate pass
/// and the report.
#[allow(clippy::too_many_arguments)]
fn ids_outcome(
    ids_stopped: Option<StoppedCheck>,
    front: Option<(bool, bool)>,
    mut scanned: Scanned,
    file: &crate::read::input::InputFile,
    schedules: &KindSchedules,
    max_ids: [i64; 3],
    pin: &PinnedInput,
    ids_opts: &VerifyIdsOptions,
    host: &crate::host::Entered<'_>,
) -> Result<CheckOutcome<VerifyIdsReport>> {
    match (ids_stopped, front) {
        (Some(stopped), _) => Ok(CheckOutcome::Stopped(stopped)),
        (None, None) => Err(crate::Error::internal(
            "check pair: no front probe for a live ID check",
        )),
        (None, Some((header_sorted, indexed))) => {
            let duplicates = if ids_opts.full {
                CheckOutcome::stop_on_finding(verify_ids::duplicate_pass(
                    &mut scanned,
                    file,
                    schedules,
                    max_ids,
                    pin,
                    ids_opts.max_errors,
                    host,
                ))?
            } else {
                CheckOutcome::Complete(())
            };
            Ok(match duplicates {
                CheckOutcome::Complete(()) => {
                    CheckOutcome::Complete(scanned.into_report(ids_opts.max_errors).into_report(
                        header_sorted,
                        indexed,
                        ids_opts.full,
                    ))
                }
                CheckOutcome::Stopped(stopped) => CheckOutcome::Stopped(stopped),
            })
        }
    }
}

/// The start and end markers of each shared phase, indexed by ordinal.
const PHASE_MARKERS: [(&str, &str); 3] = [
    ("CHECKPAIR_NODES_START", "CHECKPAIR_NODES_END"),
    ("CHECKPAIR_WAYS_START", "CHECKPAIR_WAYS_END"),
    ("CHECKPAIR_RELATIONS_START", "CHECKPAIR_RELATIONS_END"),
];

/// Validate one blob of `kind`'s phase once and judge it for each check
/// reading it: the ID check (`ids`) by [`KindJudge`], the reference check
/// (`sets`, `None` when it does not read the phase or has failed) as its
/// own phase judges it. A validation failure is the phase's; the reference
/// judgement's failure rides in the result.
fn judge_blob(
    kind: ElementKind,
    buffer: crate::read::block::BlockBuffer<'_>,
    scratch: &mut PairScratch,
    ids: bool,
    sets: Option<&Sets>,
    judge: Judge,
) -> Result<PairBlob> {
    let mut ids_judge = ids.then(|| KindJudge::new(kind));
    let refs = match (kind, sets) {
        (ElementKind::Node, sets) => {
            let (KindIds { .. }, accepted) =
                buffer.project(|_, _| KindIds::of(ElementKind::Node, &mut scratch.ids))?;
            let bytes = accepted.bytes();
            if let Some(j) = ids_judge.as_mut() {
                scratch.ids.each(bytes, |_, id| {
                    j.push(id, None);
                    Ok(())
                })?;
            }
            sets.map(|sets| {
                refs::replay_nodes(&scratch.ids, bytes, &sets.nodes)
                    .map(|(count, overflow)| RefsBlob::Nodes(count, overflow))
            })
        }
        (ElementKind::Way, Some(sets)) => {
            let (WaySpans { .. }, accepted) =
                buffer.project(|_, _| WaySpans::new(None, &mut scratch.ways))?;
            if let Some(j) = ids_judge.as_mut() {
                for way in &scratch.ways {
                    j.push(way.id, None);
                }
            }
            Some(refs::judge_ways(&scratch.ways, accepted.bytes(), sets, judge).map(RefsBlob::Ways))
        }
        (ElementKind::Relation, Some(sets)) => {
            let (RelationSpans { .. }, accepted) =
                buffer.project(|_, _| RelationSpans::new(&mut scratch.relations))?;
            if let Some(j) = ids_judge.as_mut() {
                for relation in &scratch.relations {
                    j.push(relation.id, None);
                }
            }
            Some(
                refs::judge_relations(&scratch.relations, accepted.bytes(), sets, judge)
                    .map(RefsBlob::Relations),
            )
        }
        (kind, None) => {
            let (KindIds { .. }, accepted) =
                buffer.project(|_, _| KindIds::of(kind, &mut scratch.ids))?;
            if let Some(j) = ids_judge.as_mut() {
                scratch.ids.each(accepted.bytes(), |_, id| {
                    j.push(id, None);
                    Ok(())
                })?;
            }
            None
        }
    };
    Ok(PairBlob {
        ids: ids_judge.map(|j| j.r),
        refs,
    })
}

/// What one unindexed blob yields: the ID check's per-kind judgements and
/// group spans where it is live, the reference check's combined-phase
/// result (or its judgement failure) where it is live.
struct UnindexedPairBlob {
    ids: Option<([BlobVerifyResult; 3], KindSpans<usize>)>,
    refs: Option<Result<refs::UnindexedBlobResult>>,
}

/// An unindexed file (module documentation, "Unindexed input"): one pass
/// over every blob for both checks, then the reference check's re-reads of
/// the judgements it dropped and its report. Returns the ID check's merged
/// scan (empty where it is stopped, which `ids_stopped` then says) and the
/// reference check's outcome; an error is the ID check's, ending the call.
#[allow(clippy::too_many_lines)]
fn unindexed_pass(
    file: &crate::read::input::InputFile,
    blobs: &[ScheduledBlob],
    max_ids: [i64; 3],
    ids_stopped: &mut Option<StoppedCheck>,
    ids_opts: &VerifyIdsOptions,
    judge: Judge,
    host: &crate::host::Entered<'_>,
) -> Result<(Scanned, Result<CheckOutcome<RefCheckResult>>)> {
    let type_filter = TypeFilter::or_all(ids_opts.type_filter);
    let limit = verify_ids::effective_error_limit(ids_opts.max_errors);
    let mut scanned = Scanned {
        type_order: Vec::new(),
        type_order_total: 0,
        kinds: [None, None, None],
    };
    let mut refs = match Sets::allocate(file, max_ids, judge) {
        Ok(sets) => RefsState::Live {
            sets,
            tally: Tally::default(),
        },
        Err(error) => RefsState::Done(Err(error)),
    };
    let ids_reads = ids_stopped.is_none();
    let refs_reads = refs.is_live();
    if !ids_reads && !refs_reads {
        let RefsState::Done(outcome) = refs else {
            return Err(crate::Error::internal(
                "check pair: reference state changed",
            ));
        };
        return Ok((scanned, outcome));
    }

    // What the one projection records, fixed for the pass from who reads
    // it: node IDs for either check, way spans for the reference check's
    // judgement and the ID check's way IDs, relation spans under
    // `check_relations`, and otherwise the relation IDs the ID check
    // selects through the ID projection.
    let ids_selects = |kind| ids_reads && verify_ids::selects(&type_filter, kind);
    let relation_spans = refs_reads && judge.check_relations;
    let way_spans = refs_reads || ids_selects(ElementKind::Way);
    let mut id_kinds = crate::element_kind::ElementKindSet::default();
    if refs_reads || ids_selects(ElementKind::Node) {
        id_kinds.add(ElementKind::Node);
    }
    if ids_selects(ElementKind::Relation) && !relation_spans {
        id_kinds.add(ElementKind::Relation);
    }

    host.invocation.marker("CHECKPAIR_UNINDEXED_START");
    let mut per_kind: [Vec<Option<BlobVerifyResult>>; 3] = std::array::from_fn(|_| {
        if ids_reads {
            (0..blobs.len()).map(|_| None).collect()
        } else {
            Vec::new()
        }
    });
    let mut spans: KindSpans<(usize, usize)> = [None; 3];
    let mut again = refs::Again::default();
    let mut refs_failed: Option<crate::Error> = None;
    let refs_dead = AtomicBool::new(false);
    let phase = {
        let (sets, mut tally) = match &mut refs {
            RefsState::Live { sets, tally } => (Some(&*sets), Some(tally)),
            RefsState::Done(_) => (None, None),
        };
        let refs_dead = &refs_dead;
        let ids_selects = &ids_selects;
        parallel_project_phase(
            file,
            blobs,
            None,
            PairScratch::default,
            |_, buffer, scratch| -> Result<UnindexedPairBlob> {
                let PairScratch {
                    ids,
                    ways,
                    relations,
                } = scratch;
                ways.clear();
                relations.clear();
                let (((KindIds { .. }, _), (_, groups)), accepted) = buffer.project(|_, _| {
                    (
                        (
                            KindIds::new(id_kinds, &mut *ids),
                            way_spans.then(|| WaySpans::new(None, &mut *ways)),
                        ),
                        (
                            relation_spans.then(|| RelationSpans::new(&mut *relations)),
                            GroupKinds::default(),
                        ),
                    )
                })?;
                let bytes = accepted.bytes();
                let ids_blob = if ids_reads {
                    let mut judges = KINDS.map(KindJudge::new);
                    ids.each(bytes, |kind, id| {
                        if ids_selects(kind) {
                            judges[usize::from(kind.ordinal())].push(id, None);
                        }
                        Ok(())
                    })?;
                    if ids_selects(ElementKind::Way) {
                        for way in ways.iter() {
                            judges[1].push(way.id, None);
                        }
                    }
                    if ids_selects(ElementKind::Relation) && relation_spans {
                        for relation in relations.iter() {
                            judges[2].push(relation.id, None);
                        }
                    }
                    Some((judges.map(|j| j.r), groups.spans))
                } else {
                    None
                };
                let refs_blob = sets
                    .filter(|_| !refs_dead.load(Ordering::Relaxed))
                    .map(|sets| refs::judge_unindexed(ids, ways, relations, bytes, sets, judge));
                Ok(UnindexedPairBlob {
                    ids: ids_blob,
                    refs: refs_blob,
                })
            },
            |seq, blob| {
                if let Some((results, groups)) = blob.ids {
                    for (slots, r) in per_kind.iter_mut().zip(results) {
                        slots[seq] = Some(r);
                    }
                    verify_ids::fold_group_spans(&mut spans, seq, groups);
                }
                let (Some(tally), Some(result)) = (tally.as_deref_mut(), blob.refs) else {
                    return;
                };
                if refs_failed.is_some() {
                    return;
                }
                match result {
                    Ok(r) => again.merge(seq, r, tally, judge, host),
                    Err(error) => {
                        refs_dead.store(true, Ordering::Relaxed);
                        refs_failed = Some(file.locate_blob(&blobs[seq], error));
                    }
                }
            },
            host,
        )
    };
    host.invocation.marker("CHECKPAIR_UNINDEXED_END");
    host.invocation
        .counter("checkpair_physical_blobs", blobs.len() as u64);

    // The reference check's own failure came first, whatever ended the
    // pass after it.
    if let Some(error) = refs_failed {
        refs = RefsState::Done(Err(error));
    }
    if let Err(error) = phase {
        match StoppedCheck::from_error(&error) {
            Some(stopped) => {
                if refs.is_live() {
                    refs = RefsState::Done(Ok(CheckOutcome::Stopped(stopped.clone())));
                }
                if ids_reads {
                    *ids_stopped = Some(stopped);
                }
            }
            None if ids_reads => return Err(error),
            None => {
                if refs.is_live() {
                    refs = RefsState::Done(Err(error));
                }
            }
        }
    } else if ids_reads {
        verify_ids::check_type_order(
            &spans,
            &mut scanned.type_order,
            &mut scanned.type_order_total,
            limit,
        );
        for (kind, per_blob) in KINDS.into_iter().zip(per_kind) {
            if verify_ids::selects(&type_filter, kind) {
                scanned.kinds[usize::from(kind.ordinal())] =
                    Some(verify_ids::merge_kind(per_blob, kind, limit)?);
            }
        }
    }

    // The reference check's re-reads and report; its sets are gone before
    // the ID check's duplicate pass allocates.
    let refs = match refs {
        RefsState::Done(outcome) => outcome,
        RefsState::Live {
            mut sets,
            mut tally,
        } => {
            sets.nodes.merge(std::mem::take(&mut tally.node_overflow));
            let (way_again, relation_again) = again.into_schedules(blobs);
            CheckOutcome::stop_on_finding(refs::later_phases(
                file,
                &mut sets,
                tally,
                judge,
                0,
                blobs.len() as u64,
                &way_again,
                &relation_again,
                host,
            ))
        }
    };
    Ok((scanned, refs))
}
