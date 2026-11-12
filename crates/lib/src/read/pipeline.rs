//! Three-stage pipelined PBF reader.
//!
//! Overlaps sequential I/O with parallel decompression and protobuf parsing,
//! delivering decoded `PrimitiveBlock`s to a caller-supplied closure in file order.

use super::blob::{BlobReader, BlobType, DecompressPool};
use super::block::PrimitiveBlock;
use super::pipeline_metrics::PipelineMetrics;
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

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
        invocation: &crate::host::Recorder,
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
            // Once per invocation: multi-pass commands open several
            // filtered pipelines over the same input.
            crate::host::diag_once!(
                diagnostics,
                invocation,
                Note,
                "reader.filter_without_indexdata",
                "Input has no indexdata on its first data blob; blob type and spatial filters cannot skip unindexed blobs"
            );
            invocation.counter("pipeline_filter_without_indexdata", 1);
        }
    }
}

/// One decode slot's result as it crosses to the consumer: `None` delivers
/// nothing (a skipped or non-data blob), and the permit rides with it until
/// the consumer has handled it.
type DecodedItem<T> = (usize, Option<Result<T>>, Option<Permit>);

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
    state: Mutex<GateState>,
    cond: Condvar,
    cap: NonZeroUsize,
    #[cfg(test)]
    observer: Condvar,
}

struct GateState {
    count: usize,
    #[cfg(test)]
    parked: crate::test_support::Parked,
}

impl AdmissionGate {
    fn new(cap: NonZeroUsize) -> Self {
        Self {
            state: Mutex::new(GateState {
                count: 0,
                #[cfg(test)]
                parked: crate::test_support::Parked::default(),
            }),
            cond: Condvar::new(),
            cap,
            #[cfg(test)]
            observer: Condvar::new(),
        }
    }

    fn acquire(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut blocked = false;
        while state.count >= self.cap.get() {
            blocked = true;
            #[cfg(test)]
            state.parked.enter(&self.observer);
            state = self
                .cond
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            #[cfg(test)]
            state.parked.leave();
        }
        state.count += 1;
        blocked
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            state.count > 0,
            "decode admission permit released below zero"
        );
        state.count -= 1;
        drop(state);
        self.cond.notify_all();
    }
}

struct Permit(Arc<AdmissionGate>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.release();
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
    /// Whether each decoded data block is checked against its blob's
    /// indexdata kind claim. Under [`KindClaims::Refuse`] the reader parses
    /// every blob's indexdata, and a decoded block holding an element of a
    /// kind other than the claim is refused inside the decode step, before
    /// the transform sees it (`IndexdataMismatch` with a `KindContradiction`,
    /// located at the blob). A blob the filter skips, or a transform declines
    /// to decode, is not checked: the refusal costs no decode. `Unchecked`
    /// (the default) checks nothing. Sequential iteration (`for_each`,
    /// `par_map_reduce`) never runs this engine and is unaffected.
    ///
    /// [`KindClaims::Refuse`]: crate::scan::classify::KindClaims::Refuse
    kind_claims: crate::scan::classify::KindClaims,
}

impl PipelineConfig {
    pub(crate) fn set_kind_claims(&mut self, claims: crate::scan::classify::KindClaims) {
        self.kind_claims = claims;
    }

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
            kind_claims: crate::scan::classify::KindClaims::Unchecked,
        }
    }
}

/// What the engine runs on a decode worker for each blob.
///
/// [`apply`](Self::apply) is called for every blob the reader yields after
/// the leading header, concurrently and in unspecified order (the consumer
/// still sees the results in file order), except a data blob the blob filter
/// skips, which never reaches it. It runs inside the engine's
/// `catch_unwind`, and an error it returns is located at the blob by the
/// engine.
///
/// The plain pipelined surfaces run [`Blocks`], which hands the decoded
/// block itself to the consumer; full-scan commands run their per-block
/// work here instead, so only the compact result crosses to the ordered
/// consumer (ADR-0009).
pub(crate) trait DecodeTransform: Sync {
    type Out: Send;

    /// Whether the reader parses each blob header's indexdata, so
    /// [`crate::blob::Blob::index`] answers in `apply`. With no blob filter
    /// set, the engine skips parsing it otherwise.
    const PARSE_INDEXDATA: bool;

