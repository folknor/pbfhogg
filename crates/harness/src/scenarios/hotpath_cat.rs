//! `hotpath-cat`: one cat, the process's only library work, profilable.
//! Setup does nothing: the bindings check reads metadata only, and the
//! input is opened inside the call. The assertion reads the call's record,
//! nothing instrumented.

use super::{Scenario, assertion, installed_ok};
use crate::protocol::Outcome;
use crate::runner::{CallRecord, Done, Observation};
use crate::workload::{CallPlan, InputRole, Workload, cat};

pub(crate) struct HotpathCat;

impl Scenario for HotpathCat {
    fn workload(&self) -> Workload {
        let mut w = Workload::new(
            "hotpath-cat",
            "one cat in a fresh process, the only library work in it: profilable",
            600,
        );
        w.hotpath = true;
        w.inputs = vec![InputRole::dataset("input", "indexed")];
        w.calls = vec![CallPlan::plain(
            "a",
            "cat input -> out",
            cat(&["input"], "out"),
        )];
        w.assertions = vec![assertion("a.outcome", Some("a"), &[])];
        w
    }

    fn observe(&self) -> Box<dyn Observation> {
        Box::new(Judge)
    }
}

struct Judge;

impl Observation for Judge {
    fn judge_call(&self, record: &CallRecord, _done: &Done<'_>) -> Vec<Outcome> {
        vec![installed_ok("a.outcome", record, "out")]
    }
}
