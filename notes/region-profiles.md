# Cross-region profiling

Host: plantasjen — AMD Ryzen 9 5900X 12c/24t, 32 GB DDR4 (30 GB available), Samsung 970 EVO Plus NVMe.
Build: fat LTO, zlib-ng, `--features hotpath` / `--features hotpath-alloc`.
Baseline commit for all region data (except where noted): `aed93e0`.
Script: `scripts/profile-region.sh <name> <pbf> <osc>`

## Region rationale

Denmark and Germany are Central European, node-dominated, moderate-tag-density —
they cover the same code paths. These regions target the gaps.

- **Japan (2.4 GB)** — tagged nodes + CJK string table + urban density. Japanese
  addressing puts many tags on nodes; CJK characters create high FxHashMap miss
  rates in StringTable; Tokyo metro exercises dense urban mapping.
- **Norway (1.4 GB)** — coastline/fjord complexity + long ways. World's most
  complex coastline: ways with 200-500+ node refs, island multipolygon relations,
  archipelago relations with many outer rings.
- **Switzerland (524 MB)** — multilingual string table. 4 official languages mean
  4-5 name variants per feature → 4-5× string table entries. Complex alpine
  multipolygon relations and deeply nested admin boundaries.
- **Greater London (122 MB)** — urban relation density. TfL transit route
  relations with 30-100 stops each. Dense urban ways without CJK confound —
  isolates urban density from string table stress.
- **Malta (8 MB)** — edge cases + tiny dataset. ~100 blobs total tests batch
  boundary conditions, pipeline setup/teardown overhead ratios, writer flush
  with few blobs.

### Coverage matrix

| Pipeline component | DK | DE | JP | NO | CH | London | MT |
|--------------------|----|----|----|----|----|----|------|
| Dense node tag scanning | . | . | **X** | . | . | . | . |
| CJK / multilingual string table | . | . | **X** | . | **X** | . | . |
| StringTable FxHashMap miss rate | . | . | **X** | . | **X** | . | . |
| Long way ref decode/re-encode | . | . | . | **X** | . | . | . |
| Many-member relations | . | . | . | **X** | **X** | **X** | . |
| Transit route relations | . | . | . | . | . | **X** | . |
| BlockBuilder way alloc pressure | . | . | . | . | . | **X** | . |
| Small-file edge cases | . | . | . | . | . | . | **X** |
| Mid-scale rewrite fraction | . | **X** | **X** | . | . | . | . |
| Merge overhead ratios | . | . | . | . | . | . | **X** |
| Urban tagged node density | . | . | **X** | . | . | **X** | . |
| Blob compression ratio (CJK) | . | . | **X** | . | . | . | . |

**X** = primary stress target, **.** = incidental coverage

## Datasets

| Region | PBF (MB) | Indexed (MB) | Elements | Blobs | Diff | Changes | Rewrite % |
|--------|----------|--------------|----------|-------|------|---------|-----------|
| Denmark | 465 | 465 | 59.1M | 7,396 | 300 KB | 9K | 8.5% |
| Germany | 4,500 | 4,500 | ~500M | 62,461 | 5.9 MB | 146K | 18.4% |
| Malta | 8 | 8 | 918K | 117 | 4 KB | 47 | 8.5% |
| Gr. London | 122 | 123 | 12.6M | 1,575 | 173 KB | 3.3K | 16.8% |
| Switzerland | 524 | 528 | 61.5M | 7,693 | 1.2 MB | 23K | 18.2% |
| Norway | 1,361 | 1,369 | 220.5M | 27,568 | 1.0 MB | 20K | 1.3% |
| Japan | 2,372 | 2,389 | 344.3M | 43,035 | 4.3 MB | 230K | 8.2% |

## Read baseline: tags-count (pipelined read)

Exercises `ElementReader::for_each_pipelined` — same path as elivagar/nidhogg
ingest. Measures decompress + parse + element iteration + tag HashMap inserts.

| Region | Wall | decompress_blob | decompress avg | block::new | RSS |
|--------|------|-----------------|----------------|------------|-----|
| Denmark | 8.30s | 2.77s (33%) | 374 μs | 103ms | 616 MB |
| Malta | 70ms | 55ms (79%) | 466 μs | 1.9ms | 23 MB |
| Gr. London | 1.94s | 728ms (38%) | 462 μs | 39ms | 194 MB |
| Switzerland | 4.17s | 2.98s (72%) | 388 μs | 117ms | 553 MB |
| Norway | 13.54s | 6.59s (49%) | 239 μs | 149ms | 1.3 GB |
| Japan | 21.47s | 13.10s (61%) | 304 μs | 388ms | 2.2 GB |

**Findings:**
- **RSS scales with element count** — Norway 1.3 GB for 220M, Japan 2.2 GB for
  344M. The tags-count HashMap holds all distinct tag values in memory.
