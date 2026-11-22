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
//! units bound the scenario's simultaneous grants, checked at each request
//! ([`FdTicket`]), and its simultaneous whole runs by the scenario's
//! construction; one unit is charged `TEST_HEAVY_RUN_FDS`, which
//! `process_nofile.rs` witnesses for the largest external join a heavy
//! grant admits (every stage-1 fleet and the relation scan's input open at
//! once, then every stage 3 worker's bucket file with stage 4's input and
//! output open at once, the most slot buckets, reached as External and as
//! Auto, with buffered, O_DIRECT and io_uring output where the host
//! accepts them, under a kernel limit leaving exactly the charge free);
//! fault paths are bounded from the code, since no fault hook opens a
//! descriptor. A test's `host::FdPool` is built only by [`pool`], which
//! requires a ticket above the light grant. Test-local providers that
//! replace a host's descriptor answers outright (rather than delegating to
//! a ticket) are light by convention. A heavy scenario holds no descriptor
//! of its own across a run beyond the input pin the run shares. Light
//! tests' descriptors are not requested or counted; the reserve covers
//! them by assertion. Neither a count of
//! `/proc/self/fd` at a ticket's release nor an invariant checked at
//! admission observes them: the count is process-wide, includes every
//! concurrent test and is taken after the run's peak, and light use grows
//! with the runner's thread count, so a check would fail on large hosts
//! against whichever test happened to be admitted. A scenario runs inside
//! [`with_heavy_ticket`], which owns the ticket's lifetime: asking for a
//! second ticket while one runs on the thread panics, and so does any part
//! of the ticket outliving the scenario, so neither defect can wait out the
//! admission bound and pass as a skip. A scenario that cannot be admitted
//! (its units exceed what the limit leaves, or the admission bound expires
//! behind its siblings' running scenarios) skips as environmental before it
//! opens anything.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
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
/// existed. The share returns when the last handle and every reservation
/// granted through it have dropped.
///
/// A ticket of `units` admits at most `units` live grants at once: a
/// request that would exceed them panics, naming the scenario. This is a
/// conservative request-concurrency contract, not a descriptor count (two
/// small grants could fit one charge), and it observes overlapping
/// requests, not overlapping whole runs: an external join asks only for
/// its stage-1 shard files, so two joins on one unit that overlap only
/// after stage 1 are not caught.
pub(crate) struct FdTicket {
    admission: Arc<Admission>,
}

struct Admission {
    descriptors: u64,
    units: u64,
    live: AtomicU64,
    test: String,
}

/// One live grant against an admission; dropping it ends the grant.
struct Hold(Arc<Admission>);

impl Admission {
    fn hold(self: &Arc<Self>) -> Hold {
        let live = self.live.fetch_add(1, Ordering::AcqRel) + 1;
        assert!(
            live <= self.units,
            "{}: {live} descriptor grants live at once on a ticket of {} unit(s); \
             take a unit per simultaneous heavy run",
            self.test,
            self.units
        );
        Hold(Arc::clone(self))
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::AcqRel);
    }
}

impl pbfhogg::host::ProcessActions for FdTicket {
    fn fd_allowance(&self, wanted: u64) -> pbfhogg::process::FdReservation {
        let hold = self.admission.hold();
        let grant = heavy_grant(wanted);
        pbfhogg::process::FdReservation::with_release(grant.allowance(), move || drop(hold))
    }
}

/// A test's `host::FdPool`, built only here so that a pool large enough
/// for a heavy run is reached through a ticket.
pub(crate) struct TestPool {
    pool: Arc<pbfhogg::host::FdPool>,
    provider: Arc<dyn pbfhogg::host::ProcessActions>,
}

impl TestPool {
    /// Descriptors not granted now; see `FdPool::available`.
    pub(crate) fn available(&self) -> u64 {
        self.pool.available()
    }

    /// The provider a host built on this pool installs.
    pub(crate) fn provider(&self) -> Arc<dyn pbfhogg::host::ProcessActions> {
        Arc::clone(&self.provider)
    }
}

