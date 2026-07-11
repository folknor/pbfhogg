//! Ordered, byte-bounded batched decode pipeline.
//!
//! The pump assigns monotonically increasing batch sequence numbers in file
//! order; entries within each batch retain that order; and the consumer's
//! reorder buffer delivers batches by sequence number. Therefore blocks reach
//! the callback in exact file order. The pump flushes a partial batch before
//! blocking for raw capacity: a worker can only free bytes belonging to a
//! published batch, so waiting with an unpublished batch would deadlock.
//!
//! This deliberately owns copies of the batching primitives used by
//! `reader::par_fold_blobs`. The gated engine must be removable with its two
//! dispatch arms, without entangling the shipped fold path. Unlike that pump,
//! this one needs `try_acquire` for the flush-before-blocking rule above.
//!
//! `PBFHOGG_READ_AHEAD_BYTES` and `PBFHOGG_DECODE_AHEAD_BYTES` override this
//! engine's raw and decoded byte budgets. The count knobs (`read_ahead` and
//! `decode_ahead`) are intentionally ignored because admission is byte-native.
//! `PBFHOGG_BLOCK_QUEUE_BYTES` and `PBFHOGG_CMD_BATCH_BYTES` remain above this
//! seam and apply unchanged under either engine.
//!
//! `PBFHOGG_READ_AHEAD_BYTES` keeps its name but has a wider domain here: the
//! default engine releases its raw permit when the dispatcher picks the blob up,
//! while this engine holds the raw charge through worker decode. `PBFHOGG_FADVISE_BATCH_BYTES`
//! remains below this seam in `BlobReader`, so it composes unchanged with either
//! engine. The only verdict-bearing gate combination is
//! `PBFHOGG_BATCHED_PIPELINE=1 PBFHOGG_FUSE_TRANSFORM=1`; batched byte-knob
//! combinations are supported for correctness testing, not verdict measurement.
//!
//! This pump flushes before acquiring raw capacity. The par-fold pump's
//! acquire-then-flush ordering is safe only because its 256 MiB budget dwarfs
//! its 4 MiB batch plus the maximum legal blob. Do not blindly unify these pumps:
//! this engine's 32 MiB default and arbitrary byte-budget overrides lack that
//! margin.

use super::blob::{Blob, BlobReader, BlobType, DecompressPool};
use super::block::PrimitiveBlock;
use super::pipeline::{PipelineConfig, should_skip_blob};
use super::pipeline_metrics::{PIPELINE_METRICS, elapsed_ns_u64};
use crate::blob_meta::BlobFilter;
use crate::error::{ErrorKind, Result, new_error};
use crate::reorder_buffer::ReorderBuffer;
use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Instant;

// Planet primary averages about 1.84 MiB compressed and 3.68 MiB decoded per
// blob, so 16 raw blobs are about 29.4 MiB and 32 decoded blobs about 117.7
// MiB. These parity round-ups retain today's primary shape; 4 MiB batches are
// about two primary blobs or roughly sixty 8k blobs. Charges use declared
// decoded capacity/retained length rather than this approximate 2x ratio.
const BATCH_MAX_BLOBS: usize = 64;
const BATCH_TARGET_BYTES: u64 = 4 * 1024 * 1024;
const RAW_INFLIGHT_BUDGET: u64 = 32 * 1024 * 1024;
const DECODED_INFLIGHT_BUDGET: u64 = 128 * 1024 * 1024;
// Floors zero-datasize blobs, so a pathological flood cannot create unbounded
// queue slots while consuming no byte-budget capacity.
const MIN_BLOB_CHARGE: u64 = 1024;

static BATCHES: AtomicU64 = AtomicU64::new(0);
static BATCH_RAW_WAIT_NS: AtomicU64 = AtomicU64::new(0);
static BATCH_DECODED_WAIT_NS: AtomicU64 = AtomicU64::new(0);
// Reorder occupancy measured in batches, not individual blobs.
static BATCHED_REORDER_HIGH_WATER: AtomicU64 = AtomicU64::new(0);

struct BatchMsg {
    seq: usize,
    blobs: Vec<Blob>,
    raw_bytes: u64,
    decoded: Option<Permit>,
}

struct DecodedBatch {
    entries: Vec<Result<PrimitiveBlock>>,
    decoded: Option<Permit>,
}

struct Budget {
    state: Mutex<(u64, bool)>,
    cond: Condvar,
    cap: u64,
}

impl Budget {
    fn new(cap: u64) -> Self {
        Self {
            state: Mutex::new((0, false)),
            cond: Condvar::new(),
            cap: cap.max(1),
        }
    }

