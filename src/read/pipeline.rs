//! Three-stage pipelined PBF reader.
//!
//! Overlaps sequential I/O with parallel decompression and protobuf parsing,
//! delivering decoded `PrimitiveBlock`s to a caller-supplied closure in file order.

use super::blob::{BlobReader, BlobType, DecompressPool};
use super::block::PrimitiveBlock;
use crate::blob_index::BlobFilter;
use crate::error::Result;
use std::collections::VecDeque;
use std::io::Read;
use std::sync::mpsc::sync_channel;
use std::sync::Arc;

/// Number of raw blobs the I/O thread can read ahead.
const READ_AHEAD: usize = 16;

/// Number of decoded blocks that can be in-flight before backpressure stalls decode.
const DECODE_AHEAD: usize = 32;

/// Runs a three-stage pipeline over a PBF file:
///
/// 1. **Reader thread**: sequential I/O, reads raw `Blob`s from the file.
/// 2. **Rayon pool**: parallel decompression (zlib) + protobuf parse.
/// 3. **Main thread**: reorder buffer delivers `PrimitiveBlock`s in file order to `block_fn`.
///
/// The closure runs on the calling thread and may hold mutable state.
/// PBF ordering (nodes → ways → relations) is preserved.
#[allow(clippy::needless_pass_by_value)]
#[hotpath::measure]
pub(crate) fn run_pipeline<R, F>(
    blob_reader: BlobReader<R>,
    decode_thread_count: Option<usize>,
    blob_filter: Option<BlobFilter>,
    mut block_fn: F,
) -> Result<()>
where
    R: Read + Send,
    F: FnMut(PrimitiveBlock) -> Result<()>,
{
    type RawItem = (usize, crate::error::Result<crate::blob::Blob>);
    type DecodedItem = (usize, Option<crate::error::Result<PrimitiveBlock>>);

    let (raw_tx, raw_rx) = sync_channel::<RawItem>(READ_AHEAD);
    let (decoded_tx, decoded_rx) = sync_channel::<DecodedItem>(DECODE_AHEAD);

    std::thread::scope(|scope| {
        // Stage 1: Sequential I/O reader thread
        scope.spawn(move || {
            for (seq, blob_result) in blob_reader.enumerate() {
                if raw_tx.send((seq, blob_result)).is_err() {
                    break; // receiver dropped, pipeline shutting down
                }
            }
        });

        // Stage 2: Dispatcher thread — fans out to dedicated pool for parallel decode.
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
                        decode_pool.spawn(move || {
                            let item = match blob.get_type() {
                                BlobType::OsmData => {
                                    // If a blob filter is set and the blob has indexdata,
                                    // skip decompression for blobs that don't match.
                                    // Files without indexdata always pass through.
                                    if let Some(ref filter) = blob_filter
                                        && let Some(idx) = blob.index()
                                        && !filter.wants(idx.kind)
                                    {
                                        drop(tx.send((seq, None)));
                                        return;
                                    }
                                    Some(blob.to_primitiveblock_pooled(&bp))
                                }
                                _ => None,
                            };
                            drop(tx.send((seq, item)));
                        });
                    }
                    Err(e) => {
                        // Forward I/O error directly to main thread
                        drop(tx.send((seq, Some(Err(e)))));
                    }
                }
            }
            // dispatch_tx clone drops here
        });

        // Drop the original so the channel closes when all rayon task clones are done
        drop(decoded_tx);

        // Stage 3: Reorder buffer on main thread — deliver blocks in file order.
        //
        // We use a VecDeque instead of a HashMap because:
        //   - Sequence numbers are consecutive integers (0, 1, 2, …) and we always
        //     drain from the front in order, which is exactly what VecDeque excels at.
        //   - The out-of-order window is bounded: at most DECODE_AHEAD items can be
        //     in-flight, so the deque never grows larger than that.
        //   - VecDeque stores elements contiguously in a ring buffer, giving
        //     cache-friendly iteration and O(1) push/pop from both ends. HashMap
        //     has hashing overhead, pointer chasing, and worse cache locality for
        //     this access pattern.
        //
        // Indexing scheme:
        //   Slot index = seq - next_seq. When seq == next_seq the item lands at
        //   index 0 (the front). When a block arrives with seq > next_seq, we may
        //   need to grow the deque with empty (None) slots to reach that index.
        //
        // Each slot is `Option<Option<Result<PrimitiveBlock>>>`:
        //   - Outer `None`  → slot not yet filled (decode still in progress)
        //   - `Some(None)`  → slot filled, but blob was a header/unknown (skip)
        //   - `Some(Some(Ok(block)))` → decoded data block ready to deliver
        //   - `Some(Some(Err(e)))` → decode or I/O error to propagate
        let mut next_seq: usize = 0;
        let mut pending: VecDeque<Option<Option<Result<PrimitiveBlock>>>> =
            VecDeque::with_capacity(DECODE_AHEAD);

        for (seq, item) in decoded_rx {
            let slot_idx = seq - next_seq;

            // Grow the deque with empty (unfilled) slots if this sequence number
            // is beyond the current length. This happens when items arrive out of
            // order — e.g. blob 5 finishes decoding before blob 3.
            if slot_idx >= pending.len() {
                pending.resize_with(slot_idx + 1, || None);
            }

            // Fill the slot. The slot must be None (unfilled) because each
            // sequence number is unique.
            pending[slot_idx] = Some(item);

            // Drain all consecutive ready blocks from the front.
            // We peek at front() to check if the next slot is filled, then
            // pop_front() to take ownership. Unfilled slots (outer None) at
            // the front mean we're still waiting for an earlier blob to
            // finish decoding, so we stop and wait for the next channel recv.
            loop {
                // Check if the front slot exists and is filled (Some(_)).
                // We can't pop directly because we need to distinguish
                // "front is None (unfilled)" from "deque is empty".
                let front_is_filled = pending.front().is_some_and(Option::is_some);
                if !front_is_filled {
                    break;
                }
                // Safe to unwrap: we just confirmed front is Some(Some(_)).
                #[allow(clippy::unwrap_used)]
                let item = pending.pop_front().unwrap().unwrap();
                next_seq += 1;
                match item {
                    Some(Ok(block)) => block_fn(block)?,
                    Some(Err(e)) => return Err(e),
                    None => {} // header or unknown blob — skip
                }
            }
        }

        Ok(())
    })
}
