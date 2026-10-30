//! The scenarios the harness declares, one file per family, and what their
//! calls share: the fully spelled cat options, the plan's host and output
//! policy fragments, and the judgments of a call's publications.

mod cancel;
mod concurrent_cat;
mod error_path;
mod executor;
mod isolation_cat;
mod nofile;
mod page_cache;
mod repeat_cat;
mod seeded_replace;

use crate::process::Nofile;
use crate::protocol::{AssertionDecl, Outcome, ScenarioDecl};
use crate::runner::{CallRecord, Ctx};
use pbfhogg::cat::CleanAttrs;
use pbfhogg::write::{OutputBackend, WriteOptions};
use pbfhogg::writer::Compression;
use pbfhogg::{Decision, Effect, FieldOverride, HeaderOverrides};
use serde_json::{Value, json};

/// The scenario-level assertion every scenario carries: no call diagnostic
/// arrived after its call document was written.
pub(crate) const DIAGNOSTICS_CLOSED: &str = "call-diagnostics-closed";

/// One scenario: its declaration, its plan, and its run.
pub(crate) trait Scenario {
    /// Its describe entry, without the assertions every scenario carries.
    fn decl(&self) -> ScenarioDecl;
    /// Every workload choice, resolved: nothing run-specific.
    fn plan(&self) -> Value;
    /// Its soft `RLIMIT_NOFILE` policy, applied before any library work.
    fn nofile(&self) -> Nofile {
        Nofile::RaiseToHard
    }
    /// Whether setup starts rayon's global pool before the first call.
    fn starts_global_pool(&self) -> bool {
        true
    }
    /// Write every generated role, before any call.
    fn setup(&self, _ctx: &Ctx<'_>) -> Result<(), String> {
        Ok(())
    }
    /// Run the calls, writing each call document through `ctx`, and return
    /// the scenario's own results (all but [`DIAGNOSTICS_CLOSED`]). An
    /// `Err` refuses the run.
    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String>;
}

/// Every scenario, in describe order.
pub(crate) fn all() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(repeat_cat::RepeatCat),
        Box::new(concurrent_cat::ConcurrentCat),
        Box::new(executor::FixedResourcesExecutor),
        Box::new(cancel::CancelCat::PRE_DECISION),
        Box::new(cancel::CancelCat::POST_DECISION),
        Box::new(nofile::NofileCatRefused),
        Box::new(nofile::NofileAltwExternal),
        Box::new(error_path::ErrorPath),
        Box::new(seeded_replace::SeededReplace),
        Box::new(page_cache::PageCacheKeep),
        Box::new(isolation_cat::IsolationCat),
    ]
}

/// The scenario named `id`.
pub(crate) fn find(id: &str) -> Option<Box<dyn Scenario>> {
    all().into_iter().find(|s| s.decl().id == id)
}

/// A scenario's full declaration, the shared assertions included.
pub(crate) fn declared(scenario: &dyn Scenario) -> ScenarioDecl {
    let mut decl = scenario.decl();
    decl.assertions.push(AssertionDecl {
        id: DIAGNOSTICS_CLOSED,
        call: None,
        covers: &[],
    });
    decl
}

/// How the CLI's process policy differs from a plain call's.
pub(crate) const POLICY_DIFFERENCES: &[&str] = &[
    "the CLI runs on PageCachePolicy::Evict; the call runs on Keep",
    "the CLI raises its soft RLIMIT_NOFILE when a command asks; the harness set it at start",
    "the CLI caps malloc arenas when a command asks; the call leaves the allocator alone",
    "the CLI sizes work by probing the machine; the call gets fixed parallelism and no memory figure",
];

/// The write options every call uses, every field spelled out so a changed
/// library default cannot change the workload unseen.
pub(crate) const WRITE: WriteOptions = WriteOptions {
    compression: Compression::Zlib(6),
    backend: OutputBackend::Buffered,
    direct_reads: false,
    force: false,
};

/// No attribute cleaned.
pub(crate) const CLEAN_NONE: CleanAttrs = CleanAttrs {
    version: false,
    changeset: false,
    timestamp: false,
    uid: false,
    user: false,
};

pub(crate) fn overrides() -> HeaderOverrides {
    HeaderOverrides {
        generator: None,
        replication_timestamp: FieldOverride::Unchanged,
        replication_sequence_number: FieldOverride::Unchanged,
        replication_base_url: FieldOverride::Unchanged,
    }
}

/// The plan's spelling of [`WRITE`].
pub(crate) fn write_json() -> Value {
    json!({ "compression": "zlib:6", "backend": "buffered", "direct_reads": false, "force": false })
}

