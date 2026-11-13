//! One launch in `--test-harness` mode: read the request, establish the
//! workload's process policy, run setup, build the resources the workload
//! declares, drive its schedule (one call after another, or each on a
//! harness thread of its own where it runs them at once), drop the
//! resources, then judge each call and the scenario and write the artefacts
//! (`reference/library-protocol.md`, The run protocol and The artefacts). Every
//! workload choice comes from the scenario's [`Workload`]; the scenario
//! supplies only observation and judgments ([`Observation`]).
//!
//! Each call, in this order, closed before its successor starts:
//!
//! 1. A fresh call sink on the one marker FIFO, tagged with the call id; a
//!    fresh invocation that owns it (`Invocation::with_call_sink`); a fresh
//!    host whose diagnostics go to the call's own collector. None of this
//!    emits anything.
//! 2. `entry_us`, the call (constructing its outputs and readers inside
//!    it), `return_us`.
//! 3. The end-of-run emitters, each once: writer counters, read counters,
//!    writer degradations.
//! 4. The seal attempt (`Invocation::seal`), then `seal_us`. A seal ends
//!    the call stream (its sealing emissions and terminal record) before it
//!    returns; the census of an outstanding attempt is kept as taken then.
//! 5. The call's tap, if it has one, released (dropping the invocation it
//!    may hold), then every value of the call dropped. The library's work does not outlive
//!    the call that started it (its pools are joined, its writers wait for
//!    their framing). Exclusive collector ownership checks that no call
//!    diagnostic sender survives this cleanup; an escaped sender fails the
//!    scenario even if it has not emitted yet.
//! 6. The call's collection closed, its publications and its call sink's
//!    account (`Invocation::stream_account`) read.
//!
//! Once the schedule is done, each call's judgments, then its call
//! document, in declared order.
//!
//! `scenario.json` is written last, with setup's and the harness's own
//! diagnostics and the scenario-level results.

use crate::diag::Collector;
use crate::outcome::CallOutcome;
use crate::protocol::{Outcome, PROTOCOL, ScenarioDecl, Status};
use crate::workload::{
    CallPlan, ExecutorPlan, Parallelism, ProcessPlan, Returned, Schedule, Workload,
};
use pbfhogg::debug::{CallId, InstrumentSink, MarkerFifo, StreamAccount, StreamState};
use pbfhogg::host::{
    Diagnostics, Executor, FdPool, Fixed, Host, Invocation, NoProcessActions, ProcessActions,
    SealOutcome,
};
use pbfhogg::process::FdReservation;
use pbfhogg::{Decision, Effect};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The harness's exit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exit {
    /// Ran every declared call and assertion exactly once, and all passed.
    Passed,
    /// Ran, and some result it reported did not pass.
    Failed,
    /// Could not run the request.
    Refused,
}

impl Exit {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::Passed => 0,
            Self::Failed => 1,
            Self::Refused => 2,
        }
    }
}

/// Where one launch reads and writes.
pub(crate) struct Launch<'a> {
    pub(crate) request: &'a Path,
    pub(crate) private_dir: &'a Path,
    pub(crate) artefacts: Option<PathBuf>,
    pub(crate) fifo: Option<PathBuf>,
    pub(crate) profile: bool,
}

/// `--test-harness`, with the marker FIFO and the profiling request read
/// from the environment the launcher (a script bench or the library
/// driver) sets.
pub(crate) fn run_from_env(request: &Path, private_dir: &Path, artefacts: &Path) -> Exit {
    run(&Launch {
        request,
        private_dir,
        artefacts: Some(artefacts.to_path_buf()),
        fifo: std::env::var_os("BROKKR_MARKER_FIFO").map(PathBuf::from),
        // Profile only when brokkr asks, whatever the build.
        profile: std::env::var_os("HOTPATH_OUTPUT_PATH").is_some(),
    })
}

/// Run one launch; a refusal writes `refusal.json` when it can.
pub(crate) fn run(launch: &Launch<'_>) -> Exit {
    match run_launch(launch) {
        Ok(exit) => exit,
        Err(reason) => {
            if let Some(artefacts) = &launch.artefacts {
                let reason = crate::protocol::one_line(&reason, crate::protocol::REASON_MAX);
                let reason = if reason.is_empty() {
                    "refused".to_owned()
                } else {
                    reason
                };
                let doc = json!({ "protocol": PROTOCOL, "reason": reason });
                drop(write_json(&artefacts.join("refusal.json"), &doc));
            }
            Exit::Refused
        }
    }
}