- **decompress avg**: Norway cheapest (239 μs) — many small blobs with simple
  tagless nodes. Malta most expensive (466 μs) despite tiny file — fewer blobs
  means each one is relatively dense with tagged elements.

## Read baseline: check-refs (pipelined read, lightweight)

| Region | Wall | decompress_blob | RSS |
|--------|------|-----------------|-----|
| Denmark | 6.94s | 2.49s (36%) | 125 MB |
| Malta | 111ms | 53ms (48%) | 23 MB |
| Gr. London | 1.68s | 698ms (42%) | 50 MB |
| Switzerland | 8.56s | 2.75s (32%) | 498 MB |
| Norway | 20.34s | 6.26s (31%) | 821 MB |
| Japan | 38.80s | 11.95s (31%) | 1.7 GB |

**Findings:**
- **check-refs RSS is much lower than tags-count** — only stores ref sets, not
  tag strings. But still scales with element count (Japan 1.7 GB).
- **Norway check-refs is 1.5x tags-count wall time** (20s vs 14s) despite doing
  less work per element. The extra time is in building/querying the reference
  lookup structure across 220M elements.

## Decode + write: cat --type (zlib, pipelined writer)

Full element decode through BlockBuilder + PbfWriter. Exercises string table
interning, dense node packing, way ref encode, relation member encode.

| Region | Wall | frame_blob | take | add_node | add_way | add_relation | RSS |
|--------|------|------------|------|----------|---------|--------------|-----|
| Denmark | 42s | 24.0s (57%) | 3.46s (8%) | 2.27s (5%) | 1.45s (4%) | 25ms | 19 MB |
| Malta | 719ms | 423ms (59%) | 60ms (8%) | 21ms (3%) | 30ms (4%) | 1.5ms | 14 MB |
| Gr. London | 11.28s | 6.70s (59%) | 982ms (9%) | 316ms (3%) | 678ms (6%) | 26ms | 29 MB |
| Switzerland | 43.4s | 25.5s (59%) | 3.73s (9%) | 1.49s (3%) | 1.47s (3%) | 82ms | 32 MB |
| Norway | 110s | 57.2s (52%) | 10.7s (10%) | 4.51s (4%) | 2.68s (2%) | 227ms | 28 MB |
| Japan | 208s | 116.2s (56%) | 18.2s (9%) | 6.38s (3%) | 8.81s (4%) | 123ms | 20 MB |

## Per-element costs (from cat --type)

These are per-call averages from the decode+write path. Shows how element
complexity varies by region (tag density, ref counts, member counts).

| Region | Nodes | add_node (ns) | Ways | add_way (ns) | Relations | add_relation (ns) |
|--------|-------|---------------|------|--------------|-----------|-------------------|
| Denmark | 52.5M | 43 | 6.6M | 219 | 46K | 544 |
| Malta | 772K | 27 | 145K | 210 | 2.1K | 719 |
| Gr. London | 10.5M | 30 | 2.1M | 325 | 31K | 836 |
| Switzerland | 55.2M | 26 | 6.2M | 237 | 139K | 591 |
| Norway | 207.7M | 21 | 12.0M | 222 | 777K | 292 |
| Japan | 301.1M | 21 | 42.9M | 205 | 217K | 564 |

**Findings:**
- **add_node**: Norway and Japan tied at 21ns — both have massive tagless node
  populations (coastline/mountains). Switzerland/Malta (26-27ns) → London (30ns)
  → Denmark (43ns, older build?). Denmark number is from earlier profiling run.
- **add_way**: Japan cheapest at 205ns despite having the MOST ways (42.9M).
  London is 59% more expensive (325ns) — dense urban tagging. Japan's CJK
  strings don't appear to slow down way encoding.
  Norway/Denmark/Malta cluster around 210-222ns.
- **add_relation**: London most expensive at 836ns (transit route relations with
  many members). Norway cheapest at 292ns despite having the MOST relations
  (777K) — they're predominantly small/simple. Japan 564ns is mid-range,
  similar to Denmark.

## Merge: indexdata + Compression::None (nidhogg production path, commit aed93e0)

| Region | Wall | rewrite_block | classify_blob | take | read_raw_frame | RSS |
|--------|------|---------------|---------------|------|----------------|-----|
| Denmark | 1.90s | 936ms (49%) | 609ms (32%) | 597ms (31%) | 85ms (4%) | 85 MB |
| Germany | 52.3s | 16.4s (31%) | 11.7s (22%) | — | — | 338 MB |
| Malta | 48ms | 19ms (39%) | — | 13ms (26%) | 5ms (10%) | 27 MB |
| Gr. London | 1.89s | 445ms (24%) | 312ms (17%) | 305ms (16%) | 39ms (2%) | 85 MB |
| Switzerland | 8.38s | 1.91s (23%) | 1.43s (17%) | 1.28s (15%) | 99ms (1%) | 121 MB |
| Norway | 9.13s | 654ms (7%) | 400ms (4%) | 444ms (5%) | 230ms (3%) | 80 MB |
| Japan | 24.4s | 4.52s (19%) | 2.63s (11%) | 2.59s (11%) | 421ms (2%) | 224 MB |

