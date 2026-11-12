//! Admission of descriptor-heavy scenarios in a binary whose tests share
//! one `RLIMIT_NOFILE`.
//!
//! A light host (`common::test_host()`) grants at most
//! `TEST_LIGHT_FD_GRANT` per request, below any external join's stage-1
//! minimum, so a descriptor-heavy run on it refuses typed instead of
//! running ungated. A heavy scenario takes one [`FdTicket`] of whole-run
//! units from this binary's gate before it builds anything, and builds its
//! hosts from it ([`heavy_host`], or a test-local provider delegating to the
//! ticket). The ticket is the provider, so it is released only when the
//! test's handle and every host and provider holding it have dropped.
//!
//! What it does and does not bound: the gate admits tickets in arrival
//! order against the soft limit less `TEST_LIGHT_FD_RESERVE`; a ticket's
//! units bound the scenario's simultaneous whole runs by the scenario's
//! construction, which nothing checks; one unit is charged
//! `TEST_HEAVY_RUN_FDS`, which `process_nofile.rs` witnesses for the
//! largest external join a heavy grant admits (every stage-1 fleet and the
//! relation scan's input open at once, the most slot buckets, buffered
//! output, under a kernel limit leaving exactly the charge free), not for
//! Auto routing, fault paths, other output backends or the scenario's own
//! fixtures and scratch. Light tests' descriptors are not requested or
//! counted; the reserve covers them by assertion. Neither a count of
//! `/proc/self/fd` at a ticket's release nor an invariant checked at
//! admission observes them: the count is process-wide, includes every
//! concurrent test and is taken after the run's peak, and light use grows
//! with the runner's thread count, so a check would fail on large hosts
//! against whichever test happened to be admitted. A test
//! holding a ticket must not ask for another: that waits on itself until
//! the admission bound and skips. A scenario that cannot be admitted (its
//! units exceed what the limit leaves, or the admission bound expires
//! behind its siblings) skips as environmental before it opens anything.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

struct Gate {
    capacity: u64,
    state: Mutex<State>,
    bell: Condvar,
}

struct State {
    free: u64,
    queue: VecDeque<u64>,
    next: u64,
}

static GATE: OnceLock<Gate> = OnceLock::new();

fn gate() -> &'static Gate {
    GATE.get_or_init(|| {
        // The library `all` binary raised its soft limit before `main`.
        let soft = pbfhogg::process::current_nofile()
            .probe
            .map_or(0, |probe| probe.soft_limit);
        let capacity = soft.saturating_sub(pbfhogg::TEST_LIGHT_FD_RESERVE);
        Gate {
            capacity,
            state: Mutex::new(State {
                free: capacity,
                queue: VecDeque::new(),
                next: 0,
            }),
            bell: Condvar::new(),
        }
    })
}

// A panicking holder cannot leave the counts torn: every update is a single
// assignment made under the lock, so a poisoned state is still consistent.
fn lock(gate: &Gate) -> MutexGuard<'_, State> {
    gate.state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An admitted heavy scenario's share of the descriptor limit, and the
/// descriptor provider of every host built from it: each request is
/// granted `min(wanted, TEST_FD_GRANT)`, as on a host before this gate
/// existed. Dropping the last handle returns the share.
pub(crate) struct FdTicket {
    descriptors: u64,
}

impl pbfhogg::host::ProcessActions for FdTicket {
    fn fd_allowance(&self, wanted: u64) -> pbfhogg::process::FdReservation {
        heavy_grant(wanted)
    }
}

/// What every heavy host's provider grants a request: `min(wanted,
/// TEST_FD_GRANT)`, reserving nothing. Shared with the own-process witness
/// that runs the largest run this grant admits under a kernel limit of
/// exactly `TEST_HEAVY_RUN_FDS` free descriptors (`process_nofile.rs`).
pub(crate) fn heavy_grant(wanted: u64) -> pbfhogg::process::FdReservation {
    pbfhogg::process::FdReservation::unreserved(pbfhogg::process::FdAllowance::host_grant(
        wanted.min(super::TEST_FD_GRANT),
    ))
}

impl Drop for FdTicket {
    fn drop(&mut self) {
        let gate = gate();
        lock(gate).free += self.descriptors;
        gate.bell.notify_all();
    }
}

/// Admit a scenario running up to `runs` descriptor-heavy runs at once.
/// `None` means the scenario was skipped as environmental and reported;
/// the caller returns before building anything.
pub(crate) fn heavy_ticket(runs: u64, test: &str) -> Option<Arc<FdTicket>> {
    let want = runs.saturating_mul(pbfhogg::TEST_HEAVY_RUN_FDS);
    let gate = gate();
    if want > gate.capacity {
        super::skip(format!(
            "{test}: needs {want} descriptors for {runs} heavy run(s), but the soft \
             descriptor limit leaves {} after the light-test reserve",
            gate.capacity
        ));
        return None;
    }
    let deadline = Instant::now() + pbfhogg::TEST_FD_ADMISSION_BOUND;
    let mut state = lock(gate);
    let id = state.next;
    state.next += 1;
    state.queue.push_back(id);
    // loop-ends: parks on the gate's condvar; admission and every ticket
    // drop notify, and the deadline is an environmental skip
    loop {
        if state.queue.front() == Some(&id) && state.free >= want {
            state.queue.pop_front();
            state.free -= want;
            drop(state);
            // The next head may fit in what is left.
            gate.bell.notify_all();
            return Some(Arc::new(FdTicket { descriptors: want }));
        }
        let now = Instant::now();
        if now >= deadline {
            state.queue.retain(|&queued| queued != id);
            drop(state);
            // A departed head must not block the waiters behind it.
            gate.bell.notify_all();
            super::skip(format!(
                "{test}: not admitted to {want} descriptors within {:?} behind \
                 descriptor-heavy siblings",
                pbfhogg::TEST_FD_ADMISSION_BOUND
            ));
            return None;
        }
        // Expiry is not taken as the answer: the loop decides admission or
        // skip under the lock from the queue and the counts.
        let (next, _) = gate
            .bell
            // wait-deliberate: the loop above decides under the lock
            .wait_timeout(state, deadline - now)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state = next;
    }
}

/// [`super::test_host`] whose descriptor provider is `ticket`.
pub(crate) fn heavy_host(ticket: &Arc<FdTicket>) -> pbfhogg::host::Host {
    let mut host = super::test_host();
    host.process = Arc::clone(ticket) as Arc<dyn pbfhogg::host::ProcessActions>;
    host
}
