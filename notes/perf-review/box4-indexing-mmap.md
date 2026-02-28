# Box 4: Indexing, Blob Filtering, and Mmap Paths

Performance review of `blob_index.rs`, `indexed.rs`, and `mmap_blob.rs`.

## 1. Executive Summary

- **Indexdata ecosystem gap is real but self-healing.** Non-indexed PBFs (Geofabrik) fall back gracefully (no failure, just slower classification in merge). The merge command itself adds indexdata to passthrough blobs via `reframe_raw_with_index`, so a single merge produces an indexed output. The cat passthrough path does NOT add indexdata -- only the filtered cat path (which decodes and re-encodes) does.
- **MmapBlobReader copy tradeoff is well-justified.** The `Bytes::copy_from_slice` per blob costs ~4s for a planet file (~80GB memcpy), which is 1.3% of total parallel decode time (~300s). The alternative (shared Arc atomics) would cause measurable contention on 14+ decode threads. The current design is structurally correct.
- **IndexedReader is a legacy API with no internal consumers.** It is not used by any command in the pbfhogg CLI. It is only a public library API, inherited from osmpbf. Its BTreeSet design would OOM at planet scale if used for `read_ways_and_deps` (~20B node refs = ~960GB).
- **scan_block_ids is efficient for its purpose** -- it skips stringtable, coordinates, tags, refs, metadata. Dense node scanning must decode all varints (unavoidable for delta encoding). Ways/relations only decode the first varint per message (field 1 = id).
- **BlobFilter has no effect on MmapBlobReader** -- the mmap path has no filter integration. Filter only works via the pipelined read path (`pipeline.rs:112-118`).

## 2. Finding 1: Indexdata Ecosystem Gap

### How indexdata gets embedded

The write path embeds indexdata automatically whenever a block is encoded via `write_primitive_block` (writer.rs:325-356). Both sync and pipelined paths call `scan_block_ids` on the serialized PrimitiveBlock bytes, then pass the 26-byte result to `encode_blob_header_into` (writer.rs:802-817) which writes it as BlobHeader field 2.

The key code path (writer.rs:332-334, pipelined):
```rust
let indexdata = blob_index::scan_block_ids(&uncompressed)
    .map(|idx| idx.serialize());
```

### What generates indexdata and what doesn't

| Operation | Adds indexdata? | Why |
|---|---|---|
| `cat --type node,way,relation` | Yes | Filtered path: full decode + re-encode via `write_primitive_block` |
| `cat` (no filter) | **No** | Passthrough: `write_raw` / `write_raw_copy` passes frames unchanged |
| `merge` | **Yes** (to non-indexed blobs) | `coalesce_passthrough` (merge.rs:1382-1399) reframes non-indexed blobs via `reframe_raw_with_index` |
| `sort` | Yes | sort.rs:336 calls `reframe_raw_with_index` for passthrough blobs |
| Any `write_primitive_block` consumer | Yes | Automatic |

The CLAUDE.md states "cat embeds indexdata automatically when writing" -- this is only true for the filtered `cat` path (`cat --type node,way,relation`), not the passthrough path. The documented workflow (`cargo dev run cat input.osm.pbf --type node,way,relation -o output.osm.pbf`) works correctly because the type filter forces the decode path.

### Cost of adding indexdata to a planet file

Running `cat --type node,way,relation` on an 80GB planet file requires:
- Full decompression of all ~2.5M blobs (sequential I/O + parallel decode)
- Full re-encoding via BlockBuilder + re-compression
- Estimated time: ~30 minutes (based on Denmark 465MB in ~13s pipelined write = ~28 min at planet scale)
- This is a full rewrite. There is no incremental "add indexdata" operation.

### Self-healing property

The merge command is the primary consumer of indexdata, and it adds indexdata to non-indexed passthrough blobs during output (merge.rs:1382-1399). So the typical workflow is:

1. Download Geofabrik PBF (no indexdata)
2. First merge with OSC diff: slow classification (decompress + scan for every blob), but output has indexdata
3. Second merge onward: fast classification via index for passthrough blobs (~92% of blobs at planet scale)

This means the gap is a one-time cost on the first merge. The performance difference is significant: with indexdata, passthrough classification is ~26 bytes of fixed-format decode (BlobIndex::deserialize, blob_index.rs:50-69). Without indexdata, it requires decompression (~150us per blob) + scan_block_ids (~10us per blob).

### Quantifying the first-merge overhead

At planet scale (~2.5M blobs, ~92% passthrough):
- With indexdata: ~2.3M blobs x 0 decompression = 0s classify
- Without indexdata: ~2.3M blobs x ~160us (decompress + scan) = ~370s extra classify time
- This is non-trivial (~6 minutes) but happens only once. After the first merge, subsequent merges are fast.

### Verdict: Real but mitigated. Priority: Low.

The self-healing property of merge is sufficient. The one-time cost on first merge is acceptable. No code change needed -- the current design is intentional and well-documented.

## 3. Finding 2: MmapBlobReader Copy Tradeoff

### What happens

`MmapBlobReader::next()` (mmap_blob.rs:261-391) does:

1. Sub-slices the mmap via `&data[self.offset..]` -- zero-cost pointer arithmetic (line 283)
2. Parses the BlobHeader from `&slice[4..4 + header_size]` (line 316) -- already zero-copy (WireBlobHeader::parse takes `&[u8]`)
3. Copies the blob data payload via `Bytes::copy_from_slice(&slice[data_start..chunk_size])` (line 384) -- **this is the copy**

### Why the copy exists (documented in lines 119-163)

The design doc in the source explains three alternatives considered:

1. **`Bytes::slice()` on shared mmap Arc**: Atomic increment/decrement per slice on a single cache line. For planet (~2.5M blobs, 3 slices per blob = ~7.5M atomic ops), all contending on the same Arc.
2. **`Bytes::copy_from_slice()`**: Independent allocation per blob, no atomic contention, no mmap pinning.
3. **Offset-based with no Bytes at all**: Current design for iteration, Bytes only for payload.

The current design (option 2+3) eliminates all atomic operations during iteration and creates independent per-blob Bytes objects.

### Cost analysis

Average compressed blob size in a planet file: ~32KB (80GB / 2.5M blobs).

Total memcpy cost: 2.5M blobs x 32KB = ~80GB of memcpy.
At memory bandwidth of ~20GB/s (single-threaded memcpy): ~4 seconds.
At ~10GB/s (realistic with cache pressure from concurrent decode): ~8 seconds.

Total parallel decode time for planet: ~300s (from MEMORY.md baseline: Denmark 465MB in 310ms parallel, linear extrapolation to 80GB).

**Copy overhead: ~1.3-2.7% of total decode time.** This is noise.

### The alternative: shared Arc atomics

If using `Bytes::slice()` instead, each blob would:
- Atomic increment (Arc clone) on slice creation
- Atomic decrement on drop
- With 14 decode threads + 1 I/O thread, all hitting the same cache line

Atomic operations on contended cache lines cost 20-100ns depending on cross-core traffic. With 2.5M blobs x 2 atomic ops x ~50ns average = ~250ms. This seems cheaper than 4-8s of memcpy, BUT:

1. The atomics contend across cores (MESI protocol coherence traffic), causing pipeline stalls in the decode threads
2. The copy produces cache-friendly independent allocations (~32KB each, fits L1)
3. The shared mmap Bytes would pin the entire ~80GB mapping as long as any blob is alive

The mmap pinning is the critical issue: with pipelined processing, blobs are in-flight across stages. If any stage holds a `Bytes::slice()` reference, the entire 80GB mmap stays mapped. With independent copies, the mmap can be unmapped as soon as iteration completes.

### Does MmapBlobReader support BlobFilter?

