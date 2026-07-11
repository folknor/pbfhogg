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
use std::cell::RefCell;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Instant;

type RawItem = (
    usize,
    crate::error::Result<crate::blob::Blob>,
    Option<BytePermit>,
);
type DecodedPayload = Option<crate::error::Result<PrimitiveBlock>>;
type DecodedItem = (usize, DecodedPayload, Option<Permit>, Option<BytePermit>);

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

/// Number of raw blobs the I/O thread can read ahead.
pub(crate) const DEFAULT_READ_AHEAD: usize = 16;

/// Number of decoded blocks that can be in-flight before backpressure stalls decode.
pub(crate) const DEFAULT_DECODE_AHEAD: usize = 32;

const COUNT_BACKSTOP_MULTIPLIER: usize = 16;

// The overnight gates use 87 GiB / 50,816 planet-primary blobs = 1,838,309
// bytes/blob: read-ahead 16 * 1,838,309 = 29,412,944 and decode-ahead
// 32 * 1,838,309 = 58,825,888. The byte budgets preserve today's primary
// working sets while the raised count backstops admit the smaller 8k blobs.

fn byte_budget_from_env(var: &str) -> Result<Option<usize>> {
    match std::env::var(var) {
        Ok(value) => {
            let bytes = value.parse::<usize>().map_err(|_| {
                crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{var} must be a non-zero byte count, got {value:?}"),
                )))
            })?;
            if bytes == 0 {
                return Err(crate::error::new_error(crate::error::ErrorKind::Io(
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("{var} must be a non-zero byte count, got {value:?}"),
                    ),
                )));
            }
            Ok(Some(bytes))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(crate::error::new_error(
            crate::error::ErrorKind::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{var} must be a non-zero Unicode byte count"),
            )),
        )),
    }
}

pub(crate) fn block_queue_bytes_from_env() -> Result<Option<usize>> {
    byte_budget_from_env("PBFHOGG_BLOCK_QUEUE_BYTES")
}

/// Bounds decode items admitted but not yet delivered from the reorder buffer.
///
/// Single-acquirer invariant: only the one stage-2 dispatcher thread calls
/// `acquire`. `release` can therefore use `notify_one`; adding a second
/// acquirer requires changing this to `notify_all` or per-waiter wakeups.
struct AdmissionGate {
    count: Mutex<usize>,
    cond: Condvar,
    cap: usize,
}

impl AdmissionGate {
    fn new(cap: usize) -> Self {
        Self {
            count: Mutex::new(0),
            cond: Condvar::new(),
            cap: cap.max(1),
        }
    }

