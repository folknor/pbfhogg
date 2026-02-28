# Box 7: Linux Direct I/O and io_uring Path

## 1. Executive Summary

- **io_uring operational complexity is real but well-mitigated.** The code handles RLIMIT_MEMLOCK errors with actionable messages, SQ overflow with squeue_wait(), and init failures with synchronous error propagation. The remaining risk is kernel version sensitivity (sqpoll requires 5.12+ for unprivileged use), which is documented but not runtime-detected.

- **Write amplification is negligible.** At most 4095 bytes of padding on the final buffer, followed by ftruncate. For a 465 MB Denmark file, that is <0.001%. The reviewer's concern about "tail-heavy workloads" does not apply -- O_DIRECT padding occurs only at file end, not per-blob.

- **copy_file_range branching complexity is justified by the performance delta.** Three I/O modes (buffered + copy_file_range, O_DIRECT sync, io_uring + linked ReadFixed/WriteFixed) serve genuinely different scale points. The branching in merge.rs (lines 1036-1041) is a single expression and the fallback paths are correct.

- **The linked ReadFixed+WriteFixed chain has a subtle pre-zeroing correctness issue that is handled correctly.** Pre-zeroing the padding region at lines 308-311 happens before ReadFixed fills the buffer, so the read overwrites [0..len) and the padding [len..aligned_len) remains zeroed. This is correct because O_DIRECT reads on Linux guarantee filling the requested length from a regular file, and short reads are treated as errors (lines 479-484) with the linked write canceled.

- **The fd registration stall (drain before register_files_update) is the most impactful real-world performance concern, but analysis shows it is bounded to <1 ms in practice.** On the first CopyRange, all in-flight SQEs are drained (line 281). For merge with indexdata PBFs, the very first data blob is typically a passthrough node blob, meaning this drain happens almost immediately after the header write -- at most 1 buffer in flight.

## 2. Finding 1: Operational Complexity (Real, Well-Mitigated)

**Assessment: Real complexity, but defense-in-depth is solid.**

### What the reviewer said

> io_uring adds significant operational complexity: sensitivity to kernel version, RLIMIT_MEMLOCK, sqpoll behavior, queue depth. Risk of subtle bugs.

### What the code actually does

**RLIMIT_MEMLOCK handling** (`uring_writer.rs` lines 639-656):
`register_buffers` failure is caught with specific ENOMEM/EPERM detection. The error message includes the required memory (16 MB) and the fix (`ulimit -l unlimited`). This is better than most io_uring code in the wild.

**Init error propagation** (`writer.rs` lines 221-261):
A oneshot `SyncSender` carries init success/failure from the writer thread back to the constructor. If init fails, the thread is joined and the error propagated. If the thread panics before sending, that is caught too. This is correct and prevents silent failures.

**SQ overflow** (`uring_writer.rs` lines 382-438):
The `push_sqe` and `push_sqe_pair` functions handle SQ full by calling `squeue_wait()`. The `push_sqe_pair` function (lines 409-437) pre-checks for 2 free SQ slots before pushing either SQE, preventing a half-linked chain. This is the fix for the sqpoll crash mentioned in MEMORY.md, and it is correct.

**Kernel version sensitivity**: The code uses `setup_clamp()` (line 611) which tells the kernel to silently cap unsupported features rather than failing. This handles forward compatibility. However, there is no runtime detection of kernel version or opcode support via `register_probe()`. If io_uring creation fails on an old kernel, the error message is generic.

### Recommendations

1. (low priority) Add `register_probe()` at startup to verify WriteFixed/ReadFixed opcode support and emit a specific error if absent, instead of letting the first SQE fail cryptically.
2. (cosmetic) The sqpoll idle_ms of 2000ms (line 613) means the kernel thread stays alive for 2s after last I/O. Since benchmark data shows sqpoll adds <1% benefit, consider removing the sqpoll code path entirely to reduce maintenance surface. See section 7 for full analysis.

## 3. Finding 2: Write Amplification (Not Real)

**Assessment: Not a concern. Quantified below.**