    /// The slot's result: `None` delivers nothing for this blob. `decode`
    /// decodes the blob as a data block, or runs a projection over its
    /// decompressed bytes (both on the worker's reused scratch), and may be
    /// used only for an `OsmData` blob.
    fn apply(&self, blob: &crate::blob::Blob, decode: Decoder<'_>) -> Option<Result<Self::Out>>;
}

thread_local! {
    // Thread-local scratch buffers for a block's string table and group
    // ranges, one pair per decode worker, avoiding a fresh pair of
    // Vec<(u32, u32)> per blob. They live on the read's own decode pool,
    // which is joined (thread-local destructors included) before the read
    // ends, so what they retain lasts one read, never across reads; for
    // `into_blocks_pipelined` that read runs in the background, so it lasts
    // while a paused iterator holds the read open.
    //
    // They deliberately do not follow the release rule
    // (`commands::release_scratch` against `MAX_RETAINED_SCRATCH_CAPACITY`)
    // that apply-changes' worker scratch applies to its own copies of the
    // same two lists. On the measured planet file the largest any worker
    // reached was 2 MiB for the string table and 4 KiB for the groups
    // (`pipeline_scratch_*_capacity_peak_bytes` of a planet pipelined read,
    // bygg, 30 decode workers, at 82be10a4; table sizes are the data's),
    // so the 4 MiB line would never fire there: the rule frees nothing on
    // that input and the check would only be measured for its own cost.
    // What each worker keeps below the line is the working set the next
    // large block needs. Above the line, inputs the planet file does not
    // contain (an entry is at least two wire bytes, so a 32 MiB block can
    // grow the table to 128 MiB, four times its payload; hostile input, or
    // a writer packing far more strings per block) would leave each worker
    // that parsed one holding it for the rest of the read. Releasing would
    // narrow that from workers x size to min(workers, decode_ahead) x size,
    // a real gain only where decode_ahead is below the worker count (a
    // caller's setting, or a host with more than decode_ahead + 2 CPUs).
    // That is the cheap partial step if a bound on such input is wanted;
    // a whole-read memory budget would be the stronger control.
    //
    // What the whole pool holds is `pipeline_scratch_pool_capacity_peak_bytes`
    // (the per-worker maxima above bound one worker only). Planet
    // `inspect --extended` (bygg, 30 decode workers, on 26db96fe plus that
    // counter) measured 31.6 MiB of capacity against a 277 MiB peak anon.
    // Building a block's offset tables straight into its payload tail (a
    // count pass per block, or resumable scanning into a growing payload)
    // would remove this list only where the block is built: projections
    // and `validate_in_place` read the tables from scratch, and so does
    // that measured run, so it would remove none of those 31.6 MiB there.
    // On the block-building paths (plain pipelined reads, geocode pass 1,
    // time-filter, the fused altw and tags-filter transforms, getid's
    // accepted projections) it would stop a large table staying on every
    // worker that met one, but no peak there is set by this scratch, and it
    // would cost a count pass or extra payload growth on every block plus a
    // validator that reads the tables from two representations. Declined
    // on that evidence, not as impossible: a workload whose peak is this
    // scratch would reopen it.
    static ST_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
    static GR_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
}

/// A data blob's decode, as the engine runs it on a decode worker: the
/// decompression buffer from the run's pool, the worker's scratch (its
/// growth recorded for the invocation's peaks) and the run's kind-claim
/// refusal, whichever form the transform asks for.
pub(crate) struct Decoder<'a> {
    blob: &'a crate::blob::Blob,
    pool: &'a Arc<DecompressPool>,
    /// Refuse a decoded block that contradicts its blob's kind claim.
    refuse_kinds: bool,
    metrics: &'a PipelineMetrics,
    /// The read's running sum of its decode workers' scratch capacity.
    scratch_pool: &'a AtomicU64,
}

/// Byte capacity of a scratch list of `(u32, u32)` pairs.
fn scratch_bytes(list: &Vec<(u32, u32)>) -> usize {
    list.capacity().saturating_mul(size_of::<(u32, u32)>())
}

/// Records a worker's scratch growth over one blob when dropped, so growth
/// made before a transform panics (which the task's `catch_unwind` turns
/// into an error) is counted like any other: the lists keep that capacity
/// for the rest of the read.
struct ScratchGrowth<'s> {
    st: &'s mut Vec<(u32, u32)>,
    gr: &'s mut Vec<(u32, u32)>,
    before: (usize, usize),
    metrics: &'s PipelineMetrics,
    scratch_pool: &'s AtomicU64,
}

impl Drop for ScratchGrowth<'_> {
    fn drop(&mut self) {
        let after = (scratch_bytes(self.st), scratch_bytes(self.gr));
        if after != self.before {
            self.metrics
                .record_scratch_growth(self.scratch_pool, self.before, after);
        }
    }
}

impl Decoder<'_> {
    /// Run `f` on the worker's two scratch lists, recording what they grew
    /// by.
    fn with_scratch<R>(
        &self,
        f: impl FnOnce(&mut Vec<(u32, u32)>, &mut Vec<(u32, u32)>) -> R,
    ) -> R {
        ST_SCRATCH.with_borrow_mut(|st| {
            GR_SCRATCH.with_borrow_mut(|gr| {
                let before = (scratch_bytes(st), scratch_bytes(gr));
                let growth = ScratchGrowth {
                    st,
                    gr,
                    before,
                    metrics: self.metrics,
                    scratch_pool: self.scratch_pool,
                };
                f(&mut *growth.st, &mut *growth.gr)
            })
        })
    }

    /// The blob as a validated data block.
    #[expect(
        clippy::redundant_closure_for_method_calls,
        reason = "the method path `BlockBuffer::block` is not generic over the buffer's lifetime"
    )]
    pub(crate) fn block(self) -> Result<PrimitiveBlock> {
        self.buffer(|buffer| buffer.block())
    }

    /// Run `f` on the blob decompressed into a buffer from the run's pool,
    /// with the worker's string table and group scratch and the run's
    /// kind-claim refusal: `f` validates it as a block
    /// ([`BlockBuffer::block`](super::block::BlockBuffer::block)) or through
    /// a projection ([`BlockBuffer::project`](super::block::BlockBuffer::project)),
    /// which refuse the same blobs, and may build the block from a
    /// projection's validated bytes.
    pub(crate) fn buffer<R>(
        self,
        f: impl FnOnce(super::block::BlockBuffer<'_>) -> Result<R>,
    ) -> Result<R> {
        let buf = self.blob.decompress_pooled(self.pool)?;
        let claim = if self.refuse_kinds {
            self.blob.index().map(|index| index.kind)
        } else {
            None
        };
        self.with_scratch(|st, gr| {
            f(super::block::BlockBuffer::new(
                buf,
                Some(self.pool),
                st,
                gr,
                claim,
            ))
        })
    }
}

