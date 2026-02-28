# Theoretical Performance Review (pbfhogg V1)

Date: 2026-02-28
Target codebase reviewed: `/home/folk/Programs/pbfhogg`
Output location: `elivagar/.plans/`
Method: Static/theoretical analysis only (no benchmark runs in this pass).

## Scope
This review mirrors the previous box-based review style and focuses on planet-scale behavior and 64 GB RAM-class Linux hosts.

## Box 1: Read-Side Orchestration and API Modes
### Scope
- `src/read/reader.rs`
- `src/read/pipeline.rs`

### Cost Model
- Main pipeline is 3-stage: sequential blob read -> parallel decode/parse -> ordered delivery.
- Throughput bounded by decode pool and consumer callback behavior.
- Memory bounded by read-ahead/decode-ahead plus decoded block size variability.

### Findings
1. `high`: Fixed queue depths can become either under-buffered or memory-heavy depending on dataset/hardware.
   - Anchors: `READ_AHEAD=16`, `DECODE_AHEAD=32` in [/home/folk/Programs/pbfhogg/src/read/pipeline.rs](/home/folk/Programs/pbfhogg/src/read/pipeline.rs:16), [/home/folk/Programs/pbfhogg/src/read/pipeline.rs](/home/folk/Programs/pbfhogg/src/read/pipeline.rs:19)
2. `critical`: `par_map_reduce` full-collect strategy can become non-viable at largest inputs (stores all compressed OSMData blobs before parallel phase).
   - Anchors: [/home/folk/Programs/pbfhogg/src/read/reader.rs](/home/folk/Programs/pbfhogg/src/read/reader.rs:350), [/home/folk/Programs/pbfhogg/src/read/reader.rs](/home/folk/Programs/pbfhogg/src/read/reader.rs:454)
3. `medium`: Default decode thread heuristic (`available_parallelism - 2`) is reasonable but static; optimal split depends on callback workload and storage profile.
   - Anchor: [/home/folk/Programs/pbfhogg/src/read/pipeline.rs](/home/folk/Programs/pbfhogg/src/read/pipeline.rs:80)

### Candidate Changes
1. Add tunables for `READ_AHEAD` and `DECODE_AHEAD`.
2. Add chunked variant of `par_map_reduce` to cap peak memory.
3. Add adaptive decode-thread policy option for consumer-heavy workloads.

### Validation Plan
- Measure queue occupancy, decode idle time, and callback stall time.
- Track peak RSS for `par_map_reduce` on large files.

## Box 2: Blob Decode, Decompression, and Buffer Reuse
### Scope
- `src/read/blob.rs`

### Cost Model
- Dominant operations are zlib/zstd decompression and protobuf parse prep.
- Pooling amortizes allocation churn; decode size guards cap malformed input.

### Findings
1. `high`: Decompression pool uses a single `Mutex<Vec<Vec<u8>>>`; lock contention risk increases with high decode thread counts.
   - Anchor: [/home/folk/Programs/pbfhogg/src/read/blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs:25)
2. `medium`: Strict max blob size checks are correct for safety but can induce abrupt failure behavior on borderline producer variants.
   - Anchors: [/home/folk/Programs/pbfhogg/src/read/blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs:220), [/home/folk/Programs/pbfhogg/src/read/blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs:225)
3. `medium`: Decode path includes multiple decode helpers with copy and zero-copy variants; misuse at callsites can quietly add extra copies.
   - Anchors: [/home/folk/Programs/pbfhogg/src/read/blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs:811), [/home/folk/Programs/pbfhogg/src/read/blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs:819)

### Candidate Changes
1. Consider lock-sharded pool or thread-local recycle bins with periodic global drain.
2. Add explicit telemetry counters for pooled-hit vs fresh-alloc behavior.
3. Tighten internal API defaults toward zero-copy variants in hot paths.

### Validation Plan
- Track lock wait time and pool hit-rate under different decode thread counts.
- Track alloc bytes/op in decode stage.

## Box 3: Wire Parsing and Element View Layer
### Scope
- `src/read/wire.rs`
- `src/read/block.rs`
- `src/read/elements.rs`
- `src/read/dense.rs`

