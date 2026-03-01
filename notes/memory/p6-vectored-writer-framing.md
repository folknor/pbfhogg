# P6: Writer Framing Without Full-Frame Concatenation

## Problem Statement

At planet scale (~600K output blobs, ~75 GB output), the pipelined writer path
allocates a fresh `Vec<u8>` (~32-64 KB) for every blob via `frame_blob_into`.
This Vec concatenates three segments (4-byte length prefix, BlobHeader bytes,
Blob body bytes) that already exist in separate scratch buffers, only to be
sent through the channel and written by the writer thread. Total allocator
churn: ~600K * ~50 KB = ~30 GB (compressed blobs) or ~600K * ~1.4 MB = ~840 GB
(`Compression::None`). The goal is to eliminate this concatenation allocation
while maintaining or improving throughput.

---

## Current State

### `frame_blob_into` (writer.rs lines 733-757)

The hot-path function called by every rayon compression task:

```rust
fn frame_blob_into(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    scratch: &mut FrameScratch,
) -> io::Result<Vec<u8>> {
    encode_blob_body(uncompressed, compression, scratch)?;
    // ... encode BlobHeader into scratch.header_buf ...
    let total_len = 4 + scratch.header_buf.len() + scratch.blob_buf.len();
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&header_len.to_be_bytes());  // 4 bytes
    out.extend_from_slice(&scratch.header_buf);          // ~40-100 bytes
    out.extend_from_slice(&scratch.blob_buf);            // ~32-64 KB (compressed)
    Ok(out)
}
```

**What it allocates:** One `Vec<u8>` per blob (~50 KB typical, ~1.4 MB for
`Compression::None`). The Vec is sent through the pipeline channel as
`PipelinePayload::Bytes(Ok(Vec<u8>))`.

**Why it concatenates:** The pipeline channel sends a single `Vec<u8>` per blob.
The writer thread receives it and calls `writer.write_all(&result?)`. The
channel type is `PipelinePayload::Bytes(io::Result<Vec<u8>>)` -- it expects
one contiguous buffer.

### `FrameScratch` (writer.rs lines 93-104)

Reusable scratch buffers stored per-thread (`PIPELINE_SCRATCH` thread-local for
pipelined path, `self.scratch` for sync path):

```rust
struct FrameScratch {
    blob_buf: Vec<u8>,      // Blob protobuf body (raw/compressed)
    header_buf: Vec<u8>,    // BlobHeader protobuf
    compress_buf: Vec<u8>,  // Intermediate compression output
    zlib_compressor: Option<Compress>,
    zstd_compressor: Option<zstd::bulk::Compressor<'static>>,
}
```

After warmup, `blob_buf` and `header_buf` have sufficient capacity and are
reused via `.clear()` (no allocation). The `compress_buf` is also reused for
zlib; zstd's `compress()` returns a new Vec (the `zstd` crate API limitation).

### `encode_blob_body` (writer.rs lines 764-798)

Writes the Blob protobuf into `scratch.blob_buf`. For zlib, the compressed
bytes are first written to `scratch.compress_buf`, then the Blob protobuf
fields (`raw_size` + `zlib_data`) are assembled in `scratch.blob_buf`
referencing `compress_buf`. After `encode_blob_body` returns, `blob_buf`
contains the complete Blob protobuf body.

### `reframe_raw_with_index` (writer.rs lines 861-881)

Used by merge and sort to add indexdata/tagdata to passthrough blobs. Allocates
a fresh output Vec just like `frame_blob_into`:

```rust
pub(crate) fn reframe_raw_with_index(
    blob_bytes: &[u8],
    indexdata: &[u8],
    tagdata: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    // ... encode BlobHeader ...
    let total_len = 4 + header_buf.len() + blob_bytes.len();
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(&header_buf);
    out.extend_from_slice(blob_bytes);
    Ok(out)
}
```