    /// `None` means capacity would block, `Some(false)` means shutdown.
    fn try_acquire(&self, bytes: u64) -> Option<bool> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.1 {
            return Some(false);
        }
        if state.0 == 0 || state.0.saturating_add(bytes) <= self.cap {
            state.0 = state.0.saturating_add(bytes);
            Some(true)
        } else {
            None
        }
    }

    /// Returns `(admitted, blocked)`, keeping budget-wait metrics free of
    /// uncontended mutex overhead.
    fn acquire(&self, bytes: u64) -> (bool, bool) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut blocked = false;
        loop {
            if state.1 {
                return (false, blocked);
            }
            if state.0 == 0 || state.0.saturating_add(bytes) <= self.cap {
                state.0 = state.0.saturating_add(bytes);
                return (true, blocked);
            }
            blocked = true;
            state = self
                .cond
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn release(&self, bytes: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.0 = state.0.saturating_sub(bytes);
        drop(state);
        self.cond.notify_all();
    }

    fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.1 = true;
        drop(state);
        self.cond.notify_all();
    }

    fn is_shutdown(&self) -> bool {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).1
    }
}

struct Permit {
    budget: Arc<Budget>,
    bytes: u64,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

struct Queue {
    state: Mutex<(VecDeque<BatchMsg>, bool)>,
    cond: Condvar,
}

impl Queue {
    fn new() -> Self {
        Self {
            state: Mutex::new((VecDeque::new(), false)),
            cond: Condvar::new(),
        }
    }

    fn push(&self, msg: BatchMsg) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.0.push_back(msg);
        drop(state);
        self.cond.notify_one();
    }

    fn pop(&self) -> Option<BatchMsg> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(msg) = state.0.pop_front() {
                return Some(msg);
            }
            if state.1 {
                return None;
            }
            state = self
                .cond
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.1 = true;
        drop(state);
        self.cond.notify_all();
    }
}

/// Cancels every blocking point when an owning scope exits exceptionally.
struct CancelGuard<'a> {
    raw: &'a Budget,
    decoded: &'a Budget,
    queue: &'a Queue,
    armed: bool,
}

impl<'a> CancelGuard<'a> {
    fn new(raw: &'a Budget, decoded: &'a Budget, queue: &'a Queue) -> Self {
        Self {
            raw,
            decoded,
            queue,
            armed: true,
        }
    }

    fn cancel(&mut self) {
        if self.armed {
            self.raw.shutdown();
            self.decoded.shutdown();
            self.queue.close();
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelGuard<'_> {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Owns a dequeued batch until its compressed storage has been freed and its
/// raw charge released. This closes the unwind gap between queue pop and send.
struct BatchCharge<'a> {
    msg: Option<BatchMsg>,
    raw: &'a Budget,
}

impl<'a> BatchCharge<'a> {
    fn new(msg: BatchMsg, raw: &'a Budget) -> Self {
        Self {
            msg: Some(msg),
            raw,
        }
    }

    fn blobs(&self) -> &[Blob] {
        &self.msg.as_ref().expect("batch charge present").blobs
    }

    fn finish(self, entries: Vec<Result<PrimitiveBlock>>) -> (usize, DecodedBatch) {
        let (seq, decoded) = self.finish_parts();
        (seq, DecodedBatch { entries, decoded })
    }

    fn finish_parts(mut self) -> (usize, Option<Permit>) {
        let mut msg = self.msg.take().expect("batch charge present");
        msg.blobs.clear();
        self.raw.release(msg.raw_bytes);
        (msg.seq, msg.decoded.take())
    }
}

impl Drop for BatchCharge<'_> {
    fn drop(&mut self) {
        if let Some(mut msg) = self.msg.take() {
            msg.blobs.clear();
            self.raw.release(msg.raw_bytes);
        }
    }
}

fn charge(bytes: u64) -> u64 {
    bytes.max(MIN_BLOB_CHARGE)
}

fn worker_count(threads: Option<usize>) -> usize {
    threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|count| count.get().saturating_sub(2).max(1))
            .unwrap_or(4)
    })
}

fn cancel(raw: &Budget, decoded: &Budget, queue: &Queue) {
    raw.shutdown();
    decoded.shutdown();
    queue.close();
}

fn flush(
    batch: &mut Vec<Blob>,
    raw_bytes: &mut u64,
    seq: &mut usize,
    queue: &Queue,
    decoded: &Arc<Budget>,
) -> bool {
    if batch.is_empty() {
        return true;
    }
    let decoded_bytes = batch
        .iter()
        .map(|blob| charge(u64::try_from(blob.decoded_len_hint()).unwrap_or(u64::MAX)))
        .fold(0, u64::saturating_add);
    // The only decoded-budget blocking point is this flush point. This checks
    // the actual admission invariant rather than merely restating the early
    // empty-batch return above: all raw bytes held by this partial batch are
    // exactly the charges carried into the published message.
    debug_assert_eq!(
        *raw_bytes,
        batch
            .iter()
            .map(|blob| charge(blob.retained_len()))
            .sum::<u64>(),
        "decoded-budget wait must publish every held raw charge"
    );
    let start = Instant::now();
    let (admitted, blocked) = decoded.acquire(decoded_bytes);
    if !admitted {
        return false;
    }
    if blocked {
        BATCH_DECODED_WAIT_NS.fetch_add(elapsed_ns_u64(start), Relaxed);
    }
    let permit = Permit {
        budget: Arc::clone(decoded),
        bytes: decoded_bytes,
    };
    queue.push(BatchMsg {
        seq: *seq,
        blobs: std::mem::take(batch),
        raw_bytes: std::mem::replace(raw_bytes, 0),
        decoded: Some(permit),
    });
    *seq += 1;
    BATCHES.fetch_add(1, Relaxed);
    true
}

