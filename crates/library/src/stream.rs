//! The call-stream verdict of a verification run: each call's FIFO records
//! held against its call document in the harness protocol.
//!
//! A defect the captured records prove fails the call's stream whether or
//! not the capture is complete, since losing records cannot manufacture one.
//! Only "every snapshot counter has its sealing emission" needs a complete
//! stream; an incomplete one is unevaluated.

use std::collections::{BTreeMap, BTreeSet};

use super::artefact::{CallDoc, Seal, SinkState, Status};
use super::describe::Scenario;
use crate::sidecar::{CallCensus, CaptureLoss, Marker, Record, SinkEnd, Timeline};

/// Violations named in full before the rest are counted.
use crate::tunables::VIOLATIONS_SHOWN;

/// One call's stream, judged and summarized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallStream {
    pub(crate) status: Status,
    /// Why it is not a pass; empty on a pass.
    pub(crate) detail: String,
    /// Each phase's name and length in microseconds, in time order.
    pub(crate) phases: Vec<(String, i64)>,
    /// The markers after return, in time order.
    pub(crate) tail: Vec<String>,
}

/// The failure a run's census proves: a tagged record named a call the
/// describe does not declare. Records are whole lines or dropped, so no
/// loss can produce one.
pub(crate) fn undeclared_failure(scenario: &Scenario, census: &CallCensus) -> Option<String> {
    let declared: BTreeSet<&str> = scenario.calls.iter().map(|c| c.id.as_str()).collect();
    let unknown: Vec<&str> = census
        .ids
        .iter()
        .map(String::as_str)
        .filter(|id| !declared.contains(id))
        .collect();
    if !unknown.is_empty() {
        return Some(format!(
            "the FIFO carried records of call {}, which describe does not declare",
            unknown
                .iter()
                .map(|id| format!("{id:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // The census holds one more id than the declared calls before it
    // overflows, so an overflow proves an id beyond them.
    census
        .overflowed
        .then(|| "the FIFO carried records of more call ids than describe declares".to_owned())
}

/// A u64 counter value as the wire carries it.
fn saturated(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_ts(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// What one pass over a call's records in arrival order found.
#[derive(Default)]
struct Order<'a> {
    violations: Vec<String>,
    /// The first terminal record's ending and drops.
    terminal: Option<(SinkEnd, u64)>,
    /// A sealing or terminal record arrived.
    sealing_began: bool,
    /// Each sealing emission's value, by name.
    sealed: BTreeMap<&'a str, i64>,
}

/// Judge call `call`'s stream: its records, in arrival order, within the
/// run's `records`, against `doc`; `loss` is the run's capture loss.
pub(crate) fn judge(
    call: &str,
    doc: &CallDoc,
    records: &[Record],
    loss: &CaptureLoss,
) -> CallStream {
    let mine: Vec<&Record> = records.iter().filter(|r| r.call() == Some(call)).collect();
    let mut order = order(&mine, doc);
    let snapshot = match (&doc.seal, &doc.snapshot) {
        (Seal::Sealed, Some(snapshot)) => snapshot,
        // Checked before any snapshot comparison: an unsealed call has none.
        _ => {
            if order.sealing_began {
                order.violations.push(
                    "the call did not seal, yet its sink emitted sealing or terminal records"
                        .to_owned(),
                );
            }
            return finish(
                &order.violations,
                Some("the call did not seal".to_owned()),
                &mine,
                doc,
            );
        }
    };
    snapshot_violations(&mut order, &mine, snapshot, doc, loss);

    let account = doc.stream;
    let mut incomplete = loss.stream_causes();
    match order.terminal {
        None => incomplete.push(format!(
            "no terminal record arrived (the sink dropped {})",
            crate::output::count(usize::try_from(account.drops).unwrap_or(usize::MAX), "line")
        )),
        Some((SinkEnd::Failed, _)) => incomplete.push("the sink failed".to_owned()),
        Some((SinkEnd::Sealed, drops)) if drops > 0 => incomplete.push(format!(
            "the sink dropped {}",
            crate::output::count(usize::try_from(drops).unwrap_or(usize::MAX), "line")
        )),
        Some(_) => {}
    }
    // The one judgment that needs every record: each snapshot counter has
    // its sealing emission.
    if incomplete.is_empty() {
        let missing: Vec<String> = snapshot
            .keys()
            .filter(|name| !order.sealed.contains_key(name.as_str()))
            .map(|n| format!("{n:?}"))
            .collect();
        if !missing.is_empty() {
            order
                .violations
                .push(format!("sealing never emitted {}", missing.join(", ")));
        }
    }
    let unevaluated =
        (!incomplete.is_empty()).then(|| format!("incomplete: {}", incomplete.join("; ")));
    finish(&order.violations, unevaluated, &mine, doc)
}

/// The sealing emissions and ordinary counters against the snapshot, and
/// the terminal record against the artefact's account of the sink.
fn snapshot_violations(
    order: &mut Order<'_>,
    mine: &[&Record],
    snapshot: &BTreeMap<String, u64>,
    doc: &CallDoc,
    loss: &CaptureLoss,
) {
    for (name, value) in &order.sealed {
        match snapshot.get(*name) {
            None => order
                .violations
                .push(format!("sealing emits {name:?}, which the snapshot lacks")),
            Some(final_value) if saturated(*final_value) != *value => {
                order.violations.push(format!(
                    "sealing emits {name:?} = {value}, but the snapshot holds {}",
                    saturated(*final_value)
                ));
            }
            Some(_) => {}
        }
    }
    for record in mine {
        if let Record::Counter(c) = record
            && let Some(final_value) = snapshot.get(&c.name)
            && c.value > saturated(*final_value)
        {
            order.violations.push(format!(
                "`{}` is above the snapshot's final {}",
                record.wire(),
                saturated(*final_value)
            ));
        }
    }
    let account = doc.stream;
    match order.terminal {
        Some((end, drops)) => {
            let same_state = matches!(
                (end, account.state),
                (SinkEnd::Sealed, SinkState::Sealed) | (SinkEnd::Failed, SinkState::Failed)
            );
            if !same_state || drops != account.drops {
                order.violations.push(format!(
                    "the terminal record says {} drops={drops}, the artefact {} drops={}",
                    end.name(),
                    account.state.name(),
                    account.drops
                ));
            }
        }
        // The artefact's drops count the terminal record's own, so nothing
        // explains its absence.
        None if !loss.stream_lost() && account.drops == 0 => order.violations.push(
            "no terminal record arrived, yet the sink dropped nothing and the reader lost nothing"
                .to_owned(),
        ),
        None => {}
    }
}

/// The checks of arrival order and time a call's records must meet.
fn order<'a>(mine: &[&'a Record], doc: &CallDoc) -> Order<'a> {
    let (entry, ret, seal_end) = (
        to_ts(doc.entry_us),
        to_ts(doc.return_us),
        to_ts(doc.seal_us),
    );
    let mut found = Order::default();
    let mut terminals = 0usize;
    let mut ordinary_max: Option<i64> = None;
    let mut closing_min: Option<i64> = None;
    let mut closing_last: Option<i64> = None;
    let violations = &mut found.violations;
    for &record in mine {
        let ts = record.timestamp_us();
        let wire = record.wire();
        if ts < entry || ts > seal_end {
            violations.push(format!("`{wire}` lies outside entry_us..seal_us"));
        }
        if terminals > 0 {
            violations.push(match record {
                Record::Terminal(_) => format!("`{wire}` is a second terminal record"),
                _ => format!("`{wire}` arrived after the terminal record"),
            });
        }
        match record {
            Record::Marker(_) | Record::Counter(_) => {
                if found.sealing_began {
                    violations.push(format!("`{wire}` arrived after sealing began"));
                }
                ordinary_max = Some(ordinary_max.map_or(ts, |m| m.max(ts)));
            }
            Record::Sealing(_) | Record::Terminal(_) => {
                found.sealing_began = true;
                if ts < ret {
                    violations.push(format!("`{wire}` is stamped before return_us"));
                }
                if closing_last.is_some_and(|last| ts < last) {
                    violations.push(format!(
                        "`{wire}` is stamped before the sealing record ahead of it"
                    ));
                }
                closing_last = Some(ts);
                closing_min = Some(closing_min.map_or(ts, |m| m.min(ts)));
            }
        }
        match record {
            Record::Sealing(s) => {
                if found.sealed.insert(&s.name, s.value).is_some() {
                    violations.push(format!("`{wire}` emits {:?} a second time", s.name));
                }
            }
            Record::Terminal(t) => {
                terminals += 1;
                if found.terminal.is_none() {
                    found.terminal = Some((t.end, t.drops));
                }
            }
            Record::Marker(_) | Record::Counter(_) => {}
        }
    }
    // Every ordinary write completed before sealing began, and each line is
    // stamped before it is written.
    if let (Some(ordinary), Some(closing)) = (ordinary_max, closing_min)
        && ordinary > closing
    {
        violations.push(format!(
            "an ordinary record is stamped {ordinary}, after a sealing record stamped {closing}"
        ));
    }
    found
}

/// The verdict from what was found, with the call's phases and tail.
fn finish(
    violations: &[String],
    unevaluated: Option<String>,
    mine: &[&Record],
    doc: &CallDoc,
) -> CallStream {
    let (entry, ret) = (to_ts(doc.entry_us), to_ts(doc.return_us));
    let (status, detail) = if violations.is_empty() {
        match unevaluated {
            Some(why) => (Status::Unevaluated, why),
            None => (Status::Pass, String::new()),
        }
    } else {
        let mut detail = violations
            .iter()
            .take(VIOLATIONS_SHOWN)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        if violations.len() > VIOLATIONS_SHOWN {
            detail.push_str(&format!(
                "\nand {} more",
                violations.len() - VIOLATIONS_SHOWN
            ));
        }
        (Status::Fail, detail)
    };
    // A marker stamped before entry is neither a phase nor the tail (its
    // time is a violation already).
    let markers: Vec<Marker> = mine
        .iter()
        .filter_map(|r| match r {
            Record::Marker(m) if m.timestamp_us >= entry => Some(m.clone()),
            _ => None,
        })
        .collect();
    let (phase_markers, tail_markers): (Vec<Marker>, Vec<Marker>) =
        markers.into_iter().partition(|m| m.timestamp_us <= ret);
    let phases = Timeline::closed(&phase_markers, ret)
        .segments()
        .into_iter()
        .map(|s| {
            let len = s.end_us.unwrap_or(s.start_us).saturating_sub(s.start_us);
            (s.name, len)
        })
        .collect();
    let tail = Timeline::open(&tail_markers)
        .marker_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    CallStream {
        status,
        detail,
        phases,
        tail,
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