**No.** There is no `blob_filter` field or filter check in `mmap_blob.rs`. The `MmapBlob` struct does not expose an `index()` method. BlobFilter only works via the pipelined path in `pipeline.rs:112-118`, where `blob.index()` checks the BlobHeader's indexdata field.

This means the mmap path cannot skip decompression of filtered blob types. For commands that use BlobFilter (cat, tags-filter, check-refs, node-stats, diff, getid, add-locations-to-ways, tags-count -- 8 commands total), the mmap path would decompress all blobs regardless.

However, no command currently uses the mmap path directly. Commands use `ElementReader` which uses `BlobReader` + `pipeline.rs`, not `MmapBlobReader`. The mmap path is exposed as a public API for library users and benchmarks only.

### Verdict: Not a problem. Priority: None.

The copy tradeoff is sound. The 1.3% overhead is well within noise. The mmap pinning argument alone justifies the copy. No change needed.

## 4. Finding 3: IndexedReader Scaling

### Current state: no internal consumers

`IndexedReader` is defined in `src/read/indexed.rs` (line 117) and re-exported as public API via `src/lib.rs:114`. However, searching the entire `src/commands/` directory reveals **zero uses**. No CLI command uses `IndexedReader`.

The struct is inherited from the original osmpbf crate. It provides:
- `read_ways_and_deps()`: two-pass way filtering with dependent node lookup
- `for_each_node()`: filtered node iteration

Both of these are now superseded by:
- `ElementReader` + `BlobFilter` for type-filtered iteration (much faster, parallel)
- Multi-pass approaches in `extract.rs` using `ElementReader::into_blocks_pipelined`

### Design and scaling analysis

#### Index construction (create_index, lines 150-188)

`create_index()` scans the file sequentially using `next_header_skip_blob()`, recording only byte offsets and blob types. No decompression. For a planet file (~2.5M blobs), this creates a `Vec<BlobInfo>` of ~2.5M entries.

Memory per `BlobInfo`: `ByteOffset(u64)` + `SimpleBlobType(enum, 1 byte padded to 8)` + `Option<IdRanges>` (initially None, 8 bytes). Total: ~24 bytes per entry. 2.5M entries = ~60MB. Acceptable.

#### ID range population (update_element_id_ranges, lines 199-246)

On first access to each blob, the full `PrimitiveBlock` is parsed (not just IDs). The comment at lines 192-197 explicitly acknowledges that a lightweight scan mode was considered but rejected because "this runs once per IndexedReader session, not in a hot loop."

The full parse includes stringtable validation (UTF-8 check of every entry), which is the most expensive part of `PrimitiveBlock::new()`.

#### BTreeSet scaling for read_ways_and_deps (lines 289-350)

The two-pass approach:
1. **Pass 1**: Iterate all way blobs, filter ways, collect `node_ids` into a `BTreeSet<i64>`
2. **Pass 2**: Iterate node blobs whose ID range overlaps the BTreeSet, filter individual nodes

At planet scale:
- ~800M ways, each with ~25 node refs on average = ~20B node references
- If a user filter selects 10% of ways (80M ways), that's ~2B node IDs in the BTreeSet
- BTreeSet overhead: ~48 bytes per entry (B-tree node pointers + value)
- **Memory: 2B x 48 bytes = ~96GB.** This would OOM on any reasonable machine.

Even a conservative filter selecting 1% of ways (8M ways, ~200M node refs) would use ~9.6GB for the BTreeSet alone.

#### Sequential seek-based access

`IndexedReader` uses `BlobReader<File>` (without BufReader -- see from_path, lines 430-443) for random-access seeks. The comment at lines 432-442 explains this is intentional: BufReader's read-ahead buffer is wasted on random seeks.

For the two-pass pattern, Pass 1 is sequential (reads all way blobs in order), which is efficient even without BufReader. Pass 2 is semi-random (seeks to node blobs whose ranges match), which benefits from kernel readahead on sorted PBFs.

#### No parallel support

