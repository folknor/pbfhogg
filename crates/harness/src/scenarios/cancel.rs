//! `cancel-pre-decision` and `cancel-post-decision`: one cat whose tap
//! cancels the call's own invocation when a named marker passes.
//!
//! Before the decision (`CAT_SCAN_START`, after the output was bound and
//! before the scan) the cancel refuses the pending output: the call ends
//! cancelled and publishes nothing. After it (`PUBLISH_START`, emitted once
//! the publication was authorized) there is nothing pending: the
//! publication completes and its outcome stands, so the call returns ok with
//! its output installed.
//!
//! The tap cancels from inside the marker callback, which the instrument
//! sink contract permits for `Invocation::cancel` alone (it takes only the
//! publication log's lock, under which nothing emits). The runner arms it
//! with the call's invocation, which it holds in a slot it empties when it
//! trips, and releases it after the seal in any case, so the invocation
//! never keeps itself alive through its own tap.

use super::{Scenario, account, assertion, published_once};
use crate::protocol::Outcome;
use crate::runner::{CallRecord, Done, Observation, Tap};
use crate::workload::{CallPlan, Expect, InputRole, Workload, cat};
use pbfhogg::debug::InstrumentSink;
use pbfhogg::host::Invocation;
use pbfhogg::{CancelOutcome, Decision, Effect};
use serde_json::json;
use std::sync::{Arc, Mutex, PoisonError};

pub(crate) struct CancelCat {
    id: &'static str,
    description: &'static str,
    marker: &'static str,
    post: bool,
    covers: &'static [&'static str],
}

impl CancelCat {
    pub(crate) const PRE_DECISION: Self = Self {
        id: "cancel-pre-decision",
        description: "cat whose tap cancels at CAT_SCAN_START, before its output is decided",
        marker: "CAT_SCAN_START",
        post: false,
        covers: &["cancel.pre-decision.v1"],
    };
    pub(crate) const POST_DECISION: Self = Self {
        id: "cancel-post-decision",
        description: "cat whose tap cancels at PUBLISH_START, after its output was authorized",
        marker: "PUBLISH_START",
        post: true,
        covers: &["cancel.post-decision.v1"],
    };
}

/// Cancels its invocation the first time `marker` passes.
struct CancelAt {
    marker: &'static str,
    invocation: Mutex<Option<Invocation>>,
    result: Mutex<Option<CancelOutcome>>,
}

impl InstrumentSink for CancelAt {
    fn marker(&self, name: &str) {
        if name != self.marker {
            return;
        }
        let taken = self
            .invocation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(invocation) = taken {
            let outcome = invocation.cancel();
            *self.result.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
        }
    }

    fn counter(&self, _name: &str, _value: i64) {}
}

impl Tap for CancelAt {
    fn armed(&self, invocation: &Invocation) {
        *self
            .invocation
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(invocation.clone());
    }

    fn release(&self) {
        drop(
            self.invocation
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
    }

    fn cancel(&self) -> Option<CancelOutcome> {
        *self.result.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Scenario for CancelCat {
    fn workload(&self) -> Workload {
        let mut w = Workload::new(self.id, self.description, 600);
        w.inputs = vec![InputRole::dataset("input", "indexed")];
        // A cancel after the decision leaves the call's ok; one before it
        // ends the call cancelled.
        let mut call = CallPlan::plain(
            "a",
            "cat input -> out, cancelled by its tap",
            cat(&["input"], "out"),
        )
        .expecting(if self.post {
            Expect::Ok
        } else {
            Expect::Cancelled
        });
        call.notes = Some(json!({ "cancel": {
            "by": "the call's tap, on its calling thread", "at_marker": self.marker, "times": 1,
        } }));
        w.calls = vec![call];
        w.assertions = vec![assertion("a.cancel", Some("a"), self.covers)];
        w
    }

    fn observe(&self) -> Box<dyn Observation> {
        Box::new(Judge {
            post: self.post,
            tap: Arc::new(CancelAt {
                marker: self.marker,
                invocation: Mutex::new(None),
                result: Mutex::new(None),
            }),
        })
    }
}

struct Judge {
    post: bool,
    tap: Arc<CancelAt>,
}

impl Observation for Judge {
    fn tap(&self, _call: &str) -> Option<Arc<dyn Tap>> {
        Some(Arc::clone(&self.tap) as Arc<dyn Tap>)
    }

    fn judge_call(&self, record: &CallRecord, done: &Done<'_>) -> Vec<Outcome> {
        let out_absent = super::absent(done.binding("out"));
        let ok = if self.post {
            record.outcome.is_ok()
                && matches!(
                    record.cancel,
                    Some(CancelOutcome::AfterDecision { authorized: 1 })
                )
                && published_once(record, Decision::Authorized, "out", Effect::Installed)
                && matches!(out_absent, Ok(false))
        } else {
            record.outcome.is_cancelled()
                && matches!(
                    record.cancel,
                    Some(CancelOutcome::BeforeDecision { refused: 1 })
                )
                && published_once(record, Decision::RefusedCancelled, "out", Effect::None)
                && matches!(out_absent, Ok(true))
        };
        let detail = format!("{}, output absent {out_absent:?}", account(record));
        vec![Outcome::judge("a.cancel", ok, detail)]
    }
}
