//! The publication log of one invocation: every publication it decided, in
//! order, with each member's effect on its destination (ADR-0017, ADR-0019).
//!
//! An output bound to an invocation ([`Output::bind`](crate::Output::bind)
//! and its siblings) is enlisted here as pending: the invocation has accepted
//! responsibility for deciding it. Every unit is decided exactly once, by
//! one transition from pending to a row: by its publication
//! ([`authorized`](Decision::Authorized), or refused), by
//! [`Invocation::cancel`](crate::host::Invocation::cancel)
//! ([`refused_cancelled`](Decision::RefusedCancelled)), or by its owner
//! dropping it undecided. Rows are numbered from 1 without gaps, in decision
//! order, which for authorized rows is also execution order: publications of
//! one invocation run one after the other through completion.
//!
//! The log is one mutex of plain data. Nothing runs under it but its own
//! bookkeeping (no hook, sink, filesystem operation or destructor of a
//! caller's value), and every transition reserves what it allocates before
//! it changes anything, so no mutation can unwind half done: a poisoned lock
//! is recovered.

use std::path::{Path, PathBuf};

/// How a publication was decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Publication went ahead. Its members' effects say what it did.
    Authorized,
    /// Refused because the invocation was cancelled first.
    RefusedCancelled,
    /// Refused for another reason: an output not ready to publish, a
    /// publication re-entered from a callback, or an owner dropped without
    /// publishing.
    RefusedError,
}

/// What a publication did to one destination, relative to the destination
/// just before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// The destination is as it was (a rollback to absence included).
    None,
    /// The new generation is at the destination (a rollback that could not
    /// remove it included).
    Installed,
    /// The previous generation was moved aside and put back.
    Restored,
    /// The previous generation was moved aside and not put back: the
    /// destination is absent, and the previous generation is in a
    /// `.pbfhogg-backup-*` entry the error names.
    Displaced,
}

/// One member of a publication: a destination and the effect on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedMember {
    destination: PathBuf,
    effect: Effect,
}

impl PublishedMember {
    /// The path the caller named.
    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// What the publication did to it. `None` while the publication is
    /// still running.
    #[must_use]
    pub fn effect(&self) -> Effect {
        self.effect
    }
}

/// One decided publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    ordinal: u64,
    decision: Decision,
    finished: bool,
    members: Vec<PublishedMember>,
}

impl Publication {
    /// Its position among the invocation's decisions, from 1.
    #[must_use]
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }

    /// How it was decided.
    #[must_use]
    pub fn decision(&self) -> Decision {
        self.decision
    }

    /// Whether its effects are final. A refused publication is finished when
    /// decided; an authorized one once it has completed or rolled back.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Every destination it covers, in the order the output was built with:
    /// one for a file or a directory, one per member for a set.
    #[must_use]
    pub fn members(&self) -> &[PublishedMember] {
        &self.members
    }
}

/// What [`Invocation::cancel`](crate::host::Invocation::cancel) found, read
/// at the instant it tripped the token, under the publication log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CancelOutcome {
    /// Pending publications existed; each was decided
    /// [`RefusedCancelled`](Decision::RefusedCancelled), in bind order.
    /// `refused` is the ordinal of the first.
    BeforeDecision { refused: u64 },
    /// Nothing was pending, but a publication had been authorized:
    /// `authorized` is the latest such ordinal. It completes regardless.
    AfterDecision { authorized: u64 },
    /// Nothing was pending and nothing had been authorized.
    NoPublicationPending,
    /// The invocation is sealed (or a seal is under way): it admits nothing,
    /// so there was nothing to cancel and nothing was tripped.
    Ended,
}

/// The result of one decision attempt on a unit.
pub(crate) enum Decided {
    /// This attempt decided it, as `decision`, at `ordinal`.
    Now { decision: Decision, ordinal: u64 },
    /// It had already been decided (by a cancel, say), as this.
    Earlier(Decision),
}

struct Pending {
    unit: u64,
    members: Vec<PublishedMember>,
}