`IndexedReader` is inherently single-threaded (uses seek on a single File handle). There is no parallel indexed read path. This is a fundamental limitation for planet-scale use.

### Verdict: Real but academic. Priority: Low.

IndexedReader's scaling problems are real, but no internal code exercises them. The API exists for backward compatibility with osmpbf library users. Any planet-scale use would hit OOM. However, since no commands use it, the practical impact is zero. If a library user hits this, they should use `ElementReader` + `BlobFilter` instead.

## 5. BlobIndex Scanner Analysis

### scan_block_ids efficiency (blob_index.rs:142-166)

The scanner walks the PrimitiveBlock wire format manually, reading only element IDs. Here is what it parses vs. what `PrimitiveBlock::new()` (block.rs:351) parses:

| Component | scan_block_ids | PrimitiveBlock::new |
|---|---|---|
| Top-level PB fields (tag loop) | Yes (to find field 2 groups) | Yes |
| StringTable (field 1) | **Skipped** (skip_field) | Parsed + UTF-8 validated |
| PrimitiveGroup submessages | Entered, scanned for type | Stored as offset ranges |
| DenseNodes IDs (field 1 packed sint64) | **Decoded** (all varints for min/max) | Not decoded until iteration |
| DenseNodes other fields (lat, lon, keys_vals, info) | **Skipped** (skip_field) | Not decoded until iteration |
| Way/Relation messages | Entered per message, extract field 1 (id varint) | Not decoded until iteration |
| Way/Relation other fields (keys, vals, refs, info) | **Skipped** (skip_field) | Not decoded until iteration |
| Granularity, offsets (fields 17-20) | Skipped | Parsed |

The scanner is substantially lighter than a full parse because:
1. StringTable parsing + UTF-8 validation is the most expensive part of `PrimitiveBlock::new()`, and scan_block_ids skips it entirely
2. For dense nodes, only the ID-packed field is decoded; lat, lon, keys_vals (the bulk of the data) are skipped via length-delimited skip
3. For ways/relations, `extract_element_id` (blob_index.rs:285-294) reads only the first varint field and skips everything else

### Dense node ID scanning (blob_index.rs:204-240)

Dense node IDs are delta-encoded packed sint64 values. At 8000 nodes per block:
- Each varint is 1-3 bytes (most deltas are small for sorted IDs)
- Total: ~8000 varint reads + zigzag decodes + additions
- Estimated cost: ~10-20us per dense node block

**Can this be optimized?** For a sorted PBF:
- First delta + base = min_id
- Sum of all deltas + base = max_id
- You CANNOT skip middle deltas because they're delta-encoded (each depends on the previous)
- A SIMD varint decoder could speed up the loop, but the data is typically <16KB per block (8000 varints at ~2 bytes each), which is already in L1 cache

The scanner uses min/max tracking (lines 213-214) rather than relying on sort order, which is correct for unsorted PBFs. For sorted PBFs, `min_id = first_id` and `max_id = last_id`, but the general approach adds negligible overhead (~1 comparison per varint).

### Way/Relation ID scanning efficiency

For ways and relations, `scan_repeated_element_ids` (blob_index.rs:245-281) iterates each message in the group:
1. Call `extract_element_id` which reads the first varint field (field 1 = id)
2. For each subsequent field in the message, `skip_field` skips it by wire type

A way message has ~7 fields (id, keys, vals, info, refs, lats, lons). The `refs` field alone can be large (~25 varints for a typical way). But `skip_field` for length-delimited fields is O(1): read the length varint, advance the cursor by that many bytes.

For a block with 8000 ways:
- 8000 x extract_element_id (~2 varint reads: tag + id) = ~16K varint reads
- 8000 x ~6 skip_field calls = ~48K skip operations (mostly O(1) length reads)
- Estimated cost: ~20-30us per way block

### BlobIndex serialization format (26 bytes)

The 26-byte format (blob_index.rs:29-30) is:
```
1B version (0x01)
1B element_type
8B min_id (i64 LE)
8B max_id (i64 LE)
8B count (u64 LE)
```