fn pump<R, E, M>(
    mut reader: BlobReader<R>,
    queue: &Queue,
    raw: &Arc<Budget>,
    decoded: &Arc<Budget>,
    tx: &SyncSender<(usize, E)>,
    make_error: M,
) where
    R: Read + Send,
    E: Send,
    M: Fn(crate::error::Error) -> E,
{
    // A reader/pump panic must wake workers blocked in pop and the consumer
    // blocked in recv before scoped-thread join begins.
    let mut guard = CancelGuard::new(raw, decoded, queue);
    let mut batch = Vec::new();
    let mut raw_bytes = 0;
    let mut seq = 0;
    for item in &mut reader {
        let blob = match item {
            Ok(blob) => blob,
            Err(error) => {
                if flush(&mut batch, &mut raw_bytes, &mut seq, queue, decoded) {
                    // Receiver drop wakes this direct send on consumer error.
                    drop(tx.send((seq, make_error(error))));
                }
                break;
            }
        };
        if raw.is_shutdown() {
            break;
        }
        if blob.get_type() != BlobType::OsmData {
            continue;
        }
        let bytes = charge(blob.retained_len());
        match raw.try_acquire(bytes) {
            Some(true) => {}
            Some(false) => break,
            None => {
                if !flush(&mut batch, &mut raw_bytes, &mut seq, queue, decoded) {
                    break;
                }
                // Raw waits never retain an unpublished partial batch.
                debug_assert!(batch.is_empty());
                let start = Instant::now();
                let (admitted, blocked) = raw.acquire(bytes);
                if !admitted {
                    break;
                }
                if blocked {
                    BATCH_RAW_WAIT_NS.fetch_add(elapsed_ns_u64(start), Relaxed);
                }
            }
        }
        if !batch.is_empty()
            && raw_bytes.saturating_add(bytes) > BATCH_TARGET_BYTES
            && !flush(&mut batch, &mut raw_bytes, &mut seq, queue, decoded)
        {
            break;
        }
        raw_bytes = raw_bytes.saturating_add(bytes);
        batch.push(blob);
        if batch.len() == BATCH_MAX_BLOBS
            && !flush(&mut batch, &mut raw_bytes, &mut seq, queue, decoded)
        {
            break;
        }
    }
    let _ = flush(&mut batch, &mut raw_bytes, &mut seq, queue, decoded);
    queue.close();
    guard.disarm();
}

fn decode_batch_entry(
    blob: &Blob,
    filter: Option<&BlobFilter>,
    pool: &Arc<DecompressPool>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
) -> Option<Result<PrimitiveBlock>> {
    if let Some(filter) = filter
        && should_skip_blob(filter, blob)
    {
        PIPELINE_METRICS
            .blobs_skipped_by_filter
            .fetch_add(1, Relaxed);
        return None;
    }
    PIPELINE_METRICS.decode_tasks.fetch_add(1, Relaxed);
    Some(blob.to_primitiveblock_inline_with_scratch(pool, st_scratch, gr_scratch))
}

fn run_worker(
    queue: &Queue,
    raw: &Arc<Budget>,
    decoded: &Arc<Budget>,
    tx: &SyncSender<(usize, DecodedBatch)>,
    filter: Option<&BlobFilter>,
    pool: &Arc<DecompressPool>,
) {
    let mut guard = CancelGuard::new(raw, decoded, queue);
    let mut st_scratch = Vec::new();
    let mut gr_scratch = Vec::new();
    while let Some(msg) = queue.pop() {
        let charge = BatchCharge::new(msg, raw);
        if raw.is_shutdown() {
            drop(charge);
            break;
        }
        #[cfg(feature = "test-hooks")]
        test_hooks::maybe_stall(charge.msg.as_ref().expect("batch charge present").seq);
        let mut entries = Vec::with_capacity(charge.blobs().len());
        for blob in charge.blobs() {
            let entry = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                #[cfg(feature = "test-hooks")]
                test_hooks::maybe_panic(charge.msg.as_ref().expect("batch charge present").seq);
                decode_batch_entry(blob, filter, pool, &mut st_scratch, &mut gr_scratch)
            }));
            match entry {
                Ok(Some(Ok(block))) => entries.push(Ok(block)),
                Ok(Some(Err(error))) => {
                    entries.push(Err(error));
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    entries.push(Err(new_error(ErrorKind::Io(std::io::Error::other(
                        "decode task panicked",
                    )))));
                    break;
                }
            }
        }
        let output = charge.finish(entries);
        if tx.send(output).is_err() {
            cancel(raw, decoded, queue);
            break;
        }
    }
    guard.disarm();
}

