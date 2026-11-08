//! Every performance and behaviour tunable of the library, in one place.
//!
//! "What are the tunables?" has one answer: this file. Each constant keeps the
//! rationale it had where it was first needed. Sections are by scope.
//!
//! What does not live here: format and protocol constants fixed by a spec or
//! an on-disk layout (protobuf field numbers and wire tags, geocode record
//! sizes, `MAX_BLOB_DATASIZE`-style format limits, compact record offsets,
//! header feature strings). Those stay beside the code that encodes them.
//!
//! Merge-changes has no section of its own: its tunables are the worker
//! triple, the streaming batch size and in-flight budget, and the idle
//! `recv_timeout` below. The simplify path's chunking rule (one chunk per
//! worker) selects its gzip member layout, which makes it an output contract
//! fact rather than a tunable, so it stays in `merge_changes`.
//!
//! Public tunables (`pub const`) are re-exported from the path they were
//! public at, so no public path changed.
//!
//! Centralized is not configurable. These stay constants that the engines
//! read directly; there is deliberately no per-scope tuning struct (a read,
//! writer, pipeline, altw or geocode "tuning" passed in by the caller). Some
//! values were measured into place (the parallel writer's pool size came from
//! a pool-size sweep, several worker caps and walker thresholds from planet
//! runs), others are stated defaults (the ordered pool's window per worker
//! says so); a public knob would make each a contract its value then has to
//! defend, and would turn compile-time constants into runtime values on hot
//! paths. That second point is a reason not to make the change unmeasured,
//! not a finding that runtime values would be slower: nothing was measured
//! for this decision. Public configuration gets added for a named consumer
//! requirement, not to mirror this file.
//!
//! Tests reach small configurations through the seams that already exist:
//! `host::Resources` (`host::Fixed`) and `host::Parallelism` for worker counts
//! (each command keeps its own minimum, and the parallel writer always runs
//! its fixed pool), the engines' own parameters (`ordered_pool::PoolConfig`,
//! the private `par_fold_blobs` budget and batch sizes), and crate-private
//! dispatch functions for thresholds. When a constant blocks a named
//! small-fixture test, prefer a narrow private parameter at the mechanism that
//! owns it. A test instrument public only because its callers live in another
//! crate (`getparents_with_min_blobs`, behind the CLI's hidden
//! `--full-scan-min-blobs`) is a documented exception, hidden and unsupported.
//! Known and accepted: altw external's small fixtures still open all
//! `NUM_BUCKETS` id-shard files per worker, which costs fixture time, not
//! coverage. No tunable reads the environment (ADR-0016).

// ---------------------------------------------------------------------------
// Worker sizing
//
// `host::sized_workers(resources, reserve, min, fallback)` computes
// `parallelism - reserve`, floored at `min`, or `fallback` when the host
// reports no parallelism. Each call-site family has its own triple.
// ---------------------------------------------------------------------------

/// Cores left out of the default decode-worker count at the pipelined and
/// hand-rolled decode pools (`sized_workers` reserve argument).
pub(crate) const DECODE_WORKERS_RESERVED: usize = 2;

/// Floor of the default decode-worker count at the pipelined and hand-rolled
/// decode pools (`sized_workers` min argument).
pub(crate) const DECODE_WORKERS_MIN: usize = 1;

/// Decode-worker count at the pipelined and hand-rolled decode pools when the
/// host reports no parallelism (`sized_workers` fallback argument).
pub(crate) const DECODE_WORKERS_FALLBACK: usize = 4;

/// Cores left out of the default `par_map_reduce` worker count: one thread for
/// the frame pump (the calling thread); the rest decode.
pub(crate) const PAR_READER_WORKERS_RESERVED: usize = 1;

/// Floor of the default `par_map_reduce` worker count.
pub(crate) const PAR_READER_WORKERS_MIN: usize = 1;

/// `par_map_reduce` worker count when the host reports no parallelism.
pub(crate) const PAR_READER_WORKERS_FALLBACK: usize = 3;

/// Cores left out of the default apply-changes worker count.
pub(crate) const APPLY_CHANGES_WORKERS_RESERVED: usize = 2;

/// Floor of the default apply-changes worker count.
pub(crate) const APPLY_CHANGES_WORKERS_MIN: usize = 2;

/// Apply-changes worker count when the host reports no parallelism.
pub(crate) const APPLY_CHANGES_WORKERS_FALLBACK: usize = 4;

/// Cores left out of the default merge-changes worker count.
pub(crate) const MERGE_CHANGES_WORKERS_RESERVED: usize = 0;

/// Floor of the default merge-changes worker count.
pub(crate) const MERGE_CHANGES_WORKERS_MIN: usize = 1;

/// Merge-changes worker count when the host reports no parallelism.
pub(crate) const MERGE_CHANGES_WORKERS_FALLBACK: usize = 8;

/// Raw element bytes (tag content before entity decoding, as the OSC reader
/// copies it) at which a merge-changes reader seals a batch for encoding.
/// Every sealed batch becomes its own gzip member (or plain fragment) with
/// its own action sections, so changing it changes the output bytes but
/// never the changes or their order; nothing about the host or `-j` does
/// either. Small enough to spread one input over the pool, large enough
/// that member and section overhead stays negligible.
pub(crate) const MERGE_CHANGES_BATCH_BYTES: usize = 512 * 1024;

/// Raw element bytes admitted to merge-changes encoding and not yet written
/// to the output or a spill file, summed over every input. Credit is taken
/// when a reader seals a batch and returned when the coordinator writes the
/// encoded member, so resident batch memory is a bounded multiple of this
/// (the lexical batch, its descriptors and the encoded member it becomes),
/// plus one batch under construction per reader. 64 MiB keeps 30+ workers
/// fed with 512 KiB batches while staying far below the old whole-output
/// residency.
pub(crate) const MERGE_CHANGES_INFLIGHT_BYTES: usize = 64 * 1024 * 1024;

/// Cores left out of the gzip worker count of the derive-changes OSC
/// assembler. The count comes from host parallelism, not from `-j`.
pub(crate) const DERIVE_GZIP_WORKERS_RESERVED: usize = 0;

/// Floor of the gzip worker count of the derive-changes OSC assembler.
pub(crate) const DERIVE_GZIP_WORKERS_MIN: usize = 1;

/// Gzip worker count of the derive-changes OSC assembler when the host
/// reports no parallelism.
pub(crate) const DERIVE_GZIP_WORKERS_FALLBACK: usize = 4;

// ---------------------------------------------------------------------------
// Hand-rolled decode pools: channel depths and reorder capacities
//
// Every hand-rolled descriptor -> worker -> reorder pool (classify, extract,
// tags-filter, altw passthrough and stage 4, ...) shares these depths.
// ---------------------------------------------------------------------------

/// Depth of the descriptor channel feeding a hand-rolled decode pool's
/// workers.
pub(crate) const POOL_DESCRIPTOR_CHANNEL_DEPTH: usize = 16;

/// Depth of the result channel a decode pool's workers send to: the
/// unordered classify pools, extract's hand-built ones, and every ordered
/// pool (`crate::ordered_pool`, capped there at the run's window). Altw stage
/// 4's 256-depth A/B probe, from when it was a hand-built pool, showed the
/// `s4_send_ms` pressure was steady-state consumer/compression saturation,
/// not burst absorption: a deeper channel just moved the wait into the
/// writer pipeline with no wall change.
pub(crate) const POOL_RESULT_CHANNEL_DEPTH: usize = 32;

/// Admission window of cat `--clean`'s ordered pool (at most this many
/// decoded results alive between decode and write).
pub(crate) const POOL_REORDER_CAPACITY_WIDE: usize = 64;

/// Default result window of the ordered pool (`crate::ordered_pool`), per
/// worker: at most `workers x` this many tasks are admitted but not yet
/// consumed. Large enough that a worker is never parked behind a slightly
/// slow neighbour, small enough that buffered results stay bounded. Carried
/// over from renumber's pool, where it was set; for every other pool it is a
/// default, not a measurement.
pub(crate) const ORDERED_POOL_WINDOW_PER_WORKER: usize = 8;

/// Floor of the ordered pool's default result window.
pub(crate) const ORDERED_POOL_WINDOW_MIN: usize = 32;

/// How long a cooperative wait on an executor parks in `recv_timeout` once
/// `yield_now` reports the executor idle, before polling again. Used by the
/// pipelined writer's permit wait and merge-changes' ordered member drain.
pub(crate) const EXECUTOR_IDLE_RECV_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(1);

// ---------------------------------------------------------------------------
// Read path
// ---------------------------------------------------------------------------

/// Number of raw blobs the I/O thread can read ahead.
pub(crate) const DEFAULT_READ_AHEAD: usize = 16;

/// Number of decoded blocks that can be in-flight before backpressure stalls decode.
pub(crate) const DEFAULT_DECODE_AHEAD: usize = 32;

/// Number of decoded blocks buffered between the pipeline and the consumer iterator.
pub(crate) const BLOCK_QUEUE: usize = 8;