This is called per-passthrough-blob when the input lacks indexdata. At planet
scale with an indexed input, this path is rare (merge only hits it for
non-indexed blobs). But `reframe_raw_with_index` also copies the entire Blob
body (~55 KB compressed) into the output Vec.

---

## The Three Write Paths

### 1. Sync Path (`write_framed_blob`, lines 508-545)

**Already optimal.** Writes directly from scratch buffers:

```rust
writer.write_all(&header_len.to_be_bytes())?;  // 4 bytes
writer.write_all(&self.scratch.header_buf)?;    // ~40-100 bytes
writer.write_all(&self.scratch.blob_buf)?;      // ~32-64 KB
```

No concatenation Vec. Three `write_all` calls go through `BufWriter` (256 KB
buffer), so the three calls are assembled in the BufWriter's buffer with no
extra syscalls. This is the ideal pattern.

### 2. Pipelined Path (`frame_blob_into` + channel + `writer_thread`)

**The problem path.** Each rayon task calls `frame_blob_into` which allocates
a fresh `Vec<u8>`, concatenates the three segments, sends it through
`sync_channel(32)`. The writer thread receives items, reorders them via a
`VecDeque`, and calls `writer.write_all(&result?)`.

Pipeline flow:
```
Main thread                    Rayon pool                      Writer thread
write_primitive_block  --->  frame_blob_into (compress)  --->  reorder + write_all
                             returns Vec<u8>                    via BufWriter<File>
```

The `PipelinePayload` enum:
```rust
enum PipelinePayload {
    Bytes(io::Result<Vec<u8>>),    // <-- the concatenated frame
    CopyRange { in_fd, offset, len },  // kernel-space copy (O_DIRECT only)
}
```

The `writer_thread` (lines 616-660) receives `PipelineItem { seq, data }`,
reorders by sequence number, and writes:
```rust
PipelinePayload::Bytes(result) => writer.write_all(&result?)?
```

The writer thread uses `FileWriter::Buffered(BufWriter<File>)` by default, or
`FileWriter::Direct(DirectWriter)` when O_DIRECT is enabled.

### 3. io_uring Path (`uring_writer_thread`)

The io_uring writer receives the same `PipelinePayload::Bytes(Vec<u8>)` from
the same channel. It copies the Vec contents into registered page-aligned
buffers (256 KB each) via `UringState::write(&data)`:

```rust
PipelinePayload::Bytes(result) => {
    state.write(&result?)?;
}
```

`UringState::write` copies bytes into the current registered buffer. When the
buffer fills, it submits a `WriteFixed` SQE. So the data flows:

```
Rayon task  --Vec-->  channel  --Vec-->  uring_main_loop
                                            |
                                    copy into registered buffer
                                            |
                                    submit WriteFixed SQE
```

The io_uring path **always copies** from the received Vec into registered
buffers. It cannot use the Vec directly because registered buffers must be
page-aligned and pre-registered with the kernel. This is an inherent
architectural constraint -- `io_uring` `WriteFixed` only works with
pre-registered buffer addresses.

---

## Design: Vectored Framing for the Pipelined Path

### Core Insight

The three segments of a framed blob are:
1. **Length prefix**: 4 bytes (`header_len.to_be_bytes()`)
2. **BlobHeader**: ~40-100 bytes (type string, indexdata, datasize, tagdata)
3. **Blob body**: ~32-64 KB compressed (the dominant segment)

The Blob body is the largest segment and already exists in
`scratch.compress_buf` (for zlib/zstd) or can be referenced from the
uncompressed input (for `Compression::None`). The BlobHeader is tiny. The
concatenation exists solely to package these into a single `Vec<u8>` for the
channel.

### Approach A: Multi-Segment Payload (Rejected)

**Idea:** Change `PipelinePayload::Bytes` to hold a small header buffer plus a
separate body buffer:

```rust
enum PipelinePayload {
    Segments {
        header: [u8; 128],  // length prefix + BlobHeader (always < 128 bytes)
        header_len: usize,
        body: Vec<u8>,      // Blob protobuf body
    },
    // ...
}
```