fn record_reorder_high_water(value: usize) {
    let value = value as u64;
    let mut current = BATCHED_REORDER_HIGH_WATER.load(Relaxed);
    while value > current {
        match BATCHED_REORDER_HIGH_WATER.compare_exchange_weak(current, value, Relaxed, Relaxed) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

fn emit_batched_metrics() {
    for (name, value) in [
        ("pipeline_batches", &BATCHES),
        ("pipeline_batch_raw_wait_ns", &BATCH_RAW_WAIT_NS),
        ("pipeline_batch_decoded_wait_ns", &BATCH_DECODED_WAIT_NS),
        (
            "pipeline_batched_reorder_high_water",
            &BATCHED_REORDER_HIGH_WATER,
        ),
    ] {
        crate::debug::emit_counter(name, i64::try_from(value.load(Relaxed)).unwrap_or(i64::MAX));
    }
}

fn consume<F>(
    rx: Receiver<(usize, DecodedBatch)>,
    raw: &Budget,
    decoded: &Budget,
    queue: &Queue,
    block_fn: &mut F,
) -> Result<()>
where
    F: FnMut(PrimitiveBlock) -> Result<()>,
{
    let mut pending = ReorderBuffer::with_capacity(8);
    loop {
        let start = Instant::now();
        let next = rx.recv();
        PIPELINE_METRICS
            .decoded_recv_wait_ns
            .fetch_add(elapsed_ns_u64(start), Relaxed);
        let (seq, batch) = match next {
            Ok(item) => item,
            Err(_) => break,
        };
        pending.push(seq, batch);
        record_reorder_high_water(pending.filled_len());
        while let Some(batch) = pending.pop_ready() {
            for entry in batch.entries {
                if let Err(error) = entry.and_then(&mut *block_fn) {
                    cancel(raw, decoded, queue);
                    // This local receiver is dropped before the scope joins.
                    drop(rx);
                    return Err(error);
                }
            }
            drop(batch.decoded);
        }
    }
    debug_assert_eq!(pending.filled_len(), 0, "batches lost at clean EOF");
    Ok(())
}

/// Runs the gated ordered-batch engine with the same surface as `run_pipeline`.
#[allow(clippy::needless_pass_by_value)]
#[hotpath::measure]
pub(crate) fn run_batched_pipeline<R, F>(
    mut reader: BlobReader<R>,
    decode_threads: Option<usize>,
    config: PipelineConfig,
    filter: Option<BlobFilter>,
    mut block_fn: F,
) -> Result<()>
where
    R: Read + Send,
    F: FnMut(PrimitiveBlock) -> Result<()>,
{
    reader.set_parse_tagdata(filter.as_ref().is_some_and(BlobFilter::has_tag_filter));
    reader.set_parse_indexdata(filter.is_some());
    let raw = Arc::new(Budget::new(
        config
            .read_ahead_bytes
            .map_or(RAW_INFLIGHT_BUDGET, |bytes| bytes as u64),
    ));
    let decoded = Arc::new(Budget::new(
        config
            .decode_ahead_bytes
            .map_or(DECODED_INFLIGHT_BUDGET, |bytes| bytes as u64),
    ));
    let queue = Arc::new(Queue::new());
    let pool = DecompressPool::new();
    let workers = worker_count(decode_threads);
    let (tx, rx) = sync_channel(workers.saturating_mul(2).max(1));

    std::thread::scope(|scope| {
        // This guard must exist before any spawn: unwinding a callback then
        // wakes budget- and queue-blocked threads before scoped join begins.
        let mut consumer_guard = CancelGuard::new(&raw, &decoded, &queue);
        let pump_queue = Arc::clone(&queue);
        let pump_raw = Arc::clone(&raw);
        let pump_decoded = Arc::clone(&decoded);
        let pump_tx = tx.clone();
        scope.spawn(move || {
            pump(
                reader,
                &pump_queue,
                &pump_raw,
                &pump_decoded,
                &pump_tx,
                |error| DecodedBatch {
                    entries: vec![Err(error)],
                    decoded: None,
                },
            );
        });
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let raw = Arc::clone(&raw);
            let decoded = Arc::clone(&decoded);
            let tx = tx.clone();
            let pool = Arc::clone(&pool);
            let filter = filter.as_ref();
            scope.spawn(move || run_worker(&queue, &raw, &decoded, &tx, filter, &pool));
        }
        drop(tx);
        let result = consume(rx, &raw, &decoded, &queue, &mut block_fn);
        // `consume` performs cancellation before it drops the receiver on its
        // error path. Disarming here makes the receiver-drop ordering explicit;
        // an unwind bypasses this line and remains covered by the armed guard.
        consumer_guard.disarm();
        PIPELINE_METRICS.emit();
        emit_batched_metrics();
        result
    })
}

// ---------------------------------------------------------------------------
// Fusion section: removable independently of the batched engine.
// `decode_batch_entry` above is permanent; transform happens at its call site.
// ---------------------------------------------------------------------------

struct FusedDecodedBatch<T> {
    entries: Vec<Result<T>>,
    decoded: Option<Permit>,
}

fn run_fused_worker<T, X>(
    queue: &Queue,
    raw: &Arc<Budget>,
    decoded: &Arc<Budget>,
    tx: &SyncSender<(usize, FusedDecodedBatch<T>)>,
    filter: Option<&BlobFilter>,
    pool: &Arc<DecompressPool>,
    transform: &X,
) where
    T: Send,
    X: Fn(PrimitiveBlock) -> std::result::Result<T, String> + Sync,
{
    let mut guard = CancelGuard::new(raw, decoded, queue);
    let mut st_scratch = Vec::new();
    let mut gr_scratch = Vec::new();
    while let Some(msg) = queue.pop() {
        let charge = BatchCharge::new(msg, raw);
        if raw.is_shutdown() {
            break;
        }
        let mut entries = Vec::with_capacity(charge.blobs().len());
        for blob in charge.blobs() {
            let entry = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                decode_batch_entry(blob, filter, pool, &mut st_scratch, &mut gr_scratch).map(
                    |result| {
                        result.and_then(|block| {
                            transform(block).map_err(|error| {
                                new_error(ErrorKind::Io(std::io::Error::other(error)))
                            })
                        })
                    },
                )
            }));
            match entry {
                Ok(Some(Ok(item))) => entries.push(Ok(item)),
                Ok(Some(Err(error))) => {
                    entries.push(Err(error));
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    entries.push(Err(new_error(ErrorKind::Io(std::io::Error::other(
                        "decode task panicked",
                    )))));
                    break;
                }
            }
        }
        let (seq, decoded_permit) = charge.finish_parts();
        if tx
            .send((
                seq,
                FusedDecodedBatch {
                    entries,
                    decoded: decoded_permit,
                },
            ))
            .is_err()
        {
            cancel(raw, decoded, queue);
            break;
        }
    }
    guard.disarm();
}