/// Total compressed bytes allowed in flight across the frame pump's queue and
/// the decode workers. Bounds the reader working set independent of file size.
pub(crate) const PAR_INFLIGHT_BUDGET: u64 = 256 * 1024 * 1024;

/// Maximum number of compressed blobs gathered into one batch handed to a
/// worker. Amortizes queue synchronization without starving load balancing.
pub(crate) const PAR_BATCH_MAX_BLOBS: usize = 64;

/// Byte target that flushes a batch: the pump flushes the current batch before
/// admitting a blob that would carry its retained weight past this, so a batch
/// exceeds it only when a lone blob larger than the whole target forms its own
/// one-element batch (blobs are never split).
pub(crate) const PAR_BATCH_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Slots ahead of `next_seq` a reorder buffer will grow to before it calls the
/// producer a runaway.
///
/// `push` sizes `pending` from the sequence number it is handed, so before
/// this bound existed a producer that ran away from a stalled slot could
/// grow the buffer to an arbitrary size and the only thing stopping it was
/// permit accounting in the caller - an invariant that lived nowhere near
/// the code that depended on it, and that every one of the twenty call
/// sites had to re-establish for itself.
///
/// The default is deliberately far above any real pipeline window (the
/// largest caller asks for a few hundred slots) so it never fires on a
/// working configuration: it converts an unbounded memory-exhaustion vector
/// into a localised panic that names the buffer. Callers that know their
/// exact permit count can pin it with `ReorderBuffer::with_max_window`.
pub(crate) const DEFAULT_MAX_WINDOW: usize = 65_536;

/// Size of the initial header probe `pread` per blob. One page covers the
/// 4-byte length prefix plus the header bytes for essentially every blob
/// in real PBFs; the fallback path in `next_header` handles the rare
/// exception where a blob's header exceeds the probe window. The walk uses
/// it where recent frames are large; dense files read
/// [`HEADER_WINDOW_SIZE`] windows instead.
pub(crate) const HEADER_PROBE_SIZE: usize = 4096;

/// Size of a header walk's read when its recent frames are dense
/// ([`HEADER_WINDOW_DENSE_FRAME_BYTES`]) and its recent cache-only reads
/// missed ([`HEADER_WINDOW_MISS_STREAK`]): one `pread` covering the next
/// header and the frames after it, kept across payload skips so the headers
/// inside it cost no syscall. The walk's frame N+1 offset is only known from
/// frame N's header, so on a cold cache a 4 KiB probe per frame is one
/// device latency per frame with nothing else in flight; a window this size
/// resolves many headers per latency, and the block layer splits it into
/// several requests in flight. It bounds what a walk that stops early (the
/// blob-count probe) reads past its last header.
pub(crate) const HEADER_WINDOW_SIZE: usize = 1024 * 1024;

/// Mean frame size (prefix, header and payload) below which a header walk
/// reads [`HEADER_WINDOW_SIZE`] windows instead of [`HEADER_PROBE_SIZE`]
/// probes. Roughly one cold read latency times device bandwidth: above it,
/// the bytes a window reads between headers cost more than the latency they
/// save. Geofabrik-style 8,000-element files (about 66 KB per frame) fall
/// below it, the official planet dump (about 1.8 MB per frame) above it, so
/// its walk keeps the per-frame probe.
pub(crate) const HEADER_WINDOW_DENSE_FRAME_BYTES: u64 = 128 * 1024;

/// How many recently completed frames the header walk's window decision
/// averages over. A fixed ring, so a change of density settles within this
/// many frames, and an alternation of small and large frames reads as its
/// mean rather than as whichever frame came last.
pub(crate) const HEADER_WINDOW_SAMPLE_FRAMES: usize = 8;

/// Dense header-walk refills in a row whose cache-only read must miss before
/// an exact refill reads a [`HEADER_WINDOW_SIZE`] window instead of a probe.
/// A single miss is weak evidence (an in-flight page, contention, one evicted
/// header among cached ones); a run of them is a cold region. A cold walk
/// pays this many cold probes before its first window; any hit resets it.
pub(crate) const HEADER_WINDOW_MISS_STREAK: u32 = 4;

/// Number of leading frames sampled when estimating a file's blob count.
pub(crate) const SAMPLE_CAP: usize = 1_000;

/// Blob-count policy boundary measured in `reference/blob-density.md`.
pub(crate) const FULL_SCAN_ARM_MIN_BLOBS: u64 = 150_000;

/// Maximum capacity (bytes) of a buffer retained in a decompression pool.
/// Larger buffers are dropped on return instead of recycled, so an outlier
/// block cannot pin its size in the pool. A cache policy, not a format
/// bound: a decoded buffer holds the payload (up to 32 MiB) plus its inline
/// string and group tables, which can exceed the payload on table-heavy
/// input. Repeated blocks above it churn the allocator again.
///
/// 16 MiB retains every buffer of the planet file (payload plus tables; its
/// blocks run a few MiB, pool drops 0 at `getparents` planet on bygg). The
/// earlier 4 MiB cap, combined with a 4x-compressed-size reservation,
/// dropped every planet buffer (see `reference/performance.md`, "Decode
/// buffers sized from the blob and recycled"). One retained outlier per
/// worker is the cost: up to this much per pread worker.
///
/// The same limit binds the plain decode buffers workers keep across blobs
/// outside a pool, checked once per task by `blob::release_decode_outlier`, so no
/// kept decode buffer holds more than this between blobs.
pub(crate) const DECOMPRESS_POOL_MAX_RETAINED_CAPACITY: usize = 16 * 1024 * 1024;

/// Maximum idle capacity (bytes, the sum of buffer capacities) one
/// decompression pool retains. Per pool, not per process: the pread engines
/// build one pool per worker, which in their synchronous shape holds one
/// buffer at a time; the shared `run_pipeline` pool holds up to its decode
/// window. It bounds idle storage only, not checked-out buffers.
pub(crate) const DECOMPRESS_POOL_MAX_RETAINED_BYTES: usize = 256 * 1024 * 1024;

/// Maximum number of buffers retained in a decompression pool.
/// Defense-in-depth beside the byte budget, which does not bound a pool of
/// tiny buffers.
pub(crate) const DECOMPRESS_POOL_MAX_BUFFERS: usize = 64;

/// Decompression buffer capacity guess, as a multiple of the compressed size:
/// the capacity requested for a blob that declares no `raw_size`. Blobs that
/// declare one are sized from it exactly.
pub(crate) const DECOMPRESS_EXPANSION_GUESS: usize = 4;

/// `WILLNEED` coalescing gap: schedule bodies separated by up to this many
/// bytes (a BlobHeader-sized gap) are joined into one advised range.
pub(crate) const WILLNEED_COALESCE_GAP_BYTES: u64 = 4 * 1024;

/// How far ahead of the dispatch position (bytes) the sliding `WILLNEED`
/// prefetch advises, rather than prefetching the entire schedule before any
/// reads begin.
pub(crate) const WILLNEED_WINDOW_BYTES: u64 = 256 * 1024 * 1024;

/// Byte budget of the region grid's index.
pub(crate) const GRID_MAX_INDEX_BYTES: u64 = 256 * 1024 * 1024;

/// Coverage budget of the region grid in (cell, region) pairs: the index byte
/// budget over four bytes per pair.
pub(crate) const GRID_MAX_PAIRS: u64 = GRID_MAX_INDEX_BYTES / 4;

/// Largest soft `RLIMIT_NOFILE` the process helpers report, so downstream
/// arithmetic cannot overflow when the limit is `RLIM_INFINITY` (`u64::MAX`
/// on Linux).
pub(crate) const NOFILE_SOFT_LIMIT_CLAMP: u64 = 1 << 30;

/// Soft `RLIMIT_NOFILE` assumed when the `getrlimit` probe itself fails, so
/// callers can still compute a sensible cap.
pub(crate) const NOFILE_PROBE_FAILED_ASSUMED_SOFT: u64 = 1024;

// ---------------------------------------------------------------------------
// Write path
// ---------------------------------------------------------------------------

/// Shared buffered and aligned I/O transfer size. Every direct backend reads
/// and writes in whole buffers of this size, so it bounds the direct I/O
/// alignment a file may require (a larger one is refused at open; see
/// `crate::direct_io`).
pub(crate) const IO_BUF_BYTES: usize = 256 * 1024;
const _: () = assert!(IO_BUF_BYTES.is_multiple_of(crate::direct_io::DIO_ALIGN_FLOOR));

/// Default per-block element cap used by `BlockBuilder::new`.
///
/// Matches the common PBF packing convention used by osmium. Non-zero by
/// type, like every element cap: a zero cap is unrepresentable rather than
/// checked.
pub const DEFAULT_ELEMENT_CAP: std::num::NonZeroUsize =
    std::num::NonZeroUsize::MIN.saturating_add(7999);