### What the reviewer said

> Page-aligned padding increases write amplification. Tail-heavy workloads pad significantly.

### Quantified analysis

O_DIRECT padding occurs in exactly two places:

1. **Per-buffer padding** (`submit_buffer`, line 230): Each 256 KB buffer is already page-aligned in size. A full buffer has `data_len == 256 KB`, so `aligned_len == 256 KB`, zero padding. Only the *last* partially-filled buffer gets padded.

2. **Final ftruncate** (`flush_final`, line 519): After writing the last padded buffer, `set_len(logical_size)` truncates back to the actual data size. The padding bytes are never part of the final file.

**For a typical Denmark merge (465 MB output):**
- Last buffer has at most 256 KB - 1 byte of data.
- Padding: at most 4095 bytes (to next page boundary).
- Write amplification: 4095 / 487,587,840 = 0.00084%.
- After ftruncate, the file is exactly the correct size.

**For planet (80 GB output):**
- Same: at most 4095 bytes of padding.
- Write amplification: 4095 / 85,899,345,920 = 0.0000048%.

**There is no per-blob padding.** Each blob's framed bytes are accumulated into the 256 KB buffer via `UringState::write` (line 168). Only when the buffer fills completely (line 198: `self.current_len == self.pool.buf_size`) is it submitted. Blobs smaller than 256 KB share a buffer with no alignment waste.

### Crash safety consideration

The reviewer asked about a race between the last write and ftruncate. The sequence in `flush_final` (lines 516-522):

```
submit_current()  -> submit partial buffer (padded)
drain()           -> wait for ALL in-flight CQEs
set_len()         -> ftruncate to logical_size
sync_all()        -> fsync
```

If the process crashes after the padded write but before ftruncate, the file will be slightly larger than intended (up to 4095 extra zero bytes at the end). This is harmless for PBF files -- the PBF reader reads blob-by-blob using explicit length fields, so trailing zeros are never parsed. The ftruncate just keeps the file size clean.

There is no data corruption risk because `drain()` ensures all writes complete before truncation.

## 4. Finding 3: copy_file_range Complexity (Real, Justified)

**Assessment: Real branching complexity, but each path serves a distinct use case.**

### Three I/O modes for passthrough

| Mode | Writer type | Passthrough mechanism | When used |
|------|------------|----------------------|-----------|
| Buffered | BufWriter\<File\> | copy_file_range syscall | Default merge/sort |
| O_DIRECT sync | DirectWriter | Userspace copy via write_raw | `--direct-io` without `--io-uring` |
| io_uring | UringState | Linked ReadFixed+WriteFixed | `--io-uring` |

The branching in `merge.rs` (line 1040): `use_copy_range = io_uring || !direct_io`:
- Buffered output: `direct_io=false`, `io_uring=false` -> `use_copy_range = true` (copy_file_range)
- O_DIRECT sync: `direct_io=true`, `io_uring=false` -> `use_copy_range = false` (userspace copy)
- io_uring: `io_uring=true` -> `use_copy_range = true` (CopyRange payload -> linked SQEs)

**Why O_DIRECT sync cannot use copy_file_range:** `DirectWriter` maintains its own page-aligned buffer with a tracked write position. `copy_file_range` writes directly to the fd, bypassing the buffer, which would corrupt the file layout. The `FileWriter::flush_and_raw_fd()` method (`file_writer.rs` lines 52-61) returns `None` for the Direct variant, making this impossible.

**Why io_uring uses its own CopyRange instead of copy_file_range:** The output fd is O_DIRECT with io_uring managing the `write_offset`. A synchronous `copy_file_range` to that fd would advance the fd's file position independently of io_uring's `write_offset` tracking, corrupting the output. The linked ReadFixed+WriteFixed chain keeps everything within the ring's offset management.

### Alternative: eliminate copy_file_range entirely

Could the code use only userspace copy (`write_raw`) for all modes? Yes, but at a cost. `copy_file_range` on btrfs/xfs performs CoW reflinks (metadata-only, instant). Even on ext4, it avoids a userspace round-trip. For sort with mostly passthrough blobs (90%+ blobs unchanged), eliminating `copy_file_range` would add significant data movement.

