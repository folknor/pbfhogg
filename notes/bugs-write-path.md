# Bug hunt: write path (`src/write/`)

Hunt date 2026-08-12. Single Opus hunter, scope: `src/write/` in full. Findings
only, unverified. Ordered roughly by severity as the hunter ranked them.

## The default writer thread has no reorder-gap check - silent truncation reported as success

`src/write/pipeline.rs::writer_thread`.

`writer_thread` loops on `rx.recv()`, and on channel close simply `break`s, calls
`sink.flush()` and returns `Ok(())`. If a rayon framing closure spawned by
`write_primitive_block*` never sends its `PipelineItem` (panic inside
`scan_block_ids` / `frame_blob_into` / allocation failure - rayon catches spawn
panics and drops the closure's captured `tx`), every seq after the missing one
sits in the `ReorderBuffer` forever, gets dropped, and the writer reports
success. The output PBF is short by an arbitrary number of blobs and `flush()`
returns `Ok`.

Both sibling backends explicitly guard this exact case and say so in comments:
`parallel_writer::dispatch_loop` and `uring_writer::uring_main_loop` both return
`Err("... item(s) still in reorder buffer ...")`. The default backend - the one
every command uses without `--io-uring` / `--direct-io` on a non-parallel path -
is the one that does not. This directly violates the "no silent data loss"
stance in `reference/truncation-handling.md`.

Confidence: high; three-line fix (mirror the `pending_len() > 0` check).

Related: repeated rayon panics also leak dispatch permits (`permit_tx.send` in
the closure never runs), so after 64 such panics `write_primitive_block` blocks
forever on `permit_rx.recv()` with no timeout and no diagnostic.

## `DirectWriter` corrupts or drops data if `write` follows `flush`

`src/write/direct_writer.rs`.

`flush()` calls `flush_final()`, which pads the buffer to a page, writes it,
`set_len(logical_size)`, and latches `flushed = true`. Nothing prevents further
`write()` calls. If they happen:

- the fd position is now past the zero padding, so subsequent data lands after a
  mid-file zero gap (readers see nulls where a BlobHeader should be - exactly the
  bug the uring `CopyRange` comment describes having already been fixed once);
- the final partial buffer is silently dropped, because `flush_final` early-
  returns on `flushed`, and `Drop` calls the same latched function.

`DirectWriter` is a `pub` type implementing `Write`, so this is reachable by
contract-conforming use. Either make post-flush writes a hard error, or reset
`flushed` / reposition on write.

Confidence: high.

Same file: `flush_buf` treats any `libc::write` return `< 0` as fatal, including
`EINTR`, unlike the `pread` loop in `parallel_writer::copy_range_fallback_pwrite`
which retries. Inconsistent and needlessly fragile.

## `PbfWriter::flush()` doc is false, and misuse panics

`src/write/writer.rs`.

The doc says "After flush, the pipeline is stopped and subsequent writes go
through the direct (non-pipelined) path." For every pipelined constructor
(`to_path`, `to_path_direct`, `to_path_uring`, `to_path_parallel`)
`self.writer` is `None` - the file was moved into the writer thread - so any
post-flush write hits `writer_mut()`'s `.expect("writer consumed by pipeline")`
and panics. The documented recovery path does not exist.

Confidence: high.

## Real I/O errors are replaced by "writer thread terminated"

`src/write/writer.rs`.

When the writer thread dies (ENOSPC, EIO, uring short write), the receiver
drops, so the next `write_raw` / `write_raw_owned` / `write_raw_chunks` /
`write_raw_copy` returns a synthetic `io::Error::other("writer thread
terminated")`. The caller typically `?`-bails on that, which drops `PbfWriter`,
and `Drop` discards the join result - so the actual ENOSPC error is thrown away
and the user is told "writer thread terminated". The fix is for the send-failure
path to join the handle and surface the thread's error.

Confidence: high.

Note also the asymmetry: `write_primitive_block*` cannot detect this at all (the
rayon closure `drop(tx.send(...))`s), so a full disk lets the whole dataset be
processed before anything is reported.

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

## `add_way_with_locations` silently drops node refs on a length mismatch

`src/write/block_builder.rs` plus `encode.rs`.

The length invariant is only a `debug_assert_eq!`. In release,
`encode_way_with_locations` zips `refs.iter().zip(locations.iter())`, so if
`locations` is shorter the way is written with fewer refs than it has - a
silently corrupted way, no error anywhere. Given the ALTW paths feed this,
promote to a hard `assert!` / `io::Error`.

Confidence: high.

## Unknown relation member types are silently rewritten as nodes

`src/write/block_builder.rs::member_type_value`.

`MemberType::Unknown(_) => 0` turns an unmodelled member type into a node member
on write. The comment shifts responsibility to callers ("should filter these
out"); nothing enforces it. A round-trip of a future PBF extension silently
produces wrong data. This should be an error at the encode boundary, not a
defaulted enum.

Confidence: high.

## `HeaderBuilder` drops `source`, and `build`'s doc claims otherwise

`src/write/header_builder.rs`.

`from_header` copies bbox / replication / historical but not `source` (field 17),
and `build()` has no code path that emits field 17 at all - while its own doc
comment lists `source (string, field 17)` among the fields it writes. Every
command that rewrites a header via `HeaderBuilder` therefore silently loses
`source`. `framing.rs::strip_header_block_fields` even documents this
("Rebuilding through `HeaderBuilder` would silently drop several of those"). Fix
the builder or fix the doc; today the durable doc is false.

Confidence: high.

Also here: `replication_timestamp` / `replication_sequence_number` go through
`encode_int64_field`, which skips zero - sequence number 0 is silently dropped.

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