/// Number of worker threads doing `pwrite`s / `copy_file_range`s in
/// parallel. 16 is the empirical planet-bench value (commit `80b37dfb`,
/// 2026-04-21, raising it from 4); the smaller settings leave disk
/// bandwidth on the table. Could become a CLI flag later.
///
/// The doc here used to justify 8 while the constant read 16, because
/// that commit changed the number and rewrote the prose around a value
/// it did not use.
pub(crate) const POOL_SIZE: usize = 16;

/// Per-worker channel capacity. Kept small so the writer thread's
/// dispatch doesn't outrun the pool. A `WriteOp::Write` owns one frame's
/// buffer (a rewritten output frame, or a whole input frame on an owned
/// passthrough route); a `WriteOp::CopyRange` owns no buffer. The
/// channels therefore queue at most `PER_WORKER_CAPACITY` × `POOL_SIZE`
/// operations, and what that holds scales with `POOL_SIZE` and with the
/// allocated capacity of the frames queued. It is not the pool's whole
/// residency: each worker also holds the operation it is executing, and
/// the dispatcher one blocked on a full queue.
pub(crate) const PER_WORKER_CAPACITY: usize = 8;

/// Capacity of the ordered channel between producers and the writer thread.
///
/// # Why this equals [`PIPELINE_DISPATCH_PERMITS`]
///
/// The channel has room for one full batch of dispatches. Permits bound tasks
/// dispatched but not yet sent: each sender returns its permit after sending,
/// even while its item is still buffered. Items in the channel and the
/// reorder buffer are bounded by [`PIPELINE_DELIVERY_WINDOW`] instead.
///
/// A send can block on a full channel while the writer is handing a chunk to
/// its backend.
/// This cannot create a scheduling deadlock with framing: the writer uses a
/// dedicated OS thread and receives items into its `ReorderBuffer` without
/// waiting for a particular seq. It can drain the channel even when all rayon
/// workers are blocked in sends. Only writing ready items is ordered by seq.
///
/// Backpressure comes from both the producer's permit wait and channel sends.
pub(crate) const WRITE_AHEAD: usize = PIPELINE_DISPATCH_PERMITS;

/// Maximum number of in-flight rayon dispatches in the pipelined writer.
///
/// Counting-semaphore cap that bounds how many uncompressed block `Vec<u8>`s
/// can be owned by rayon closures simultaneously (queued, being compressed,
/// or waiting on the bounded output channel). Without this, `rayon::spawn`'s
/// unbounded internal task queue grows without limit when the producer side
/// out-runs compression throughput - exactly what happened in commit
/// `e7219f0` on planet when parallel pass 1 / stage 2a / stage 2d started
/// emitting blocks faster than zlib:6 could drain them, killing pbfhogg via
/// OOM at 26 GB anon RSS.
///
/// What it bounds: dispatched framing work (queued, running, or blocked
/// sending) and producer-side raw sends - for the input blocks, about
/// `PIPELINE_DISPATCH_PERMITS × block size` (~256 MB at ~4 MB per
/// uncompressed block, an assumed size, not an enforced one). A permit
/// returns once its result is in the writer channel, so results waiting
/// there or in the writer's reorder buffer are bounded by
/// [`PIPELINE_DELIVERY_WINDOW`] instead.
pub(crate) const PIPELINE_DISPATCH_PERMITS: usize = 64;

/// Maximum number of pipelined-writer items reserved but not yet handed to
/// the backend: the end-to-end admission window.
///
/// The producer takes one window token per item before it is given a
/// sequence number, and the writer thread returns it only after the item's
/// ordered handoff. Sequence numbers are assigned in reservation order and
/// delivered in sequence order, so no item can sit more than this many
/// positions past the next one to deliver, and a slow head stalls the
/// producer here instead of letting finished frames pile up behind it. It
/// is a separate pool from [`PIPELINE_DISPATCH_PERMITS`] on purpose: that
/// one bounds retained input work, this one tolerated output latency, and
/// folding them together would either throttle framing behind a slow head
/// or admit as many queued input blocks as the window allows.
///
/// It counts items, not bytes; the bytes its reorder-buffer leg held at peak
/// are `writer_reorder_bytes_high_water`. The value is headroom above what planet runs
/// were seen to need - a reorder span in the low hundreds behind a slow
/// head, plus the dispatch permits and the channel - so that ordinary input
/// does not wait on it; it is not a guarantee that no input ever does.
pub(crate) const PIPELINE_DELIVERY_WINDOW: usize = 512;

/// The window must cover a full batch of dispatches plus the channel, or it
/// rather than the dispatch pool would cap framing parallelism.
const _: () = assert!(
    PIPELINE_DELIVERY_WINDOW >= PIPELINE_DISPATCH_PERMITS + WRITE_AHEAD,
    "PIPELINE_DELIVERY_WINDOW must cover the dispatch permits and the channel"
);

/// Keep room for a full batch of dispatches when changing either capacity.
/// This is a buffering choice; liveness comes from the writer's independent,
/// unordered receive loop, as described in [`WRITE_AHEAD`].
const _: () = assert!(
    WRITE_AHEAD >= PIPELINE_DISPATCH_PERMITS,
    "WRITE_AHEAD must have room for a full batch of dispatches"
);

/// Default chunk size: 2 MB uncompressed per gzip member. Larger chunks
/// amortize the 18-byte per-member framing and let DEFLATE find more
/// cross-sequence matches; smaller chunks expose more parallelism at
/// the worker pool. 2 MB is the `pigz` default and matches reviewer
/// consensus for typical text-like workloads.
pub const DEFAULT_CHUNK_SIZE: usize = 2 * 1024 * 1024;

/// Bounded channel depth for both raw-in and compressed-out queues.
/// Sized at `2 * worker_count` so a full queue on either side
/// backpressures the producer thread naturally without starving the
/// pool. Together with [`DEFAULT_CHUNK_SIZE`] it sets the writer's memory
/// envelope, which grows with the worker count; `ParallelGzipWriter::new`
/// states the bound.
pub(crate) const QUEUE_SLOTS_PER_WORKER: usize = 2;

/// How long the parallel gzip writer parks on its reorder-permit channel
/// before re-checking whether the pool has stopped.
pub(crate) const GZIP_REORDER_PERMIT_POLL: std::time::Duration =
    std::time::Duration::from_millis(25);

/// Number of io_uring registered buffers. 64 × 256 KB = 16 MB total.
/// Charged against `RLIMIT_MEMLOCK`.
#[cfg(feature = "linux-io-uring")]
pub(crate) const NUM_BUFS: u16 = 64;

/// `user_data` packs `buf_idx` into the low 16 bits, but `ud_buf_idx` masks
/// `0x3F` - six bits. The two disagree for any `NUM_BUFS` above 64, and the
/// disagreement is silent: a CQE for buffer 64 returns index 0 to the free
/// list, so two writers end up filling the same registered buffer and the
/// output is corrupt with nothing reported. Widening the pool means widening
/// the mask.
#[cfg(feature = "linux-io-uring")]
const _: () = assert!(
    NUM_BUFS <= 64,
    "NUM_BUFS exceeds the 6-bit mask in ud_buf_idx; widen the mask first"
);

/// Retained per-thread scratch is kept at or below 4 MiB per buffer. Blocks
/// built under the default 8000-element cap are normally below this (the
/// frame scratch's header and compression buffers are sized by the block's
/// header and compressed bytes; writer counters show few buffers capped at
/// planet), so the cap only
/// engages on outliers. It is independent of the read side's `DecompressPool`
/// limit, whose buffers also carry inline tables; a larger buffer is dropped
/// once its frame is assembled, then the thread-local scratch is allowed to
/// warm up again. A run whose blocks all exceed it (repack with a raised
/// `--elements-per-blob`) pays a fresh allocation per buffer per block, which
/// is small next to compressing a block of that size. Both write paths keep
/// both buffers across blobs (the pipelined path copies them into one owned
/// frame; the body's few head bytes have a buffer of their own that never
/// approaches the limit), so they retain at most 8 MiB per `FrameScratch`,
/// in addition to the reused compressor state.
///
/// The same limit applies, per buffer, to everything a `BlockBuilder` keeps
/// across blocks (its dense vectors, wire scratch, tag-key scratch, string
/// table, encode buffer, and the reservation it makes after an owned take)
/// and to the scratch vectors a worker keeps beside its builder, so a
/// builder kept by a pool worker for a whole run holds a fixed bound per
/// buffer rather than the largest of every block it met.
///
/// It also bounds an apply-changes worker's base-read buffer, which holds an
/// input frame rather than a built block, so there the limit does not engage
/// only on outliers: under `--locations-on-ways` an ordinary way frame
/// exceeds it (planet LocationsOnWays blob p99 about 5.4 MB), and the buffer
/// is dropped after about every way blob and regrown by the next. Measured at
/// planet against a 16 MiB line that kept every frame, that churn (about
/// 15,000 releases in 51,000 reads) cost no wall time above noise and lowered
/// the run's peak anon by about 100 MB more (`reference/performance.md`,
/// "apply-changes' worker scratch follows the release rule"). That is a
/// measured outcome under glibc, not a property of the rule: freed capacity
/// lowers the peak only as far as the allocator returns or reuses it.
pub(crate) const MAX_RETAINED_SCRATCH_CAPACITY: usize = 4 * 1024 * 1024;