    fn acquire(&self) -> bool {
        let mut count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        let mut blocked = false;
        while *count >= self.cap {
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
        self.cond.notify_one();
    }
}

struct Permit(Arc<AdmissionGate>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[cfg(feature = "test-hooks")]
pub(crate) mod test_hooks {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

    pub static BLOCK_DECODE_SEQ: AtomicUsize = AtomicUsize::new(usize::MAX);
    pub static BLOCKED_DECODE_READY: AtomicBool = AtomicBool::new(false);
    pub static RELEASE_BLOCKED_DECODE: AtomicBool = AtomicBool::new(false);
    pub static REORDER_FILLED_HIGH_WATER: AtomicUsize = AtomicUsize::new(0);
    pub static REORDER_WINDOW_HIGH_WATER: AtomicUsize = AtomicUsize::new(0);

    pub fn reset() {
        BLOCK_DECODE_SEQ.store(usize::MAX, Relaxed);
        BLOCKED_DECODE_READY.store(false, Relaxed);
        RELEASE_BLOCKED_DECODE.store(false, Relaxed);
        REORDER_FILLED_HIGH_WATER.store(0, Relaxed);
        REORDER_WINDOW_HIGH_WATER.store(0, Relaxed);
    }

    pub(crate) fn maybe_block_decode(seq: usize) {
        if BLOCK_DECODE_SEQ.load(Relaxed) != seq {
            return;
        }
        BLOCKED_DECODE_READY.store(true, Relaxed);
        while !RELEASE_BLOCKED_DECODE.load(Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub(crate) fn record_reorder_levels(filled: usize, window: usize) {
        cas_max_usize(&REORDER_FILLED_HIGH_WATER, filled);
        cas_max_usize(&REORDER_WINDOW_HIGH_WATER, window);
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
#[derive(Clone, Copy, Debug)]
pub(crate) struct PipelineConfig {
    pub(crate) read_ahead: usize,
    pub(crate) decode_ahead: usize,
    pub(crate) read_ahead_bytes: Option<usize>,
    pub(crate) decode_ahead_bytes: Option<usize>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            read_ahead: DEFAULT_READ_AHEAD,
            decode_ahead: DEFAULT_DECODE_AHEAD,
            read_ahead_bytes: None,
            decode_ahead_bytes: None,
        }
    }
}

impl PipelineConfig {
    pub(crate) fn from_env() -> Result<Self> {
        Ok(Self {
            read_ahead: DEFAULT_READ_AHEAD,
            decode_ahead: DEFAULT_DECODE_AHEAD,
            read_ahead_bytes: byte_budget_from_env("PBFHOGG_READ_AHEAD_BYTES")?,
            decode_ahead_bytes: byte_budget_from_env("PBFHOGG_DECODE_AHEAD_BYTES")?,
        })
    }

    pub(crate) fn read_ahead_byte_budget(mut self, bytes: usize) -> Self {
        self.read_ahead_bytes = Some(bytes.max(1));
        self
    }

    pub(crate) fn decode_ahead_byte_budget(mut self, bytes: usize) -> Self {
        self.decode_ahead_bytes = Some(bytes.max(1));
        self
    }

    fn effective_read_ahead(self) -> usize {
        if self.read_ahead_bytes.is_some() {
            DEFAULT_READ_AHEAD * COUNT_BACKSTOP_MULTIPLIER
        } else {
            self.read_ahead
        }
    }

    fn effective_decode_ahead(self) -> usize {
        if self.decode_ahead_bytes.is_some() {
            DEFAULT_DECODE_AHEAD * COUNT_BACKSTOP_MULTIPLIER
        } else {
            self.decode_ahead
        }
    }
}

pub(crate) struct ByteBudget {
    used: Mutex<usize>,
    cond: Condvar,
    cap: usize,
}

impl ByteBudget {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            used: Mutex::new(0),
            cond: Condvar::new(),
            cap: cap.max(1),
        }
    }

    pub(crate) fn acquire(self: &Arc<Self>, bytes: usize) -> BytePermit {
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        while *used != 0 && used.saturating_add(bytes) > self.cap {
            used = self.cond.wait(used).unwrap_or_else(PoisonError::into_inner);
        }
        *used = used.saturating_add(bytes);
        BytePermit {
            budget: Arc::clone(self),
            bytes,
        }
    }

    fn release(&self, bytes: usize) {
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        *used = used.saturating_sub(bytes);
        drop(used);
        self.cond.notify_one();
    }
}

pub(crate) struct BytePermit {
    budget: Arc<ByteBudget>,
    bytes: usize,
}

impl Drop for BytePermit {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

fn send_direct_error(tx: &SyncSender<DecodedItem>, seq: usize, e: crate::error::Error) -> bool {
    let t_send = Instant::now();
    let sent = tx.send((seq, Some(Err(e)), None, None)).is_ok();
    PIPELINE_METRICS
        .decoded_send_wait_ns
        .fetch_add(elapsed_ns_u64(t_send), Relaxed);
    sent
}

struct DecodeTask {
    seq: usize,
    blob: crate::blob::Blob,
    tx: SyncSender<DecodedItem>,
    buffer_pool: Arc<DecompressPool>,
    blob_filter: Option<Arc<BlobFilter>>,
    permit: Permit,
    decoded_byte_permit: Option<BytePermit>,
    shutdown: Arc<AtomicBool>,
}

fn spawn_decode_task(decode_pool: &rayon::ThreadPool, task: DecodeTask) {
    let DecodeTask {
        seq,
        blob,
        tx,
        buffer_pool,
        blob_filter,
        permit,
        decoded_byte_permit,
        shutdown,
    } = task;
    decode_pool.spawn(move || {
        #[cfg(feature = "test-hooks")]
        test_hooks::maybe_block_decode(seq);

        // Thread-local scratch buffers for parse_and_inline. Avoids allocating
        // fresh Vec<(u32, u32)> per blob.
        thread_local! {
            static ST_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
            static GR_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
        }
        let item =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match blob.get_type() {
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
            }));
        let item = match item {
            Ok(item) => item,
            Err(_) => Some(Err(crate::error::new_error(crate::error::ErrorKind::Io(
                std::io::Error::other("decode task panicked"),
            )))),
        };
        let t_send = Instant::now();
        if tx
            .send((seq, item, Some(permit), decoded_byte_permit))
            .is_err()
        {
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
    block_fn: &mut F,
) -> Result<()>
where
    F: FnMut(PrimitiveBlock) -> Result<()>,
{
    // Normal completion: the dispatcher drops its sender after raw EOF, task
    // clones finish, and recv returns Err once the last sender is gone.
    //
    // Early exit: returning from this helper drops `decoded_rx` before scoped
    // threads join. Blocked senders then fail, set shutdown, release permits,
    // wake the dispatcher, and let stage 1 stop at the raw channel.
    let mut pending: ReorderBuffer<(DecodedPayload, Option<Permit>, Option<BytePermit>)> =
        ReorderBuffer::with_capacity(decode_ahead);

    loop {
        let t_recv = Instant::now();
        let next = decoded_rx.recv();
        PIPELINE_METRICS
            .decoded_recv_wait_ns
            .fetch_add(elapsed_ns_u64(t_recv), Relaxed);
        let (seq, item, permit, byte_permit) = match next {
            Ok(pair) => pair,
            Err(_) => break,
        };
        pending.push(seq, (item, permit, byte_permit));
        PIPELINE_METRICS.record_reorder_levels(pending.filled_len(), pending.pending_len());
        #[cfg(feature = "test-hooks")]
        test_hooks::record_reorder_levels(pending.filled_len(), pending.pending_len());

        while let Some((item, permit, byte_permit)) = pending.pop_ready() {
            match item {
                Some(Ok(block)) => {
                    let result = block_fn(block);
                    drop(permit);
                    drop(byte_permit);
                    result?;
                }
                Some(Err(e)) => return Err(e),
                None => drop(permit),
            }
        }
    }
    Ok(())
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
    mut block_fn: F,
) -> Result<()>
where
    R: Read + Send,
    F: FnMut(PrimitiveBlock) -> Result<()>,
{
    // Enable tagdata parsing only when the filter needs tag key matching.
    // Enable indexdata parsing only when any filter is active (should_skip_blob
    // checks blob.index() for type + spatial filtering).
    let has_tag_filter = blob_filter.as_ref().is_some_and(BlobFilter::has_tag_filter);
    blob_reader.set_parse_tagdata(has_tag_filter);
    blob_reader.set_parse_indexdata(blob_filter.is_some());
    let blob_filter = blob_filter.map(Arc::new);
    let read_ahead = pipeline_config.effective_read_ahead();
    let decode_ahead = pipeline_config.effective_decode_ahead();
    let raw_budget = pipeline_config
        .read_ahead_bytes
        .map(ByteBudget::new)
        .map(Arc::new);
    let decoded_budget = pipeline_config
        .decode_ahead_bytes
        .map(ByteBudget::new)
        .map(Arc::new);
    let (raw_tx, raw_rx) = sync_channel::<RawItem>(read_ahead);
    let (decoded_tx, decoded_rx) = sync_channel::<DecodedItem>(decode_ahead);

    std::thread::scope(|scope| {
        // Stage 1: Sequential I/O reader thread
        let raw_budget_for_reader = raw_budget.clone();
        scope.spawn(move || {
            for (seq, blob_result) in blob_reader.enumerate() {
                let byte_permit = blob_result.as_ref().ok().and_then(|blob| {
                    raw_budget_for_reader.as_ref().map(|budget| {
                        budget.acquire(usize::try_from(blob.retained_len()).unwrap_or(usize::MAX))
                    })
                });
                let t_send = Instant::now();
                let send_result = raw_tx.send((seq, blob_result, byte_permit));
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
                std::thread::available_parallelism()
                    .map(|n| n.get().saturating_sub(2).max(1))
                    // If available_parallelism() is unsupported (e.g. some WASM runtimes),
                    // fall back to 4 threads as a reasonable default for most desktops.
                    .unwrap_or(4)
            });
            let decode_pool = match rayon::ThreadPoolBuilder::new()
                .num_threads(decode_threads)
                .build()
            {
                Ok(pool) => pool,
                Err(e) => {
                    let err = crate::error::new_error(crate::error::ErrorKind::Io(
                        std::io::Error::other(format!("failed to build decode pool: {e}")),
                    ));
                    drop(dispatch_tx.send((0, Some(Err(err)), None, None)));
                    return;
                }
            };
            let buffer_pool = DecompressPool::new();
            let gate = Arc::new(AdmissionGate::new(decode_ahead));
            let shutdown = Arc::new(AtomicBool::new(false));
            for (seq, blob_result, _raw_permit) in raw_rx {
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
                        // Reserve before dispatch, in sequence order. Reserving only
                        // after parallel decode can let later blocks fill a tiny byte
                        // budget and strand the earlier sequence behind the reorder
                        // buffer.
                        let decoded_byte_permit = decoded_budget
                            .as_ref()
                            .map(|budget| budget.acquire(blob.decoded_len_hint()));
                        PIPELINE_METRICS.decode_tasks.fetch_add(1, Relaxed);
                        spawn_decode_task(
                            &decode_pool,
                            DecodeTask {
                                seq,
                                blob,
                                tx: dispatch_tx.clone(),
                                buffer_pool: Arc::clone(&buffer_pool),
                                blob_filter: blob_filter.clone(),
                                permit,
                                decoded_byte_permit,
                                shutdown: Arc::clone(&shutdown),
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
            // dispatch_tx clone drops here
        });

        // Drop the original so the channel closes when all rayon task clones are done
        drop(decoded_tx);

        let result = drain_decoded(decoded_rx, decode_ahead, &mut block_fn);

        // Emit reader-pipeline counters before returning so that even
        // an error path produces sidecar data. Mirrors the writer's
        // WRITER_METRICS.emit() in flush().
        PIPELINE_METRICS.emit();
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn gate_blocks_at_cap_and_release_unblocks() {
        let gate = Arc::new(AdmissionGate::new(2));
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
    fn permit_drop_releases() {
        let gate = Arc::new(AdmissionGate::new(1));
        {
            assert!(!gate.acquire());
            let _permit = Permit(Arc::clone(&gate));
        }
        assert!(!gate.acquire());
        gate.release();
    }
}
