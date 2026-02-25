# Linux Async I/O for PBF Processing

Research notes on io_uring, O_DIRECT, and copy_file_range for planet-scale PBF
read/write/merge. Target: Linux 6.18, nidhogg weekly planet merge (~80GB PBF).

## io_uring Rust Ecosystem (Feb 2026)

### io-uring crate (low-level bindings)

- **Crate:** [io-uring](https://crates.io/crates/io-uring) v0.7.11
- **Maintainer:** tokio-rs organization (1.6K stars, 58 contributors, 17.4K dependents)
- **License:** Apache-2.0 / MIT
- **API stability:** 0.x semver, but core API has been stable across 0.6→0.7. Breaking
  changes are infrequent.

Thin type-safe wrapper over the kernel shared-memory rings:
- `IoUring<S, C>` — generic over SQE/CQE entry sizes (64/128-byte SQEs, 16/32-byte CQEs)
- `Builder` — fluent config (`setup_sqpoll()`, `setup_iopoll()`, `setup_cqsize()`)
- `Submitter` — syscall interface (submit, register buffers/files, probe)
- `opcode` module — 89 type-safe opcode builder structs

**Can be used synchronously without any async runtime.** No tokio, no futures, no
executor. The core loop is: push SQEs → `submit_and_wait(n)` → reap CQEs. This is the
intended use for a dedicated I/O thread.

```rust
use io_uring::{IoUring, opcode, types};

let mut ring = IoUring::new(256)?;

let write_e = opcode::WriteFixed::new(
    types::Fixed(0),    // registered fd index
    buf_ptr,
    buf_len as u32,
    0,                  // buf_index into registered buffers
)
.offset(file_offset)
.build()
.user_data(my_tag);

unsafe { ring.submission().push(&write_e).expect("SQ full"); }
ring.submit_and_wait(1)?;

for cqe in ring.completion() {
    let result = cqe.result();  // bytes written, or -errno
}
```

### tokio-uring

**Not production-ready.** Under tokio-rs org but low priority. Changelog stale since 2022.
Requires a single-threaded Tokio runtime (`current_thread` mode only). Awkward owned-buffer
API (`Vec<u8>` passed in and returned on completion). Adds async complexity with no benefit
for our synchronous pipeline. **Not suitable.**

### Other runtimes (all unsuitable — full async runtimes)

| Crate | Maintainer | Notes |
|-------|-----------|-------|
| **glommio** | DataDog | Thread-per-core, alpha, kernel 5.8+ |
| **monoio** | ByteDance | Thread-per-core, io_uring primary + epoll fallback, production at ByteDance for networking |
| **compio** | Community | Cross-platform (IOCP/io_uring/polling), younger |
| **nuclei** | Community | Proactor-based, runtime-agnostic, kernel 5.19+ |
| **rio** | spacejam | Had sync API but unmaintained, soundness concerns raised |
| **rustix::io_uring** | Community | Raw syscall bindings only, not a ring manager |

None of these are relevant. We want synchronous io_uring in a dedicated thread, which
`io-uring` 0.7.x provides directly.

## Relevant SQE Opcodes

| Op | Rust type | Kernel | Notes |
|----|-----------|--------|-------|
| `WRITE_FIXED` | `opcode::WriteFixed` | 5.1 | Write from registered buffer. Takes `buf_index: u16`. |
| `WRITEV` | `opcode::Writev` | 5.1 | Vectored write (`pwritev2`). Does NOT support registered buffers. |
| `WRITE` | `opcode::Write` | 5.6 | Non-vectored, single buffer, no registered buffer support. |
| `READ_FIXED` | `opcode::ReadFixed` | 5.1 | Read into registered buffer. |
| `OPENAT2` | `opcode::OpenAt2` | 5.6 | Open with `open_how` struct. Supports O_DIRECT, O_CREAT. |
| `SPLICE` | `opcode::Splice` | 5.7 | Splice between pipe and fd. Closest to copy_file_range in io_uring. |
| `FSYNC` | `opcode::Fsync` | 5.1 | fsync/fdatasync. |
| `CLOSE` | `opcode::Close` | 5.6 | Async close. |
| `FALLOCATE` | `opcode::Fallocate` | 5.6 | Pre-allocate file space. |
| `FTRUNCATE` | `opcode::Ftruncate` | 6.9 | Truncate file. |

All opcodes through kernel 6.16 are available on our 6.18 target. Use
`Submitter::register_probe()` to check opcode support at runtime if portability matters.

## copy_file_range: No io_uring Opcode

**`IORING_OP_COPY_FILE_RANGE` does not exist in the Linux kernel** (verified through 6.18).
It was never added to the io_uring opcode set. The `io-uring` crate's opcode module (89
structs) does not include it.

