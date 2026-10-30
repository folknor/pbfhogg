//! `isolation-cat`: one cat, the process's only library work, timed against
//! its CLI counterpart (brokkr#58) and profilable. Setup does nothing: the
//! bindings check reads metadata only, and the input is opened inside the
//! call. The assertion reads the call's record and the destination's
//! metadata, nothing instrumented.

use super::{
    CLEAN_NONE, POLICY_DIFFERENCES, Scenario, cat_into, cat_plan, installed_ok, process_json,
};
use crate::protocol::{Arg, AssertionDecl, CallDecl, Counterpart, Outcome, RoleDecl, ScenarioDecl};
use crate::runner::{CallSpec, Ctx};
use serde_json::{Value, json};

pub(crate) struct IsolationCat;

const ARGV: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input"),
    Arg::Literal("-o"),
    Arg::Output("out"),
];

impl Scenario for IsolationCat {
    fn decl(&self) -> ScenarioDecl {
        ScenarioDecl {
            id: "isolation-cat",
            description: "one cat in a fresh process, timed against the CLI and profilable",
            deadline_s: 600,
            hotpath: true,
            isolation: true,
            calls: vec![CallDecl {
                id: "a",
                label: "cat input -> out",
                cli: Some(Counterpart::output(ARGV, Some(false), POLICY_DIFFERENCES)),
            }],
            roles: vec![
                RoleDecl::dataset("input", "indexed"),
                RoleDecl::output("out", "a"),
            ],
            assertions: vec![AssertionDecl {
                id: "a.outcome",
                call: Some("a"),
            }],
        }
    }

    fn plan(&self) -> Value {
        json!({
            "schedule": { "order": ["a"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "started before the first call" },
            "process": process_json(self.nofile()),
            "calls": { "a": cat_plan(&json!(["input"]), "out", false) },
        })
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let record = ctx.call(CallSpec::plain("a"), |host, env| {
            let input = env.binding("input");
            cat_into(env, &[input], "out", None, &CLEAN_NONE, host).map(drop)
        })?;
        let judged = installed_ok("a.outcome", &record, "out");
        ctx.finish_call(&record, &[judged])?;
        Ok(Vec::new())
    }
}