fn run_launch(launch: &Launch<'_>) -> Result<Exit, String> {
    let artefacts = launch.artefacts.as_deref().ok_or("no artefact directory")?;
    let fifo_path = launch
        .fifo
        .as_deref()
        .ok_or("BROKKR_MARKER_FIFO is not set")?;
    let text = std::fs::read_to_string(launch.request)
        .map_err(|e| format!("cannot read the request: {e}"))?;
    let request = crate::request::parse(&text)?;
    let scenario = crate::scenarios::find(&request.scenario)
        .ok_or_else(|| format!("no scenario {:?}", request.scenario))?;
    let workload = scenario.workload();
    workload.validate()?;
    let plan = workload.plan()?;
    let decl = crate::scenarios::declared(&workload);
    decl.validate()?;
    check_bindings(&decl, &request.bindings)?;
    let fifo =
        MarkerFifo::open(fifo_path).map_err(|e| format!("cannot open the marker FIFO: {e}"))?;
    // The plan's process policy, before any library work.
    workload.nofile.apply()?;
    // Held to the end of this function: the profile covers every call and
    // its cleanup, and is written when the guard drops, after scenario.json.
    let _guard = launch.profile.then(|| {
        hotpath::HotpathGuardBuilder::new("pbfhogg_harness::run")
            .percentiles(&[50.0, 95.0, 99.0])
            .functions_limit(0)
            .build()
    });
    let calls_dir = artefacts.join("calls");
    std::fs::create_dir(&calls_dir).map_err(|e| format!("cannot create calls/: {e}"))?;
    let mut ctx = Ctx {
        decl: &decl,
        bindings: &request.bindings,
        setup: Arc::new(Collector::new(fifo.clone())),
        fifo,
        private_dir: launch.private_dir,
        calls_dir,
        settings: Settings::establish(workload.starts_global_pool),
        all_passed: true,
        late: Vec::new(),
        inventory: Mutex::new(Inventory::default()),
    };
    // Setup: every generated role, before any call.
    for input in &workload.inputs {
        if let crate::workload::Source::Generated(recipe) = &input.source {
            recipe
                .write(ctx.binding(input.id))
                .map_err(|e| format!("writing {}: {e}", input.id))?;
        }
    }
    let observation = scenario.observe();
    let records = ctx.run_schedule(&workload, observation.as_ref())?;
    let done = Done {
        records: &records,
        bindings: &request.bindings,
    };
    for record in &records {
        let judged = observation.judge_call(record, &done);
        ctx.finish_call(record, &judged)?;
    }
    let mut results = observation.judge_scenario(&done);
    let late = ctx.late_report();
    results.push(Outcome::judge(
        crate::scenarios::DIAGNOSTICS_CLOSED,
        late.is_empty(),
        if late.is_empty() {
            "every call diagnostic sender was released at collection close; no late or over-budget event observed"
                .to_owned()
        } else {
            late
        },
    ));
    ctx.inventory().complete(&decl, &results)?;
    let passed = ctx.all_passed && results.iter().all(|r| r.status == Status::Pass);
    let diagnostics: Vec<Value> = ctx
        .setup
        .close()
        .iter()
        .map(crate::diag::Captured::to_json)
        .collect();
    let doc = json!({
        "protocol": PROTOCOL,
        "scenario": decl.id,
        "plan": plan,
        "assertions": results.iter().map(Outcome::to_json).collect::<Vec<_>>(),
        "diagnostics": diagnostics,
    });
    write_json(&artefacts.join("scenario.json"), &doc)?;
    Ok(if passed { Exit::Passed } else { Exit::Failed })
}

/// Every role bound and nothing else; inputs existing files; outputs and
/// generated roles under resolvable directories, absent (a seeded output
/// holding exactly its seed), and naming neither an input nor each other
/// once the directories are resolved.
fn check_bindings(decl: &ScenarioDecl, bindings: &BTreeMap<String, PathBuf>) -> Result<(), String> {
    use crate::protocol::Role;
    let declared: Vec<&str> = decl.roles.iter().map(|r| r.id).collect();
    let bound: Vec<&str> = bindings.keys().map(String::as_str).collect();
    let mut sorted = declared.clone();
    sorted.sort_unstable();
    if sorted != bound {
        return Err(format!(
            "bindings name {bound:?}, the scenario's roles are {sorted:?}"
        ));
    }
    let mut seen: Vec<(PathBuf, bool, &str)> = Vec::new();
    for role in &decl.roles {
        let path = &bindings[role.id];
        let resolved = match role.role {
            Role::Dataset { .. } => {
                let real = path
                    .canonicalize()
                    .map_err(|e| format!("input {}: {e}", role.id))?;
                if !real.is_file() {
                    return Err(format!("input {} is not a regular file", role.id));
                }
                real
            }
            Role::Generated { .. } | Role::OutputPbf { .. } => {
                let seeded = matches!(role.role, Role::OutputPbf { seeded: true, .. });
                let initial_ok = if seeded {
                    path.symlink_metadata().is_ok_and(|m| m.is_file())
                        && crate::scenarios::file_assertions::holds_seed(path, role.id)
                            .map_err(|e| format!("inspect seed {}: {e}", role.id))?
                } else {
                    crate::scenarios::absent(path)
                        .map_err(|e| format!("inspect role {}: {e}", role.id))?
                };
                if !initial_ok {
                    return Err(format!("role {} is not in its initial state", role.id));
                }
                let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                    return Err(format!("role {} has no directory", role.id));
                };
                let parent = parent
                    .canonicalize()
                    .map_err(|e| format!("role {}: {e}", role.id))?;
                parent.join(name)
            }
        };
        // Inputs may share a file; a written role shares its path with
        // nothing.
        let is_written = !matches!(role.role, Role::Dataset { .. });
        let clash = seen
            .iter()
            .find(|(p, other_written, _)| *p == resolved && (is_written || *other_written));
        if let Some((_, _, other)) = clash {
            return Err(format!("roles {other} and {} name one path", role.id));
        }
        seen.push((resolved, is_written, role.id));
    }
    Ok(())
}