### Recommendation

No change needed. The complexity is inherent to supporting three distinct I/O strategies. The branching is clean and each path is well-motivated.

## 5. AlignedBufferPool Analysis

### Sizing: 64 buffers x 256 KB = 16 MB

**Is 64 the right number?**

The pool has 64 buffers and the ring depth is `NUM_BUFS * 4 = 256`. The theoretical maximum in-flight:

- **Standalone WriteFixed:** Each write uses 1 buffer + 1 SQE. Max 64 writes in-flight = 64 SQEs.
- **Linked ReadFixed+WriteFixed pairs:** Each pair uses 1 buffer + 2 SQEs. Max 64 pairs = 128 SQEs.
- **Mixed workload:** Some buffers for standalone writes, some for linked pairs. Always bounded by 64 buffers total.

So the maximum SQE count is 128 (all buffers used for linked pairs). Ring depth of 256 provides 2x headroom. This is generous -- even with sqpoll's SQ accumulation behavior, 128 spare SQ slots prevent stalls.

**Is 64 too many?** The pipeline channel has `WRITE_AHEAD = 32` capacity (`writer.rs` line 33). So at most 32 items can be pending in the channel. But a single CopyRange item can consume multiple buffers (1.5 MB uncompressed blob / 256 KB per buffer = 6 buffers). A burst of 5-6 consecutive CopyRange items could consume all 64 buffers. Having 64 buffers prevents `acquire_buffer()` from blocking in this scenario.

**Math for worst case:** If 32 channel items are all CopyRange of 1.5 MB blobs: 32 x 6 = 192 buffers needed. This exceeds 64 buffers, so `acquire_buffer()` will block and wait for CQEs. However, the pipeline processes items sequentially in the main loop (line 697), and each CopyRange is fully submitted before the next is started (`handle_copy_range_uring` drains within its while loop). The 64-buffer pool is sufficient because only one CopyRange is active at a time. **64 is the right size.**

### Hugepages opportunity

The 16 MB contiguous allocation (64 x 256 KB) spans 8 hugepages (2 MB each). TLB coverage:
- Without hugepages: 16 MB / 4 KB = 4096 TLB entries needed
- With hugepages: 16 MB / 2 MB = 8 TLB entries needed

However, TLB pressure is unlikely to matter here. The write pattern is linear: fill one buffer, submit it, move to the next. At most 2 buffers are actively touched at once (current fill buffer + the one just submitted). That is 512 KB, fitting in ~128 TLB entries.

Adding `madvise(MADV_HUGEPAGE)` to the allocation would cost one line and might save a few TLB misses. But the benefit would be undetectable in benchmarks given the sequential access pattern.

**Recommendation:** Not worth the complexity. Skip hugepages.

### Threading: could rayon threads write directly into registered buffers?

Currently, rayon compression threads produce a `Vec<u8>` (`frame_blob_into`, `writer.rs` line 697), send it via the channel, and the writer thread copies it into a registered buffer (`UringState::write`, line 191).

To eliminate this copy, rayon threads would need to:
1. Acquire a buffer index from the pool (requires thread-safe access).
2. Compress directly into the registered buffer.
3. Send the buffer index through the channel.
4. The writer thread submits the pre-filled buffer.

**Problems:**
- The `AlignedBufferPool` would need to be shared across threads with a Mutex or atomic free-list. Currently it lives in the single writer thread.
- Compression output size is unknown before compression. `frame_blob_into` uses `Vec::with_capacity` for worst-case size, then truncates. With a fixed 256 KB registered buffer, the compressed output might not fit (though typical PBF blobs compress to 16-64 KB, well under 256 KB).
- A single framed blob may span multiple 256 KB buffers (`Compression::None` blobs are ~1.5 MB). The rayon thread would need to acquire multiple buffers and manage partial fills.
- The writer thread still needs to submit SQEs in sequence order. Moving buffer management to rayon threads adds complexity without eliminating the sequential submission requirement.

