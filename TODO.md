# pbfhogg TODO

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` — it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    scripts/test.sh -- --ignored

## Performance: hotpath profiling results (Denmark 483 MB, Feb 2026)

**Primary consumers:** Both elivagar and nidhogg use the same pattern:
`ElementReader::from_path` -> `for_each_pipelined`. Nidhogg does 3 pipelined
passes per ingest. Nidhogg also uses `pbfhogg::merge::merge()` for weekly
planet updates. Both the read path and the merge write path are now heavily
optimized. The read path has decompression buffer pooling (`DecompressPool` +
`Bytes::from_owner`) and a zero-copy wire-format parser (`wire.rs`), yielding
**43-44% wall time improvement** across all single-threaded read modes. The
merge write path has parallel compression via `PbfWriter::to_path_pipelined`,
yielding **62% wall time improvement** (8.06s → 3.03s on Denmark). Finding 7
(blob-level indexdata) embeds element type + ID range in BlobHeader so that
subsequent merges can classify passthrough blobs without decompression — saving
~8% on Denmark (3.07s → 2.81s) with larger gains expected at planet scale.
O_DIRECT writes (`--direct-io`) add no measurable overhead on Denmark (2821ms
BufWriter vs 2876ms O_DIRECT, within noise) — the pipeline is CPU-bound on zlib
compression at this scale. The real win is page cache hygiene at planet scale
(80GB writes not evicting useful host data).

All command entry points, merge internals (`classify_blob`, `read_raw_frame`,
`rewrite_block`), and pipeline (`run_pipeline`) are instrumented with
`#[hotpath::measure]`. Run with `scripts/run-hotpath.sh` (timing only) or
`scripts/run-hotpath-alloc.sh` (timing + allocation tracking).

### Raw hotpath data (current, with wire parser + pool + blob indexdata)

**check-refs (Denmark, 52M nodes, 6.6M ways, 46K relations):**
```
Wall: 7.51s, RSS: 143 MB (hotpath timing, no alloc tracking)
check_refs:           1 call, 7.51s (100%)
run_pipeline:         1 call, 7.50s (99.9%)
for_each_pipelined:   1 call, 7.50s (99.9%)
decompress_blob:      7396 calls, 2.55s (33.9%), avg 345µs, P50 198µs, P99 1.72ms
block::new:           7396 calls, 97ms (1.3%), avg 13µs, P50 5µs, P99 166µs
Main thread: 100% CPU. Workers: 1% CPU each (5 threads, vastly idle).
```

**merge without indexdata (Denmark base + 1 OSC, osmium input):**
```
Wall: 3.16s, RSS: 91 MB (hotpath timing)
frame_blob:           629 calls, 5.63s total (178%, parallel), avg 9.0ms
classify_blob:        7386 calls, 3.25s (103%), avg 440µs, P50 259µs, P99 2.40ms
rewrite_block:        630 calls, 1.82s (57.7%), avg 2.89ms
block_builder::take:  7407 calls, 733ms (23.2%), avg 99µs
read_raw_frame:       7399 calls, 103ms (3.2%), avg 14µs
block::new:           630 calls, 14ms (0.5%), avg 23µs
Clean (no hotpath): 3.07s best of 3.
```

**merge with indexdata (Denmark base + 1 OSC, pbfhogg input):**
```
Wall: 2.86s, RSS: 85 MB (hotpath timing)
frame_blob:           549 calls, 5.40s total (189%, parallel), avg 9.8ms
rewrite_block:        550 calls, 1.71s (59.7%), avg 3.11ms
block_builder::take:  7408 calls, 673ms (23.5%), avg 91µs
classify_blob:        7382 calls, 603ms (21.1%), avg 82µs, P50 150ns, P99 1.99ms
read_raw_frame:       7400 calls, 90ms (3.2%), avg 12µs
block::new:           631 calls, 14ms (0.5%), avg 23µs
Clean (no hotpath): 2.81s best of 3.
```

**bench-self (Denmark, best of 3, no hotpath overhead):**
```
sequential:  3076 ms
parallel:     302 ms
pipelined:   1599 ms
mmap:        3229 ms
blobreader:  3215 ms
```