**Key findings:**
- **Norway merge is I/O-dominated at 1.3% rewrite:** rewrite_block + classify_blob
  + take = ~1.5s, but wall time is 9.13s. The remaining ~7.6s is passthrough blob
  I/O (27K blobs scanned + written sequentially). This path is NOT instrumented
  in hotpath. At low rewrite fractions, the merge bottleneck shifts from
  rewrite_block to passthrough I/O.
- **Japan at 8.2% rewrite:** instrumented work = ~10.2s of 24.4s. Still ~14s in
  passthrough I/O (39K passthrough blobs). Japan confirms the passthrough I/O
  dominance pattern seen in Norway — any dataset with 30K+ passthrough blobs will
  spend significant time in uninstrumented I/O.

## Merge: indexdata + zlib (commit aed93e0, Denmark† at b750e60)

† Denmark re-measured at commit `b750e60` after passthrough I/O
optimizations (eliminated blob_bytes duplication, write_raw_owned, direct
&[u8] decode). Improved from 5.16s to 3.36s (-35%). Other regions were
profiled at commit `aed93e0` (before these optimizations); expect similar
improvements on passthrough-dominated regions (Norway, Japan).

| Region | Wall | rewrite_block | classify_blob | frame_blob | RSS |
|--------|------|---------------|---------------|------------|-----|
| Denmark† | 3.36s | 592ms (18%) | 607ms (18%) | 6.19s (184%) | 74 MB |
| Germany | 49.9s | 17.7s (36%) | 11.7s (23%) | 109.8s (220%) | 374 MB |
| Malta | 58ms | 17ms (30%) | — | 13ms (23%) | 34 MB |
| Gr. London | 1.54s | 473ms (31%) | 321ms (21%) | 2.77s (181%) | 94 MB |
| Switzerland | 6.83s | 2.02s (30%) | 1.44s (21%) | 12.3s (180%) | 130 MB |
| Norway | 8.68s | 694ms (8%) | 397ms (5%) | 4.29s (49%) | 90 MB |
| Japan | 26.1s | 4.85s (19%) | 2.60s (10%) | 23.4s (90%) | 244 MB |

## Merge: no indexdata + zlib (baseline, commit aed93e0)

| Region | Wall | classify_blob | rewrite_block | frame_blob | RSS |
|--------|------|---------------|---------------|------------|-----|
| Denmark | 3.50s | 3.26s (93%) | 1.99s (57%) | 5.70s (163%) | 95 MB |
| Germany | 50.0s | 33.8s (67%) | 17.6s (35%) | 109.9s (220%) | 364 MB |
| Malta | 61ms | 10ms (17%) | 18ms (29%) | 43ms (71%) | 37 MB |
| Gr. London | 1.65s | 888ms (54%) | 445ms (27%) | 2.78s (169%) | 100 MB |
| Switzerland | 7.00s | 3.68s (53%) | 2.02s (29%) | 12.5s (178%) | 130 MB |
| Norway | 10.1s | 8.31s (83%) | 687ms (7%) | 4.44s (44%) | 110 MB |
| Japan | 22.8s | 16.6s (73%) | 4.67s (20%) | 23.5s (103%) | 265 MB |

**Findings:**
- **Norway no-indexdata:** classify_blob jumps from 400ms (indexed) to 8.31s
  (no-indexdata) — a 21x increase. Without index, every blob must be partially
  decompressed and scanned to check for overlapping IDs. Wall time only goes
  from 9.1s to 10.1s though — classify was already overlapping with I/O.
- **Japan no-indexdata:** classify_blob jumps from 2.6s to 16.6s (6x). Wall
  time actually drops slightly (24.4→22.8s) — without indexdata, the pipelined
  writer's compression overlaps more work. Same pattern as Germany where
  no-indexdata+zlib ≈ indexdata+zlib on wall time.

## Decode + write allocations (cat --type)

> **Note (prost removed + FrameScratch, commit `75e8edd`):** The columns below
> reflect the old prost-based encoding (commit `aed93e0`) — preserved as baseline.
> Current state: all prost code replaced with hand-rolled wire-format encoding
> (ways/relations into reusable scratch buffers, DenseNodes without proto struct,
> take reuses encode buffer). `frame_blob_into()` reuses blob_buf, header_buf,
> compress_buf via FrameScratch (thread_local for pipelined path). Denmark current:
> add_way 4.1→1.2 GB (-71%), frame_blob 4.0→2.9 GB (-28%), add_node 1.8→1.4 GB
> (-22%). See `notes/hotpath-profile.md` for full current numbers.

