//! Three-stage pipelined PBF reader.
//!
//! Overlaps sequential I/O with parallel decompression and protobuf parsing,
//! delivering decoded `PrimitiveBlock`s to a caller-supplied closure in file order.

use super::blob::{BlobReader, BlobType, DecompressPool};
use super::block::PrimitiveBlock;
use super::pipeline_metrics::{PIPELINE_METRICS, elapsed_ns_u64};
use crate::blob_meta::BlobFilter;
use crate::error::Result;
use crate::reorder_buffer::ReorderBuffer;
use crate::tunables::{
    DECODE_WORKERS_FALLBACK, DECODE_WORKERS_MIN, DECODE_WORKERS_RESERVED, DEFAULT_DECODE_AHEAD,
    DEFAULT_READ_AHEAD,
};
use std::cell::RefCell;
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Instant;

type RawItem = (usize, crate::error::Result<crate::blob::Blob>);

/// Notes, once per run (checked at most once per pipeline), that a blob filter can skip nothing because
/// the input's first data blob carries no indexdata (the common "why is my
/// filtered read slow" case). Inert when no filter is set, or when the
/// filter has no indexdata-backed part (every kind wanted, no bbox: a
/// tag-key-only filter skips through tagdata, which this note does not
/// cover); costs one branch per blob on the reader thread after the first
/// data blob.
struct UnindexedFilterNote {
    pending: bool,
}

impl UnindexedFilterNote {
    fn new(filter: Option<&BlobFilter>) -> Self {
        Self {
            pending: filter.is_some_and(BlobFilter::uses_index),
        }
    }

    fn observe(
        &mut self,
        blob_result: &crate::error::Result<crate::blob::Blob>,
        diagnostics: &crate::host::Diagnostics,
    ) {
        if !self.pending {
            return;
        }
        let Ok(blob) = blob_result else { return };
        if blob.get_type() != BlobType::OsmData {
            return;
        }
        self.pending = false;
        if blob.index().is_none() {
            // Once per run: multi-pass commands open several filtered
            // pipelines over the same input.
            crate::host::diag_once!(
                diagnostics,
                Note,
                "reader.filter_without_indexdata",
                "Input has no indexdata on its first data blob; blob type and spatial filters cannot skip unindexed blobs"
            );
            crate::debug::emit_counter("pipeline_filter_without_indexdata", 1);
        }
    }
}

type DecodedPayload = Option<crate::error::Result<PrimitiveBlock>>;
type DecodedItem = (usize, DecodedPayload, Option<Permit>);

/// Returns `true` if the blob should be skipped based on the filter.
///
/// Checks indexdata (element type + spatial bbox) and tagdata (tag key presence).
/// Blobs without indexdata or tagdata always pass through (conservative).
fn should_skip_blob(filter: &BlobFilter, blob: &super::blob::Blob) -> bool {
    if let Some(idx) = blob.index()
        && !filter.wants_index(&idx)
    {
        return true;
    }
    if filter.has_tag_filter()
        && let Some(tag_idx) = blob.tag_index()
        && !filter.wants_tag_index(&tag_idx)
    {
        return true;
    }
    false
}

/// Bounds decode items admitted but not yet delivered from the reorder buffer.
///
/// Today only the one stage-2 dispatcher thread calls `acquire`, so a single
/// wakeup would suffice - but `release` notifies all waiters anyway. With one
/// waiter the two are the same call; with two, `notify_one` can wake the thread
/// that still cannot proceed and leave the pipeline hung with permits free.
/// Paying nothing now to remove a load-bearing invariant that nothing in the
/// type could enforce is the better trade: a future second acquirer is a
/// correctness question about *this* comment, not a silent deadlock under load.
struct AdmissionGate {
    count: Mutex<usize>,
    cond: Condvar,
    cap: NonZeroUsize,
}

impl AdmissionGate {
    fn new(cap: NonZeroUsize) -> Self {
        Self {
            count: Mutex::new(0),
            cond: Condvar::new(),
            cap,
        }
    }