### Cost Model
- Zero-copy borrow model minimizes per-element allocation.
- Cost scales with varint decode density, tag iteration, and repeated field traversal.

### Findings
1. `high`: Full parse cost is unavoidable for many command paths; selective/ID-only parse exists in some places but not generalized.
   - Anchors: [/home/folk/Programs/pbfhogg/src/blob_index.rs](/home/folk/Programs/pbfhogg/src/blob_index.rs:142), [/home/folk/Programs/pbfhogg/src/read/indexed.rs](/home/folk/Programs/pbfhogg/src/read/indexed.rs:197)
2. `medium`: Repeated conversion from borrowed tags/refs to owned structures in higher layers can shift pressure from parser to allocator.
   - Anchor examples in commands: [/home/folk/Programs/pbfhogg/src/commands/sort.rs](/home/folk/Programs/pbfhogg/src/commands/sort.rs:519)
3. `medium`: Mixed block-type handling is robust but can force slower branches in unsorted/mixed inputs.
   - Anchor: [/home/folk/Programs/pbfhogg/src/read/block.rs](/home/folk/Programs/pbfhogg/src/read/block.rs:224)

### Candidate Changes
1. Expand lightweight scanners for command workflows that only need IDs/types.
2. Add specialized APIs that return prefiltered projections (e.g., IDs-only iterator).
3. Add counters for per-element conversion ownership pressure in command code.

### Validation Plan
- Compare full-parse vs selective-scan CPU for representative commands.
- Profile allocation hotspots by command and element type.

## Box 4: Indexing, Blob Filtering, and Mmap Paths
### Scope
- `src/blob_index.rs`
- `src/read/indexed.rs`
- `src/read/mmap_blob.rs`

### Cost Model
- Blob index enables cheap type/range decisions; benefit depends on indexdata availability.
- Mmap path trades page-cache behavior and memcpy strategy against refcount contention.

### Findings
1. `high`: Blob filtering effectiveness strongly depends on indexdata presence; non-indexed files still pay full decode for filtering.
   - Anchors: [/home/folk/Programs/pbfhogg/src/read/reader.rs](/home/folk/Programs/pbfhogg/src/read/reader.rs:58), [/home/folk/Programs/pbfhogg/src/read/pipeline.rs](/home/folk/Programs/pbfhogg/src/read/pipeline.rs:110)
2. `medium`: `MmapBlobReader` deliberately copies per-blob payload (`Bytes::copy_from_slice`), reducing atomic contention but increasing memcpy bandwidth demand.
   - Anchors: [/home/folk/Programs/pbfhogg/src/read/mmap_blob.rs](/home/folk/Programs/pbfhogg/src/read/mmap_blob.rs:139), [/home/folk/Programs/pbfhogg/src/read/mmap_blob.rs](/home/folk/Programs/pbfhogg/src/read/mmap_blob.rs:384)
3. `medium`: `IndexedReader` uses `BTreeSet`/range checks and full block decode for some range updates, which may become expensive for repeated large scans.
   - Anchors: [/home/folk/Programs/pbfhogg/src/read/indexed.rs](/home/folk/Programs/pbfhogg/src/read/indexed.rs:197)

### Candidate Changes
1. Provide utility to re-index legacy PBFs up-front when filtered workflows are expected.
2. Add mmap mode guidance/tunables by storage type and workload.
3. Revisit `IndexedReader` data-structure strategy for very large repeated ID queries.

### Validation Plan
- Compare indexed vs non-indexed filtered runs.
- Measure memcpy bandwidth and total decode time for mmap vs non-mmap modes.

## Box 5: Writer Pipeline, Framing, and Compression
### Scope
- `src/write/writer.rs`
- `src/write/file_writer.rs`

### Cost Model
- Pipelined writer dispatches per-block framing/compress tasks in rayon and reorders on a writer thread.
- Costs: input copy, compression CPU, reorder buffering, output I/O.

### Findings
1. `high`: Pipelined write path clones block bytes (`to_vec`) before dispatch, creating extra copy bandwidth and transient memory.
   - Anchor: [/home/folk/Programs/pbfhogg/src/write/writer.rs](/home/folk/Programs/pbfhogg/src/write/writer.rs:332)