The writer thread would then do:
```rust
writer.write_all(&header[..header_len])?;
writer.write_all(&body)?;
```

**Problem:** This doesn't eliminate the allocation. The Blob body (`body`) is
still a `Vec<u8>` that must be owned. Currently `scratch.blob_buf` contains the
Blob protobuf, but it's a reusable scratch buffer -- we can't send it through
the channel without taking ownership. We'd need to `std::mem::take` the
scratch buffer and replace it with a new one, which trades one allocation for
another.

For zlib/zstd, the Blob protobuf body contains both the `raw_size` varint field
AND the compressed data (`zlib_data` / `zstd_data` field). These are assembled
into `scratch.blob_buf` by `encode_blob_body`. The compressed data itself lives
in `scratch.compress_buf`, but the Blob protobuf wraps it with additional fields.

This approach saves the ~4+100 byte header copy but not the ~50 KB body
allocation. The savings are negligible.

### Approach B: `write_vectored` / `IoSlice` in Writer Thread (Rejected for BufWriter)

**Idea:** Instead of concatenating in the rayon task, send segments through
the channel and use `write_vectored` in the writer thread.

**`BufWriter` limitation:** Rust's `BufWriter::write_vectored` exists but has
a critical limitation: it calls the inner writer's `write_vectored` only when
the total data exceeds the buffer capacity. For data smaller than the buffer
(our ~50 KB blobs vs 256 KB buffer), it falls back to copying each slice into
the internal buffer sequentially -- exactly what `write_all` already does. No
benefit.

**`File::write_vectored`:** Calls `writev(2)` directly. But our `FileWriter`
wraps `BufWriter<File>`, not `File` directly. Bypassing BufWriter would mean
one `writev` syscall per blob (~600K syscalls vs ~600K/5 with BufWriter
batching). This would be worse.

**`DirectWriter`:** Does not implement `write_vectored`. Its page-alignment
buffering operates on `&[u8]` slices. Adding vectored I/O support would require
significant refactoring.

**Verdict:** `write_vectored` / `IoSlice` provides no benefit for our write
paths. The BufWriter already coalesces small writes efficiently. The problem
is not the write call pattern -- it's the allocation of the concatenation Vec
on the sending side.

### Approach C: Scratch Buffer Reuse via Swap (Recommended)

**Idea:** Instead of allocating a fresh `Vec<u8>` per blob in `frame_blob_into`,
maintain a **pool of reusable output buffers** that circulate between the rayon
threads and the writer thread.

**Mechanism:** Use a bounded `crossbeam_channel` or `mpsc` reverse channel to
return consumed `Vec<u8>` buffers from the writer thread back to the rayon
tasks.

```
Rayon task                         Writer thread
   |                                   |
   |--- fill Vec from pool ------>     |
   |    (or allocate if pool empty)    |
   |--- send via forward channel --->  |
   |                                   |--- write_all(&vec) --->
   |                                   |--- return Vec to pool --->
   |<-- receive recycled Vec ------    |
```

**Implementation:**
- Add a `recycle_tx: SyncSender<Vec<u8>>` to the writer thread.
- Add a `recycle_rx: Receiver<Vec<u8>>` accessible to rayon tasks (via
  thread-local or shared state).
- After `writer_thread` calls `write_all(&data)`, it sends the consumed Vec
  back via `recycle_tx`.
- Before `frame_blob_into` allocates, it tries `recycle_rx.try_recv()` to get
  a recycled buffer. If empty, allocates a fresh one.
- The recycled Vec retains its capacity, so after warmup, no allocations occur.

**Complications:**
- Thread-local `PIPELINE_SCRATCH` would need access to the recycle channel.
  This means either (a) passing the receiver into the rayon closure, or (b)
  using a shared `Arc<Mutex<Receiver<Vec<u8>>>>`.
- The `PipelinePayload::Bytes(io::Result<Vec<u8>>)` type signature remains
  unchanged -- the Vec is just reused instead of freshly allocated.
