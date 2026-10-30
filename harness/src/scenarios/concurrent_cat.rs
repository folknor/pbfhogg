//! `concurrent-cat`: two cats at once on two threads, each under its own
//! invocation and host, over generated inputs of different sizes (one
//! block and three), so counters delivered to the wrong call show.
//!
//! Both hosts share one `FdPool` sized for both cats' full requests (one
//! distinct input plus `CAT_EXTRA_FDS` each). Each host's process actions
//! wrap the pool: a request is granted by the pool, then waits at a
//! two-party gate until the other call's request was granted too, and the
//! second to arrive reads the pool's free count while both are held behind
//! the gate. That proves the lifetimes intersected and both grants were
//! held at once, by parking, not by thread start order. The wait is in the
//! caller's `ProcessActions::fd_allowance`, which cat calls on its own
//! thread before pinning, outside any library lock; the taps never block.
//! Each wrapper also tracks its own grant's release, which each call's tap
//! reads at `CAT_SCAN_START`: cat still holds its grant while it scans.

use super::{
    CLEAN_NONE, POLICY_DIFFERENCES, Scenario, account, cat_into, clean_json, host_json,
    output_policy_json, overrides_json, process_json, write_json,
};
use crate::protocol::{
    Arg, AssertionDecl, CallDecl, Counterpart, GeneratedKind, Outcome, Role, RoleDecl, ScenarioDecl,
};
use crate::runner::{CallRecord, CallSpec, Ctx, Process, Tap};
use pbfhogg::debug::InstrumentSink;
use pbfhogg::host::{FdPool, NoProcessActions, ProcessActions};
use pbfhogg::process::FdReservation;
use pbfhogg::{Decision, Effect};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

pub(crate) struct ConcurrentCat;

/// What one cat asks of the pool: one distinct input and `CAT_EXTRA_FDS`.
const CAT_WANTS: u64 = 9;
/// Both cats' full requests.
const BUDGET: u64 = 2 * CAT_WANTS;
/// Nodes per generated block.
const NODES_PER_BLOCK: i64 = 100;
/// How long a request waits for the other call's.
const GATE_WAIT: Duration = Duration::from_secs(60);

/// Each call: its id, input role, input blocks, output role.
const CALLS: [(&str, &str, u64, &str); 2] = [
    ("a", "input-small", 1, "out-a"),
    ("b", "input-large", 3, "out-b"),
];

const ARGV_A: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input-small"),
    Arg::Literal("-o"),
    Arg::Output("out-a"),
];
const ARGV_B: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input-large"),
    Arg::Literal("-o"),
    Arg::Output("out-b"),
];

/// The two-party gate both calls' descriptor requests meet at.
struct Gate {
    pool: Arc<FdPool>,
    state: Mutex<GateState>,
    met: Condvar,
}

#[derive(Default)]
struct GateState {
    arrived: u32,
    /// Both arrived; set once, by the second.
    opened: bool,
    /// One arrival gave up waiting. Sticky: a late second arrival does not
    /// open the gate.
    failed: bool,
    /// The pool's free count while both grants were held behind the gate.
    free_when_met: Option<u64>,
}

impl Gate {
    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn arrive(&self) {
        let mut state = self.state();
        if state.failed {
            return;
        }
        state.arrived += 1;
        if state.arrived == 2 {
            state.free_when_met = Some(self.pool.available());
            state.opened = true;
            self.met.notify_all();
            return;
        }
        let (mut state, _) = self
            .met
            .wait_timeout_while(state, GATE_WAIT, |s| !s.opened && !s.failed)
            .unwrap_or_else(PoisonError::into_inner);
        if !state.opened {
            state.failed = true;
            self.met.notify_all();
        }
    }
}

/// One call's process actions: the shared pool behind the gate, with its
/// own grant's size and release tracked.
struct Gated {
    gate: Arc<Gate>,
    granted: Mutex<Option<u64>>,
    released: Arc<AtomicBool>,
}

impl ProcessActions for Gated {
    fn cross_thread_alloc_pool(&self) {
        self.gate.pool.cross_thread_alloc_pool();
    }

    fn fd_allowance(&self, wanted: u64) -> FdReservation {
        let inner = self.gate.pool.fd_allowance(wanted);
        let allowance = inner.allowance();
        *self.granted.lock().unwrap_or_else(PoisonError::into_inner) = Some(allowance.descriptors);
        self.gate.arrive();
        let released = Arc::clone(&self.released);
        FdReservation::with_release(allowance, move || {
            drop(inner);
            released.store(true, Ordering::Release);
        })
    }
}