/// The machine settings this launch read once.
struct Settings {
    parallelism: Option<NonZeroUsize>,
    /// The ambient rayon pool's size, when the scenario starts it: reading
    /// it starts rayon's global pool here, during setup, so no call pays
    /// for starting it. `None` for a scenario that leaves it unstarted.
    ambient_threads: Option<usize>,
}

impl Settings {
    fn establish(start_global_pool: bool) -> Self {
        Self {
            parallelism: std::thread::available_parallelism().ok(),
            ambient_threads: start_global_pool.then(rayon::current_num_threads),
        }
    }
}

/// What a call body sees besides its host.
pub(crate) struct CallEnv<'a> {
    bindings: &'a BTreeMap<String, PathBuf>,
    private_dir: &'a Path,
}

impl CallEnv<'_> {
    /// The path bound to `role`, which the bindings check guaranteed.
    pub(crate) fn binding(&self, role: &str) -> &Path {
        self.bindings
            .get(role)
            .map_or(Path::new(""), PathBuf::as_path)
    }

    /// The launch's private directory, for staging and scratch.
    pub(crate) fn private_dir(&self) -> &Path {
        self.private_dir
    }
}

/// One decided publication, its members mapped to roles.
pub(crate) struct Published {
    pub(crate) ordinal: u64,
    pub(crate) decision: Decision,
    pub(crate) members: Vec<(&'static str, Effect)>,
}

/// A tap on a call's stream: an [`InstrumentSink`] the runner releases
/// once the call's seal was attempted, before the call's host is dropped
/// and its diagnostics closed. Releasing drops whatever the tap holds of
/// the call (its invocation, which would otherwise keep itself alive
/// through its own tap); what it observed stays readable.
pub(crate) trait Tap: InstrumentSink {
    /// Called with the call's invocation before the command runs.
    fn armed(&self, _invocation: &Invocation) {}
    fn release(&self);
    /// What a cancel the tap issued returned, for the call document.
    fn cancel(&self) -> Option<pbfhogg::CancelOutcome> {
        None
    }
}

/// What a descriptor observer may read of the planned pool: its budget and
/// its live free count. It can neither grant nor release.
pub(crate) struct FdView<'a> {
    pool: &'a FdPool,
    budget: u64,
}

impl FdView<'_> {
    pub(crate) fn available(&self) -> u64 {
        self.pool.available()
    }

    pub(crate) fn budget(&self) -> u64 {
        self.budget
    }
}