Alternatives for kernel-space file-to-file copy:

1. **`copy_file_range(2)` syscall directly** — synchronous, works fine, kernel-space copy.
   On btrfs/xfs with reflinks it's metadata-only (instant). Call it outside of io_uring.
2. **`IORING_OP_SPLICE` through a pipe pair** — chain `Splice(src_fd → pipe_write)` then
   `Splice(pipe_read → dst_fd)`. Adds complexity (pipe management, two SQEs per copy).
3. **`ReadFixed` + `WriteFixed` through registered buffers** — explicit read-then-write
   through our own buffer pool. Most flexible, integrates naturally with the io_uring ring.

For merge blob passthrough, option 1 (synchronous `copy_file_range(2)`) is simplest and
sufficient. Option 3 makes sense if the blob data is already in a registered buffer.

## Registered Buffers

### Registration API

```rust
// Register fixed buffers (unsafe: must remain valid until unregistered)
unsafe { ring.submitter().register_buffers(&iovecs)?; }

// Sparse table (Linux 5.13+): register empty, fill later
ring.submitter().register_buffers_sparse(64)?;
unsafe { ring.submitter().register_buffers_update(0, &iovecs[0..16], None)?; }

// Unregister
ring.submitter().unregister_buffers()?;
```

Registered buffers are pinned into kernel memory and charged against `RLIMIT_MEMLOCK`.
The kernel maps them once, avoiding per-I/O page pinning overhead.

### Allocation for registered buffers

```rust
const BUF_SIZE: usize = 128 * 1024;  // 128 KiB per buffer
const ALIGN: usize = 4096;           // page-aligned for O_DIRECT

let layout = std::alloc::Layout::from_size_align(BUF_SIZE, ALIGN).unwrap();
let ptr = unsafe { std::alloc::alloc(layout) };

let iovec = libc::iovec {
    iov_base: ptr as *mut _,
    iov_len: BUF_SIZE,
};
```

### Constraints

- **RLIMIT_MEMLOCK:** Registered buffers are charged against this. Default is often 64 KiB.
  Must raise it (`ulimit -l unlimited` or `setrlimit`).
- **Registration is slow:** Kernel maps pages. Do it once at startup.
- **WRITEV does NOT work with registered buffers.** Only `WriteFixed` (single buffer per
  SQE) supports them. Multiple buffers = multiple linked `WriteFixed` SQEs.

## Registered File Descriptors

```rust
ring.submitter().register_files(&[fd.as_raw_fd()])?;

// Use Fixed(index) instead of Fd(raw_fd) in opcodes
opcode::WriteFixed::new(types::Fixed(0), ...)
```

Avoids per-operation fd lookup in kernel. Registration is one-time cost, per-IO overhead
drops. Worth doing for the output file fd which receives thousands of writes.

## O_DIRECT

### Opening with O_DIRECT

```rust
use std::os::unix::fs::OpenOptionsExt;

let file = std::fs::OpenOptions::new()
    .write(true)
    .create(true)
    .custom_flags(libc::O_DIRECT)
    .open(path)?;
```

Or via io_uring:
```rust
let how = types::OpenHow::new()
    .flags(libc::O_WRONLY | libc::O_DIRECT | libc::O_CREAT)
    .mode(0o644);
let entry = opcode::OpenAt2::new(types::Fd(libc::AT_FDCWD), path_ptr, &how as *const _)
    .build();
```

### Alignment Requirements

| What | Alignment | Notes |
|------|-----------|-------|
| Buffer address | 4096 bytes (page) | Use `Layout::from_size_align(size, 4096)` |
| I/O size | 512 bytes (sector) | Each write length must be sector-multiple |
| File offset | 512 bytes (sector) | Each write offset must be sector-aligned |