    fn acquire(&self) -> bool {
        let mut count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        let mut blocked = false;
        while *count >= self.cap.get() {
            blocked = true;
            count = self
                .cond
                .wait(count)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *count += 1;
        blocked
    }

    fn release(&self) {
        let mut count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(*count > 0, "decode admission permit released below zero");
        *count -= 1;
        drop(count);
        self.cond.notify_all();
    }
}

struct Permit(Arc<AdmissionGate>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Fault-injection hooks for the pipelined reader's decode stage.
///
/// The decode gate parks the decode task for one armed sequence number
/// until the test releases it. Its state lives under one mutex so that
/// "is this seq armed" and "publish that a decode is parked" happen
/// atomically with respect to `reset()`.
///
/// Lifecycle contract, one controller at a time: `reset()`, then
/// `arm_block_decode(seq)`, then poll `blocked_decode_ready()`, then
/// `release_blocked_decode()`, then join the pipeline, then `reset()`.
/// `reset()` cancels any parked decode and waits for it to leave the
/// hook, so it is safe on every exit path. It does not stop a pipeline
/// that is still running: a pipeline that has not been joined can still
/// record reorder levels or reach a re-armed seq, so join before
/// re-arming or reading the filled high-water mark.
///
/// Every wait here is bounded and panics with a named message rather
/// than hanging until the check watchdog reports an unrelated-looking
/// futex wait.
///
/// Process-global, so gated behind `static-test-hooks` and `not(test)`:
/// only an integration-test target of its own can arm it.
#[cfg(all(feature = "static-test-hooks", not(test)))]
pub(crate) mod test_hooks {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
    use std::time::{Duration, Instant};

    pub static REORDER_FILLED_HIGH_WATER: AtomicUsize = AtomicUsize::new(0);

    /// How long an armed decode stays parked before it gives up and
    /// panics. Shorter than the 20 s check watchdog so the failure names
    /// the hook; longer than any legitimate park in the fault test.
    const PARK_LIMIT: Duration = Duration::from_secs(10);
    /// How long `reset()` waits for a cancelled decode to leave the hook.
    const RESET_LIMIT: Duration = Duration::from_secs(5);

    struct DecodeGate {
        armed_seq: Option<usize>,
        ready: bool,
        released: bool,
        /// Decodes currently inside the park loop.
        parked: usize,
        /// Bumped by `reset()`; a parked decode exits when it changes.
        epoch: u64,
        /// Set while `reset()` waits for parked decodes to drain, so an
        /// `arm_block_decode` cannot slip in while the lock is released.
        resetting: bool,
    }

    static GATE: Mutex<DecodeGate> = Mutex::new(DecodeGate {
        armed_seq: None,
        ready: false,
        released: false,
        parked: 0,
        epoch: 0,
        resetting: false,
    });
    static GATE_CV: Condvar = Condvar::new();

    fn lock() -> MutexGuard<'static, DecodeGate> {
        GATE.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait_until(
        guard: MutexGuard<'static, DecodeGate>,
        deadline: Instant,
    ) -> MutexGuard<'static, DecodeGate> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        GATE_CV
            .wait_timeout(guard, remaining)
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }

    /// Park the decode task for `seq` until released or reset. The gate
    /// must be idle: call `reset()` first.
    pub fn arm_block_decode(seq: usize) {
        let mut g = lock();
        if g.resetting || g.armed_seq.is_some() || g.parked != 0 {
            let (armed, parked, resetting) = (g.armed_seq, g.parked, g.resetting);
            drop(g);
            panic!(
                "test-hooks: arm_block_decode({seq}) on a gate that is not idle \
                 (armed {armed:?}, parked {parked}, resetting {resetting}); call reset() first"
            );
        }
        g.armed_seq = Some(seq);
        g.ready = false;
        g.released = false;
    }

    /// True once the armed decode has reached the hook and parked.
    pub fn blocked_decode_ready() -> bool {
        lock().ready
    }

    /// Let the parked decode (and any later one for the armed seq) go.
    pub fn release_blocked_decode() {
        lock().released = true;
        GATE_CV.notify_all();
    }

    /// Cancel any parked decode, wait for it to leave the hook, then clear
    /// all hook state. Safe to call when nothing is armed.
    pub fn reset() {
        let mut g = lock();
        g.epoch = g.epoch.wrapping_add(1);
        g.armed_seq = None;
        g.ready = false;
        g.released = false;
        g.resetting = true;
        GATE_CV.notify_all();
        let deadline = Instant::now() + RESET_LIMIT;
        while g.parked != 0 {
            if Instant::now() >= deadline {
                let parked = g.parked;
                g.resetting = false;
                drop(g);
                panic!(
                    "test-hooks: reset() cancelled the read-pipeline decode gate but \
                     {parked} decode(s) did not leave maybe_block_decode within {RESET_LIMIT:?}"
                );
            }
            g = wait_until(g, deadline);
        }
        g.resetting = false;
        drop(g);
        REORDER_FILLED_HIGH_WATER.store(0, Relaxed);
    }

    pub(crate) fn maybe_block_decode(seq: usize) {
        let mut g = lock();
        if g.armed_seq != Some(seq) {
            return;
        }
        g.ready = true;
        g.parked += 1;
        let epoch = g.epoch;
        GATE_CV.notify_all();
        let deadline = Instant::now() + PARK_LIMIT;
        loop {
            if g.released || g.epoch != epoch {
                break;
            }
            if Instant::now() >= deadline {
                g.parked -= 1;
                drop(g);
                GATE_CV.notify_all();
                panic!(
                    "test-hooks: read-pipeline decode for seq {seq} parked in \
                     maybe_block_decode for {PARK_LIMIT:?} and was never released; \
                     the test must call release_blocked_decode() or reset()"
                );
            }
            g = wait_until(g, deadline);
        }
        g.parked -= 1;
        drop(g);
        GATE_CV.notify_all();
    }

    pub(crate) fn record_reorder_filled(filled: usize) {
        cas_max_usize(&REORDER_FILLED_HIGH_WATER, filled);
    }

    fn cas_max_usize(field: &AtomicUsize, candidate: usize) {
        let mut current = field.load(Relaxed);
        while candidate > current {
            match field.compare_exchange_weak(current, candidate, Relaxed, Relaxed) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }
}

/// Runtime-tunable pipeline buffering configuration.
///
/// Depths stay nonzero because `sync_channel(0)` is a rendezvous channel, not
/// an empty buffer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PipelineConfig {
    read_ahead: NonZeroUsize,
    decode_ahead: NonZeroUsize,
}

impl PipelineConfig {
    fn depth(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap_or(NonZeroUsize::MIN)
    }

