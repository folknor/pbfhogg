# Bug hunt: write path (`src/write/`)

Hunt date 2026-08-12. Single Opus hunter, scope: `src/write/` in full. Findings
only, unverified. Ordered roughly by severity as the hunter ranked them.

## Large `--elements-per-blob` hard-fails on the BlobHeader size cap, via tagdata

`src/write/block_builder.rs` plus `framing.rs::encode_blob_header_into`.

`encode_block` serializes every distinct tag key in the block into `tagdata`,
which becomes BlobHeader field 4. `encode_blob_header_into` rejects any header
`>= 65536` bytes. At the default 8000-element cap the tagdata is already tens of
KB; `repack --elements-per-blob 228000` (an advertised, benchmarked use case -
planet-style densities) will routinely blow past 64 KB and abort the whole write
with an opaque "BlobHeader for OSMData is N bytes" error. There is no fallback
(truncate the key set, drop tagdata, split the blob).

Confidence: medium-high on the mechanism; worth an actual repack-at-density
test.

Secondary, currently masked by the above: `count as u16` and `key_len as u16` in
the tagdata serializer (and in `blob_meta/tag_index.rs::serialize`) truncate
silently. Since tagdata is consumed as a skip filter, a truncated key list would
make `tags-filter` skip blobs that do contain the key - wrong results, not just
slow ones. Add an explicit cap/error rather than relying on the header limit to
catch it first.

## `write_primitive_block*` cannot detect a dead writer thread

`src/write/writer.rs`.

The `write_raw*` / `write_raw_copy` send-failure paths now join the writer
thread and surface its real error, but `write_primitive_block*` still hands its
result to a rayon closure that ends in `drop(tx.send(...))`. A writer thread
that died on ENOSPC is therefore invisible until `flush()`, so a full disk lets
the whole dataset be processed before anything is reported. The permit pool is
the only backpressure, and it keeps handing out permits because the closures
keep completing.

Filed standalone from the (resolved) "Real I/O errors are replaced by writer
thread terminated" finding, whose fix covers only the paths that had a `Result`
to return.

Confidence: high on the mechanism; the cost is latency of reporting, not
correctness.

## `AlignedBufferPool` is freed while io_uring writes may still reference it

`src/write/uring_writer.rs`, error paths out of `uring_main_loop` /
`flush_final`.

On any error return, `UringState` is dropped without `drain()`. Field order
drops `ring` before `pool`, but closing the ring fd does not synchronously
complete or cancel in-flight `WriteFixed` SQEs; the kernel keeps the registered
pages pinned while userspace `dealloc`s the virtual range, which the allocator
can hand back out. A subsequent allocation can then be overwritten by a
completing DMA. Narrow (error paths only) but it is a memory-corruption class
bug, not a data bug. Best fix: make teardown attempt `drain()` (best-effort,
ignoring errors) before the pool is released.

Confidence: medium - depends on kernel teardown ordering; worth confirming
against `io_uring_release` semantics.

Same file: `user_data` packs `buf_idx` in the low 16 bits but `ud_buf_idx` masks
`0x3F`. Raising `NUM_BUFS` above 64 silently returns the wrong index to the free
list - two writers sharing one buffer, i.e. corrupt output, with no compile-time
guard. Add a `const _: () = assert!(NUM_BUFS <= 64);`.

## `Write::flush` means `fsync` - surprising and expensive

`src/write/file_writer.rs`.

`FileWriter::flush` calls `sync_all()` on both variants. Any incidental `flush()`
(a wrapping `BufWriter`, a generic helper, a library consumer following `Write`
semantics) triggers a full fsync. Should be a distinct `sync()` method; `flush`
should flush.

Confidence: high, low blast radius today because the pipeline flushes once.

Mirror issue in `parallel_gzip.rs`: `flush()` is a documented no-op, so a caller
who flushes and checks the error gets `Ok(())` with nothing written and no way to
know.

## `CopyRange` carries a bare `RawFd` across a thread boundary