- Error paths must handle the case where the recycle channel is disconnected.
- The number of Vecs in circulation is bounded by `WRITE_AHEAD` (32), so
  memory is bounded.

**Drawback:** Adds cross-thread synchronization (channel send/recv per blob).
At ~50 KB per blob, the allocator (jemalloc/mimalloc) already handles this
efficiently via thread-local caches. The actual throughput benefit may be
marginal.

### Approach D: Thread-Local Output Buffer Reuse (Recommended -- Simplest)

**Idea:** Instead of returning a fresh `Vec<u8>` from `frame_blob_into`, use
a thread-local output buffer that is swapped out via `std::mem::replace`.

```rust
thread_local! {
    static PIPELINE_SCRATCH: RefCell<FrameScratch> = ...;
    static OUTPUT_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}
```

In `frame_blob_into`, instead of:
```rust
let mut out = Vec::with_capacity(total_len);
// ... fill out ...
Ok(out)
```

Do:
```rust
OUTPUT_BUF.with_borrow_mut(|out| {
    out.clear();
    out.reserve(total_len);
    // ... fill out ...
    Ok(std::mem::replace(out, Vec::new()))
})
```

**Wait -- this doesn't help.** `std::mem::replace(out, Vec::new())` gives
ownership of the filled buffer away and replaces it with an empty Vec. The next
call allocates again because the capacity was taken.

The fundamental problem is that the channel needs to **own** the buffer, and
once ownership is transferred, the thread-local loses its capacity.

### Approach E: Eliminate the Output Vec by Writing Segments Directly (Recommended)

**Insight:** The sync path already writes three segments directly to the writer.
The pipelined path concatenates them only because the channel type demands a
single `Vec<u8>`. If we change the channel payload to carry segments instead of
a concatenated buffer, the writer thread can write them directly.

**New payload type:**

```rust
/// A framed blob as separate segments, avoiding concatenation allocation.
struct FramedSegments {
    /// BlobHeader protobuf bytes (length prefix prepended).
    /// Small: 4 bytes length prefix + ~40-100 bytes header = always < 200 bytes.
    header: Vec<u8>,
    /// Blob protobuf body. For compressed blobs, this is the FrameScratch.blob_buf
    /// taken via std::mem::replace. For Compression::None, this wraps the uncompressed
    /// data with the Blob protobuf framing.
    body: Vec<u8>,
}
```

**How it eliminates the concatenation:**

In `frame_blob_into`, instead of allocating a new `out` Vec:

```rust
fn frame_segments_into(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    scratch: &mut FrameScratch,
) -> io::Result<FramedSegments> {
    encode_blob_body(uncompressed, compression, scratch)?;

    let datasize = i32::try_from(scratch.blob_buf.len())?;
    encode_blob_header_into(blob_type, datasize, indexdata, tagdata, &mut scratch.header_buf);

    let header_len = u32::try_from(scratch.header_buf.len())?;

    // Take header: prepend 4-byte length prefix, then header bytes.
    // This is ~104 bytes total -- tiny allocation.
    let mut header = Vec::with_capacity(4 + scratch.header_buf.len());
    header.extend_from_slice(&header_len.to_be_bytes());
    header.extend_from_slice(&scratch.header_buf);

    // Take body: move the blob_buf out, replace with an empty Vec.
    // After warmup, the replacement Vec will be given capacity on the
    // next encode_blob_body call via blob_buf.reserve().
    let body = std::mem::replace(&mut scratch.blob_buf, Vec::new());

    Ok(FramedSegments { header, body })
}
```

**Wait -- this still allocates.** `std::mem::replace(&mut scratch.blob_buf,
Vec::new())` takes the blob_buf's capacity away. Next call, `encode_blob_body`
calls `scratch.blob_buf.clear()` (no-op on empty Vec) then writes to it,
triggering a new allocation.

