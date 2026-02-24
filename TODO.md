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
planet updates. **Findings 1 and 3 are the top priorities** — they affect
every pipelined read, which is the only read path the consumers use.
Merge has not been profiled yet.

Profiled with `hotpath-alloc` on two commands: `check-refs` and `tags-count`.
Run with `scripts/run-hotpath-alloc.sh <command> <file>`. Use `--min-count` for
tags-count to keep stdout manageable.

### Finding 1: `decode_blob` allocation churn — 10.2 GB for a 483 MB file

`decode_blob` is called 7,396 times (one per blob), averaging 1.4 MB per call
(P95: 5.2 MB, P99: 5.9 MB). Total cumulative allocations: **10.2 GB** — a 21x
amplification over the compressed file size. These are `Vec<u8>` decompression
buffers that are allocated and freed per blob.

**Planet extrapolation: ~1.7 TB of cumulative alloc/dealloc through decode_blob.**

The allocations are short-lived (10 GB allocated = 10 GB deallocated), so RSS stays
reasonable, but the allocation throughput hammers the allocator.

- [x] **Reuse decompression buffers in merge.** Added `decompress_blob_data_into()`
  with caller-provided buffer. Merge's `classify_blob` uses `map_init(Vec::new, ...)`
  for per-thread buffer reuse via rayon. Passthrough blobs (91%) reuse the buffer;
  only MayOverlap/Fallback (9%) take ownership via `mem::take`. Also fixed double-copy:
  `parse_primitive_block_from_bytes(&raw)` → `parse_primitive_block_from_bytes_owned(&Bytes::from(raw))`.
  Result: `decompress_blob_data_from_bytes` dropped from hotpath report entirely,
  process alloc -100 MB, main thread alloc -200 MB.
- [ ] **Reuse decompression buffers in decode_blob (read path).** `decode_blob` wraps
  the decompressed Vec as `Bytes::from(decoded)` for zero-copy protobuf parsing.
  The parsed PrimitiveBlock holds references into the Bytes, so the buffer cannot be
  reclaimed until the message is dropped. Buffer reuse here requires either switching
  to `parse_from_bytes` (copying parse, creates many small String allocations — likely
  worse) or a custom allocator like jemalloc that pools large alloc/free efficiently.

### Finding 2: `tags_count` allocates 2.5 GB on the main thread (Denmark)

The `HashMap<(String, String), u64>` for tag counting uses `k.to_string(),
v.to_string()` on every tag of every element (`tags_count.rs:51-55`). For Denmark's
59M elements (~118M tags), this creates ~236M temporary String allocations — most
immediately dropped when the HashMap entry already exists. The HashMap itself retains
2.5 GB for 3.3M distinct entries.

**Planet extrapolation: ~40-50 GB HashMap + ~40B temporary String allocations.**

- [ ] **Intern tag strings in tags_count.** Replace `HashMap<(String, String), u64>`
  with a string interner (e.g. `lasso` crate or manual arena). Look up the interned
  key first; only allocate a new String if the key is truly new. This eliminates
  ~99% of the temporary String allocations (most tags are repeats).
- [ ] **Avoid Box<dyn Iterator> in tags_count.** Line 44 uses `Box::new(dn.tags())`
  for dynamic dispatch on every element. Use a match-with-inline-body or a macro
  instead. Minor allocation but called 59M times.

### Finding 3: main thread is the bottleneck in pipelined reads

In both check-refs and tags-count, the main thread (element consumer) is at
99-100% CPU while pipeline worker threads sit at 4-7% CPU. The pipeline is
consumer-bound, not producer-bound. `for_each_pipelined` delivers decoded blocks
faster than the main thread can process elements.

This means optimizing the consumer closure matters more than optimizing decode
throughput for pipelined reads. For `par_map_reduce` (which is fully parallel),
`decode_blob` is the bottleneck instead.

### Finding 4: `block::new` is allocation-free and cheap

`PrimitiveBlock::new()` (stringtable validation) allocates 0 bytes and takes
P50=3.8µs, P99=141µs, total 75ms (1.1% of wall time). Not a target.

### Finding 5: missing hotpath instrumentation

Only `check_refs` is instrumented among the commands. Add `#[hotpath::measure]`
to the main function in each command file for per-command visibility:

- [ ] `tags_count.rs` — `tags_count()`
- [ ] `sort.rs` — `sort()`
- [ ] `cat.rs` — `cat()`
- [ ] `merge.rs` — `merge()`
- [ ] `extract.rs` — `extract()`
- [ ] `derive_changes.rs` — `derive_changes()`
- [ ] `diff.rs` — `diff()`
- [ ] `getid.rs` — `getid()`
- [ ] `tags_filter.rs` — `tags_filter()`
- [ ] `add_locations_to_ways.rs` — `add_locations_to_ways()`

### Raw data

**check-refs (Denmark, 52M nodes, 6.6M ways, 46K relations):**
```
Wall: 6.73s, RSS: 437.6 MB
decode_blob:          7396 calls, 8.81s total (130%), 10.2 GB alloc
for_each_pipelined:   1 call, 6.72s (99.83%), 262.4 MB alloc
block::new:           7396 calls, 70.87ms (1.05%), 0 B alloc
Main thread: 99% CPU. Workers: 4-5% CPU each (5 threads).
Process: 10.0 GB alloc, 10.0 GB dealloc.
```

**tags-count (Denmark, same file):**
```
Wall: 8.11s, RSS: 1.1 GB
decode_blob:          7396 calls, 11.61s total (143%), 10.2 GB alloc
for_each_pipelined:   1 call, 7.19s (88.72%), 2.5 GB alloc
block::new:           7396 calls, 70.44ms (0.86%), 0 B alloc
Main thread: 100% CPU, 2.5 GB alloc, 10.6 GB dealloc (sole consumer).
No worker threads visible (finished before report).
Process: 2.6 GB alloc, 10.6 GB dealloc.
```

**merge (Denmark base + 1 OSC diff, 630/7396 blobs rewritten):**
```
BEFORE (buffer reuse):
Wall: 8.63s, RSS: 97.6 MB
write_blob:                    632 calls, 5.32s (61.70%), 434.2 MB alloc
decompress_blob_data_from_bytes: 7384 calls, 2.90s (33.64%), 1.6 GB alloc
block_builder::take:           7407 calls, 692.85ms (8.02%), 266.1 MB alloc
Main thread: 99% CPU. Process: 7.7 GB alloc, 7.7 GB dealloc.

AFTER (buffer reuse + double-copy fix):
Wall: 8.57s, RSS: 94.5 MB
write_blob:                    632 calls, 5.32s (62.05%), 434.2 MB alloc
block_builder::take:           7407 calls, 683ms (7.97%), 266.1 MB alloc
decompress_blob_data_from_bytes: GONE from report (buffer reuse)
Main thread: 99% CPU, alloc 5.5 GB (was 5.7 GB). Process: 7.6 GB alloc (was 7.7 GB).
```

### Finding 6: merge spends 62% of time compressing rewritten blobs

`write_blob` (zlib compression) takes 5.32s for only 632 blobs. The other 6,766
are raw passthrough (no decode, no re-encode). Compression is the bottleneck.

- [ ] **Parallel compression in merge.** Rewritten blobs are independent — compress
  them on a rayon thread pool instead of sequentially on the main thread. This is
  the biggest single win for merge.

### Finding 7: merge decompresses all blobs for ID range scanning

`decompress_blob_data_from_bytes` was called 7,384 times (33.6% of wall time,
1.6 GB alloc) to scan each blob's ID ranges and decide passthrough vs rewrite.

**Partially addressed:** Buffer reuse via `decompress_blob_data_into` eliminates
allocation churn for the 6,766 passthrough blobs. The decompression CPU work still
happens but the allocation cost is gone.

- [ ] **Skip decompression entirely for passthrough blobs in merge.** If blob
  headers or a pre-scan index can determine ID ranges without decompression,
  6,766 of 7,384 decompressions become unnecessary. This would save the CPU
  time (~2.9s Denmark, ~8 min planet) not just the allocations.

## Performance: parallelism

- [ ] `pipeline.rs:13-16` — `READ_AHEAD` / `DECODE_AHEAD` constants should be configurable
  or auto-tuned. Current `DECODE_AHEAD=32` means up to 64-256MB of decoded blocks in flight.

## Performance: parsing hot paths