/// A pool of `budget` descriptors. A budget above `TEST_LIGHT_FD_GRANT`
/// needs `ticket`: every request then holds one of its units for as long
/// as the pool's reservation lives and asks the pool for at most
/// `TEST_FD_GRANT`, the grant the charge witness covers.
pub(crate) fn pool(budget: u64, ticket: Option<&Arc<FdTicket>>) -> TestPool {
    let pool = Arc::new(pbfhogg::host::FdPool::new(budget));
    let provider: Arc<dyn pbfhogg::host::ProcessActions> = match ticket {
        Some(ticket) => Arc::new(Ticketed {
            admission: Arc::clone(&ticket.admission),
            pool: Arc::clone(&pool),
        }),
        None => {
            assert!(
                budget <= pbfhogg::TEST_LIGHT_FD_GRANT,
                "a pool of {budget} descriptors exceeds the light grant and needs a ticket"
            );
            Arc::clone(&pool) as Arc<dyn pbfhogg::host::ProcessActions>
        }
    };
    TestPool { pool, provider }
}

struct Ticketed {
    admission: Arc<Admission>,
    pool: Arc<pbfhogg::host::FdPool>,
}

impl pbfhogg::host::ProcessActions for Ticketed {
    fn fd_allowance(&self, wanted: u64) -> pbfhogg::process::FdReservation {
        let hold = self.admission.hold();
        let inner = self.pool.fd_allowance(wanted.min(super::TEST_FD_GRANT));
        let allowance = inner.allowance();
        pbfhogg::process::FdReservation::with_release(allowance, move || {
            drop(inner);
            drop(hold);
        })
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

impl Drop for Admission {
    fn drop(&mut self) {
        let gate = gate();
        lock(gate).free += self.descriptors;
        gate.bell.notify_all();
    }
}

thread_local! {
    /// The heavy scenario running on this thread, if any.
    static RUNNING: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Clears this thread's running scenario, on return and on unwind alike.
struct Running;

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.with(|running| running.borrow_mut().take());
    }
}

/// Run a scenario of up to `runs` descriptor-heavy runs at once under one
/// admitted ticket. `None` means the scenario was skipped as environmental
/// and reported, and `run` never ran: its units exceed what the limit
/// leaves, or it was not admitted within the bound behind siblings holding
/// the gate (on an ordinary host the gate never fills, so that skip is
/// reached only where the limit admits few heavy scenarios at once).
///
/// Ownership defects fail rather than wait and skip, on every host: a
/// scenario asking for a ticket while one runs on its thread panics before
/// it queues, and once `run` returns the scenario must hold nothing of its
/// ticket (no host, provider, pool or grant built from it, in its result
/// or anywhere else), or the call panics naming the scenario. The
/// descriptors themselves return to the gate when the last real holder
/// drops.
pub(crate) fn with_heavy_ticket<R>(
    runs: u64,
    test: &str,
    run: impl FnOnce(&Arc<FdTicket>) -> R,
) -> Option<R> {
    if let Some(outer) = RUNNING.with(|running| running.borrow().clone()) {
        panic!(
            "{test}: asked for a descriptor ticket while {outer} holds one on this thread; \
             a scenario takes one ticket with a unit per simultaneous heavy run"
        );
    }
    let ticket = admit(runs, test)?;
    RUNNING.with(|running| *running.borrow_mut() = Some(test.to_owned()));
    let running = Running;
    let admission = Arc::downgrade(&ticket.admission);
    let result = run(&ticket);
    drop(ticket);
    drop(running);
    // Counted without upgrading: a temporary owner taken here could become
    // the last one and release into the gate's lock from inside this check.
    let owners = admission.strong_count();
    assert!(
        owners == 0,
        "{test}: {owners} owner(s) of its descriptor ticket outlived the scenario; \
         every host, provider and grant built from a ticket ends with its scenario"
    );
    Some(result)
}

/// Admit a scenario running up to `runs` descriptor-heavy runs at once.
/// `None` means the scenario was skipped as environmental and reported.
fn admit(runs: u64, test: &str) -> Option<Arc<FdTicket>> {
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
    // drop notify, and the deadline is an environmental skip: a nested
    // request on the thread was refused before queueing, and a ticket
    // outliving its scenario fails that scenario's test, so a skip here is
    // never the only report of an ownership defect
    loop {
        if state.queue.front() == Some(&id) && state.free >= want {
            state.queue.pop_front();
            state.free -= want;
            drop(state);
            // The next head may fit in what is left.
            gate.bell.notify_all();
            return Some(Arc::new(FdTicket {
                admission: Arc::new(Admission {
                    descriptors: want,
                    units: runs,
                    live: AtomicU64::new(0),
                    test: test.to_owned(),
                }),
            }));
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