**The blob_buf allocation is unavoidable** in the pipelined path. The rayon
thread must produce an owned buffer to send through the channel. The only
question is whether we do one allocation (current: `out` Vec) or one allocation
(new: `blob_buf` re-grow). It's a wash.

### Approach F: Recycle via the Forward Channel Return Path (Recommended)

**The real insight:** The writer thread receives `Vec<u8>` buffers and drops
them after `write_all`. If we send the `Vec<u8>` back to the rayon pool, the
next rayon task can reuse it. This is Approach C, which I initially considered
complicated. Let me reconsider its feasibility.

**Simplified design using a lock-free stack:**

```rust
use std::sync::Arc;
use crossbeam_deque::{Injector, Steal};

struct BufferPool {
    stack: Injector<Vec<u8>>,
}

impl BufferPool {
    fn acquire(&self) -> Vec<u8> {
        loop {
            match self.stack.steal() {
                Steal::Success(v) => return v,
                _ => return Vec::new(),  // No recycled buffer available
            }
        }
    }

    fn release(&self, mut v: Vec<u8>) {
        v.clear();
        self.stack.push(v);
    }
}
```

The `BufferPool` is shared via `Arc` between the rayon tasks and writer thread.
The writer thread calls `pool.release(vec)` after writing. Rayon tasks call
`pool.acquire()` before framing.

**But:** This adds a dependency on `crossbeam-deque`. Alternatively, use
`std::sync::Mutex<Vec<Vec<u8>>>` (a Mutex-guarded free-list). The contention
is low: the writer thread is the only producer (one release at a time), and
rayon tasks are the consumers (one acquire at a time, but potentially
concurrent). With `WRITE_AHEAD=32`, there are at most 32 buffers in
circulation.

**Even simpler:** Use an `mpsc::sync_channel` as a return path. The writer
thread sends consumed buffers back. Rayon tasks try_recv to get a recycled
buffer.

---

## Revised Design: Buffer Recycling Pool

After analyzing all approaches, the most practical design is a buffer recycling
pool. Here is the detailed design.

### Channel Payload: Unchanged

```rust
pub(crate) enum PipelinePayload {
    Bytes(io::Result<Vec<u8>>),
    #[cfg(feature = "linux-direct-io")]
    CopyRange { in_fd, offset, len },
}
```

No change to the payload type. The `Vec<u8>` is still the concatenated frame.
What changes is where the Vec comes from and where it goes after consumption.

### Buffer Pool: `Arc<Mutex<Vec<Vec<u8>>>>`

A simple shared free-list. No external dependency.

```rust
use std::sync::{Arc, Mutex};

type BufferPool = Arc<Mutex<Vec<Vec<u8>>>>;

fn pool_acquire(pool: &BufferPool) -> Vec<u8> {
    pool.lock()
        .ok()
        .and_then(|mut stack| stack.pop())
        .unwrap_or_default()
}

fn pool_release(pool: &BufferPool, mut buf: Vec<u8>) {
    buf.clear();
    if let Ok(mut stack) = pool.lock() {
        // Cap the pool to avoid unbounded growth
        if stack.len() < 64 {
            stack.push(buf);
        }
    }
}
```

### Modified `frame_blob_into`

```rust
fn frame_blob_into(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    scratch: &mut FrameScratch,
    out: &mut Vec<u8>,            // <-- reusable output buffer
) -> io::Result<()> {
    encode_blob_body(uncompressed, compression, scratch)?;
    // ... encode header ...
    let total_len = 4 + scratch.header_buf.len() + scratch.blob_buf.len();
    out.clear();
    out.reserve(total_len);
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(&scratch.header_buf);
    out.extend_from_slice(&scratch.blob_buf);
    Ok(())
}
```

The caller acquires `out` from the pool, passes it in, then sends it through
the channel. The writer thread releases it back to the pool after writing.

### Modified Pipeline Flow