**The current memcpy cost:** Each compressed blob is typically 32-64 KB. Copying 64 KB into a registered buffer takes ~2 microseconds on modern hardware. At 10K blobs per merge, that is 20 ms total -- negligible compared to the 35+ second merge time.

**Recommendation:** Not worth it. The memcpy overhead is negligible and the complexity would be significant.

## 6. UringState Accumulation Pattern Analysis

### The write path (lines 168-203)

Data arrives as `&[u8]` slices. `UringState::write` copies into the current 256 KB registered buffer. When full, submits WriteFixed. This is a standard buffered I/O pattern, just with registered buffers instead of a kernel `BufWriter`.

**For compressed blobs (typical 32-64 KB):** ~4-8 blobs share one 256 KB buffer. Each WriteFixed SQE writes 256 KB = 64 pages of 4 KB. This is efficient -- NVMe drives handle 256 KB writes well.

**For Compression::None passthrough (blobs ~1.5 MB uncompressed):** Each blob fills ~6 registered buffers, generating 6 WriteFixed SQEs. Total per blob: 1.5 MB / 256 KB = 6 SQEs.

**Would scatter-gather (writev) be better for Compression::None?** No, for two reasons:
1. `WRITEV` does not support registered buffers (confirmed in `notes/linux-async-io.md` line 145). Using writev would lose the kernel page-pinning optimization.
2. Each 256 KB buffer is submitted as a WriteFixed as soon as it fills. The kernel processes them asynchronously while the next buffer is being filled. This effectively pipelines the writes.

**For CopyRange passthrough:** `handle_copy_range_uring` (lines 541-563) bypasses `UringState::write` entirely. Each 256 KB chunk gets a linked ReadFixed+WriteFixed pair. For a 55 KB framed blob (typical compressed passthrough), that is 1 pair. For a 1.5 MB uncompressed passthrough, that is 6 pairs. This is the optimal path since it avoids the userspace memcpy entirely.

### submit() frequency

Every `submit_buffer()` call (lines 260-263) calls `ring.submitter().submit()`. With 64 in-flight buffers and sequential writes, the kernel batches these internally. The `submit()` calls are not a bottleneck because:
- Without sqpoll: each `submit()` is one `io_uring_enter` syscall. At ~10-20K SQEs/s, that is 10-20K syscalls/s -- negligible.
- With sqpoll: `submit()` returns without a syscall most of the time.

## 7. sqpoll Analysis (Quantified Syscall Overhead)

### Benchmark data recap

From MEMORY.md, North America (18.8 GB):

| Config | ms | vs regular uring |
|--------|------|-----------------|
| uring+zlib | 32,621 | baseline |
| uring+sqpoll+zlib | 32,971 | +1.1% |
| uring+none | 25,500 | baseline |
| uring+sqpoll+none | 25,274 | -0.9% |

sqpoll adds no measurable improvement. The question is why.

### Syscall overhead calculation

Without sqpoll, each `submit()` call triggers one `io_uring_enter` syscall.

**Write throughput for North America merge (uring+none):**
- Output size: ~18.8 GB (similar to input with ~8% rewrite)
- Time: 25.5 seconds
- Throughput: 18.8 GB / 25.5 s = 737 MB/s
- Buffers submitted: 18.8 GB / 256 KB = 75,366 buffers

Plus CopyRange linked pairs. With ~92% passthrough:
- ~92% of 18.8 GB = 17.3 GB via CopyRange
- 17.3 GB / 256 KB = 69,230 linked pairs = 69,230 submits (each pair submitted together)
- Plus ~8% of blobs go through write path: ~6,000 standalone writes

Total `submit()` calls: ~75,000 (rough estimate).

**Syscall overhead:**
- `io_uring_enter` latency: ~0.5-1 us on modern kernels
- 75,000 x 1 us = 75 ms
- As fraction of total time: 75 ms / 25,500 ms = 0.29%

**This explains the benchmark data.** At 0.3% overhead, eliminating syscalls via sqpoll saves ~75 ms on a 25-second operation. That is within measurement noise.

