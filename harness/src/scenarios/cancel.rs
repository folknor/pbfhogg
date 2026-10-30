//! `cancel-pre-decision` and `cancel-post-decision`: one cat whose tap
//! cancels the call's own invocation when a named marker passes.
//!
//! Before the decision (`CAT_SCAN_START`, after the output was bound and
//! before the scan) the cancel refuses the pending output: the call ends
//! cancelled and publishes nothing. After it (`PUBLISH_START`, emitted once
//! the publication was authorized) there is nothing pending: the
//! publication completes and its outcome stands, so the call returns ok with
//! its output installed, and a CLI counterpart can still check it.
//!
//! The tap cancels from inside the marker callback, which the instrument
//! sink contract permits for `Invocation::cancel` alone (it takes only the
//! publication log's lock, under which nothing emits). It holds the
//! invocation in a slot it empties when it trips, and the runner releases
//! it after the seal in any case, so the invocation never keeps itself
//! alive through its own tap.

use super::{
    CLEAN_NONE, POLICY_DIFFERENCES, Scenario, account, cat_into, cat_plan, process_json,
    published_once,
};
use crate::protocol::{Arg, AssertionDecl, CallDecl, Counterpart, Outcome, RoleDecl, ScenarioDecl};
use crate::runner::{CallSpec, Ctx, Tap};
use pbfhogg::debug::InstrumentSink;
use pbfhogg::host::Invocation;
use pbfhogg::{CancelOutcome, Decision, Effect};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, PoisonError};

pub(crate) struct CancelCat {
    id: &'static str,
    description: &'static str,
    marker: &'static str,
    post: bool,
}

impl CancelCat {
    pub(crate) const PRE_DECISION: Self = Self {
        id: "cancel-pre-decision",
        description: "cat whose tap cancels at CAT_SCAN_START, before its output is decided",
        marker: "CAT_SCAN_START",
        post: false,
    };
    pub(crate) const POST_DECISION: Self = Self {
        id: "cancel-post-decision",
        description: "cat whose tap cancels at PUBLISH_START, after its output was authorized",
        marker: "PUBLISH_START",
        post: true,
    };
}

const ARGV: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input"),
    Arg::Literal("-o"),
    Arg::Output("out"),
];

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
    fn release(&self) {
        drop(
            self.invocation
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
    }
}

impl Scenario for CancelCat {
    fn decl(&self) -> ScenarioDecl {
        ScenarioDecl {
            id: self.id,
            description: self.description,
            deadline_s: 600,
            hotpath: false,
            isolation: false,
            calls: vec![CallDecl {
                id: "a",
                label: "cat input -> out, cancelled by its tap",
                // A cancelled call gets no counterpart; one cancelled after
                // its decision returns ok and published, which the CLI can
                // reproduce.
                cli: self
                    .post
                    .then(|| Counterpart::output(ARGV, Some(false), POLICY_DIFFERENCES)),
            }],
            roles: vec![
                RoleDecl::dataset("input", "indexed"),
                RoleDecl::output("out", "a"),
            ],
            assertions: vec![AssertionDecl {
                id: "a.cancel",
                call: Some("a"),
            }],
        }
    }

    fn plan(&self) -> Value {
        let mut call = cat_plan(&json!(["input"]), "out", false);
        if let Some(object) = call.as_object_mut() {
            object.insert(
                "cancel".to_owned(),
                json!({ "by": "the call's tap, on its calling thread", "at_marker": self.marker,
                        "times": 1 }),
            );
        }
        json!({
            "schedule": { "order": ["a"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "started before the first call" },
            "process": process_json(self.nofile()),
            "calls": { "a": call },
        })
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let tap = Arc::new(CancelAt {
            marker: self.marker,
            invocation: Mutex::new(None),
            result: Mutex::new(None),
        });
        let mut spec = CallSpec::plain("a");
        spec.tap = Some(Arc::clone(&tap) as Arc<dyn Tap>);
        let armed = Arc::clone(&tap);
        let mut record = ctx.call(spec, |host, env| {
            *armed
                .invocation
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(host.invocation.clone());
            let input = env.binding("input");
            cat_into(env, &[input], "out", None, &CLEAN_NONE, host).map(drop)
        })?;
        record.cancel = *tap.result.lock().unwrap_or_else(PoisonError::into_inner);
        let out_absent = super::absent(ctx.binding("out"));
        let ok = if self.post {
            record.outcome.is_ok()
                && matches!(
                    record.cancel,
                    Some(CancelOutcome::AfterDecision { authorized: 1 })
                )
                && published_once(&record, Decision::Authorized, "out", Effect::Installed)
                && !out_absent
        } else {
            record.outcome.is_cancelled()
                && matches!(
                    record.cancel,
                    Some(CancelOutcome::BeforeDecision { refused: 1 })
                )
                && published_once(&record, Decision::RefusedCancelled, "out", Effect::None)
                && out_absent
        };
        let detail = format!("{}, output absent {out_absent}", account(&record));
        ctx.finish_call(&record, &[Outcome::judge("a.cancel", ok, detail)])?;
        Ok(Vec::new())
    }
}