2. `medium`: `WRITE_AHEAD=32` is static; too small can underlap compression, too large can increase memory pressure.
   - Anchor: [/home/folk/Programs/pbfhogg/src/write/writer.rs](/home/folk/Programs/pbfhogg/src/write/writer.rs:33)
3. `medium`: Compression mode differences (none/zlib/zstd) materially change bottleneck placement, but pipeline policy is mostly fixed.
   - Anchors: [/home/folk/Programs/pbfhogg/src/write/writer.rs](/home/folk/Programs/pbfhogg/src/write/writer.rs:44), [/home/folk/Programs/pbfhogg/src/write/writer.rs](/home/folk/Programs/pbfhogg/src/write/writer.rs:722)

### Candidate Changes
1. Introduce owned-buffer API in write path to avoid avoidable copy on rayon dispatch.
2. Make write queue depth tunable.
3. Add adaptive pipeline policy by compression mode.

### Validation Plan
- Measure copy bytes/sec and peak RSS in pipelined write mode.
- Profile compressor utilization vs writer-thread idle time.

## Box 6: BlockBuilder and Encoding
### Scope
- `src/write/block_builder.rs`

### Cost Model
- One block type per `PrimitiveBlock`, max 8000 entities.
- Hot costs: string table interning, delta packing, repeated protobuf field encoding.

### Findings
1. `high`: String interning and tag-heavy encode path likely dominate CPU for heavily attributed datasets.
   - Anchors: [/home/folk/Programs/pbfhogg/src/write/block_builder.rs](/home/folk/Programs/pbfhogg/src/write/block_builder.rs:22), [/home/folk/Programs/pbfhogg/src/write/block_builder.rs](/home/folk/Programs/pbfhogg/src/write/block_builder.rs:74)
2. `medium`: Fixed max entities/block (`8000`) is compatible but may not be optimal for all compression and cache behaviors.
   - Anchor: [/home/folk/Programs/pbfhogg/src/write/block_builder.rs](/home/folk/Programs/pbfhogg/src/write/block_builder.rs:22)
3. `medium`: Dense metadata mixed-presence handling is correctness-focused but adds branch and vector maintenance overhead.
   - Anchors: [/home/folk/Programs/pbfhogg/src/write/block_builder.rs](/home/folk/Programs/pbfhogg/src/write/block_builder.rs:441), [/home/folk/Programs/pbfhogg/src/write/block_builder.rs](/home/folk/Programs/pbfhogg/src/write/block_builder.rs:502)

### Candidate Changes
1. Add mode-specific tuning for block target size/entity cap.
2. Add pre-seed/stringtable hints for known tag-heavy transformations.
3. Add per-block encode telemetry (strings added, tags encoded, bytes out).

### Validation Plan
- Correlate tags-per-entity and unique-string count with encode time.
- Evaluate different block sizing policies.

## Box 7: Linux Direct I/O and io_uring Path
### Scope
- `src/read/direct_reader.rs`
- `src/write/direct_writer.rs`
- `src/write/uring_writer.rs`
- integration in `src/write/writer.rs`

### Cost Model
- O_DIRECT removes page-cache effects; io_uring path uses registered aligned buffers and async submission/reap.
- Gains are workload/storage dependent; complexity and operational constraints are higher.

### Findings
1. `high`: io_uring path adds significant operational complexity and sensitivity to kernel/setup (`memlock`, sqpoll behavior, queue depth).
   - Anchors: [/home/folk/Programs/pbfhogg/src/write/uring_writer.rs](/home/folk/Programs/pbfhogg/src/write/uring_writer.rs:55), [/home/folk/Programs/pbfhogg/src/write/uring_writer.rs](/home/folk/Programs/pbfhogg/src/write/uring_writer.rs:620)
2. `medium`: Page-aligned padding and fixed buffer protocol can increase write amplification in tail-heavy workloads.
   - Anchors: [/home/folk/Programs/pbfhogg/src/write/uring_writer.rs](/home/folk/Programs/pbfhogg/src/write/uring_writer.rs:230), [/home/folk/Programs/pbfhogg/src/write/uring_writer.rs](/home/folk/Programs/pbfhogg/src/write/uring_writer.rs:526)