**At what I/O rate would syscall overhead become significant (>5%)?**
- Need: syscall_time / total_time > 0.05
- At 1 us/syscall: need >5% of time in syscalls
- 5% of 25.5s = 1.275s = 1,275,000 syscalls
- At 256 KB per buffer: 1,275,000 x 256 KB = 327 GB output
- That requires ~327 GB of output, or roughly 4x planet scale

**sqpoll is not useful for this workload.** The syscall overhead only becomes significant at throughputs far beyond planet scale.

### SQ overflow fix correctness

The `push_sqe()` function (lines 382-401):
1. Try to push the SQE.
2. If SQ is full, call `squeue_wait()` to block until the kernel drains entries.
3. Retry the push.

The `push_sqe_pair()` function (lines 409-437):
1. Loop: check if SQ has at least 2 free slots.
2. If not, drop the SQ reference (without pushing) and call `squeue_wait()`.
3. Once 2 slots are available, push both SQEs atomically.

This is correct. The key insight is that `push_sqe_pair` checks capacity BEFORE pushing either SQE, preventing a half-linked chain. The capacity check at line 418 (`sq.capacity() - sq.len() >= 2`) uses the SQ's own view of free space, which is accurate because only this thread pushes.

## 8. Linked SQE Chain Correctness Analysis

### Buffer management in linked ReadFixed+WriteFixed

**submit_copy_chain** (lines 297-353):

1. Buffer is acquired before calling `submit_copy_chain` (`handle_copy_range_uring` line 556).
2. ReadFixed SQE: user_data encodes `buf_idx | READ_FLAG | (len << 16)`.
3. WriteFixed SQE: user_data encodes `buf_idx | (aligned_len << 16)` (no READ_FLAG).
4. Both use IO_LINK (line 323) -- if read fails, write is canceled.
5. `in_flight` incremented by 2 (line 350) -- both SQEs produce CQEs.

**reap_cqes** (lines 451-503):

- **Read CQE (READ_FLAG set):**
  - Success: Don't release buffer (lines 470-484). The linked write still needs it.
  - Failure (result < 0): Release buffer (line 476), return error. The write CQE will arrive as ECANCELED.
  - Short read: Release buffer (line 480), return error. Same ECANCELED handling.

- **Write CQE (READ_FLAG clear):**
  - ECANCELED: Skip without releasing (lines 487-491). Buffer was already released in the read error path.
  - Success: Release buffer (line 500).
  - Failure or short write: Return error (no release -- buffer is leaked on error, but we are about to shut down anyway).

**Is there a double-free risk?** No. On read failure:
1. Read CQE handler releases the buffer at line 476.
2. Write CQE arrives as ECANCELED, hits lines 487-491 which `continue`s without releasing.

On read success:
1. Read CQE handler does NOT release (lines 470-484 returns Ok).
2. Write CQE handler releases at line 500.

Each buffer is released exactly once in all paths. **Correct.**

**Is there a leak risk?** On write failure (non-ECANCELED, lines 492-494), the function returns Err without releasing the buffer. This is intentional -- the error causes the writer thread to exit, and the `AlignedBufferPool`'s `Drop` impl deallocates all memory. No actual leak since the pool owns the memory. But in-flight buffers are not tracked individually -- there is no cleanup loop that drains remaining CQEs on error. This is acceptable because the process is about to fail.

### Pre-zeroing correctness (lines 306-312)

```rust
if aligned_len > len {
    unsafe {
        std::ptr::write_bytes(buf_ptr.add(len), 0, aligned_len - len);
    }
}
```

This zeros `[len..aligned_len)` BEFORE the ReadFixed fills `[0..len)`. Sequence of events:

1. Pre-zero `[len..aligned_len)` with `write_bytes`.
2. Kernel executes ReadFixed: fills `[0..len)` from the input file.
3. Kernel executes WriteFixed: writes `[0..aligned_len)` to the output file.

Between step 1 and step 2, the entire buffer region `[0..aligned_len)` may contain anything. After step 2, `[0..len)` contains file data and `[len..aligned_len)` contains zeros. Step 3 writes the correct content.