/// The identity transform: data blobs decode and the block itself is
/// delivered, every other blob is skipped. What the plain pipelined surfaces
/// (`for_each_block_pipelined`, `into_blocks_pipelined`) run.
pub(crate) struct Blocks;

impl DecodeTransform for Blocks {
    type Out = PrimitiveBlock;
    const PARSE_INDEXDATA: bool = false;

    #[inline]
    fn apply(
        &self,
        blob: &crate::blob::Blob,
        decode: Decoder<'_>,
    ) -> Option<Result<PrimitiveBlock>> {
        if blob.get_type() != BlobType::OsmData {
            return None;
        }
        Some(decode.block())
    }
}

/// The block-only command transform: data blobs decode and go through the
/// closure, every other blob is skipped. What
/// `ElementReader::for_each_fused_block` runs.
pub(crate) struct BlockOnly<X>(pub(crate) X);

impl<T, X> DecodeTransform for BlockOnly<X>
where
    T: Send,
    X: Fn(PrimitiveBlock) -> Result<T> + Sync,
{
    type Out = T;
    const PARSE_INDEXDATA: bool = false;

    #[inline]
    fn apply(&self, blob: &crate::blob::Blob, decode: Decoder<'_>) -> Option<Result<T>> {
        if blob.get_type() != BlobType::OsmData {
            return None;
        }
        Some(decode.block().and_then(&self.0))
    }
}

/// The projecting command transform: each data blob's decompressed buffer
/// goes to the closure, which validates it through a projection (and may
/// build the block from the validated bytes); every other blob is skipped.
/// What `ElementReader::for_each_fused_buffer` runs.
pub(crate) struct BufferOnly<X>(pub(crate) X);

impl<T, X> DecodeTransform for BufferOnly<X>
where
    T: Send,
    X: Fn(super::block::BlockBuffer<'_>) -> Result<T> + Sync,
{
    type Out = T;
    const PARSE_INDEXDATA: bool = false;

    #[inline]
    fn apply(&self, blob: &crate::blob::Blob, decode: Decoder<'_>) -> Option<Result<T>> {
        if blob.get_type() != BlobType::OsmData {
            return None;
        }
        Some(decode.buffer(&self.0))
    }
}

/// Forwards a reader error as a sequenced item. The wait is recorded whether
/// or not the send succeeds, like every other timed send in the engine.
fn send_direct_error<T>(
    tx: &SyncSender<DecodedItem<T>>,
    seq: usize,
    e: crate::error::Error,
    metrics: &PipelineMetrics,
) -> bool {
    let t_send = crate::counters::Stopwatch::start();
    let sent = tx.send((seq, Some(Err(e)), None)).is_ok();
    t_send.record_ns(&metrics.decoded_send_wait_ns);
    sent
}

struct DecodeTask<'scope, X: DecodeTransform> {
    seq: usize,
    blob: crate::blob::Blob,
    tx: SyncSender<DecodedItem<X::Out>>,
    buffer_pool: Arc<DecompressPool>,
    blob_filter: Option<Arc<BlobFilter>>,
    /// Refuse a decoded block that contradicts its blob's kind claim.
    refuse_kinds: bool,
    permit: Permit,
    shutdown: Arc<AtomicBool>,
    /// The run's transform, borrowed from the caller for the whole scope.
    transform: &'scope X,
    /// The path of the reader the blob came from, borrowed from the
    /// dispatcher (captured once per pipeline, no per-blob refcounting) so a
    /// decode failure can be located with the blob's true provenance.
    source: Option<&'scope Arc<Path>>,
    /// The invocation's pipeline counters, borrowed like `source`.
    metrics: &'scope PipelineMetrics,
    /// The read's running scratch sum, borrowed like `source`.
    scratch_pool: &'scope AtomicU64,
    /// The run's fault plan (the decode gate), borrowed like `source`.
    #[cfg(pbfhogg_test_hooks)]
    faults: &'scope crate::test_hooks::FaultPlan,
}

