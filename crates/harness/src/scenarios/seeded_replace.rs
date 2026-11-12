//! `seeded-replace`: two cats onto seeded destinations. `replace` replaces
//! its seed under `Overwrite::Replace` (a lone file: one rename, no backup,
//! effect `installed`). `refuse` meets its seed under `Overwrite::Refuse`:
//! the output is refused when it is built, inside the call, so the call
//! records no publication and its destination keeps the seed. No fault is
//! armed: a lone replaced file that failed after its rename would keep the
//! new content.

use super::{Scenario, account, assertion, installed_ok};
use crate::protocol::Outcome;
use crate::runner::{CallRecord, Done, Observation};
use crate::workload::{
    CLEAN_NONE, CallPlan, Command, Initial, InputRole, Inputs, OutputPlan, WRITE, Workload,
    no_overrides,
};
use pbfhogg::Overwrite;

pub(crate) struct SeededReplace;

fn onto_seed(role: &'static str, overwrite: Overwrite) -> Command {
    Command::Cat {
        inputs: Inputs::Roles(&["input"]),
        output: OutputPlan {
            role,
            overwrite,
            initial: Initial::Seed,
        },
        type_filter: None,
        clean: CLEAN_NONE,
        write: WRITE,
        overrides: no_overrides(),
    }
}

impl Scenario for SeededReplace {
    fn workload(&self) -> Workload {
        let mut w = Workload::new(
            "seeded-replace",
            "cat over a seeded output: replaced under Replace, refused under Refuse",
            600,
        );
        w.inputs = vec![InputRole::dataset("input", "indexed")];
        w.calls = vec![
            CallPlan::plain(
                "replace",
                "cat input -> out-replace, replacing its seed",
                onto_seed("out-replace", Overwrite::Replace),
            ),
            CallPlan::plain(
                "refuse",
                "cat input -> out-refuse, refusing its seed",
                onto_seed("out-refuse", Overwrite::Refuse),
            ),
        ];
        w.assertions = vec![
            assertion("replace.outcome", Some("replace"), &["output.replace.v1"]),
            assertion(
                "replace.seed-replaced",
                Some("replace"),
                &["output.replace.v1"],
            ),
            assertion(
                "refuse.refused",
                Some("refuse"),
                &["output.refuse-existing.v1"],
            ),
        ];
        w
    }

    fn observe(&self) -> Box<dyn Observation> {
        Box::new(Judge)
    }
}

struct Judge;

impl Observation for Judge {
    fn judge_call(&self, record: &CallRecord, done: &Done<'_>) -> Vec<Outcome> {
        if record.id == "replace" {
            let seed_state =
                super::file_assertions::holds_seed(done.binding("out-replace"), "out-replace");
            vec![
                installed_ok("replace.outcome", record, "out-replace"),
                Outcome::judge(
                    "replace.seed-replaced",
                    matches!(seed_state, Ok(false)),
                    format!("seed observation {seed_state:?}"),
                ),
            ]
        } else {
            let seed_state =
                super::file_assertions::holds_seed(done.binding("out-refuse"), "out-refuse");
            let ok = record
                .outcome
                .error()
                .is_some_and(|(kind, _)| kind == "output")
                && record.publications.is_empty()
                && matches!(seed_state, Ok(true));
            vec![Outcome::judge(
                "refuse.refused",
                ok,
                format!("{}, seed observation {seed_state:?}", account(record)),
            )]
        }
    }
}