    pub(crate) fn set_read_ahead(&mut self, value: usize) {
        self.read_ahead = Self::depth(value);
    }

    pub(crate) fn set_decode_ahead(&mut self, value: usize) {
        self.decode_ahead = Self::depth(value);
    }
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            read_ahead: Self::depth(DEFAULT_READ_AHEAD),
            decode_ahead: Self::depth(DEFAULT_DECODE_AHEAD),
        }
    }
}

fn send_direct_error(tx: &SyncSender<DecodedItem>, seq: usize, e: crate::error::Error) -> bool {
    let t_send = Instant::now();
    let sent = tx.send((seq, Some(Err(e)), None)).is_ok();
    PIPELINE_METRICS
        .decoded_send_wait_ns
        .fetch_add(elapsed_ns_u64(t_send), Relaxed);
    sent
}

struct DecodeTask<'scope> {
    seq: usize,
    blob: crate::blob::Blob,
    tx: SyncSender<DecodedItem>,
    buffer_pool: Arc<DecompressPool>,
    blob_filter: Option<Arc<BlobFilter>>,
    permit: Permit,
    shutdown: Arc<AtomicBool>,
    /// The path of the reader the blob came from, borrowed from the
    /// dispatcher (captured once per pipeline, no per-blob refcounting) so a
    /// decode failure can be located with the blob's true provenance.
    source: Option<&'scope Arc<Path>>,
}

fn spawn_decode_task<'scope>(scope: &rayon::Scope<'scope>, task: DecodeTask<'scope>) {
    let DecodeTask {
        seq,
        blob,
        tx,
        buffer_pool,
        blob_filter,
        permit,
        shutdown,
        source,
    } = task;
    scope.spawn(move |_| {
        // Thread-local scratch buffers for parse_and_inline. Avoids allocating
        // fresh Vec<(u32, u32)> per blob.
        thread_local! {
            static ST_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
            static GR_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
        }
        let item = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Inside catch_unwind so the hook's park-timeout panic becomes a
            // decode error on the pipeline instead of escaping the rayon
            // spawn, which has no panic handler and would abort the process.
            #[cfg(all(feature = "static-test-hooks", not(test)))]
            test_hooks::maybe_block_decode(seq);
            match blob.get_type() {
                BlobType::OsmData => {
                    if let Some(ref filter) = blob_filter
                        && should_skip_blob(filter, &blob)
                    {
                        PIPELINE_METRICS
                            .blobs_skipped_by_filter
                            .fetch_add(1, Relaxed);
                        return None;
                    }
                    ST_SCRATCH.with_borrow_mut(|st| {
                        GR_SCRATCH.with_borrow_mut(|gr| {
                            let result =
                                blob.to_primitiveblock_inline_with_scratch(&buffer_pool, st, gr);
                            // Per-thread scratch retention is the iter-5 residual
                            // alloc bucket. Record current capacity in (u32, u32)
                            // pairs * 8 bytes for the global peak.
                            PIPELINE_METRICS.record_scratch_capacity(
                                st.capacity().saturating_mul(8),
                                gr.capacity().saturating_mul(8),
                            );
                            Some(result)
                        })
                    })
                }
                _ => None,
            }
        }));
        let item = match item {
            Ok(item) => item,
            Err(payload) => Some(Err(crate::error::panic_error(
                "decode task",
                payload.as_ref(),
            ))),
        };
        // Cold: only a failure pays for the location. The blob carries its
        // true index and offset; `seq` is a pipeline-internal ordering key
        // and is never reported as a location.
        let item = item.map(|r| r.map_err(|e| blob.locate_error(source, e)));
        let t_send = Instant::now();
        if tx.send((seq, item, Some(permit))).is_err() {
            shutdown.store(true, Relaxed);
        }
        PIPELINE_METRICS
            .decoded_send_wait_ns
            .fetch_add(elapsed_ns_u64(t_send), Relaxed);
    });
}