### Merge: remaining optimization theories

With indexdata, the merge bottleneck is `rewrite_block` (60%) + `block_builder::take`
(24%) — the actual decode/re-encode work on the ~550 rewritten blocks. These
process ~4.4M elements (most unaffected by the diff) at Denmark scale.

- [ ] **Element-level raw passthrough in rewrite_block** — most elements in
  rewritten blocks are unmodified. Currently they're fully decoded (tags, refs,
  metadata) then re-encoded via BlockBuilder. Copying wire-format bytes directly
  for unaffected elements would skip decode+re-encode for ~99% of elements.
  Largest potential win but most complex — requires BlockBuilder to accept raw
  protobuf fragments, or a separate "patch block" codepath.

- [ ] **Tag/ref Vec allocation churn** — `write_base_dense_node`, `write_base_way`,
  etc. collect tags and refs into fresh Vecs per element (~4.4M small allocations).
  Could reuse thread-local buffers, or pass iterators directly to BlockBuilder
  instead of collecting.

- [ ] **BlockBuilder Vec reuse across blocks** — after `take()`, internal Vecs
  lose capacity via `mem::take`. Re-allocating with `Vec::with_capacity(8000)`
  in `reset()` would avoid grow-from-zero on each of the ~550 rewritten blocks.

- [ ] **Protobuf serialization in `take`** — `write_to_bytes()` uses the
  `protobuf` crate (not the fastest). 673ms across 7408 calls (91µs avg) is
  modest. Would only matter if the other items are addressed first.

## Performance: parallelism

- [ ] `pipeline.rs:13-16` — `READ_AHEAD` / `DECODE_AHEAD` constants should be configurable
  or auto-tuned. Current `DECODE_AHEAD=32` means up to 64-256MB of decoded blocks in flight.

