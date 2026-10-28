//! Fault injection for tests: what a test can inject, in one place.
//!
//! Compiled only under the `test-hooks` feature; a product build has none of
//! it, and every hook site is gated on the same feature, so release code is
//! unchanged by its presence.
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

pub use crate::output::{PublishFault, PublishStep};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Upper bound on every bounded hook wait, so a broken pipeline fails the
/// test that armed it with a named message instead of hanging it. Under the
/// 20 s check watchdog, longer than any legitimate park in a fault test.
const HOOK_WAIT: Duration = Duration::from_secs(10);

/// Every token-scoped fault and observation. See the module docs.
///
/// Fields are public atomics: a test stores to arm and loads to observe.
/// Each group names the code it reaches.
pub struct FaultPlan {
    // --- read pipeline (`read::pipeline::run_pipeline`) ---
    decode_gate: Mutex<DecodeGate>,
    decode_gate_cv: Condvar,

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
    pub held_pool_workers: AtomicU64,
    /// Test-side release for parked workers (cleanup after the assertion).
    pub release_pool_workers: AtomicBool,
    /// Set when the owner is about to join a worker other than the panicked
    /// one while some worker is still parked: the owner is waiting for its
    /// pool rather than walking away from it. Releases parked workers.
    pub join_reached_held_worker: AtomicBool,
    /// Pool worker threads running: incremented on worker entry, decremented
    /// as the very last thing the worker does.
    pub live_pool_workers: AtomicU64,
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
    pub gzip_barriers_queued: AtomicU64,
    /// A bounded gzip hook wait expired.
    pub gzip_hook_timed_out: AtomicBool,

    // --- io_uring writer (`write::uring_writer`) ---
    /// Dispatch count (1-based) at which the uring writer thread panics.
    /// A dispatch is one handoff operation: a `Framed` or `Raw` chunk, each
    /// piece of a `RawChunks`, a `CopyRange`. The header is not one.
    pub uring_panic_at_dispatch: AtomicU64,
    /// Handoff operations the uring writer thread has dispatched.
    pub uring_dispatch_count: AtomicU64,

    // --- pipelined `PbfWriter` framing (`write::writer`) ---
    /// Sequence number whose framing job panics on the executor.
    pub framing_panic_at: AtomicUsize,

    // --- output construction (`Output::pbf_writer`) ---
    /// Refuse every output backend as a `PlatformUnsupported` setup error,
    /// produced as the constructor's result (so the output's failure
    /// bookkeeping runs as for a real refusal). The source error's message
    /// is [`INJECTED_BACKEND_REFUSAL`].
    pub refuse_output_backend: AtomicBool,
    /// Output constructions refused by `refuse_output_backend`.
    pub output_backend_refusals: AtomicU64,

    // --- diff and derive-changes parallel shards ---
    /// Shard index at which the parallel diff's shard worker panics.
    pub diff_shard_panic: AtomicUsize,
    /// Shard index at which the parallel derive-changes shard worker panics.
    pub derive_shard_panic: AtomicUsize,

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
    pub apply_changes_dispatch_blocked: AtomicBool,

    // --- apply-changes node-to-way barrier (`apply_changes::{scanner, drain}`) ---
    /// Hold the scanner's node boundary (bounded) until the drain has
    /// processed every seq below it, so the boundary arrives after the last
    /// node item: the case the drain once needed an idle re-poll for.
    pub apply_changes_late_boundary: AtomicBool,
    /// The drain's next expected seq, published after each advance.
    pub apply_changes_drain_next_seq: AtomicU64,
    /// Boundaries the drain received after it had already processed every
    /// seq below them, so a late-boundary test can prove its case ran.
    pub apply_changes_late_boundaries_seen: AtomicU64,
    /// Set by the scanner as it starts waiting for the barrier signal (at
    /// the first way or relation descriptor, or at end of input).
    pub apply_changes_scanner_in_barrier_wait: AtomicBool,
    /// Set by the scanner when it finds the barrier channel closed (in its
    /// wait at the first way or relation, or at a poll) and stops walking
    /// before end of input.
    pub apply_changes_scanner_stopped_before_eof: AtomicBool,
    /// Set by the scanner when its header walk reaches end of input.
    pub apply_changes_scanner_reached_eof: AtomicBool,
    /// OsmData headers the scanner has walked, counted as each is read.
    pub apply_changes_scanner_headers_walked: AtomicU64,
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
    pub apply_changes_drain_returned: AtomicBool,
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
    pub apply_changes_candidate_marked: AtomicBool,
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
    pub apply_changes_drain_past_nodes: AtomicBool,
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
    pub altw_stage3_bucket_held: AtomicBool,
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
    /// Set by a stage 4 worker just before it waits on the router for way
    /// blob 0.
    pub altw_stage4_way0_entered: AtomicBool,
    /// A bounded altw hook wait expired: the handshake did not happen, and
    /// a test relying on it proved nothing.
    pub altw_hook_timed_out: AtomicBool,