/// The largest read-buffer allocation a holder keeps from one frame to the
/// next (`crate::read::read_buf`): every buffer that holds a compressed blob
/// payload or a whole input frame across blobs, and the aligned scratch of
/// the direct reads beneath them. An allocation grown past it by one frame
/// is freed once that frame is done with, counted in
/// `pipeline_read_buf_releases`.
///
/// It sits at the decode buffers' line, `DECOMPRESS_POOL_MAX_RETAINED_CAPACITY`,
/// as a policy choice, not a size relationship: a read allocation holds the
/// serialized Blob (sometimes the whole frame with its BlobHeader, or an
/// aligned span with its padding), so it can exceed its decoded size and be
/// released where its decode buffer is kept. On planet it never fires
/// (largest data frame 3.7 MB indexed, 9.6 MB under LocationsOnWays). It
/// bounds each kept allocation, not a worker: one holding both a plain buffer
/// and an aligned scratch, or a direct walk's caller buffer beside its
/// reader's scratch, keeps up to this much in each.
///
/// The scratch limit was measured in its place and costs time: on
/// `check --locations` over the LocationsOnWays planet a 4 MiB line freed
/// and regrew a buffer on most way-blob reads (90,972 releases) and ran
/// 214.4 and 222.9 s against 197.4 to 204.2 s, with no lower run peak; this
/// line ran 204.4 s with no release (bygg, `22a5f24a`,
/// `reference/performance.md`, "Worker read buffers follow a retention
/// limit"). apply-changes keeps the stricter scratch limit on its base-read
/// buffer, measured neutral there (`MAX_RETAINED_SCRATCH_CAPACITY`).
pub(crate) const READ_BUF_MAX_RETAINED_CAPACITY: usize = DECOMPRESS_POOL_MAX_RETAINED_CAPACITY;

/// Zlib worst-case output bound reserved up front: input + input >> this
/// shift (~0.1%) + [`ZLIB_RESERVE_SLACK`] header/trailer bytes.
pub(crate) const ZLIB_RESERVE_SHIFT: u32 = 10;

/// Header/trailer slack of the zlib worst-case output bound.
pub(crate) const ZLIB_RESERVE_SLACK: usize = 64;

// ---------------------------------------------------------------------------
// Apply-changes
// ---------------------------------------------------------------------------

/// Channel capacity for the unified DrainItem stream from scanner +
/// workers: one item per input data blob, plus the scanner's node-boundary
/// control message under `--locations-on-ways`.
///
/// A rewritten blob's item carries every frame its rewrite produced (about
/// 4 MB at planet, more with the creates in its range), so this depth is
/// what bounds the results queued into a drain that is busy writing. While
/// the drain is writer-bound (the node phase at planet) the workers
/// out-produce it in bursts; the channel holds that backlog, and once it is
/// full each worker blocks sending its one finished result, which costs
/// nothing because the writer is the bottleneck. The backlog is about this
/// capacity plus the worker count times a rewrite result; at 256 it reached
/// about 1 GB at planet and stayed as allocator free memory, and 32 holds the
/// channel near 0.15 GB with the same wall time (`reference/performance.md`,
/// "apply-changes' drain channel held the node phase's backlog"). Fixed
/// rather than scaled with the worker count, which already adds one held
/// result per worker.
///
/// Not a ceiling on the run's memory: it does not bound the drain's
/// separate reorder buffer, which absorbs out-of-order arrivals beyond it
/// ([`APPLY_CHANGES_SEQ_WINDOW`] bounds the items in both together, as a
/// count; the node boundary is not a seq), the item the drain is writing,
/// or anything already handed to the writer.
pub(crate) const DRAIN_CHANNEL_CAPACITY: usize = 32;

/// Channel capacity for the scanner→workers candidate dispatch. Workers
/// pull at decompress + parse pace (ms per blob); the producer (scanner)
/// runs at header-walk pace (~µs per blob), so a moderate buffer keeps
/// workers fed during scanner pauses.
pub(crate) const CANDIDATE_CHANNEL_CAPACITY: usize = 64;

/// Apply-changes sequence admission window: the most blob seqs the scanner
/// may have reserved that the drain has not yet processed, wherever they
/// sit (candidate channel, workers, drain channel, reorder map). It bounds
/// how far completed blobs can pile up behind a slow head seq. A count, not
/// bytes: a rewritten blob's output grows with the creates in its range.
///
/// 512 is chosen to sit above what planet runs keep in flight, so that on
/// normal input it does not bind; it exists so a pathological stall cannot
/// grow the map without limit. The measured in-flight high water and the
/// planet A/B are in `reference/performance.md` ("Apply-changes sequence
/// window").
pub(crate) const APPLY_CHANGES_SEQ_WINDOW: usize = 512;

/// Owned-route (`--direct-io`) passthrough frames the drain coalesces into
/// one `write_raw_chunks` call are flushed once they reach this many bytes.
/// Without a cap a long unchanged same-kind run is buffered whole until the
/// next rewrite, gap create or kind change. 32 MiB keeps the batching that
/// makes the coalescer worth having (tens of frames) and bounds what it holds
/// to the cap plus one frame.
pub(crate) const OWNED_PASSTHROUGH_FLUSH_BYTES: usize = 32 << 20;

/// Blocks of one create run the drain encodes and frames in parallel per
/// executor call, before it writes them in order and starts the next batch.
/// A run longer than one block (the OSC's creates past the last base blob
/// of a kind, about 300 node blocks at planet) is encoded on the executor
/// so the drain is not busy for the whole run while the workers keep
/// producing behind it. The batch bounds what one call holds (its frames,
/// and a builder per running task), whatever the run's length; 64 keeps a
/// 32-thread executor busy through a batch. The integration test that
/// crosses a batch boundary sizes its run at 65 full blocks and one
/// element; a unit test in `stream_output.rs` fails if this value grows
/// past what that run crosses, so raising it means growing the run.
pub(crate) const APPLY_CHANGES_CREATE_BATCH_BLOCKS: usize = 64;

/// Filter bits per OSC node in the `--locations-on-ways` refresh probe
/// (`apply_changes/refresh.rs`), before rounding up to a power of two. The
/// filter is the only state the probe holds beside the sorted diff, and it is
/// consulted for every ref of every searched way, so it trades its size
/// against the exact searches its false positives cost: at 16 bits and four
/// bits per key in one word, about 0.35 % of absent IDs pass (8 MB at a
/// planet daily diff).
pub(crate) const APPLY_CHANGES_REFRESH_FILTER_BITS_PER_KEY: usize = 16;

/// Drain `recv_timeout`: how long the drain blocks on an empty item channel
/// before checking the producer failure slot and the cancellation token.
/// The node boundary arrives on the item channel itself, so no polling is
/// needed for the barrier. A failed worker never delivers its seq while the scanner can sit in its
/// barrier wait and the other workers idle on the candidate channel, so no
/// channel disconnect would wake the drain; the timeout does.
///
/// 25 ms is a tradeoff: tighter polling adds CPU at very small
/// dataset sizes; looser polling adds latency at the moment of
/// node→way transition. At planet, the transition fires once per
/// run, so even 100 ms total polling latency is invisible against
/// ~100 s wall.
pub(crate) const DRAIN_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(25);

/// Fixed per-descriptor byte cost added when accounting a drain or scanner
/// item for reorder high-water telemetry (not an admission permit or a memory
/// bound).
pub(crate) const DESCRIPTOR_OVERHEAD: usize = 64;

// ---------------------------------------------------------------------------
// Add-locations-to-ways (altw)
// ---------------------------------------------------------------------------

/// Route to external when the estimated sparse store exceeds this many
/// percent of the page-cache budget. This is a policy threshold selected
/// from the measured store-versus-cache behavior, not a measured optimum;
/// see `reference/performance.md` for the supporting measurements.
pub(crate) const AUTO_STORE_BUDGET_PCT: u64 = 80;

/// Anon footprint a sparse run holds outside the mmap store (IdSet, decode
/// buffers, writer queues). Subtracted from available memory before applying
/// [`AUTO_STORE_BUDGET_PCT`], so the budget reflects what the page cache can
/// actually keep resident for the store. See `reference/performance.md` for
/// measured resource use.
/// Keep this measured allowance fixed: indexed node count estimates the
/// file-backed store, not the referenced-ID span or the decode/queue footprint.
/// Scaling anon headroom from that count would assert an unmeasured relation.
pub(crate) const AUTO_ANON_HEADROOM_BYTES: u64 = 3 << 30;

/// Number of buckets for radix partitioning. 256 = partition by high byte.
pub(crate) const NUM_BUCKETS: usize = 256;

