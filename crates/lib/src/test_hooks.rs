//! Fault injection for tests: what a test can inject, in one place.
//!
//! Compiled only under the compiler cfg `pbfhogg_test_hooks`, which is not a
//! Cargo feature: no feature selection, dependency or feature unification can
//! switch it on, only an explicit `--cfg` (the hook-testing check sweeps pass
//! it). Every hook site is gated on the same cfg, so a build without it has
//! none of this code.
//!
//! # The fault plan rides the invocation token
//!
//! [`FaultPlan`] holds every token-scoped hook: arming fields and the
//! observation counters a test reads back. One plan lives inside each
//! [`Invocation`](crate::host::Invocation) token ([`Invocation::faults`](crate::host::Invocation::faults)),
//! because the token is already what an invocation hands to every worker
//! loop it starts (the read pipeline, the output writers, the diff and derive
//! shards, altw's stages, the geocode passes). Arming a fault is configuring
//! the token a test passes in its `Host`; nothing is process-global, so a
//! fault test shares a harness with every other test and needs no reset.
//!
//! The rule that follows: **a token is one fault scenario.** Everything run
//! under one token shares its plan - every reader's blob sequence numbers,
//! every writer's worker indices and framing sequence numbers, every counter.
//! A test that needs two independent scenarios (or an armed and an unarmed
//! run to compare) uses two tokens; `common::test_host()` mints a fresh one
//! per call. Per-token ownership keeps a fault from reaching another test; it
//! does not stop an armed run's own workers, so a test that parks workers
//! still releases and joins them on every exit path.
//!
//! The library's public path constructors (`PbfWriter::to_path` and kin)
//! mint their own token; [`PbfWriter::test_faults`](crate::writer::PbfWriter::test_faults)
//! reaches that writer's plan.
//!
//! # Hooks owned elsewhere
//!
//! Output publication's hooks are token-scoped like the rest: a
//! [`PublishFault`] is armed with [`FaultPlan::arm_publish_fault`], and
//! [`FaultPlan::on_publish_step`] observes every [`PublishStep`] (both types
//! live with the output code and are re-exported here). A front end builds
//! its outputs in several places, and the token is the one thing they all
//! publish under.
//!
//! These are per-object rather than per-token, and stay with their owner:
//!
//! - `Invocation::on_check` / `Invocation::checks_passed`: the cancellation
//!   checkpoint callback (cancellation semantics, not a fault).
//! - The geocode index reader's lookup counter, on the reader.
//!
//! Disarmed is a sentinel (`MAX` for an index or a count), never zero, since
//! zero is a real worker, shard or sequence number.
//!
//! # Waiting: the plan's bell
//!
//! A hook that parks and a test that waits for a hook both wait on the
//! plan's bell, never on a sleep loop. Every field some wait's condition
//! reads is a [`Watched`] atomic: anyone may load it, but only this module
//! writes it, and every write rings the bell after the store, so a waiter
//! wakes at the transition. A code path outside this module that moves such
//! a field calls an `on_*` hook here, and a test releases parked work
//! through a `release_*` method. Tripping the invocation's token rings the
//! bell too ([`Invocation::cancel`](crate::host::Invocation::cancel)).
//!
//! A wait snapshots the bell's ring count, evaluates its condition without
//! the bell's lock, and sleeps only while the count is unchanged, so a ring
//! between the evaluation and the sleep is never lost, and a condition may
//! take any other lock. A condition over state that does not ring (the
//! merge-changes ledger's halt, the altw router's abort, a test's own
//! counters) waits polled instead: it is also re-evaluated every
//! `TEST_HOOK_TICK`. Every wait ends at one absolute deadline,
//! `TEST_HOOK_WAIT` after it starts. [`FaultPlan::wait_for`], the test-side
//! wait, is always polled, since a closure cannot prove that what it reads
//! rings; the hook waits in this module are classified one by one.
//!
//! The bell is a notification, not a snapshot: a condition over several
//! fields reads them one by one, and a transient state that comes and goes
//! between two evaluations can be missed. A handshake that needs either
//! belongs in state under one lock, as the read pipeline's decode gate is.

pub use crate::output::{PublishFault, PublishStep};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Upper bound on every bounded hook wait, so a broken pipeline fails the
/// test that armed it with a named message instead of hanging it; see the
/// test time bounds in `tunables`.
const HOOK_WAIT: Duration = crate::tunables::TEST_HOOK_WAIT;

/// How often a polled wait re-evaluates a condition over unrung state.
const HOOK_TICK: Duration = crate::tunables::TEST_HOOK_TICK;

/// The plan's doorbell: a ring count under a lock, and the condition
/// variable every waiter sleeps on. See the module docs.
struct Bell {
    rings: Mutex<u64>,
    rung: Condvar,
}

impl Bell {
    const fn new() -> Self {
        Self {
            rings: Mutex::new(0),
            rung: Condvar::new(),
        }
    }

    fn rings(&self) -> MutexGuard<'_, u64> {
        self.rings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wake every waiter: called after the store it announces.
    fn ring(&self) {
        let mut rings = self.rings();
        *rings = rings.wrapping_add(1);
        // Every waiter: the bell is shared by unrelated conditions.
        self.rung.notify_all();
    }

    /// Sleep while the ring count is still `seen`, at most `timeout`.
    fn sleep_past(&self, seen: u64, timeout: Duration) {
        let rings = self.rings();
        drop(
            self.rung
                // wait-deliberate: the bell's park; callers re-evaluate their condition
                .wait_timeout_while(rings, timeout, |rings| *rings == seen)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }
}

/// What a wait's condition reads, which decides how it waits.
#[derive(Clone, Copy)]
enum Wake {
    /// Only state whose every write rings the bell: [`Watched`] fields and
    /// the token. Sleeps until a ring or the deadline.
    Rung,
    /// Also state that does not ring: re-evaluated every [`HOOK_TICK`] as
    /// well.
    Polled,
}

/// A plan field some wait's condition reads: loadable by anyone, written
/// only by this module, and every write rings the plan's bell. See the
/// module docs.
pub struct Watched<A>(A);

impl Watched<AtomicBool> {
    const fn new(value: bool) -> Self {
        Self(AtomicBool::new(value))
    }

    /// The current value.
    pub fn load(&self, order: Ordering) -> bool {
        self.0.load(order)
    }

    fn store(&self, value: bool, bell: &Bell) {
        self.0.store(value, Ordering::SeqCst);
        bell.ring();
    }
}

impl Watched<AtomicU64> {
    const fn new(value: u64) -> Self {
        Self(AtomicU64::new(value))
    }

    /// The current value.
    pub fn load(&self, order: Ordering) -> u64 {
        self.0.load(order)
    }

    fn store(&self, value: u64, bell: &Bell) {
        self.0.store(value, Ordering::SeqCst);
        bell.ring();
    }

    fn fetch_add(&self, n: u64, bell: &Bell) {
        self.0.fetch_add(n, Ordering::SeqCst);
        bell.ring();
    }

    fn fetch_sub(&self, n: u64, bell: &Bell) {
        self.0.fetch_sub(n, Ordering::SeqCst);
        bell.ring();
    }
}

impl Watched<AtomicUsize> {
    const fn new(value: usize) -> Self {
        Self(AtomicUsize::new(value))
    }

    /// The current value.
    pub fn load(&self, order: Ordering) -> usize {
        self.0.load(order)
    }

    fn store(&self, value: usize, bell: &Bell) {
        self.0.store(value, Ordering::SeqCst);
        bell.ring();
    }
}

/// Why a parked framing job's wait ended.
#[derive(Clone, Copy)]
enum FramingRelease {
    /// The waiting writer had taken back every permit not held by a parked
    /// job.
    Reached,
    /// The test released it.
    Released,
}

/// Every token-scoped fault and observation. See the module docs.
///
/// Arming fields and plain counters are public atomics: a test stores to
/// arm and loads to observe. A field some wait reads is [`Watched`]
/// instead: loadable, and moved only through the plan's methods. Each group
/// names the code it reaches.
pub struct FaultPlan {
    /// Rung after every write to a [`Watched`] field.
    bell: Bell,

    // --- read pipeline (`read::pipeline::run_pipeline`) ---
    decode_gate: Mutex<DecodeGate>,
    decode_gate_cv: Condvar,

    // --- library-built rayon pools (`joined_pool::JoinedPool`) ---
    /// Index of the pool thread whose spawn fails with an injected I/O error
    /// (message [`INJECTED_POOL_SPAWN_FAILURE`]), so the build fails with the
    /// threads below that index already running. Every pool built under the
    /// token sees it.
    pub built_pool_spawn_fail_at: AtomicUsize,
    /// Park every pool thread once its rayon main loop has returned (counted
    /// in `built_pool_threads_held`) until the test sets
    /// `release_built_pool_threads` (bounded). An owner that does not join
    /// its pool therefore returns while a thread is still parked.
    pub hold_built_pool_threads: AtomicBool,
    /// Pool threads that reached the park; only ever grows.
    pub built_pool_threads_held: Watched<AtomicU64>,
    /// Test-side release for parked pool threads.
    pub release_built_pool_threads: Watched<AtomicBool>,
    /// Pool threads whose exit hook completed: the last thing a thread's
    /// main function does. Its thread-local destructors run after it, so
    /// this counts hook completion, not thread exit.
    pub built_pool_exit_hooks_done: AtomicU64,
    /// A bounded pool-thread park expired.
    pub built_pool_hook_timed_out: AtomicBool,
    /// Owners that reached their pool's join: counted as the join guard
    /// drops, before it joins the first thread (on success, on a build
    /// that failed part way, and during an unwind). With the threads held,
    /// it says the owner is at its join while they are still running; it
    /// does not by itself say the join waits.
    pub built_pool_joins_started: Watched<AtomicU64>,