**Is this safe if the read writes fewer than `len` bytes?** The code treats short reads as errors (lines 479-484), so the write is never executed on short read (IO_LINK cancels it). The pre-zeroed padding is irrelevant in the error case.

**What about re-using a buffer that previously held longer data?** The pre-zeroing only covers `[len..aligned_len)`. If a previous use stored data in `[0..len)`, the ReadFixed overwrites it entirely. If the previous use stored data in `[aligned_len..BUF_SIZE)`, that region is not read by the WriteFixed (it only writes `aligned_len` bytes). **No stale data leak.**

**Verdict: Correct.** The pre-zeroing before ReadFixed is safe because (a) ReadFixed fully overwrites `[0..len)`, (b) the padding `[len..aligned_len)` is zeroed before the write, and (c) short reads are treated as errors with the linked write canceled.

## 9. Additional Findings

### A. Ring depth sizing

Ring depth = `NUM_BUFS * 4` = 256.

Maximum SQE usage scenarios:
- All 64 buffers used for standalone writes: 64 SQEs.
- All 64 buffers used for linked R+W pairs: 128 SQEs.
- Theoretical mixed worst case: 128 SQEs (all linked) + queued standalone writes from `submit_current`.

The SQ depth (256) is the submission queue capacity. The CQ depth defaults to 2x SQ = 512. With 128 max SQEs in flight, 256 SQ depth provides 128 spare entries. With sqpoll where entries accumulate, this headroom prevents stalls on burst submissions.

**Is 256 enough?** Yes. The mathematical upper bound is 128 SQEs (64 linked pairs). Even with sqpoll accumulation lag, 256 provides adequate headroom. The `push_sqe`/`push_sqe_pair` functions handle SQ full gracefully with `squeue_wait()`. **Ring depth is correctly sized.**

### B. fd registration stall

`register_input_fd` (lines 271-288) calls `drain()` before `register_files_update`. This blocks until all in-flight SQEs complete.

**When does the first CopyRange occur in merge?**

For a typical merge with indexdata PBF, `classify_only` (lines 849-852) detects passthrough on the very first data blob via the inline index. The first data blob in a sorted PBF is a node blob. With a daily diff affecting <1% of node IDs, the first node blob almost always has a non-overlapping ID range -> passthrough.

The pipeline processes items sequentially in `uring_main_loop`. The header write (line 676) puts at most 1 partial buffer worth of data in flight (OSMHeader is typically <100 bytes, well under 256 KB, so the buffer is not submitted at all -- it is still being filled). When the first CopyRange arrives, `handle_copy_range_uring` calls `submit_current()` (line 550) which submits the partial header buffer, then `register_input_fd` calls `drain()` which waits for that 1 WriteFixed CQE.

**The stall is one 256 KB write completion, typically <1 ms.** This is not a performance concern.

**For sort:** sort also uses `write_raw_copy` (`sort.rs` line 327). Sort uses `to_path_pipelined` (not io_uring), so `register_input_fd` is not called. **No issue.**

### C. Error recovery for orphaned CQEs

When `reap_cqes` encounters an error (e.g., short write at line 495), it returns Err immediately. Any remaining CQEs in the completion queue are not reaped. When the writer thread exits, the `IoUring` is dropped, which unregisters buffers and closes the ring. The kernel cancels any pending SQEs.

**Is there a risk?** The kernel handles cleanup when the ring fd is closed. Unreaped CQEs are discarded. In-flight writes may or may not complete, but since we are returning an error, the output file is considered invalid anyway. **No practical concern.**

One edge case: if the writer thread panics (rather than returning Err), the `IoUring`'s Drop still runs. The `uring_writer_thread` wrapper (lines 576-588) does not panic -- it catches the Result and returns it. The only panic risk is if the Receiver iterator panics, which cannot happen in safe Rust. **Robust.**

### D. DirectWriter vs io_uring comparison