/// Size of the write buffer per ALTW bucket file (256 KB).
///
/// This is an ALTW-specific batching tunable, separate from
/// `crate::write::IO_BUF_BYTES`. Bucket files have many independent buffered
/// writers, so this capacity also determines ALTW's aggregate buffering.
pub(crate) const BUCKET_BUF_SIZE: usize = 256 * 1024;

/// Widest bucket the record encoding accepts. A bucket-local id must fit in
/// `LOCAL_ID_MASK` (31 bits; the top bit is `CLOSURE_FLAG`), and the last
/// bucket can extend one full width past its nominal end, so a width of 2^30
/// keeps every local id at or below 2^30 with margin to spare.
pub(crate) const MAX_BUCKET_WIDTH: u64 = 1 << 30;

/// Stage-1 blob-level count stream: one u64 per way blob plus the total. This
/// sidecar batching capacity is distinct from the PBF writer transfer size in
/// `crate::write::IO_BUF_BYTES`.
pub(crate) const REF_COUNT_SIDECAR_BUF_BYTES: usize = 64 * 1024;

/// Stage-1 per-way stream: each blob's way count and each way's refcount as
/// varints. This sidecar batching capacity is distinct from the PBF writer
/// transfer size in `crate::write::IO_BUF_BYTES`, despite having the same value.
pub(crate) const PER_WAY_REFCOUNT_SIDECAR_BUF_BYTES: usize = 256 * 1024;

/// Initial capacity of stage 1's varint scratch buffer.
pub(crate) const STAGE1_VARINT_BUF_CAPACITY: usize = 1024;

/// Headroom for the descriptors stage 1 opens beyond the shard fleet
/// after the allowance is taken: the input PBF, the sidecars and
/// whatever the reader and writer stages open.
pub(crate) const STAGE1_HEADROOM_FDS: u64 = 64;

/// Stage 2's per-slot-bucket local buffers are flushed when any buffer
/// exceeds this, and at the end of each id bucket. This worker-local batch
/// size controls mutex/write batching; it is separate from the shared file
/// `BufWriter` capacity.
pub(crate) const STAGE2_FLUSH_THRESHOLD: usize = 256 * 1024;

/// Cap on stage 2's worker count (applied on top of the decode-worker
/// sizing).
pub(crate) const ALTW_STAGE2_WORKERS_CAP: usize = 6;

/// Cap on the external join's stage 3 worker count (applied on top of the
/// decode-worker sizing). Backed off from the pre-streaming cap of 6 because
/// stage 3 and stage 4 worker buffers are now both resident at the same time
/// (streaming stage 3 -> 4 landed in `beb7838` + `f93d896` + `eecb46c`). The
/// stage 3 worker pool and its router backing files share this count; the
/// scratch precheck counts one payload file per worker.
pub(crate) const ALTW_STAGE3_WORKERS_CAP: usize = 4;

// ---------------------------------------------------------------------------
// Renumber
// ---------------------------------------------------------------------------

/// Pass 1 worker count.
pub(crate) const RENUMBER_PASS1_WORKERS: usize = 4;

/// Parallel way-assembly workers in stage 2d, one `IdSet` shard each.
pub(crate) const RENUMBER_STAGE2D_WORKERS: usize = 6;

/// Cap on the relation stage's decode-worker count (applied on top of the
/// decode-worker sizing).
pub(crate) const RENUMBER_RELATIONS_WORKERS_CAP: usize = 6;

/// IDs the pass 1 node pre-allocation may cover per input byte, passed to
/// `IdSet::file_bounded_max_id` with the schedule's claimed highest node ID.
///
/// Unlike `check`'s `id_alloc_bound` there is no live-OSM floor, because
/// nothing depends on the bound covering every ID: pass 1 hands an ID
/// outside the pre-allocation back as overflow and inserts it after the
/// pass, where `IdSet` memory follows the number of distinct IDs. A small
/// extract carrying real ~1e10 node IDs therefore costs memory in proportion
/// to its nodes, not to its largest ID, and is not refused. The overflow path
/// is serial (~3.7 ns per ID on bygg), and honest files gain nothing from a
/// tight bound: their set ends as the same bitmaps either way. At 8 per
/// byte, denmark (highest node ID about 29 times its size in bytes) sent 37
/// of its 52 M nodes through overflow, 137 ms of a 0.85 s run; at 64 every
/// country-sized extract stays on the parallel path, and only small files
/// with high IDs, whose overflow is small, take it.
pub(crate) const RENUMBER_PREALLOC_IDS_PER_BYTE: u64 = 64;

// ---------------------------------------------------------------------------
// Degrade
// ---------------------------------------------------------------------------

/// How far a `--corrupt-indexdata` index overstates the blob it describes.
/// Large enough that no honest count could coincide with it.
pub(crate) const CORRUPT_INDEXDATA_INFLATION: i64 = 1_000_000;

// ---------------------------------------------------------------------------
// Blob-level merge (sort, cat --dedupe)
// ---------------------------------------------------------------------------

/// Overlap runs admitted ahead of the write loop per sweep worker: run `k`
/// is handed to a worker only while `k < drained + window`, with the window
/// this times the worker count. Two keeps every worker fed while the run at
/// the head drains. With [`OVERLAP_RUN_CHANNEL_DEPTH`] and
/// [`OVERLAP_RUN_CHUNK_BYTES`] it bounds the framed output buffered between
/// the sweeps and the writer at `window x depth` chunks.
pub(crate) const OVERLAP_RUN_WINDOW_PER_WORKER: usize = 2;

/// Chunks one overlap run's sweep may have framed ahead of the write loop
/// before its worker parks. A run of a single giant overlap component (two
/// regional extracts whose ID ranges interleave) therefore streams through
/// the writer instead of being held whole.
pub(crate) const OVERLAP_RUN_CHANNEL_DEPTH: usize = 4;

/// Allocation capacity of framed blobs a sweep packs into one chunk before
/// sending it; a frame larger than this is framed into pieces of at most
/// this size, so no chunk exceeds it. Sized so a typical
/// run (two dense planet blobs merged, about 2 MB compressed) completes in a
/// chunk or two without parking behind the write loop, which is what lets
/// later runs proceed while the head drains; the planet self-merge's
/// `overlap_run_chunks` bears that out (reference/performance.md). The wall
/// effect of other sizes is not measured.
pub(crate) const OVERLAP_RUN_CHUNK_BYTES: usize = 2 << 20;

/// Bytes one overlap sweep's open cursors may be charged for (decoded block
/// payloads, and the view vectors of sorted-views cursors) before it
/// demotes mostly consumed cursors (less than an eighth of their elements
/// left) to owned tails, releasing their blocks. On planet merged with
/// itself the largest run charge, two dense twin blobs, is about a fifth of
/// this (`overlap_run_pinned_high_water_bytes`, reference/performance.md),
/// so ordinary runs never demote; a run with many sparse tails stops
/// pinning whole blocks for a few elements each.
///
/// This value, [`OVERLAP_RUN_CHUNK_BYTES`] and [`OVERLAP_RUN_CHANNEL_DEPTH`]
/// size the resource tests in `tests/merge_pbf.rs`, which cannot read them;
/// a unit test in `blob_merge/sweep.rs` fails when one changes, so changing
/// one means resizing those fixtures.
pub(crate) const SWEEP_PINNED_BYTES: usize = 64 << 20;

/// Heap removals (emitted elements and dropped duplicates alike) an overlap
/// sweep makes between two checks of cancellation and abandonment, beside
/// the check before each blob it reads. Without it a run's final heap drain,
/// or a long stretch of exact duplicates that fills no block, ran unchecked
/// to the end. Two atomic loads per check, so the interval only has to
/// keep a check's cost far below the removals it covers; not measured.
pub(crate) const SWEEP_CHECKPOINT_REMOVALS: usize = 4096;

/// Distinct versions an overlap sweep's same-ID group holds before its
/// dedupe lookup switches from a linear scan of the group to a per-group
/// version index. Ordinary groups (one version, a few copies) never reach
/// it, so they pay no hashing; a group of very many versions (a history
/// object, adversarial input) costs linear time instead of quadratic. Not
/// measured: any value far above ordinary group sizes and far below the
/// sizes where a scan dominates serves.
pub(crate) const SWEEP_GROUP_INDEX_THRESHOLD: usize = 32;

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

/// Work budget for one ordered sequence diff in `diff -v` details (a
/// relation's member list, a way's node refs, or a tag list with a duplicate
/// key). Counts the Myers search's diagonal visits plus snake comparisons;
/// since the traceback keeps one snapshot entry per diagonal visit, it also
/// caps that storage (8 bytes an entry, so 8 MiB at most). Past the budget
/// the detail prints one line naming the replaced old and new ranges instead
/// of an edit script. Per call and independent of shard count, so a run's
/// memory does not grow with it beyond one budget per worker. Sized so one
/// call stays in the low milliseconds; not measured.
pub(crate) const VERBOSE_SEQUENCE_DIFF_MAX_WORK: usize = 1 << 20;