**Extension potential**: The version byte (currently 0x01) allows forward-compatible extensions. A v2 format could add bbox coordinates for spatial filtering. However:
- Dense node coordinates require the block's granularity/lat_offset/lon_offset to decode, which are top-level PrimitiveBlock fields. The scanner would need to parse these (currently skipped).
- Way/relation bboxes require resolving node references, which is not available from the way message alone.
- A more practical extension would be to add the bbox from the node blocks only, and store it in an additional 32 bytes (min_lat, max_lat, min_lon, max_lon as i32 decimicrodegrees).
- Total extended format: 26 + 32 = 58 bytes per blob, still negligible in the BlobHeader.

### How merge uses BlobIndex

Merge is the primary consumer of BlobIndex. The classification path (merge.rs:840-879, `classify_only`):

1. **Index fast path** (merge.rs:849-853): If `frame.index` is `Some`, call `ranges.range_overlaps(idx.kind, idx.min_id, idx.max_id)`. This is a single binary search on the sorted diff ID vectors. If no overlap, return `Passthrough` immediately. **No decompression at all.**

2. **Slow path** (merge.rs:856-865): If no indexdata, decompress the blob (`decompress_blob_data_into`), then call `scan_block_ids` on the decompressed data. If no range overlap, return `Passthrough`.

3. **Full parse** (merge.rs:868-878): If range overlaps, parse the full PrimitiveBlock and check individual element IDs against the diff.

The index fast path is the critical optimization. For a typical daily diff (100-200K changes), >90% of blobs have no overlap. With indexdata, those blobs are classified in ~100ns (26-byte deserialize + binary search). Without indexdata, they cost ~160us each (decompress + scan).

## 6. Additional Findings

### 6.1 BlobFilter not available on MmapBlobReader

As noted in Finding 2, `MmapBlobReader` has no `blob_filter` support. The `MmapBlob` struct (mmap_blob.rs:77-81) stores a `WireBlobHeader` which does contain the `indexdata` field, but there is no `index()` method exposed, and no filtering logic in `next()`.

Adding filter support to the mmap path would require:
1. Adding a `blob_filter: Option<BlobFilter>` field to `MmapBlobReader`
2. In `next()`, after parsing the header: check `header.indexdata` -> `BlobIndex::deserialize` -> `filter.wants()`
3. If filtered out, skip to the next blob (advance `self.offset` by `chunk_size`)

This would be straightforward (~20 lines of code) but has no current consumer. The mmap path is not used by any command -- they all use `ElementReader` which goes through `pipeline.rs`.

### 6.2 MmapBlobReader is slower than sequential BlobReader

MEMORY.md reports Denmark benchmarks: mmap 2900ms vs sequential 2800ms. The ~100ms difference (~3.5%) is consistent with the copy overhead analysis:

Denmark = 465MB, ~4700 blobs. Average blob = ~99KB (larger than planet average because Denmark is a smaller file with the same blob count structure). 4700 x 99KB = 466MB of memcpy. At 20GB/s = ~23ms. But the benchmark runs single-threaded (sequential mode), so the copy cost is amortized into the total. The 100ms difference likely includes:
- memcpy cost (~23ms)
- mmap page fault overhead (first access to each page triggers a soft fault: ~1us per 4KB page = 465MB / 4KB x 1us = ~116ms)

The page fault overhead is the dominant cost, not the memcpy. This is inherent to mmap on first access and cannot be optimized away without `madvise(MADV_SEQUENTIAL)` or `madvise(MADV_WILLNEED)`.

### 6.3 IndexedReader does not use blob-level indexdata

`IndexedReader::create_index()` (indexed.rs:150-188) scans blob headers using `next_header_skip_blob`, which reads the `WireBlobHeader` including the `indexdata` field. However, the `BlobInfo` struct (indexed.rs:53-57) does not store the indexdata. Instead, it uses `id_ranges: Option<IdRanges>` which is populated later by `update_element_id_ranges` (indexed.rs:199-246) through full PrimitiveBlock parsing.

