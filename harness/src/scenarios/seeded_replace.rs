//! `seeded-replace`: two cats onto seeded destinations. `replace` replaces
//! its seed under `Overwrite::Replace` (a lone file: one rename, no backup,
//! effect `installed`). `refuse` meets its seed under `Overwrite::Refuse`:
//! the output is refused when it is built, inside the call, so the call
//! records no publication and its destination keeps the seed, and the CLI
//! without `--overwrite` refuses with exit 2. No fault is armed: a lone
//! replaced file that failed after its rename would keep the new content.

use super::{
    CLEAN_NONE, POLICY_DIFFERENCES, Scenario, WRITE, account, cat_plan, installed_ok,
    output_policy_json, overrides, process_json,
};
use crate::protocol::{
    Arg, AssertionDecl, CallDecl, Counterpart, Expect, Outcome, Role, RoleDecl, STREAM_SEALED,
    ScenarioDecl, seed,
};
use crate::runner::{CallSpec, Ctx};
use pbfhogg::Overwrite;
use serde_json::{Value, json};

pub(crate) struct SeededReplace;

const ARGV_REPLACE: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input"),
    Arg::Literal("-o"),
    Arg::Output("out-replace"),
];
const ARGV_REFUSE: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input"),
    Arg::Literal("-o"),
    Arg::Output("out-refuse"),
];

const fn seeded(id: &'static str, owner: &'static str) -> RoleDecl {
    RoleDecl {
        id,
        role: Role::OutputPbf {
            owner,
            seeded: true,
        },
    }
}

impl Scenario for SeededReplace {
    fn decl(&self) -> ScenarioDecl {
        ScenarioDecl {
            id: "seeded-replace",
            description: "cat over a seeded output: replaced under Replace, refused under Refuse",
            deadline_s: 600,
            hotpath: false,
            isolation: false,
            calls: vec![
                CallDecl {
                    id: "replace",
                    label: "cat input -> out-replace, replacing its seed",
                    cli: Some(Counterpart::output(
                        ARGV_REPLACE,
                        Some(true),
                        POLICY_DIFFERENCES,
                    )),
                    stream: STREAM_SEALED,
                },
                CallDecl {
                    id: "refuse",
                    label: "cat input -> out-refuse, refusing its seed",
                    cli: Some(Counterpart {
                        argv: ARGV_REFUSE,
                        scratch: false,
                        expect: Expect::Refusal { exit: 2 },
                        overwrite: Some(false),
                        policy_differences: POLICY_DIFFERENCES,
                    }),
                    stream: STREAM_SEALED,
                },
            ],
            roles: vec![
                RoleDecl::dataset("input", "indexed"),
                seeded("out-replace", "replace"),
                seeded("out-refuse", "refuse"),
            ],
            assertions: vec![
                AssertionDecl {
                    id: "replace.outcome",
                    call: Some("replace"),
                    covers: &["output.replace.v1"],
                },
                AssertionDecl {
                    id: "replace.seed-replaced",
                    call: Some("replace"),
                    covers: &["output.replace.v1"],
                },
                AssertionDecl {
                    id: "refuse.refused",
                    call: Some("refuse"),
                    covers: &["output.refuse-existing.v1"],
                },
            ],
        }
    }

    fn plan(&self) -> Value {
        let call = |output, overwrite| {
            let mut call = cat_plan(&json!(["input"]), output, false);
            if let Some(object) = call.as_object_mut() {
                object.insert("output_policy".to_owned(), output_policy_json(overwrite));
                object.insert("output_initial".to_owned(), json!("seed"));
            }
            call
        };
        json!({
            "schedule": { "order": ["replace", "refuse"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "started before the first call" },
            "process": process_json(self.nofile()),
            "calls": { "replace": call("out-replace", "replace"),
                       "refuse": call("out-refuse", "refuse") },
        })
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        for (call, role, stance) in [
            ("replace", "out-replace", "replace"),
            ("refuse", "out-refuse", "refuse"),
        ] {
            let overwrite = if stance == "replace" {
                Overwrite::Replace
            } else {
                Overwrite::Refuse
            };
            let mut spec = CallSpec::plain(call);
            spec.overwrite = stance;
            let record = ctx.call(spec, |host, env| {
                let out = super::output(env, role, overwrite, host)?;
                let input = env.binding("input");
                pbfhogg::cat::cat(&[input], out, None, &CLEAN_NONE, &WRITE, &overrides(), host)
                    .map(drop)
            })?;
            let now = std::fs::read(ctx.binding(role)).ok();
            let holds_seed = now.as_deref() == Some(seed(role).as_slice());
            let results = if call == "replace" {
                vec![
                    installed_ok("replace.outcome", &record, role),
                    Outcome::judge(
                        "replace.seed-replaced",
                        now.is_some() && !holds_seed,
                        format!(
                            "destination present {}, holds its seed {holds_seed}",
                            now.is_some()
                        ),
                    ),
                ]
            } else {
                let ok = record
                    .outcome
                    .error()
                    .is_some_and(|(kind, _)| kind == "output")
                    && record.publications.is_empty()
                    && holds_seed;
                vec![Outcome::judge(
                    "refuse.refused",
                    ok,
                    format!("{}, holds its seed {holds_seed}", account(&record)),
                )]
            };
            ctx.finish_call(&record, &results)?;
        }
        Ok(Vec::new())
    }
}