#[derive(Default)]
pub(crate) struct PublicationLog {
    rows: Vec<Publication>,
    /// Undecided units, in bind order.
    pending: Vec<Pending>,
    next_unit: u64,
    latest_authorized: Option<u64>,
    /// The thread running an authorized publication, from its decision to
    /// its row's finalization: a publication attempted on it again is a
    /// callback re-entering publication.
    executing: Option<std::thread::ThreadId>,
    /// Every unit decided since enlisting and its decision, for a publisher
    /// whose unit was decided by someone else while it waited. Indexed by
    /// unit id; grows by one per bind, reserved at enlistment.
    decided: Vec<Option<Decision>>,
}

impl PublicationLog {
    /// Enlist a unit covering `destinations`; returns its id.
    pub(crate) fn enlist(&mut self, destinations: Vec<PathBuf>) -> u64 {
        let members: Vec<PublishedMember> = destinations
            .into_iter()
            .map(|destination| PublishedMember {
                destination,
                effect: Effect::None,
            })
            .collect();
        self.pending.reserve(1);
        self.decided.reserve(1);
        let unit = self.next_unit;
        self.pending.push(Pending { unit, members });
        self.decided.push(None);
        self.next_unit += 1;
        unit
    }

    /// Whether this thread is executing a publication of this invocation.
    pub(crate) fn reentered(&self) -> bool {
        self.executing == Some(std::thread::current().id())
    }

    /// The one pending-to-decided transition. Refused rows are born
    /// finished. An authorized decision marks this thread executing.
    pub(crate) fn decide(&mut self, unit: u64, decision: Decision) -> Decided {
        let Some(index) = self.pending.iter().position(|p| p.unit == unit) else {
            let earlier = usize::try_from(unit)
                .ok()
                .and_then(|u| self.decided.get(u).copied().flatten())
                .unwrap_or(Decision::RefusedError);
            return Decided::Earlier(earlier);
        };
        self.rows.reserve(1);
        // Nothing below allocates or calls out.
        let Pending { members, .. } = self.pending.remove(index);
        let ordinal = self.push_row(decision, members);
        if let Some(slot) = usize::try_from(unit)
            .ok()
            .and_then(|u| self.decided.get_mut(u))
        {
            *slot = Some(decision);
        }
        if decision == Decision::Authorized {
            self.latest_authorized = Some(ordinal);
            self.executing = Some(std::thread::current().id());
        }
        Decided::Now { decision, ordinal }
    }

    /// Capacity must be reserved.
    fn push_row(&mut self, decision: Decision, members: Vec<PublishedMember>) -> u64 {
        let ordinal = u64::try_from(self.rows.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        self.rows.push(Publication {
            ordinal,
            decision,
            finished: decision != Decision::Authorized,
            members,
        });
        ordinal
    }

    /// Record an authorized row's effects, mark it finished and end the
    /// execution. `effects` is in member order.
    pub(crate) fn finish(&mut self, ordinal: u64, effects: impl Iterator<Item = Effect>) {
        if let Some(row) = usize::try_from(ordinal)
            .ok()
            .and_then(|o| o.checked_sub(1))
            .and_then(|i| self.rows.get_mut(i))
        {
            for (member, effect) in row.members.iter_mut().zip(effects) {
                member.effect = effect;
            }
            row.finished = true;
        }
        self.executing = None;
    }

    /// Cancel's half: decide every pending unit `RefusedCancelled`.
    pub(crate) fn cancel(&mut self) -> CancelOutcome {
        if self.pending.is_empty() {
            return match self.latest_authorized {
                Some(authorized) => CancelOutcome::AfterDecision { authorized },
                None => CancelOutcome::NoPublicationPending,
            };
        }
        self.rows.reserve(self.pending.len());
        let mut first = None;
        for Pending { unit, members } in std::mem::take(&mut self.pending) {
            let ordinal = self.push_row(Decision::RefusedCancelled, members);
            first.get_or_insert(ordinal);
            if let Some(slot) = usize::try_from(unit)
                .ok()
                .and_then(|u| self.decided.get_mut(u))
            {
                *slot = Some(Decision::RefusedCancelled);
            }
        }
        CancelOutcome::BeforeDecision {
            refused: first.unwrap_or(0),
        }
    }

    /// Every row, copied.
    pub(crate) fn rows(&self) -> Vec<Publication> {
        self.rows.clone()
    }
}