#[allow(clippy::needless_pass_by_value)]
fn drain_decoded<F>(
    decoded_rx: Receiver<DecodedItem>,
    decode_ahead: usize,
    cancel: &crate::host::Cancel,
    block_fn: &mut F,
) -> Result<()>
where
    F: FnMut(PrimitiveBlock) -> Result<()>,
{
    // Cancellation is checked here, on the consumer, before each wait and on
    // each arrival (a gap in the reorder buffer can hold back every pop, so a
    // pop-only check could starve), and returns through the same early exit
    // as a `block_fn` error. Stage 1's reader also stops on the token and
    // delivers `Cancelled` as a sequenced error item; this check only makes
    // the stop independent of how many decoded items are queued ahead of it.
    // Normal completion: the dispatcher drops its sender after raw EOF, task
    // clones finish, and recv returns Err once the last sender is gone.
    //
    // Early exit: returning from this helper drops `decoded_rx` before scoped
    // threads join. Blocked senders then fail, set shutdown, release permits,
    // wake the dispatcher, and let stage 1 stop at the raw channel.
    let mut pending: ReorderBuffer<(DecodedPayload, Option<Permit>)> =
        ReorderBuffer::with_capacity(decode_ahead);

    loop {
        cancel.check()?;
        let t_recv = Instant::now();
        let next = decoded_rx.recv();
        PIPELINE_METRICS
            .decoded_recv_wait_ns
            .fetch_add(elapsed_ns_u64(t_recv), Relaxed);
        let (seq, item, permit) = match next {
            Ok(pair) => pair,
            Err(_) => break,
        };
        cancel.check()?;
        pending.push(seq, (item, permit));
        PIPELINE_METRICS.record_reorder_levels(pending.filled_len(), pending.pending_len());
        #[cfg(all(feature = "static-test-hooks", not(test)))]
        test_hooks::record_reorder_filled(pending.filled_len());

        while let Some((item, permit)) = pending.pop_ready() {
            match item {
                Some(Ok(block)) => {
                    let result = block_fn(block);
                    drop(permit);
                    result?;
                }
                Some(Err(e)) => return Err(e),
                None => drop(permit),
            }
        }
    }
    cancel.check()
}

