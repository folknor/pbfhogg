//! `error-path`: cat of an input that is no PBF. The call returns the
//! missing-header error, its bound output is refused and stays absent, and
//! nothing reaches stderr (brokkr's check); the CLI refuses the same input
//! with exit 2.

use super::{CLEAN_NONE, POLICY_DIFFERENCES, Scenario, account, cat_into, cat_plan, process_json};
use crate::protocol::{
    Arg, AssertionDecl, CallDecl, Counterpart, Expect, GeneratedKind, Outcome, Role, RoleDecl,
    ScenarioDecl,
};
use crate::runner::{CallSpec, Ctx};
use pbfhogg::{Decision, Effect};
use serde_json::{Value, json};

pub(crate) struct ErrorPath;

const ARGV: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("bad-input"),
    Arg::Literal("-o"),
    Arg::Output("out"),
];

impl Scenario for ErrorPath {
    fn decl(&self) -> ScenarioDecl {
        ScenarioDecl {
            id: "error-path",
            description: "cat of an empty file: a missing-header error, nothing published",
            deadline_s: 120,
            hotpath: false,
            isolation: false,
            calls: vec![CallDecl {
                id: "a",
                label: "cat bad-input -> out",
                cli: Some(Counterpart {
                    argv: ARGV,
                    scratch: false,
                    expect: Expect::Refusal { exit: 2 },
                    overwrite: Some(false),
                    policy_differences: POLICY_DIFFERENCES,
                }),
            }],
            roles: vec![
                RoleDecl {
                    id: "bad-input",
                    role: Role::Generated {
                        kind: GeneratedKind::File,
                    },
                },
                RoleDecl::output("out", "a"),
            ],
            assertions: vec![AssertionDecl {
                id: "a.refused",
                call: Some("a"),
            }],
        }
    }

    fn plan(&self) -> Value {
        json!({
            "schedule": { "order": ["a"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [{ "role": "bad-input", "recipe": "an empty file" }],
                       "rayon_global_pool": "started before the first call" },
            "process": process_json(self.nofile()),
            "calls": { "a": cat_plan(&json!(["bad-input"]), "out", false) },
        })
    }

    fn setup(&self, ctx: &Ctx<'_>) -> Result<(), String> {
        std::fs::write(ctx.binding("bad-input"), b"").map_err(|e| format!("writing bad-input: {e}"))
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let record = ctx.call(CallSpec::plain("a"), |host, env| {
            let input = env.binding("bad-input");
            cat_into(env, &[input], "out", None, &CLEAN_NONE, host).map(drop)
        })?;
        let out_absent = super::absent(ctx.binding("out"));
        // The output was bound before the input was read, so its owner's
        // drop decided it refused; nothing reached the destination.
        let ok = record
            .outcome
            .error()
            .is_some_and(|(kind, _)| kind == "missing_header")
            && record.cancel.is_none()
            && super::published_once(&record, Decision::RefusedError, "out", Effect::None)
            && out_absent;
        let detail = format!("{}, output absent {out_absent}", account(&record));
        ctx.finish_call(&record, &[Outcome::judge("a.refused", ok, detail)])?;
        Ok(Vec::new())
    }
}
