//! `fixed-resources-executor`: two cats under fixed `Resources` and one
//! `Executor::Pool` the harness built, driven from the harness's own thread
//! (never from a worker of that pool). Setup never starts rayon's global
//! pool; after both calls `build_global` must still succeed, which proves no
//! work of either call initialised the global registry (any use of the global
//! pool does).
//!
//! Call `a` is `cat --clean version` on the indexed variant. Its clean pass
//! frames every block on cat's own classify workers and hands the writer
//! finished frames, so the executor runs none of its framing: `a` proves only
//! that the global pool was never used.
//!
//! Call `b` is `cat --type node,way,relation --force` on the raw variant (no
//! indexdata). Every blob is decoded on the calling thread and re-encoded
//! through the pipelined writer, whose framing is dispatched to the
//! executor. The writer counts each successfully framed payload
//! (`writer_payload_framed_items`) and, checked on the thread that framed it,
//! the subset framed on a worker of its own executor
//! (`writer_payload_framed_on_executor_items`). Equality of the two, with no
//! blob passed through and some framed, is the proof that writer framing ran
//! on the caller's pool: framing done inline on the calling thread, on
//! rayon's global pool, on another pool or on any library thread counts in
//! the first and not the second.

use super::{
    Scenario, cat_into, clean_json, host_json, output, output_policy_json, overrides,
    overrides_json, process_json, write_json,
};
use crate::protocol::{
    Arg, AssertionDecl, CallDecl, Counterpart, Outcome, RoleDecl, STREAM_SEALED, ScenarioDecl,
};
use crate::runner::{CallSpec, Ctx, Exec};
use pbfhogg::TypeFilter;
use pbfhogg::cat::CleanAttrs;
use pbfhogg::write::WriteOptions;
use serde_json::{Value, json};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

pub(crate) struct FixedResourcesExecutor;

/// The call completed on a caller's pool and rayon's global pool was never
/// built: nothing about where the work ran beyond that.
const GLOBAL_POOL_UNUSED: &[&str] = &["executor.global-pool-unused.v1"];

/// Every payload the writer framed was framed on a worker of the caller's
/// pool.
const CALLER_POOL: &[&str] = &["executor.caller-pool.v1"];

/// The fixed parallelism and the caller pool's size.
const THREADS: usize = 4;

const CLEAN_VERSION: CleanAttrs = CleanAttrs {
    version: true,
    changeset: false,
    timestamp: false,
    uid: false,
    user: false,
};

/// [`super::WRITE`] with `force`: the raw input has no indexdata, which a
/// type filter otherwise refuses.
const WRITE_FORCED: WriteOptions = WriteOptions {
    force: true,
    ..super::WRITE
};

const FRAMED: &str = "writer_payload_framed_items";
const FRAMED_ON_EXECUTOR: &str = "writer_payload_framed_on_executor_items";

const ARGV_A: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input"),
    Arg::Literal("--clean"),
    Arg::Literal("version"),
    Arg::Literal("-o"),
    Arg::Output("out"),
];

const ARGV_B: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("raw"),
    Arg::Literal("--type"),
    Arg::Literal("node,way,relation"),
    Arg::Literal("--force"),
    Arg::Literal("-o"),
    Arg::Output("framed"),
];

const EXECUTOR_DIFFERENCES: &[&str] = &[
    "the CLI runs on PageCachePolicy::Evict; the call runs on Keep",
    "the CLI raises its soft RLIMIT_NOFILE when a command asks; the harness set it at start",
    "the CLI caps malloc arenas when a command asks; the call leaves the allocator alone",
    "the CLI sizes work by probing the machine and runs on rayon's global pool; the call gets fixed parallelism and a pool of its own",
];

impl Scenario for FixedResourcesExecutor {
    fn decl(&self) -> ScenarioDecl {
        ScenarioDecl {
            id: "fixed-resources-executor",
            description: "two cats on fixed Resources and a caller-built Executor::Pool: \
                          the global pool unused, and writer framing on the caller's pool",
            deadline_s: 900,
            hotpath: false,
            isolation: false,
            calls: vec![
                CallDecl {
                    id: "a",
                    label: "cat --clean version input -> out on the caller's pool",
                    cli: Some(Counterpart::output(
                        ARGV_A,
                        Some(false),
                        EXECUTOR_DIFFERENCES,
                    )),
                    stream: STREAM_SEALED,
                },
                CallDecl {
                    id: "b",
                    label: "cat --type node,way,relation --force raw -> framed on the caller's pool",
                    cli: Some(Counterpart::output(
                        ARGV_B,
                        Some(false),
                        EXECUTOR_DIFFERENCES,
                    )),
                    stream: STREAM_SEALED,
                },
            ],
            roles: vec![
                RoleDecl::dataset("input", "indexed"),
                RoleDecl::dataset("raw", "raw"),
                RoleDecl::output("out", "a"),
                RoleDecl::output("framed", "b"),
            ],
            assertions: vec![
                AssertionDecl {
                    id: "a.outcome",
                    call: Some("a"),
                    covers: GLOBAL_POOL_UNUSED,
                },
                AssertionDecl {
                    id: "b.outcome",
                    call: Some("b"),
                    covers: GLOBAL_POOL_UNUSED,
                },
                AssertionDecl {
                    id: "b.framing-on-caller-pool",
                    call: Some("b"),
                    covers: CALLER_POOL,
                },
                AssertionDecl {
                    id: "global-registry-unused",
                    call: None,
                    covers: GLOBAL_POOL_UNUSED,
                },
            ],
        }
    }