    // --- geocode builder pass 3 ---
    /// Street way index at which the Stage A streets worker panics.
    pub geocode_streets_panic_at_way: AtomicU32,

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
}

/// A publication observer; it may block (a test parks the publishing thread
/// in it until the process is killed).
type PublishObserver = Box<dyn Fn(PublishStep) + Send + Sync>;

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

struct DecodeGate {
    armed_seq: Option<usize>,
    ready: bool,
    released: bool,
}

impl Default for FaultPlan {
    fn default() -> Self {
        Self::new()
    }
}

impl FaultPlan {
    /// A plan with every fault disarmed and every counter at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            decode_gate: Mutex::new(DecodeGate {
                armed_seq: None,
                ready: false,
                released: false,
            }),
            decode_gate_cv: Condvar::new(),
            force_linear_region_grid: AtomicBool::new(false),
            region_grids_declined: AtomicU64::new(0),
            pool_worker_panic: AtomicU64::new(u64::MAX),
            hold_other_pool_workers: AtomicBool::new(false),
            held_pool_workers: AtomicU64::new(0),
            release_pool_workers: AtomicBool::new(false),
            join_reached_held_worker: AtomicBool::new(false),
            live_pool_workers: AtomicU64::new(0),
            pool_op_count: AtomicU64::new(0),
            pool_hook_timed_out: AtomicBool::new(false),
            gzip_panic_at_op: AtomicU64::new(u64::MAX),
            gzip_op_count: AtomicU64::new(0),
            gzip_panic_waits_for_barrier: AtomicBool::new(false),
            gzip_barriers_queued: AtomicU64::new(0),
            gzip_hook_timed_out: AtomicBool::new(false),
            uring_panic_at_dispatch: AtomicU64::new(u64::MAX),
            uring_dispatch_count: AtomicU64::new(0),
            framing_panic_at: AtomicUsize::new(usize::MAX),
            refuse_output_backend: AtomicBool::new(false),
            output_backend_refusals: AtomicU64::new(0),
            diff_shard_panic: AtomicUsize::new(usize::MAX),
            derive_shard_panic: AtomicUsize::new(usize::MAX),
            apply_changes_panic_at_blob_seq: AtomicU64::new(u64::MAX),
            apply_changes_panic_waits_for_barrier_wait: AtomicBool::new(false),
            apply_changes_park_at_blob_seq: AtomicU64::new(u64::MAX),
            apply_changes_hold_workers_until_dispatch_blocked: AtomicBool::new(false),
            apply_changes_dispatch_blocked: AtomicBool::new(false),
            apply_changes_late_boundary: AtomicBool::new(false),
            apply_changes_drain_next_seq: AtomicU64::new(0),
            apply_changes_late_boundaries_seen: AtomicU64::new(0),
            apply_changes_scanner_in_barrier_wait: AtomicBool::new(false),
            apply_changes_scanner_stopped_before_eof: AtomicBool::new(false),
            apply_changes_scanner_reached_eof: AtomicBool::new(false),
            apply_changes_scanner_headers_walked: AtomicU64::new(0),
            apply_changes_headers_walked_at_release: AtomicU64::new(u64::MAX),
            apply_changes_non_node_dispatched: AtomicBool::new(false),
            apply_changes_hold_walk_until_drain_past_nodes: AtomicBool::new(false),
            apply_changes_poll_waits_for_drain_exit_after_seq: AtomicU64::new(u64::MAX),
            apply_changes_drain_returned: AtomicBool::new(false),
            apply_changes_merge_waits_for_scanner_or_candidate: AtomicBool::new(false),
            apply_changes_mark_candidate_seq: AtomicU64::new(u64::MAX),
            apply_changes_candidate_marked: AtomicBool::new(false),
            apply_changes_signal_waits_for_scanner_wait: AtomicBool::new(false),
            apply_changes_non_node_dispatched_before_signal: AtomicBool::new(false),
            apply_changes_fail_at_refusal: AtomicBool::new(false),
            apply_changes_cancel_at_refusal: AtomicBool::new(false),
            apply_changes_drain_past_nodes: AtomicBool::new(false),
            apply_changes_barrier_signalled: AtomicBool::new(false),
            apply_changes_force_owned_route: AtomicBool::new(false),
            apply_changes_hook_timed_out: AtomicBool::new(false),
            apply_changes_worker_counters: Mutex::new(Vec::new()),
            check_overflow_ids: AtomicU64::new(0),
            altw_stage3_panic_at_bucket: AtomicUsize::new(usize::MAX),
            altw_stage3_hold_bucket: AtomicUsize::new(usize::MAX),
            altw_stage3_bucket_held: AtomicBool::new(false),
            altw_stage3_saw_abort: AtomicBool::new(false),
            altw_stage4_fail_at_start: AtomicBool::new(false),
            altw_stage4_fail_consume_at: AtomicUsize::new(usize::MAX),
            altw_stage4_panic_consume_at: AtomicUsize::new(usize::MAX),
            altw_stage4_way0_entered: AtomicBool::new(false),
            altw_hook_timed_out: AtomicBool::new(false),
            geocode_streets_panic_at_way: AtomicU32::new(u32::MAX),
            force_pin_path_reopen: AtomicBool::new(false),
            input_replacement: Mutex::new(None),
            publish_faults: Mutex::new(Vec::new()),
            publish_observer: OnceLock::new(),
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
    /// of every publication under this token, with no lock held. It may
    /// block: publication is not cancellable, so a test that parks it there
    /// ends the process to move on. Set once; a second call is ignored.
    pub fn on_publish_step(&self, observer: impl Fn(PublishStep) + Send + Sync + 'static) {
        drop(self.publish_observer.set(Box::new(observer)));
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
                .wait_timeout(g, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    // ---- parallel pwrite writer ----------------------------------------

    fn wait_until(&self, timed_out: &AtomicBool, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + HOOK_WAIT;
        while !cond() {
            if Instant::now() >= deadline {
                timed_out.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Called by a parallel-writer pool worker after it pops an op and
    /// before it runs it.
    pub(crate) fn on_pool_op(&self, worker: usize) {
        self.pool_op_count.fetch_add(1, Ordering::SeqCst);
        let hold = self.hold_other_pool_workers.load(Ordering::SeqCst);
        if worker as u64 == self.pool_worker_panic.load(Ordering::SeqCst) {
            if hold {
                self.wait_until(&self.pool_hook_timed_out, || {
                    self.held_pool_workers.load(Ordering::SeqCst) >= 1
                });
            }
            panic!("test-hooks: parallel_writer pool worker {worker} panicking");
        }
        if hold {
            self.held_pool_workers.fetch_add(1, Ordering::SeqCst);
            self.wait_until(&self.pool_hook_timed_out, || {
                self.join_reached_held_worker.load(Ordering::SeqCst)
                    || self.release_pool_workers.load(Ordering::SeqCst)
            });
            self.held_pool_workers.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Called by the parallel writer's owner just before it joins `worker`.
    pub(crate) fn before_pool_join(&self, worker: usize) {
        if worker as u64 != self.pool_worker_panic.load(Ordering::SeqCst)
            && self.held_pool_workers.load(Ordering::SeqCst) > 0
        {
            self.join_reached_held_worker.store(true, Ordering::SeqCst);
        }
    }

    /// `live_pool_workers` membership for one pool worker thread, released
    /// on drop (the unwind included).
    pub(crate) fn live_pool_worker(&self) -> LivePoolWorker<'_> {
        self.live_pool_workers.fetch_add(1, Ordering::SeqCst);
        LivePoolWorker(self)
    }

    // ---- OSC gzip pool --------------------------------------------------

    /// Called by a gzip worker after it dequeues chunk `seq`, before it
    /// compresses it.
    pub(crate) fn on_gzip_compress(&self, seq: usize) {
        let n = self.gzip_op_count.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.gzip_panic_at_op.load(Ordering::SeqCst) {
            if self.gzip_panic_waits_for_barrier.load(Ordering::SeqCst) {
                self.wait_until(&self.gzip_hook_timed_out, || {
                    self.gzip_barriers_queued.load(Ordering::SeqCst) != 0
                });
            }
            panic!("test-hooks: parallel_gzip pool op #{n} (seq {seq}) panicking");
        }
    }

    /// Called by the gzip writer's `flush` once its barrier is queued.
    pub(crate) fn on_gzip_barrier_queued(&self) {
        self.gzip_barriers_queued.fetch_add(1, Ordering::SeqCst);
    }

    // ---- io_uring writer -------------------------------------------------

    /// Called by the uring writer thread per handoff operation.
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
            self.wait_until(timed_out, || {
                self.apply_changes_dispatch_blocked.load(Ordering::SeqCst)
            });
        }
        if seq == self.apply_changes_park_at_blob_seq.load(Ordering::SeqCst) {
            self.wait_until(timed_out, &cancelled);
        }
        if seq == self.apply_changes_panic_at_blob_seq.load(Ordering::SeqCst) {
            if self
                .apply_changes_panic_waits_for_barrier_wait
                .load(Ordering::SeqCst)
            {
                self.wait_until(timed_out, || {
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
            self.wait_until(&self.apply_changes_hook_timed_out, || {
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
            self.wait_until(&self.apply_changes_hook_timed_out, || {
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
            self.wait_until(&self.apply_changes_hook_timed_out, || {
                self.apply_changes_drain_returned.load(Ordering::SeqCst)
            });
        }
    }

    /// Called by the drain just before it merges the coordinate slots at
    /// the barrier.
    pub(crate) fn before_apply_changes_coord_merge(&self) {
        if self
            .apply_changes_merge_waits_for_scanner_or_candidate
            .load(Ordering::SeqCst)
        {
            let mark = self.apply_changes_mark_candidate_seq.load(Ordering::SeqCst);
            self.wait_until(&self.apply_changes_hook_timed_out, || {
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
            self.apply_changes_candidate_marked
                .store(true, Ordering::SeqCst);
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
            self.wait_until(&self.apply_changes_hook_timed_out, || {
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

    // ---- altw external stages 3 and 4 -------------------------------------

    /// Called by a stage 3 worker as it claims slot bucket `bucket`, before
    /// any of its work; `aborted` reads the router. Holds the armed bucket
    /// until the router is aborted, and records whether it was.
    pub(crate) fn on_altw_stage3_bucket(&self, bucket: usize, aborted: impl Fn() -> bool) {
        if bucket != self.altw_stage3_hold_bucket.load(Ordering::SeqCst) {
            return;
        }
        self.altw_stage3_bucket_held.store(true, Ordering::SeqCst);
        self.wait_until(&self.altw_hook_timed_out, &aborted);
        self.altw_stage3_saw_abort
            .store(aborted(), Ordering::SeqCst);
    }

    /// Called by a stage 4 worker just before it waits on the router for way
    /// blob `way_blob_idx`.
    pub(crate) fn on_altw_stage4_ready_wait(&self, way_blob_idx: usize) {
        if way_blob_idx == 0 {
            self.altw_stage4_way0_entered.store(true, Ordering::SeqCst);
        }
    }

    /// Called as stage 4 starts: fails it, as armed, once the held stage 3
    /// bucket has been claimed.
    pub(crate) fn on_altw_stage4_start(&self) -> crate::Result<()> {
        if !self.altw_stage4_fail_at_start.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.wait_until(&self.altw_hook_timed_out, || {
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
        self.wait_until(&self.altw_hook_timed_out, || {
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
        self.0.live_pool_workers.fetch_sub(1, Ordering::SeqCst);
    }
}