fn spawn_decode_task<'scope, X: DecodeTransform>(
    scope: &rayon::Scope<'scope>,
    task: DecodeTask<'scope, X>,
) {
    let DecodeTask {
        seq,
        blob,
        tx,
        buffer_pool,
        blob_filter,
        refuse_kinds,
        permit,
        shutdown,
        transform,
        source,
        metrics,
        scratch_pool,
        #[cfg(pbfhogg_test_hooks)]
        faults,
    } = task;
    scope.spawn(move |_| {
        let item = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Inside catch_unwind so the hook's park-timeout panic becomes a
            // decode error on the pipeline instead of escaping the rayon
            // spawn, which has no panic handler and would abort the process.
            #[cfg(pbfhogg_test_hooks)]
            faults.maybe_block_decode(seq);
            if blob.get_type() == BlobType::OsmData
                && let Some(filter) = blob_filter.as_ref()
                && should_skip_blob(filter, &blob)
            {
                metrics.blobs_skipped_by_filter.fetch_add(1, Relaxed);
                return None;
            }
            let decode = Decoder {
                blob: &blob,
                pool: &buffer_pool,
                refuse_kinds,
                metrics,
                scratch_pool,
            };
            transform.apply(&blob, decode)
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
        let t_send = crate::counters::Stopwatch::start();
        if tx.send((seq, item, Some(permit))).is_err() {
            shutdown.store(true, Relaxed);
        }
        t_send.record_ns(&metrics.decoded_send_wait_ns);
    });
}