**DirectWriter** (`direct_writer.rs`, 230 lines) is a synchronous O_DIRECT writer with:
- Single 256 KB page-aligned buffer.
- Synchronous `libc::write` (lines 140-151).
- Same flush_final pattern: pad + write + ftruncate (lines 158-174).
- Implements `std::io::Write` trait.

**UringState** (`uring_writer.rs`, ~530 lines of logic) adds:
- 64 concurrent buffer slots (vs 1).
- Asynchronous submission + CQE reaping.
- Linked SQE chains for CopyRange.
- fd registration, init error propagation.

**When does async beat sync?**

DirectWriter makes one synchronous write per 256 KB buffer. Each write blocks until the kernel returns. With NVMe drives, a 256 KB write takes ~100 us. At 737 MB/s throughput (North America uring+none), that is one write every 347 us. If each write blocks for 100 us, the pipeline stalls for 100/347 = 29% of the time.

With io_uring, writes are asynchronous. While one buffer is being written, the next is being filled. With 64 buffers, the pipeline never stalls on I/O unless the storage device falls behind. This explains the North America results: uring+none is 30% faster than buffered+none (36,394 ms vs 25,500 ms).

**At small scale (Denmark 465 MB):** The file fits in page cache. Buffered writes complete almost instantly (memcpy to page cache). O_DIRECT bypasses this fast path, making uring actually slower. This explains the 4-5% regression for uring on Denmark.

**Crossover point:** Based on benchmark data, io_uring becomes beneficial somewhere between Japan (2.3 GB, 3.5% slower) and North America (18.8 GB, 30% faster). The crossover is where the working set exceeds available RAM for page cache.

### E. DirectReader for planet-scale reads

`DirectReader` (`direct_reader.rs`, 224 lines) uses a single 256 KB page-aligned buffer with synchronous `libc::read`. It implements `std::io::Read` so it slots into `BlobReader`/`ElementReader` transparently.

**Is O_DIRECT beneficial for sequential reads at planet scale?**