    // --- publication execution lock (`host::Recorder::publication_lock`) ---
    /// Publications that found the invocation's execution lock held and
    /// went on to wait for it: counted before the blocking acquire, so a
    /// test that sees it knows the publication is waiting undecided.
    pub publication_lock_waits: Watched<AtomicU64>,

    // --- multi-extract (`commands::extract::multi`) ---
    /// Decline the region grid, so classification runs the linear scan even
    /// at or above the region threshold (the grid-fallback note still fires).
    pub force_linear_region_grid: AtomicBool,
    /// Grids declined by `force_linear_region_grid`, so a parity test can
    /// prove its linear arm did not build one.
    pub region_grids_declined: AtomicU64,

    // --- parallel pwrite writer (`write::parallel_writer`) ---
    /// Index of the pool worker that panics on the first op it pops. Worker 0
    /// deterministically gets the first dispatched op (round-robin starts at
    /// 0). With [`hold_other_pool_workers`](Self::hold_other_pool_workers)
    /// set, the panic waits until at least one sibling is held mid-op, so the
    /// failure provably lands while another worker is alive.
    pub pool_worker_panic: AtomicU64,
    /// Park every worker other than the panicking one on each op it pops
    /// (counted in `held_pool_workers`) until the owner starts joining a
    /// non-panicked worker (`join_reached_held_worker`) or the test sets
    /// `release_pool_workers`. A writer that returns without joining its pool
    /// therefore returns while a worker is still parked, which
    /// `live_pool_workers` shows deterministically.
    pub hold_other_pool_workers: AtomicBool,
    /// Workers currently parked by `hold_other_pool_workers`.
    pub held_pool_workers: Watched<AtomicU64>,
    /// Test-side release for parked workers (cleanup after the assertion).
    pub release_pool_workers: Watched<AtomicBool>,
    /// Set when the owner is about to join a worker other than the panicked
    /// one while some worker is still parked: the owner is waiting for its
    /// pool rather than walking away from it. Releases parked workers.
    pub join_reached_held_worker: Watched<AtomicBool>,
    /// Pool worker threads running: incremented on worker entry, decremented
    /// as the very last thing the worker does.
    pub live_pool_workers: Watched<AtomicU64>,
    /// Ops popped and executed across the pool. Ops discarded after the pool
    /// stopped are not counted.
    pub pool_op_count: AtomicU64,
    /// A bounded parallel-writer hook wait expired: the handshake did not
    /// happen, and a test relying on it proved nothing.
    pub pool_hook_timed_out: AtomicBool,

    // --- OSC gzip pool (`write::parallel_gzip`) ---
    /// Compress-op count (1-based, across the pool) at which the acting
    /// worker panics.
    pub gzip_panic_at_op: AtomicU64,
    /// Compress ops dequeued across the pool.
    pub gzip_op_count: AtomicU64,
    /// The worker about to panic first waits (bounded) until a flush barrier
    /// is queued, so the failure provably lands in front of it.
    pub gzip_panic_waits_for_barrier: AtomicBool,
    /// Flush barriers accepted into the raw queue.
    pub gzip_barriers_queued: Watched<AtomicU64>,
    /// A bounded gzip hook wait expired.
    pub gzip_hook_timed_out: AtomicBool,

    // --- io_uring writer (`write::uring_writer`) ---
    /// Dispatch count (1-based) at which the uring writer thread panics.
    /// A dispatch is one handoff operation: a `Raw` chunk, each
    /// piece of a `RawChunks`, a `CopyRange`. The header is not one.
    pub uring_panic_at_dispatch: AtomicU64,
    /// Handoff operations the uring writer thread has dispatched.
    pub uring_dispatch_count: AtomicU64,

    // --- pipelined `PbfWriter` framing (`write::writer`) ---
    /// Sequence number whose framing job panics on the executor.
    pub framing_panic_at: AtomicUsize,
    /// Sequence number whose framing job parks before it frames (counted in
    /// `held_framing_jobs`) until a writer waiting for its framing work has
    /// taken back every dispatch permit but the parked jobs' own (then
    /// `framing_wait_reached` is set), or the test sets `release_framing`.
    /// A writer that returns without that wait, or whose wait stops short of
    /// the parked job's permit, therefore returns while the job is still
    /// parked, which `held_framing_jobs` shows deterministically.
    pub hold_framing_at: AtomicUsize,
    /// Framing jobs currently parked by `hold_framing_at`.
    pub held_framing_jobs: Watched<AtomicU64>,
    /// Framing jobs the producer has dispatched to the executor, counted on
    /// the producer thread before the spawn.
    pub framing_jobs_dispatched: AtomicU64,
    /// Framing jobs whose body ran to its end (a panicking job is not
    /// counted): the last thing such a job does before it drops what it
    /// holds and returns its dispatch permit.
    pub framing_jobs_done: AtomicU64,
    /// Set when a parked framing job was released by a writer that had
    /// taken back every permit not held by a parked job.
    pub framing_wait_reached: AtomicBool,
    /// Test-side release for a parked framing job (cleanup after the
    /// assertion).
    pub release_framing: Watched<AtomicBool>,
    /// A bounded framing hook wait expired.
    pub framing_hook_timed_out: AtomicBool,
    /// Permits the writer waiting for its framing jobs has taken back so
    /// far (`MAX`: none is waiting), and how many it needs.
    framing_wait_taken: Watched<AtomicU64>,
    framing_wait_needed: Watched<AtomicU64>,
    /// Reservations that found the delivery window empty, counted on the
    /// producer thread before it waits for a token. An empty-window
    /// observation, not proof the thread is parked: a producer on an
    /// executor worker waits cooperatively.
    pub window_waits: Watched<AtomicU64>,

    // --- output construction (`BoundOutput::pbf_writer`) ---
    /// Refuse every output backend as a `PlatformUnsupported` setup error,
    /// produced as the constructor's result (so the output's failure
    /// bookkeeping runs as for a real refusal). The source error's message
    /// is [`INJECTED_BACKEND_REFUSAL`].
    pub refuse_output_backend: AtomicBool,
    /// Output constructions refused by `refuse_output_backend`.
    pub output_backend_refusals: AtomicU64,

    // --- diff and derive-changes block-pair work units ---
    /// Unit index (in run order, across kinds) at which a text diff's unit
    /// worker panics.
    pub diff_unit_panic: AtomicUsize,
    /// Unit index at which a derive-changes unit worker panics.
    pub derive_unit_panic: AtomicUsize,
    /// Overrides the planner's unit target bytes when nonzero, so a small
    /// fixture is cut into many units and every cut path runs.
    pub diff_unit_target_bytes: AtomicUsize,
    /// Overrides a derive unit's output chunk bytes when nonzero, so a small
    /// fixture hands each section over in several members.
    pub derive_chunk_bytes: AtomicUsize,
    /// Members a derive unit queues before it spills the rest as if its
    /// channel were full (`usize::MAX`, the default, leaves it to the
    /// channel), so the spill and its ordering run on a small fixture.
    pub derive_spill_after: AtomicUsize,
    /// Overrides a text diff unit's output chunk bytes when nonzero, so a
    /// small fixture hands its text over in many chunks.
    pub diff_text_chunk_bytes: AtomicUsize,
    /// Overrides the text diff's run-wide overflow budget when nonzero, so
    /// the overflow and its fallback to blocking sends run on a small
    /// fixture.
    pub diff_text_overflow_bytes: AtomicUsize,
    /// Overrides the planner's element cut (the claimed elements of
    /// output-bound frames at which a unit closes) when nonzero, so a small
    /// fixture is cut by elements while the byte target stays far away.
    pub diff_unit_lone_elements: AtomicU64,

    // --- apply-changes create runs ---
    /// Overrides the blocks of one create run encoded per executor call
    /// when nonzero, so a run of a few blocks crosses several batches. The
    /// block cut stays at the element cap: this moves scheduling only.
    pub apply_changes_create_batch_blocks: AtomicUsize,

    // --- merge-changes ordered merge (`commands::merge_changes::stream`) ---
    /// Members of input 1 the ordered merge (not `--simplify`) spills before
    /// input 0 is read (`usize::MAX`, the default, leaves it to
    /// scheduling). Input 0's reader waits (bounded) until input 1 is
    /// complete, or its reader is held and that many of its members are in
    /// its spill file; input 1's reader holds before it seals batch `N`
    /// until input 1 is the head. So input 1 has every member spilled and
    /// its file closed when it is promoted if it has at most `N` batches,
    /// and is promoted mid-read with its file open if it has more. Needs
    /// two inputs and two readers; anything else waits out the bound, which
    /// fails the run (`merge_changes_hook_timed_out`).
    pub merge_changes_spill_first: AtomicUsize,
    /// Members of input 1 written to its spill file.
    pub merge_changes_members_spilled: Watched<AtomicU64>,
    /// Set when input 1 is complete while not the head, after its spill
    /// file (if any) was flushed and closed.
    pub merge_changes_spilled_input_complete: Watched<AtomicBool>,
    /// Set when input 1's reader holds before sealing batch `N`.
    pub merge_changes_reader_held: Watched<AtomicBool>,
    /// The head input, published once each promotion (its copy-back and
    /// spill file removal included) has finished.
    pub merge_changes_head: Watched<AtomicUsize>,
    /// Promotions that copied back a spill file still open (the input was
    /// still being read).
    pub merge_changes_copied_open: AtomicU64,
    /// Promotions that copied back a spill file closed when its input
    /// completed.
    pub merge_changes_copied_closed: AtomicU64,
    /// A bounded merge-changes hook wait expired; the waiting reader fails
    /// the run.
    pub merge_changes_hook_timed_out: AtomicBool,