Some filesystems (XFS, ext4) accept 512-byte alignment. Others require 4096. Check with
`statfs` or `ioctl(BLKSSZGET)`. Use 4096 for maximum compatibility.

**Final write problem:** The last blob in a PBF is unlikely to be sector-aligned. Write
the padded sector, then `ftruncate` to actual file size. io_uring has
`IORING_OP_FTRUNCATE` (kernel 6.9+).

### Why O_DIRECT Matters for Planet Merge

Planet merge: 80GB read + 80GB write = 160GB page cache churn. On a 64GB host:
- Without O_DIRECT: evicts ALL useful cached data from other services
- With O_DIRECT: reads go to application buffers (DecompressPool), writes go straight to
  disk. Zero page cache impact.

The read pipeline already manages its own buffering via `DecompressPool` + `Bytes::from_owner`.
The page cache is pure overhead at planet scale — we never re-read a blob.

## Buffer Ownership and Lifetime

The critical safety concern with io_uring:

> The kernel owns the buffer from SQE submission until CQE completion. During that window,
> the buffer must not be read, written, freed, or moved by userspace. Any access is a data
> race.

The `io-uring` crate puts this on the caller: `register_buffers()` and opcode constructors
are `unsafe`. Buffer lifetime tracking is manual.

**Pattern for PBF writer:** Free-list of buffer indices.
- Compression thread finishes → copies output to registered buffer → sends
  `(buf_index, len, offset)` to I/O thread
- I/O thread pushes `WriteFixed` SQE with that buf_index
- On CQE completion → buf_index returns to free-list
- **Never touch a buffer while its index is in-flight**

## SQ/CQ Sizing

- SQ depth (`IoUring::new(n)`) should be ≥ max in-flight I/O count
- CQ defaults to 2× SQ size
- If CQEs aren't reaped fast enough, submissions fail with `EBUSY`
- For a writer with 64 registered buffers: `IoUring::new(64)` or larger

## SQ Polling Mode

`setup_sqpoll(idle_ms)` creates a kernel thread that polls the SQ, eliminating
`io_uring_enter` syscalls entirely. The kernel thread sleeps after `idle_ms` of inactivity.
Consumes a CPU core. Probably overkill for PBF writing where the bottleneck is compression,
not syscall overhead — but worth benchmarking for the `Compression::None` case where writes
become the bottleneck.

## Provided Buffer Rings (Kernel 5.19+)

For read operations where the kernel picks which buffer to fill:

```rust
unsafe {
    ring.submitter().register_buf_ring_with_flags(
        ring_addr, ring_entries, bgid, flags,
    )?;
}
```

More relevant for network recv / disk read where you want the kernel to select a buffer.
For writes, we control which buffer to use, so `register_buffers()` + `WriteFixed` is
the right approach.

## Current pbfhogg Write Path (for context)

The pipelined writer (`PbfWriter::to_path_pipelined`) uses:
- `WRITE_AHEAD = 32` bounded channel for backpressure
- Rayon pool for parallel compression (`frame_blob()`)
- Dedicated writer thread with VecDeque reorder buffer
- `BufWriter<File>` with 256KB capacity
- `write_all()` for each reordered blob

Passthrough blobs (`write_raw`) skip rayon entirely — sent directly to writer thread.

The writer thread loop: receive from channel → insert into VecDeque at sequence slot →
drain consecutive ready items → `write_all()` each → flush on channel close.

## Key Observations

- **No io_uring opcode for copy_file_range.** Blob passthrough kernel-copy must use the
  syscall directly, Splice through a pipe, or explicit Read+Write through the ring.

- **Registered buffers and WRITEV are mutually exclusive.** Can't do scatter-gather with
  fixed buffers. Each registered buffer needs its own `WriteFixed` SQE.

- **O_DIRECT works without io_uring.** Plain synchronous `write()` with aligned buffers
  on an O_DIRECT fd bypasses the page cache. io_uring adds batching on top.

- **Kernel 6.18 has everything.** All opcodes through 6.16 available. `FTRUNCATE` (6.9+)
  solves the final-write alignment problem.