For planet merge (80 GB input), the input is read sequentially and never re-read. Without O_DIRECT, the 80 GB read pollutes the entire page cache, evicting useful data from other processes. With O_DIRECT, reads go directly to the application buffer (`DecompressPool` / `BlobReader`'s internal buffer), leaving the page cache undisturbed.

However, O_DIRECT removes kernel readahead. The buffered path uses `posix_fadvise(POSIX_FADV_SEQUENTIAL)` (`file_reader.rs` lines 36-38) to hint for aggressive readahead. `DirectReader` does not have this optimization. For sequential reads from spinning disks, this could reduce throughput. For NVMe, the difference is minimal since NVMe has low latency and the 256 KB read granularity provides adequate prefetching.

**Current usage:** `DirectReader` is used when `--direct-io` is passed to commands. In merge (line 1015), the reader thread opens the base PBF with `FileReader::open` which selects buffered or direct based on the flag. The pipelined reader thread with 64-frame read-ahead (`BATCH_SIZE` at line 1011) provides its own prefetching effect.

**Recommendation:** `DirectReader` is appropriate for planet scale. The lack of readahead hint is compensated by the reader thread's prefetching. No change needed.

## 10. Cross-Box Interactions

### Box 5 (writer pipeline) -> Box 7

The writer pipeline dispatches `PipelineItem` objects to the writer thread. For io_uring, the `uring_main_loop` (lines 689-729) replaces `writer_thread` (`writer.rs` lines 564-608). The reorder logic is identical -- both use `VecDeque` with the same `WRITE_AHEAD` capacity.

**Key difference:** The io_uring path handles `PipelinePayload::CopyRange` via `handle_copy_range_uring` (line 719), while the buffered `writer_thread` handles it via `copy_range` syscall (lines 591-601). The io_uring path submits the current accumulation buffer first (line 550), then processes the CopyRange, maintaining byte ordering.

**Potential issue:** `WRITE_AHEAD = 32` is the channel capacity for pipeline items. Each item can generate 1-6 buffer submissions. In theory, 32 items x 6 buffers = 192 submissions, exceeding the 64-buffer pool. But the `uring_main_loop` processes items sequentially (one CopyRange at a time), and each CopyRange blocks in `acquire_buffer()` when the pool is exhausted, providing natural backpressure. **No issue.**

### Box 1 (read pipeline) -> Box 7

The read pipeline uses `FileReader` (buffered or `DirectReader`) for all I/O. When O_DIRECT is enabled for reads, the same flag controls whether the write side uses `DirectWriter` or io_uring.

For merge, the read side opens a second file handle for copy_file_range/CopyRange (`merge.rs` line 1038). This handle is always buffered (`FileReader::buffered`), even when the main reader uses O_DIRECT. This is correct -- `copy_file_range` uses explicit offsets and does not interfere with the reader's buffer state.

### Box 8 (commands) -> Box 7

Merge and sort are the primary consumers of the O_DIRECT/io_uring path:
- **Merge:** Full integration -- io_uring, O_DIRECT sync, and buffered modes all available. CopyRange passthrough is the unique feature.
- **Sort:** Uses `write_raw_copy` for passthrough but only with buffered output (`to_path_pipelined`). Sort does not offer an io_uring mode.
- **Cat:** Uses `FileReader::open` with direct_io flag for reads, but writes are always pipelined buffered.
- **Other commands:** Use `direct_io` for reads only (`ElementReader::open`).

**Observation:** Only merge exposes all three write modes. Sort and cat could benefit from io_uring at planet scale but currently only use buffered output. This is a reasonable prioritization -- merge is the highest-throughput write workload.

## 11. Recommended Actions (Prioritized)

### Priority 1: No action needed (well-designed)
The overall architecture is sound. The three I/O modes serve distinct scale points correctly. Buffer management in linked SQE chains is correct with no double-free or leak risks. Write amplification is negligible. The sqpoll code path works but adds no measurable benefit.

### Priority 2: Consider removing sqpoll (low effort, reduces maintenance)
Benchmark data across 3 scales (Denmark, Japan, North America) shows sqpoll provides <1% improvement. The quantified syscall overhead (0.29%) explains why. Removing the sqpoll code path (conditional in `uring_init_and_run` lines 612-615, `push_sqe_pair`'s `squeue_wait`, the sqpoll parameter throughout) would eliminate ~30 lines and remove a kernel 5.12+ dependency. The sqpoll SQ overflow fix was a real bug -- removing sqpoll removes the entire class of bugs.

**Counter-argument:** sqpoll might matter at planet scale (80 GB) where throughput is higher. Given that North America (18.8 GB) shows no benefit, this is unlikely. But it could be verified with a planet-scale benchmark before removal.

### Priority 3: Add opcode probe at init (low effort, better diagnostics)
Call `register_probe()` after ring creation to verify WriteFixed and ReadFixed support. Emit a specific error message if unsupported instead of letting the first SQE fail with a generic error. This costs ~5 lines and improves the user experience on older kernels.

### Priority 4: Document the fd registration stall window (documentation only)
The `drain()` before `register_files_update` is correct but undocumented in terms of its performance impact. Add a comment noting that the stall is bounded by the header write (1 partial buffer, <1 ms) for merge. This helps future maintainers understand why the drain is not a performance concern.

### Priority 5: Extend io_uring to sort (medium effort, improves planet-scale sort)
Sort currently uses buffered output only. For planet-scale sorts (80 GB), io_uring would provide the same ~25-30% improvement seen in merge. This requires plumbing the `--io-uring` flag through the sort command and adjusting the `write_raw_copy` path. Medium effort but would benefit the nidhogg planet refresh workflow.

### Not recommended
- **Hugepages for buffer pool:** Undetectable benefit due to sequential access pattern.
- **Rayon threads writing to registered buffers:** Significant complexity for ~20 ms savings on a 35s operation.
- **Scatter-gather (writev):** Incompatible with registered buffers per Linux io_uring design.
- **Runtime self-check diagnostics (reviewer suggestion):** The init error propagation already provides this. RLIMIT_MEMLOCK failures give actionable messages. Adding more checks would be defensive programming with no user benefit.