| Region | Total | take | add_way | frame_blob | add_node | decompress | add_relation |
|--------|-------|------|---------|------------|----------|------------|--------------|
| Denmark | 16.8 GB | 4.6 GB | 4.1 GB | 4.0 GB | 1.8 GB | 1.6 GB | 52 MB |
| Malta | 260 MB | 54 MB | 86 MB | 70 MB | 5 MB | 28 MB | 5 MB |
| Gr. London | 3.8 GB | 772 MB | 1.4 GB | 942 MB | 120 MB | 403 MB | 66 MB |
| Switzerland | 14.9 GB | 3.6 GB | 4.2 GB | 4.3 GB | 306 MB | 1.7 GB | 208 MB |
| Norway | 44.5 GB | 12.9 GB | 10.1 GB | 13.5 GB | 584 MB | 5.3 GB | 575 MB |
| Japan | 79.0 GB | 18.8 GB | 25.2 GB | 22.4 GB | 462 MB | 9.1 GB | 275 MB |

**Findings:**
- **Japan add_way alloc dominates**: 25.2 GB for 42.9M ways = 617 bytes/call.
  Lower per-call than Norway (900 B/call) but total is 2.5x because Japan has
  3.6x more ways. Japan ways have fewer node refs on average but more of them.
- **Norway add_way alloc**: 10.1 GB for 12M ways = 900 bytes/call avg. Higher
  than Denmark (659 bytes/call) and Switzerland (726 bytes/call). Norwegian
  ways have more node refs (coastlines/fjords) → larger refs Vec allocations.
  This confirms the coastline hypothesis even though per-call time (222ns)
  didn't show it — the cost is in allocation, not in the delta encoding loop.
- **Norway add_relation alloc**: 575 MB for 777K relations = 776 bytes/call.
  Lower than London (2.1 KB/call for 31K relations). Norway relations are
  plentiful but small; London relations are fewer but much larger (TfL routes).
- **Japan total alloc is 79 GB** — 1.8x Norway despite 1.6x elements. The
  take + frame_blob overhead scales roughly linearly with blob count.

## Merge allocations (indexdata + none, commit aed93e0)

Measured before passthrough I/O optimizations (commit `b750e60`). read_raw_frame alloc is now
~42% lower (blob_bytes duplication eliminated), and write_raw .to_vec()
copies are eliminated entirely. Denmark post-optimization: read_raw_frame
465 MB (was ~795 MB), total merge 931 MB.

| Region | Total | rewrite_block | read_raw_frame | take | classify_blob |
|--------|-------|---------------|----------------|------|---------------|
| Malta | 67 MB | 32 MB (48%) | 16 MB (25%) | 5 MB (7%) | — |
| Gr. London | 1.3 GB | 740 MB (57%) | 236 MB (18%) | 124 MB (10%) | 142 MB (11%) |
| Switzerland | 5.3 GB | 3.0 GB (56%) | 1.0 GB (19%) | 552 MB (10%) | 692 MB (13%) |
| Norway | 5.3 GB | 1.3 GB (24%) | 2.6 GB (48%) | 133 MB (2%) | 213 MB (4%) |
| Japan | 15.3 GB | 6.7 GB (44%) | 4.5 GB (29%) | 1.2 GB (8%) | 1.5 GB (10%) |

**Findings:**
- **Norway alloc pattern is unique:** read_raw_frame dominates at 48% (2.6 GB)
  because 27K blobs need I/O buffers even for passthrough. rewrite_block is only
  24% because only 354 blobs get rewritten. This inverts the pattern seen in all
  other regions where rewrite_block dominates. Post-optimization, Norway's
  read_raw_frame would drop to ~1.3 GB (~25%), shifting the balance.
- **Japan splits the difference:** rewrite_block at 44% (6.7 GB) because it
  rewrites 3544 blobs (10x Norway). read_raw_frame at 29% (4.5 GB) for 39K+
  passthrough blobs. Post-optimization, ~2.3 GB → total ~13 GB.
  Japan is the closest proxy to planet-scale merge behavior.

## Status

- **Completed**: Malta, Greater London, Switzerland, Norway, Japan (all timing + alloc)
- **Not run**: Germany full suite (only merge timing exists, no read/write/alloc)
- **Not downloaded**: Kantō sub-region

To run Germany full suite:
```
scripts/profile-region.sh germany data/germany-20260224-seq4704.osm.pbf data/germany-20260225-seq4705.osc.gz 2>&1 | tee notes/germany-profile-raw.txt
```