#[allow(clippy::needless_pass_by_value)]
fn drain_decoded<T, F>(
    decoded_rx: Receiver<DecodedItem<T>>,
    decode_ahead: usize,
    cancel: &crate::host::Recorder,
    consume: &mut F,
) -> Result<()>
where
    F: FnMut(T) -> Result<()>,
{
    // Cancellation is checked here, on the consumer, before each wait and on
    // each arrival (a gap in the reorder buffer can hold back every pop, so a
    // pop-only check could starve), and returns through the same early exit
    // as a `consume` error. Stage 1's reader also stops on the token and
    // delivers `Cancelled` as a sequenced error item; this check only makes
    // the stop independent of how many decoded items are queued ahead of it.
    // Normal completion: the dispatcher drops its sender after raw EOF, task
    // clones finish, and recv returns Err once the last sender is gone.
    //
    // Early exit: returning from this helper drops `decoded_rx` before scoped
    // threads join. Blocked senders then fail, set shutdown, release permits,
    // wake the dispatcher, and let stage 1 stop at the raw channel.
    // The admission gate is this buffer's bound, not its ceiling: at most
    // `decode_ahead` decode tasks are admitted and not yet delivered, so
    // `with_capacity`'s generous runaway ceiling is a safety net that never
    // fires, and `decode_ahead` also sizing the decoded channel adds nothing
    // the gate does not already cap. Deliberately not pinned with
    // `with_max_window`: a reader error item rides without a permit, and an
    // exact ceiling would turn any miscount in that reasoning into a panic on
    // the hottest read path for no bound the gate does not already give.
    // This engine keeps its own permit gate rather than the ordered pool's
    // (`crate::ordered_pool`): the permit travels with each decoded item and
    // is released on delivery or drop, which fits rayon tasks and filtered
    // items.
    let mut pending: ReorderBuffer<(Option<Result<T>>, Option<Permit>)> =
        ReorderBuffer::with_capacity(decode_ahead);

    let metrics = &cancel.metrics().pipeline;
    loop {
        cancel.check()?;
        let t_recv = crate::counters::Stopwatch::start();
        let next = decoded_rx.recv();
        t_recv.record_ns(&metrics.decoded_recv_wait_ns);
        let (seq, item, permit) = match next {
            Ok(pair) => pair,
            Err(_) => break,
        };
        cancel.check()?;
        pending.push(seq, (item, permit));
        metrics.record_reorder_levels(pending.filled_len(), pending.pending_len());

        while let Some((item, permit)) = pending.pop_ready() {
            match item {
                Some(Ok(item)) => {
                    let result = consume(item);
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
/// 2. **Rayon pool**: parallel decompression (zlib) + protobuf parse, then
///    the [`DecodeTransform`] on the same worker. The pool is built for this
///    run and joined before it returns, thread-local destructors included.
/// 3. **Main thread**: reorder buffer delivers the transform's results in
///    file order to `consume`.
///
/// This is the one ordered read engine: the plain pipelined surfaces run it
/// with [`Blocks`] (the decoded block itself is the result), full-scan
/// commands with their own transform. `consume` runs on the calling thread
/// and may hold mutable state. PBF ordering (nodes → ways → relations) is
/// preserved.
///
/// # Memory: decoded-block working set
///
/// Each decoded `PrimitiveBlock` is built through `BlockBuffer`: validation
/// records the string-table entries and group ranges as `(u32, u32)` pairs in
/// reused thread-local scratch, construction appends them as bytes to the end
/// of the decompressed buffer (`WireBlock::inline_tables`), and the buffer
/// returns to `DecompressPool` on drop. There is
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
/// The bound counts items, not bytes: a transform whose output is larger
/// than its input block (add-locations-to-ways' decode-all fallback) holds
/// up to `decode_ahead` of its outputs, which the admission window does not
/// size. Backpressure from a slow consumer propagates through the decoded
/// channel to the admission gate, then to the raw channel and stage 1.
#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::too_many_arguments
)]
#[hotpath::measure]
pub(crate) fn run_pipeline<R, X, F>(
    mut blob_reader: BlobReader<R>,
    rec: &crate::host::Recorder,
    decode_thread_count: Option<usize>,
    pipeline_config: PipelineConfig,
    blob_filter: Option<BlobFilter>,
    resources: Option<crate::host::ResourcesHandle>,
    diagnostics: crate::host::Diagnostics,
    transform: X,
    mut consume: F,
) -> Result<()>
where
    R: Read + Send,
    X: DecodeTransform,
    F: FnMut(X::Out) -> Result<()>,
{
    // Borrowed by every decode task for the whole scope.
    let transform = &transform;
    // Enable tagdata parsing only when the filter needs tag key matching.
    // Enable indexdata parsing when any filter is active (should_skip_blob
    // checks blob.index() for type + spatial filtering) or the transform
    // reads it.
    let mut unindexed = UnindexedFilterNote::new(blob_filter.as_ref());
    let has_tag_filter = blob_filter.as_ref().is_some_and(BlobFilter::has_tag_filter);
    blob_reader.set_parse_tagdata(has_tag_filter);
    let refuse_kinds = pipeline_config.kind_claims == crate::scan::classify::KindClaims::Refuse;
    blob_reader.set_parse_indexdata(blob_filter.is_some() || X::PARSE_INDEXDATA || refuse_kinds);
    let blob_filter = blob_filter.map(Arc::new);
    let source = blob_reader.source().cloned();
    // The capability the caller runs the read under (a reader method's call,
    // a block iterator thread's permit), borrowed by every stage through
    // teardown. Stage 1 walks without a call of its own, so a read blocked
    // in a caller's stream holds no admission.
    let cancel = rec;
    // Borrowed into the decode tasks; the dispatcher closure moves the
    // reference, not the token.
    #[cfg(pbfhogg_test_hooks)]
    let faults = cancel.faults();
    let metrics: &PipelineMetrics = &cancel.metrics().pipeline;
    // The decode pool's buffers count their drops into the same invocation.
    let invocation = cancel;
    let (raw_tx, raw_rx) = sync_channel::<RawItem>(pipeline_config.read_ahead.get());
    let (decoded_tx, decoded_rx) =
        sync_channel::<DecodedItem<X::Out>>(pipeline_config.decode_ahead.get());

    // This read's decode workers' scratch capacity, summed as it grows
    // (`PipelineMetrics::record_scratch_growth`); per read, because each
    // read has its own decode pool.
    let scratch_pool = &AtomicU64::new(0);
    // Declared before the scope so it drops after the scope has joined.
    let _emit = EmitOnDrop(metrics, invocation);
    let result = crate::threads::scope(|scope| {
        // Stage 1: Sequential I/O reader thread
        scope.spawn(move || {
            for (seq, blob_result) in std::iter::from_fn(|| blob_reader.next_blob()).enumerate() {
                unindexed.observe(&blob_result, &diagnostics, invocation);
                let t_send = crate::counters::Stopwatch::start();
                let send_result = raw_tx.send((seq, blob_result));
                t_send.record_ns(&metrics.raw_send_wait_ns);
                if send_result.is_err() {
                    break; // receiver dropped, pipeline shutting down
                }
            }
        });

        // Stage 2: Dispatcher thread - fans out to dedicated pool for parallel decode.
        //
        // We use a dedicated rayon pool rather than the global rayon pool so that
        // the caller's own parallelism (e.g. geometry processing, tile generation)
        // is not starved by decode work. The global pool is left entirely free,
        // and so is the host's `Executor`: this pool is a dedicated per-read
        // worker set, which the `Executor` rustdoc names as outside it. It is
        // a `JoinedPool` owned by this closure: when the closure ends (on
        // every path) its workers are joined, thread-local destructors
        // included, so none outlives the read. The scope below waits for this
        // thread, and so for that join, before `run_pipeline` returns.
        //
        // Thread count rationale:
        //   The pipeline occupies 2 threads beyond this pool: the Stage 1 I/O reader
        //   thread and the Stage 3 main/consumer thread (which runs `consume`).
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
            let decode_pool = match crate::joined_pool::JoinedPool::build(
                "decode pool",
                decode_threads,
                invocation,
            ) {
                Ok(pool) => pool,
                Err(err) => {
                    // The build has joined the threads it spawned. Seq 0 is
                    // correct here, not a magic value that can collide
                    // with a real item: the pool is built before the loop over
                    // `raw_rx` below, so nothing has been dispatched yet and 0
                    // is legitimately the reorder buffer's first push. Keep the
                    // build above the loop if this is ever restructured.
                    drop(dispatch_tx.send((0, Some(Err(err)), None)));
                    return;
                }
            };
            // Rebound after the pool so that on an unwind too the sender drops
            // (closing the decoded channel) before the pool is joined.
            let dispatch_tx = dispatch_tx;
            let buffer_pool = DecompressPool::new(invocation);
            let gate = Arc::new(AdmissionGate::new(pipeline_config.decode_ahead));
            let shutdown = Arc::new(AtomicBool::new(false));
            // Tasks borrow the reader's path from this scope: one capture per
            // pipeline, no refcounting per blob.
            let source = source.as_ref();
            #[allow(
                clippy::disallowed_methods,
                reason = "the read's own joined decode pool, not the host executor"
            )]
            decode_pool.pool().in_place_scope(|task_scope| {
                for (seq, blob_result) in raw_rx {
                    if shutdown.load(Relaxed) {
                        break;
                    }
                    match blob_result {
                        Ok(blob) => {
                            let t_admit = crate::counters::Stopwatch::start();
                            let blocked = gate.acquire();
                            t_admit.record_ns(&metrics.decode_admit_wait_ns);
                            if blocked {
                                metrics.decode_admit_blocked.fetch_add(1, Relaxed);
                            }
                            let permit = Permit(Arc::clone(&gate));
                            metrics.decode_tasks.fetch_add(1, Relaxed);
                            spawn_decode_task(
                                task_scope,
                                DecodeTask {
                                    seq,
                                    blob,
                                    tx: dispatch_tx.clone(),
                                    buffer_pool: Arc::clone(&buffer_pool),
                                    blob_filter: blob_filter.clone(),
                                    refuse_kinds,
                                    permit,
                                    shutdown: Arc::clone(&shutdown),
                                    transform,
                                    source,
                                    metrics,
                                    scratch_pool,
                                    #[cfg(pbfhogg_test_hooks)]
                                    faults,
                                },
                            );
                        }
                        Err(e) => {
                            if !send_direct_error(&dispatch_tx, seq, e, metrics) {
                                break;
                            }
                        }
                    }
                }
            });
            // The decoded channel closes once every task clone is gone too;
            // then the pool's workers are joined (on an unwind, the same two
            // drops happen in the same order, by declaration order).
            drop(dispatch_tx);
            drop(decode_pool);
        });

        // Drop the original so the channel closes when all rayon task clones are done
        drop(decoded_tx);

        drain_decoded(
            decoded_rx,
            pipeline_config.decode_ahead.get(),
            cancel,
            &mut consume,
        )
    });
    // `_emit` drops here, after the scope has joined every stage, on every
    // path: success, error, and a stage thread's panic re-raised by the scope
    // (the guard emits on unwind). So the snapshot includes the waits
    // recorded while the stages shut down (an error, a cancellation or an
    // early drop wakes blocked senders, which record their waits after the
    // consumer has returned). The invocation's pool-drop totals are emitted
    // separately, after dispatch.
    result
}