/// Runs a three-stage pipeline over a PBF file:
///
/// 1. **Reader thread**: sequential I/O, reads raw `Blob`s from the file.
/// 2. **Rayon pool**: parallel decompression (zlib) + protobuf parse.
/// 3. **Main thread**: reorder buffer delivers `PrimitiveBlock`s in file order to `block_fn`.
///
/// The closure runs on the calling thread and may hold mutable state.
/// PBF ordering (nodes → ways → relations) is preserved.
///
/// # Memory: decoded-block working set
///
/// Each decoded `PrimitiveBlock` is built by the inline-entries constructors
/// (`to_primitiveblock_inline_with_scratch` / `from_vec_pooled_with_scratch`):
/// the string-table entries and group ranges live as `(u32, u32)` slices
/// carved from the decompressed `Bytes` using reused thread-local scratch,
/// and the decompression buffer returns to `DecompressPool` on drop. There is
/// no per-block ~10 KB `WireStringTable::entries` heap Vec allocated on a
/// decode thread and freed on the consumer thread, so the earlier cross-thread
/// "free but mapped" retention pathology (once measured at 25+ GB at
/// Europe/planet scale) no longer occurs. The decoded working set is bounded
/// by the admission window below, not by file size.
///
/// Decode admission is bounded by `decode_ahead`: at most `decode_ahead`
/// decode tasks are admitted but not yet delivered from the reorder buffer.
/// The permit rides with decoded items through the channel and any reorder
/// slot, so completion skew cannot grow decoded-block memory with file size.
/// Backpressure from a slow consumer propagates through the decoded channel
/// to the admission gate, then to the raw channel and stage 1.
#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub(crate) fn run_pipeline<R, F>(
    mut blob_reader: BlobReader<R>,
    decode_thread_count: Option<usize>,
    pipeline_config: PipelineConfig,
    blob_filter: Option<BlobFilter>,
    resources: Option<crate::host::ResourcesHandle>,
    diagnostics: crate::host::Diagnostics,
    mut block_fn: F,
) -> Result<()>
where
    R: Read + Send,
    F: FnMut(PrimitiveBlock) -> Result<()>,
{
    // Enable tagdata parsing only when the filter needs tag key matching.
    // Enable indexdata parsing only when any filter is active (should_skip_blob
    // checks blob.index() for type + spatial filtering).
    let mut unindexed = UnindexedFilterNote::new(blob_filter.as_ref());
    let has_tag_filter = blob_filter.as_ref().is_some_and(BlobFilter::has_tag_filter);
    blob_reader.set_parse_tagdata(has_tag_filter);
    blob_reader.set_parse_indexdata(blob_filter.is_some());
    let blob_filter = blob_filter.map(Arc::new);
    let source = blob_reader.source().cloned();
    let cancel = blob_reader.cancel_token().clone();
    let (raw_tx, raw_rx) = sync_channel::<RawItem>(pipeline_config.read_ahead.get());
    let (decoded_tx, decoded_rx) = sync_channel::<DecodedItem>(pipeline_config.decode_ahead.get());

    std::thread::scope(|scope| {
        // Stage 1: Sequential I/O reader thread
        scope.spawn(move || {
            for (seq, blob_result) in blob_reader.enumerate() {
                unindexed.observe(&blob_result, &diagnostics);
                let t_send = Instant::now();
                let send_result = raw_tx.send((seq, blob_result));
                PIPELINE_METRICS
                    .raw_send_wait_ns
                    .fetch_add(elapsed_ns_u64(t_send), Relaxed);
                if send_result.is_err() {
                    break; // receiver dropped, pipeline shutting down
                }
            }
        });

        // Stage 2: Dispatcher thread - fans out to dedicated pool for parallel decode.
        //
        // We use a dedicated rayon pool rather than the global rayon pool so that
        // the caller's own parallelism (e.g. geometry processing, tile generation)
        // is not starved by decode work. The global pool is left entirely free.
        //
        // Thread count rationale:
        //   The pipeline occupies 2 threads beyond this pool: the Stage 1 I/O reader
        //   thread and the Stage 3 main/consumer thread (which runs `block_fn`).
        //   We subtract those 2 from the available hardware parallelism so the total
        //   thread count stays close to the physical core count, avoiding excessive
        //   context switching. The minimum is clamped to 1 to handle tiny VMs / CI.
        //
        //   Previously this was hardcoded to 4, which under-utilized machines with
        //   many cores and over-subscribed 2-core machines. Dynamic sizing ensures
        //   the decode pool scales with the hardware.
        //
        //   This pool is the primary bottleneck for pipelined reads: each blob
        //   requires zlib decompression followed by protobuf deserialization, both
        //   of which are CPU-bound. Maximizing the thread count here directly
        //   reduces wall-clock time for large files.
        let dispatch_tx = decoded_tx.clone();
        // `move` captures `raw_rx` into the stage-2 closure. On early
        // return from pool-build failure below, `raw_rx` drops with
        // the closure's locals and `sync_channel::send` in stage 1
        // wakes blocked senders with `Err`, letting stage 1 exit
        // cleanly. Do not refactor this into a form where `raw_rx`
        // outlives an error return from this closure: the reader
        // thread will block forever on a full channel.
        scope.spawn(move || {
            let decode_threads = decode_thread_count.unwrap_or_else(|| {
                crate::host::sized_workers(
                    resources.as_ref().map_or(
                        &crate::host::MachineProbe as &dyn crate::host::Resources,
                        |r| &*r.0,
                    ),
                    DECODE_WORKERS_RESERVED,
                    DECODE_WORKERS_MIN,
                    DECODE_WORKERS_FALLBACK,
                )
            });
            let decode_pool = match rayon::ThreadPoolBuilder::new()
                .num_threads(decode_threads)
                .build()
            {
                Ok(pool) => pool,
                Err(e) => {
                    let err = crate::error::Error::pool_build("decode pool", e);
                    // Seq 0 is correct here, not a magic value that can collide
                    // with a real item: the pool is built before the loop over
                    // `raw_rx` below, so nothing has been dispatched yet and 0
                    // is legitimately the reorder buffer's first push. Keep the
                    // build above the loop if this is ever restructured.
                    drop(dispatch_tx.send((0, Some(Err(err)), None)));
                    return;
                }
            };
            let buffer_pool = DecompressPool::new();
            let gate = Arc::new(AdmissionGate::new(pipeline_config.decode_ahead));
            let shutdown = Arc::new(AtomicBool::new(false));
            // Tasks borrow the reader's path from this scope: one capture per
            // pipeline, no refcounting per blob.
            let source = source.as_ref();
            decode_pool.in_place_scope(|task_scope| {
                for (seq, blob_result) in raw_rx {
                    if shutdown.load(Relaxed) {
                        break;
                    }
                    match blob_result {
                        Ok(blob) => {
                            let t_admit = Instant::now();
                            let blocked = gate.acquire();
                            PIPELINE_METRICS
                                .decode_admit_wait_ns
                                .fetch_add(elapsed_ns_u64(t_admit), Relaxed);
                            if blocked {
                                PIPELINE_METRICS.decode_admit_blocked.fetch_add(1, Relaxed);
                            }
                            let permit = Permit(Arc::clone(&gate));
                            PIPELINE_METRICS.decode_tasks.fetch_add(1, Relaxed);
                            spawn_decode_task(
                                task_scope,
                                DecodeTask {
                                    seq,
                                    blob,
                                    tx: dispatch_tx.clone(),
                                    buffer_pool: Arc::clone(&buffer_pool),
                                    blob_filter: blob_filter.clone(),
                                    permit,
                                    shutdown: Arc::clone(&shutdown),
                                    source,
                                },
                            );
                        }
                        Err(e) => {
                            if !send_direct_error(&dispatch_tx, seq, e) {
                                break;
                            }
                        }
                    }
                }
            });
            // dispatch_tx clone drops here
        });

        // Drop the original so the channel closes when all rayon task clones are done
        drop(decoded_tx);

        let result = drain_decoded(
            decoded_rx,
            pipeline_config.decode_ahead.get(),
            &cancel,
            &mut block_fn,
        );

        // Emit reader-pipeline counters before returning so that even
        // an error path produces sidecar data. Process-wide pool-drop
        // totals are emitted separately by the host after dispatch.
        PIPELINE_METRICS.emit();
        result
    })
}

// ---------------------------------------------------------------------------
// Fusion section: ordered decode-worker transforms for full-scan commands.
// ---------------------------------------------------------------------------