```rust
// In write_primitive_block (pipelined path):
rayon::spawn(move || {
    // Acquire recycled buffer from pool
    let mut out = pool_acquire(&pool);

    let result = PIPELINE_SCRATCH.with_borrow_mut(|scratch| {
        frame_blob_into(
            "OSMData", &uncompressed, &compression,
            indexdata, tagdata, scratch, &mut out,
        )
    });

    let payload = match result {
        Ok(()) => PipelinePayload::Bytes(Ok(out)),
        Err(e) => PipelinePayload::Bytes(Err(e)),
    };
    drop(tx.send(PipelineItem { seq, data: payload }));
});
```

### Modified Writer Thread

```rust
fn writer_thread(
    rx: Receiver<PipelineItem>,
    mut writer: FileWriter,
    pool: BufferPool,
) -> io::Result<()> {
    // ... reorder logic unchanged ...
    match payload {
        PipelinePayload::Bytes(result) => {
            let buf = result?;
            writer.write_all(&buf)?;
            pool_release(&pool, buf);  // <-- recycle
        }
        // ...
    }
}
```

### Modified io_uring Writer

```rust
fn uring_main_loop(
    rx: &Receiver<PipelineItem>,
    state: &mut UringState,
    pool: &BufferPool,
) -> io::Result<()> {
    // ... reorder logic unchanged ...
    match payload {
        PipelinePayload::Bytes(result) => {
            let buf = result?;
            state.write(&buf)?;
            pool_release(pool, buf);  // <-- recycle
        }
        // ...
    }
}
```

---

## Interaction Analysis

### Interaction with Compression

No change to compression logic. `encode_blob_body` still writes into
`scratch.blob_buf` and `scratch.compress_buf`. The only change is that the
final concatenation writes into a recycled buffer instead of a fresh allocation.
The compression output sizes are unchanged.

### Interaction with Passthrough (raw blobs)

Passthrough blobs (`write_raw`, `write_raw_owned`) bypass `frame_blob_into`
entirely. They send pre-framed bytes directly. **No change needed** for
passthrough.

For `reframe_raw_with_index`, the function currently allocates a fresh Vec.
It could also accept a reusable buffer parameter. However, in merge,
`reframe_raw_with_index` results go into the `passthrough_buf` coalescing
buffer via `extend_from_slice`, so the reframed Vec is immediately consumed.
Optimization here is secondary.

### Interaction with `write_primitive_block_owned`

This variant takes ownership of `block_bytes: Vec<u8>`. The owned bytes are
used as the uncompressed input. The framing still happens via `frame_blob_into`
in the rayon closure. The buffer pool integration applies identically.

### Interaction with Sync Path

The sync path (`write_framed_blob`) already writes directly from scratch
buffers. **No change needed.**

### Impact on io_uring Path

The io_uring path copies received bytes into registered buffers regardless.
Buffer recycling means the received `Vec<u8>` is returned to the pool after
copying, rather than being dropped. This is strictly better -- the same buffer
is reused by the next rayon task instead of allocating a new one.

The io_uring `WriteFixed` registered buffer architecture is orthogonal to
this change. Registered buffers are page-aligned, pre-allocated 256 KB chunks
managed by `AlignedBufferPool`. The recycled Vecs are standard heap buffers
used only to shuttle data from rayon to the uring thread.

### Impact on `CopyRange` Passthrough

`CopyRange` payloads carry no `Vec<u8>` -- they specify fd/offset/len for
kernel-space copy. No interaction with buffer recycling.

---

## Memory Savings Estimate

### Current State (Planet Scale, ~600K output blobs)

**Compressed blobs (zlib:6):**
- `frame_blob_into` output Vec: ~50 KB per blob
- 600K blobs * 50 KB = ~30 GB cumulative allocation
- With jemalloc thread-local caches, most allocations hit the tcache (free-list
  reuse within the same thread). But rayon tasks run on different threads, and
  the writer thread frees the Vec on yet another thread, defeating tcache reuse.

