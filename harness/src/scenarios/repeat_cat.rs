//! `repeat-cat`: the same cat twice in a row, each call under its own
//! invocation and host, each with a CLI counterpart that must write an
//! equal output.

use super::{CLEAN_NONE, POLICY_DIFFERENCES, Scenario, WRITE, cat_plan, installed_ok, overrides};
use crate::protocol::{Arg, AssertionDecl, CallDecl, Counterpart, Outcome, RoleDecl, ScenarioDecl};
use crate::runner::{CallSpec, Ctx};
use pbfhogg::{Output, Overwrite, Staging};
use serde_json::{Value, json};

pub(crate) struct RepeatCat;

/// The calls, each with the output role it owns and its assertion.
const CALLS: [(&str, &str, &str); 2] = [("a", "out-a", "a.outcome"), ("b", "out-b", "b.outcome")];

const ARGV_A: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input"),
    Arg::Literal("-o"),
    Arg::Output("out-a"),
];
const ARGV_B: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input"),
    Arg::Literal("-o"),
    Arg::Output("out-b"),
];

impl Scenario for RepeatCat {
    fn decl(&self) -> ScenarioDecl {
        let call = |id, label, argv| CallDecl {
            id,
            label,
            cli: Some(Counterpart::output(argv, Some(false), POLICY_DIFFERENCES)),
        };
        ScenarioDecl {
            id: "repeat-cat",
            description: "cat the input twice in a row, each call under its own invocation",
            deadline_s: 600,
            hotpath: false,
            isolation: false,
            calls: vec![
                call("a", "cat input -> out-a", ARGV_A),
                call("b", "cat input -> out-b", ARGV_B),
            ],
            roles: vec![
                RoleDecl::dataset("input", "indexed"),
                RoleDecl::output("out-a", "a"),
                RoleDecl::output("out-b", "b"),
            ],
            assertions: vec![
                AssertionDecl {
                    id: "a.outcome",
                    call: Some("a"),
                },
                AssertionDecl {
                    id: "b.outcome",
                    call: Some("b"),
                },
                AssertionDecl {
                    id: "outputs-equal",
                    call: None,
                },
            ],
        }
    }

    fn plan(&self) -> Value {
        let call = |output| cat_plan(&json!(["input"]), output, false);
        json!({
            "schedule": { "order": ["a", "b"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "started before the first call" },
            "process": super::process_json(self.nofile()),
            "calls": { "a": call("out-a"), "b": call("out-b") },
        })
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        for (call, output, assertion) in CALLS {
            let record = ctx.call(CallSpec::plain(call), |host, env| {
                let out = Output::with_staging(
                    env.binding(output),
                    Overwrite::Refuse,
                    Staging::DirWithBackups(env.private_dir()),
                    &host.diagnostics,
                )?;
                let input = env.binding("input");
                pbfhogg::cat::cat(&[input], out, None, &CLEAN_NONE, &WRITE, &overrides(), host)
                    .map(drop)
            })?;
            let judged = installed_ok(assertion, &record, output);
            ctx.finish_call(&record, &[judged])?;
        }
        Ok(vec![outputs_equal(ctx)])
    }
}

/// `outputs-equal`: the two outputs are byte-identical.
fn outputs_equal(ctx: &Ctx<'_>) -> Outcome {
    let read = |role| std::fs::read(ctx.binding(role));
    match (read("out-a"), read("out-b")) {
        (Ok(a), Ok(b)) => Outcome::judge(
            "outputs-equal",
            a == b,
            format!("out-a {} bytes, out-b {} bytes", a.len(), b.len()),
        ),
        (a, b) => Outcome::judge(
            "outputs-equal",
            false,
            format!("cannot read both outputs: {:?} {:?}", a.err(), b.err()),
        ),
    }
}
