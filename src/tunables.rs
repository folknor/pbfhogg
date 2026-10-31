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
/// exception where a blob's header exceeds the probe window.
pub(crate) const HEADER_PROBE_SIZE: usize = 4096;

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
/// even while its item is still buffered. They therefore do not bound channel
/// occupancy together with active tasks, or items held in the reorder buffer.
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
/// Memory bound: `PIPELINE_DISPATCH_PERMITS × max_block_size`. At ~4 MB per
/// uncompressed block and 64 permits that's ~256 MB worst case, small
/// compared to the 2.79 GB stage-2b peak.
pub(crate) const PIPELINE_DISPATCH_PERMITS: usize = 64;

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
/// pool.
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
/// built under the default 8000-element cap are normally below this (each of
/// the three buffers is sized by the block's raw or framed bytes; writer
/// counters show few buffers capped at planet), so the cap only engages on
/// outliers. It is independent of the read side's `DecompressPool` limit,
/// whose buffers also carry inline tables; larger buffers are handed
/// to the writer or dropped, then the thread-local scratch is allowed to warm
/// up again. A run whose blocks all exceed it (repack with a raised
/// `--elements-per-blob`) pays a fresh allocation per buffer per block, which
/// is small next to compressing a block of that size. On the pipelined path
/// the blob and header buffers are handed off and replaced on every blob
/// regardless, so only `compress_buf` loses cross-blob reuse there; the sync
/// writer path writes from all three in place and loses reuse on each. The
/// three scratch Vecs therefore retain at most 12 MiB per `FrameScratch`, in
/// addition to the reused compressor state.
pub(crate) const MAX_RETAINED_SCRATCH_CAPACITY: usize = 4 * 1024 * 1024;

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
/// control message under `--locations-on-ways`. Fixed, so the share per
/// worker shrinks as the worker count (`nproc` minus a reserve, or `-j`)
/// grows. It does not bound the drain's separate reorder buffer, which
/// absorbs out-of-order arrivals beyond it.
pub(crate) const DRAIN_CHANNEL_CAPACITY: usize = 256;

/// Channel capacity for the scanner→workers candidate dispatch. Workers
/// pull at decompress + parse pace (ms per blob); the producer (scanner)
/// runs at header-walk pace (~µs per blob), so a moderate buffer keeps
/// workers fed during scanner pauses.
pub(crate) const CANDIDATE_CHANNEL_CAPACITY: usize = 64;

/// Drain `recv_timeout`: how long the drain blocks on an empty item channel
/// before checking the producer failure slot and the cancellation token, and
/// re-polling the scanner's node boundary so the barrier can fire from idle.
/// A failed worker never delivers its seq while the scanner can sit in its
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
// Re-encoding (repack, degrade)
// ---------------------------------------------------------------------------

/// Batch size for the parallel-framing fan-out on the merge thread. The
/// merge thread accumulates `OwnedBlock`s in seq order as the central
/// builder flushes; once a batch is full it is framed in parallel via
/// rayon and the framed bytes are written in seq order. Shared by repack and
/// degrade.
pub(crate) const FRAME_BATCH: usize = 32;

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
/// sending it; a frame larger than this travels alone. Sized so a typical
/// run (two dense planet blobs merged, about 2 MB compressed) completes in a
/// chunk or two without parking behind the write loop, which is what lets
/// later runs proceed while the head drains; not otherwise measured.
pub(crate) const OVERLAP_RUN_CHUNK_BYTES: usize = 2 << 20;

/// Decoded block bytes one overlap sweep may pin before it demotes mostly
/// consumed cursors (less than an eighth of their elements left) to owned
/// tails, releasing their blocks. Two dense planet blobs pin about 20 MB, so
/// ordinary runs never demote; a run with many sparse tails stops pinning
/// whole blocks for a few elements each. Not measured.
pub(crate) const SWEEP_PINNED_BYTES: usize = 64 << 20;

/// Heap removals (emitted elements and dropped duplicates alike) an overlap
/// sweep makes between two checks of cancellation and abandonment, beside
/// the check before each blob it reads. Without it a run's final heap drain,
/// or a long stretch of exact duplicates that fills no block, ran unchecked
/// to the end. Two atomic loads per check, so the interval only has to
/// keep a check's cost far below the removals it covers; not measured.
pub(crate) const SWEEP_CHECKPOINT_REMOVALS: usize = 4096;

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

/// Maximum number of extracts a multi-extract config may declare.
pub(crate) const MAX_EXTRACTS: usize = 500;

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Squared segment length below which a segment is treated as degenerate (a
/// single point) by the cos-projection closest-point and Douglas-Peucker
/// code.
pub(crate) const DEGENERATE_SEGMENT_EPSILON: f64 = 1e-30;

/// Headroom, in segment placements, that `ring_search_budget` grants on top
/// of its quadratic allowance. It is spent only at branching endpoints.
#[cfg(feature = "commands")]
pub(crate) const RING_SEARCH_EXTRA_STEPS: u64 = 1 << 20;

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

/// Per-segment sampling budget: 2^24 points. In the pathological case that
/// is roughly 270 MB of dedup set on one rayon worker and a few seconds; in
/// every real case - OSM segments are a small fraction of a degree - it is a
/// handful of samples and the budget never comes near being spent.
pub(crate) const MAX_SEGMENT_SAMPLES: f64 = 16_777_216.0;

/// Largest separation two legal coordinates can have in the flat degree
/// metric `cover_segment` measures with: `hypot(180, 360)` = 402.5, rounded
/// up. Latitude spans at most 180 degrees and longitude at most 360.
pub(crate) const MAX_SEGMENT_SPAN_DEG: f64 = 405.0;

/// Highest S2 level accepted for the segment-sampled street and coarse cells.
///
/// Pinned to `MAX_SEGMENT_SAMPLES` by `level_exceeds_sampling_budget` and by
/// the unit test in pass 3, which walks every level and asserts this is
/// exactly the boundary - raising the constant without raising the budget
/// fails the test rather than reintroducing the hang.
pub(crate) const MAX_BUILD_S2_LEVEL: u8 = 19;
const _: () = assert!(MAX_BUILD_S2_LEVEL <= crate::geocode_index::format::S2_MAX_LEVEL);

/// Maximum number of S2 cells the admin flood fill may visit for one polygon.
/// This is the full sphere's cell count at level 10: six faces times 4^10.
pub(crate) const ADMIN_CELL_COUNT_BUDGET: u64 = 6_291_456;
const _: () = assert!(
    ADMIN_CELL_COUNT_BUDGET == 6 * (1 << 20),
    "the admin cell budget is documented as six faces times 4^10"
);

/// Factor of the globally smallest S2 cell width at a level that the segment
/// sampling step uses: a little under half, so no cell can be stepped over.
pub(crate) const SEGMENT_STEP_WIDTH_FACTOR: f64 = 0.4;

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

/// Tolerance, in E7 units on each axis, under which an address point counts as
/// exactly at an interpolation endpoint.
pub(crate) const INTERP_ENDPOINT_EXACT_TOLERANCE_E7: i64 = 1;

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