    // --- apply-changes streaming workers ---
    /// Blob sequence number at whose dequeue a streaming worker panics.
    pub apply_changes_panic_at_blob_seq: AtomicU64,
    /// The armed panic first waits (bounded) until the scanner is in its
    /// node-to-way barrier wait, so the failure provably lands there.
    pub apply_changes_panic_waits_for_barrier_wait: AtomicBool,
    /// Blob sequence number at whose dequeue a streaming worker parks
    /// (bounded) until the run is cancelled, holding the node phase open.
    pub apply_changes_park_at_blob_seq: AtomicU64,
    /// Park every streaming worker at each dequeue (bounded) until the
    /// scanner has found the candidate channel full, so a failure armed
    /// alongside provably lands while the scanner is blocked dispatching.
    pub apply_changes_hold_workers_until_dispatch_blocked: AtomicBool,
    /// Set by the scanner when a candidate send finds the channel full.
    pub apply_changes_dispatch_blocked: Watched<AtomicBool>,

    // --- apply-changes node-to-way barrier (`apply_changes::{scanner, drain}`) ---
    /// Hold the scanner's node boundary (bounded) until the drain has
    /// processed every seq below it, so the boundary arrives after the last
    /// node item: the case the drain once needed an idle re-poll for.
    pub apply_changes_late_boundary: AtomicBool,
    /// The drain's next expected seq, published after each advance.
    pub apply_changes_drain_next_seq: Watched<AtomicU64>,
    /// Boundaries the drain received after it had already processed every
    /// seq below them, so a late-boundary test can prove its case ran.
    pub apply_changes_late_boundaries_seen: AtomicU64,
    /// Set by the scanner as it starts waiting for the barrier signal (at
    /// the first way or relation descriptor, or at end of input).
    pub apply_changes_scanner_in_barrier_wait: Watched<AtomicBool>,
    /// Set by the scanner when it finds the barrier channel closed (in its
    /// wait at the first way or relation, or at a poll) and stops walking
    /// before end of input.
    pub apply_changes_scanner_stopped_before_eof: AtomicBool,
    /// Set by the scanner when its header walk reaches end of input.
    pub apply_changes_scanner_reached_eof: AtomicBool,
    /// OsmData headers the scanner has walked, counted as each is read.
    pub apply_changes_scanner_headers_walked: Watched<AtomicU64>,
    /// `apply_changes_scanner_headers_walked` when the scanner's barrier
    /// wait at a way or relation ended (`u64::MAX` if it never did): the
    /// node headers plus the one non-node header that triggered the wait,
    /// so nothing past it is walked or buffered before publication.
    pub apply_changes_headers_walked_at_release: AtomicU64,
    /// Set by the scanner when it dispatches a way or relation descriptor.
    pub apply_changes_non_node_dispatched: AtomicBool,
    /// Hold the scanner (bounded), once its barrier wait has ended, before
    /// it reads the next header, until the drain has begun processing an
    /// item past the node phase: only a scanner that dispatches way work as
    /// it walks, rather than at end of input, gets past this.
    pub apply_changes_hold_walk_until_drain_past_nodes: AtomicBool,
    /// Seq of a dispatched descriptor after which the scanner's barrier
    /// poll is held (bounded) until the drain has returned, so the poll
    /// provably finds the channel closed.
    pub apply_changes_poll_waits_for_drain_exit_after_seq: AtomicU64,
    /// Set by `merge` once the drain has returned (its barrier sender
    /// dropped with it).
    pub apply_changes_drain_returned: Watched<AtomicBool>,
    /// Hold the drain (bounded) just before it merges the coordinate slots
    /// at the barrier until either the scanner is in its barrier wait with
    /// fewer headers walked than `apply_changes_mark_candidate_seq` + 1 (it
    /// has not reached that descriptor), or a worker has finished the
    /// candidate at that seq (its coordinates flushed into its slot): a
    /// scanner that dispatched a late node before publication then has it
    /// in the merge.
    pub apply_changes_merge_waits_for_scanner_or_candidate: AtomicBool,
    /// The candidate seq whose completion `apply_changes_candidate_marked`
    /// records.
    pub apply_changes_mark_candidate_seq: AtomicU64,
    /// Set by a worker once it has finished the candidate at
    /// `apply_changes_mark_candidate_seq`.
    pub apply_changes_candidate_marked: Watched<AtomicBool>,
    /// Hold the drain (bounded) after it has published the map and before
    /// it sends the signal, until the scanner is in its barrier wait, then
    /// record in `apply_changes_non_node_dispatched_before_signal` whether
    /// a way or relation descriptor had already been dispatched.
    pub apply_changes_signal_waits_for_scanner_wait: AtomicBool,
    /// See `apply_changes_signal_waits_for_scanner_wait`.
    pub apply_changes_non_node_dispatched_before_signal: AtomicBool,
    /// The drain publishes a producer failure just before deciding the
    /// missing-node refusal, as if one landed after its per-receive check.
    pub apply_changes_fail_at_refusal: AtomicBool,
    /// The drain cancels the run just before deciding the missing-node
    /// refusal, as if the cancellation landed after its per-receive check.
    pub apply_changes_cancel_at_refusal: AtomicBool,
    /// Set by the drain once it has handled anything past the node phase: a
    /// way or relation item, or its trailing creates.
    pub apply_changes_drain_past_nodes: Watched<AtomicBool>,
    /// Set by the drain once it has sent the barrier signal.
    pub apply_changes_barrier_signalled: AtomicBool,
    /// Run with the owned route (workers pread passthroughs, no splice)
    /// whatever the output backend supports.
    pub apply_changes_force_owned_route: AtomicBool,
    /// A bounded apply-changes hook wait expired: the handshake did not
    /// happen, and a test relying on it proved nothing.
    pub apply_changes_hook_timed_out: AtomicBool,
    /// The streaming worker pool's counters as the last run under this token
    /// left them, by their sidecar names; see
    /// [`apply_changes_worker_counter`](Self::apply_changes_worker_counter).
    apply_changes_worker_counters: Mutex<Vec<(&'static str, u64)>>,

    // --- check ID sets (`commands::check::{refs, verify_ids}`) ---
    /// Positive element IDs `check --refs` or `check --ids --full` inserted
    /// after a parallel phase because they lay above the pre-allocated range.
    pub check_overflow_ids: AtomicU64,

    // --- altw external stage 3 ---
    /// Slot bucket index at which the stage 3 worker claiming it panics.
    pub altw_stage3_panic_at_bucket: AtomicUsize,
    /// Slot bucket index the stage 3 worker claiming it holds, before any of
    /// its work, until the router is aborted (bounded): its slots stay
    /// unpublished, so a stage 4 worker waiting on one stays parked.
    pub altw_stage3_hold_bucket: AtomicUsize,
    /// Set when the held bucket is claimed.
    pub altw_stage3_bucket_held: Watched<AtomicBool>,
    /// Set when the held bucket saw the router aborted before the bound.
    pub altw_stage3_saw_abort: AtomicBool,

    // --- altw external stage 4 ---
    /// Stage 4 fails as it starts, before its ordered pool, once the held
    /// stage 3 bucket is claimed (bounded).
    pub altw_stage4_fail_at_start: AtomicBool,
    /// Decode ordinal at which stage 4's consumer fails with an error, once
    /// the held stage 3 bucket is claimed and a worker has reached its
    /// readiness wait for way blob 0 (bounded).
    pub altw_stage4_fail_consume_at: AtomicUsize,
    /// The same, panicking instead of returning an error.
    pub altw_stage4_panic_consume_at: AtomicUsize,
    /// Decode ordinal at which a stage 4 worker panics as it starts the
    /// task, once the held stage 3 bucket is claimed and another worker has
    /// reached its readiness wait for way blob 0 (bounded).
    pub altw_stage4_panic_work_at: AtomicUsize,
    /// Set by a stage 4 worker just before it waits on the router for way
    /// blob 0.
    pub altw_stage4_way0_entered: Watched<AtomicBool>,
    /// A bounded altw hook wait expired: the handshake did not happen, and
    /// a test relying on it proved nothing.
    pub altw_hook_timed_out: AtomicBool,

    // --- altw external stage 1 ---
    /// The stage-1 fleet rendezvous; see
    /// [`arm_altw_stage1_fleet`](Self::arm_altw_stage1_fleet).
    altw_stage1_fleet: Mutex<FleetRendezvous>,

    // --- geocode builder pass 3 ---
    /// Street way index at which the Stage A streets worker panics.
    pub geocode_streets_panic_at_way: AtomicU32,
    /// Address point index Pass 3 files under the wrong cell at one level;
    /// see [`arm_geocode_misfile_address`](Self::arm_geocode_misfile_address).
    geocode_misfile_address_at: AtomicUsize,
    geocode_misfile_address_coarse: AtomicBool,

    // --- pinned inputs (`read::input::PinnedInput`) ---
    /// Pin every input taken under this token as on a host whose
    /// `/proc/self/fd` cannot reopen it: later opens go through the path and
    /// refuse a replaced file. Read once, when the pin is taken.
    pub force_pin_path_reopen: AtomicBool,
    input_replacement: Mutex<Option<InputReplacement>>,

    // --- output publication (`output::publish_all`) ---
    /// Faults every publication under this token injects, read once as each
    /// publication starts; see [`arm_publish_fault`](Self::arm_publish_fault).
    publish_faults: Mutex<Vec<PublishFault>>,
    /// Hears every [`PublishStep`]; see [`on_publish_step`](Self::on_publish_step).
    publish_observer: OnceLock<PublishObserver>,

    // --- sealing (`host::Invocation::seal`) ---
    /// Hears every [`SealStep`]; see [`on_seal_step`](Self::on_seal_step).
    seal_observer: OnceLock<SealObserver>,
}

/// A publication observer running under the invocation execution lock.
/// See [`FaultPlan::on_publish_step`] for permitted blocking dependencies.
type PublishObserver = Box<dyn Fn(PublishStep) + Send + Sync>;

/// A seal observer; it may block or panic.
type SealObserver = Box<dyn Fn(SealStep) + Send + Sync>;

/// The points of a seal's finalization a test can observe, on the thread
/// that won the seal's exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SealStep {
    /// Before the snapshot is taken: a panic here fails the finalization,
    /// and the stream stays open.
    Snapshot,
    /// After the snapshot is committed and every sealing emission written,
    /// before the terminal record: a panic here ends the stream `failed`,
    /// and the invocation is sealed regardless.
    Stream,
}

/// A geocode cell level, for [`FaultPlan::arm_geocode_misfile_address`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeocodeCellLevel {
    Fine,
    Coarse,
}