**Uncompressed blobs (`Compression::None`):**
- `frame_blob_into` output Vec: ~1.4 MB per blob (raw PrimitiveBlock)
- 600K blobs * 1.4 MB = ~840 GB cumulative allocation
- These are too large for tcache (jemalloc's large size class, >256 KB) and go
  through the central arena, causing contention.

### After Buffer Recycling

- After warmup (first ~32 blobs to fill the pipeline), all subsequent blobs
  reuse recycled buffers. Zero new allocations for the output Vec.
- The pool holds at most `WRITE_AHEAD` (32) buffers * ~50 KB = ~1.6 MB resident.
  For `Compression::None`: 32 * ~1.4 MB = ~44.8 MB resident.
- Total allocation savings: ~30 GB (zlib) or ~840 GB (none) at planet scale.

### Merge-Specific Savings

Merge has ~92% passthrough (planet daily diff). Only ~8% of blobs go through
`frame_blob_into` (the rewritten blobs). At planet scale:
- 600K * 8% = ~48K blobs through `frame_blob_into`
- 48K * 50 KB = ~2.4 GB allocation savings (zlib)

For merge, the savings are modest because most blobs are passthrough.

### Cat / Sort / Tags-Filter / Extract

These commands write every blob through `frame_blob_into` (no passthrough
except sort with pre-sorted input). At planet scale:
- cat: ~600K blobs * 50 KB = ~30 GB savings
- sort: similar to cat
- extract: depends on extraction area, but all kept blobs go through framing

---

## Implementation Steps

### Step 1: Add `BufferPool` type and pool functions

Add to `src/write/writer.rs`:

```rust
type BufferPool = Arc<Mutex<Vec<Vec<u8>>>>;

fn pool_acquire(pool: &BufferPool) -> Vec<u8> {
    pool.lock()
        .ok()
        .and_then(|mut stack| stack.pop())
        .unwrap_or_default()
}

fn pool_release(pool: &BufferPool, mut buf: Vec<u8>) {
    buf.clear();
    if let Ok(mut stack) = pool.lock() {
        if stack.len() < 64 {
            stack.push(buf);
        }
    }
}
```

### Step 2: Change `frame_blob_into` signature

Change from `-> io::Result<Vec<u8>>` to accepting `&mut Vec<u8>`:

```rust
fn frame_blob_into(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    scratch: &mut FrameScratch,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    // ... same logic, writes into `out` instead of allocating
}
```

Update `frame_blob` (the non-scratch wrapper) to allocate a fresh buffer
internally (only used for one-off header framing, not hot path).

### Step 3: Thread the `BufferPool` through `PbfWriter`

Add `pool: Option<BufferPool>` to `PbfWriter`. Initialize it in
`start_pipeline`, `to_path_pipelined_uring`, etc. Pass a clone to the writer
thread and to each rayon closure.

### Step 4: Update `write_primitive_block` pipelined path

In the rayon closure:
1. `pool_acquire(&pool)` to get a recycled buffer.
2. Call `frame_blob_into` with the buffer.
3. Send the buffer through the channel.

### Step 5: Update `writer_thread` to recycle

After `write_all`, call `pool_release(&pool, buf)`.

### Step 6: Update `uring_main_loop` to recycle

After `state.write(&buf)`, call `pool_release(&pool, buf)`.

### Step 7: Update `write_primitive_block_owned` similarly

Same pattern as Step 4.

### Step 8: Tests

- Existing roundtrip tests (`tests/roundtrip.rs`, `tests/roundtrip_real.rs`)
  validate correctness.
- Run `brokkr check` to verify clippy + tests pass.
- Run `brokkr check -- --ignored` for the full Denmark roundtrip.
- Run `brokkr bench write` to measure throughput impact.

---

## Risk Assessment

### Mutex Contention

The `Mutex<Vec<Vec<u8>>>` is accessed by:
- N rayon threads (acquire, non-blocking try pattern)
- 1 writer thread (release after each write)

At 600K blobs over ~40 seconds (planet merge), that's ~15K ops/sec on the pool.
Each operation is O(1) -- push or pop on a Vec. The lock hold time is ~10ns.
Contention is negligible.

**Mitigation:** If contention becomes measurable (unlikely), replace with a
lock-free stack (e.g., `crossbeam-deque::Injector`).

### Partial Writes with Vectored I/O

Not applicable -- we chose buffer recycling (Approach F) over vectored I/O
(Approach B). No `write_vectored` is used.

### Platform Compatibility

No platform-specific changes. `Arc<Mutex<Vec<_>>>` is fully portable.

### Throughput Impact

**Positive or neutral.**
- The Mutex overhead (~10ns per blob) is dwarfed by compression time (~1ms per
  blob for zlib:6).
- Eliminating allocator pressure reduces the chance of mmap/munmap syscalls
  for large allocations (`Compression::None` path, >256 KB Vecs).
- At planet scale with many rayon threads, reduced allocator contention on the
  central arena may provide a small throughput improvement.

**Risk:** For `Compression::None` where blobs are ~1.4 MB, the recycled buffers
retain ~1.4 MB capacity. With 32 buffers in the pool + 32 in-flight, peak
resident for the pool is ~90 MB. This is acceptable for planet-scale (64 GB
RAM target).

### Error Handling

If the writer thread panics or the channel disconnects, `pool_acquire` returns
a fresh `Vec::new()` (fallback). The pool lock uses `.ok()` to gracefully
handle poisoning. No new error paths are introduced.

### API Compatibility

No public API changes. `PbfWriter::write_primitive_block`,
`write_primitive_block_owned`, `write_raw`, `write_raw_owned` signatures are
unchanged. The buffer pool is an internal implementation detail.

### Complexity

The change adds:
- ~20 lines for `BufferPool`, `pool_acquire`, `pool_release`
- ~5 lines in `PbfWriter` struct for the pool field
- ~10 lines per pipeline constructor (threading the pool through)
- ~5 lines each in `write_primitive_block` and `write_primitive_block_owned`
- ~3 lines each in `writer_thread` and `uring_main_loop` for recycling

Total: ~60 lines of new code. Low risk, well-isolated.

---

## Alternative Considered: Direct Segment Writes in Writer Thread

Instead of buffer recycling, change the channel payload to carry segments:

```rust
enum PipelinePayload {
    Segments {
        header: SmallVec<[u8; 128]>,
        body: Vec<u8>,
    },
    Raw(Vec<u8>),
    CopyRange { ... },
}
```

The writer thread writes header and body as two `write_all` calls.

**Why this is worse than buffer recycling:**
1. The `body: Vec<u8>` still needs to be owned. It's either (a) taken from
   `scratch.blob_buf` via `std::mem::replace`, causing blob_buf to re-allocate
   on the next blob, or (b) a copy of blob_buf, which is the same as the
   current approach.
2. The two `write_all` calls in the writer thread are no worse than one (both
   go through BufWriter), but the complexity of a new payload variant and
   the `SmallVec` dependency are not justified.
3. No net allocation savings vs the current approach.

Buffer recycling is strictly better because it addresses the root cause (fresh
allocation per blob) without changing the payload type.

---

## Summary

| Aspect | Current | After P6 |
|---|---|---|
| Allocation per blob | `Vec::with_capacity(~50KB)` | Recycled from pool (zero after warmup) |
| Planet-scale alloc churn | ~30 GB (zlib), ~840 GB (none) | ~0 after first 32 blobs |
| Channel payload type | `Vec<u8>` (unchanged) | `Vec<u8>` (unchanged) |
| Writer thread change | None | `pool_release` after write |
| Rayon task change | `Vec::with_capacity` | `pool_acquire` before framing |
| New dependencies | None | None (std::sync only) |
| Sync path impact | None | None |
| io_uring path impact | Recycles after copy | Same |
| Code size | N/A | ~60 lines added |
| Risk | N/A | Low -- fallback to fresh alloc on pool miss |
