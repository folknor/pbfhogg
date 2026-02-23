//! Three-stage pipelined PBF reader.
//!
//! Overlaps sequential I/O with parallel decompression and protobuf parsing,
//! delivering decoded `PrimitiveBlock`s to a caller-supplied closure in file order.

use crate::blob::{BlobReader, BlobType};
use crate::block::PrimitiveBlock;
use crate::error::Result;
use std::collections::HashMap;
use std::io::Read;
use std::sync::mpsc::sync_channel;

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
pub(crate) fn run_pipeline<R, F>(blob_reader: BlobReader<R>, mut block_fn: F) -> Result<()>
where
    R: Read + Send,
    F: FnMut(&PrimitiveBlock) -> Result<()>,
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
        // Uses a separate pool so the global rayon pool remains free for caller work
        // (e.g. parallel geometry processing in tilegen).
        let dispatch_tx = decoded_tx.clone();
        scope.spawn(move || {
            let decode_pool = rayon::ThreadPoolBuilder::new()
                .num_threads(4)
                .build()
                .expect("failed to build decode pool");
            for (seq, blob_result) in raw_rx {
                let tx = dispatch_tx.clone();
                match blob_result {
                    Ok(blob) => {
                        decode_pool.spawn(move || {
                            let item = match blob.get_type() {
                                BlobType::OsmData => Some(blob.to_primitiveblock()),
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

        // Stage 3: Reorder buffer on main thread — deliver blocks in file order
        let mut next_seq: usize = 0;
        let mut pending: HashMap<usize, Option<Result<PrimitiveBlock>>> = HashMap::new();

        for (seq, item) in decoded_rx {
            pending.insert(seq, item);

            // Drain all consecutive ready blocks
            while let Some(item) = pending.remove(&next_seq) {
                next_seq += 1;
                match item {
                    Some(Ok(ref block)) => block_fn(block)?,
                    Some(Err(e)) => return Err(e),
                    None => {} // header or unknown blob — skip
                }
            }
        }

        Ok(())
    })
}