/// Watches one call's descriptor reservations from the planned pool. The
/// runner's wrapper takes the grant, calls `granted` while holding it (an
/// observer may block there, to rendezvous with another call), and at
/// release gives the descriptors back before calling `released`.
pub(crate) trait FdObserver: Send + Sync {
    fn granted(&self, wanted: u64, granted: u64, pool: &FdView<'_>);
    fn released(&self, pool: &FdView<'_>);
}

/// The scenario's handwritten part of a run: the observation of each call
/// and the judgments. It chooses nothing about the workload.
pub(crate) trait Observation {
    fn tap(&self, _call: &str) -> Option<Arc<dyn Tap>> {
        None
    }
    fn fd_observer(&self, _call: &str) -> Option<Arc<dyn FdObserver>> {
        None
    }
    /// The call's own results, once every call of the schedule is closed.
    fn judge_call(&self, record: &CallRecord, done: &Done<'_>) -> Vec<Outcome>;
    /// The scenario's own results (all but the shared diagnostics one).
    fn judge_scenario(&self, _done: &Done<'_>) -> Vec<Outcome> {
        Vec::new()
    }
}

/// The finished schedule, for judgments.
pub(crate) struct Done<'a> {
    records: &'a [CallRecord],
    bindings: &'a BTreeMap<String, PathBuf>,
}

impl Done<'_> {
    pub(crate) fn record(&self, id: &str) -> Option<&CallRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    /// The path bound to `role`.
    pub(crate) fn binding(&self, role: &str) -> &Path {
        self.bindings
            .get(role)
            .map_or(Path::new(""), PathBuf::as_path)
    }
}

/// One call's observation hooks: its tap and its descriptor observer.
type Hooks = (Option<Arc<dyn Tap>>, Option<Arc<dyn FdObserver>>);

/// A planned pool wrapped so its observer sees each grant.
struct Observed {
    pool: Arc<FdPool>,
    budget: u64,
    observer: Arc<dyn FdObserver>,
}

impl ProcessActions for Observed {
    fn fd_allowance(&self, wanted: u64) -> FdReservation {
        let inner = self.pool.fd_allowance(wanted);
        let allowance = inner.allowance();
        self.observer.granted(
            wanted,
            allowance.descriptors,
            &FdView {
                pool: &self.pool,
                budget: self.budget,
            },
        );
        let observer = Arc::clone(&self.observer);
        let pool = Arc::clone(&self.pool);
        let budget = self.budget;
        FdReservation::with_release(allowance, move || {
            drop(inner);
            observer.released(&FdView {
                pool: &pool,
                budget,
            });
        })
    }
}

/// The resources the workload declares, built by the runner before the
/// first call, each dropped once its last user has closed.
struct Resources {
    executors: Vec<(&'static str, Arc<rayon::ThreadPool>)>,
    fd_pools: Vec<(&'static str, Arc<FdPool>, u64)>,
}

impl Resources {
    fn build(workload: &Workload) -> Result<Self, String> {
        let mut executors = Vec::new();
        for pool in &workload.executor_pools {
            let id = pool.id;
            let built = rayon::ThreadPoolBuilder::new()
                .num_threads(pool.threads)
                .thread_name(move |i| format!("harness-{id}-{i}"))
                .build()
                .map_err(|e| format!("building executor pool {id}: {e}"))?;
            if built.current_num_threads() != pool.threads {
                return Err(format!("executor pool {id} has the wrong size"));
            }
            executors.push((id, Arc::new(built)));
        }
        let fd_pools = workload
            .fd_pools
            .iter()
            .map(|p| (p.id, Arc::new(FdPool::new(p.budget)), p.budget))
            .collect();
        Ok(Self {
            executors,
            fd_pools,
        })
    }

    /// Drop every resource whose last user, in schedule order, is call
    /// `index`, now that it has closed.
    fn release_after(&mut self, workload: &Workload, index: usize) {
        self.executors.retain(|(id, _)| {
            workload.last_user(|h| matches!(h.executor, ExecutorPlan::Pool(p) if p == *id))
                != Some(index)
        });
        self.fd_pools.retain(|(id, _, _)| {
            workload.last_user(|h| matches!(h.process, ProcessPlan::FdPool(p) if p == *id))
                != Some(index)
        });
    }

    fn executor(&self, id: &str) -> Result<&Arc<rayon::ThreadPool>, String> {
        self.executors
            .iter()
            .find(|(name, _)| *name == id)
            .map(|(_, pool)| pool)
            .ok_or_else(|| format!("no executor pool {id}"))
    }

    fn fd_pool(&self, id: &str) -> Result<(&Arc<FdPool>, u64), String> {
        self.fd_pools
            .iter()
            .find(|(name, _, _)| *name == id)
            .map(|(_, pool, budget)| (pool, *budget))
            .ok_or_else(|| format!("no descriptor pool {id}"))
    }
}

/// Releases a tap when dropped, so an early return cannot leave the
/// invocation holding itself.
struct Released(Option<Arc<dyn Tap>>);

impl Drop for Released {
    fn drop(&mut self) {
        if let Some(tap) = self.0.take() {
            tap.release();
        }
    }
}

/// Everything a call document reports, before its assertions.
pub(crate) struct CallRecord {
    pub(crate) id: &'static str,
    pub(crate) outcome: CallOutcome,
    pub(crate) publications: Vec<Published>,
    pub(crate) cancel: Option<pbfhogg::CancelOutcome>,
    /// What the command returned, when it returned ok.
    pub(crate) returned: Option<Returned>,
    pub(crate) entry_us: u64,
    pub(crate) return_us: u64,
    seal_us: u64,
    seal: SealOutcome,
    stream: StreamAccount,
    context: Value,
    diagnostics: Vec<Value>,
    collector: Arc<Collector>,
    diagnostics_released: bool,
}

impl CallRecord {
    pub(crate) fn sealed(&self) -> bool {
        matches!(self.seal, SealOutcome::Sealed(_))
    }

    /// Counter `name` in the sealed call's final snapshot; `None` when the
    /// call did not seal or never recorded it.
    pub(crate) fn sealed_counter(&self, name: &str) -> Option<u64> {
        match &self.seal {
            SealOutcome::Sealed(snapshot) => snapshot
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, value)| value),
            _ => None,
        }
    }
}

/// The run of one scenario: what its calls share.
pub(crate) struct Ctx<'a> {
    decl: &'a ScenarioDecl,
    bindings: &'a BTreeMap<String, PathBuf>,
    fifo: MarkerFifo,
    private_dir: &'a Path,
    calls_dir: PathBuf,
    /// Setup's and the harness's own diagnostics.
    setup: Arc<Collector>,
    settings: Settings,
    all_passed: bool,
    /// Each call's collector, kept to detect a late event.
    late: Vec<(&'static str, Arc<Collector>, bool)>,
    inventory: Mutex<Inventory>,
}

impl Ctx<'_> {
    fn inventory(&self) -> std::sync::MutexGuard<'_, Inventory> {
        self.inventory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The path bound to `role`.
    pub(crate) fn binding(&self, role: &str) -> &Path {
        self.bindings
            .get(role)
            .map_or(Path::new(""), PathBuf::as_path)
    }

