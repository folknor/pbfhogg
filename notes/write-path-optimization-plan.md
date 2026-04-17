# Write-path optimization plan

This note is the working plan for the generic write / compression path shared
by `cat`, `extract`, `merge`, `sort`, ALTW stage 4, and other PBF-producing
commands.

It is intentionally separate from the ALTW external plan:

- it covers `src/write/writer.rs`, `src/write/file_writer.rs`, and the output
  side of commands that feed them
- it excludes ALTW-local decode / join / scatter work
- it focuses on the smallest harness that can answer each question
- it records both code-level and policy-level questions (for example,
  compression defaults and durability semantics)

Use this as the sequencing document once ALTW-local work is exhausted.

## Current architecture

The generic write path is:

1. command produces serialized `PrimitiveBlock` bytes or raw framed passthrough
   bytes
2. `PbfWriter` either:
   - frames + compresses synchronously (`PbfWriter::new`)
   - or dispatches framing + compression to rayon and hands framed blobs to a
     dedicated writer thread (`PbfWriter::to_path`)
3. the writer thread reorders by sequence number and writes to `FileWriter`
4. `FileWriter` is either:
   - buffered (`BufWriter<File>`, 256 KiB)
   - `O_DIRECT` (`DirectWriter`)
   - or `io_uring` via a separate writer-thread implementation

Important implementation details on current `main`:

- `WRITE_AHEAD = 32`
- `PIPELINE_DISPATCH_PERMITS = 64`
- pipelined framing uses per-thread `FrameScratch`
- passthrough blobs bypass compression and go straight to the writer thread
- buffered `FileWriter::flush()` ends with `sync_all()`

That last point is significant: every completed buffered write currently gets
an fsync-like durability barrier at the end of the command, not just a
userspace flush.

This is deliberate policy, not accidental drift:

- added in commit `1c04b5e` (`Fsync output files after writing for crash durability`)
- documented externally as "`--fsync` always enabled" in
  [reference/osmium-parity.md](../reference/osmium-parity.md)
- chosen partly to align buffered / direct output with the `io_uring` writer,
  which already did a final `sync_all()`

What is *not* currently well documented is the exact product invariant this is
meant to protect beyond the generic "crash durability" wording, or what the
real measured cost is on today's workloads.

## Current measured signals

### Isolated write throughput

From [reference/performance.md](../reference/performance.md):

- pipelined write already hides most compression cost on the synthetic write
  bench:
  - Denmark: `7.3s` none, `7.4s` zlib:6, `7.3s` zstd:3
  - North America: pipelined zlib `4m27s`, none/zstd `~4m20s`
- sync zlib is much slower than pipelined zlib

Interpretation:

- on the isolated write bench, the generic pipelined writer is usually not the
  main bottleneck once compression is parallelized
- that means many "writer optimizations" should be judged with an end-to-end
  command, not just `bench write`

### Command-level output bottlenecks

The ALTW work established a second truth:

- command producers can still outrun the writer/compression side even when the
  isolated write bench looks healthy
- Europe ALTW default showed large cumulative downstream pressure:
  `s4_send_ms` in the hundreds of seconds cumulative, with `s4_consumer_write_ms`
  and `s4_flush_ms` confirming steady-state writer limitation
- `zstd:1` materially improved ALTW Europe wall earlier in the arc, proving
  that compression choice can be the real wall lever even when the isolated
  writer bench looks flat

So this plan treats the write path as two related rails:

- **generic writer rail** - framing, compression, channeling, file backends
- **command integration rail** - whether a command is actually gated by the
  writer on the target workload

### Existing measured backend facts

- `io_uring` already has a measured crossover for merge-like workloads:
  [reference/performance.md](../reference/performance.md) records that it
  starts to pay around `4-5 GB` input and shows a clear North America win
- `zstd` is already a proven internal-pipeline lever when interop does not
  matter
- the repo already rejected at least one naive writer-memory idea:
  `frame_blob_into` buffer recycling via shared mutex pool regressed throughput
  (`notes/memory/p6-vectored-writer-framing.md`, referenced from
  [src/write/writer.rs](../src/write/writer.rs))

## Harness ladder

This plan only works if each item uses the smallest harness that can answer it.

### `brokkr bench write`

Use this first for:

- framing/compression CPU
- rayon dispatch / permit / writer-thread reorder behavior
- compression algorithm and level sweeps

It is the smallest meaningful harness for generic `PbfWriter` work.

Do **not** use it for:

- fsync / `sync_all` cost
- real file backend behavior
- `io_uring` output I/O

because the benchmark writes to `/dev/null` and therefore does not exercise the
real file backend.

### Real-file command harnesses

Use these when the file backend itself is part of the question:

- `cat` for general real-file output throughput
- `apply-changes` / merge for passthrough + copy-range + large-output backend
  behavior
- ALTW Europe only when the question is specifically "is the writer the wall
  under this command?"

Hidden dev-only helper:

- `PBFHOGG_WRITE_SKIP_SYNC_ALL=1`
  - keep this as a measurement aid, not a user-facing feature
  - use it only for tiny real-file A/Bs where the question is:
    - "is this small tail just durability policy noise?"
    - "how much lower is the no-durability floor on this harness?"
  - specifically useful for:
    - Denmark `cat` / merge / `apply-changes` tail checks
    - later `fdatasync` vs `sync_all` work
    - small buffered/direct/`io_uring` comparisons where end-of-command flush
      cost could mask the signal
  - do **not** use it as a normal benchmark mode, and do not use it for the
    generic `brokkr bench write` compression / queue items

