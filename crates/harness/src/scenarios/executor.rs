//! `fixed-resources-executor`: two cats under fixed `Resources` and one
//! `Executor::Pool` the runner built, driven from a harness thread (never
//! from a worker of that pool). Rayon's global pool is never started; after
//! both calls `build_global` must still succeed, which proves no work of
//! either call initialised the global registry (any use of the global pool
//! does).
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

use super::{Scenario, assertion, installed_ok};
use crate::protocol::Outcome;
use crate::runner::{CallRecord, Done, Observation};
use crate::workload::{
    CLEAN_NONE, CallPlan, Command, ExecutorPlan, ExecutorPool, HostPlan, InputRole, Inputs,
    OutputPlan, Parallelism, ProcessPlan, Returned, WRITE, Workload, no_overrides,
};
use pbfhogg::TypeFilter;
use pbfhogg::cat::CleanAttrs;
use pbfhogg::host::PageCachePolicy;
use pbfhogg::write::WriteOptions;
use std::num::NonZeroUsize;

pub(crate) struct FixedResourcesExecutor;

/// The call completed on a caller's pool and rayon's global pool was never
/// built: nothing about where the work ran beyond that.
const GLOBAL_POOL_UNUSED: &[&str] = &["executor.global-pool-unused.v1"];

/// Every payload the writer framed was framed on a worker of the caller's
/// pool.
const CALLER_POOL: &[&str] = &["executor.caller-pool.v1"];

/// The fixed parallelism and the caller pool's size.
const THREADS: NonZeroUsize = NonZeroUsize::MIN.saturating_add(3);

const CLEAN_VERSION: CleanAttrs = CleanAttrs {
    version: true,
    ..CLEAN_NONE
};

/// [`WRITE`] with `force`: the raw input has no indexdata, which a type
/// filter otherwise refuses.
const WRITE_FORCED: WriteOptions = WriteOptions {
    force: true,
    ..WRITE
};

const FRAMED: &str = "writer_payload_framed_items";
const FRAMED_ON_EXECUTOR: &str = "writer_payload_framed_on_executor_items";

const HOST: HostPlan = HostPlan {
    parallelism: Parallelism::Fixed(THREADS),
    executor: ExecutorPlan::Pool("caller"),
    process: ProcessPlan::None,
    page_cache: PageCachePolicy::Keep,
};

impl Scenario for FixedResourcesExecutor {
    fn workload(&self) -> Workload {
        let mut w = Workload::new(
            "fixed-resources-executor",
            "two cats on fixed Resources and a caller-built Executor::Pool: \
             the global pool unused, and writer framing on the caller's pool",
            900,
        );
        w.starts_global_pool = false;
        w.inputs = vec![
            InputRole::dataset("input", "indexed"),
            InputRole::dataset("raw", "raw"),
        ];
        w.executor_pools = vec![ExecutorPool {
            id: "caller",
            threads: THREADS.get(),
        }];
        let mut a = CallPlan::plain(
            "a",
            "cat --clean version input -> out on the caller's pool",
            Command::Cat {
                inputs: Inputs::Roles(&["input"]),
                output: OutputPlan::fresh("out"),
                type_filter: None,
                clean: CLEAN_VERSION,
                write: WRITE,
                overrides: no_overrides(),
            },
        );
        a.host = HOST;
        let mut b = CallPlan::plain(
            "b",
            "cat --type node,way,relation --force raw -> framed on the caller's pool",
            Command::Cat {
                inputs: Inputs::Roles(&["raw"]),
                output: OutputPlan::fresh("framed"),
                type_filter: Some(TypeFilter::all()),
                clean: CLEAN_NONE,
                write: WRITE_FORCED,
                overrides: no_overrides(),
            },
        );
        b.host = HOST;
        w.calls = vec![a, b];
        w.assertions = vec![
            assertion("a.outcome", Some("a"), GLOBAL_POOL_UNUSED),
            assertion("b.outcome", Some("b"), GLOBAL_POOL_UNUSED),
            assertion("b.framing-on-caller-pool", Some("b"), CALLER_POOL),
            assertion("global-registry-unused", None, GLOBAL_POOL_UNUSED),
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
            vec![installed_ok("a.outcome", record, "out")]
        } else {
            vec![
                installed_ok("b.outcome", record, "framed"),
                framing_on_caller_pool(record),
            ]
        }
    }

    /// Both calls are closed and the runner dropped their pool: whatever
    /// they did is done.
    fn judge_scenario(&self, _done: &Done<'_>) -> Vec<Outcome> {
        let global = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build_global();
        vec![Outcome::judge(
            "global-registry-unused",
            global.is_ok(),
            match global {
                Ok(()) => "rayon's global pool was still unbuilt after both calls".to_owned(),
                Err(e) => format!("rayon's global pool was already initialised: {e}"),
            },
        )]
    }
}

/// `b.framing-on-caller-pool`: no blob passed through, some decoded, some
/// payloads framed, and every framed payload framed on a worker of the
/// caller's pool, read from the sealed call's final counters.
fn framing_on_caller_pool(record: &CallRecord) -> Outcome {
    let stats = match record.returned {
        Some(Returned::Cat {
            passthrough,
            decoded,
        }) => Some((passthrough, decoded)),
        _ => None,
    };
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