    /// Run the workload's schedule from harness threads, with the resources
    /// it declares, and return every closed call's record in declared
    /// order. Each resource is dropped once its last user has closed: in a
    /// sequential schedule after that call, in a concurrent one after every
    /// call has joined.
    fn run_schedule(
        &self,
        workload: &Workload,
        observation: &dyn Observation,
    ) -> Result<Vec<CallRecord>, String> {
        let mut resources = Resources::build(workload)?;
        let hooks: Vec<Hooks> = workload
            .calls
            .iter()
            .map(|c| (observation.tap(c.id), observation.fd_observer(c.id)))
            .collect();
        let records = match workload.schedule {
            Schedule::Sequential => {
                let mut records = Vec::new();
                for (index, (plan, (tap, observer))) in workload.calls.iter().zip(hooks).enumerate()
                {
                    records.push(self.call(plan, &resources, tap, observer)?);
                    resources.release_after(workload, index);
                }
                records
            }
            Schedule::Concurrent => {
                let resources = &resources;
                let joined: Vec<Result<CallRecord, String>> = std::thread::scope(|scope| {
                    let handles: Vec<_> = workload
                        .calls
                        .iter()
                        .zip(hooks)
                        .map(|(plan, (tap, observer))| {
                            scope.spawn(move || self.call(plan, resources, tap, observer))
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .unwrap_or_else(|_| Err("a call's thread panicked".to_owned()))
                        })
                        .collect()
                });
                joined.into_iter().collect::<Result<Vec<_>, _>>()?
            }
        };
        drop(resources);
        Ok(records)
    }

    /// Run one call (steps 1 to 6 of the module's order): its host from
    /// its plan and the declared resources, its command from its plan.
    /// Takes `&self`, so concurrent calls run on threads of their own.
    fn call(
        &self,
        plan: &CallPlan,
        resources: &Resources,
        tap: Option<Arc<dyn Tap>>,
        observer: Option<Arc<dyn FdObserver>>,
    ) -> Result<CallRecord, String> {
        let id = plan.id;
        self.inventory().start(self.decl, id)?;
        if plan.command.names_missing()
            && !crate::scenarios::absent(&self.private_dir.join(crate::workload::MISSING))
                .map_err(|e| format!("inspect the missing input: {e}"))?
        {
            return Err(format!(
                "call {id}: its missing input {} exists",
                crate::workload::MISSING
            ));
        }
        let context = self.context(plan, resources)?;
        let process: Arc<dyn ProcessActions> = match plan.host.process {
            ProcessPlan::None => Arc::new(NoProcessActions),
            ProcessPlan::FdPool(pool_id) => {
                let (pool, budget) = resources.fd_pool(pool_id)?;
                match observer {
                    Some(observer) => Arc::new(Observed {
                        pool: Arc::clone(pool),
                        budget,
                        observer,
                    }),
                    None => Arc::clone(pool) as Arc<dyn ProcessActions>,
                }
            }
        };
        let executor = match plan.host.executor {
            ExecutorPlan::Ambient => Executor::Ambient,
            ExecutorPlan::Pool(pool_id) => Executor::Pool(Arc::clone(resources.executor(pool_id)?)),
        };
        let parallelism = match plan.host.parallelism {
            Parallelism::Machine => self.settings.parallelism,
            Parallelism::Fixed(n) => Some(n),
        };
        let sink = self
            .fifo
            .call(CallId::new(id).map_err(|e| e.report())?)
            .map_err(|e| e.report())?;
        let sink_tap = tap.clone().map(|tap| tap as Arc<dyn InstrumentSink>);
        let invocation = Invocation::with_call_sink(sink, sink_tap);
        if let Some(tap) = &tap {
            tap.armed(&invocation);
        }
        let released = Released(tap.clone());
        let mut collector = Arc::new(Collector::new(self.fifo.clone()));
        let host = Host {
            diagnostics: Diagnostics::new(
                Arc::clone(&collector) as Arc<dyn pbfhogg::host::DiagnosticSink>
            ),
            process,
            resources: Arc::new(Fixed {
                parallelism,
                memory_available: None,
            }),
            page_cache: plan.host.page_cache,
            executor,
            invocation: invocation.clone(),
        };
        let env = CallEnv {
            bindings: self.bindings,
            private_dir: self.private_dir,
        };
        let entry_us = self.fifo.now_us();
        let result = plan.command.execute(&env, &host);
        let return_us = self.fifo.now_us();
        pbfhogg::write::emit_writer_counters(&invocation);
        pbfhogg::read::emit_read_counters(&invocation);
        pbfhogg::write::report_writer_degradations(&host);
        // The seal attempt, and its census kept as taken now, although the
        // call's values are dropped before the document is written.
        let seal = invocation.seal();
        let seal_us = self.fifo.now_us();
        drop(released);
        drop(host);
        // The body must join its work before returning. Exclusive ownership
        // proves that no diagnostic sender (including a weak reference) escaped
        // that scope. A counter sample alone cannot prove future silence.
        let (diagnostics, diagnostics_released) = close_call_diagnostics(&mut collector);
        let publications = self.publications(id, &invocation)?;
        let stream = invocation
            .stream_account()
            .ok_or("the call's invocation has no call sink")?;
        let cancel = tap.as_ref().and_then(|tap| tap.cancel());
        drop(tap);
        Ok(CallRecord {
            id,
            outcome: CallOutcome::of(&result),
            publications,
            cancel,
            returned: result.ok(),
            entry_us,
            return_us,
            seal_us,
            seal,
            stream,
            context,
            diagnostics,
            collector,
            diagnostics_released,
        })
    }