/// One call's tap: whether its grant was still held at `CAT_SCAN_START`,
/// and the cat counters it was delivered.
struct Watch {
    released: Arc<AtomicBool>,
    held_at_scan: Mutex<Option<bool>>,
    counters: Mutex<Vec<(String, i64)>>,
}

impl InstrumentSink for Watch {
    fn marker(&self, name: &str) {
        if name == "CAT_SCAN_START" {
            let held = !self.released.load(Ordering::Acquire);
            *self
                .held_at_scan
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(held);
        }
    }

    fn counter(&self, name: &str, value: i64) {
        if name.starts_with("cat_") {
            self.counters
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((name.to_owned(), value));
        }
    }
}

impl Tap for Watch {
    fn release(&self) {}
}

impl Watch {
    fn counter_value(&self, name: &str) -> Option<i64> {
        self.counters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
    }
}

const fn generated(id: &'static str) -> RoleDecl {
    RoleDecl {
        id,
        role: Role::Generated {
            kind: GeneratedKind::Pbf,
        },
    }
}

impl Scenario for ConcurrentCat {
    fn decl(&self) -> ScenarioDecl {
        let call = |id, label, argv| CallDecl {
            id,
            label,
            cli: Some(Counterpart::output(argv, Some(false), POLICY_DIFFERENCES)),
        };
        ScenarioDecl {
            id: "concurrent-cat",
            description: "two cats at once over inputs of different sizes, sharing one FdPool",
            deadline_s: 300,
            hotpath: false,
            isolation: false,
            calls: vec![
                call("a", "cat input-small -> out-a, beside b", ARGV_A),
                call("b", "cat input-large -> out-b, beside a", ARGV_B),
            ],
            roles: vec![
                generated("input-small"),
                generated("input-large"),
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
                    id: "a.own-counters",
                    call: Some("a"),
                },
                AssertionDecl {
                    id: "b.own-counters",
                    call: Some("b"),
                },
                AssertionDecl {
                    id: "lifetimes-intersected",
                    call: None,
                },
                AssertionDecl {
                    id: "pool-whole-after",
                    call: None,
                },
            ],
        }
    }

    fn plan(&self) -> Value {
        let call = |input: &str, output: &str| {
            json!({
                "command": "cat",
                "inputs": [input],
                "output": output,
                "type_filter": null,
                "clean": clean_json(false),
                "write": write_json(),
                "header_overrides": overrides_json(),
                "output_policy": output_policy_json("refuse"),
                "host": host_json(&json!("machine"), &json!("ambient"),
                                  &json!({ "fd_pool": BUDGET, "shared_by": ["a", "b"],
                                          "gate": "both requests granted before either returns" })),
            })
        };
        let recipe = |blocks: u64| {
            json!({ "writer": "BlockBuilder and PbfWriter, zlib:6, sorted header",
                    "blocks": blocks, "nodes_per_block": NODES_PER_BLOCK,
                    "node_ids": "1 upward", "tags": "none", "metadata": "none" })
        };
        json!({
            "schedule": { "order": ["a", "b"], "concurrency": "both at once, one thread each",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [
                           { "role": "input-small", "recipe": recipe(1) },
                           { "role": "input-large", "recipe": recipe(3) } ],
                       "rayon_global_pool": "started before the first call" },
            "process": process_json(self.nofile()),
            "calls": { "a": call("input-small", "out-a"), "b": call("input-large", "out-b") },
        })
    }

    fn setup(&self, ctx: &Ctx<'_>) -> Result<(), String> {
        for (_, role, blocks, _) in CALLS {
            write_nodes(ctx.binding(role), blocks).map_err(|e| format!("writing {role}: {e}"))?;
        }
        Ok(())
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let pool = Arc::new(FdPool::new(BUDGET, Arc::new(NoProcessActions)));
        let gate = Arc::new(Gate {
            pool: Arc::clone(&pool),
            state: Mutex::new(GateState::default()),
            met: Condvar::new(),
        });
        let parts: Vec<(Arc<Gated>, Arc<Watch>)> = CALLS
            .iter()
            .map(|_| {
                let released = Arc::new(AtomicBool::new(false));
                (
                    Arc::new(Gated {
                        gate: Arc::clone(&gate),
                        granted: Mutex::new(None),
                        released: Arc::clone(&released),
                    }),
                    Arc::new(Watch {
                        released,
                        held_at_scan: Mutex::new(None),
                        counters: Mutex::new(Vec::new()),
                    }),
                )
            })
            .collect();
        let shared: &Ctx<'_> = ctx;
        let records: Vec<Result<CallRecord, String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = CALLS
                .iter()
                .zip(&parts)
                .map(|(&(id, input, _, output), (gated, watch))| {
                    let mut spec = CallSpec::plain(id);
                    spec.process = Process::Pool {
                        actions: Arc::clone(gated) as Arc<dyn ProcessActions>,
                        budget: BUDGET,
                    };
                    spec.tap = Some(Arc::clone(watch) as Arc<dyn Tap>);
                    scope.spawn(move || {
                        shared.call(spec, |host, env| {
                            let input = env.binding(input);
                            cat_into(env, &[input], output, None, &CLEAN_NONE, host).map(drop)
                        })
                    })
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
        let mut done = Vec::new();
        for record in records {
            done.push(record?);
        }
        let mut intervals = Vec::new();
        for ((&(id, _, blocks, output), (gated, watch)), record) in
            CALLS.iter().zip(&parts).zip(&done)
        {
            let judged = judge(id, blocks, output, gated, watch, record);
            intervals.push((record.entry_us, record.return_us));
            ctx.finish_call(record, &judged)?;
        }
        let state = gate.state();
        let overlapped = intervals
            .iter()
            .all(|&(entry, _)| intervals.iter().all(|&(_, ret)| entry <= ret));
        let met = state.opened && !state.failed && state.free_when_met == Some(0);
        let intersected = Outcome::judge(
            "lifetimes-intersected",
            met && overlapped,
            format!(
                "gate opened {}, failed {}, pool free while both were held {:?}, \
                 call intervals overlap {overlapped}",
                state.opened, state.failed, state.free_when_met
            ),
        );
        drop(state);
        let whole = Outcome::judge(
            "pool-whole-after",
            pool.available() == BUDGET,
            format!("{} of {BUDGET} free after both calls", pool.available()),
        );
        Ok(vec![intersected, whole])
    }
}

/// `<call>.outcome` and `<call>.own-counters` for one of the two calls.
fn judge(
    id: &str,
    blocks: u64,
    output: &str,
    gated: &Gated,
    watch: &Watch,
    record: &CallRecord,
) -> [Outcome; 2] {
    let (outcome_id, counters_id) = if id == "a" {
        ("a.outcome", "a.own-counters")
    } else {
        ("b.outcome", "b.own-counters")
    };
    let granted = *gated.granted.lock().unwrap_or_else(PoisonError::into_inner);
    let held = *watch
        .held_at_scan
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let released = gated.released.load(Ordering::Acquire);
    let ok = record.outcome.is_ok()
        && record.cancel.is_none()
        && super::published_once(record, Decision::Authorized, output, Effect::Installed)
        && granted == Some(CAT_WANTS)
        && held == Some(true)
        && released;
    let outcome = Outcome::judge(
        outcome_id,
        ok,
        format!(
            "{}, granted {granted:?}, held at CAT_SCAN_START {held:?}, released after {released}",
            account(record)
        ),
    );
    // Exact values from the generated input, independent of what the
    // call returned: every block of an indexed input is copied raw.
    let passthrough = watch.counter_value("cat_blobs_passthrough");
    let decoded = watch.counter_value("cat_blobs_decoded");
    let expected = i64::try_from(blocks).unwrap_or(i64::MAX);
    let own = Outcome::judge(
        counters_id,
        passthrough == Some(expected) && decoded == Some(0),
        format!(
            "delivered to this call's tap: cat_blobs_passthrough {passthrough:?} \
             (its input has {blocks} blocks), cat_blobs_decoded {decoded:?}"
        ),
    );
    [outcome, own]
}

/// A sorted PBF of `blocks` blocks of untagged nodes, ids from 1, each
/// block written as one blob with indexdata.
fn write_nodes(path: &std::path::Path, blocks: u64) -> pbfhogg::Result<()> {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};
    let file = std::fs::File::create(path)?;
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::Zlib(6));
    writer.write_header(&HeaderBuilder::new().sorted().build()?)?;
    let mut block = BlockBuilder::new();
    let mut id = 1_i64;
    for _ in 0..blocks {
        for _ in 0..NODES_PER_BLOCK {
            let lat = i32::try_from(id).unwrap_or(0) * 1000;
            block.add_node(id, lat, lat, std::iter::empty(), None);
            id += 1;
        }
        if let Some(bytes) = block.take()? {
            writer.write_primitive_block(bytes)?;
        }
    }
    writer.flush()
}