/// An armed input replacement and what it observed; see
/// [`FaultPlan::arm_input_replacement`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputReplacement {
    /// The pinned input's path, as the command was given it.
    pub target: PathBuf,
    /// The file renamed over `target`.
    pub replacement: PathBuf,
    /// The verified open after which the replacement happens (1-based).
    pub after: u64,
    /// Opens of `target` attempted through its pin.
    pub attempts: u64,
    /// Opens of `target` that returned a descriptor verified against the
    /// pin.
    pub verified: u64,
    /// Opens of `target` refused because its path named another file.
    pub refused: u64,
    /// Whether the rename happened.
    pub fired: bool,
    /// Attempts made after the rename.
    pub attempts_after_fired: u64,
}

/// The message of the source error behind an injected backend refusal, so a
/// test can tell it from a genuine platform refusal.
pub const INJECTED_BACKEND_REFUSAL: &str = "test-hooks: injected output backend refusal";

/// The message of the injected I/O error behind a failed pool-thread spawn
/// ([`FaultPlan::built_pool_spawn_fail_at`]).
pub const INJECTED_POOL_SPAWN_FAILURE: &str = "test-hooks: injected pool thread spawn failure";

/// How an armed stage-1 fleet rendezvous ended; see
/// [`FaultPlan::arm_altw_stage1_fleet`]. Sticky: the first outcome stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FleetOutcome {
    /// Every expected party arrived while every earlier one was still
    /// parked: all their descriptors were open at once.
    Complete,
    /// The bound passed first (a party failed or never came), or the run was
    /// cancelled: the overlap did not happen, and nothing was proved.
    Expired,
    /// A party arrived twice or a worker outside the armed set arrived.
    Unexpected,
}

/// State of the stage-1 fleet rendezvous, under one lock so the overlap is
/// decided at one instant.
struct FleetRendezvous {
    /// `Some(n)`: armed for stage-1 workers `0..n`.
    workers: Option<usize>,
    /// The relation scan is a party too.
    relation: bool,
    arrived_workers: Vec<usize>,
    relation_arrived: bool,
    outcome: Option<FleetOutcome>,
}

/// A party to the stage-1 fleet rendezvous.
#[derive(Clone, Copy, Debug)]
pub(crate) enum FleetParty {
    /// A stage-1 worker, after it opened its id-shard files.
    Worker(usize),
    /// The relation-member scan, after it opened its input.
    Relation,
}

struct DecodeGate {
    armed_seq: Option<usize>,
    ready: bool,
    released: bool,
}