    /// Every effective per-call setting, from the plan the host is built
    /// from and the resources it names.
    fn context(&self, plan: &CallPlan, resources: &Resources) -> Result<Value, String> {
        let mut context = json!({
            "parallelism": match plan.host.parallelism {
                Parallelism::Machine => self.settings.parallelism,
                Parallelism::Fixed(n) => Some(n),
            }.map(NonZeroUsize::get),
            "memory_available": null,
            "page_cache": crate::workload::page_cache_name(plan.host.page_cache),
            "process_actions": "none",
            "rlimit_nofile_soft": crate::process::soft_nofile()?.to_json(),
            "overwrite": crate::workload::overwrite_json(plan.command.overwrite()),
            "staging": "dir_with_backups_private",
        });
        let Some(object) = context.as_object_mut() else {
            return Err("the context is not an object".to_owned());
        };
        match plan.host.executor {
            ExecutorPlan::Ambient => {
                let threads = self
                    .settings
                    .ambient_threads
                    .ok_or("an ambient call in a scenario that leaves the global pool unstarted")?;
                object.insert("executor".to_owned(), json!("ambient"));
                object.insert("executor_threads".to_owned(), json!(threads));
            }
            ExecutorPlan::Pool(pool_id) => {
                let pool = resources.executor(pool_id)?;
                object.insert("executor".to_owned(), json!("pool"));
                object.insert("executor_pool".to_owned(), json!(pool_id));
                object.insert(
                    "executor_threads".to_owned(),
                    json!(pool.current_num_threads()),
                );
            }
        }
        if let ProcessPlan::FdPool(pool_id) = plan.host.process {
            let (_, budget) = resources.fd_pool(pool_id)?;
            object.insert("process_actions".to_owned(), json!("fd_pool"));
            object.insert("fd_pool".to_owned(), json!(pool_id));
            object.insert("fd_pool_budget".to_owned(), json!(budget));
        }
        Ok(context)
    }

    /// The call's publications, each member's destination mapped to the
    /// output role of this call bound there.
    fn publications(&self, id: &str, invocation: &Invocation) -> Result<Vec<Published>, String> {
        let outputs = self.decl.outputs_of(id);
        let mut rows = Vec::new();
        let mut unmapped = None;
        invocation.for_each_publication(|row| {
            let mut members = Vec::new();
            for member in row.members() {
                match outputs
                    .iter()
                    .find(|role| self.binding(role) == member.destination())
                {
                    Some(role) => members.push((*role, member.effect())),
                    None => unmapped = Some(member.destination().to_path_buf()),
                }
            }
            rows.push(Published {
                ordinal: row.ordinal(),
                decision: row.decision(),
                members,
            });
        });
        match unmapped {
            Some(path) => Err(format!(
                "call {id} published {}, which is none of its output roles",
                path.display()
            )),
            None => Ok(rows),
        }
    }

