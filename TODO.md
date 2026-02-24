# pbfhogg TODO

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` — it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    scripts/test.sh -- --ignored

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
