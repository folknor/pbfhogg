//! The default pair of `check`: the ID check and the reference check over
//! one read of an indexed file.
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
//! An unindexed file is checked by the two checks' own engines in turn,
//! each reading the file once (the reference check reading again only the
//! blobs its combined phase could not judge). Folding them into one pass
//! is possible (one projection carrying node and relation IDs, way spans
//! and group kinds) and not done yet.
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
    self, BlobVerifyResult, KINDS, KindJudge, Scanned, VerifyIdsOptions, VerifyIdsReport,
};
use super::{CheckOutcome, StoppedCheck};
use crate::ElementKind;
use crate::error::Result;
use crate::owned::TypeFilter;
use crate::read::input::{InputSource, PinnedInput};
use crate::read::project_ids::{
    IdScratch, KindIds, RelationSpan, RelationSpans, WaySpan, WaySpans,
};
use crate::scan::classify::{KindSchedules, parallel_project_phase};

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

    if let KindSchedules::Unindexed(_) = schedules {
        drop(file);
        host.invocation.marker("CHECKPAIR_SCAN_END");
        return unindexed_in_turn(&pin, ids_stopped, ids_opts, refs_opts, host);
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

    let ids = match (ids_stopped, front) {
        (Some(stopped), _) => CheckOutcome::Stopped(stopped),
        (None, None) => {
            return Err(crate::Error::internal(
                "check pair: no front probe for a live ID check",
            ));
        }
        (None, Some((header_sorted, indexed))) => {
            let duplicates = if ids_opts.full {
                CheckOutcome::stop_on_finding(verify_ids::duplicate_pass(
                    &mut scanned,
                    &file,
                    &schedules,
                    max_ids,
                    &pin,
                    ids_opts.max_errors,
                    host,
                ))?
            } else {
                CheckOutcome::Complete(())
            };
            match duplicates {
                CheckOutcome::Complete(()) => {
                    CheckOutcome::Complete(scanned.into_report(ids_opts.max_errors).into_report(
                        header_sorted,
                        indexed,
                        ids_opts.full,
                    ))
                }
                CheckOutcome::Stopped(stopped) => CheckOutcome::Stopped(stopped),
            }
        }
    };
    host.invocation.marker("CHECKPAIR_SCAN_END");
    Ok(PairOutcome { ids, refs })
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

/// An unindexed file: the ID check's engine, then the reference check's,
/// each on the pin, attributed as the two calls attribute them.
fn unindexed_in_turn(
    pin: &PinnedInput,
    ids_stopped: Option<StoppedCheck>,
    ids_opts: &VerifyIdsOptions,
    refs_opts: &CheckRefsOptions,
    host: &crate::host::Entered<'_>,
) -> Result<PairOutcome> {
    let ids = match ids_stopped {
        Some(stopped) => CheckOutcome::Stopped(stopped),
        None => {
            CheckOutcome::stop_on_finding(verify_ids::verify_ids_inner(pin.into(), ids_opts, host))?
        }
    };
    let refs = CheckOutcome::stop_on_finding(refs::check_refs_inner(pin.into(), refs_opts, host));
    Ok(PairOutcome { ids, refs })
}