/// Most edit lines (removals plus insertions) one ordered sequence prints
/// under a modified element in `diff -v`. A larger script, including a pure
/// bulk insertion or deletion that needs no search at all, prints one
/// summary line naming the replaced old and new ranges instead. This is a
/// readability bound on output, independent of the search budget above
/// (which by itself already stops near 1023 edits): it is what keeps a
/// reversed long way or a rebuilt large relation from printing tens of
/// thousands of lines. Per sequence, not per element; keyed tag lines are
/// not counted.
pub(crate) const VERBOSE_SEQUENCE_DIFF_MAX_EDITS: usize = 1000;

/// Compressed frame bytes (both inputs together) at which the block-pair
/// planner closes a work unit, at the next cut its closure rule allows.
/// Larger units cost memory (a window of them is held); smaller ones cost
/// CPU, since a blob crossing a cut is decoded once by each unit it
/// touches, about one blob per unit. At ~1 MB planet blobs this is a few
/// dozen blobs a unit, so the crossing decode stays a few percent. Measured
/// on planet diff (bygg, 32 workers, reference/performance.md): 16 MiB cost
/// about 4 % wall for about a third less peak memory.
pub(crate) const DIFF_UNIT_TARGET_BYTES: usize = 32 << 20;

/// Claimed elements of single-sided frames (no frame of the other input can
/// share a key with them) past which a unit closes at its next cut, beside
/// the byte target. Every element of such a frame becomes output, so a unit
/// cut by bytes alone there makes tens of MB of text that its channel cannot
/// hold: later units park on a full channel and the head formats the stretch
/// alone. About 2 MB of compact lines, under the text channel's depth of
/// 1 MiB chunks; a target, not a cap, since a frame is never split (planet
/// blobs of ~318k nodes make one-blob units). Only a scheduling input:
/// claims never change the output.
pub(crate) const DIFF_UNIT_LONE_ELEMENTS: u64 = 1 << 17;

/// Frame bytes past which a unit that still cannot close is refused as a
/// resource limit. A unit of duplicate-free input closes within the target
/// plus a few maximum-size frames (one crossing each cut per input, one read
/// past the target per input: under 200 MiB at the 32 MiB frame limit), so
/// only many adjacent blobs starting at one ID - a duplicate-ID run no
/// window can split - reach this.
pub(crate) const DIFF_UNIT_CAP_BYTES: usize = 1 << 30;

/// Planned units waiting for a worker, beyond the ones the pool's window
/// already holds: enough that a worker freeing up rarely waits on the
/// planner's reads. Not measured.
pub(crate) const DIFF_UNIT_QUEUE_DEPTH: usize = 4;

/// Units issued beyond one per worker. The pool holds at most `workers +
/// DIFF_UNIT_WINDOW_SLACK` units, which with the queue above bounds the
/// frame memory of a run; a little slack keeps every worker busy while the
/// consumer drains the head. Not measured.
pub(crate) const DIFF_UNIT_WINDOW_SLACK: usize = 2;

/// Frames issued beyond one per worker by `read::frame_pool` (`check
/// --index`, `cat --reindex`): the walk runs at most `workers +
/// FRAME_POOL_WINDOW_SLACK` raw frames ahead of the in-order fold, which
/// bounds the frames in flight, and a little slack keeps every worker busy
/// while the fold waits on the head. Not measured.
pub(crate) const FRAME_POOL_WINDOW_SLACK: usize = 2;

/// Record buffers `digest` keeps for reuse once its ordered consumer has
/// hashed them; beyond it a returned buffer is freed. A buffer is taken by
/// an admitted decode task and returned before that task's permit is
/// released, so with the read pipeline's default window
/// ([`DEFAULT_DECODE_AHEAD`]) no more than that many exist, pooled or in
/// use, and a cap at the window never refuses one. Retention is by
/// capacity, not count: a planet block's full-form records run to tens of
/// megabytes, so at planet this holds a few hundred megabytes for the run.
pub(crate) const DIGEST_RECORD_BUFFERS_KEPT: usize = DEFAULT_DECODE_AHEAD;

/// Formatted output bytes a unit's worker gathers before handing a chunk to
/// the consumer, so a unit with a large output streams it instead of
/// holding it whole. The text diff takes a chunk after a whole merge action,
/// so a single-sided blob run can pass it by a blob's lines; derive polls
/// such runs element by element, so a derive chunk (one gzip member of a
/// `.osc.gz`) passes it by at most one element. Not measured.
pub(crate) const DIFF_OUTPUT_CHUNK_BYTES: usize = 1 << 20;

/// Read buffer for appending a derive unit's spill file to its fragment;
/// the cancellation token is checked once per buffer. Not measured.
pub(crate) const DIFF_SPILL_COPY_BUFFER: usize = 1 << 20;

/// Bytes of XML gathered in front of a streaming OSC gzip encoder before
/// deflate runs: the XML writers emit many tiny writes per element, and
/// deflate called once per write was most of the writer's time. Not
/// measured beyond "large".
pub(crate) const OSC_GZIP_WRITE_BUFFER: usize = 256 << 10;

/// Output chunks one unit may have queued for the consumer before its
/// worker blocks (text) or spills the rest of the unit to scratch (derive).
pub(crate) const DIFF_OUTPUT_CHUNK_DEPTH: usize = 4;

// ---------------------------------------------------------------------------
// Cat
// ---------------------------------------------------------------------------

/// Descriptors a `cat` run asks the host for beyond its baseline input
/// count: one new pin per distinct input path for plain cat (a caller's
/// pin is already open), one pass-2 source per argument for
/// `cat --dedupe`. It covers, at worst, the writer's own open
/// of the output's staging path, the io_uring ring when that backend is
/// chosen, and one input descriptor beside the baseline (a header reader,
/// indexdata probe, body reader or classify walker, a pin's procfs check
/// while pins are taken, or dedupe's newly opened source before the pin it
/// came from is dropped), with slack over their sum; recheck this when an output backend or a cat pool starts holding more.
/// A fixed, conservative allowance rather than per-backend arithmetic,
/// which would be a second copy of the writers' internals.
pub(crate) const CAT_EXTRA_FDS: u64 = 8;

// ---------------------------------------------------------------------------
// Filtering and extract
// ---------------------------------------------------------------------------

/// Descriptors a multi-region `extract` asks the host for beyond its
/// per-region count (one held writer per region on the single-pass route)
/// and its new input pin. It covers, at worst, one region's extract on the
/// sequential route (the writer's own open of the output's staging path, the
/// io_uring ring when that backend is chosen, and the up-to-three input
/// descriptors a strategy holds at once: a header reader, a classify input
/// and a walker), or on the single-pass route the shared input descriptor
/// the classify and write phases pread through, the sortedness probe that
/// closes before the writers open, and the pin's transient procfs check,
/// with slack over each. Every output's staging descriptor is opened when
/// the caller builds the output set, before the call, so it is already
/// counted as open and not asked for. Recheck this when a strategy or
/// output backend starts holding more.
pub(crate) const EXTRACT_MULTI_EXTRA_FDS: u64 = 8;

/// Number of decoded blocks collected before unsorted pass-2 dispatches them
/// to rayon.
pub(crate) const EXTRACT_SIMPLE_BATCH_SIZE: usize = 64;

/// Region count from which multi-extract builds a `RegionGrid` to prune
/// candidates instead of testing every region linearly.
pub(crate) const GRID_REGION_THRESHOLD: usize = 16;

/// Largest member count a multi-extract membership page (4096 IDs) stores
/// as a sorted offset list instead of a union bitmap. A lookup policy, not
/// the memory crossover (about 264 members): above it a binary search on the
/// hot way-classify path costs more than the bitmap's rank.
pub(crate) const MEMBERSHIP_SPARSE_MEMBERS_MAX: usize = 256;

/// Largest member count of one region's sorted offset list inside a mutable
/// multi-extract membership page, before it becomes a 64-word bitmap.
pub(crate) const MEMBERSHIP_PART_SPARSE_MAX: usize = 32;

/// Whole-page merges a compact multi-extract membership page takes before it
/// converts, one way, to its mutable form. Honest sorted input merges only
/// pages two blobs share; the budget bounds what a page that keeps being
/// revisited (duplicates, false sort claims, tiny blobs) costs to rebuild.
pub(crate) const MEMBERSHIP_MERGE_BUDGET: u8 = 2;

/// Pages a multi-extract membership chunk (2^25 IDs) keeps in a sorted list
/// before it switches to its 8192-slot table, so scattered isolated IDs do not
/// each pay a whole table.
pub(crate) const MEMBERSHIP_SPARSE_TABLE_MAX: usize = 256;