/// Fused sibling of `run_pipeline`. Keep this additive: the plain engine
/// continues to serve non-transform consumers with a different read shape.
#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::too_many_arguments
)]
#[hotpath::measure]
pub(crate) fn run_pipeline_fused<R, T, X, F>(
    mut blob_reader: BlobReader<R>,
    decode_thread_count: Option<usize>,
    pipeline_config: PipelineConfig,
    blob_filter: Option<BlobFilter>,
    resources: Option<crate::host::ResourcesHandle>,
    diagnostics: crate::host::Diagnostics,
    transform: &X,
    mut consume: F,
) -> Result<()>
where
    R: Read + Send,
    T: Send,
    X: Fn(PrimitiveBlock) -> Result<T> + Sync,
    F: FnMut(T) -> Result<()>,
{
    let mut unindexed = UnindexedFilterNote::new(blob_filter.as_ref());
    let has_tag_filter = blob_filter.as_ref().is_some_and(BlobFilter::has_tag_filter);
    blob_reader.set_parse_tagdata(has_tag_filter);
    blob_reader.set_parse_indexdata(blob_filter.is_some());
    let blob_filter = blob_filter.map(Arc::new);
    let source = blob_reader.source().cloned();
    let cancel = blob_reader.cancel_token().clone();
    let (raw_tx, raw_rx) = sync_channel::<RawItem>(pipeline_config.read_ahead.get());
    type FusedPayload<T> = Option<crate::error::Result<T>>;
    let (decoded_tx, decoded_rx) = sync_channel::<(usize, FusedPayload<T>, Option<Permit>)>(
        pipeline_config.decode_ahead.get(),
    );

    std::thread::scope(|scope| {
        scope.spawn(move || {
            for (seq, blob_result) in blob_reader.enumerate() {
                unindexed.observe(&blob_result, &diagnostics);
                let start = Instant::now();
                let send_result = raw_tx.send((seq, blob_result));
                PIPELINE_METRICS
                    .raw_send_wait_ns
                    .fetch_add(elapsed_ns_u64(start), Relaxed);
                if send_result.is_err() {
                    break;
                }
            }
        });

        let dispatch_tx = decoded_tx.clone();
        scope.spawn(move || {
            let threads = decode_thread_count.unwrap_or_else(|| {
                crate::host::sized_workers(resources.as_ref().map_or(&crate::host::MachineProbe as &dyn crate::host::Resources, |r| &*r.0), DECODE_WORKERS_RESERVED, DECODE_WORKERS_MIN, DECODE_WORKERS_FALLBACK)
            });
            let pool = match rayon::ThreadPoolBuilder::new().num_threads(threads).build() {
                Ok(pool) => pool,
                Err(e) => {
                    let error = crate::error::Error::pool_build("decode pool", e);
                    // Seq 0 is correct here for the same reason as in the
                    // block-oriented variant above: the pool is built before
                    // the loop over `raw_rx`, so no item has been dispatched
                    // and 0 is the reorder buffer's first legitimate push.
                    drop(dispatch_tx.send((0, Some(Err(error)), None)));
                    return;
                }
            };
            let buffer_pool = DecompressPool::new();
            let gate = Arc::new(AdmissionGate::new(pipeline_config.decode_ahead));
            let shutdown = Arc::new(AtomicBool::new(false));
            // Tasks borrow the reader's path from this scope: one capture per
            // pipeline, no refcounting per blob.
            let source = source.as_ref();
            pool.in_place_scope(|task_scope| {
                for (seq, blob_result) in raw_rx {
                    if shutdown.load(Relaxed) {
                        break;
                    }
                    match blob_result {
                        Ok(blob) => {
                            let start = Instant::now();
                            let blocked = gate.acquire();
                            PIPELINE_METRICS
                                .decode_admit_wait_ns
                                .fetch_add(elapsed_ns_u64(start), Relaxed);
                            if blocked {
                                PIPELINE_METRICS.decode_admit_blocked.fetch_add(1, Relaxed);
                            }
                            let permit = Permit(Arc::clone(&gate));
                            let tx = dispatch_tx.clone();
                            let pool = Arc::clone(&buffer_pool);
                            let filter = blob_filter.clone();
                            let shutdown = Arc::clone(&shutdown);
                            PIPELINE_METRICS.decode_tasks.fetch_add(1, Relaxed);
                            task_scope.spawn(move |_| {
                                // Rayon keeps these thread-local buffers with its worker,
                                // avoiding a fresh pair of Vec allocations per blob task.
                                thread_local! {
                                    static ST_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
                                    static GR_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
                                }
                                let item =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        if blob.get_type() != BlobType::OsmData {
                                            return None;
                                        }
                                        if let Some(filter) = filter.as_ref()
                                            && should_skip_blob(filter, &blob)
                                        {
                                            PIPELINE_METRICS
                                                .blobs_skipped_by_filter
                                                .fetch_add(1, Relaxed);
                                            return None;
                                        }
                                        ST_SCRATCH.with_borrow_mut(|st| {
                                            GR_SCRATCH.with_borrow_mut(|gr| {
                                                let result = blob
                                                    .to_primitiveblock_inline_with_scratch(&pool, st, gr)
                                                    .and_then(transform);
                                                PIPELINE_METRICS.record_scratch_capacity(
                                                    st.capacity().saturating_mul(8),
                                                    gr.capacity().saturating_mul(8),
                                                );
                                                Some(result)
                                            })
                                        })
                                    }));
                                let item = match item {
                                    Ok(item) => item,
                                    Err(payload) => Some(Err(crate::error::panic_error(
                                        "decode task",
                                        payload.as_ref(),
                                    ))),
                                };
                                // Cold: only a failure pays for the location.
                                // `seq` is a pipeline ordering key, never a
                                // location.
                                let item =
                                    item.map(|r| r.map_err(|e| blob.locate_error(source, e)));
                                let start = Instant::now();
                                if tx.send((seq, item, Some(permit))).is_err() {
                                    shutdown.store(true, Relaxed);
                                }
                                PIPELINE_METRICS
                                    .decoded_send_wait_ns
                                    .fetch_add(elapsed_ns_u64(start), Relaxed);
                            });
                        }
                        Err(error) => {
                            let start = Instant::now();
                            if dispatch_tx
                                .send((seq, Some(Err(error)), None))
                                .is_err()
                            {
                                break;
                            }
                            PIPELINE_METRICS
                                .decoded_send_wait_ns
                                .fetch_add(elapsed_ns_u64(start), Relaxed);
                        }
                    }
                }
            });
        });
        drop(decoded_tx);
        let result = drain_fused(
            decoded_rx,
            pipeline_config.decode_ahead.get(),
            &cancel,
            &mut consume,
        );
        PIPELINE_METRICS.emit();
        result
    })
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
fn drain_fused<T, F>(
    decoded_rx: Receiver<(usize, Option<crate::error::Result<T>>, Option<Permit>)>,
    decode_ahead: usize,
    cancel: &crate::host::Cancel,
    consume: &mut F,
) -> Result<()>
where
    F: FnMut(T) -> Result<()>,
{
    // Cancellation checks as in `drain_decoded`.
    let mut pending = ReorderBuffer::with_capacity(decode_ahead);
    loop {
        cancel.check()?;
        let start = Instant::now();
        let next = decoded_rx.recv();
        PIPELINE_METRICS
            .decoded_recv_wait_ns
            .fetch_add(elapsed_ns_u64(start), Relaxed);
        let (seq, item, permit) = match next {
            Ok(item) => item,
            Err(_) => break,
        };
        cancel.check()?;
        pending.push(seq, (item, permit));
        PIPELINE_METRICS.record_reorder_levels(pending.filled_len(), pending.pending_len());
        #[cfg(all(feature = "static-test-hooks", not(test)))]
        test_hooks::record_reorder_filled(pending.filled_len());
        while let Some((item, permit)) = pending.pop_ready() {
            match item {
                Some(Ok(item)) => {
                    let result = consume(item);
                    drop(permit);
                    result?;
                }
                Some(Err(error)) => return Err(error),
                None => drop(permit),
            }
        }
    }
    cancel.check()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::block_builder::{BlockBuilder, HeaderBuilder};
    use crate::writer::{Compression, PbfWriter};
    use crate::{BlobFilter, ElementReader};
    use std::io::{Cursor, Read};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[derive(Clone)]
    struct CountingRead {
        inner: Cursor<Vec<u8>>,
        bytes_read: Arc<AtomicUsize>,
    }

    impl CountingRead {
        fn new(bytes: Vec<u8>, bytes_read: Arc<AtomicUsize>) -> Self {
            Self {
                inner: Cursor::new(bytes),
                bytes_read,
            }
        }
    }

    impl Read for CountingRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(buf)?;
            self.bytes_read.fetch_add(read, Ordering::Relaxed);
            if read > 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(read)
        }
    }

    fn assert_completes<F, T>(label: &str, f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            let result = f();
            tx.send(()).expect("watchdog receiver open");
            result
        });
        rx.recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| panic!("{label}: did not complete within timeout"));
        handle.join().expect("watchdog thread joins")
    }

    fn fused_pbf(blocks: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut writer = PbfWriter::new(&mut bytes, Compression::None);
            writer
                .write_header(&HeaderBuilder::new().build().unwrap())
                .unwrap();
            for id in 0..blocks {
                let mut builder = BlockBuilder::new();
                builder.add_node(
                    i64::try_from(id).unwrap(),
                    0,
                    0,
                    std::iter::empty::<(&str, &str)>(),
                    None,
                );
                writer
                    .write_primitive_block(builder.take().unwrap().unwrap())
                    .unwrap();
            }
            writer.flush().unwrap();
        }
        bytes
    }

    fn fused_counts(reader: ElementReader<Cursor<Vec<u8>>>) -> Result<Vec<usize>> {
        let mut counts = Vec::new();
        reader.for_each_fused_block(
            |block| Ok::<_, crate::Error>(block.elements().count()),
            |count| {
                counts.push(count);
                Ok(())
            },
        )?;
        Ok(counts)
    }

    #[test]
    fn fused_output_matches_block_pipelined() {
        let bytes = fused_pbf(3);
        let mut pipelined = Vec::new();
        ElementReader::new(Cursor::new(bytes.clone()))
            .unwrap()
            .for_each_block_pipelined(|block| {
                pipelined.push(block.elements().count());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            fused_counts(ElementReader::new(Cursor::new(bytes)).unwrap()).unwrap(),
            pipelined
        );
    }

    #[test]
    fn fused_transform_error_surfaces_after_prior_blocks() {
        let mut consumed = Vec::new();
        let result = ElementReader::new(Cursor::new(fused_pbf(4)))
            .unwrap()
            .decode_threads(1)
            .for_each_fused_block(
                |block| {
                    let id = match block.elements().next().expect("fixture element") {
                        crate::Element::DenseNode(node) => node.id(),
                        crate::Element::Node(node) => node.id(),
                        crate::Element::Way(way) => way.id(),
                        crate::Element::Relation(relation) => relation.id(),
                    };
                    if id == 2 {
                        Err(crate::Error::internal("transform failure"))
                    } else {
                        Ok(id)
                    }
                },
                |seq| {
                    consumed.push(seq);
                    Ok(())
                },
            );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("transform failure")
        );
        assert_eq!(consumed, [0, 1]);
    }

    #[test]
    fn fused_transform_panic_reports_error() {
        let result = assert_completes("fused transform panic", || {
            ElementReader::new(Cursor::new(fused_pbf(1)))
                .unwrap()
                .for_each_fused_block(
                    |_| -> crate::Result<()> { panic!("test panic") },
                    |_| Ok(()),
                )
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("decode task panicked: test panic")
        );
    }

    #[test]
    fn fused_early_consumer_error_stops_promptly() {
        let bytes = fused_pbf(128);
        let full_len = bytes.len();
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let result = ElementReader::new(CountingRead::new(bytes, Arc::clone(&bytes_read)))
            .unwrap()
            .for_each_fused_block(
                |_| Ok::<_, crate::Error>(()),
                |_| Err(crate::Error::internal("stop")),
            );
        assert!(result.unwrap_err().to_string().contains("stop"));
        assert!(bytes_read.load(Ordering::Relaxed) < full_len);
    }

    #[test]
    fn fused_tiny_count_bounds_complete_in_order() {
        assert_eq!(
            fused_counts(
                ElementReader::new(Cursor::new(fused_pbf(3)))
                    .unwrap()
                    .read_ahead(1)
                    .decode_ahead(1)
            )
            .unwrap(),
            [1, 1, 1]
        );
    }

    #[test]
    fn fused_stalled_transform_preserves_order() {
        let stalled = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let worker_stalled = Arc::clone(&stalled);
        let worker_release = Arc::clone(&release);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut ids = Vec::new();
            let result = ElementReader::new(Cursor::new(fused_pbf(3)))
                .unwrap()
                .decode_threads(2)
                .for_each_fused_block(
                    |block| {
                        let id = match block.elements().next().unwrap() {
                            crate::Element::DenseNode(node) => node.id(),
                            crate::Element::Node(node) => node.id(),
                            crate::Element::Way(way) => way.id(),
                            crate::Element::Relation(relation) => relation.id(),
                        };
                        if id == 0 {
                            worker_stalled.store(true, Ordering::Relaxed);
                            while !worker_release.load(Ordering::Relaxed) {
                                std::thread::yield_now();
                            }
                        }
                        Ok::<_, crate::Error>(id)
                    },
                    |id| {
                        ids.push(id);
                        Ok(())
                    },
                );
            done_tx.send((result, ids)).unwrap();
        });
        let start = std::time::Instant::now();
        while !stalled.load(Ordering::Relaxed) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "transform did not stall"
            );
            std::thread::yield_now();
        }
        release.store(true, Ordering::Relaxed);
        let (result, ids) = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        result.unwrap();
        assert_eq!(ids, [0, 1, 2]);
    }

    #[test]
    fn fused_thread_count_parity() {
        let one = fused_counts(
            ElementReader::new(Cursor::new(fused_pbf(8)))
                .unwrap()
                .decode_threads(1),
        )
        .unwrap();
        let many = fused_counts(
            ElementReader::new(Cursor::new(fused_pbf(8)))
                .unwrap()
                .decode_threads(8),
        )
        .unwrap();
        assert_eq!(one, many);
    }

    #[test]
    fn fused_blobfilter_skips_blobs() {
        let reader = ElementReader::new(Cursor::new(fused_pbf(2)))
            .unwrap()
            .with_blob_filter(BlobFilter::new(false, true, false));
        assert!(fused_counts(reader).unwrap().is_empty());
    }

    #[test]
    fn gate_blocks_at_cap_and_release_unblocks() {
        let gate = Arc::new(AdmissionGate::new(NonZeroUsize::new(2).unwrap()));
        assert!(!gate.acquire());
        assert!(!gate.acquire());

        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let child_gate = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            ready_tx.send(()).expect("ready channel open");
            let blocked = child_gate.acquire();
            done_tx.send(blocked).expect("done channel open");
        });

        ready_rx.recv().expect("child reached acquire");
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());

        gate.release();
        match done_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(blocked) => assert!(blocked),
            Err(e) => panic!("child did not acquire after release: {e}"),
        }
        if let Err(e) = handle.join() {
            panic!("child panicked: {e:?}");
        }
    }

    #[test]
    fn pipeline_config_keeps_zero_depths_at_one() {
        let mut config = PipelineConfig::default();
        config.set_read_ahead(0);
        config.set_decode_ahead(0);

        assert_eq!(config.read_ahead.get(), 1);
        assert_eq!(config.decode_ahead.get(), 1);
    }

    #[test]
    fn permit_drop_releases() {
        let gate = Arc::new(AdmissionGate::new(NonZeroUsize::new(1).unwrap()));
        {
            assert!(!gate.acquire());
            let _permit = Permit(Arc::clone(&gate));
        }
        assert!(!gate.acquire());
        gate.release();
    }
}