impl FaultPlan {
    /// A plan with every fault disarmed and every counter at zero.
    ///
    /// Only an invocation builds one: the seal is constructible in `host`
    /// alone, there is no `Default`, and the private `bell` field rules out a
    /// struct literal. So every plan is the one an `Invocation` carries, and
    /// fault state is as process-global as that invocation is: a static
    /// invocation is refused by the static inventory in library source and
    /// by the `no-static-test-hosts` text rule in test code.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "one initializer per field, a flat list that grows with the plan"
    )]
    pub(crate) fn new(_seal: crate::host::PlanSeal) -> Self {
        Self {
            bell: Bell::new(),
            decode_gate: Mutex::new(DecodeGate {
                armed_seq: None,
                ready: false,
                released: false,
            }),
            decode_gate_cv: Condvar::new(),
            built_pool_spawn_fail_at: AtomicUsize::new(usize::MAX),
            hold_built_pool_threads: AtomicBool::new(false),
            built_pool_threads_held: Watched::<AtomicU64>::new(0),
            release_built_pool_threads: Watched::<AtomicBool>::new(false),
            built_pool_exit_hooks_done: AtomicU64::new(0),
            built_pool_hook_timed_out: AtomicBool::new(false),
            built_pool_joins_started: Watched::<AtomicU64>::new(0),
            publication_lock_waits: Watched::<AtomicU64>::new(0),
            force_linear_region_grid: AtomicBool::new(false),
            region_grids_declined: AtomicU64::new(0),
            pool_worker_panic: AtomicU64::new(u64::MAX),
            hold_other_pool_workers: AtomicBool::new(false),
            held_pool_workers: Watched::<AtomicU64>::new(0),
            release_pool_workers: Watched::<AtomicBool>::new(false),
            join_reached_held_worker: Watched::<AtomicBool>::new(false),
            live_pool_workers: Watched::<AtomicU64>::new(0),
            pool_op_count: AtomicU64::new(0),
            pool_hook_timed_out: AtomicBool::new(false),
            gzip_panic_at_op: AtomicU64::new(u64::MAX),
            gzip_op_count: AtomicU64::new(0),
            gzip_panic_waits_for_barrier: AtomicBool::new(false),
            gzip_barriers_queued: Watched::<AtomicU64>::new(0),
            gzip_hook_timed_out: AtomicBool::new(false),
            uring_panic_at_dispatch: AtomicU64::new(u64::MAX),
            uring_dispatch_count: AtomicU64::new(0),
            framing_panic_at: AtomicUsize::new(usize::MAX),
            hold_framing_at: AtomicUsize::new(usize::MAX),
            held_framing_jobs: Watched::<AtomicU64>::new(0),
            framing_jobs_dispatched: AtomicU64::new(0),
            framing_jobs_done: AtomicU64::new(0),
            framing_wait_reached: AtomicBool::new(false),
            release_framing: Watched::<AtomicBool>::new(false),
            framing_hook_timed_out: AtomicBool::new(false),
            framing_wait_taken: Watched::<AtomicU64>::new(u64::MAX),
            framing_wait_needed: Watched::<AtomicU64>::new(u64::MAX),
            window_waits: Watched::<AtomicU64>::new(0),
            refuse_output_backend: AtomicBool::new(false),
            output_backend_refusals: AtomicU64::new(0),
            diff_unit_panic: AtomicUsize::new(usize::MAX),
            derive_unit_panic: AtomicUsize::new(usize::MAX),
            diff_unit_target_bytes: AtomicUsize::new(0),
            derive_chunk_bytes: AtomicUsize::new(0),
            derive_spill_after: AtomicUsize::new(usize::MAX),
            diff_text_chunk_bytes: AtomicUsize::new(0),
            diff_text_overflow_bytes: AtomicUsize::new(0),
            diff_unit_lone_elements: AtomicU64::new(0),
            apply_changes_create_batch_blocks: AtomicUsize::new(0),
            merge_changes_spill_first: AtomicUsize::new(usize::MAX),
            merge_changes_members_spilled: Watched::<AtomicU64>::new(0),
            merge_changes_spilled_input_complete: Watched::<AtomicBool>::new(false),
            merge_changes_reader_held: Watched::<AtomicBool>::new(false),
            merge_changes_head: Watched::<AtomicUsize>::new(0),
            merge_changes_copied_open: AtomicU64::new(0),
            merge_changes_copied_closed: AtomicU64::new(0),
            merge_changes_hook_timed_out: AtomicBool::new(false),
            apply_changes_panic_at_blob_seq: AtomicU64::new(u64::MAX),
            apply_changes_panic_waits_for_barrier_wait: AtomicBool::new(false),
            apply_changes_park_at_blob_seq: AtomicU64::new(u64::MAX),
            apply_changes_hold_workers_until_dispatch_blocked: AtomicBool::new(false),
            apply_changes_dispatch_blocked: Watched::<AtomicBool>::new(false),
            apply_changes_late_boundary: AtomicBool::new(false),
            apply_changes_drain_next_seq: Watched::<AtomicU64>::new(0),
            apply_changes_late_boundaries_seen: AtomicU64::new(0),
            apply_changes_scanner_in_barrier_wait: Watched::<AtomicBool>::new(false),
            apply_changes_scanner_stopped_before_eof: AtomicBool::new(false),
            apply_changes_scanner_reached_eof: AtomicBool::new(false),
            apply_changes_scanner_headers_walked: Watched::<AtomicU64>::new(0),
            apply_changes_headers_walked_at_release: AtomicU64::new(u64::MAX),
            apply_changes_non_node_dispatched: AtomicBool::new(false),
            apply_changes_hold_walk_until_drain_past_nodes: AtomicBool::new(false),
            apply_changes_poll_waits_for_drain_exit_after_seq: AtomicU64::new(u64::MAX),
            apply_changes_drain_returned: Watched::<AtomicBool>::new(false),
            apply_changes_merge_waits_for_scanner_or_candidate: AtomicBool::new(false),
            apply_changes_mark_candidate_seq: AtomicU64::new(u64::MAX),
            apply_changes_candidate_marked: Watched::<AtomicBool>::new(false),
            apply_changes_signal_waits_for_scanner_wait: AtomicBool::new(false),
            apply_changes_non_node_dispatched_before_signal: AtomicBool::new(false),
            apply_changes_fail_at_refusal: AtomicBool::new(false),
            apply_changes_cancel_at_refusal: AtomicBool::new(false),
            apply_changes_drain_past_nodes: Watched::<AtomicBool>::new(false),
            apply_changes_barrier_signalled: AtomicBool::new(false),
            apply_changes_force_owned_route: AtomicBool::new(false),
            apply_changes_hook_timed_out: AtomicBool::new(false),
            apply_changes_worker_counters: Mutex::new(Vec::new()),
            check_overflow_ids: AtomicU64::new(0),
            altw_stage3_panic_at_bucket: AtomicUsize::new(usize::MAX),
            altw_stage3_hold_bucket: AtomicUsize::new(usize::MAX),
            altw_stage3_bucket_held: Watched::<AtomicBool>::new(false),
            altw_stage3_saw_abort: AtomicBool::new(false),
            altw_stage4_fail_at_start: AtomicBool::new(false),
            altw_stage4_fail_consume_at: AtomicUsize::new(usize::MAX),
            altw_stage4_panic_consume_at: AtomicUsize::new(usize::MAX),
            altw_stage4_panic_work_at: AtomicUsize::new(usize::MAX),
            altw_stage4_way0_entered: Watched::<AtomicBool>::new(false),
            altw_hook_timed_out: AtomicBool::new(false),
            altw_stage1_fleet: Mutex::new(FleetRendezvous {
                workers: None,
                relation: false,
                arrived_workers: Vec::new(),
                relation_arrived: false,
                outcome: None,
            }),
            geocode_streets_panic_at_way: AtomicU32::new(u32::MAX),
            geocode_misfile_address_at: AtomicUsize::new(usize::MAX),
            geocode_misfile_address_coarse: AtomicBool::new(false),
            force_pin_path_reopen: AtomicBool::new(false),
            input_replacement: Mutex::new(None),
            publish_faults: Mutex::new(Vec::new()),
            publish_observer: OnceLock::new(),
            seal_observer: OnceLock::new(),
        }
    }

    // ---- sealing ----------------------------------------------------------

    /// Run `observer` on the finalizing thread at every [`SealStep`] of a
    /// seal of this token's invocation. It may block (concurrent sealers are
    /// refused meanwhile) or panic (see each step). Panics if an observer
    /// is already installed.
    pub fn on_seal_step(&self, observer: impl Fn(SealStep) + Send + Sync + 'static) {
        assert!(
            self.seal_observer.set(Box::new(observer)).is_ok(),
            "a seal observer is already installed"
        );
    }

    /// Called by the finalizing seal at each step.
    pub(crate) fn seal_step(&self, step: SealStep) {
        if let Some(observer) = self.seal_observer.get() {
            observer(step);
        }
    }

    // ---- output publication -------------------------------------------

    /// Make every publication under this token inject `fault`. Arm before
    /// publishing: each publication reads the armed set once, as it starts.
    pub fn arm_publish_fault(&self, fault: PublishFault) {
        self.publish_faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(fault);
    }

    /// The armed publication faults, for a publication starting now.
    pub(crate) fn publish_faults(&self) -> Vec<PublishFault> {
        self.publish_faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Run `observer` on the publishing thread after every namespace step
    /// of every publication under this token. The invocation's publication
    /// execution lock is held; the publication log lock is not held.
    /// Same-thread publication re-entry under this invocation is refused.
    /// An observer must not wait for another thread to publish under this
    /// invocation, or for any work that depends on acquiring its execution
    /// lock: that forms a deadlock. It may wait for an independent test
    /// controller that guarantees release on every exit path. Cancellation
    /// does not release a parked observer; an intentionally permanent park
    /// requires a killable child process.
    ///
    /// Panics if an observer is already installed.
    pub fn on_publish_step(&self, observer: impl Fn(PublishStep) + Send + Sync + 'static) {
        assert!(
            self.publish_observer.set(Box::new(observer)).is_ok(),
            "a publication observer is already installed"
        );
    }

    /// Called by publication after each step.
    pub(crate) fn publish_step(&self, step: PublishStep) {
        if let Some(observer) = self.publish_observer.get() {
            observer(step);
        }
    }

    // ---- pinned inputs ----------------------------------------------------

    fn replacement(&self) -> MutexGuard<'_, Option<InputReplacement>> {
        self.input_replacement
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Make the geocode builder's Pass 3 file address point `index` at
    /// `level` under its antipode's cell instead of its own, as a broken cell
    /// assignment would, leaving its entry at the other level and every
    /// street entry in place.
    pub fn arm_geocode_misfile_address(&self, index: usize, level: GeocodeCellLevel) {
        self.geocode_misfile_address_coarse
            .store(level == GeocodeCellLevel::Coarse, Ordering::Relaxed);
        self.geocode_misfile_address_at
            .store(index, Ordering::Relaxed);
    }

    /// The armed address misfiling, if any.
    pub(crate) fn geocode_misfile_address(&self) -> Option<(usize, GeocodeCellLevel)> {
        let index = self.geocode_misfile_address_at.load(Ordering::Relaxed);
        if index == usize::MAX {
            return None;
        }
        let level = if self.geocode_misfile_address_coarse.load(Ordering::Relaxed) {
            GeocodeCellLevel::Coarse
        } else {
            GeocodeCellLevel::Fine
        };
        Some((index, level))
    }

    /// Rename `replacement` over `target` right after the `after`-th open of
    /// the pinned input `target` (matched by the path the pin was taken
    /// with) returns a verified descriptor, and count every open of it.
    /// While armed, each open of `target` - open, verification, counting and
    /// the rename - runs under one lock, so concurrent opens fall clearly
    /// before or after the replacement. A failed rename fails that open.
    ///
    /// # Panics
    /// `after` is zero: the replacement follows an open, and a replacement
    /// before the pin is taken is not a case a pin addresses.
    pub fn arm_input_replacement(&self, target: &Path, replacement: &Path, after: u64) {
        assert!(
            after >= 1,
            "arm_input_replacement: `after` counts opens from 1"
        );
        *self.replacement() = Some(InputReplacement {
            target: target.to_path_buf(),
            replacement: replacement.to_path_buf(),
            after,
            attempts: 0,
            verified: 0,
            refused: 0,
            fired: false,
            attempts_after_fired: 0,
        });
    }

    /// The armed replacement and its counts, if one was armed.
    pub fn input_replacement(&self) -> Option<InputReplacement> {
        self.replacement().clone()
    }

    /// Whether pins taken under this token must reopen through the path.
    pub(crate) fn forces_pin_path_reopen(&self) -> bool {
        self.force_pin_path_reopen.load(Ordering::Relaxed)
    }

    /// Called by a pinned input for every later open of `target`; `open`
    /// opens and verifies.
    pub(crate) fn pinned_open<T>(
        &self,
        target: &Path,
        open: impl FnOnce() -> crate::Result<T>,
    ) -> crate::Result<T> {
        let mut guard = self.replacement();
        let Some(armed) = guard.as_mut().filter(|armed| armed.target == target) else {
            drop(guard);
            return open();
        };
        armed.attempts += 1;
        if armed.fired {
            armed.attempts_after_fired += 1;
        }
        let result = open();
        match &result {
            Ok(_) => {
                armed.verified += 1;
                if !armed.fired && armed.verified == armed.after {
                    // The fault itself: a file renamed over the input
                    // mid-run, as an atomic replace by a downloader does.
                    #[allow(clippy::disallowed_methods)]
                    std::fs::rename(&armed.replacement, &armed.target).map_err(|e| {
                        crate::Error::from(e).context(format!(
                            "test-hooks: renaming {} over the input {}",
                            armed.replacement.display(),
                            armed.target.display()
                        ))
                    })?;
                    armed.fired = true;
                }
            }
            Err(e) if matches!(e.kind(), crate::ErrorKind::InputReplaced) => armed.refused += 1,
            Err(_) => {}
        }
        result
    }

    // ---- read pipeline decode gate -------------------------------------

    fn gate(&self) -> MutexGuard<'_, DecodeGate> {
        self.decode_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Park the pipelined read's decode task for `seq` until
    /// [`release_blocked_decode`](Self::release_blocked_decode). Arm before
    /// the read starts; once released, the gate stays open.
    ///
    /// The gate matches `seq` alone, not a pass: every pipelined read under
    /// this token parks at that `seq` until release, whether it delivers
    /// plain blocks or runs a command transform (digest, getid, getparents,
    /// tags-filter, the add-locations-to-ways fallback). In a command that
    /// reads more than once, the first pipelined pass that dispatches the
    /// armed `seq` parks, whichever pass the test meant to target.
    pub fn arm_block_decode(&self, seq: usize) {
        let mut g = self.gate();
        g.armed_seq = Some(seq);
        g.ready = false;
        g.released = false;
    }

    /// True once the armed decode has reached the gate and parked.
    pub fn blocked_decode_ready(&self) -> bool {
        self.gate().ready
    }

    /// Let the parked decode (and any later one for the armed seq) go. Safe
    /// to call on every exit path, armed or not.
    pub fn release_blocked_decode(&self) {
        self.gate().released = true;
        self.decode_gate_cv.notify_all();
    }

    /// Called by a decode task before it decodes `seq`. Parks while `seq` is
    /// armed and unreleased, at most [`HOOK_WAIT`]; past that it panics with a
    /// named message (inside the task's `catch_unwind`, so the read fails
    /// rather than the process).
    pub(crate) fn maybe_block_decode(&self, seq: usize) {
        let mut g = self.gate();
        if g.armed_seq != Some(seq) {
            return;
        }
        g.ready = true;
        self.decode_gate_cv.notify_all();
        // Gate lock, then bell lock: no wait holds the bell's lock while
        // it evaluates a condition, so this order cannot invert.
        self.bell.ring();
        let deadline = Instant::now() + HOOK_WAIT;
        while !g.released {
            if Instant::now() >= deadline {
                drop(g);
                panic!(
                    "test-hooks: read-pipeline decode for seq {seq} parked for {HOOK_WAIT:?} \
                     and was never released; the test must call release_blocked_decode()"
                );
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            g = self
                .decode_gate_cv
                // wait-deliberate: hook wait under its deadline; expiry panics
                .wait_timeout(g, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    // ---- parallel pwrite writer ----------------------------------------

    /// The one wait: evaluates `cond` until it yields, at most
    /// [`HOOK_WAIT`] from now (`None` past it). See the module docs for why
    /// a ring is never lost.
    fn wait<R>(&self, wake: Wake, mut cond: impl FnMut() -> Option<R>) -> Option<R> {
        let deadline = Instant::now() + HOOK_WAIT;
        loop {
            let seen = *self.bell.rings();
            if let Some(reason) = cond() {
                return Some(reason);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let remaining = deadline - now;
            let timeout = match wake {
                Wake::Rung => remaining,
                Wake::Polled => remaining.min(HOOK_TICK),
            };
            self.bell.sleep_past(seen, timeout);
        }
    }

    /// A hook's bounded wait for `cond`; past the bound it sets
    /// `timed_out` and returns, so the test that armed it can fail named.
    fn wait_until(&self, timed_out: &AtomicBool, wake: Wake, cond: impl Fn() -> bool) {
        if self.wait(wake, || cond().then_some(())).is_none() {
            timed_out.store(true, Ordering::SeqCst);
        }
    }

    /// A test's wait for `cond`, at most `TEST_HOOK_WAIT`: false if it
    /// never held, for the caller's assertion to name. It wakes at once on
    /// any write to a [`Watched`] field or a tripped token, and re-evaluates
    /// every `TEST_HOOK_TICK` besides, so `cond` may read anything.
    pub fn wait_for(&self, cond: impl Fn() -> bool) -> bool {
        self.wait(Wake::Polled, || cond().then_some(())).is_some()
    }

    /// Wait for `token`, the invocation this plan belongs to, to trip, at
    /// most `TEST_HOOK_WAIT`: false if it never did. Rung, never polled:
    /// [`Invocation::cancel`](crate::host::Invocation::cancel) rings the
    /// bell after the trip, so the wait ends at the trip itself.
    pub fn wait_cancelled(&self, token: &crate::host::Invocation) -> bool {
        assert!(
            std::ptr::eq(token.faults(), self),
            "a plan waits only for its own invocation's token"
        );
        self.wait(Wake::Rung, || token.is_cancelled().then_some(()))
            .is_some()
    }

    /// Ring the bell for a transition outside the plan that some wait
    /// reads: the invocation's token tripping.
    pub(crate) fn ring(&self) {
        self.bell.ring();
    }

    // ---- library-built rayon pools ---------------------------------------

    /// Called by `JoinedPool::build` before it spawns pool thread `index`;
    /// the armed index fails as the OS refusing the thread would.
    pub(crate) fn on_built_pool_spawn(&self, index: usize) -> std::io::Result<()> {
        if index == self.built_pool_spawn_fail_at.load(Ordering::SeqCst) {
            // `WouldBlock`: what a thread spawn refused for resources
            // (`EAGAIN` from `pthread_create`) reports.
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                INJECTED_POOL_SPAWN_FAILURE,
            ));
        }
        Ok(())
    }

    /// Called by a publication that found the execution lock held, before
    /// it blocks on it.
    pub(crate) fn on_publication_lock_wait(&self) {
        self.publication_lock_waits.fetch_add(1, &self.bell);
    }

    /// Called by a pool's join guard as it drops, before its first join.
    pub(crate) fn on_built_pool_join(&self) {
        self.built_pool_joins_started.fetch_add(1, &self.bell);
    }

    /// Release pool threads parked by `hold_built_pool_threads`. Safe to
    /// call on every exit path, armed or not.
    pub fn release_built_pool_threads(&self) {
        self.release_built_pool_threads.store(true, &self.bell);
    }

    /// Called by a pool thread after its rayon main loop returns, as the last
    /// thing its main function does.
    pub(crate) fn on_built_pool_thread_exit(&self) {
        if self.hold_built_pool_threads.load(Ordering::SeqCst) {
            self.built_pool_threads_held.fetch_add(1, &self.bell);
            self.wait_until(&self.built_pool_hook_timed_out, Wake::Rung, || {
                self.release_built_pool_threads.load(Ordering::SeqCst)
            });
        }
        self.built_pool_exit_hooks_done
            .fetch_add(1, Ordering::SeqCst);
    }

    /// Called by a parallel-writer pool worker after it pops an op and
    /// before it runs it.
    pub(crate) fn on_pool_op(&self, worker: usize) {
        self.pool_op_count.fetch_add(1, Ordering::SeqCst);
        let hold = self.hold_other_pool_workers.load(Ordering::SeqCst);
        if worker as u64 == self.pool_worker_panic.load(Ordering::SeqCst) {
            if hold {
                self.wait_until(&self.pool_hook_timed_out, Wake::Rung, || {
                    self.held_pool_workers.load(Ordering::SeqCst) >= 1
                });
            }
            panic!("test-hooks: parallel_writer pool worker {worker} panicking");
        }
        if hold {
            self.held_pool_workers.fetch_add(1, &self.bell);
            self.wait_until(&self.pool_hook_timed_out, Wake::Rung, || {
                self.join_reached_held_worker.load(Ordering::SeqCst)
                    || self.release_pool_workers.load(Ordering::SeqCst)
            });
            self.held_pool_workers.fetch_sub(1, &self.bell);
        }
    }

    /// Release workers parked by `hold_other_pool_workers`. Safe to call on
    /// every exit path, armed or not.
    pub fn release_pool_workers(&self) {
        self.release_pool_workers.store(true, &self.bell);
    }

    /// Called by the parallel writer's owner just before it joins `worker`.
    pub(crate) fn before_pool_join(&self, worker: usize) {
        if worker as u64 != self.pool_worker_panic.load(Ordering::SeqCst)
            && self.held_pool_workers.load(Ordering::SeqCst) > 0
        {
            self.join_reached_held_worker.store(true, &self.bell);
        }
    }

    /// `live_pool_workers` membership for one pool worker thread, released
    /// on drop (the unwind included).
    pub(crate) fn live_pool_worker(&self) -> LivePoolWorker<'_> {
        self.live_pool_workers.fetch_add(1, &self.bell);
        LivePoolWorker(self)
    }

    // ---- OSC gzip pool --------------------------------------------------

    /// Called by a gzip worker after it dequeues chunk `seq`, before it
    /// compresses it.
    pub(crate) fn on_gzip_compress(&self, seq: usize) {
        let n = self.gzip_op_count.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.gzip_panic_at_op.load(Ordering::SeqCst) {
            if self.gzip_panic_waits_for_barrier.load(Ordering::SeqCst) {
                self.wait_until(&self.gzip_hook_timed_out, Wake::Rung, || {
                    self.gzip_barriers_queued.load(Ordering::SeqCst) != 0
                });
            }
            panic!("test-hooks: parallel_gzip pool op #{n} (seq {seq}) panicking");
        }
    }

    /// Called by the gzip writer's `flush` once its barrier is queued.
    pub(crate) fn on_gzip_barrier_queued(&self) {
        self.gzip_barriers_queued.fetch_add(1, &self.bell);
    }

    // ---- io_uring writer -------------------------------------------------

    /// Called by the uring writer thread per handoff operation.
    #[cfg(feature = "linux-io-uring")]
    pub(crate) fn on_uring_dispatch(&self) {
        let n = self.uring_dispatch_count.fetch_add(1, Ordering::Relaxed) + 1;
        if n == self.uring_panic_at_dispatch.load(Ordering::Relaxed) {
            panic!("test-hooks: uring_writer dispatch #{n} panicking");
        }
    }

    // ---- single-shot selectors -------------------------------------------

    /// Whether the pipelined writer's framing job for `seq` panics.
    pub(crate) fn framing_panics_at(&self, seq: usize) -> bool {
        self.framing_panic_at.load(Ordering::Relaxed) == seq
    }

    // ---- pipelined `PbfWriter` framing barrier ---------------------------

    /// Called by the producer just before it dispatches a framing job.
    pub(crate) fn on_framing_dispatch(&self) {
        self.framing_jobs_dispatched.fetch_add(1, Ordering::SeqCst);
    }

    /// Called by the framing job for `seq` before it frames.
    pub(crate) fn on_framing_job(&self, seq: usize) {
        if seq != self.hold_framing_at.load(Ordering::SeqCst) {
            return;
        }
        self.held_framing_jobs.fetch_add(1, &self.bell);
        // A writer that has taken back every other permit wins over a test
        // release seen in the same evaluation.
        let ended = self.wait(Wake::Rung, || {
            let taken = self.framing_wait_taken.load(Ordering::SeqCst);
            let held = self.held_framing_jobs.load(Ordering::SeqCst);
            if taken != u64::MAX
                && taken.saturating_add(held) >= self.framing_wait_needed.load(Ordering::SeqCst)
            {
                return Some(FramingRelease::Reached);
            }
            self.release_framing
                .load(Ordering::SeqCst)
                .then_some(FramingRelease::Released)
        });
        match ended {
            // Published before the job counts as no longer parked.
            Some(FramingRelease::Reached) => {
                self.framing_wait_reached.store(true, Ordering::SeqCst);
            }
            Some(FramingRelease::Released) => {}
            None => self.framing_hook_timed_out.store(true, Ordering::SeqCst),
        }
        self.held_framing_jobs.fetch_sub(1, &self.bell);
    }

    /// Release a framing job parked by `hold_framing_at`. Safe to call on
    /// every exit path, armed or not.
    pub fn release_framing(&self) {
        self.release_framing.store(true, &self.bell);
    }

    /// Called by a framing job whose body ran to its end, just before it
    /// drops what it holds and returns its dispatch permit.
    pub(crate) fn on_framing_job_done(&self) {
        self.framing_jobs_done.fetch_add(1, Ordering::SeqCst);
    }

    /// Called by a writer waiting for its framing jobs: on entry
    /// (`taken` 0) and after each permit it takes back, of `needed`.
    pub(crate) fn on_framing_wait(&self, taken: usize, needed: usize) {
        self.framing_wait_needed.store(needed as u64, &self.bell);
        self.framing_wait_taken.store(taken as u64, &self.bell);
    }

    /// Called by a producer whose reservation found the delivery window
    /// empty, before it waits for a token.
    pub(crate) fn on_window_wait(&self) {
        self.window_waits.fetch_add(1, &self.bell);
    }

    /// Called by a writer whose wait for its framing jobs is over.
    pub(crate) fn after_framing_wait(&self) {
        self.framing_wait_taken.store(u64::MAX, &self.bell);
    }

    /// Whether the multi-extract region grid is declined (counted).
    pub(crate) fn forces_linear_region_grid(&self) -> bool {
        let declined = self.force_linear_region_grid.load(Ordering::Relaxed);
        if declined {
            self.region_grids_declined.fetch_add(1, Ordering::Relaxed);
        }
        declined
    }

    /// The injected backend refusal, when armed.
    pub(crate) fn output_backend_refusal(&self) -> Option<crate::Error> {
        if !self.refuse_output_backend.load(Ordering::Relaxed) {
            return None;
        }
        self.output_backend_refusals.fetch_add(1, Ordering::Relaxed);
        Some(crate::Error::platform_unsupported(
            "direct-io",
            std::io::Error::new(std::io::ErrorKind::Unsupported, INJECTED_BACKEND_REFUSAL),
        ))
    }

    // ---- merge-changes ordered merge -------------------------------------

    /// A bounded wait for the `merge_changes_spill_first` handshake that
    /// fails the run when the bound expires, so a run that did not get the
    /// ordering it was armed for (a subprocess included) cannot pass.
    fn merge_changes_wait(
        &self,
        stopped: impl Fn() -> bool,
        cond: impl Fn() -> bool,
    ) -> crate::Result<()> {
        let timed_out = &self.merge_changes_hook_timed_out;
        // Polled: `stopped` reads the ledger's halt, which `Ledger::stop`
        // sets without tripping the token or ringing the plan.
        self.wait_until(timed_out, Wake::Polled, || stopped() || cond());
        if timed_out.load(Ordering::SeqCst) {
            return Err(crate::Error::internal(format!(
                "test-hooks: the merge-changes spill-first handshake did not happen within \
                 {HOOK_WAIT:?} (it needs two inputs and two readers)"
            )));
        }
        Ok(())
    }

    /// Called by an ordered merge's reader of `input` before it reads;
    /// `stopped` is true once the run is failing or cancelled.
    pub(crate) fn before_merge_changes_read(
        &self,
        input: usize,
        stopped: impl Fn() -> bool,
    ) -> crate::Result<()> {
        let first = self.merge_changes_spill_first.load(Ordering::SeqCst);
        if input != 0 || first == usize::MAX {
            return Ok(());
        }
        self.merge_changes_wait(stopped, || {
            self.merge_changes_spilled_input_complete
                .load(Ordering::SeqCst)
                || (self.merge_changes_reader_held.load(Ordering::SeqCst)
                    && self.merge_changes_members_spilled.load(Ordering::SeqCst) >= first as u64)
        })
    }

    /// Called by an ordered merge's reader of `input` before it seals batch
    /// `seq`.
    pub(crate) fn before_merge_changes_seal(
        &self,
        input: usize,
        seq: usize,
        stopped: impl Fn() -> bool,
    ) -> crate::Result<()> {
        if input != 1 || seq != self.merge_changes_spill_first.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.merge_changes_reader_held.store(true, &self.bell);
        self.merge_changes_wait(stopped, || {
            self.merge_changes_head.load(Ordering::SeqCst) >= 1
        })
    }

    /// Called by the coordinator after it wrote a member of `input` to
    /// that input's spill file.
    pub(crate) fn on_merge_changes_spilled(&self, input: usize) {
        if input == 1 {
            self.merge_changes_members_spilled.fetch_add(1, &self.bell);
        }
    }

    /// Called by the coordinator once `input`, not the head, is complete
    /// and its spill file (if any) flushed and closed.
    pub(crate) fn on_merge_changes_complete(&self, input: usize) {
        if input == 1 {
            self.merge_changes_spilled_input_complete
                .store(true, &self.bell);
        }
    }

    /// Called by the coordinator once `head` has been promoted, after
    /// copying back a spill file that was still `open` or already closed
    /// (`None`: there was none).
    pub(crate) fn on_merge_changes_promoted(&self, head: usize, copied: Option<bool>) {
        match copied {
            Some(true) => {
                self.merge_changes_copied_open
                    .fetch_add(1, Ordering::SeqCst);
            }
            Some(false) => {
                self.merge_changes_copied_closed
                    .fetch_add(1, Ordering::SeqCst);
            }
            None => {}
        }
        self.merge_changes_head.store(head, &self.bell);
    }

    // ---- apply-changes ---------------------------------------------------

    /// Called by a streaming worker right after it dequeues blob `seq`:
    /// holds, parks or panics it as armed. `cancelled` reads the run's token.
    pub(crate) fn on_apply_changes_dequeue(
        &self,
        seq: u64,
        worker: usize,
        cancelled: impl Fn() -> bool,
    ) {
        let timed_out = &self.apply_changes_hook_timed_out;
        if self
            .apply_changes_hold_workers_until_dispatch_blocked
            .load(Ordering::SeqCst)
        {
            self.wait_until(timed_out, Wake::Rung, || {
                self.apply_changes_dispatch_blocked.load(Ordering::SeqCst)
            });
        }
        if seq == self.apply_changes_park_at_blob_seq.load(Ordering::SeqCst) {
            // Rung: tripping the token rings the plan.
            self.wait_until(timed_out, Wake::Rung, &cancelled);
        }
        if seq == self.apply_changes_panic_at_blob_seq.load(Ordering::SeqCst) {
            if self
                .apply_changes_panic_waits_for_barrier_wait
                .load(Ordering::SeqCst)
            {
                self.wait_until(timed_out, Wake::Rung, || {
                    self.apply_changes_scanner_in_barrier_wait
                        .load(Ordering::SeqCst)
                });
            }
            panic!("test-hooks: panic_at_blob_seq={seq} triggered in worker {worker}");
        }
    }

    /// Called by the scanner just before it sends its node boundary with
    /// exclusive end `node_end_seq`.
    pub(crate) fn before_apply_changes_boundary(&self, node_end_seq: u64) {
        if self.apply_changes_late_boundary.load(Ordering::SeqCst) {
            self.wait_until(&self.apply_changes_hook_timed_out, Wake::Rung, || {
                self.apply_changes_drain_next_seq.load(Ordering::SeqCst) >= node_end_seq
            });
        }
    }

    /// Called by the scanner, once its barrier wait has ended, before each
    /// header it reads: under `apply_changes_hold_walk_until_drain_past_nodes`,
    /// holds the first such read (bounded) until the drain has begun
    /// processing an item past the node phase.
    pub(crate) fn before_apply_changes_walk_after_release(&self) {
        if self
            .apply_changes_hold_walk_until_drain_past_nodes
            .swap(false, Ordering::SeqCst)
        {
            self.wait_until(&self.apply_changes_hook_timed_out, Wake::Rung, || {
                self.apply_changes_drain_past_nodes.load(Ordering::SeqCst)
            });
        }
    }

    /// Called by the scanner before its barrier poll after dispatching the
    /// descriptor at `seq`.
    pub(crate) fn before_apply_changes_poll(&self, seq: u64) {
        if seq
            == self
                .apply_changes_poll_waits_for_drain_exit_after_seq
                .load(Ordering::SeqCst)
        {
            self.wait_until(&self.apply_changes_hook_timed_out, Wake::Rung, || {
                self.apply_changes_drain_returned.load(Ordering::SeqCst)
            });
        }
    }

    /// Called by the scanner as it reads each OsmData header.
    pub(crate) fn on_apply_changes_header_walked(&self) {
        self.apply_changes_scanner_headers_walked
            .fetch_add(1, &self.bell);
    }

    /// Called by the scanner when a candidate send finds the channel full.
    pub(crate) fn on_apply_changes_dispatch_blocked(&self) {
        self.apply_changes_dispatch_blocked.store(true, &self.bell);
    }

    /// Called by the scanner as it starts waiting for the barrier signal.
    pub(crate) fn on_apply_changes_barrier_wait(&self) {
        self.apply_changes_scanner_in_barrier_wait
            .store(true, &self.bell);
    }

    /// Called by the drain after each advance, with the seq it expects next.
    pub(crate) fn on_apply_changes_drain_advanced(&self, next_seq: u64) {
        self.apply_changes_drain_next_seq
            .store(next_seq, &self.bell);
    }

    /// Called by the drain once it handles anything past the node phase.
    pub(crate) fn on_apply_changes_drain_past_nodes(&self) {
        self.apply_changes_drain_past_nodes.store(true, &self.bell);
    }

    /// Called by `merge` once the drain has returned.
    pub(crate) fn on_apply_changes_drain_returned(&self) {
        self.apply_changes_drain_returned.store(true, &self.bell);
    }

    /// Called by the drain just before it merges the coordinate slots at
    /// the barrier.
    pub(crate) fn before_apply_changes_coord_merge(&self) {
        if self
            .apply_changes_merge_waits_for_scanner_or_candidate
            .load(Ordering::SeqCst)
        {
            let mark = self.apply_changes_mark_candidate_seq.load(Ordering::SeqCst);
            self.wait_until(&self.apply_changes_hook_timed_out, Wake::Rung, || {
                // Waiting without having walked the marked descriptor, or
                // the marked candidate done: whichever the scanner's shape
                // allows.
                (self
                    .apply_changes_scanner_in_barrier_wait
                    .load(Ordering::SeqCst)
                    && self
                        .apply_changes_scanner_headers_walked
                        .load(Ordering::SeqCst)
                        <= mark)
                    || self.apply_changes_candidate_marked.load(Ordering::SeqCst)
            });
        }
    }

    /// Called by a streaming worker once it has finished candidate `seq`
    /// (coordinates flushed and its item sent).
    pub(crate) fn on_apply_changes_candidate_done(&self, seq: u64) {
        if seq == self.apply_changes_mark_candidate_seq.load(Ordering::SeqCst) {
            self.apply_changes_candidate_marked.store(true, &self.bell);
        }
    }

    /// Called by the drain after it has published the map and just before
    /// it sends the barrier signal: under
    /// `apply_changes_signal_waits_for_scanner_wait`, holds it (bounded)
    /// until the scanner is in its barrier wait and records whether any way
    /// or relation descriptor was dispatched before the signal.
    pub(crate) fn before_apply_changes_barrier_send(&self) {
        if self
            .apply_changes_signal_waits_for_scanner_wait
            .load(Ordering::SeqCst)
        {
            self.wait_until(&self.apply_changes_hook_timed_out, Wake::Rung, || {
                self.apply_changes_scanner_in_barrier_wait
                    .load(Ordering::SeqCst)
            });
            self.apply_changes_non_node_dispatched_before_signal.store(
                self.apply_changes_non_node_dispatched
                    .load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
        }
    }

    /// A streaming-worker counter of the last apply-changes run under this
    /// token, by its sidecar name (`merge_streaming_blobs_processed` and
    /// kin); `None` before a run or for a name the pool does not emit.
    pub fn apply_changes_worker_counter(&self, name: &str) -> Option<u64> {
        self.apply_changes_worker_counters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| *v)
    }

    /// Called by the streaming pool as it finishes, with every counter it
    /// emits.
    pub(crate) fn record_apply_changes_worker_counters(&self, counters: Vec<(&'static str, u64)>) {
        *self
            .apply_changes_worker_counters
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = counters;
    }

    /// Called by the drain when the node boundary arrives, with the seq it
    /// expects next.
    pub(crate) fn on_apply_changes_boundary(&self, node_end_seq: u64, next_seq: u64) {
        if next_seq >= node_end_seq {
            self.apply_changes_late_boundaries_seen
                .fetch_add(1, Ordering::SeqCst);
        }
    }

    // ---- altw external stage 1 --------------------------------------------

    /// Hold every stage-1 worker `0..workers` (and the relation-member scan,
    /// if `relation`) once it has opened its files, until all of them have:
    /// the run then has every id-shard fleet and the scan's input open at
    /// once, whichever worker the ordered pool hands which blob. Arm before
    /// the run; read the result with
    /// [`altw_stage1_fleet_outcome`](Self::altw_stage1_fleet_outcome).
    /// Parties are released by the last arrival or by the token tripping;
    /// a party that fails before arriving leaves the others parked until
    /// the hook bound, and the outcome is then `Expired`.
    pub fn arm_altw_stage1_fleet(&self, workers: usize, relation: bool) {
        let mut fleet = self.fleet();
        // One-shot: a second arming would inherit the first run's arrivals
        // and outcome.
        assert!(
            fleet.workers.is_none(),
            "the stage-1 fleet rendezvous is armed once per token"
        );
        fleet.workers = Some(workers);
        fleet.relation = relation;
        drop(fleet);
        self.bell.ring();
    }

    /// How the armed rendezvous ended; `None` while it is still open or
    /// when it was never armed.
    pub fn altw_stage1_fleet_outcome(&self) -> Option<FleetOutcome> {
        self.fleet().outcome
    }

    fn fleet(&self) -> MutexGuard<'_, FleetRendezvous> {
        self.altw_stage1_fleet
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Called by a stage-1 party right after it opened its files;
    /// `cancelled` reads the run's token. Parks as armed; see
    /// [`arm_altw_stage1_fleet`](Self::arm_altw_stage1_fleet).
    pub(crate) fn on_altw_stage1_fleet(&self, party: FleetParty, cancelled: impl Fn() -> bool) {
        let mut fleet = self.fleet();
        let Some(workers) = fleet.workers else {
            return;
        };
        if fleet.outcome.is_none() {
            let fresh = match party {
                FleetParty::Worker(id) => {
                    let fresh = id < workers && !fleet.arrived_workers.contains(&id);
                    fleet.arrived_workers.push(id);
                    fresh
                }
                FleetParty::Relation => {
                    let fresh = fleet.relation && !fleet.relation_arrived;
                    fleet.relation_arrived = true;
                    fresh
                }
            };
            if !fresh {
                fleet.outcome = Some(FleetOutcome::Unexpected);
            } else if fleet.arrived_workers.len() == workers
                && fleet.relation_arrived == fleet.relation
            {
                // Decided under the lock while every earlier party is
                // parked with its descriptors open.
                fleet.outcome = Some(FleetOutcome::Complete);
            }
        }
        drop(fleet);
        self.bell.ring();
        // Rung: the outcome is written only here, each write followed by a
        // ring, and a tripped token rings the bell.
        // Past the bound or on a trip with no outcome, it is `Expired`.
        let _ = self.wait(Wake::Rung, || {
            (self.fleet().outcome.is_some() || cancelled()).then_some(())
        });
        let mut fleet = self.fleet();
        if fleet.outcome.is_none() {
            fleet.outcome = Some(FleetOutcome::Expired);
            drop(fleet);
            self.bell.ring();
        }
    }

    // ---- altw external stages 3 and 4 -------------------------------------

    /// Called by a stage 3 worker as it claims slot bucket `bucket`, before
    /// any of its work; `aborted` reads the router. Holds the armed bucket
    /// until the router is aborted, and records whether it was.
    pub(crate) fn on_altw_stage3_bucket(&self, bucket: usize, aborted: impl Fn() -> bool) {
        if bucket != self.altw_stage3_hold_bucket.load(Ordering::SeqCst) {
            return;
        }
        self.altw_stage3_bucket_held.store(true, &self.bell);
        // Polled: the router's abort does not ring the plan.
        self.wait_until(&self.altw_hook_timed_out, Wake::Polled, &aborted);
        self.altw_stage3_saw_abort
            .store(aborted(), Ordering::SeqCst);
    }

    /// Called by a stage 4 worker just before it waits on the router for way
    /// blob `way_blob_idx`.
    pub(crate) fn on_altw_stage4_ready_wait(&self, way_blob_idx: usize) {
        if way_blob_idx == 0 {
            self.altw_stage4_way0_entered.store(true, &self.bell);
        }
    }

    /// Called by a stage 4 worker as it starts decode ordinal `ordinal`:
    /// panics there, as armed, once the held stage 3 bucket has been
    /// claimed and a worker is at its readiness wait for way blob 0.
    pub(crate) fn on_altw_stage4_work(&self, ordinal: usize) {
        if ordinal != self.altw_stage4_panic_work_at.load(Ordering::SeqCst) {
            return;
        }
        self.wait_until(&self.altw_hook_timed_out, Wake::Rung, || {
            self.altw_stage3_bucket_held.load(Ordering::SeqCst)
                && self.altw_stage4_way0_entered.load(Ordering::SeqCst)
        });
        panic!("test-hooks: stage 4 worker panicking at ordinal {ordinal}");
    }

    /// Called as stage 4 starts: fails it, as armed, once the held stage 3
    /// bucket has been claimed.
    pub(crate) fn on_altw_stage4_start(&self) -> crate::Result<()> {
        if !self.altw_stage4_fail_at_start.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.wait_until(&self.altw_hook_timed_out, Wake::Rung, || {
            self.altw_stage3_bucket_held.load(Ordering::SeqCst)
        });
        Err(crate::Error::internal(
            "test-hooks: stage 4 failing at start",
        ))
    }

    /// Called by stage 4's consumer before it consumes decode ordinal
    /// `ordinal`: fails or panics there, as armed, once the held stage 3
    /// bucket has been claimed and a worker is at its readiness wait for way
    /// blob 0.
    pub(crate) fn on_altw_stage4_consume(&self, ordinal: usize) -> crate::Result<()> {
        let fails = ordinal == self.altw_stage4_fail_consume_at.load(Ordering::SeqCst);
        let panics = ordinal == self.altw_stage4_panic_consume_at.load(Ordering::SeqCst);
        if !fails && !panics {
            return Ok(());
        }
        self.wait_until(&self.altw_hook_timed_out, Wake::Rung, || {
            self.altw_stage3_bucket_held.load(Ordering::SeqCst)
                && self.altw_stage4_way0_entered.load(Ordering::SeqCst)
        });
        if panics {
            panic!("test-hooks: stage 4 consumer panicking at ordinal {ordinal}");
        }
        Err(crate::Error::internal(format!(
            "test-hooks: stage 4 consumer failing at ordinal {ordinal}"
        )))
    }
}

/// See [`FaultPlan::live_pool_worker`].
pub(crate) struct LivePoolWorker<'a>(&'a FaultPlan);

impl Drop for LivePoolWorker<'_> {
    fn drop(&mut self) {
        self.0.live_pool_workers.fetch_sub(1, &self.0.bell);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic(expected = "a seal observer is already installed")]
    fn duplicate_seal_observer_is_refused() {
        let invocation = crate::host::Invocation::new();
        let plan = invocation.faults();
        plan.on_seal_step(|_| {});
        plan.on_seal_step(|_| {});
    }

    #[test]
    #[should_panic(expected = "a publication observer is already installed")]
    fn duplicate_publication_observer_is_refused() {
        let invocation = crate::host::Invocation::new();
        let plan = invocation.faults();
        plan.on_publish_step(|_| {});
        plan.on_publish_step(|_| {});
    }
}