### Dataset ladder

**Denmark** - correctness, smoke tests, tiny-end tail checks, very local file
backend questions.

**Japan** - first real gate for writer-local CPU/compression work. Large enough
to show framing/compression differences without paying Europe costs.

**Europe** - first real gate for:

- real-file backend behavior
- `io_uring`
- any command-level writer bottleneck question
- any change whose only value appears once page cache / writeback matter

**Planet** - only for ship/no-ship or when the whole point is "does this hold
at planet?"

## Instrumentation gaps

The current write path is under-instrumented compared to ALTW.

Before tuning anything subtle, add generic writer counters:

- `writer_permit_wait_ns`
- `writer_frame_ns`
- `writer_compress_ns`
- `writer_pipeline_send_wait_ns`
- `writer_recv_wait_ns`
- `writer_reorder_high_water`
- `writer_write_ns`
- `writer_flush_ns`
- `writer_bytes_framed`
- `writer_bytes_written`
- payload mix:
  - framed bytes
  - raw bytes
  - raw chunks
  - `copy_file_range` bytes

And backend counters in `FileWriter` / `uring_writer`:

- buffered/direct write call count
- buffered/direct write bytes
- `sync_all` time
- `io_uring` submit/wait time
- `io_uring` CQ drain wait

Without these, queue-tuning work is mostly guesswork.

## Measured and shelved

Do not spend time here again unless a precondition changes:

- shared mutex-based output buffer recycling for framed blobs
  - explicitly documented as a throughput regression in
    [src/write/writer.rs](../src/write/writer.rs)
- naive "compression is the whole problem" assumptions on the isolated bench
  - pipelined none/zlib/zstd already converged on several datasets
- ALTW-local node-wire filtering as a writer fix
  - it reduced worker CPU but did not deliver a worthwhile wall win
- swapping the zlib compressor for another api-compatible zlib
  implementation (zlib-ng, Chromium zlib)
  - `zlib-rs` is already at or near the top of that family: ~6 % faster
    than `zlib-ng` at level 6, ~10 %+ at level 9, marginally slower at
    some other levels. It is also the fastest api-compatible decompressor
    (6-13 % over `zlib-ng`, 1-6 % over Chromium zlib). Source:
    <https://trifectatech.org/blog/zlib-rs-is-faster-than-c/>.
  - so the "swap for a faster zlib" lever is essentially closed. Only
    `libdeflate` (non-api-compatible, one-shot, different algorithm tier)
    remains - see item 2b, which is a *revisit* rather than a new idea.
- `libdeflate` as the zlib compressor in pipelined mode at Denmark scale
  - three-commit arc Feb 27 - Mar 1, 2026:
    - `4a55c88` added `libdeflater` feature flag
    - `2cd6ed6` measured: sync `zlib:6` `24.4 s → 12.7 s` (`1.92×`),
      pipelined Denmark `6.9 s → 6.7 s` (essentially flat)
    - `d180d62` removed, citing "pipelined mode is decode-bound" plus
      "zero C dependencies for compression, one backend everywhere"
  - the *sync* `1.92×` win is banked - that is not the shelved piece
  - the *pipelined* null result is shelved **only at Denmark scale**.
    The benchmark commit is explicit that decode, not compression, was
    the wall ("decode is the bottleneck at Denmark scale"). Denmark is
    the smoke tier of this plan's dataset ladder, not the real gate.
  - item 2b re-opens this at planet `apply-changes` (`~92 %` rewrite
    ratio → compression actually on the critical path), and treats the
    "zero C deps" policy as a separate decision from the measurement

## Proposed order

### 0. Instrument the writer path

Hypothesis:

- The next useful write-path decisions depend on queue/permit/writeback
  attribution we do not currently expose.

Code surface:

- `src/write/writer.rs`
- `src/write/file_writer.rs`
- `src/write/uring_writer.rs`
- any command glue needed to surface counters cleanly

Smallest meaningful dataset:

- Denmark smoke via `brokkr bench write`

Keep gate:

- counters exist and are legible on both isolated write bench and at least one
  real-file command

Why first:

- every later item depends on it

### 1. Revalidate the deliberate end-of-command durability policy (`sync_all`)

Hypothesis:

- Buffered `FileWriter::flush()` currently calls `sync_all()` unconditionally.
  That is deliberate policy, but the original reason and current cost should be
  revalidated rather than treated as permanent by inertia.

Code surface:

- `src/write/file_writer.rs`
- `src/write/uring_writer.rs`
- `reference/osmium-parity.md`
- CLI surface only if a new opt-in / opt-out flag is eventually required

Questions to answer:

- What exact invariant is the current always-fsync policy meant to guarantee?
  Examples:
  - "successful command exit means bytes are on stable storage"
  - "output files must survive power loss without a caller-managed fsync"
  - "match osmium `--fsync` semantics by default"
- Does that invariant still matter for all write commands?
- What is the real cost on current hardware and datasets?
- If the invariant is still desired, keep it and stop.
- If the invariant is no longer universal, should durability become:
  - opt-in (`--fsync`)
  - command-specific
  - or disabled by default for temporary/internal outputs

Smallest meaningful dataset:

- Denmark real-file command

Metrics:

- total wall
- final flush time
- explicit `sync_all` time (item 0 prerequisite)

Keep gate:

- only change policy if:
  - the measured tail is material (`>= 3%` wall on Denmark or clearly larger
    on Japan/Europe), and
  - the original durability invariant is either no longer required or can be
    preserved behind a more explicit option/policy boundary

Discard gate:

- the durability guarantee is still wanted as a repo-wide invariant
- or measured tail is too small to matter

Result (2026-04-16):

- invariant recovered:
  - commit `1c04b5e` explicitly framed this as crash durability
  - `reference/osmium-parity.md` documents the repo policy as
    "`--fsync` always enabled"
  - so the current invariant is effectively:
    "successful command exit means the output has crossed a durability barrier"
- measured on Denmark `cat` via same-commit A/B using the hidden
  `PBFHOGG_WRITE_SKIP_SYNC_ALL=1` benchmark bypass from `1ea2844`:
  - buffered baseline `8976e31f`: `2901 ms`,
    `writer_sync_all_ns=252680975`
  - buffered skip-sync `fc8266e1`: `2600 ms`,
    `writer_sync_all_ns=0`
  - buffered delta: about `-301 ms` / `-10.4%`
  - direct-io baseline `cda1f09f`: `3100 ms`,
    `writer_sync_all_ns=8274803`
  - direct-io skip-sync `6d60927d`: `3000 ms`,
    `writer_sync_all_ns=0`
  - direct-io delta: about `-100 ms` / `-3.2%`, with only ~`8 ms`
    directly attributable to `sync_all` itself
- read:
  - the durability tail is real and material on the buffered path
  - it is small on the direct path
  - the invariant is explicit enough that the default policy should stay
    in place for now
- decision:
  - keep repo-wide durability semantics as the default
  - do not spend more time on "remove `sync_all` entirely"
  - if we revisit this line, it should be as a narrower item:
    `fdatasync` vs `sync_all`, or a more explicit durability policy surface

Why second:

- The code inspection already found real policy history (`1c04b5e`,
  `reference/osmium-parity.md`), so this is no longer a speculative "maybe we
  forgot why this exists" item.
- It is still conceptually simple and can be answered on a small dataset before
  any deeper writer tuning.

### 2. Compression characterization: zlib levels and zstd rail

Hypothesis:

- The best generic write-path win may be configuration, not code.
- `zlib:6` is a compatibility default, not automatically the best internal
  pipeline choice.

Code surface:

- possibly none at first: this begins as measurement and policy
- later maybe CLI presets or command-specific defaults

Smallest meaningful dataset:

- Denmark for initial compression matrix
- Japan for first real gate
- Europe only if command-level integration is ambiguous

Primary harness:

- `brokkr bench write`

Secondary harness:

- a real-file command when the question is backend or page-cache sensitive

Suggested matrix:

- `none`
- `zlib:1`
- `zlib:3`
- `zlib:6`
- `zstd:1`
- `zstd:3`

Metrics:

- total wall
- output size
- `writer_compress_ns`
- `writer_permit_wait_ns`
- `writer_reorder_high_water`

Keep gate:

- keep a new recommendation / preset if Japan improves `>= 5%` with acceptable
  output-size tradeoff

Discard gate:

- if the matrix shows no meaningful difference on Japan

Result (2026-04-16):

- Denmark matrix on `1ea2844`:
  - sync:
    - `none` `62ec73fc`: `7743 ms`
    - `zlib:1` `08b9ec09`: `10595 ms`
    - `zlib:3` `be21d55c`: `14586 ms`
    - `zlib:6` `bdcefe12`: `16249 ms`
    - `zstd:1` `131392d9`: `8794 ms`
    - `zstd:3` `5697b761`: `9397 ms`
  - pipelined:
    - `none` `cc883dcf`: `6701 ms`
    - `zlib:1` `06dd6a12`: `6678 ms`
    - `zlib:3` `8d34a40e`: `6759 ms`
    - `zlib:6` `e07bf173`: `6817 ms`
    - `zstd:1` `e3874060`: `6704 ms`
    - `zstd:3` `8a6d7fdd`: `6662 ms`
- Japan matrix on `1ea2844`:
  - sync:
    - `none` `cdeb497b`: `37003 ms`
    - `zlib:1` `e6618434`: `50788 ms`
    - `zlib:3` `8edd1b8f`: `69393 ms`
    - `zlib:6` `28560747`: `76946 ms`
    - `zstd:1` `b13782e2`: `41946 ms`
    - `zstd:3` `ec7102f5`: `45063 ms`
  - pipelined:
    - `none` `10a28de7`: `32416 ms`
    - `zlib:1` `ee91bd9e`: `32641 ms`
    - `zlib:3` `4932573f`: `32728 ms`
    - `zlib:6` `3cd5810f`: `32786 ms`
    - `zstd:1` `eb30edf9`: `32664 ms`
    - `zstd:3` `7d3aade9`: `33165 ms`
- read:
  - sync mode is extremely compression-sensitive
  - pipelined mode is effectively flat across this whole matrix on Denmark and
    Japan
  - Japan pipelined spread is only about `2.3%` best-to-worst, so the generic
    pipelined preset question does **not** clear the plan's `>= 5%` gate
- decision:
  - no generic pipelined preset change from `bench write`
  - keep the narrower lesson that sync mode strongly disfavors `zlib:6`
  - move on; do not spend more time on generic compression-level tuning on this
    harness

Why third:

- This is already partially de-risked by existing ALTW and write-bench data
- It may close the problem without any deeper code work