This is a missed optimization: if indexdata is available, `create_index` could populate `id_ranges` directly from the deserialized `BlobIndex` without needing a full parse later. This would make `read_ways_and_deps` skip fewer blobs on the first pass (currently, blobs with `id_ranges: None` are conservatively included).

However, since IndexedReader has zero internal consumers, this is purely theoretical.

### 6.4 copy_file_range interaction with mmap

The `copy_file_range` optimization (writer.rs:510-554) is used by merge and cat for passthrough blobs when the `linux-direct-io` feature is enabled. It copies data between file descriptors in kernel space, avoiding userspace copies.

This does NOT interact with mmap reads. `copy_file_range` takes a source fd + offset and a destination fd. The mmap path does not provide a raw fd (it uses `memmap2::Mmap` which abstracts the fd). If `IndexedReader` ever needed passthrough for merge, it would need to use `write_raw` (userspace copy) rather than `copy_file_range`.

### 6.5 scan_block_ids skip_field efficiency

The reviewer's concern about "skip to field N" being faster is unfounded. Protobuf wire format requires sequential field parsing -- there is no random access to field N. Each field is preceded by a tag (varint) that encodes the field number and wire type. The `skip_field` operation is already optimal:

- Varint: read and discard varints until the value is read (O(varint_size))
- Length-delimited: read length varint, advance cursor by that many bytes (O(1))
- Fixed32/64: advance cursor by 4/8 bytes (O(1))

For the scan_block_ids use case, the bulk of the data is length-delimited fields (keys, vals, refs, lats, lons, info), which are each skipped in O(1). The total skip cost per way/relation message is ~6 length-delimited skips = ~12 varint reads. This is negligible compared to the tag reads.

### 6.6 BlobFilter granularity: spatial filtering potential

The current `BlobFilter` is element-type only (nodes/ways/relations). For spatial queries (like `extract`), a bbox filter would be valuable. The `BlobIndex` has min_id/max_id but no coordinates.

A spatial index extension would require:
- For node blobs: scan dense node lat/lon packed fields (fields 8/9 of DenseNodes) to compute bbox. This requires knowing granularity and lat_offset/lon_offset from the PrimitiveBlock top-level fields.
- For way/relation blobs: no coordinate data available (coordinates live in node blobs).
- This limits spatial blob filtering to node blobs only, which is still useful (~85% of blobs in a typical PBF are node blobs).

The existing extract command (extract.rs) does not use blob-level filtering -- it reads all blobs sequentially and filters elements. A spatial blob filter could skip node blobs outside the extraction bbox, saving ~85% of decompression for small extracts from planet files.

Estimated benefit for a city-level extract from planet:
- Planet has ~2.1M node blobs. A city bbox covers perhaps 0.01-0.1% of the coordinate space.
- Without spatial filter: decompress all 2.1M node blobs (~300s decode)
- With spatial filter: decompress ~2100-21000 node blobs (~0.3-3s decode)
- Savings: 99%+ for node decompression

This is a significant potential optimization for the extract command, but requires the BlobIndex format extension discussed in section 5.

## 7. Cross-box Interactions

### Box 1 (Pipeline, pipeline.rs)

BlobFilter integration is in pipeline.rs:112-118. The filter checks `blob.index()` -- if the blob has indexdata, the filter can skip decompression. If not, the blob passes through and is decompressed anyway. This means:

- For pbfhogg-written PBFs: BlobFilter skips ~85% of blobs for ways-only queries
- For Geofabrik PBFs: BlobFilter has no effect (no indexdata), all blobs are decompressed
- After one merge (which adds indexdata), subsequent queries benefit

**Planet-scale impact**: A ways-only query (like elivagar ocean processing) on a Geofabrik planet file decompresses all ~2.5M blobs. After indexdata is added (via merge or filtered cat), it decompresses only ~375K way blobs (15%), saving ~300s x 0.85 = ~255s of decode time.