/// Maximum number of extracts a multi-extract config may declare.
pub(crate) const MAX_EXTRACTS: usize = 500;

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Squared segment length below which a segment is treated as degenerate (a
/// single point) by the Douglas-Peucker code. The geocode closest-point
/// search does not use it: it measures to every point of a segment however
/// short, since a query on one is at distance zero.
pub(crate) const DEGENERATE_SEGMENT_EPSILON: f64 = 1e-30;

/// Headroom, in segment placements, that `ring_search_budget` grants on top
/// of its quadratic allowance. It is spent only at branching endpoints.
#[cfg(feature = "commands")]
pub(crate) const RING_SEARCH_EXTRA_STEPS: u64 = 1 << 20;

// ---------------------------------------------------------------------------
// getid
// ---------------------------------------------------------------------------

/// `getid --add-referenced`: records (16 bytes each) in the relation-closure
/// edge store's first anonymous mapping, 1 MiB. The store doubles from
/// there; a file without relations in relations never maps it at all.
pub(crate) const GETID_EDGE_STORE_FIRST_RECORDS: usize = 1 << 16;

// ---------------------------------------------------------------------------
// Check and inspect
// ---------------------------------------------------------------------------

/// Minimum node ID-space allocation used by check commands.
pub(crate) const NODE_ID_FLOOR: i64 = 14_000_000_000;

/// Minimum way ID-space allocation used by check commands.
pub(crate) const WAY_ID_FLOOR: i64 = 2_000_000_000;

/// Minimum relation ID-space allocation used by check commands.
pub(crate) const RELATION_ID_FLOOR: i64 = 25_000_000;

/// IDs of range per input byte that `id_alloc_bound` allows above the floor:
/// at one bit per ID, a bitset no larger than the input file.
pub(crate) const CHECK_IDS_PER_BYTE: u64 = 8;

/// `check --locations`: share, in percent, of the host's available memory
/// (measured before the check reads any blob, less
/// `CHECK_LOCATIONS_HEADROOM_BYTES` and an upper bound on the node-ID set and
/// its rank index) that one partition's coordinate store may take. The store is anonymous memory touched at random by every way
/// worker, so it must stay resident; the remainder is left for the page
/// cache the sequential blob reads stream through and for worker buffers.
pub(crate) const CHECK_LOCATIONS_STORE_BUDGET_PCT: u64 = 60;

/// `check --locations`: memory held back from the store budget for the
/// decode workers' blob buffers and the rest of the process.
pub(crate) const CHECK_LOCATIONS_HEADROOM_BYTES: u64 = 2 << 30;

/// `check --locations`: store budget when the host reports no available
/// memory. Planet needs several partitions at this size; a smaller file runs
/// in one.
pub(crate) const CHECK_LOCATIONS_FALLBACK_BUDGET_BYTES: u64 = 4 << 30;

/// `check --locations`: the least store budget a run is given, so a host
/// whose available memory is already spent still makes progress rather than
/// planning one partition per handful of nodes.
pub(crate) const CHECK_LOCATIONS_MIN_BUDGET_BYTES: u64 = 64 << 20;

/// `check --locations`: planned partitions above which the run warns that it
/// will read every way blob that many times (the budget it was given and the
/// reason are in the warning).
pub(crate) const CHECK_LOCATIONS_PARTITION_WARN: usize = 16;

/// `check --locations`: shards of the key-set build. Workers insert IDs
/// straight into the shard owning their `IdSet` chunk, so the build is not
/// serialised on one merge thread and no per-blob ID vector is queued.
pub(crate) const CHECK_LOCATIONS_KEY_SHARDS: usize = 64;

/// `check --locations`: IDs a worker buffers before inserting them into the
/// key-set shards, bounding its buffer at 512 KiB however many IDs a blob
/// holds.
pub(crate) const CHECK_LOCATIONS_KEY_BATCH: usize = 1 << 16;

/// `inspect --anomalies` flags a block whose element count is below
/// `ANOMALY_SMALL_NUMERATOR / ANOMALY_SMALL_DENOMINATOR` of its type's median.
pub(crate) const ANOMALY_SMALL_NUMERATOR: u64 = 1;
/// Denominator of the small-block anomaly ratio; see `ANOMALY_SMALL_NUMERATOR`.
pub(crate) const ANOMALY_SMALL_DENOMINATOR: u64 = 2;
/// `inspect --anomalies` flags a block whose element count is above
/// `ANOMALY_LARGE_NUMERATOR / ANOMALY_LARGE_DENOMINATOR` of its type's median.
pub(crate) const ANOMALY_LARGE_NUMERATOR: u64 = 3;
/// Denominator of the large-block anomaly ratio; see `ANOMALY_LARGE_NUMERATOR`.
pub(crate) const ANOMALY_LARGE_DENOMINATOR: u64 = 2;

/// `inspect`: ordering segments (runs of same-kind blobs) the report keeps
/// with their block ranges; the rest are only counted, so a file whose kind
/// changes every blob costs no per-blob memory. A standard order has at most
/// three segments, so a head of four or more always holds the segment where
/// a file first leaves standard order (asserted where it is used). 64 leaves
/// room to see the shape of a non-standard file.
pub(crate) const INSPECT_ORDERING_HEAD_SEGMENTS: usize = 64;

/// `inspect tags`: the count table has `1 << TAGS_COUNT_SHARD_BITS` shards,
/// each behind its own mutex. Every decode worker merges its blob's distinct
/// pairs into the shards itself, one shard locked at a time, so the merge is
/// not serialised on one thread (a single consumer merging per-blob maps was
/// the whole wall at planet, workers parked behind it). With many more
/// shards than workers, two workers rarely want one shard at once; each blob
/// takes each shard it touches once.
pub(crate) const TAGS_COUNT_SHARD_BITS: u32 = 8;

/// `inspect tags`: bytes per chunk of a count shard's string arena. A shard
/// appends strings to fixed chunks instead of a doubling vector, which at
/// planet reserved about half again what it stored and copied on every
/// growth. A string longer than this gets a chunk of its own. The unfilled
/// rest of each shard's current chunk is the floor of the waste, so the
/// shard count times this stays small beside a planet table.
pub(crate) const TAGS_COUNT_ARENA_CHUNK_BYTES: usize = 256 * 1024;

/// `inspect tags`: how far a worker moves the shard its next blob starts
/// merging at, and the multiple of the worker's seed its first blob starts
/// at, so the workers' walks over the shards start apart rather than all at
/// shard 0. Odd, so a worker's starts cycle through every shard of the
/// power-of-two table.
pub(crate) const TAGS_COUNT_ROTATION_STEP: usize = 97;

// ---------------------------------------------------------------------------
// Debug sidecar
// ---------------------------------------------------------------------------

/// Longest marker line the FIFO emitter will write. POSIX guarantees a write of
/// at most `PIPE_BUF` bytes to a pipe/FIFO is atomic; 512 is the POSIX
/// minimum, Linux is 4096.
pub(crate) const PIPE_BUF: usize = 512;

// ---------------------------------------------------------------------------
// Geocode index: mechanism
// ---------------------------------------------------------------------------

/// How far Pass 2 classification may run ahead of the earliest unmerged blob.
/// Both merges must see blobs in file order (record order, string offsets and
/// the surviving duplicate admin way depend on it), and this bounds the
/// results held for reordering to this many per-blob outputs, each bounded by
/// one input blob's contents. Wide enough that a slow blob at the head rarely
/// parks the workers.
pub(crate) const GEOCODE_MERGE_WINDOW_BLOBS: usize = 256;

/// Write buffer of each index data file the builder streams record by
/// record. The Pass 2 merges are serial and write one small record per
/// address, street or node. With std's 8 KiB default, growing `AddrPoint`
/// from 20 to 24 bytes made the node merge the bottleneck of Pass 2a on
/// Germany (0.95 s to 1.44 s on bygg); a 1 MiB buffer took it back to
/// 0.95 s. See `reference/performance.md`, build-geocode-index.
pub(crate) const GEOCODE_DATA_FILE_BUFFER_BYTES: usize = 1 << 20;

/// IDs the dense node-ID bitmap may cover per input byte, passed to
/// `IdSet::file_bounded_max_id` because `max_node_id` can come from
/// unverified indexdata. References above the resulting bound are collected
/// as plain per-worker ID lists and inserted with `IdSet::set` after the
/// parallel scan, as in external renumber, so an honest high-ID extract is
/// not rejected or silently dropped. Same factor as renumber's, for the same
/// reason: country-sized extracts stay on the parallel path.
///
/// The lists are deliberately not per-worker `IdSet`s: every worker that
/// sees more IDs in a 2^25-ID chunk than an array chunk holds would promote
/// its own 4 MiB copy of that chunk, multiplying the overflow range's bitmap by the
/// worker count - the per-worker accumulation shape this pass was moved off.
pub(crate) const NODE_PREALLOC_IDS_PER_BYTE: u64 = 64;

/// A worker's overflow list is sorted and deduplicated whenever it has
/// doubled since its last compaction and holds at least this many IDs, so
/// refs repeated across blobs cannot grow it far past its distinct count.
pub(crate) const OVERFLOW_COMPACT_MIN: usize = 1 << 20;

/// Maximum number of distinct sample IDs kept for a build diagnostic.
pub(crate) const DIAGNOSTIC_ID_SAMPLE_LIMIT: usize = 16;

/// Per-segment cell budget for the street and interpolation cover: 2^24 raw
/// cell entries, before deduplication. In the pathological case (a pole to
/// pole segment half the globe wide) that is a 134 MB `Vec<u64>` on one rayon
/// worker, twice that while it grows; every real OSM segment is a small
/// fraction of a degree and covers a handful of cells.
pub(crate) const MAX_SEGMENT_CELLS: f64 = 16_777_216.0;

/// Highest S2 level accepted for the street and coarse cells.
///
/// Pinned to `MAX_SEGMENT_CELLS` by `level_exceeds_segment_cell_budget` and by
/// the unit test in the path cover, which walks every level and asserts this
/// is exactly the boundary - raising the constant without raising the budget
/// fails the test rather than reintroducing unbounded work.
pub(crate) const MAX_BUILD_S2_LEVEL: u8 = 19;
const _: () = assert!(MAX_BUILD_S2_LEVEL <= crate::geocode_index::format::S2_MAX_LEVEL);

/// Maximum number of S2 cells the admin flood fill may visit for one polygon.
/// This is the full sphere's cell count at level 10: six faces times 4^10.
pub(crate) const ADMIN_CELL_COUNT_BUDGET: u64 = 6_291_456;
const _: () = assert!(
    ADMIN_CELL_COUNT_BUDGET == 6 * (1 << 20),
    "the admin cell budget is documented as six faces times 4^10"
);

/// Largest distance, as a fraction of the globally smallest S2 cell width at
/// the level, between a covered lat/lon path and the great-circle chords the
/// path cover replaces it with. The cover dilates each chord by the distance
/// it actually proves, so this only decides how finely a long or strongly
/// curved path is cut: smaller means more chords, larger means more cells
/// that lie near the path without touching it. At 1/1024 an ordinary street
/// segment (a few hundred metres at most) is one chord at the default street
/// level.
pub(crate) const PATH_COVER_TOLERANCE_WIDTH_FRACTION: f64 = 1.0 / 1024.0;

/// Absolute slack, in S2 face (u, v) units, added to every comparison of the
/// path cover against the cell grid. It absorbs the f64 rounding of the
/// projection, the clip and the index conversions (each of order 1e-15), so a
/// path running on a cell boundary emits the cells on both sides. 1e-9 is
/// about 6 mm on the ground.
pub(crate) const PATH_COVER_UV_SLACK: f64 = 1e-9;

/// Longest piece, in radians of the path's own (lat, lon) parameter
/// (`sqrt(dlat^2 + dlon^2)`), the path cover turns into one chord. It keeps
/// every chord far from antipodal and short enough that the cube faces it can
/// reach are found by a fixed dot-product test.
pub(crate) const PATH_COVER_MAX_PIECE_RAD: f64 = 0.1;

/// Number of pass 3 buckets for bucketed cell assignment. Pass 3 buckets a
/// cell by its top byte (`cell_id >> 56`), so this must stay 256.
pub(crate) const GEOCODE_NUM_BUCKETS: usize = 256;
const _: () = assert!(
    GEOCODE_NUM_BUCKETS == 1 << 8,
    "pass 3 buckets by the top byte of the cell id (bucket_for_cell)"
);

/// Pass 3 stage A chunk size, in street ways.
pub(crate) const GEOCODE_STREET_CHUNK: u32 = 100_000;

/// Pass 3 stage A chunk size, in address points.
pub(crate) const GEOCODE_ADDR_CHUNK: usize = 500_000;

/// Pass 3 stage A chunk size, in interpolation ways.
pub(crate) const GEOCODE_INTERP_CHUNK: usize = 100_000;

/// Bound on the parse/sort lookahead of pass 3 stage B: the number of buckets
/// sorted at once. Keeping every sorted bucket alive defeats the external
/// bucketing and costs more RAM than the 15-byte scratch records because a
/// decoded `BucketRecord` is 16 bytes.
pub(crate) const GEOCODE_STAGE_B_SORT_WINDOW: usize = 8;

/// Number of hole vertices sampled when deciding which outer ring owns the
/// hole (majority vote).
pub(crate) const HOLE_OWNERSHIP_SAMPLES: usize = 5;

/// Total length (radians) under which an interpolation way is unusable to the
/// reader.
pub(crate) const INTERP_MIN_TOTAL_LEN: f64 = 1e-15;

// ---------------------------------------------------------------------------
// Geocode index: policy (what counts as a street, an admin boundary, ...)
// ---------------------------------------------------------------------------

/// Highway classes excluded from the street set. Shared vocabulary for the
/// wire scanner and decoded-way classifier.
pub(crate) const GEOCODE_EXCLUDED_HIGHWAYS: [&[u8]; 9] = [
    b"footway",
    b"path",
    b"track",
    b"steps",
    b"cycleway",
    b"service",
    b"pedestrian",
    b"bridleway",
    b"construction",
];

/// Lowest `admin_level` accepted for an admin boundary relation (the highest
/// is `format::MAX_ADMIN_LEVEL`, fixed by the index layout).
pub(crate) const GEOCODE_ADMIN_LEVEL_MIN: u8 = 2;

/// The `admin_level` whose boundaries carry the country code.
pub(crate) const GEOCODE_COUNTRY_ADMIN_LEVEL: u8 = 2;

// The CLI cap is interpreted as the local-boundary budget, with progressively
// more detail retained for larger administrative levels. This reduces border
// displacement without making a country and a municipality share one arbitrary
// vertex budget. Postal boundaries deliberately use the local cap: their
// reserved level is not an administrative hierarchy level and must not inherit
// country budgets.

/// Admin vertex-budget multiplier for levels 0 to 2 (applied to the base
/// `max_admin_vertices`).
pub(crate) const ADMIN_VERTEX_MULTIPLIER_LEVELS_0_TO_2: usize = 16;
/// Admin vertex-budget multiplier for level 3.
pub(crate) const ADMIN_VERTEX_MULTIPLIER_LEVEL_3: usize = 8;
/// Admin vertex-budget multiplier for level 4.
pub(crate) const ADMIN_VERTEX_MULTIPLIER_LEVEL_4: usize = 4;
/// Admin vertex-budget multiplier for level 5.
pub(crate) const ADMIN_VERTEX_MULTIPLIER_LEVEL_5: usize = 2;
/// Admin vertex-budget multiplier for every other level.
pub(crate) const ADMIN_VERTEX_MULTIPLIER_DEFAULT: usize = 1;

/// Default street-cell S2 level of `BuildConfig`.
pub(crate) const GEOCODE_DEFAULT_STREET_LEVEL: u8 = 17;

/// Default coarse-cell S2 level of `BuildConfig`.
pub(crate) const GEOCODE_DEFAULT_COARSE_LEVEL: u8 = 14;

/// Default admin-cell S2 level of `BuildConfig`.
pub(crate) const GEOCODE_DEFAULT_ADMIN_LEVEL: u8 = 10;

/// Default `max_admin_vertices` of `BuildConfig`.
pub(crate) const GEOCODE_DEFAULT_MAX_ADMIN_VERTICES: u16 = 500;

/// Default fine search radius of `BuildConfig`, in metres.
pub(crate) const GEOCODE_DEFAULT_FINE_SEARCH_RADIUS_M: f32 = 75.0;

/// Default coarse search radius of `BuildConfig`, in metres.
pub(crate) const GEOCODE_DEFAULT_COARSE_SEARCH_RADIUS_M: f32 = 1000.0;

/// Widest search radius, in smallest S2 cell widths at the radius's level,
/// that a build accepts and a reader opens. A query reads every cell that
/// can hold a point within the radius, roughly `(2 * ratio + 1)^2` cells per
/// level searched, so this bounds the per-query work a header can ask for.
/// The default radii are a few widths at their default levels; this leaves
/// room for deliberately wide searches while refusing a radius that would
/// make each query walk thousands of cells.
pub(crate) const GEOCODE_MAX_RADIUS_CELL_WIDTHS: f64 = 32.0;

/// Smallest width, in coarse search radii, of the cells the geocode reader
/// proves a region empty with before a coarse search. The 3x3 block of such
/// cells around a query reaches at least about one width, so at two radii it
/// covers the coarse radius for nearly every query; a finer level would test
/// emptiness more sharply but cover the radius less often.
pub(crate) const GEOCODE_PRESENCE_CELL_RADII: f64 = 2.0;

/// Cells a geocode query's search of one level keeps inline before its walk
/// spills to the heap. A default radius reaches a few dozen cells at most,
/// rejected border cells included, so the ordinary query allocates nothing.
pub(crate) const GEOCODE_WALK_INLINE_CELLS: usize = 128;

/// Slack, in radians, taken off each axis gap of the geocode reader's cell
/// lower bound (about 6 micrometres on the ground), so float rounding in the
/// gap and in the distance it is compared with can never lift the bound
/// above a real distance.
pub(crate) const GEOCODE_CELL_BOUND_SLACK_RAD: f64 = 1e-12;