- [ ] **Rayon alternatives for slice-based parallelism** — Wild linker discussion
  ([davidlattimore/wild#1072](https://github.com/davidlattimore/wild/discussions/1072)) surveys
  the landscape. Key options:
  - **paralight** (v0.0.8) — lightweight, targets slice/mut-slice parallelism. Can run on top of
    rayon's thread pool via `RayonThreadPool::new_global` (no extra threads). Has proper
    `try_for_each_init` that inits once per thread (rayon inits once per work item). Only needs
    `&` not `&mut` for the rayon backend. Limitation: no scopes, no graph algorithms, no recursive
    parallelism. Max `u32::MAX` elements.
  - **orx-parallel** — has `using()` API for guaranteed per-thread init. No thread pool yet
    (spawns threads per pipeline), on roadmap. No scopes/graph support.
  - **chili** — low-level, only provides `join`. A rayon fork (`par-iter`) builds par_iter on top
    of it. Uses lazy scheduling (less overhead for fine-grained work).
  - **forte** — experimental, rayon-like API with lazy scheduling. Supports spawn, join, scopes,
    scoped spawns. No par_iter or par_bridge yet.
  - **spindle** — built on rayon, optimised for small tasks. Very early.

  Wild's `thread_local` crate trick is also relevant: wrap per-thread state in
  `thread_local::ThreadLocal` and `.get_or()` inside rayon closures to guarantee one init per
  thread. Simple and works today without switching libraries.

  **Relevance to pbfhogg:** `par_map_reduce` uses rayon's `par_bridge` which has known overhead
  for ordered iteration. `for_each_pipelined` uses a custom 3-stage pipeline that doesn't depend
  on rayon's par_iter at all (it uses `rayon::spawn` for the decode pool). The main rayon
  consumer is merge's `par_bridge` in `classify_blob`. The `thread_local::ThreadLocal` trick
  could replace merge's `map_init(Vec::new, ...)` pattern for per-thread buffer reuse.

## Performance: memory / planet-scale

- [ ] `commands/sort.rs` — reads entire PBF into fully-decoded owned Rust structs (`OwnedNode`,
  `OwnedWay`, `OwnedRelation` with heap-allocated String tags, metadata, refs, members).
  Planet estimate: ~9B nodes × ~140 bytes + ~1B ways × ~312 bytes + ~17M relations × ~500 bytes
  = **~1,400 GB RAM**. Even without metadata, nodes alone require ~430 GB. Osmium has the same
  limitation (in-memory sort, recommends splitting first for large files).

  **Approach A: Blob-level permutation sort (preferred).** Use indexdata (element type +
  min/max ID per blob) to build a permutation of blob offsets. If blobs have non-overlapping
  ID ranges within each type (very common — planet PBFs have this property), the "sort" is
  just rewriting the file with blobs in the correct order via `write_raw`. No decode at all.
  Only blobs with overlapping ID ranges need decode+re-sort+re-encode. For typical planet PBFs
  this means 99.9%+ of blobs are just copied. Memory: O(num_blobs) for the offset array
  (~600K × 40 bytes = ~24 MB for planet). First pass scans indexdata (or falls back to
  decompress+scan for blobs without it), second pass copies/rewrites.

  **Approach B: Streaming overlap detection.** Single streaming pass: read blobs, check if
  each blob's min_id > previous blob's max_id (same type). If yes, pass through raw. If not,
  buffer the overlapping run, decode+sort+re-encode just that run. For already-sorted PBFs
  (Geofabrik, planet.osm.org), this is essentially a verification pass that copies the file.
  Memory: proportional to the longest overlapping run, not the file size.

  **Approach C: Sort-on-read virtual view.** Don't rewrite the file at all. Build a blob-level
  index and provide a `SortedReader` that yields elements in sorted order by seeking blobs in
  the right sequence. Zero disk write, zero extra memory for already-sorted files. Degrades
  for unsorted input (must sort within overlapping blob groups). Most useful if consumers
  only need sorted iteration, not a sorted file on disk.

  **Approach D: External merge sort (conventional).** Three phases: (1) streaming split into
  sharded temp PBF files by type+ID range (~256MB per shard), (2) sort each shard in memory,
  (3) sequential concatenation. Memory: ~2 GB regardless of input size. Most complex, only
  needed if input is heavily unsorted (rare in practice). The existing `PbfWriter`/`BlockBuilder`
  are fully streaming and already support this — no writer changes needed.

## Performance: Linux kernel features for planet-scale I/O

Research notes: `docs/linux-async-io.md`.

Target deployment: nidhogg weekly planet merge on Linux 6.18, planet PBF on erofs.
Nidhogg will use erofs (atomic swap of entire planet data at runtime), so
`Compression::None` PBFs on erofs is the baseline assumption for the optimized path.
The library also needs to work well for the broader OSM ecosystem (standard
zlib-compressed PBFs, any filesystem, any Linux 5.x+), so there are two tiers.

### Tier 1: Generic path (any Linux, zlib PBFs, any filesystem)

The generic path is CPU-bound on zlib compression/decompression. io_uring adds
negligible value here (~30ms syscall savings on 80GB). Focus on page cache hygiene
and kernel-space copy.

- [x] **O_DIRECT for planet-scale writes (write path).** Feature-gated
  `linux-direct-io`. `DirectWriter` uses page-aligned buffer (`AlignedBuffer` via
  `std::alloc`), `O_DIRECT` + `libc::write`, and `ftruncate` for final partial page.
  `FileWriter` enum wraps `BufWriter<File>` or `DirectWriter` — single concrete type
  across all 7 command files, zero-cost when feature is off. Both sync (`to_path_direct`)
  and pipelined (`to_path_pipelined_direct`) constructors. CLI: `--direct-io` on merge.

- [ ] **O_DIRECT for planet-scale reads (read path).** The write path is done; the
  read side still uses standard `File`/`BufReader`/mmap which pollutes the page cache
  with 80GB of input data. Same principle: the read pipeline manages its own buffers
  (`DecompressPool` + `Bytes::from_owner`), so the page cache is pure overhead.
  `BlobReader` and `MmapBlobReader` need aligned-read variants.

- [ ] **`copy_file_range` for blob passthrough in sort/merge.** Blobs can be copied
  between file descriptors entirely in kernel space via `copy_file_range(in_fd,
  &offset, out_fd, NULL, blob_len, 0)`. No userspace buffer, no user/kernel boundary
  crossing. On btrfs/xfs with reflinks, it's metadata-only (instant, zero I/O). For
  planet sort where 99.9% of blobs are passthrough, the kernel shuffles 80GB without
  pbfhogg touching the data. Independent of io_uring — there is no
  `IORING_OP_COPY_FILE_RANGE` opcode in the kernel (verified through 6.18). Call the
  syscall directly. Requires switching `write_raw` from `&[u8]` to accepting
  fd+offset+len for the kernel-copy path.

- [ ] **Large folios for mmap reads.** On 6.14+, file-backed mmap gets transparent
  2MB huge pages automatically. An 80GB mmap'd PBF goes from ~20M TLB entries
  (4KB pages) to ~40K entries (2MB folios). Combined with `MADV_POPULATE_READ`
  (5.14+) to prefault pages ahead, the mmap read path gets substantially faster.
  `MmapBlobReader` could use `MADV_SEQUENTIAL` + `MADV_POPULATE_READ` in chunks
  (e.g. 256MB ahead) for predictable prefaulting without committing all 80GB at once.

### Tier 2: erofs + io_uring (nidhogg, Linux 6.14+, Compression::None)

With erofs + `Compression::None`, zlib is eliminated entirely. erofs handles lz4 in
kernel at ~4 GB/s (SIMD-optimized), `decompress_blob` becomes a no-op, and the
pipeline becomes **I/O-bound**. Now io_uring's batched async writes and registered
buffers actually matter — the writer thread is the bottleneck, not compression.

- [ ] **erofs + uncompressed PBFs (single decompression layer).** Currently double
  decompression: erofs lz4 in kernel → zlib in userspace. With `Compression::None`
  on erofs, the stack becomes: erofs lz4 → raw blob data. On-disk size is comparable
  (erofs lz4 achieves similar ratios to zlib on OSM data). Trade-off: PBF files are
  3-4x larger when copied off erofs, but for a local pipeline where you control
  storage this is the single biggest throughput win. Add `--compression none` flag
  to merge/sort/cat.

- [ ] **io_uring writer thread.** Replace the synchronous `BufWriter` + `write_all`
  writer thread with an io_uring submission loop. Register the output fd
  (`register_files`) + a pool of page-aligned buffers (`register_buffers`) once at
  startup. Compression/framing threads fill registered buffers, send
  `(buf_index, len)` to the I/O thread, which pushes `WriteFixed` SQEs and reaps
  CQEs to recycle buffer indices via a free-list. No async runtime — the `io-uring`
  crate (v0.7.x, tokio-rs org) works synchronously in a dedicated thread.

  Key constraints discovered in research:
  - `WRITEV` does NOT support registered buffers — each buffer needs its own
    `WriteFixed` SQE (no scatter-gather with fixed buffers)
  - Registered buffers are pinned in kernel memory, charged against `RLIMIT_MEMLOCK`
    — must raise the limit
  - Buffer ownership: kernel owns the buffer from SQE submission to CQE completion;
    userspace must not touch it during that window
  - `tokio-uring` is not production-ready and not needed; `io-uring` 0.7.x is the
    right crate (synchronous, no async runtime, 1.6K stars, 17K dependents)
  - SQ polling (`setup_sqpoll`) eliminates `io_uring_enter` syscalls entirely but
    consumes a CPU core — worth benchmarking for the Compression::None case

  This combines naturally with the O_DIRECT item from Tier 1: open the output fd
  with `O_DIRECT` and all writes through registered (page-aligned) buffers bypass
  the page cache automatically.

  For blob passthrough: `copy_file_range(2)` called synchronously between SQE
  batches (no io_uring opcode exists), or read into a registered buffer then
  `WriteFixed` — the latter integrates more cleanly with the ring loop.

### Implementation order

1. ~~**O_DIRECT (Tier 1)** — write path done. Read path next.~~
2. **`copy_file_range` (Tier 1)** — orthogonal, change `write_raw` API. Kernel-space
   blob copy for merge/sort passthrough.
3. **`Compression::None` (Tier 2 prereq)** — CLI flag, trivial in PbfWriter (already
   supported), needed to make the pipeline I/O-bound.
4. **io_uring writer thread (Tier 2)** — only after Compression::None makes writes the
   bottleneck. Registered buffers + WriteFixed + free-list pattern.

## Dependencies

- [ ] CLI-only dependencies (`clap`, `quick-xml`, `serde_json`) are runtime deps of the library
  crate. Library-only users pay the compile cost. Consider a `cli` feature gate or separate
  `pbfhogg-cli` binary crate.
- [ ] `protobuf` crate: currently v3.7 (stepancheg/rust-protobuf, community, approaching EOL).
  Only used for HeaderBlock parsing, blob envelope (BlobHeader/Blob), and write path.
  The hot read path (PrimitiveBlock) now uses the custom wire-format parser in `wire.rs`.
  Migration to v4 or prost would be lower-impact now since the performance-critical path
  no longer depends on it.

## Before crates.io publish

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Verify edition 2024 is intentional — most published crates use 2021 for broader compatibility
- [ ] Add `tests/test.osm.pbf` to version control (generated by `cargo run --example gen_test_pbf`)
- [ ] Make writing program configurable in `build_header()` instead of hardcoded "pbfhogg"
- [ ] Add doc comments to `writer.rs` public API (PbfWriter, Compression)
- [ ] Add doc comments to `block_builder.rs` public API (BlockBuilder, Metadata, MemberData)
- [ ] Add crate-level documentation for write/merge workflows (lib.rs)
- [ ] Publish to crates.io

## GitHub

- [ ] Write GitHub repo description and tags (openstreetmap, pbf, protobuf, osm, rust)
- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Add a CHANGELOG.md before first tagged release

## Website

- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

## Code TODOs

- [ ] `src/indexed.rs:42` — use `relation_ids` field in `IdRanges` filtering.
  Leave as-is until a concrete consumer exists. No current or planned command benefits.
  Implementing `read_relations_and_deps` requires 3+ passes (relations→ways→nodes) and
  recursive member resolution. See git log for full analysis (commit a38c258).

## Merge correctness

Merge is fully validated: 11 unit tests + 4-tool cross-validation (commit a38c258).
pbfhogg matches osmosis and osmconvert exactly; osmium diverges on delete semantics
(version-based vs unconditional). See git log and `scripts/xval-merge.sh` for details.

## Benchmarking

- [ ] Track peak RSS during reads and merges at scale. Denmark for CI, planet for release validation.

## Planned commands

- [ ] `pbfhogg extract --smart` strategy — three passes, complete boundary/multipolygon
  relations (all member ways + their nodes included even if outside the extract region).

- [ ] `pbfhogg add-locations-to-ways` mmap index backends — currently in-memory only
  (HashMap), which limits to country-scale PBFs. Add mmap dense and mmap sparse backends
  for planet-scale processing.

## CLI cross-validation

Commands need cross-validation against osmium-tool (and where applicable osmosis/osmconvert)
on real PBF data, like the merge cross-validation in `scripts/xval-merge.sh`. Run each
command with pbfhogg and osmium on the same input, diff the outputs.

- [x] `merge` — cross-validated against osmium, osmosis, osmconvert (commit a38c258)
- [ ] `sort` — compare `pbfhogg sort` vs `osmium sort` output
- [ ] `cat` — compare `pbfhogg cat` vs `osmium cat` output (with and without type filters)
- [ ] `extract` — compare bbox and polygon extract vs `osmium extract`
- [ ] `derive-changes` — compare OSC output vs `osmium derive-changes`
- [ ] `diff` — compare output vs `osmium diff`
- [ ] `add-locations-to-ways` — compare vs `osmium add-locations-to-ways`
- [ ] `tags-filter` — compare vs `osmium tags-filter`
- [ ] `getid` / `removeid` — compare vs `osmium getid` / `osmium removeid`
- [ ] `check-refs` — compare vs `osmium check-refs`

## Nice to have

- [ ] Consider adding `serde` feature for element serialization
