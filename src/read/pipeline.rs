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
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::sync_channel;
use std::time::Instant;

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

/// Runtime-tunable pipeline buffering configuration.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PipelineConfig {
    pub(crate) read_ahead: usize,
    pub(crate) decode_ahead: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            read_ahead: DEFAULT_READ_AHEAD,
            decode_ahead: DEFAULT_DECODE_AHEAD,
        }
    }
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
/// # Memory warning: cross-thread PrimitiveBlock retention
///
/// Each `PrimitiveBlock` allocates `WireStringTable::entries` (~10 KB) and
/// `group_ranges` (~8 bytes) on a rayon decode thread. The consumer drops
/// them on the calling thread. Neither glibc nor jemalloc returns these
/// freed pages to the OS promptly - they accumulate as anonymous RSS.
///
/// At 400K+ blocks (Europe/planet scale), this causes **25+ GB of heap
/// retention** that the allocator holds as "free but mapped" memory.
/// This was measured and verified across glibc, jemalloc, and multiple
/// `MALLOC_ARENA_MAX` configurations.
///
/// **Mitigation patterns:**
/// - **Sequential reader**: use `BlobReader` directly instead of this
///   pipeline. All alloc/free on one thread. Used by external join stages 2+4.
/// - **Node-only scanner**: use `scan::node::extract_node_tuples`
///   to bypass PrimitiveBlock entirely. Zero per-block heap allocations.
///   Used by external join stage 2 and ALTW dense/sparse pass 1.
/// - **Batch-based consumers** (e.g., `for_each_primitive_block_batch` with
///   `par_iter`) are partially mitigated because the batch processes blocks
///   on the consumer's rayon pool, reducing the cross-thread window.
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
    type RawItem = (usize, crate::error::Result<crate::blob::Blob>);
    type DecodedItem = (usize, Option<crate::error::Result<PrimitiveBlock>>);

    // Enable tagdata parsing only when the filter needs tag key matching.
    // Enable indexdata parsing only when any filter is active (should_skip_blob
    // checks blob.index() for type + spatial filtering).
    let has_tag_filter = blob_filter.as_ref().is_some_and(BlobFilter::has_tag_filter);
    blob_reader.set_parse_tagdata(has_tag_filter);
    blob_reader.set_parse_indexdata(blob_filter.is_some());
    let blob_filter = blob_filter.map(Arc::new);
    let (raw_tx, raw_rx) = sync_channel::<RawItem>(pipeline_config.read_ahead);
    let (decoded_tx, decoded_rx) = sync_channel::<DecodedItem>(pipeline_config.decode_ahead);

    std::thread::scope(|scope| {
        // Stage 1: Sequential I/O reader thread
        scope.spawn(move || {
            for (seq, blob_result) in blob_reader.enumerate() {
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
                    drop(dispatch_tx.send((0, Some(Err(err)))));
                    return;
                }
            };
            let buffer_pool = DecompressPool::new();
            for (seq, blob_result) in raw_rx {
                let tx = dispatch_tx.clone();
                let bp = Arc::clone(&buffer_pool);
                match blob_result {
                    Ok(blob) => {
                        let bf = blob_filter.clone();
                        PIPELINE_METRICS.decode_tasks.fetch_add(1, Relaxed);
                        decode_pool.spawn(move || {
                            // Thread-local scratch buffers for parse_and_inline.
                            // Avoids allocating fresh Vec<(u32, u32)> per blob.
                            thread_local! {
                                static ST_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
                                static GR_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
                            }
                            let item = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                match blob.get_type() {
                                    BlobType::OsmData => {
                                        if let Some(ref filter) = bf
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
                                                    .to_primitiveblock_inline_with_scratch(&bp, st, gr);
                                                // Per-thread scratch retention is the iter-5
                                                // residual alloc bucket (4.4 GB / 70 % at Japan).
                                                // Record current capacity in (u32, u32) pairs *
                                                // 8 bytes for the global peak.
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
                                Err(_) => Some(Err(crate::error::new_error(
                                    crate::error::ErrorKind::Io(
                                        std::io::Error::other("decode task panicked"),
                                    ),
                                ))),
                            };
                            let t_send = Instant::now();
                            drop(tx.send((seq, item)));
                            PIPELINE_METRICS
                                .decoded_send_wait_ns
                                .fetch_add(elapsed_ns_u64(t_send), Relaxed);
                        });
                    }
                    Err(e) => {
                        // Forward I/O error directly to main thread
                        let t_send = Instant::now();
                        drop(tx.send((seq, Some(Err(e)))));
                        PIPELINE_METRICS
                            .decoded_send_wait_ns
                            .fetch_add(elapsed_ns_u64(t_send), Relaxed);
                    }
                }
            }
            // dispatch_tx clone drops here
        });

        // Drop the original so the channel closes when all rayon task clones are done
        drop(decoded_tx);

        // Stage 3: Reorder buffer on main thread - deliver blocks in file order.
        //
        // Reorder by sequence number and emit only contiguous ready items.
        // The underlying storage is VecDeque-based and bounded by decode_ahead.
        //
        // Each slot is `Option<Option<Result<PrimitiveBlock>>>`:
        //   - Outer `None`  → slot not yet filled (decode still in progress)
        //   - `Some(None)`  → slot filled, but blob was a header/unknown (skip)
        //   - `Some(Some(Ok(block)))` → decoded data block ready to deliver
        //   - `Some(Some(Err(e)))` → decode or I/O error to propagate
        let mut pending: ReorderBuffer<Option<Result<PrimitiveBlock>>> =
            ReorderBuffer::with_capacity(pipeline_config.decode_ahead);

        // Use explicit recv() instead of `for ... in decoded_rx` so we
        // can attribute time spent waiting on stage 2. Iterator desugaring
        // would hide the recv inside the loop scaffolding and prevent
        // wrapping it with the timer.
        let result: Result<()> = (|| {
            loop {
                let t_recv = Instant::now();
                let next = decoded_rx.recv();
                PIPELINE_METRICS
                    .decoded_recv_wait_ns
                    .fetch_add(elapsed_ns_u64(t_recv), Relaxed);
                let (seq, item) = match next {
                    Ok(pair) => pair,
                    Err(_) => break, // all senders dropped, drain finished
                };
                pending.push(seq, item);
                PIPELINE_METRICS.record_reorder_high_water(pending.pending_len());

                while let Some(item) = pending.pop_ready() {
                    match item {
                        Some(Ok(block)) => {
                            block_fn(block)?;
                        }
                        Some(Err(e)) => return Err(e),
                        None => {} // header or unknown blob - skip
                    }
                }
            }
            Ok(())
        })();

        // Emit reader-pipeline counters before returning so that even
        // an error path produces sidecar data. Mirrors the writer's
        // WRITER_METRICS.emit() in flush().
        PIPELINE_METRICS.emit();
        result
    })
}
