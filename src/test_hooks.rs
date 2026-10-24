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
//! [`Cancel`](crate::host::Cancel) token ([`Cancel::faults`](crate::host::Cancel::faults)),
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
//! These are per-object rather than per-token, and stay with their owner:
//!
//! - [`PublishFault`], re-exported here: faults in output publication, set on
//!   the output (`Output::inject_fault` and its set and directory siblings).
//! - `Cancel::on_check` / `Cancel::checks_passed`: the cancellation
//!   checkpoint callback (cancellation semantics, not a fault).
//! - The geocode index reader's lookup counter, on the reader.
//!
//! Disarmed is a sentinel (`MAX` for an index or a count), never zero, since
//! zero is a real worker, shard or sequence number.

pub use crate::output::PublishFault;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
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
    // --- read pipeline (`read::pipeline`, block engine) ---
    decode_gate: Mutex<DecodeGate>,
    decode_gate_cv: Condvar,
    /// Highest number of filled reorder slots any pipelined read under this
    /// token has held (both the block and the fused engine record it).
    pub reorder_filled_high_water: AtomicUsize,

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
    pub uring_panic_at_dispatch: AtomicU64,
    /// Items the uring writer thread has dispatched.
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

    // --- altw external stage 3 ---
    /// Slot bucket index at which the stage 3 worker claiming it panics.
    pub altw_stage3_panic_at_bucket: AtomicUsize,

    // --- geocode builder pass 3 ---
    /// Street way index at which the Stage A streets worker panics.
    pub geocode_streets_panic_at_way: AtomicU32,
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
            reorder_filled_high_water: AtomicUsize::new(0),
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
            altw_stage3_panic_at_bucket: AtomicUsize::new(usize::MAX),
            geocode_streets_panic_at_way: AtomicU32::new(u32::MAX),
        }
    }

    // ---- read pipeline decode gate -------------------------------------

    fn gate(&self) -> MutexGuard<'_, DecodeGate> {
        self.decode_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Park the block engine's decode task for `seq` until
    /// [`release_blocked_decode`](Self::release_blocked_decode). Arm before
    /// the read starts; once released, the gate stays open.
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

    /// Called by a pipelined read's consumer after each reorder push.
    pub(crate) fn record_reorder_filled(&self, filled: usize) {
        self.reorder_filled_high_water
            .fetch_max(filled, Ordering::Relaxed);
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
    pub(crate) fn on_gzip_compress(&self, seq: u64) {
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

    /// Called by the uring writer thread per dispatched item.
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

    /// The apply-changes streaming panic selector, `None` when disarmed.
    pub(crate) fn apply_changes_panic_seq(&self) -> Option<u64> {
        match self.apply_changes_panic_at_blob_seq.load(Ordering::Relaxed) {
            u64::MAX => None,
            seq => Some(seq),
        }
    }
}

/// See [`FaultPlan::live_pool_worker`].
pub(crate) struct LivePoolWorker<'a>(&'a FaultPlan);

impl Drop for LivePoolWorker<'_> {
    fn drop(&mut self) {
        self.0.live_pool_workers.fetch_sub(1, Ordering::SeqCst);
    }
}
