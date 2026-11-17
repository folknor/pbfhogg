I found five code defects. These are source-level findings; I changed no files and ran no builds or tests.

1. **[P1] A failed io_uring submission breaks completion accounting and can hang teardown.**  
   [uring_writer.rs:274](/home/folk/Programs/pbfhogg/crates/lib/src/write/uring_writer.rs:274)

   The SQE is pushed before `submit()`, but `in_flight` increments only after submission succeeds. A submit error therefore leaves a queued request outside the count.

   Concrete sequence: request A is counted, request B is queued, B's submit fails, and teardown's `submit_and_wait` submits B. If both completions are available, [reap_cqes](/home/folk/Programs/pbfhogg/crates/lib/src/write/uring_writer.rs:349) decrements a count of one twice. This panics with overflow checks or wraps in release. The next drain can wait forever for a completion that does not exist.

   This breaks the claimed drain accounting and complete writer teardown. Account for requests when they enter the SQ, including requests whose submission subsequently fails.

2. **[P1] io_uring cleanup can free buffers while outstanding writes still reference them.**  
   [uring_writer.rs:411](/home/folk/Programs/pbfhogg/crates/lib/src/write/uring_writer.rs:411), [uring_writer.rs:471](/home/folk/Programs/pbfhogg/crates/lib/src/write/uring_writer.rs:471)

   After `NUM_BUFS * 4` unsuccessful drain attempts, cleanup proceeds even when `in_flight` remains nonzero. It skips buffer unregistration and drops the ring and pool. The accompanying comment explicitly establishes that closing the ring is asynchronous.

   This violates the submission safety requirement that buffers remain valid throughout their operations. `WriteFixed` only reading the allocation does not make freeing it safe: an allocator that reuses that storage can expose another allocation's contents to an outstanding output write.

   The retry limit needs an ownership fallback. Retain the ring and allocation until quiescence is established, or deliberately abandon their ownership without freeing the allocation.

3. **[P2] `DirectWriter::flush` reports success after a failed finalization.**  
   [direct_writer.rs:173](/home/folk/Programs/pbfhogg/crates/lib/src/write/direct_writer.rs:173)

   `flush_final` sets `flushed = true` before writing the padded tail or truncating the file. If either operation fails, every subsequent flush immediately returns `Ok(())`. `Drop` also skips the unfinished work.

   For example, a tail write fails with ENOSPC; space becomes available; retrying flush succeeds without writing the tail. A truncation failure similarly leaves padding while later flushes claim success.

   This breaks `flush`'s documented finalization contract. It affects the public `DirectWriter`, including one wrapped in `PbfWriter::new`. The pipelined `PbfWriter`'s retained flush error protects that particular route.

4. **[P2] `DirectWriter::write` mishandles partial failures, producing duplicate bytes or zero-filled extensions on recovery.**  
   [direct_writer.rs:165](/home/folk/Programs/pbfhogg/crates/lib/src/write/direct_writer.rs:165), [direct_writer.rs:217](/home/folk/Programs/pbfhogg/crates/lib/src/write/direct_writer.rs:217)

   `logical_size` counts the entire supplied slice before it is accepted. If flushing the first buffer fails, the remainder of the slice was never copied, but it is already included in the eventual truncation length.

   With a two-buffer input, failure while flushing the first buffer followed by a successful flush writes that first buffer and zero-extends the file over the missing second buffer. Separately, `flush_buf` retains the entire buffer after a partial write followed by an error, although the file cursor has advanced. Retrying writes the prefix again.

   This breaks both the `Write::write` acceptance contract and the promised logical byte count. Fixing the finalization latch alone will not fix this: acceptance accounting and partial-flush progress also need correct state.

5. **[P2] `write_header` bypasses the promised cancellation checkpoint.**  
   [writer.rs:1008](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:1008)

   Every other public write entry checks cancellation. `write_header` directly frames and writes.

   A synchronous writer created with `PbfWriter::new_in` will accept and write a header after its invocation has been cancelled, returning success. This contradicts the adjacent [per-write cancellation contract](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:992). Publication still prevents a cancelled staged output from being published; the defect is the write method's behavior.

Other discrepancies worth flagging:

- **The transport documentation is incorrect.** [supports_copy_range](/home/folk/Programs/pbfhogg/crates/lib/src/write/options.rs:77) describes kernel-space splicing and includes `Uring`, but [UringSink::copy_range](/home/folk/Programs/pbfhogg/crates/lib/src/write/uring_writer.rs:830) reads through userspace into registered buffers. [reference/pipeline.md:162](/home/folk/Programs/pbfhogg/reference/pipeline.md:162) also incorrectly attributes `copy_file_range` to `write_raw` and `write_raw_chunks`. The implementations preserve bytes; these descriptions should change.

- **“Negative IDs are rejected project-wide” is an incorrect blanket claim.** [BlockBuilder](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:748) accepts negative IDs, and the writer's [index scanner](/home/folk/Programs/pbfhogg/crates/lib/src/blob_meta/scan.rs:279) preserves them. The detailed deviations already describe deliberate acceptance and silent handling at other sites. The blanket wording in [DEVIATIONS.md:564](/home/folk/Programs/pbfhogg/DEVIATIONS.md:564) disagrees with both code and its own subsequent explanation.

- **Invalid directly constructed compression levels panic.** [Compression](/home/folk/Programs/pbfhogg/crates/lib/src/write/compression.rs:7) explicitly documents that `Zlib(10)` reaches a backend panic. Parsed CLI levels are protected. This is an existing documented exception to the broad refusal guarantee in the question; its stated compatibility justification conflicts with the supplied pre-1.0 premise.
