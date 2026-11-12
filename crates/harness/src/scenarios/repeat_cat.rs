//! `repeat-cat`: the same cat twice in a row, each call under its own
//! invocation and host; the two outputs must be byte-identical.

use super::{Scenario, assertion, installed_ok};
use crate::protocol::Outcome;
use crate::runner::{CallRecord, Done, Observation};
use crate::workload::{CallPlan, InputRole, Workload, cat};

pub(crate) struct RepeatCat;

/// Both calls installed and their outputs are byte-identical.
const REPEATABLE: &[&str] = &["calls.repeatable.v1"];

impl Scenario for RepeatCat {
    fn workload(&self) -> Workload {
        let mut w = Workload::new(
            "repeat-cat",
            "cat the input twice in a row, each call under its own invocation",
            600,
        );
        w.inputs = vec![InputRole::dataset("input", "indexed")];
        w.calls = vec![
            CallPlan::plain("a", "cat input -> out-a", cat(&["input"], "out-a")),
            CallPlan::plain("b", "cat input -> out-b", cat(&["input"], "out-b")),
        ];
        w.assertions = vec![
            assertion("a.outcome", Some("a"), REPEATABLE),
            assertion("b.outcome", Some("b"), REPEATABLE),
            assertion("outputs-equal", None, REPEATABLE),
        ];
        w
    }

    fn observe(&self) -> Box<dyn Observation> {
        Box::new(Judge)
    }
}

struct Judge;

impl Observation for Judge {
    fn judge_call(&self, record: &CallRecord, _done: &Done<'_>) -> Vec<Outcome> {
        if record.id == "a" {
            vec![installed_ok("a.outcome", record, "out-a")]
        } else {
            vec![installed_ok("b.outcome", record, "out-b")]
        }
    }

    /// `outputs-equal`: the two outputs are byte-identical.
    fn judge_scenario(&self, done: &Done<'_>) -> Vec<Outcome> {
        let judged =
            match super::file_assertions::files_equal(done.binding("out-a"), done.binding("out-b"))
            {
                Ok(equal) => Outcome::judge(
                    "outputs-equal",
                    equal,
                    format!("outputs byte-identical {equal}"),
                ),
                Err(error) => Outcome::judge(
                    "outputs-equal",
                    false,
                    format!("cannot compare outputs: {error}"),
                ),
            };
        vec![judged]
    }
}