fn consume_fused<T, F>(
    rx: Receiver<(usize, FusedDecodedBatch<T>)>,
    raw: &Budget,
    decoded: &Budget,
    queue: &Queue,
    consume: &mut F,
) -> Result<()>
where
    F: FnMut(T) -> Result<()>,
{
    let mut pending = ReorderBuffer::with_capacity(8);
    loop {
        let start = Instant::now();
        let next = rx.recv();
        PIPELINE_METRICS
            .decoded_recv_wait_ns
            .fetch_add(elapsed_ns_u64(start), Relaxed);
        let (seq, batch) = match next {
            Ok(item) => item,
            Err(_) => break,
        };
        pending.push(seq, batch);
        record_reorder_high_water(pending.filled_len());
        while let Some(batch) = pending.pop_ready() {
            for entry in batch.entries {
                if let Err(error) = entry.and_then(&mut *consume) {
                    cancel(raw, decoded, queue);
                    drop(rx);
                    return Err(error);
                }
            }
            // Charge decoded input bytes until ordered delivery. Transformed
            // AltW output can be larger, so this is not a byte ceiling there.
            drop(batch.decoded);
        }
    }
    debug_assert_eq!(pending.filled_len(), 0, "batches lost at clean EOF");
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
#[hotpath::measure]
pub(crate) fn run_batched_pipeline_fused<R, T, X, F>(
    mut reader: BlobReader<R>,
    decode_threads: Option<usize>,
    config: PipelineConfig,
    filter: Option<BlobFilter>,
    transform: &X,
    mut consume: F,
) -> Result<()>
where
    R: Read + Send,
    T: Send,
    X: Fn(PrimitiveBlock) -> std::result::Result<T, String> + Sync,
    F: FnMut(T) -> Result<()>,
{
    reader.set_parse_tagdata(filter.as_ref().is_some_and(BlobFilter::has_tag_filter));
    reader.set_parse_indexdata(filter.is_some());
    let raw = Arc::new(Budget::new(
        config
            .read_ahead_bytes
            .map_or(RAW_INFLIGHT_BUDGET, |bytes| bytes as u64),
    ));
    let decoded = Arc::new(Budget::new(
        config
            .decode_ahead_bytes
            .map_or(DECODED_INFLIGHT_BUDGET, |bytes| bytes as u64),
    ));
    let queue = Arc::new(Queue::new());
    let pool = DecompressPool::new();
    let workers = worker_count(decode_threads);
    let (tx, rx) = sync_channel(workers.saturating_mul(2).max(1));

    std::thread::scope(|scope| {
        let mut consumer_guard = CancelGuard::new(&raw, &decoded, &queue);
        let pump_queue = Arc::clone(&queue);
        let pump_raw = Arc::clone(&raw);
        let pump_decoded = Arc::clone(&decoded);
        let pump_tx = tx.clone();
        scope.spawn(move || {
            pump(
                reader,
                &pump_queue,
                &pump_raw,
                &pump_decoded,
                &pump_tx,
                |error| FusedDecodedBatch {
                    entries: vec![Err(error)],
                    decoded: None,
                },
            );
        });
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let raw = Arc::clone(&raw);
            let decoded = Arc::clone(&decoded);
            let tx = tx.clone();
            let pool = Arc::clone(&pool);
            let filter = filter.as_ref();
            scope.spawn(move || {
                run_fused_worker(&queue, &raw, &decoded, &tx, filter, &pool, transform);
            });
        }
        drop(tx);
        let result = consume_fused(rx, &raw, &decoded, &queue, &mut consume);
        consumer_guard.disarm();
        PIPELINE_METRICS.emit();
        emit_batched_metrics();
        result
    })
}