- [ ] **ID-only scan mode — probably not worth it.** The original idea was a lightweight
  protobuf parser that extracts only element IDs, skipping stringtable, tags, coordinates,
  refs, and metadata. Investigation (Feb 2025) found:
  (1) The main supposed consumer, `check_refs`, actually needs way `refs()` and relation
  `members()` — not just IDs. So a pure ID scan doesn't help it.
  (2) The only true ID-only consumer is `IndexedReader::update_element_id_ranges()`, which
  runs once per session and is not a hot path.
  (3) Decompression (zlib/zstd) is ~60% of total read time and unavoidable — even a perfect
  scan that skips ALL protobuf parsing would only save ~35-40% of the remaining ~40%.
  (4) A custom wire-format parser for 5 message types is ~200-400 lines that must stay in
  sync with the proto schema — significant maintenance burden for two non-critical consumers.
  **Not yet benchmarked** — these estimates are based on code analysis, not profiling. If
  profiling shows protobuf parsing is a larger fraction than estimated, revisit.
- [ ] **Selective parse for check_refs** — skip stringtable, tags, coordinates, and metadata
  but keep IDs + way refs + relation member IDs/types. Unlike the ID-only scan above, this
  targets fields that check_refs actually needs. A planet check_refs must decompress+parse
  ~2.5M blocks; skipping stringtable (~20-40% of block), coordinates (~30% for dense nodes),
  tags, and metadata could meaningfully reduce the protobuf parsing cost. Implementation
  would require custom wire-format parsing (same maintenance concern as above) or a two-tier
  protobuf parse where certain fields are conditionally skipped. **Not yet benchmarked.**

## Performance: memory / planet-scale

- [ ] `commands/sort.rs` — reads entire PBF into fully-decoded owned Rust structs (`OwnedNode`,
  `OwnedWay`, `OwnedRelation` with heap-allocated String tags, metadata, refs, members).
  Planet estimate: ~9B nodes × ~140 bytes + ~1B ways × ~312 bytes + ~17M relations × ~500 bytes
  = **~1,400 GB RAM**. Even without metadata, nodes alone require ~430 GB. Osmium has the same
  limitation (in-memory sort, recommends splitting first for large files).
  **Solution: external merge sort in 3 phases:**
  Phase 1 — Partitioned streaming split: read blob-by-blob, decode, classify by type, write
  into sharded temp PBF files by type+ID range (~256MB per shard). Memory: one decoded block
  at a time (~few MB) + K open file handles.
  Phase 2 — Sort each shard: each shard fits in memory (~1-2 GB decoded), sort with existing
  `sort_by_key`, write back. For mostly-sorted input (typical: planet downloads are pre-sorted),
  most shards may already be in order — verify with O(n) scan of block min/max IDs.
  Phase 3 — Sequential concatenation: shards are non-overlapping ID ranges, so writing is just
  sequential concatenation per type (nodes shards in order, then ways, then relations) through
  `BlockBuilder` + `PbfWriter`. Memory: one `BlockBuilder` buffer (~few hundred KB).
  **Optimization: block-level raw passthrough** — for blobs already in correct position with
  non-overlapping ID ranges vs neighbors, use `PbfWriter::write_raw()` to copy compressed bytes
  directly without decode/re-encode. Only decode+re-sort blobs with overlapping ranges.
  **Total memory: ~2 GB regardless of input size.**
  Note: the existing `PbfWriter`/`BlockBuilder` are fully streaming and already support this —
  no writer changes needed.
- [ ] `dense.rs` — Lazy `DenseNodeInfo` decoding — **probably not worth it.** Investigation
  (Feb 2025) found the premise was wrong: only 1 `DenseNode` is alive at any time (iterator
  yields them one at a time, no production code collects them), so peak memory is ~136 bytes
  total, not ~136 × 8000 per block. The DenseInfo packed arrays are already fully decoded by
  protobuf deserialization — making `DenseNodeInfo` lazy only avoids reading 4-5 cached values
  per node (~5-10 ns). 10 of 16 call sites need `.info()`. Overall speedup: ~0.5-1%.
  Any lazy approach requires breaking API change or dual iterators. See `DenseNode` doc comment
  in dense.rs for full rationale. **Not yet benchmarked.**

## Dependencies

- [ ] CLI-only dependencies (`clap`, `quick-xml`, `serde_json`) are runtime deps of the library
  crate. Library-only users pay the compile cost. Consider a `cli` feature gate or separate
  `pbfhogg-cli` binary crate.

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