/// Emits the invocation's [`PipelineMetrics`] when dropped: after a run's
/// scope has joined its stages, whether the run returned or unwound. The same
/// mechanism as the ordered pool engine's and the unordered classify pools'
/// guards.
struct EmitOnDrop<'a>(&'a PipelineMetrics, &'a crate::host::Recorder);

impl Drop for EmitOnDrop<'_> {
    fn drop(&mut self) {
        self.0.emit(self.1);
    }
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

    /// Run `f` under a fail-stop hang bound: a hang ends the test binary
    /// with `label` and the test's name instead of detaching a hung thread.
    fn assert_completes<T>(label: &str, f: impl FnOnce() -> T) -> T {
        let _bound = pbfhogg_test_scratch::HangBound::arm(
            crate::tunables::TEST_HANG_BOUND,
            crate::tunables::TEST_HANG_REPORT_GRACE,
            label,
        );
        f()
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
        ElementReader::for_tests(Cursor::new(bytes.clone()))
            .unwrap()
            .for_each_block_pipelined(|block| {
                pipelined.push(block.elements().count());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            fused_counts(ElementReader::for_tests(Cursor::new(bytes)).unwrap()).unwrap(),
            pipelined
        );
    }

    #[test]
    fn fused_transform_error_surfaces_after_prior_blocks() {
        let mut consumed = Vec::new();
        let result = ElementReader::for_tests(Cursor::new(fused_pbf(4)))
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
            ElementReader::for_tests(Cursor::new(fused_pbf(1)))
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
        let result = ElementReader::for_tests(CountingRead::new(bytes, Arc::clone(&bytes_read)))
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
                ElementReader::for_tests(Cursor::new(fused_pbf(3)))
                    .unwrap()
                    .read_ahead(1)
                    .decode_ahead(1)
            )
            .unwrap(),
            [1, 1, 1]
        );
    }

    /// Release the transform before joining, including when observing the
    /// stall times out and a decode worker reaches the transform later.
    struct ReleaseTransform {
        release: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for ReleaseTransform {
        fn drop(&mut self) {
            self.release.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                drop(handle.join());
            }
        }
    }

    #[test]
    fn stalled_transform_guard_releases_and_joins_on_unwind() {
        let release = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let worker_release = Arc::clone(&release);
        let worker_finished = Arc::clone(&finished);
        let result = std::panic::catch_unwind(move || {
            let handle = std::thread::spawn(move || {
                // This worker may only get scheduled after the assertion
                // unwinds: release must remain set for that late arrival.
                while !worker_release.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                worker_finished.store(true, Ordering::Relaxed);
            });
            let _release = ReleaseTransform {
                release,
                handle: Some(handle),
            };
            panic!("failed before observing the stalled transform");
        });
        assert!(result.is_err());
        assert!(finished.load(Ordering::Relaxed), "worker joined on unwind");
    }

    #[test]
    fn fused_stalled_transform_preserves_order() {
        let stalled = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let worker_stalled = Arc::clone(&stalled);
        let worker_release = Arc::clone(&release);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            let mut ids = Vec::new();
            let result = ElementReader::for_tests(Cursor::new(fused_pbf(3)))
                .unwrap()
                .decode_threads(2)
                .for_each_fused_block(
                    |block| {
                        let id = match block.elements().next().expect("fixture element") {
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
            done_tx.send((result, ids)).expect("result receiver open");
        });
        let _release = ReleaseTransform {
            release: Arc::clone(&release),
            handle: Some(handle),
        };
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

    /// What a decode worker's thread-local holds, torn down when its thread
    /// exits: it parks (bounded) until the test releases it.
    #[cfg(pbfhogg_test_hooks)]
    struct Teardown(Arc<TeardownState>);

    #[cfg(pbfhogg_test_hooks)]
    #[derive(Default)]
    struct TeardownState {
        installed: AtomicUsize,
        started: AtomicUsize,
        finished: AtomicUsize,
        release: AtomicBool,
    }

    #[cfg(pbfhogg_test_hooks)]
    impl Drop for Teardown {
        fn drop(&mut self) {
            self.0.started.fetch_add(1, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !self.0.release.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.0.finished.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Releases every parked teardown and joins the reading thread on every
    /// exit path, a failed assertion included.
    #[cfg(pbfhogg_test_hooks)]
    struct ReleaseTeardowns(Arc<TeardownState>, Option<std::thread::JoinHandle<()>>);

    #[cfg(pbfhogg_test_hooks)]
    impl Drop for ReleaseTeardowns {
        fn drop(&mut self) {
            self.0.release.store(true, Ordering::SeqCst);
            if let Some(handle) = self.1.take() {
                drop(handle.join());
            }
        }
    }

    /// The decode pool is joined before the read returns, thread teardown
    /// included: a worker parked in a thread-local destructor keeps the call
    /// from returning until it is released, so nothing a worker's
    /// thread-locals hold outlives the read. The release waits until the
    /// read has reached its decode pool's join (the pool's test hook), so
    /// the read is provably at its join while a teardown is still parked.
    #[cfg(pbfhogg_test_hooks)]
    #[test]
    fn a_decode_worker_tearing_down_holds_the_read() {
        thread_local! {
            static TEARDOWN: RefCell<Option<Teardown>> = const { RefCell::new(None) };
        }
        let state = Arc::new(TeardownState::default());
        let worker_state = Arc::clone(&state);
        let invocation = crate::host::Invocation::new();
        let reader_invocation = invocation.clone();
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            let result = ElementReader::for_tests(Cursor::new(fused_pbf(8)))
                .unwrap()
                .invocation(reader_invocation)
                .decode_threads(2)
                .for_each_fused_block(
                    |_| {
                        TEARDOWN.with_borrow_mut(|slot| {
                            if slot.is_none() {
                                worker_state.installed.fetch_add(1, Ordering::SeqCst);
                                *slot = Some(Teardown(Arc::clone(&worker_state)));
                            }
                        });
                        Ok::<_, crate::Error>(())
                    },
                    |()| Ok(()),
                );
            let finished_at_return = worker_state.finished.load(Ordering::SeqCst);
            done_tx.send((result, finished_at_return)).unwrap();
        });
        let _release = ReleaseTeardowns(Arc::clone(&state), Some(handle));

        let start = std::time::Instant::now();
        while state.started.load(Ordering::SeqCst) == 0
            || invocation
                .faults()
                .built_pool_joins_started
                .load(Ordering::SeqCst)
                == 0
        {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "no decode worker began tearing down, or the read never reached its join"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        state.release.store(true, Ordering::SeqCst);
        let (result, finished_at_return) = done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        result.unwrap();
        assert_eq!(
            finished_at_return,
            state.installed.load(Ordering::SeqCst),
            "every decode worker's teardown ends before the read returns"
        );
    }

    /// The pool peak is the largest single read's sum over its workers,
    /// never a sum across reads, and the per-worker maxima stay one
    /// worker's.
    #[test]
    fn scratch_pool_peak_is_the_largest_reads_worker_sum() {
        let metrics = PipelineMetrics::new();
        let first = AtomicU64::new(0);
        metrics.record_scratch_growth(&first, (0, 0), (64, 8));
        metrics.record_scratch_growth(&first, (0, 0), (32, 8));
        metrics.record_scratch_growth(&first, (32, 8), (128, 8));
        let second = AtomicU64::new(0);
        metrics.record_scratch_growth(&second, (0, 0), (100, 0));
        let peak = |c: &AtomicU64| c.load(Ordering::Relaxed);
        assert_eq!(
            peak(&metrics.scratch_pool_capacity_peak_bytes),
            64 + 8 + 128 + 8
        );
        assert_eq!(peak(&metrics.scratch_st_capacity_peak_bytes), 128);
        assert_eq!(peak(&metrics.scratch_gr_capacity_peak_bytes), 8);
    }

    /// Growth made before a transform panics is still counted: the worker
    /// keeps that capacity for the rest of the read.
    #[test]
    fn scratch_growth_before_a_panic_is_counted() {
        let metrics = PipelineMetrics::new();
        let pool = AtomicU64::new(0);
        let mut st = Vec::new();
        let mut gr = Vec::new();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let growth = ScratchGrowth {
                st: &mut st,
                gr: &mut gr,
                before: (0, 0),
                metrics: &metrics,
                scratch_pool: &pool,
            };
            growth.st.reserve_exact(16);
            panic!("transform failed after growing the scratch");
        }));
        assert!(outcome.is_err());
        let bytes = st.capacity() * size_of::<(u32, u32)>();
        assert!(bytes >= 128);
        assert_eq!(pool.load(Ordering::Relaxed), bytes as u64);
        assert_eq!(
            metrics
                .scratch_pool_capacity_peak_bytes
                .load(Ordering::Relaxed),
            bytes as u64
        );
    }

    #[test]
    fn fused_thread_count_parity() {
        let one = fused_counts(
            ElementReader::for_tests(Cursor::new(fused_pbf(8)))
                .unwrap()
                .decode_threads(1),
        )
        .unwrap();
        let many = fused_counts(
            ElementReader::for_tests(Cursor::new(fused_pbf(8)))
                .unwrap()
                .decode_threads(8),
        )
        .unwrap();
        assert_eq!(one, many);
    }

    /// A node blob, then a way blob whose indexdata claims nodes.
    fn pbf_with_a_way_blob_claiming_nodes() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut writer = PbfWriter::new(&mut bytes, Compression::None);
            writer
                .write_header(&HeaderBuilder::new().build().unwrap())
                .unwrap();
            let mut nodes = BlockBuilder::new();
            nodes.add_node(1, 0, 0, std::iter::empty::<(&str, &str)>(), None);
            writer
                .write_primitive_block(nodes.take().unwrap().unwrap())
                .unwrap();
            let mut ways = BlockBuilder::new();
            ways.add_way(5, std::iter::empty::<(&str, &str)>(), &[1], None);
            let mut lying = ways.take_owned().unwrap().unwrap();
            lying.index.kind = crate::ElementKind::Node;
            writer.write_primitive_block_owned(lying).unwrap();
            writer.flush().unwrap();
        }
        bytes
    }

    /// The pipelined adoption seam refuses a decoded block that contradicts
    /// its blob's kind claim, naming the claim and the blob; without it the
    /// same read delivers the block, and a kind filter that keeps the blob
    /// (it claims nodes) is what a believing caller would have used.
    #[test]
    fn refusing_kind_claims_refuses_a_block_holding_another_kind() {
        let bytes = pbf_with_a_way_blob_claiming_nodes();

        let unchecked = fused_counts(ElementReader::for_tests(Cursor::new(bytes.clone())).unwrap());
        assert_eq!(unchecked.unwrap(), [1, 1], "unchecked reads deliver both");

        for filtered in [false, true] {
            let mut reader = ElementReader::for_tests(Cursor::new(bytes.clone()))
                .unwrap()
                .refusing_kind_claims();
            if filtered {
                reader = reader.with_blob_filter(BlobFilter::new(true, false, false));
            }
            let err = fused_counts(reader).unwrap_err();
            assert_eq!(
                err.kind_contradiction(),
                Some(crate::KindContradiction {
                    claimed: crate::ElementKind::Node,
                    found: crate::ElementKind::Way,
                }),
                "filtered {filtered}: {err}"
            );
            assert_eq!(err.blob_index(), Some(2), "filtered {filtered}: {err}");
        }
    }

    #[test]
    fn fused_blobfilter_skips_blobs() {
        let reader = ElementReader::for_tests(Cursor::new(fused_pbf(2)))
            .unwrap()
            .with_blob_filter(BlobFilter::new(false, true, false));
        assert!(fused_counts(reader).unwrap().is_empty());
    }

    #[test]
    fn gate_blocks_at_cap_and_release_unblocks() {
        let gate = Arc::new(AdmissionGate::new(NonZeroUsize::new(2).unwrap()));
        assert!(!gate.acquire());
        assert!(!gate.acquire());

        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let child_gate = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            let blocked = child_gate.acquire();
            done_tx.send(blocked).expect("done channel open");
        });

        // The child found the gate full and parked, so the release below is
        // what admits it, and `acquire` reports that it blocked.
        drop(crate::test_support::wait_parked(
            &gate.state,
            &gate.observer,
            |s| &s.parked,
            1,
        ));
        gate.release();
        match done_rx.recv_timeout(crate::test_support::OBSERVE_BOUND) {
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