#[cfg(feature = "test-hooks")]
pub(crate) mod test_hooks {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

    pub static STALL_BATCH_SEQ: AtomicUsize = AtomicUsize::new(usize::MAX);
    pub static STALLED_READY: AtomicBool = AtomicBool::new(false);
    pub static RELEASE_STALLED: AtomicBool = AtomicBool::new(false);
    pub static PANIC_BATCH_SEQ: AtomicUsize = AtomicUsize::new(usize::MAX);

    pub fn reset() {
        STALL_BATCH_SEQ.store(usize::MAX, Relaxed);
        STALLED_READY.store(false, Relaxed);
        RELEASE_STALLED.store(false, Relaxed);
        PANIC_BATCH_SEQ.store(usize::MAX, Relaxed);
        // Cross-test isolation: this counter is process-global test state.
        super::BATCHED_REORDER_HIGH_WATER.store(0, Relaxed);
    }

    pub fn reorder_high_water() -> u64 {
        super::BATCHED_REORDER_HIGH_WATER.load(Relaxed)
    }

    pub(super) fn maybe_stall(seq: usize) {
        if STALL_BATCH_SEQ.load(Relaxed) == seq {
            STALLED_READY.store(true, Relaxed);
            while !RELEASE_STALLED.load(Relaxed) {
                std::thread::yield_now();
            }
        }
    }

