//! `fixed-resources-executor`: one re-encoding cat (`--clean version`, so
//! the writer frames every block) under fixed `Resources` and an
//! `Executor::Pool` the harness built. Setup never starts rayon's global
//! pool; after the call `build_global` must still succeed, which proves no
//! work of the call initialised the global registry (any use of the global
//! pool does).
//!
//! What this does not prove: that the writer's framing ran on the caller's
//! pool. A framing path that ignored the executor and framed on the calling
//! thread would pass every assertion here; showing the positive needs an
//! observation point in the library that no release build has yet.

use super::{
    Scenario, cat_into, clean_json, host_json, output_policy_json, overrides_json, process_json,
    write_json,
};
use crate::protocol::{Arg, AssertionDecl, CallDecl, Counterpart, Outcome, RoleDecl, ScenarioDecl};
use crate::runner::{CallSpec, Ctx, Exec};
use pbfhogg::cat::CleanAttrs;
use serde_json::{Value, json};
use std::num::NonZeroUsize;
use std::sync::Arc;

pub(crate) struct FixedResourcesExecutor;

/// The fixed parallelism and the caller pool's size.
const THREADS: usize = 4;

const CLEAN_VERSION: CleanAttrs = CleanAttrs {
    version: true,
    changeset: false,
    timestamp: false,
    uid: false,
    user: false,
};

const ARGV: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input"),
    Arg::Literal("--clean"),
    Arg::Literal("version"),
    Arg::Literal("-o"),
    Arg::Output("out"),
];

const EXECUTOR_DIFFERENCES: &[&str] = &[
    "the CLI evicts its input from the page cache as it reads; the call keeps it",
    "the CLI raises its soft RLIMIT_NOFILE when a command asks; the harness set it at start",
    "the CLI caps malloc arenas when a command asks; the call leaves the allocator alone",
    "the CLI sizes work by probing the machine and runs on rayon's global pool; the call gets fixed parallelism and a pool of its own",
];

impl Scenario for FixedResourcesExecutor {
    fn decl(&self) -> ScenarioDecl {
        ScenarioDecl {
            id: "fixed-resources-executor",
            description: "cat --clean version on fixed Resources and a caller-built Executor::Pool",
            deadline_s: 600,
            hotpath: false,
            isolation: false,
            calls: vec![CallDecl {
                id: "a",
                label: "cat --clean version input -> out on the caller's pool",
                cli: Some(Counterpart::output(ARGV, Some(false), EXECUTOR_DIFFERENCES)),
            }],
            roles: vec![
                RoleDecl::dataset("input", "indexed"),
                RoleDecl::output("out", "a"),
            ],
            assertions: vec![
                AssertionDecl {
                    id: "a.outcome",
                    call: Some("a"),
                },
                AssertionDecl {
                    id: "global-registry-unused",
                    call: None,
                },
            ],
        }
    }

    fn plan(&self) -> Value {
        json!({
            "schedule": { "order": ["a"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "never started",
                       "caller_pool": { "threads": THREADS, "built": "before the call" } },
            "process": process_json(self.nofile()),
            "calls": { "a": {
                "command": "cat",
                "inputs": ["input"],
                "output": "out",
                "type_filter": null,
                "clean": clean_json(true),
                "write": write_json(),
                "header_overrides": overrides_json(),
                "output_policy": output_policy_json("refuse"),
                "host": host_json(&json!(THREADS), &json!({ "pool": THREADS }), &json!("none")),
            } },
        })
    }

    fn starts_global_pool(&self) -> bool {
        false
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(THREADS)
            .thread_name(|i| format!("harness-exec-{i}"))
            .build()
            .map_err(|e| format!("building the caller pool: {e}"))?;
        let mut spec = CallSpec::plain("a");
        spec.parallelism = NonZeroUsize::new(THREADS);
        spec.executor = Exec::Pool(Arc::new(pool));
        let record = ctx.call(spec, |host, env| {
            let input = env.binding("input");
            cat_into(env, &[input], "out", None, &CLEAN_VERSION, host).map(drop)
        })?;
        let judged = super::installed_ok("a.outcome", &record, "out");
        ctx.finish_call(&record, &[judged])?;
        // The call is sealed and its values dropped: whatever it did is done.
        let global = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build_global();
        Ok(vec![Outcome::judge(
            "global-registry-unused",
            global.is_ok(),
            match global {
                Ok(()) => "rayon's global pool was still unbuilt after the call".to_owned(),
                Err(e) => format!("rayon's global pool was already initialised: {e}"),
            },
        )])
    }
}