`src/write/pipeline.rs`, `writer.rs::write_raw_copy`,
`parallel_writer::WriteOp::CopyRange`.

The input fd is queued into another thread's channel with no lifetime tie to the
owning `File`. If the caller closes the input before `flush()` (or the fd number
is recycled), the copy silently splices whatever is now at that descriptor - no
error, wrong bytes. Current in-tree callers (`apply_changes/drain.rs`,
`sort/mod.rs`) happen to keep the input alive, but nothing enforces it. Carry an
`Arc<File>` in the variant instead of a `RawFd`; the cost is one refcount per
passthrough run.

Confidence: high on the hazard, no current bug.

## Liveness of the pipeline depends on rayon scheduling order

`src/write/writer.rs` plus `pipeline.rs`.

Rayon workers block in `tx.send` on a 32-slot channel while holding a rayon
thread. The writer thread only drains in seq order. If the task for seq k is
still queued while 32 later-seq results fill the channel and the remaining rayon
threads all block in `send`, nothing can make progress: blocked rayon threads
cannot steal seq k. `PIPELINE_DISPATCH_PERMITS = 64` bounds memory but not
reorder distance. In practice rayon's FIFO injector makes a 48-job inversion very
unlikely, but the design has no structural guarantee - the invariant is "rayon
happens to run spawns roughly in order", which is not a documented rayon
property. If open to restructuring: replace `rayon::spawn` plus bounded channel
with an explicit fixed-size framing worker pool pulling from an ordered queue,
which makes liveness provable.

Confidence: low probability, high consequence; flagged as design fragility
rather than a bug.

## Smaller / structural notes

- `frame_blob_into` clones both scratch buffers per blob (`framing.rs`) while the
  doc block directly above claims "after warmup, only the returned `out` Vec is
  allocated per call". Two allocations plus two memcpys per blob, and the comment
  is wrong about it. A capacity-preserving swap (keep a small pool of Vecs in
  `FrameScratch`, swap in an empty one with reserved capacity) gets the
  documented behaviour.
- `encode_blob_body` uses `encode_bytes_field` (skip-if-empty) for the raw /
  compressed payload. A zero-length payload yields a `Blob` message with no
  fields at all. Should be `_always` for correctness of the wire contract; today
  unreachable but it is a latent malformed-output path.
- `WRITER_METRICS` is a process-global static, and `PbfWriter::flush()` calls
  `emit()`. `multi-extract` constructs one `PbfWriter` per region, so counters
  are cross-contaminated across regions and re-emitted (cumulatively) once per
  region flush. Per-writer attribution is impossible and sidecar counters for
  multi-writer commands are misleading.
- `parallel_writer.rs` module doc is stale and misleading: "Status: scaffold
  (2026-04-21) ... `to_path_parallel` / integration lands in a follow-up commit"
  - it is integrated and is the default backend for `apply-changes` and ALTW pass
  2. The whole module also sits under `#![allow(dead_code)]`, which suppresses
  genuine dead-code detection for a now-live backend. `POOL_SIZE` is 16 while its
  doc justifies 8.
- `raw_passthrough.rs` is 106 lines of `#[allow(dead_code)]` scaffolding whose own
  doc says two measurements disproved the idea at planet scale (0/50,364 and
  0/32,835 qualifying blobs). The analysis is valuable; the code is not. Move the
  prose into `notes/` and delete the module.
- `FileWriter`, `DirectWriter`, `UringState`, and `parallel_writer`'s pool each
  re-implement the same "accumulate, page-align, write, set_len" logic with
  different flush/durability semantics and different bug sets (the DirectWriter,
  uring pool, and fsync findings above are all instances). Four backends behind
  one `OutputSink` trait, three of which own their own file lifecycle and one of
  which does not, is the structural problem underneath several findings above. If
  willing to rewrite: collapse to one buffering/alignment core parameterized by
  submit strategy (write / pwrite-pool / uring), with a single explicit
  `finish() -> io::Result<()>` that is the only place `set_len` plus `sync_all`
  happen, and make `Drop` without `finish` a loud error rather than a silent
  best-effort.