    pub(super) fn maybe_panic(seq: usize) {
        assert_ne!(
            PANIC_BATCH_SEQ.load(Relaxed),
            seq,
            "batched worker hook panic"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ElementReader;
    use crate::block_builder::{BlockBuilder, HeaderBuilder};
    use crate::writer::{Compression, PbfWriter};
    use std::io::{Cursor, Read};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const RAW_INFLIGHT_BUDGET_USIZE: usize = 32 * 1024 * 1024;

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

    fn pbf(data_blobs: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut writer = PbfWriter::new(&mut bytes, Compression::Zlib(6));
            writer
                .write_header(&HeaderBuilder::new().build().unwrap())
                .unwrap();
            for id in 0..data_blobs {
                let mut builder = BlockBuilder::new();
                builder.add_node(
                    i64::try_from(id + 1).unwrap(),
                    500_000_000,
                    100_000_000,
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

    fn pbf_with_partial_then_near_cap_blob() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut writer = PbfWriter::new(&mut bytes, Compression::None);
            writer
                .write_header(&HeaderBuilder::new().build().unwrap())
                .unwrap();
            // 63 floored small-blob charges leave an unpublished partial batch.
            // This payload is safely below the PBF's 32 MiB message limit, but
            // together with that partial it exceeds the engine's 32 MiB raw cap.
            for id in 0..63_i32 {
                let mut builder = BlockBuilder::new();
                builder.add_node(
                    i64::from(id + 1),
                    500_000_000,
                    100_000_000,
                    std::iter::empty::<(&str, &str)>(),
                    None,
                );
                writer
                    .write_primitive_block(builder.take().unwrap().unwrap())
                    .unwrap();
            }
            let payload = "x".repeat(RAW_INFLIGHT_BUDGET_USIZE - 32 * 1024);
            let mut builder = BlockBuilder::new();
            builder.add_node(
                999,
                500_000_000,
                100_000_000,
                [("payload", payload.as_str())],
                None,
            );
            writer
                .write_primitive_block(builder.take().unwrap().unwrap())
                .unwrap();
            writer.flush().unwrap();
        }
        bytes
    }

    fn completes(label: &str, f: impl FnOnce() + Send + 'static) {
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            f();
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("{label} did not complete"));
    }

    fn pump_batches(bytes: Vec<u8>) -> Vec<BatchMsg> {
        let queue = Queue::new();
        let raw = Arc::new(Budget::new(512 * 1024 * 1024));
        let decoded = Arc::new(Budget::new(512 * 1024 * 1024));
        let (tx, _rx) = sync_channel(1);
        pump(
            BlobReader::new(Cursor::new(bytes)),
            &queue,
            &raw,
            &decoded,
            &tx,
            |error| DecodedBatch {
                entries: vec![Err(error)],
                decoded: None,
            },
        );
        let mut batches = Vec::new();
        while let Some(batch) = queue.pop() {
            batches.push(batch);
        }
        batches
    }

    fn pbf_with_payloads(payload_lens: &[usize]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut writer = PbfWriter::new(&mut bytes, Compression::None);
            writer
                .write_header(&HeaderBuilder::new().build().unwrap())
                .unwrap();
            for (id, len) in payload_lens.iter().copied().enumerate() {
                let payload = "x".repeat(len);
                let mut builder = BlockBuilder::new();
                builder.add_node(
                    i64::try_from(id + 1).unwrap(),
                    500_000_000,
                    100_000_000,
                    [("payload", payload.as_str())],
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

    #[test]
    fn batched_fused_matches_plain_batched() {
        let mut plain = Vec::new();
        ElementReader::new(Cursor::new(pbf(3)))
            .unwrap()
            .batched_pipeline(true)
            .for_each_block_pipelined(|block| {
                plain.push(block.elements().count());
                Ok(())
            })
            .unwrap();
        let mut fused = Vec::new();
        ElementReader::new(Cursor::new(pbf(3)))
            .unwrap()
            .batched_pipeline(true)
            .for_each_fused_block(
                |block| Ok::<_, String>(block.elements().count()),
                |count| {
                    fused.push(count);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(plain, fused);
    }

    #[test]
    fn batched_fused_transform_error_position() {
        let seen = std::sync::atomic::AtomicUsize::new(0);
        let mut consumed = Vec::new();
        let result = ElementReader::new(Cursor::new(pbf(3)))
            .unwrap()
            .batched_pipeline(true)
            .decode_threads(1)
            .for_each_fused_block(
                |_| {
                    let seq = seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if seq == 1 {
                        Err("transform failure".to_owned())
                    } else {
                        Ok(seq)
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
        assert_eq!(consumed, [0]);
    }

    #[test]
    fn batched_fused_transform_panic_reports_error() {
        let result = assert_completes("batched fused transform panic", || {
            ElementReader::new(Cursor::new(pbf(1)))
                .unwrap()
                .batched_pipeline(true)
                .for_each_fused_block(
                    |_| -> std::result::Result<(), String> { panic!("test panic") },
                    |_| Ok(()),
                )
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("decode task panicked")
        );
    }

    #[test]
    fn batched_fused_early_consumer_error_stops_promptly() {
        let bytes = pbf(128);
        let full_len = bytes.len();
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let result = ElementReader::new(CountingRead::new(bytes, Arc::clone(&bytes_read)))
            .unwrap()
            .batched_pipeline(true)
            .for_each_fused_block(
                |_| Ok::<_, String>(()),
                |_| {
                    Err(crate::error::new_error(crate::error::ErrorKind::Io(
                        std::io::Error::other("stop"),
                    )))
                },
            );
        assert!(result.unwrap_err().to_string().contains("stop"));
        assert!(bytes_read.load(Ordering::Relaxed) < full_len);
    }

    #[test]
    fn batched_fused_dispatch_via_builder() {
        let mut count = 0;
        ElementReader::new(Cursor::new(pbf(2)))
            .unwrap()
            .batched_pipeline(true)
            .for_each_fused_block(
                |_| Ok::<_, String>(()),
                |_| {
                    count += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn budget_blocks_at_cap_until_release() {
        let budget = Arc::new(Budget::new(10));
        assert_eq!(budget.acquire(6), (true, false));
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let child = Arc::clone(&budget);
        let handle = std::thread::spawn(move || done_tx.send(child.acquire(5)).unwrap());
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        budget.release(6);
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            (true, true)
        );
        handle.join().unwrap();
    }

    #[test]
    fn budget_admits_oversized_when_empty() {
        assert_eq!(Budget::new(10).acquire(100), (true, false));
    }

    #[test]
    fn budget_shutdown_wakes_acquirer() {
        let budget = Arc::new(Budget::new(10));
        assert_eq!(budget.acquire(10), (true, false));
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let child = Arc::clone(&budget);
        let handle = std::thread::spawn(move || done_tx.send(child.acquire(1)).unwrap());
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        budget.shutdown();
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            (false, true)
        );
        handle.join().unwrap();
    }

    #[test]
    fn budget_try_acquire_never_blocks() {
        let budget = Budget::new(10);
        assert_eq!(budget.try_acquire(10), Some(true));
        let start = Instant::now();
        assert_eq!(budget.try_acquire(1), None);
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn queue_drains_then_closes() {
        let queue = Queue::new();
        queue.push(BatchMsg {
            seq: 0,
            blobs: Vec::new(),
            raw_bytes: 0,
            decoded: None,
        });
        queue.close();
        assert!(queue.pop().is_some());
        assert!(queue.pop().is_none());
    }

    #[test]
    fn queue_close_wakes_blocked_consumer() {
        let queue = Arc::new(Queue::new());
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let child = Arc::clone(&queue);
        let handle = std::thread::spawn(move || done_tx.send(child.pop().is_none()).unwrap());
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        queue.close();
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        handle.join().unwrap();
    }

    #[test]
    fn pump_batches_at_exact_count_cap() {
        let batches = pump_batches(pbf(BATCH_MAX_BLOBS + 1));
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.blobs.len())
                .collect::<Vec<_>>(),
            [64, 1]
        );
    }

    #[test]
    fn pump_flushes_before_byte_target_overflow() {
        let batches = pump_batches(pbf_with_payloads(&[3 * 1024 * 1024, 2 * 1024 * 1024]));
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.blobs.len())
                .collect::<Vec<_>>(),
            [1, 1]
        );
    }

    #[test]
    fn pump_gives_lone_oversized_blob_its_own_batch() {
        let batches = pump_batches(pbf_with_payloads(&[5 * 1024 * 1024, 1]));
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.blobs.len())
                .collect::<Vec<_>>(),
            [1, 1]
        );
    }

    #[test]
    fn pump_applies_floored_minimum_charges() {
        let batches = pump_batches(pbf(2));
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].raw_bytes, 2 * MIN_BLOB_CHARGE);
    }

    #[test]
    fn two_blobs_one_worker_one_byte_raw_cap_completes() {
        completes("one-byte raw cap", || {
            let mut blocks = 0;
            ElementReader::new(Cursor::new(pbf(2)))
                .unwrap()
                .batched_pipeline(true)
                .decode_threads(1)
                .read_ahead_bytes(1)
                .for_each_block_pipelined(|_| {
                    blocks += 1;
                    Ok(())
                })
                .unwrap();
            assert_eq!(blocks, 2);
        });
    }

    #[test]
    fn partial_batch_then_near_cap_blob_completes_under_default_cap() {
        let raw_charge = BlobReader::new(Cursor::new(pbf_with_partial_then_near_cap_blob()))
            .filter_map(|item| {
                let blob = item.unwrap();
                matches!(blob.get_type(), BlobType::OsmData).then(|| charge(blob.retained_len()))
            })
            .sum::<u64>();
        assert!(
            raw_charge > RAW_INFLIGHT_BUDGET,
            "fixture must take flush-before-blocking path"
        );
        completes("default-cap partial batch", || {
            ElementReader::new(Cursor::new(pbf_with_partial_then_near_cap_blob()))
                .unwrap()
                .batched_pipeline(true)
                .decode_threads(1)
                .for_each_block_pipelined(|_| Ok(()))
                .unwrap();
        });
    }

    #[test]
    fn early_consumer_drop_wakes_pump_blocked_in_raw_budget() {
        completes("raw-budget early drop", || {
            let mut blocks = ElementReader::new(Cursor::new(pbf(128)))
                .unwrap()
                .batched_pipeline(true)
                .decode_threads(1)
                .read_ahead_bytes(1)
                .into_blocks_pipelined();
            let _ = blocks.next();
            drop(blocks);
        });
    }

    #[test]
    fn early_consumer_drop_wakes_pump_blocked_in_decoded_budget() {
        completes("decoded-budget early drop", || {
            let mut blocks = ElementReader::new(Cursor::new(pbf(BATCH_MAX_BLOBS * 2)))
                .unwrap()
                .batched_pipeline(true)
                .decode_threads(1)
                .decode_ahead_bytes(1)
                .into_blocks_pipelined();
            let _ = blocks.next();
            drop(blocks);
        });
    }

    #[test]
    fn direct_error_follows_earlier_batches_in_sequence() {
        completes("direct read error sequencing", || {
            let mut bytes = pbf(BATCH_MAX_BLOBS + 1);
            bytes.truncate(bytes.len() - 1);
            let mut delivered = 0;
            let result = ElementReader::new(Cursor::new(bytes))
                .unwrap()
                .batched_pipeline(true)
                .decode_threads(1)
                .for_each_block_pipelined(|_| {
                    delivered += 1;
                    Ok(())
                });
            assert!(result.is_err());
            assert_eq!(delivered, BATCH_MAX_BLOBS);
        });
    }
}
