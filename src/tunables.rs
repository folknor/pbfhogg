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
//! Merge-changes has no section of its own: its tunables are the worker triple
//! and the idle `recv_timeout` below. Its chunking rule (one chunk per worker)
//! and the `inputs.len() <= 1` switch between the buffered and streaming
//! strategies select different gzip member layouts, which makes them output
//! contract facts rather than tunables, so they stay in `merge_changes`.
//!
//! Public tunables (`pub const`) are re-exported from the path they were
//! public at, so no public path changed.

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

/// Depth of the result channel a hand-rolled decode pool's workers send to.
/// Altw stage 4 records why 32 is right there: the 256-depth A/B probe
/// confirmed that `s4_send_ms` pressure is steady-state consumer/compression
/// saturation, not burst absorption. A deeper channel just moves the wait into
/// the writer pipeline with no wall change.
pub(crate) const POOL_RESULT_CHANNEL_DEPTH: usize = 32;

/// Capacity of the reorder buffer behind a hand-rolled decode pool (the
/// common case).
pub(crate) const POOL_REORDER_CAPACITY: usize = 32;

/// Capacity of the wider reorder buffers (cat, time-filter and altw stage 1's
/// per-way stream).
pub(crate) const POOL_REORDER_CAPACITY_WIDE: usize = 64;

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

/// Maximum capacity (bytes) of a buffer retained in the decompression pool.
/// Buffers larger than this are dropped on return instead of recycled.
/// This prevents outlier blobs (up to 32 MB decompressed) from permanently
/// inflating the pool's retained memory.
///
/// Set to 4 MB: covers >99% of real-world PBF blocks (8000 elements at
/// typical sizes) while dropping the long tail of outlier blocks.
pub(crate) const DECOMPRESS_POOL_MAX_RETAINED_CAPACITY: usize = 4 * 1024 * 1024;

/// Maximum number of buffers retained in the decompression pool.
/// Defense-in-depth: prevents unbounded pool growth if pipeline topology changes.
pub(crate) const DECOMPRESS_POOL_MAX_BUFFERS: usize = 64;

// The retained payload capacity is at most 64 * 4 MiB = 256 MiB. Buffers
// rejected by either limit are dropped and counted for the read sidecar.
const _: () = assert!(
    DECOMPRESS_POOL_MAX_BUFFERS * DECOMPRESS_POOL_MAX_RETAINED_CAPACITY <= 256 * 1024 * 1024,
    "the decompression pool's retained payload bound is documented as 256 MiB"
);

/// Decompression buffer capacity guess, as a multiple of the compressed size:
/// the capacity requested for a blob that declares no `raw_size`, and for the
/// pooled buffers the decode workers fetch per blob of `data_size` compressed
/// bytes.
pub(crate) const DECOMPRESS_EXPANSION_GUESS: usize = 4;

/// Largest `PrimitiveBlock.granularity` (nanodegrees per raw unit) the node
/// scanner will decode coordinates from: one degree per unit. Bounds
/// `granularity * cum` inside i64 for any plausible delta.
pub(crate) const MAX_NODE_GRANULARITY: i64 = 1_000_000_000;

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

/// Shared buffered and aligned I/O transfer size.
pub(crate) const IO_BUF_BYTES: usize = 256 * 1024;
const _: () = assert!(IO_BUF_BYTES.is_multiple_of(crate::write::PAGE_SIZE));

/// Default per-block element cap used by `BlockBuilder::new`.
///
/// Matches the common PBF packing convention used by osmium.
pub const DEFAULT_ELEMENT_CAP: usize = 8000;

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
/// dispatch doesn't outrun the pool; each WriteOp holds a full owned
/// `Vec<u8>` (~800 KB for the rewrite path), so
/// `PER_WORKER_CAPACITY` × `POOL_SIZE` × 800 KB ≈ 100 MB buffered
/// across the pool at the current 8 × 16 - within the pipeline's
/// existing memory budget, but note it scales with `POOL_SIZE`.
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
/// A send can block on a full channel while the writer is inside `write_chunk`.
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
/// the three buffers is sized by the block's raw or framed bytes; the read
/// side's `DecompressPool` uses the same 4 MiB retention limit on the same
/// reasoning), so the cap only engages on outliers; larger buffers are handed
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
const _: () = assert!(
    MAX_RETAINED_SCRATCH_CAPACITY == DECOMPRESS_POOL_MAX_RETAINED_CAPACITY,
    "the write-side scratch retention limit is documented as the same as the read-side pool's"
);

/// Zlib worst-case output bound reserved up front: input + input >> this
/// shift (~0.1%) + [`ZLIB_RESERVE_SLACK`] header/trailer bytes.
pub(crate) const ZLIB_RESERVE_SHIFT: u32 = 10;

/// Header/trailer slack of the zlib worst-case output bound.
pub(crate) const ZLIB_RESERVE_SLACK: usize = 64;

// ---------------------------------------------------------------------------
// Apply-changes
// ---------------------------------------------------------------------------

/// Channel capacity for the unified DrainItem stream from scanner +
/// workers. Sized for ~16 in-flight items per worker plus headroom; the
/// drain's reorder buffer absorbs out-of-order arrivals beyond this.
pub(crate) const DRAIN_CHANNEL_CAPACITY: usize = 256;

/// Channel capacity for the scanner→workers candidate dispatch. Workers
/// pull at decompress + parse pace (ms per blob); the producer (scanner)
/// runs at header-walk pace (~µs per blob), so a moderate buffer keeps
/// workers fed during scanner pauses.
pub(crate) const CANDIDATE_CHANNEL_CAPACITY: usize = 64;

/// Drain `recv_timeout` that breaks the drain/scanner barrier cycle by
/// letting drain re-poll `last_node_seq` and fire the barrier from idle.
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

/// Result window per worker. Large enough that a worker is never parked
/// behind a slightly slow neighbour, small enough that the buffered
/// rewritten blobs stay a few hundred MB at worst.
pub(crate) const RENUMBER_WINDOW_PER_WORKER: usize = 8;

/// Floor of the renumber pool's result window.
pub(crate) const RENUMBER_WINDOW_MIN: usize = 32;

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
// Filtering and extract
// ---------------------------------------------------------------------------

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
/// bucketing and costs more RAM than the 15-byte scratch records because
/// `ParsedBucketEntry` is padded to 16 bytes.
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
