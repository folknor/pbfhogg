//! `concurrent-cat`: two cats at once on two threads, each under its own
//! invocation and host, over generated inputs of different sizes (one
//! block and three), so counters delivered to the wrong call show, both in
//! each call's sink and in each call's sealed snapshot.
//!
//! Both calls name one descriptor pool the runner builds, sized for both
//! cats' full requests (one distinct input plus `CAT_EXTRA_FDS` each). Each
//! call's descriptor observer is a party to a two-party gate: the runner
//! grants a request from the pool, then the observer waits at the gate until
//! the other call's request was granted too, and the second to arrive reads
//! the pool's free count while both are held behind the gate. That proves
//! the lifetimes intersected and both grants were held at once, by parking,
//! not by thread start order. The wait is in the call's
//! `ProcessActions::fd_allowance`, which cat calls on its own thread before
//! pinning, outside any library lock; the taps never block. Each observer
//! also tracks its own grant's release, which each call's tap reads at
//! `CAT_SCAN_START`: cat still holds its grant while it scans.

use super::{Scenario, account, assertion};
use crate::protocol::Outcome;
use crate::runner::{CallRecord, Done, FdObserver, FdView, Observation, Tap};
use crate::workload::{
    CallPlan, DescriptorPool, HostPlan, InputRole, ProcessPlan, Recipe, Schedule, Workload, cat,
};
use pbfhogg::debug::InstrumentSink;
use pbfhogg::{Decision, Effect};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

pub(crate) struct ConcurrentCat;

/// What one cat asks of the pool: one distinct input and `CAT_EXTRA_FDS`.
const CAT_WANTS: u64 = 9;
/// Both cats' full requests.
const BUDGET: u64 = 2 * CAT_WANTS;
/// Both calls ran at once and completed, each holding its full grant from
/// the shared pool across its scan.
const CONCURRENT_POOL: &[&str] = &["calls.concurrent.v1", "fd.pool.v1"];
/// Each call's sink got exactly its own cat counters.
const OWN_COUNTERS: &[&str] = &["counters.own-call.v1"];
/// Each call's sealed snapshot holds its own engine counters.
const OWN_SNAPSHOT: &[&str] = &["counters.own-snapshot.v1"];
/// The cat counters a passthrough call emits, each exactly once.
const CAT_COUNTERS: [&str; 3] = [
    "cat_blobs_passthrough",
    "cat_blobs_decoded",
    "cat_elements_written",
];

/// How long a request waits for the other call's: a failure bound, set
/// below the harness tests' child timeout so a broken rendezvous reports
/// itself rather than being killed.
const GATE_WAIT: Duration = pbfhogg::TEST_RENDEZVOUS_BOUND;

/// Each call: its id, input role, input blocks, output role.
const CALLS: [(&str, &str, u64, &str); 2] = [
    ("a", "input-small", 1, "out-a"),
    ("b", "input-large", 3, "out-b"),
];

/// The two-party gate both calls' descriptor requests meet at.
#[derive(Default)]
struct Gate {
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
    /// The pool's budget, as the second arrival saw it.
    budget: Option<u64>,
}