### 2b. Revisit `libdeflate` at planet scale

This item is a **revisit**, not a new hypothesis. `libdeflate` was added,
benchmarked, and removed in a three-commit arc earlier in 2026
(`4a55c88`, `2cd6ed6`, `d180d62`). See *Measured and shelved* above for
the full context. The short version:

- sync mode win is already *measured and banked*: `zlib:6` `24.4 s →
  12.7 s` (`1.92×`)
- pipelined mode was measured flat (`6.9 s → 6.7 s`), but only at
  **Denmark** - the smoke tier - where the bench commit itself noted
  decode was the wall, not compression
- removal also cited a policy goal ("zero C dependencies for
  compression, one backend everywhere"); that policy is a separate
  decision from the measurement

Separately, the api-compatible-zlib family (zlib-ng, Chromium zlib,
etc.) is essentially closed as an alternative: `zlib-rs` is already at
or near the top of that family for both compression and decompression
(source in *Measured and shelved*). `libdeflate` is the only remaining
tier-change lever, and specifically because it is *not* api-compatible
(one-shot, whole-buffer, different algorithm) - which is a natural fit
for PBF blobs but means it cannot replace the read path's streaming
decompressor.

Hypothesis:

- On a command where compression is actually on the critical path -
  planet `apply-changes` is the concrete target, with its
  **~92 % rewrite ratio** per [reference/pipeline.md](../reference/pipeline.md) -
  the pipelined path is compression-bound rather than decode-bound, and
  the sync-mode `1.92×` compression speedup translates into a
  measurable wall win. Denmark's null result is a dataset-scale
  artifact, not a general statement about pipelined mode.

Code surface:

- `src/write/writer.rs` (`compress_zlib`)
- `Cargo.toml` (re-add optional `libdeflater` dep)
- feature gate so the pure-Rust `zlib-rs` path stays the default
  behaviour. Likely the same shape as the old `4a55c88` gating, so the
  re-implementation is substantially a cherry-pick + policy decision
  rather than new code.

Smallest meaningful dataset:

- Europe `apply-changes` for first signal (already above the
  Denmark-scale decode wall)
- planet `apply-changes` only for ship / no-ship, because that is the
  production rail where the `92 %` rewrite ratio was measured

Explicitly **not** the isolated `brokkr bench write` harness: the
previous attempt already proved that harness cannot discriminate.

Metrics:

- `writer_compress_ns` and `writer_permit_wait_ns` (item 0 prerequisite)
- total `apply-changes` wall at Europe and planet
- peak RSS delta (libdeflate holds larger state per active compressor)
- output size delta (expected `<1 %` per libdeflate's own docs, but
  confirm on this dataset)

Keep gate:

- Europe `apply-changes` wall improves `>= 3 %` at equivalent output
  size, **and**
- planet `apply-changes` wall confirms the direction (same sign, not
  necessarily same magnitude), **and**
- peak RSS delta fits the planet host's budget
- **and** the "zero C deps" policy is actively revisited and accepted,
  not silently overridden - this is a standing policy from `d180d62`,
  not just a measurement question

Discard gate:

- item 0 counters show compression is not the dominant cost on planet
  `apply-changes` either (in which case the `92 %` rewrite intuition
  was wrong, and no compressor swap will help)
- or policy reconfirms "zero C deps" as the binding constraint, in
  which case the measurement is moot

Why here and not in item 2:

- item 2 is zlib *level* tuning within `zlib-rs`. This is an
  implementation swap with a different risk profile: new C dep, larger
  per-active-compressor state, no streaming API, and a prior explicit
  policy rejection.
- the easy configuration answer (item 2) should not be delayed by the
  harder dependency / policy question (item 2b).

### 3. Queue tuning: `WRITE_AHEAD`, permits, and raw-passthrough balance

Hypothesis:

- Current `WRITE_AHEAD = 32` and `PIPELINE_DISPATCH_PERMITS = 64` are plausible
  but not obviously optimal across workloads.
- Raw passthrough blobs bypass the permit pool entirely, so some workloads may
  be limited by writer-thread queue behavior rather than compression worker
  count.

Code surface:

- `src/write/writer.rs`

Smallest meaningful dataset:

- Japan on `brokkr bench write` for generic queue effects
- Europe on a real command if the isolated bench shows a signal

Metrics:

- `writer_permit_wait_ns`
- `writer_pipeline_send_wait_ns`
- `writer_recv_wait_ns`
- `writer_reorder_high_water`
- total wall

Keep gate:

- Japan isolated write bench improves `>= 3%`
- then confirm once on Europe with a real command before banking

Discard gate:

- queue metrics move but wall stays flat

Why fourth:

- Needs item-0 instrumentation
- More likely to matter after compression choice is understood

### 4. Command-level compression rail

This is the generic version of the ALTW compressed-output rail.

Hypothesis:

- Some commands are bottlenecked not by generic writer mechanics, but by the
  interaction between producer concurrency and compression throughput.

Targets:

- ALTW stage 4
- any other command whose producer phase can out-run pipelined compression

Smallest meaningful dataset:

- Europe

Metrics:

- command-local producer backpressure counters
- generic writer counters from item 0
- total wall

Keep gate:

- keep only if a command-level change improves Europe total wall `>= 1.5%`

Discard gate:

- if the command is not actually compression-bound after item 2

Why this is later:

- this is integration work, not generic writer work
- the generic writer rail should answer the easy questions first

### 5. `io_uring` output rail

Hypothesis:

- Real-file output on larger datasets may still benefit from the existing
  `io_uring` backend outside merge, especially for low-compression or no-
  compression paths.

Code surface:

- mostly integration and measurement first:
  - `src/commands/mod.rs`
  - command entry points that expose `--io-uring`
- later, only if needed:
  - `src/write/uring_writer.rs`

Smallest meaningful dataset:

- Europe

Why not Japan:

- existing measurements already suggest the crossover is above the smaller
  datasets; Europe is the first dataset in the usual ladder where backend I/O
  is plausibly large enough to matter

Metrics:

- total wall
- `writer_write_ns`
- `writer_flush_ns`
- `io_uring` submit/wait counters

Keep gate:

- keep only if Europe real-file wall improves `>= 5%`

Discard gate:

- if flat on the first Europe A/B, stop

Why fifth:

- more environment-sensitive than the earlier items
- already partially proven for merge-like workloads, so the first step is
  characterization, not backend surgery

### 5b. Batched `io_uring` SQE submission

Hypothesis:

- `uring_writer` calls `submit()` once per SQE today, so each frame crosses
  into the kernel via its own `io_uring_enter` syscall. Batching several
  SQEs per enter - either by waiting until the SQ has some threshold of
  entries, or until the incoming channel is briefly idle - would halve or
  quarter that syscall rate at high throughput.
- `reap_cqes(false)` is already opportunistic. Submission is eagerly
  paired with each push, making the two sides asymmetric.

Code surface:

- `src/write/uring_writer.rs` (`submit_buffer`, `submit_copy_chain`,
  `uring_main_loop`)

Smallest meaningful dataset:

- Europe on a command that already has `--io-uring` exposed and is not
  compression-bound (so the `io_uring` path is on the critical path)

Metrics:

- `io_uring` submit count (new counter from item 0)
- `io_uring` submit wait time
- total wall

Keep gate:

- submit count drops `>= 2×` with wall unchanged or improved
- no regression on small-output commands (Denmark / smoke)

Discard gate:

- `io_uring` is not on the critical path for the target command once item
  2 / item 4 has run
- or syscall cost is already drowned out by compression / memcpy

Why separate from item 5:

- item 5 is integration - whether commands should expose `--io-uring`
  more widely. This is an internal submission-strategy change that only
  matters once a command is actually on the `io_uring` path.

### 6. Low-priority extras

These are real, but should not get ahead of the queue above.

#### Buffered writer capacity sweep

Current buffered writer capacity is `256 KiB`.

Only revisit if item-0 counters show:

- many small write calls escaping the `BufWriter`
- or `writer_write_ns` remains unexpectedly high on buffered output

Smallest meaningful dataset:

- Denmark real-file first, then Europe if needed

#### Borrowed `write_primitive_block()` rescans

The borrowed API still scans block IDs and tagdata inside the writer closure,
but almost all production commands already use `write_primitive_block_owned()`.

Do not prioritize this unless a specific command still spends meaningful wall
there.

## Items from code inspection

The items above were built from measured signal. The items below came from
a fresh read of `src/write/writer.rs`, `src/write/file_writer.rs`,
`src/write/direct_writer.rs`, and `src/write/uring_writer.rs` without
reference to current users of those types. They are real code-level
observations, but none has measured priority against the backlog above: in
particular they should not jump item 0.

The one shared framing observation worth stating plainly before the items:
a compressed blob in the pipelined path currently exists in four distinct
owned regions between the compressor and the kernel - `compress_buf`
(compressor output), `blob_buf` (protobuf-framed body), `out`
(`4 B length | BlobHeader | Blob` concatenation), and the `BufWriter`
internal buffer - with a memcpy across each boundary. Several items below
attack that chain from different angles.

### 7. Vectored-write restructure of the writer thread

Hypothesis:

- Each framed blob currently materializes a fresh `out: Vec<u8>` in
  `frame_blob_into` (`4 B length | BlobHeader | Blob protobuf body`) via
  three `extend_from_slice` calls into a freshly allocated Vec, so that a
  single owned `Vec<u8>` can be sent through the pipeline channel and
  `write_all`'d by the writer thread. That is one allocation plus at
  least two memcpies per blob (`blob_buf → out`, `out → BufWriter`)
  existing purely for concatenation, not for framing or compression.
- Changing the pipeline channel payload from `Vec<u8>` to a small
  `(prefix_header, blob_body: Vec<u8>)` or equivalent slice set, and
  having the writer thread call `writev(2)` - bypassing `BufWriter` for
  the vectored path - removes both the concat and the `BufWriter` copy.
- Distinct from the shelved mutex-pool buffer recycling work - this does
  not recycle buffers, it removes the copy that motivated recycling in
  the first place.

Code surface:

- `src/write/writer.rs` (`PipelinePayload`, `frame_blob_into`,
  `writer_thread`)
- `src/write/file_writer.rs` (or a sibling) to expose a vectored write
  entry point for buffered output; `DirectWriter` stays as-is because
  `O_DIRECT` already bypasses `BufWriter`
- `src/write/uring_writer.rs` only if the chunk-list shape changes enough
  to warrant mirroring it there

Smallest meaningful dataset:

- Japan on `brokkr bench write` for framing / memcpy attribution
- Europe real-file command for backend interaction

Metrics:

- `writer_frame_ns`
- `writer_write_ns`
- allocations per blob (process-level or via `--alloc`)
- total wall

Keep gate:

- `writer_frame_ns + writer_write_ns` drops meaningfully on Japan bench
- and Europe real-file wall holds or improves on at least one command

Discard gate:

- item 0 instrumentation shows framing + writer-thread memcpy is a small
  share of the pie on every current command
- or `writev` coalescing is fighting `BufWriter`-style accumulation
  badly enough that small-output commands regress

Why here:

- this is a hypothesis from code reading, not from a measured hot spot
- on the isolated bench pipelined `none` / `zlib` / `zstd` already
  converged, so the signal has to come from item 0 instrumentation plus a
  command where the writer is actually on the path

Result (2026-04-16):

- step 1 landed as API cleanup:
  - `603385e` / `3602978`
  - `OutputChunk`, `FramedBlobParts`, `OutputSink`
  - default behavior preserved (flatten locally through `FileOutputSink`)
- first step-2 probe on planet `cat` was the wrong harness:
  - `PBFHOGG_WRITE_SKIP_SYNC_ALL=1` baseline `99bcfd69`: `476.9s`
  - initial vectored sink `01b49b83`: `476.4s`
  - batching had not engaged (`50642` writev calls for `50816` frames)
  - threshold-tunable follow-up `4a68c52` with
    `PBFHOGG_WRITE_VECTORED_BYTES=0 PBFHOGG_WRITE_VECTORED_FRAMES=64`
    produced `1b3a082b`: `502.0s`
  - batching engaged correctly there (`794` writev calls for `50816` frames,
    exactly `64.0` frames/syscall), but wall regressed hard
  - conclusion: planet `cat` is the wrong harness for this line
- framed-heavy ALTW `--compression none` is the right harness:
  - Japan, same commit `687d81e`, both with
    `PBFHOGG_WRITE_SKIP_SYNC_ALL=1`:
    - baseline `642ab89a`: `50.8s`
    - vectored `1d4913fc`: `39.7s`
    - delta: `-11.1s` (`-21.9%`)
    - `writer_write_ns`: `6.03s -> 1.34s`
  - Europe, same commit `687d81e`, both with
    `PBFHOGG_WRITE_SKIP_SYNC_ALL=1`:
    - baseline `0af3adde`: `306.8s`
    - vectored `a788e951`: `318.5s`
    - delta: `+11.7s` (`+3.8%`)
    - plain no-env reference `0d253e44`: `300.1s`
    - `writer_write_ns` still drops materially on the vectored run, but
      Europe wall regresses overall
    - batching engaged at about `14.0` frames/syscall
    - follow-up threshold probes confirmed over-batching as the Europe issue:
      - `PBFHOGG_WRITE_VECTORED_FRAMES=4` `3dcf20a0`: `313.2s`
      - `PBFHOGG_WRITE_VECTORED_FRAMES=2` `b01aa708`: `307.6s`
      - shrinking the cap reduces the regression sharply, but does not turn
        the path into a clear win on Europe/HDD
- decision:
  - item 7 is unresolved
  - keep the API cleanup (`603385e` / `3602978`)
  - the experimental vectored sink path was removed from `main` during
    surface cleanup; only the internal API cleanup remains
  - Japan proves the mechanism can help on framed-heavy buffered output, but
    Europe disproves the current threshold / batching shape as a robust win
  - the Europe follow-ups suggest the line is nearly salvageable only with
    very small batches, at which point the benefit largely disappears
  - do not treat planet `cat` as the generic gate for this family

### 8. Deferred header write (API capability)

Hypothesis:

- `PbfWriter::to_path*` requires `header_block_bytes` up front and writes
  the OSMHeader synchronously in the constructor. That forces callers to
  know the header (bbox, required-features, required string list,
  writing-program, replication metadata) before the first blob is produced
  - which is the wrong direction for callers that want to compute bbox
  from data or record counts in the header.
- Reserving a fixed-size slot at file offset 0 (say `1 KiB`, padded with
  an ignored OSMData blob if the real header ends up shorter) and writing
  the real header via `pwrite` at `finish()` time makes this possible
  without a second-pass rewrite of the whole file.
- This is a capability change, not only a performance change. It also
  simplifies the `to_path_uring` constructor, which currently has to
  serialize the framed header through the channel init path to the writer
  thread.

Code surface:

- `src/write/writer.rs` (constructors, `start_pipeline`, `writer_thread`)
- `src/write/uring_writer.rs` (init path, `flush_final`)
- `src/write/file_writer.rs` (`pwrite`-at-offset-0 entry point)
- public API: `to_path*` signatures change; `set_header` / `finish` added

Smallest meaningful dataset:

- Denmark for correctness / reader round-trip
- any command made to update its header at the end, to exercise the
  pwrite path

Metrics:

- correctness gate: round-trip `pbfhogg cat` and `osmium fileinfo` on the
  output
- wall delta on any command that adopts it

Keep gate:

- a real caller actually benefits from deferring header write (for
  example, `merge` producing a tighter bbox), and the `pwrite` path is
  robust under `O_DIRECT` / `io_uring`

Discard gate:

- no current or planned caller needs deferred header write
- or the reserved-slot scheme is not compatible with `O_DIRECT` page
  alignment without contortions

Why here:

- genuinely new capability, not on the measured backlog
- should not jump the measured items, but should not be forgotten either

### 9. `fallocate` + `posix_fadvise` on buffered output

Hypothesis:

- Sequential extension of an `~87 GB` planet file by `~400 KB` chunks
  forces extent allocation on the fly (ext4 / xfs) and churns the page
  cache. `fallocate(2)` up to a caller-supplied size hint
  (`with_size_hint`) turns extent allocation into a one-time cost.
  Periodic `posix_fadvise(POSIX_FADV_DONTNEED)` on already-drained
  regions prevents the writer from evicting unrelated hot data from the
  page cache.
- Interacts with item 1 (durability policy): pre-allocated extents mean
  `fsync` has fewer metadata updates to flush, so if fsync stays,
  `fallocate` can make it cheaper.

Code surface:

- `src/write/file_writer.rs` (buffered path)
- `src/write/writer.rs` (`to_path` and friends) to expose a size hint
- public API: `with_size_hint(u64)` builder-style on pipelined
  constructors

Smallest meaningful dataset:

- Europe real-file command (planet-scale behavior will not show on
  Japan)
- planet only for ship / no-ship

Metrics:

- buffered write call count
- `writer_flush_ns`
- `sync_all` time
- page-cache pressure (informal - `free -m` or `/proc/meminfo:Dirty`
  before / after)

Keep gate:

- Europe real-file command wall improves `>= 1.5 %`, or `sync_all` time
  drops meaningfully if item 1 keeps durability
- no correctness regression on short outputs (where the hint is wrong)

Discard gate:

- item 1 removed `sync_all` entirely, and Europe real-file wall is not
  constrained by page-cache pressure

Current status:

- a hidden-hook probe was tried briefly in `985f76c`
- Europe ALTW external `--compression none` probe:
  - nearby no-hint reference `0d253e44`: `300.1s`
  - hinted run `1fc20b79`: `303.2s`
- conclusion:
  - no obvious large win from preallocation alone
  - not worth a same-commit A/B
  - the hidden hook was removed during surface cleanup; if this line is
    revisited later, it should come back as a coherent real API or not at all

Why here:

- new observation from code read, touches both durability and caching
- should not jump item 1; actually pairs naturally with item 1's
  outcome

### 10. Parallel output via `pwrite` for `Compression::None`

Hypothesis:

- With `Compression::None`, a framed blob's output size is fully
  determined by `uncompressed.len() + BlobHeader size + 4 B length`, so
  output offsets can be computed as blobs are produced without waiting
  for a writer thread. Multiple writer threads could each `pwrite` their
  own slot in parallel, removing the writer-thread serialization point.
- With compression, this is infeasible without an extra phase - output
  size is unknown until the compressor returns - so this rail only
  applies to the `None` path.

Code surface:

- `src/write/writer.rs` (new writer topology for `Compression::None`)
- possibly a new `parallel_writer.rs`
- public API: none, if kept internal to the existing `to_path*` entry
  points and compression-gated

Smallest meaningful dataset:

- Japan isolated bench with `Compression::None`
- Europe real-file with a `Compression::None` command (ALTW stage 4 is
  plausible)

Metrics:

- total wall
- `writer_write_ns`
- writer-thread CPU

Keep gate:

- Europe `Compression::None` wall improves `>= 10 %` (this is a
  complex change - small wins do not justify the offset-reservation
  protocol)

Discard gate:

- item 2 / item 4 show no meaningful `Compression::None` rail on
  the production commands
- or writer-thread CPU was not the bottleneck on the target
  command

Why here:

- speculative and architecturally intrusive
- needs both item 0 and item 4 signal before prototyping

### 11. `PbfWriter` API consolidation

Hypothesis:

- `PbfWriter` exposes six block-entry methods with overlapping
  semantics: `write_primitive_block`, `write_primitive_block_owned`,
  `write_raw`, `write_raw_owned`, `write_raw_chunks`, and (cfg)
  `write_raw_copy`. The `W: Write` generic coexists with a pipelined
  path that requires `FileWriter`, which is why `writer_mut()` has to
  panic when the pipeline has consumed the writer.
- A two-type split (`PbfWriter<W: Write>` sync-only, generic; and
  `PipelinedPbfWriter` `FileWriter`-specific, parallel) plus a unified
  pair of entries - `write_block(OwnedBlock)` and
  `write_passthrough(Passthrough)` where `Passthrough` is an enum of
  `FramedBytes` / `FramedChunks` / `CopyRange` - removes the
  generic / pipelined mismatch, drops the `writer_mut` panic, and
  pushes all callers onto the `_owned` fast paths.

Code surface:

- `src/write/writer.rs` (type split, method consolidation)
- all callers - but nearly every one already uses `_owned` variants
  and goes through `BlockBuilder`, so the migration is largely
  mechanical

Smallest meaningful dataset:

- full test suite + Denmark correctness + `brokkr check`

Metrics:

- code size delta
- any behavioral delta on the test suite

Keep gate:

- API is meaningfully simpler (fewer methods, fewer panics, clearer
  lifetimes) and all current callers migrate without semantic loss

Discard gate:

- rare external users of `write_primitive_block` (borrowed) exist
  whose migration cost outweighs the simplification

Why here:

- pure cleanup, no measured perf justification on its own
- worth doing, but should not delay anything with measured signal
- naturally folded into item 6's "borrowed rescan" cleanup

## Hypotheticals not yet measured

These came out of the same fresh code read and are worth recording so
they are not rediscovered from scratch. None should be pursued until
the measured backlog (items 0-6) has surfaced concrete signal that one
of them could address - and none should be confused with the
**measured and shelved** list earlier in this note, which represents
work that has already been tried.

- **Per-worker SPSC buffer recycling.** Distinct from the shelved
  `Arc<Mutex<Vec<Vec<u8>>>>` pool (regressed at `+12 %`, `2bf438c`).
  The hypothesis here is that an SPSC lock-free queue per worker -
  with the writer thread returning drained buffers via a matching
  reverse channel - dodges the cross-thread contention that sank the
  mutex pool. Only worth prototyping if item 7 (vectored write)
  leaves a meaningful allocator cost on the hot path.
- **Registered-buffer `io_uring` framing.** Today the `io_uring`
  writer copies framed bytes *into* its `64 × 256 KB` registered
  buffers from rayon-produced `Vec<u8>`s. Having workers write framing
  directly into registered buffers (via a lock-free claim protocol,
  not a mutex) eliminates the writer-thread memcpy entirely. Only
  interesting if items 5 / 5b establish that the `io_uring` rail is
  meaningfully on the critical path.
- **NUMA-aware rayon worker placement.** On multi-socket hosts the
  writer thread's memcpy-to-buffer or submit-to-uring crosses NUMA
  nodes when rayon workers are not co-resident. Pinning the writer to
  one node and biasing workers to the same node removes cross-socket
  cache-line bouncing. Irrelevant on the current dev host; potentially
  relevant on future server deployments.
- **Zstd internal multi-threading (`CCtx::nb_workers`).** Useful only
  at high levels (`zstd:19`+) on archival-style one-shots where
  single-block compression throughput is the wall. The steady pipeline
  is many small blobs in parallel - already saturated - so this is
  archival-rail only.
- **Inline `BlockBuilder` → framer.** `BlockBuilder`'s `encode_buf`
  could reserve space for a `4 B` length prefix and the BlobHeader at
  the *front* of its own buffer, with a later patch-up of the length
  once the header is sized. That kills the remaining concat in
  `frame_blob_into`. Tight coupling, only worth doing once item 7
  actually ships and shows that even the vectored shape leaves
  measurable framing cost.
- **Eager per-worker compressor prewarm.** First blob on a cold rayon
  worker pays `Compress::new` (`~312 KB`) or zstd context allocation
  (`~500 KB`). A `rayon::broadcast` at `to_path` time to force warm
  thread-local scratch would shave tens of milliseconds on small-file
  commands. Only matters for short runs; large runs amortize it for
  free.
- **Inline the three `write_all` calls in the sync framing path.**
  `write_framed_blob` calls `write_all` three times (length / header /
  blob). `BufWriter` already combines them internally, so the saving
  is one vtable dispatch plus one buffer-state check per blob - pure
  microopt, worth noting only because item 7 would naturally collapse
  them anyway.
- **`fdatasync` instead of `fsync`.** If item 1 keeps durability but
  allows a weaker barrier, `fdatasync(2)` is `2-10×` faster than
  `fsync(2)` on rotational storage (smaller margin on NVMe) for a
  file written sequentially with no metadata fields beyond length.
  Only meaningful inside item 1's decision space.
- **Eliminate `PIPELINE_DISPATCH_PERMITS` via a bounded rayon
  alternative.** The permit counting-semaphore was a workaround for
  `rayon::spawn`'s unbounded task queue. A hand-rolled worker pool
  with its own bounded ingress queue, or `par_bridge` over an iterator
  with natural backpressure, would remove the per-blob permit channel
  ping-pong (two cross-thread atomics). Probably only a few percent
  even at planet scale; only interesting if item 3 identifies permit
  churn as a real cost.

These are intentionally kept as bullets, not full keep / discard
items: none has enough measured signal yet to deserve gate-style
sequencing.

## Recommended sprint shape

1. add generic writer instrumentation
2. answer the durability-policy question
3. run the compression characterization matrix
4. only then do queue tuning
5. only then pay for Europe on `io_uring` or command-level integration rails

That keeps the next cycle from degenerating into another expensive
command-specific bench hunt.

Items `2b`, `5b`, `7`-`11` and the *Hypotheticals not yet measured*
bullet list all remain parked behind item 0. They represent expansion
of the backlog from a fresh code read, not a reordering of the measured
work above - none of them should jump into the sprint without item 0
counters saying so.

## Current state

At this point, the performance side of this note is mostly exhausted.
The only clearly live high-upside item left here is the `io_uring`
rail on a real command path. The rest of the note is now either
answered, deferred for good reason, too permutation-heavy to justify
more Europe/planet time, or dependent on item 7 having turned into a
robust win, which it did not.

That does **not** mean the note is done. It still contains unresolved
API and surface-design questions that are separate from pure
performance:

- whether `OutputChunk`, `FramedBlobParts`, and `OutputSink` should
  remain the internal writer shape even if the vectored sink behavior
  does not ship
- whether deferred header write is a real future capability or just an
  interesting idea
- whether buffered size hints / preallocation should ever become a real
  builder surface

The cleanup pass already removed the writer experiment env vars. So the
remaining surface question is no longer "which knobs survive?" but
"which internal abstractions are worth keeping now that the knobs are
gone?" That is the healthier state: structure that still makes sense,
without carrying experiment routing in the shipped code.

So the practical read of this note now is:

- if the goal is more write-path performance work, the next real item
  is `io_uring`
- if the goal is reducing surface pollution and clarifying what we
  actually intend to support, this note should feed a separate cleanup
  plan rather than keep growing more experiment switches