3. `medium`: `copy_file_range` passthrough optimizes raw copy paths but adds compatibility branching and fallback complexity.
   - Anchors: [/home/folk/Programs/pbfhogg/src/write/writer.rs](/home/folk/Programs/pbfhogg/src/write/writer.rs:511), [/home/folk/Programs/pbfhogg/src/write/writer.rs](/home/folk/Programs/pbfhogg/src/write/writer.rs:610)

### Candidate Changes
1. Add runtime self-check diagnostics to classify expected benefit/risk before enabling io_uring/direct paths.
2. Add clearer mode fallback telemetry when fast-path preconditions are not met.
3. Add mode-specific queue and buffer tuning knobs.

### Validation Plan
- Compare throughput and CPU under buffered/direct/io_uring modes by compression mode.
- Track short-write/read and fallback incidence.

## Box 8: Command Layer Algorithms (`commands/*`)
### Scope
- `src/commands/*.rs`

### Cost Model
- Commands compose read+transform+write with multiple passes and ID-set accumulation.
- Peak memory depends heavily on chosen strategy (`simple`/`complete`/`smart`, merge overlap patterns, refs inclusion).

### Findings
1. `high`: Multi-pass commands (`extract`, `tags_filter`, `merge`) can accumulate very large ID structures and batch buffers; memory behavior can vary sharply by region/data skew.
   - Anchors: [/home/folk/Programs/pbfhogg/src/commands/extract.rs](/home/folk/Programs/pbfhogg/src/commands/extract.rs:900), [/home/folk/Programs/pbfhogg/src/commands/tags_filter.rs](/home/folk/Programs/pbfhogg/src/commands/tags_filter.rs:565), [/home/folk/Programs/pbfhogg/src/commands/merge.rs](/home/folk/Programs/pbfhogg/src/commands/merge.rs:1011)
2. `high`: Merge classification and rewrite/passthrough split is sophisticated but sensitive to false positives and overlap distribution; worst-case can collapse toward rewrite-heavy behavior.
   - Anchors: [/home/folk/Programs/pbfhogg/src/commands/merge.rs](/home/folk/Programs/pbfhogg/src/commands/merge.rs:838), [/home/folk/Programs/pbfhogg/src/commands/merge.rs](/home/folk/Programs/pbfhogg/src/commands/merge.rs:874)
3. `medium`: Batch size mostly fixed at 64 in command flows; one-size policy may underperform across different machines and datasets.
   - Anchors: [/home/folk/Programs/pbfhogg/src/commands/cat.rs](/home/folk/Programs/pbfhogg/src/commands/cat.rs:172), [/home/folk/Programs/pbfhogg/src/commands/tags_count.rs](/home/folk/Programs/pbfhogg/src/commands/tags_count.rs:13)

### Candidate Changes
1. Add command-level adaptive batching (based on bytes and observed stage latency).
2. Add strategy auto-selection guidance based on early scan statistics.
3. Add command telemetry schema (ID-set sizes, pass-level RSS, rewrite ratio).

### Validation Plan
- Collect per-pass memory and timing for `extract`, `tags_filter`, `merge`.
- Track rewrite/passthrough ratio distribution in real diff scenarios.

## Cross-Box Priority Ranking
1. `critical`: `par_map_reduce` full-collect memory risk on very large inputs (Box 1).
2. `high`: Writer pipelined copy overhead from `to_vec` dispatch (Box 5).
3. `high`: Command multi-pass ID accumulation and rewrite-heavy worst cases (Box 8).
4. `high`: Queue-depth/static policy sensitivity across read/write pipelines (Boxes 1 and 5).
5. `high`: Blob-filter dependence on indexdata availability (Box 4).

## Suggested Roadmap
1. Quick wins
   - Add tunables/telemetry for queue depths, batch sizes, decode threads.
   - Add warnings and docs for `par_map_reduce` memory envelope.
   - Add command-level memory counters.
2. Medium refactors
   - Add chunked `par_map_reduce` mode.
   - Add owned-buffer write API to remove avoidable `to_vec` copies.
   - Improve command adaptive strategy selection.
3. High-risk/high-reward
   - Rework command internal dataflow to reduce pass count or peak ID-set residency.
   - Further unify selective scan APIs to avoid full parse where unnecessary.