/// The plan's spelling of [`overrides`].
pub(crate) fn overrides_json() -> Value {
    json!({ "generator": null, "replication_timestamp": "unchanged",
            "replication_sequence_number": "unchanged", "replication_base_url": "unchanged" })
}

/// The plan's spelling of a clean set, `version` alone or nothing.
pub(crate) fn clean_json(version: bool) -> Value {
    json!({ "version": version, "changeset": false, "timestamp": false, "uid": false, "user": false })
}

/// The plan's host for a call: `parallelism` is `"machine"` or a fixed
/// count, `executor` and `process_actions` name what the call gets.
pub(crate) fn host_json(parallelism: &Value, executor: &Value, process_actions: &Value) -> Value {
    host_json_paging(parallelism, executor, process_actions, "keep")
}

/// [`host_json`] with the call's page-cache policy, `"keep"` or `"evict"`.
pub(crate) fn host_json_paging(
    parallelism: &Value,
    executor: &Value,
    process_actions: &Value,
    page_cache: &str,
) -> Value {
    json!({ "resources": "fixed", "parallelism": parallelism, "memory_available": null,
            "process_actions": process_actions, "page_cache": page_cache, "executor": executor })
}

/// The plan's output policy.
pub(crate) fn output_policy_json(overwrite: &str) -> Value {
    json!({ "overwrite": overwrite, "staging": "dir_with_backups_private" })
}

/// The plan of one plain cat call (no type filter): every option spelled.
pub(crate) fn cat_plan(inputs: &Value, output: &str, clean: bool) -> Value {
    json!({
        "command": "cat",
        "inputs": inputs,
        "output": output,
        "type_filter": null,
        "clean": clean_json(clean),
        "write": write_json(),
        "header_overrides": overrides_json(),
        "output_policy": output_policy_json("refuse"),
        "host": host_json(&json!("machine"), &json!("ambient"), &json!("none")),
    })
}

/// The plan's `process` block.
pub(crate) fn process_json(nofile: Nofile) -> Value {
    json!({ "rlimit_nofile_soft": nofile.to_json(), "sigpipe": "inherited" })
}

/// The call published exactly once, `decision`, its one member `role` with
/// `effect`.
pub(crate) fn published_once(
    record: &CallRecord,
    decision: Decision,
    role: &str,
    effect: Effect,
) -> bool {
    matches!(
        record.publications.as_slice(),
        [p] if p.ordinal == 1 && p.decision == decision && p.members == [(role, effect)]
    )
}

/// A one-line account of a call for an assertion's detail.
pub(crate) fn account(record: &CallRecord) -> String {
    let rows: Vec<String> = record
        .publications
        .iter()
        .map(|p| format!("{} {:?} {:?}", p.ordinal, p.decision, p.members))
        .collect();
    format!(
        "outcome {}, publications [{}], cancel {:?}, sealed {}",
        record.outcome.to_json(),
        rows.join("; "),
        record.cancel,
        record.sealed()
    )
}

/// `<call>.outcome` for a call that must return ok, cancel nothing and
/// install its one output.
pub(crate) fn installed_ok(id: &'static str, record: &CallRecord, role: &str) -> Outcome {
    let ok = record.outcome.is_ok()
        && record.cancel.is_none()
        && published_once(record, Decision::Authorized, role, Effect::Installed);
    Outcome::judge(id, ok, account(record))
}

/// Whether `path` is absent.
pub(crate) fn absent(path: &std::path::Path) -> bool {
    path.symlink_metadata().is_err()
}

/// The output bound to `role`, staged in the private directory, built
/// inside the call.
pub(crate) fn output(
    env: &crate::runner::CallEnv<'_>,
    role: &str,
    overwrite: pbfhogg::Overwrite,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<pbfhogg::Output> {
    pbfhogg::Output::with_staging(
        env.binding(role),
        overwrite,
        pbfhogg::Staging::DirWithBackups(env.private_dir()),
        &host.diagnostics,
    )
}

/// A cat of `inputs` into `role`, refusing an existing destination.
pub(crate) fn cat_into(
    env: &crate::runner::CallEnv<'_>,
    inputs: &[&std::path::Path],
    role: &str,
    type_filter: Option<pbfhogg::TypeFilter>,
    clean: &CleanAttrs,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<pbfhogg::cat::CatStats> {
    let out = output(env, role, pbfhogg::Overwrite::Refuse, host)?;
    pbfhogg::cat::cat(inputs, out, type_filter, clean, &WRITE, &overrides(), host)
}