    /// Write the call document with the call's own results.
    pub(crate) fn finish_call(
        &mut self,
        record: &CallRecord,
        results: &[Outcome],
    ) -> Result<(), String> {
        self.inventory().finish(self.decl, record.id, results)?;
        self.all_passed &= results.iter().all(|r| r.status == Status::Pass);
        self.late.push((
            record.id,
            Arc::clone(&record.collector),
            record.diagnostics_released,
        ));
        let publications: Vec<Value> = record
            .publications
            .iter()
            .map(|p| {
                json!({
                    "ordinal": p.ordinal,
                    "decision": decision_name(p.decision),
                    "members": p.members.iter()
                        .map(|(role, effect)| json!({ "role": role, "effect": effect_name(*effect) }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        let doc = json!({
            "protocol": PROTOCOL,
            "call": record.id,
            "entry_us": record.entry_us,
            "return_us": record.return_us,
            "seal_us": record.seal_us,
            "outcome": record.outcome.to_json(),
            "publications": publications,
            "cancel": cancel_json(record.cancel.as_ref())?,
            "seal": seal_json(&record.seal)?,
            "snapshot": snapshot_json(&record.seal),
            "stream": stream_json(record.stream)?,
            "context": record.context,
            "diagnostics": record.diagnostics,
            "assertions": results.iter().map(Outcome::to_json).collect::<Vec<_>>(),
        });
        write_json(&self.calls_dir.join(format!("{}.json", record.id)), &doc)
    }

    /// Ownership at close is the proof; the late count is supplemental.
    /// This covers delivery through the supplied sink, not arbitrary work or
    /// diagnostics a body sends to an unrelated sink. Events a collector
    /// refused for its budget fail the scenario too, for the setup collector
    /// as well as every call's: truncated evidence never certifies.
    fn late_report(&self) -> String {
        let calls = self
            .late
            .iter()
            .filter(|(_, c, released)| !released || c.late() > 0 || c.dropped() > 0)
            .map(|(id, c, released)| {
                format!(
                    "call {id}: diagnostic senders released at close: {released}; {} late diagnostic events; {} events dropped over budget",
                    c.late(),
                    c.dropped(),
                )
            });
        let setup = Some(self.setup.dropped())
            .filter(|&dropped| dropped > 0)
            .map(|dropped| format!("setup: {dropped} events dropped over budget"));
        setup
            .into_iter()
            .chain(calls)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Close after the call has joined its work and released its host and tap.
/// Failure of exclusive ownership is sticky even if a sender disappears before
/// the scenario's final sample. No waiting or guessed grace period is involved.
fn close_call_diagnostics(collector: &mut Arc<Collector>) -> (Vec<Value>, bool) {
    let released = Arc::get_mut(collector).is_some();
    let diagnostics = collector
        .close()
        .iter()
        .map(crate::diag::Captured::to_json)
        .collect();
    (diagnostics, released)
}

/// Record attempts before validating them: a scenario cannot swallow a refusal
/// and turn an undeclared or repeated call or assertion into a passing launch.
#[derive(Default)]
struct Inventory {
    started: Vec<&'static str>,
    finished: Vec<&'static str>,
    assertions: Vec<(Option<&'static str>, &'static str)>,
}

impl Inventory {
    fn start(&mut self, decl: &ScenarioDecl, id: &'static str) -> Result<(), String> {
        self.started.push(id);
        if !decl.calls.iter().any(|call| call.id == id) {
            return Err(format!("undeclared call {id}"));
        }
        if self.started.iter().filter(|&&name| name == id).count() != 1 {
            return Err(format!("repeated call {id}"));
        }
        Ok(())
    }

    fn finish(
        &mut self,
        decl: &ScenarioDecl,
        id: &'static str,
        results: &[Outcome],
    ) -> Result<(), String> {
        self.finished.push(id);
        self.assertions
            .extend(results.iter().map(|r| (Some(id), r.id)));
        if !self.started.contains(&id)
            || self.finished.iter().filter(|&&name| name == id).count() != 1
        {
            return Err(format!("call {id} was not started or was already finished"));
        }
        exact_inventory(
            "call assertions",
            decl.assertions
                .iter()
                .filter(|a| a.call == Some(id))
                .map(|a| a.id),
            results.iter().map(|r| r.id),
        )
    }

    fn complete(&self, decl: &ScenarioDecl, results: &[Outcome]) -> Result<(), String> {
        exact_inventory(
            "started calls",
            decl.calls.iter().map(|c| c.id),
            self.started.iter().copied(),
        )?;
        exact_inventory(
            "finished calls",
            decl.calls.iter().map(|c| c.id),
            self.finished.iter().copied(),
        )?;
        exact_inventory(
            "assertions with their scopes",
            decl.assertions.iter().map(|a| (a.call, a.id)),
            self.assertions
                .iter()
                .copied()
                .chain(results.iter().map(|r| (None, r.id))),
        )
    }
}

/// Compare multisets, retaining duplicates rather than hiding them in a set.
/// Duplicate declarations are also refused, even if execution mirrors them.
fn exact_inventory<T: Ord + std::fmt::Debug>(
    label: &str,
    expected: impl Iterator<Item = T>,
    actual: impl Iterator<Item = T>,
) -> Result<(), String> {
    let mut expected: Vec<_> = expected.collect();
    let mut actual: Vec<_> = actual.collect();
    expected.sort_unstable();
    actual.sort_unstable();
    if expected.windows(2).any(|pair| pair[0] == pair[1]) || expected != actual {
        return Err(format!(
            "{label}: declared {expected:?}, observed {actual:?}"
        ));
    }
    Ok(())
}

fn decision_name(decision: Decision) -> &'static str {
    match decision {
        Decision::Authorized => "authorized",
        Decision::RefusedCancelled => "refused_cancelled",
        Decision::RefusedError => "refused_error",
    }
}

fn effect_name(effect: Effect) -> &'static str {
    match effect {
        Effect::None => "none",
        Effect::Installed => "installed",
        Effect::Restored => "restored",
        Effect::Displaced => "displaced",
    }
}

/// The protocol's spelling of a cancel result. A result added to the
/// non-exhaustive `CancelOutcome` after this list has no protocol spelling
/// yet and refuses the run rather than be misreported.
fn cancel_json(outcome: Option<&pbfhogg::CancelOutcome>) -> Result<Value, String> {
    use pbfhogg::CancelOutcome as C;
    Ok(match outcome {
        None => Value::Null,
        Some(C::BeforeDecision { refused }) => {
            json!({ "result": "before_decision", "ordinal": refused })
        }
        Some(C::AfterDecision { authorized }) => {
            json!({ "result": "after_decision", "ordinal": authorized })
        }
        Some(C::NoPublicationPending) => json!({ "result": "no_publication_pending" }),
        Some(other) => return Err(format!("no protocol spelling for cancel result {other:?}")),
    })
}

/// The protocol's `seal`: sealed, outstanding with the census as the
/// attempt took it, or refused with its reason. A result added to the
/// non-exhaustive `SealOutcome` refuses the run rather than be misreported.
fn seal_json(seal: &SealOutcome) -> Result<Value, String> {
    Ok(match seal {
        SealOutcome::Sealed(_) => json!({ "state": "sealed" }),
        SealOutcome::Outstanding(census) => {
            let mut held = serde_json::Map::new();
            census.for_each_held(|kind, n| {
                held.insert(kind.to_owned(), json!(n));
            });
            if held.is_empty() {
                return Err("an outstanding seal reported an empty census".to_owned());
            }
            json!({ "state": "outstanding", "held": held })
        }
        SealOutcome::Refused(refusal) => json!({ "state": "refused", "reason": refusal.reason() }),
        other => return Err(format!("no protocol spelling for seal result {other:?}")),
    })
}

/// The protocol's `snapshot`: the sealed call's final counters, `null`
/// otherwise.
fn snapshot_json(seal: &SealOutcome) -> Value {
    match seal {
        SealOutcome::Sealed(snapshot) => {
            let counters: serde_json::Map<String, Value> = snapshot
                .iter()
                .map(|(name, value)| (name.to_owned(), json!(value)))
                .collect();
            Value::Object(counters)
        }
        _ => Value::Null,
    }
}

/// The protocol's `stream`: the call sink's own account.
fn stream_json(stream: StreamAccount) -> Result<Value, String> {
    let state = match stream.state {
        StreamState::Open => "open",
        StreamState::Sealed => "sealed",
        StreamState::Failed => "failed",
        other => return Err(format!("no protocol spelling for stream state {other:?}")),
    };
    Ok(json!({ "state": state, "drops": stream.drops }))
}

fn write_json(path: &Path, doc: &Value) -> Result<(), String> {
    let text =
        serde_json::to_string(doc).map_err(|e| format!("serializing {}: {e}", path.display()))?;
    std::fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{Inventory, close_call_diagnostics, exact_inventory};
    use crate::diag::Collector;
    use crate::protocol::{AssertionDecl, Outcome, ScenarioDecl};
    use pbfhogg::host::{DiagnosticSink as _, Event, EventKind};
    use std::sync::Arc;

    fn declaration() -> ScenarioDecl {
        let scenarios = crate::scenarios::all();
        let mut decl = scenarios[0].workload().decl();
        decl.calls.truncate(1);
        decl.assertions = vec![AssertionDecl {
            id: "checked",
            call: Some(decl.calls[0].id),
            covers: &[],
        }];
        decl
    }

    #[test]
    fn inventory_requires_execution_and_completion() {
        let decl = declaration();
        let id = decl.calls[0].id;
        let mut inventory = Inventory::default();
        assert!(inventory.complete(&decl, &[]).is_err());
        inventory.start(&decl, id).expect("start");
        assert!(inventory.complete(&decl, &[]).is_err());
        inventory
            .finish(&decl, id, &[Outcome::judge("checked", true, "ok")])
            .expect("finish");
        inventory.complete(&decl, &[]).expect("complete");
        assert!(inventory.start(&decl, id).is_err());
        assert!(inventory.complete(&decl, &[]).is_err());
    }

    #[test]
    fn inventory_remembers_repeated_and_undeclared_attempts() {
        let decl = declaration();
        let id = decl.calls[0].id;
        let mut inventory = Inventory::default();
        inventory.start(&decl, id).expect("start");
        let results = [Outcome::judge("checked", true, "ok")];
        inventory.finish(&decl, id, &results).expect("finish");
        assert!(inventory.finish(&decl, id, &results).is_err());
        assert!(inventory.complete(&decl, &[]).is_err());
        let mut inventory = Inventory::default();
        assert!(inventory.start(&decl, "undeclared").is_err());
        assert!(inventory.complete(&decl, &[]).is_err());
    }

    #[test]
    fn assertion_inventory_rejects_missing_duplicate_misplaced_and_undeclared() {
        let expected = [(Some("call"), "checked"), (None, "scenario")];
        for actual in [
            vec![(None, "scenario")],
            vec![
                (Some("call"), "checked"),
                (Some("call"), "checked"),
                (None, "scenario"),
            ],
            vec![(None, "checked"), (Some("call"), "scenario")],
            vec![
                (Some("call"), "checked"),
                (None, "scenario"),
                (None, "extra"),
            ],
        ] {
            assert!(
                exact_inventory("assertions", expected.into_iter(), actual.into_iter()).is_err()
            );
        }
        assert!(
            exact_inventory(
                "declarations",
                ["same", "same"].into_iter(),
                ["same", "same"].into_iter()
            )
            .is_err()
        );
    }

    #[test]
    fn diagnostic_close_refuses_a_sender_before_its_delayed_emission() {
        let (_dir, fifo, _reader) = crate::test_scratch::marker_fifo();
        let mut collector = Arc::new(Collector::new(fifo));
        let sender = Arc::clone(&collector);
        let (release, ready) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            ready.recv().expect("release signal");
            sender.event(&Event {
                kind: EventKind::Note,
                key: "delayed",
                args: format_args!("after close"),
            });
        });
        let (diagnostics, released) = close_call_diagnostics(&mut collector);
        assert!(diagnostics.is_empty());
        assert!(!released);
        assert_eq!(collector.late(), 0);
        release.send(()).expect("send release");
        task.join().expect("join sender");
        assert_eq!(collector.late(), 1);
        assert!(!released);
    }

    #[test]
    fn diagnostic_close_accepts_joined_senders_and_refuses_weak_senders() {
        let (_dir, fifo, _reader) = crate::test_scratch::marker_fifo();
        let mut collector = Arc::new(Collector::new(fifo));
        let sender = Arc::clone(&collector);
        std::thread::spawn(move || drop(sender))
            .join()
            .expect("join");
        assert!(close_call_diagnostics(&mut collector).1);
        let weak = Arc::downgrade(&collector);
        assert!(!close_call_diagnostics(&mut collector).1);
        drop(weak);
    }
}