    fn plan(&self) -> Value {
        let host = host_json(&json!(THREADS), &json!({ "pool": THREADS }), &json!("none"));
        json!({
            "schedule": { "order": ["a", "b"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "never started",
                       "caller_pool": { "threads": THREADS, "built": "before the first call",
                                        "shared_by": ["a", "b"],
                                        "calls_driven_from": "the harness thread, off the pool" } },
            "process": process_json(self.nofile()),
            "calls": {
                "a": {
                    "command": "cat",
                    "inputs": ["input"],
                    "output": "out",
                    "type_filter": null,
                    "clean": clean_json(true),
                    "write": write_json(),
                    "header_overrides": overrides_json(),
                    "output_policy": output_policy_json("refuse"),
                    "host": host,
                },
                "b": {
                    "command": "cat",
                    "inputs": ["raw"],
                    "output": "framed",
                    "type_filter": { "node": true, "way": true, "relation": true },
                    "clean": clean_json(false),
                    "write": { "compression": "zlib:6", "backend": "buffered",
                               "direct_reads": false, "force": true },
                    "header_overrides": overrides_json(),
                    "output_policy": output_policy_json("refuse"),
                    "host": host,
                },
            },
        })
    }

    fn starts_global_pool(&self) -> bool {
        false
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(THREADS)
                .thread_name(|i| format!("harness-exec-{i}"))
                .build()
                .map_err(|e| format!("building the caller pool: {e}"))?,
        );
        let spec = |id| {
            let mut spec = CallSpec::plain(id);
            spec.parallelism = NonZeroUsize::new(THREADS);
            spec.executor = Exec::Pool(Arc::clone(&pool));
            spec
        };

        let record = ctx.call(spec("a"), |host, env| {
            let input = env.binding("input");
            cat_into(env, &[input], "out", None, &CLEAN_VERSION, host).map(drop)
        })?;
        let judged = super::installed_ok("a.outcome", &record, "out");
        ctx.finish_call(&record, &[judged])?;

        let stats = Mutex::new(None);
        let record = ctx.call(spec("b"), |host, env| {
            let raw = env.binding("raw");
            let out = output(env, "framed", pbfhogg::Overwrite::Refuse, host)?;
            let got = pbfhogg::cat::cat(
                &[raw],
                out,
                Some(TypeFilter::all()),
                &super::CLEAN_NONE,
                &WRITE_FORCED,
                &overrides(),
                host,
            )?;
            if let Ok(mut slot) = stats.lock() {
                *slot = Some((got.blobs_passthrough, got.blobs_decoded));
            }
            Ok(())
        })?;
        let stats = stats.into_inner().ok().flatten();
        let outcome = super::installed_ok("b.outcome", &record, "framed");
        let framing = framing_on_caller_pool(&record, stats);
        ctx.finish_call(&record, &[outcome, framing])?;
        drop(pool);

        // Both calls are sealed and their values dropped: whatever they did
        // is done.
        let global = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build_global();
        Ok(vec![Outcome::judge(
            "global-registry-unused",
            global.is_ok(),
            match global {
                Ok(()) => "rayon's global pool was still unbuilt after both calls".to_owned(),
                Err(e) => format!("rayon's global pool was already initialised: {e}"),
            },
        )])
    }
}

/// `b.framing-on-caller-pool`: no blob passed through, some decoded, some
/// payloads framed, and every framed payload framed on a worker of the
/// caller's pool, read from the sealed call's final counters.
fn framing_on_caller_pool(
    record: &crate::runner::CallRecord,
    stats: Option<(u64, u64)>,
) -> Outcome {
    let framed = record.sealed_counter(FRAMED);
    let on_pool = record.sealed_counter(FRAMED_ON_EXECUTOR);
    let ok = matches!(
        (stats, framed, on_pool),
        (Some((0, decoded)), Some(f), Some(p)) if decoded > 0 && f > 0 && p == f
    );
    Outcome::judge(
        "b.framing-on-caller-pool",
        ok,
        format!(
            "cat (passthrough, decoded) {stats:?}; {FRAMED} {framed:?}; \
             {FRAMED_ON_EXECUTOR} {on_pool:?}"
        ),
    )
}
