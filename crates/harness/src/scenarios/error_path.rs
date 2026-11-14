//! `error-path`: cat of an input that is no PBF. The call returns the
//! missing-header error, its bound output is refused and stays absent, and
//! nothing reaches stderr (brokkr's check).

use super::{Scenario, account, assertion};
use crate::protocol::Outcome;
use crate::runner::{CallRecord, Done, Observation};
use crate::workload::{CallPlan, Expect, ExpectedError, InputRole, Recipe, Workload, cat};
use pbfhogg::{Decision, Effect};

pub(crate) struct ErrorPath;

impl Scenario for ErrorPath {
    fn workload(&self) -> Workload {
        let mut w = Workload::new(
            "error-path",
            "cat of an empty file: a missing-header error, nothing published",
            120,
        );
        w.inputs = vec![InputRole::generated("bad-input", Recipe::EmptyFile)];
        w.calls = vec![
            CallPlan::plain("a", "cat bad-input -> out", cat(&["bad-input"], "out"))
                .expecting(Expect::Error(ExpectedError::MissingHeader)),
        ];
        w.assertions = vec![assertion(
            "a.refused",
            Some("a"),
            &["errors.refused-unpublished.v1"],
        )];
        w
    }

    fn observe(&self) -> Box<dyn Observation> {
        Box::new(Judge)
    }
}

struct Judge;

impl Observation for Judge {
    fn judge_call(&self, record: &CallRecord, done: &Done<'_>) -> Vec<Outcome> {
        let out_absent = super::absent(done.binding("out"));
        // The output was bound before the input was read, so its owner's
        // drop decided it refused; nothing reached the destination.
        let ok = record
            .outcome
            .error()
            .is_some_and(|(kind, _)| kind == "missing_header")
            && record.cancel.is_none()
            && super::published_once(record, Decision::RefusedError, "out", Effect::None)
            && matches!(out_absent, Ok(true));
        let detail = format!("{}, output absent {out_absent:?}", account(record));
        vec![Outcome::judge("a.refused", ok, detail)]
    }
}