impl Gate {
    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn arrive(&self, pool: &FdView<'_>) {
        let mut state = self.state();
        if state.failed {
            return;
        }
        state.arrived += 1;
        if state.arrived == 2 {
            state.free_when_met = Some(pool.available());
            state.budget = Some(pool.budget());
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

/// One call's descriptor observer: a party to the gate, with its own
/// grant's size and release tracked.
struct Gated {
    gate: Arc<Gate>,
    granted: Mutex<Option<u64>>,
    released: Arc<AtomicBool>,
    /// The pool's free count just after this grant went back.
    free_after_release: Mutex<Option<u64>>,
}

impl FdObserver for Gated {
    fn granted(&self, _wanted: u64, granted: u64, pool: &FdView<'_>) {
        *self.granted.lock().unwrap_or_else(PoisonError::into_inner) = Some(granted);
        self.gate.arrive(pool);
    }

    fn released(&self, pool: &FdView<'_>) {
        *self
            .free_after_release
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(pool.available());
        self.released.store(true, Ordering::Release);
    }
}

/// One call's tap: whether its grant was still held at `CAT_SCAN_START`
/// and at `CAT_SCAN_END` (release is sticky, so held at both is held
/// throughout), and every cat counter sample it was delivered.
struct Watch {
    released: Arc<AtomicBool>,
    held_at_scan: Mutex<Option<bool>>,
    held_at_scan_end: Mutex<Option<bool>>,
    counters: Mutex<Vec<(String, i64)>>,
}

impl InstrumentSink for Watch {
    fn marker(&self, name: &str) {
        let slot = match name {
            "CAT_SCAN_START" => &self.held_at_scan,
            "CAT_SCAN_END" => &self.held_at_scan_end,
            _ => return,
        };
        let held = !self.released.load(Ordering::Acquire);
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(held);
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
    /// Every cat counter sample delivered, in order.
    fn samples(&self) -> Vec<(String, i64)> {
        self.counters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Scenario for ConcurrentCat {
    fn workload(&self) -> Workload {
        let mut w = Workload::new(
            "concurrent-cat",
            "two cats at once over inputs of different sizes, sharing one FdPool",
            300,
        );
        w.inputs = CALLS
            .iter()
            .map(|&(_, role, blocks, _)| InputRole::generated(role, Recipe::Nodes { blocks }))
            .collect();
        w.fd_pools = vec![DescriptorPool {
            id: "shared",
            budget: BUDGET,
        }];
        w.schedule = Schedule::Concurrent;
        let call = |id, label, input: &'static [&'static str], output| {
            let mut call = CallPlan::plain(id, label, cat(input, output));
            call.host = HostPlan {
                process: ProcessPlan::FdPool("shared"),
                ..HostPlan::PLAIN
            };
            call.notes = Some(json!({
                "fd_gate": "each grant waits until both calls' requests were granted"
            }));
            call
        };
        w.calls = vec![
            call(
                "a",
                "cat input-small -> out-a, beside b",
                &["input-small"],
                "out-a",
            ),
            call(
                "b",
                "cat input-large -> out-b, beside a",
                &["input-large"],
                "out-b",
            ),
        ];
        w.assertions = vec![
            assertion("a.outcome", Some("a"), CONCURRENT_POOL),
            assertion("b.outcome", Some("b"), CONCURRENT_POOL),
            assertion("a.own-counters", Some("a"), OWN_COUNTERS),
            assertion("b.own-counters", Some("b"), OWN_COUNTERS),
            assertion("a.own-snapshot", Some("a"), OWN_SNAPSHOT),
            assertion("b.own-snapshot", Some("b"), OWN_SNAPSHOT),
            assertion("lifetimes-intersected", None, CONCURRENT_POOL),
            assertion("pool-whole-after", None, &["fd.pool.v1"]),
        ];
        w
    }

    fn observe(&self) -> Box<dyn Observation> {
        let gate = Arc::new(Gate::default());
        let parts = CALLS
            .iter()
            .map(|_| {
                let released = Arc::new(AtomicBool::new(false));
                (
                    Arc::new(Gated {
                        gate: Arc::clone(&gate),
                        granted: Mutex::new(None),
                        released: Arc::clone(&released),
                        free_after_release: Mutex::new(None),
                    }),
                    Arc::new(Watch {
                        released,
                        held_at_scan: Mutex::new(None),
                        held_at_scan_end: Mutex::new(None),
                        counters: Mutex::new(Vec::new()),
                    }),
                )
            })
            .collect();
        Box::new(Judge { gate, parts })
    }
}

struct Judge {
    gate: Arc<Gate>,
    /// Per call, in [`CALLS`] order.
    parts: Vec<(Arc<Gated>, Arc<Watch>)>,
}

impl Judge {
    fn index(call: &str) -> Option<usize> {
        CALLS.iter().position(|&(id, ..)| id == call)
    }
}

impl Observation for Judge {
    fn tap(&self, call: &str) -> Option<Arc<dyn Tap>> {
        let i = Self::index(call)?;
        Some(Arc::clone(&self.parts[i].1) as Arc<dyn Tap>)
    }

    fn fd_observer(&self, call: &str) -> Option<Arc<dyn FdObserver>> {
        let i = Self::index(call)?;
        Some(Arc::clone(&self.parts[i].0) as Arc<dyn FdObserver>)
    }

    fn judge_call(&self, record: &CallRecord, done: &Done<'_>) -> Vec<Outcome> {
        let Some(i) = Self::index(record.id) else {
            return Vec::new();
        };
        let (id, _, blocks, output) = CALLS[i];
        let (gated, watch) = &self.parts[i];
        let written = std::fs::metadata(done.binding(output))
            .map(|m| m.len())
            .ok();
        judge(id, blocks, output, written, gated, watch, record).into()
    }

    fn judge_scenario(&self, done: &Done<'_>) -> Vec<Outcome> {
        let intervals: Vec<(u64, u64)> = CALLS
            .iter()
            .filter_map(|&(id, ..)| done.record(id))
            .map(|r| (r.entry_us, r.return_us))
            .collect();
        let overlapped = intervals.len() == CALLS.len()
            && intervals
                .iter()
                .all(|&(entry, _)| intervals.iter().all(|&(_, ret)| entry <= ret));
        let state = self.gate.state();
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
        let budget = state.budget;
        drop(state);
        // Every grant handed back: the later release saw the whole planned
        // budget free again.
        let free_after: Vec<Option<u64>> = self
            .parts
            .iter()
            .map(|(gated, _)| {
                *gated
                    .free_after_release
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            })
            .collect();
        let whole_again = free_after.iter().all(Option::is_some)
            && free_after.iter().flatten().max() == Some(&BUDGET);
        let whole = Outcome::judge(
            "pool-whole-after",
            whole_again && budget == Some(BUDGET),
            format!(
                "free after each release {free_after:?}, pool budget {budget:?}, \
                 expected {BUDGET} free after the last"
            ),
        );
        vec![intersected, whole]
    }
}

/// `<call>.outcome`, `<call>.own-counters` and `<call>.own-snapshot` for one
/// of the two calls. `written` is the size of the call's installed output.
fn judge(
    id: &str,
    blocks: u64,
    output: &str,
    written: Option<u64>,
    gated: &Gated,
    watch: &Watch,
    record: &CallRecord,
) -> [Outcome; 3] {
    let (outcome_id, counters_id, snapshot_id) = if id == "a" {
        ("a.outcome", "a.own-counters", "a.own-snapshot")
    } else {
        ("b.outcome", "b.own-counters", "b.own-snapshot")
    };
    let granted = *gated.granted.lock().unwrap_or_else(PoisonError::into_inner);
    let held = *watch
        .held_at_scan
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let held_end = *watch
        .held_at_scan_end
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let released = gated.released.load(Ordering::Acquire);
    let ok = record.outcome.is_ok()
        && record.cancel.is_none()
        && super::published_once(record, Decision::Authorized, output, Effect::Installed)
        && granted == Some(CAT_WANTS)
        && held == Some(true)
        && held_end == Some(true)
        && released;
    let outcome = Outcome::judge(
        outcome_id,
        ok,
        format!(
            "{}, granted {granted:?}, held at CAT_SCAN_START {held:?} and CAT_SCAN_END \
             {held_end:?}, released after {released}",
            account(record)
        ),
    );
    // The exact inventory, from the generated input and independent of
    // what the call returned: every block of an indexed input is copied
    // raw, so none is decoded and no element is re-encoded. Each name once,
    // no other cat counter: a sample delivered to both taps shows twice.
    let expected = i64::try_from(blocks).unwrap_or(i64::MAX);
    let mut samples = watch.samples();
    samples.sort();
    let mut inventory: Vec<(String, i64)> = CAT_COUNTERS
        .iter()
        .zip([expected, 0, 0])
        .map(|(name, value)| ((*name).to_owned(), value))
        .collect();
    inventory.sort();
    let own = Outcome::judge(
        counters_id,
        samples == inventory,
        format!(
            "delivered to this call's tap: {samples:?}; expected each once: {inventory:?} \
             (its input has {blocks} blocks)"
        ),
    );
    // The sealed snapshot itself, which the stream verdict only checks the
    // stream against: two engine counters whose values follow from this
    // call's own workload, so the other call's work added in, or the two
    // snapshots swapped, shows. Every data block of the input is copied raw
    // as one payload (the header is not a raw payload), and the bytes the
    // writer wrote are the installed output's size. Timings, high waters and
    // thread-local scratch peaks follow from no input and are not judged.
    let raw_items = record.sealed_counter("writer_payload_raw_items");
    let bytes_written = record.sealed_counter("writer_bytes_written");
    let snapshot = Outcome::judge(
        snapshot_id,
        raw_items == Some(blocks) && written.is_some() && bytes_written == written,
        format!(
            "sealed snapshot: writer_payload_raw_items {raw_items:?} (input blocks {blocks}), \
             writer_bytes_written {bytes_written:?} (installed output {written:?} bytes)"
        ),
    );
    [outcome, own, snapshot]
}