### Box 2 (Blob Decode, DecompressPool)

The mmap path does not use `DecompressPool` (from blob.rs). `MmapBlob::decode()` (mmap_blob.rs:86-99) calls `decompress_blob(&blob, None)` with `None` for the pool. This means each decompression allocates a fresh buffer rather than reusing pooled buffers.

For library users who access the mmap path directly, this means higher allocation pressure. However, since the mmap path is single-threaded (no parallel decode), the allocation pressure is lower than the pipelined path where multiple blobs are decompressed concurrently.

### Box 7 (io_uring/Direct I/O)

The mmap path and O_DIRECT are mutually exclusive approaches:
- mmap relies on the page cache (kernel manages which pages are resident)
- O_DIRECT bypasses the page cache entirely (application manages I/O)

For planet-scale reads that exceed available RAM (80GB PBF, ~30GB RAM), mmap causes page cache thrashing. O_DIRECT with explicit readahead (as in the pipelined `BlobReader::from_path_direct`) avoids this. The mmap path is only suitable for files that fit in RAM.

### Box 8 (Commands)

- **merge**: Primary BlobIndex consumer. Uses index fast path for passthrough classification. Adds indexdata to output blobs.
- **sort**: Uses `reframe_raw_with_index` to add indexdata to output blobs.
- **cat (filtered)**: Uses `write_primitive_block` which adds indexdata automatically.
- **cat (passthrough)**: Does NOT add indexdata (uses `write_raw`).
- **extract**: Does NOT use IndexedReader or BlobFilter for spatial queries. Uses full sequential decode. Could benefit from spatial blob filtering (see 6.6).
- **8 commands use BlobFilter**: cat, tags-filter, check-refs, node-stats, diff, getid, add-locations-to-ways, tags-count. All via `ElementReader::with_blob_filter` + pipelined path.

## 8. Recommended Actions

### Priority 1: None required

The three findings from the reviewer are all correctly assessed but do not require immediate action:
1. Indexdata gap: self-healing via merge
2. Mmap copy: justified, negligible cost
3. IndexedReader scaling: no consumers

### Priority 2: Low-effort improvements (nice-to-have)

**2a. Add `madvise(MADV_SEQUENTIAL)` to MmapBlobReader** (mmap_blob.rs)
After creating the mmap, call `mmap.advise(memmap2::Advice::Sequential)`. This tells the kernel to prefetch pages ahead of the read position and evict pages behind it, reducing page fault overhead. Expected improvement: ~50-80ms on Denmark (based on the ~116ms estimated page fault overhead). Single line change.

**2b. Document cat passthrough indexdata behavior**
The CLAUDE.md section on indexdata PBFs states `cat` embeds indexdata "automatically when writing." This is only true for the filtered path. The passthrough path (no `--type` flag) does not. The documentation should clarify this or the passthrough path should add indexdata via reframe (matching merge's behavior).

### Priority 3: Future work

**3a. Spatial blob filter for extract** (significant effort)
Extend BlobIndex with a bbox field for node blobs. Requires:
- Extending `scan_block_ids` to also scan lat/lon packed fields
- Adding granularity/offset parsing to the scanner
- New BlobIndex v2 format with bbox (58 bytes)
- A `SpatialBlobFilter` type that checks bbox overlap

Potential savings: 99%+ node decompression for small extracts from planet. This is the single largest optimization opportunity identified in Box 4.

**3b. Deprecate IndexedReader or document limitations**
Add a doc warning that `read_ways_and_deps` will OOM at planet scale. Suggest `ElementReader` + `BlobFilter` as the alternative. Alternatively, gate `IndexedReader` behind a feature flag to reduce API surface.

**3c. Add BlobFilter support to MmapBlobReader**
~20 lines of code. No current consumer, but would make the mmap API more complete for library users who need type-filtered iteration without the pipeline.
