# P3-20: SIMD Varint Decode/Encode in protohoggr

**Status: CLOSED - not worth pursuing.** Scalar beats SIMD in all scenarios.
See [Microbenchmark Results](#microbenchmark-results-protohoggr-go-no-go) and
[Verdict](#verdict).

Research doc for SIMD-accelerated varint operations. Corresponds to TODO.md
item P3-20.

## Problem Statement

All protobuf varint operations in protohoggr are byte-at-a-time scalar loops.
At planet scale (~80 GB PBF), a full read decodes ~119 billion varints and a
full write encodes a comparable number. The per-varint cost (~3 ns on modern
x86) is currently masked by zlib decompression on most paths, but becomes a
dominant cost when:

1. Using `Compression::None` (the production merge path)
2. Running commands that do full decode + re-encode (sort, cat,
   add-locations-to-ways)
3. Running downstream consumers (elivagar, nidhogg) that iterate all elements

The old perf-review analysis (deleted notes/perf-review/box3 and box6)
estimated ~25 s read-side and ~175 s write-side savings at planet scale,
assuming a 2× speedup from SIMD batch decode/encode.

## Varint Volume by Element Type (Planet Scale)

| Element type   | Count   | Varints/element (approx) | Total varints |
|----------------|---------|--------------------------|---------------|
| Dense nodes    | 8.5 B   | 3 (id/lat/lon deltas)    | 25.5 B        |
| DenseNodeInfo  | 8.5 B   | 6 (ver/ts/cs/uid/usid/vis)| 51 B         |
| Way refs       | ~1.2 B  | ~20 refs avg             | 24 B          |
| Way tags       | ~1.2 B  | ~4 k/v pairs avg         | 9.6 B         |
| Relations      | ~17 M   | ~43 avg                  | 0.7 B         |
| Way metadata   | ~1.2 B  | 5                        | 6 B           |
| Miscellaneous  |         |                          | ~2 B          |
| **Total**      |         |                          | **~119 B**    |

Dense nodes dominate: 76.5 B varints (64%) from id/lat/lon + metadata alone.

## Current Implementation

### Decode (protohoggr/src/lib.rs)

```
Cursor::read_varint()        - 1-byte fast path, then byte-at-a-time loop
PackedIter::next()           - wraps Cursor::read_varint(), yields u64
PackedSint64Iter::next()     - zigzag_decode_64 on top
PackedInt32Iter::next()      - truncating cast on top
```

The Iterator-based API is consumed by:

- **DenseNodeIter** (dense.rs): zips 3× PackedSint64Iter (id/lat/lon deltas)
  + DenseNodeInfoIter (5 more packed iterators) + tag cursor scan. This is the
  single hottest varint consumer - 8.5 B nodes × 9.35 varints = ~80 B.
- **WayRefIter** (elements.rs): PackedSint64Iter over delta-encoded node refs.
- **WayNodeLocationIter** (elements.rs): 2× PackedSint64Iter (lat/lon deltas).
- **RelationMemberIter** (elements.rs): 3× packed iterators (roles/ids/types).
- **TagIter** (elements.rs): 2× PackedUint32Iter (key/val string table indices).

### Encode (protohoggr/src/lib.rs + block_builder.rs)

```
encode_varint(buf, value)     - byte-at-a-time push loop into Vec<u8>
encode_packed_sint64(...)     - loop over &[i64], zigzag + encode_varint each
encode_packed_int32(...)      - loop over &[i32], encode_varint each
encode_packed_sint32(...)     - loop over &[i32], zigzag + encode_varint each
```

Write-side hot spots in block_builder.rs:

- **encode_dense_nodes_group()**: 10 packed field calls (ids, metadata ×6,
  lats, lons, keys_vals). At 8000 nodes/block, that's ~75 K varints per block.
- **encode_way()**: delta-encoded refs (packed sint64, ~20 varints/way) + tags.
- **encode_way_with_locations()**: 3 parallel packed arrays (refs/lats/lons).
- **encode_relation()**: 3 parallel packed arrays (roles/memids/types) + tags.

## SIMD Approach Options

### Option 1: varint-simd crate (external dependency)

[varint-simd](https://github.com/as-com/varint-simd) v0.4.1 (Sept 2024,
stable Rust, 109 commits).

**Decode API:**
- `decode(slice) -> Result<(u64, usize)>` - single, SSSE3
- `decode_two_unsafe(ptr) -> (u64, u64, usize)` - SSSE3
- `decode_four_unsafe(ptr) -> (u64, u64, u64, u64, usize)` - SSSE3
- `decode_zigzag(slice) -> Result<(i64, usize)>` - convenience wrapper

**Encode API:**
- `encode_to_slice(value, slice) -> usize` - SSE2
- `encode_zigzag(value) -> Vec<u8>` - convenience wrapper
- No batch encode function.

**Benchmark claims:** 330 M u64 decodes/s, 360 M u32 encodes/s (i7-8850H).

**Pros:** Maintained, stable Rust, standard LEB128, zigzag support built in.
**Cons:** External dependency (protohoggr is currently zero-dep), unsafe batch
APIs, no batch encode, SSSE3 x86_64 only (no aarch64).

### Option 2: Hand-rolled SIMD intrinsics

Write SIMD decode/encode directly in protohoggr using `std::arch::x86_64`.

**Pros:** No external dependency, full control over API surface, can target
exact batch sizes we need, can add aarch64 NEON path later.
**Cons:** Significant implementation effort (~500-1000 lines), needs extensive
testing, maintenance burden, requires `unsafe`.

### Option 3: Scalar improvements only

Add a 2-byte and 3-byte fast path to `Cursor::read_varint()`. Most
delta-encoded values in OSM data are small (1-3 byte varints after zigzag):
- Node ID deltas: mostly 1-3 bytes (IDs are roughly sequential)
- Lat/lon deltas: mostly 1-2 bytes (nodes in same block are geographically
  close)
- Way ref deltas: 2-4 bytes (node IDs jump more)
- String table indices: 1-2 bytes (most blocks have <128 unique strings)

**Pros:** Zero dependencies, zero unsafe, works everywhere, simple.
**Cons:** Smallest improvement (~10-20% vs ~2× for SIMD), doesn't help encode.

### Recommendation: Option 1 (varint-simd) behind a feature flag

**Why:** Best effort-to-impact ratio. The crate handles the hard SIMD work,
is maintained, and uses standard LEB128. The dependency is gated behind a
feature flag so protohoggr stays zero-dep by default.

**Feature flag:** `simd` in protohoggr's Cargo.toml. When enabled, `PackedIter`
uses SIMD batch decode internally; when disabled, falls back to scalar. The
public API doesn't change.

## Integration Design

### Decode: Chunked PackedIter

The key insight: `decode_four_unsafe` returns 4 decoded varints + total bytes
consumed. We can buffer these inside PackedIter transparently.

```
PackedIter (with simd feature):
  cursor: Cursor<'a>          - unchanged
  buf: [u64; 4]               - decode buffer (stack-allocated)
  buf_pos: u8                 - next index to yield from buf
  buf_len: u8                 - valid entries in buf (0-4)
```

`Iterator::next()` logic:
1. If `buf_pos < buf_len`: return `buf[buf_pos++]`
2. If remaining bytes >= 16 (safety margin for 4 max-length varints):
   call `decode_four_unsafe`, fill buf, advance cursor, return first
3. Else: fall back to scalar `cursor.read_varint()`

Step 2's safety margin: 4 varints × 10 bytes max = 40 bytes theoretical max,
but `decode_four_unsafe` reads a 16-byte SIMD register from the pointer. We
need `remaining >= 16` to avoid reading past the buffer. In practice, packed
fields inside a decompressed PrimitiveBlock always have sufficient trailing
data (the block is at least 32 KB typically), but we must be precise for
correctness.

**Important:** `decode_four_unsafe` requires the pointer to have at least 16
readable bytes from its start position (it loads an `__m128i`). For packed
fields that are the last field in a block, the underlying `Bytes` buffer may
not have 16 bytes of readable slack beyond the packed data. Options:
- Check `remaining >= 16` before SIMD path (correct but conservative - falls
  back to scalar for the last few varints in every packed field)
- Use the safe `decode()` function which does bounds checking (slower but no
  safety margin needed)
- Ensure decompression buffers always have 16 bytes of padding (requires
  changes to DecompressPool - invasive)

The `remaining >= 16` check is the pragmatic choice. For a typical packed field
of 8000 sint64 deltas averaging ~2 bytes each = ~16 KB, we only fall back to
scalar for the last few varints. The SIMD fast path covers >99.9% of varints.

### Encode: Per-value SIMD encode

varint-simd doesn't offer batch encode, so the approach is simpler:
replace individual `encode_varint(buf, val)` calls with
`encode_to_slice(val, &mut buf[len..])` in the packed encode loops.

This requires changing `encode_packed_*` to work with a pre-allocated
buffer instead of `Vec::push`. The current pattern:

```rust
// Current: push one byte at a time
fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}
```

SIMD replacement:
```rust
// With simd: write to pre-sized slice
fn encode_varint_simd(buf: &mut Vec<u8>, value: u64) {
    // ensure capacity for max 10-byte varint
    buf.reserve(10);
    let len = buf.len();
    let written = varint_simd::encode_to_slice(value, &mut buf[len..len+10]);
    buf.set_len(len + written);  // unsafe
}
```

This avoids per-byte push overhead and uses SSE2 for the encode.

For the packed encode functions, a better approach is to pre-reserve the
scratch buffer to `values.len() * 10` (max possible), then encode all values
into the slice without per-varint capacity checks:

```rust
fn encode_packed_sint64_simd(buf: &mut Vec<u8>, scratch: &mut Vec<u8>,
                              field: u32, values: &[i64]) {
    scratch.clear();
    scratch.reserve(values.len() * 10);
    let mut pos = 0;
    for &v in values {
        let n = varint_simd::encode_to_slice(
            zigzag_encode_64(v),
            &mut scratch.spare_capacity_mut()[pos..pos+10]
        );
        pos += n;
    }
    unsafe { scratch.set_len(pos); }
    encode_bytes_field(buf, field, scratch);
}
```

### Platform Gating

```toml
# protohoggr/Cargo.toml
[features]
default = []
simd = ["varint-simd"]

[target.'cfg(target_arch = "x86_64")'.dependencies]
varint-simd = { version = "0.4", optional = true }
```

At compile time:
- `cfg(all(feature = "simd", target_arch = "x86_64"))` → SIMD paths
- Otherwise → scalar (current code, unchanged)

Runtime SSSE3 detection is not needed: SSSE3 has been baseline on x86_64 since
Intel Core 2 (2006) / AMD Bulldozer (2011). All server-class hardware and all
CI runners have it. If we ever need aarch64, that's a separate feature flag
with NEON intrinsics (varint-simd doesn't support ARM).

### pbfhogg integration

```toml
# Cargo.toml (workspace root)
[features]
simd = ["protohoggr/simd"]
```

pbfhogg enables the feature; no code changes needed in pbfhogg itself since
PackedIter's API doesn't change.

## Profiling Results (Denmark 487 MB, commit fc260dc, plantasjen)

### Baseline benchmarks

| Benchmark | Mode | Elapsed |
|-----------|------|---------|
| bench read | sequential | 2,921 ms |
| bench read | parallel | 477 ms |
| bench read | pipelined | 1,505 ms |
| bench write | sync-none | 8,136 ms |
| bench write | sync-zlib:6 | 16,902 ms |
| bench write | pipelined-none | 7,259 ms |
| bench write | pipelined-zlib:6 | 7,399 ms |

### Hotpath function-level breakdown

**tags-count** (sequential decode + tag iteration, 5.8s wall):

| Function | Total | % wall | Calls |
|----------|-------|--------|-------|
| `decompress_blob` | 3.66s | 66.5% | 7,397 |
| `run_pipeline` | 1.83s | 33.2% | 1 |
| `wire::parse` | 274ms | 5.0% | 14,792 |
| `block::new` | 228ms | 4.3% | 7,396 |

Decompression dominates (66.5%). Wire parsing (includes varint decode for
message fields) is only 5%. The ~28% gap between instrumented functions and
wall time is the consumer callback (element iteration + tag counting) -
includes `PackedIter` varint decode of tags, delta accumulation, string
table lookups. Not separately instrumented.

**cat** (full decode + re-encode with zlib to /dev/null, 7.3s wall):

| Function | Total | % wall | Calls | Per-call |
|----------|-------|--------|-------|----------|
| `frame_blob_into` (compress) | 21.6s | 347% | 7,361 | 2.94ms |
| `add_node` | 3.63s | 58% | 52,489,653 | 69 ns |
| `decompress_blob` | 3.62s | 58% | 7,398 | 489 µs |
| `take_owned` (encode block) | 2.12s | 34% | 7,374 | 288 µs |
| `add_way` | 1.64s | 26% | 6,616,097 | 248 ns |
| `wire::parse` | 169ms | 2.7% | 14,792 | 11.5 µs |
| `block::new` | 228ms | 3.6% | 7,396 | 30.8 µs |

Totals >100% because `frame_blob_into` and `add_node`/`add_way`/`take_owned`
run on parallel rayon threads. The encode-side functions (`add_node` 3.63s +
`add_way` 1.64s + `take_owned` 2.12s = 7.39s) are where varint encode lives.

`add_node` at 69 ns/call does: delta computation + zigzag + varint encode for
id/lat/lon/metadata + string table insert for tags. `take_owned` at 288 µs
calls `encode_dense_nodes_group()` which runs 10 `encode_packed_*` calls
over ~8000 nodes = ~75K varints per block.

**check-refs** (pipelined read + ID set building, 7.4s wall):

| Function | Total | % wall | Calls |
|----------|-------|--------|-------|
| `decompress_blob` | 2.17s | 29.4% | 7,391 |
| `block::new` | 105ms | 1.4% | 7,390 |
| `wire::parse` | 62ms | 0.8% | 14,780 |

Consumer work (ID set operations) dominates at ~70%.

**merge-none** (passthrough, 534ms wall):

| Function | Total | % wall | Calls |
|----------|-------|--------|-------|
| `rewrite_block_parallel` | 820ms | 156% | 555 |
| `read_raw_frame` | 154ms | 29% | 7,401 |
| `take_owned` | 143ms | 27% | 559 |
| `frame_blob_into` | 85ms | 16% | 573 |
| `scan_block_ids` | 54ms | 10% | 584 |

Only 555 of 7,401 blobs rewritten (~7.5%). Varint cost is negligible.

### Analysis: where varint time actually lives

The hotpath instrumentation captures function-level timing but does NOT
instrument the inner `PackedIter`/`PackedSint64Iter` iteration. Varint
decode time is split across two uninstrumented locations:

1. **Read-side element iteration** - `DenseNodeIter::next()` calls
   `PackedSint64Iter::next()` for id/lat/lon deltas. This happens inside
   the consumer callback (the gap between `decompress_blob + wire::parse`
   and total wall time). In tags-count, this gap is ~1.5s.

2. **`wire::parse`** - parses protobuf message fields (field tags +
   length-delimited reads). Some varint decode here, but mostly
   `read_varint` for tags and `read_len_delimited` for field boundaries.
   At 274ms for tags-count, this is a small fraction.

On the encode side, varint encode is embedded in:
- `add_node` (69 ns/call × 52.5M = 3.63s) - per-element delta + varint
- `add_way` (248 ns/call × 6.6M = 1.64s) - per-element delta + varint
- `take_owned` (288 µs/call × 7,374 = 2.12s) - bulk `encode_packed_*`

### Revised estimates

The old box3/box6 estimates assumed varint was 15-50% of wall time. The
hotpath data tells a different story:

**Read side:** Decompression is 30-67% of wall time. `wire::parse` (the
only instrumented varint site) is 1-5%. The uninstrumented element
iteration gap is ~25-30%, but that includes delta accumulation, string
table lookups, tag iteration - not just varint decode. Realistic varint
decode fraction: **~10-15% of sequential read wall time** (most of the
element iteration gap is PackedIter, but the per-element work around it
is comparable).

**Write side (cat with zlib):** `add_node` + `add_way` + `take_owned` =
7.39s of encode work. Varint encode is a fraction of each (string table
lookups, delta computation, and memory management are significant). On the
`take_owned` path, `encode_packed_sint64` over 8000 values is the densest
varint site. Realistic varint encode fraction: **~30-50% of encode work**,
but encode work runs on parallel threads so wall-clock impact depends on
whether it's the bottleneck.

**Revised end-to-end predictions with 2× SIMD varint speedup:**

| Path | Varint fraction | SIMD savings | Wall-clock delta |
|------|----------------|--------------|-----------------|
| Read sequential (zlib) | ~10-15% | ~5-7% | ~150-200ms (Denmark) |
| Write sync-none | ~15-25% | ~7-12% | ~600-1000ms (Denmark) |
| Write pipelined-zlib | ~5-10% of bottleneck | ~2-5% | ~150-370ms (Denmark) |
| Merge (passthrough) | <2% | <1% | negligible |

At planet scale (160× Denmark), the write sync-none savings would be
~96-160s - still meaningful but much less than the original ~175s estimate.

## Microbenchmark Results (protohoggr, go / no-go)

Criterion benchmarks in protohoggr comparing scalar (current) vs varint-simd.
Test data: 8000 sint64 values, two scenarios - small deltas (1-byte varints,
typical of dense node id/lat/lon) and large deltas (3-byte varints, typical
of way refs).

### Decode (8000 packed sint64)

| Implementation | Small (1B varints) | Large (3B varints) |
|----------------|--------------------|--------------------|
| Scalar `PackedSint64Iter` | **4.36 µs (0.54 ns/val)** | **20.9 µs (2.61 ns/val)** |
| SIMD batch4 (decode_four, u16) | 10.2 µs (1.28 ns/val) | n/a (won't fit u16) |
| SIMD batch2 (decode_two, u32) | 17.5 µs (2.19 ns/val) | 20.8 µs (2.60 ns/val) |
| SIMD single (safe decode) | 27.7 µs (3.46 ns/val) | 27.8 µs (3.47 ns/val) |

Scalar is **2.3× faster** than the best SIMD batch for 1-byte varints,
essentially tied for 3-byte varints, and **6.3× faster** than SIMD single.

### Encode (8000 packed sint64)

| Implementation | Small (1B varints) | Large (3B varints) |
|----------------|--------------------|--------------------|
| Scalar `encode_packed_sint64` | **7.69 µs (0.96 ns/val)** | **19.6 µs (2.45 ns/val)** |
| SIMD `encode_to_slice` | 26.9 µs (3.36 ns/val) | 29.1 µs (3.64 ns/val) |

Scalar is **3.5× faster** for 1-byte and **1.5× faster** for 3-byte.

### Why scalar wins

The 1-byte fast path in `Cursor::read_varint()` is the key:

```rust
let b = self.data[self.pos];
if b < 0x80 {
    self.pos += 1;
    return Ok(u64::from(b));
}
```

For the dominant OSM case (dense node deltas where most varints are 1 byte),
this single branch is perfectly predicted by the CPU. The entire decode is:
one array access, one comparison, one increment, one zero-extend. SIMD's
`_mm_loadu_si128` + `_mm_movemask_epi8` + PSHUFB shuffle + mask lookup
can't compete with a single predicted branch + scalar load.

For 3-byte varints (way refs, larger deltas), scalar's byte-at-a-time loop
runs 3 iterations with predictable branching, roughly matching SIMD batch2.
The SIMD setup overhead exactly cancels the per-varint savings.

On the encode side, scalar's `buf.push((value as u8) | 0x80)` loop benefits
from the same branch prediction. The SIMD encode_to_slice must do SSE2
operations (shift, mask, shuffle) regardless of varint length - it can't
short-circuit the 1-byte case.

## Verdict

**P3-20 is closed. SIMD varint acceleration is not worth pursuing.**

The microbenchmark results decisively show that the current scalar
implementation in protohoggr already achieves near-optimal throughput for
LEB128 varints. The CPU's branch predictor perfectly handles the 1-byte
fast path that dominates OSM data (dense node deltas). Adding varint-simd
would make performance *worse*, not better, while introducing an external
dependency, `unsafe` code, and platform restrictions.

The original estimates (~25s read-side + ~175s write-side savings at planet
scale) assumed a 2× SIMD speedup. The actual measurement shows scalar is
2-6× *faster* than SIMD for the dominant case, so the projected savings
are negative.

**What to do instead:**

- The ~10-15% of read wall time spent on varint decode is already well-
  optimized. Further gains require attacking decompression (66% of wall
  time) or improving parallelism.
- For the encode side, the `add_node` (69 ns/call) and `take_owned`
  (288 µs/call) paths could benefit from reducing string table overhead,
  delta computation, or memory management - not varint encode speed.
- Option 3 (scalar multi-byte fast paths) is also unlikely to help given
  that the 1-byte fast path already covers the dominant case and the
  branch predictor handles the 2-3 byte cases efficiently.

## Reference: SIMD Patterns from Rust Ecosystem

Analysis of four production SIMD crates to inform our implementation
strategy. Each offers different lessons for the varint decode/encode problem.

### Crate Overview

| Crate | Domain | Key SIMD lesson for us |
|-------|--------|----------------------|
| **zlib-rs** | Compression | Feature detection, auto-vectorization hints, safety discipline |
| **memchr** | Byte search | Buffer tail safety, `Vector` trait abstraction, `#[target_feature]` propagation |
| **bitpacking** | Integer compression | Batch-then-iterate, abstracted SIMD ops, runtime enum dispatch |
| **simd-json** | JSON parsing | Bitmask-first architecture, `movemask` for byte scanning, write-past-end trick |

---

### A. Buffer Tail Safety (from memchr)

The #1 concern for our SIMD varint decode: what happens when <16 bytes
remain but we need to load a 16-byte `__m128i`?

memchr uses **three complementary strategies**. It never overreads.

**Strategy 1: Early scalar fallback for short inputs.**
AVX2 searcher has a three-tier cascade: <16 bytes → byte-at-a-time,
16-31 bytes → SSE2, ≥32 bytes → AVX2. Each tier is checked at the entry
point before the main loop.

**Strategy 2: Overlapping final load.**
After the aligned main loop, if bytes remain, memchr does a backward-shifted
unaligned load that overlaps with already-searched data:

```rust
// memchr: arch/generic/memchr.rs:219-229
if cur < end {
    cur = cur.sub(V::BYTES - end.distance(cur));  // shift back
    return self.search_chunk(cur, topos);          // overlap is harmless
}
```

This does NOT work for varint decode (stateful - we need exact start
positions). But the insight is useful: any idempotent SIMD scan can use
overlapping loads to avoid scalar tails.

**Strategy 3: Mask-based overlap handling.**
For substring search (`packedpair.rs`), memchr masks out bits in the
movemask result that correspond to previously-processed bytes. This way
an overlapping load doesn't produce duplicate matches.

**For our PackedIter:** We must use Strategy 1 (scalar fallback for tail).
When `remaining < 16`, fall back to `cursor.read_varint()`. Alternative:
copy remaining bytes into a zero-padded 16-byte stack buffer and load from
that (simd-json does this). The `remaining >= 16` check is simplest and
covers >99.9% of varints for typical 16 KB packed fields.

---

### B. Feature Detection (consensus across all four crates)

All four crates converge on the same hybrid approach:

**Compile-time fast path:** When `target_feature` is statically enabled
(e.g., `-C target-cpu=native`), the check compiles to `return true`:

```rust
#[cfg(target_feature = "avx2")]
return true;
```

**Runtime detection with caching:**

| Crate | Cache type | Cache scope |
|-------|-----------|------------|
| zlib-rs | `AtomicU32` (0/1/2) | Per-feature function |
| memchr | `AtomicPtr<()>` (function pointer) | Per-dispatch-site static |
| simd-json | `AtomicPtr<()>` (function pointer) | Per-dispatch-site static |
| bitpacking | Enum discriminant | Per-struct instance |

The `AtomicPtr` trampoline pattern (memchr + simd-json) is the most
sophisticated: first call probes CPUID and atomically replaces the function
pointer, all subsequent calls load with `Ordering::Relaxed` - effectively
zero overhead. simd-json's version:

```rust
static FN: AtomicPtr<()> = AtomicPtr::new(detect as FnRaw);

fn detect(...) -> T {
    let fun = if is_x86_feature_detected!("avx2") { avx2_impl }
              else if is_x86_feature_detected!("sse4.2") { sse42_impl }
              else { scalar_impl };
    FN.store(fun as Fn, Ordering::Relaxed);
    fun(...)  // also execute this first call
}
```

**For protohoggr:** The `AtomicPtr` trampoline is overkill for us since
PackedIter is constructed per-packed-field, not per-call. The simpler
zlib-rs `AtomicU32` pattern (or even bitpacking's enum-in-struct) suffices.
But if we later add SIMD to standalone `Cursor::read_varint()` (called
individually), the trampoline would be appropriate.

---

### C. `#[target_feature]` Propagation (from memchr)

memchr solves the "inlining across target_feature boundaries" problem with
a deliberate two-layer calling convention:

```rust
// Layer 1: PUBLIC, #[inline], NO target_feature
// → CAN be inlined into caller code (which lacks avx2)
// → Handles short-input scalar fallback (fast path inlined!)
#[inline]
pub unsafe fn find_raw(&self, start: *const u8, end: *const u8) -> Option<*const u8> {
    if end.distance(start) < __m128i::BYTES {
        return generic::fwd_byte_by_byte(start, end, ...);  // inlined
    }
    self.find_raw_impl(start, end)  // NOT inlined (call)
}

// Layer 2: PRIVATE, #[target_feature], #[inline] (not always)
// → Gets AVX2/SSE2 codegen from the annotation
// → Called via function call, never inlined into non-target code
#[target_feature(enable = "sse2")]
unsafe fn find_raw_impl(&self, start: *const u8, end: *const u8) -> Option<*const u8> {
    self.0.find_raw(start, end)  // generic code, #[inline(always)]
}
```

The generic algorithm code (`arch/generic/memchr.rs`) uses `#[inline(always)]`
everywhere so it gets inlined *into* the `#[target_feature]` function,
inheriting its SIMD codegen. The trait doc states this explicitly:
"All implementations should avoid marking routines with `#[target_feature]`
and instead mark them as `#[inline(always)]`."

**For protohoggr:** Apply this pattern to our chunked PackedIter. The
`Iterator::next()` method is `#[inline]` (no target_feature) and handles
the buffer-hit and scalar-fallback cases. The SIMD refill path is a
separate `#[target_feature(enable = "ssse3")]` function called when the
buffer is empty.

---

### D. Portable SIMD Abstraction (from memchr + bitpacking)

**memchr's `Vector` trait** (`vector.rs`, ~500 lines):

```rust
pub(crate) trait Vector: Copy + Debug {
    const BYTES: usize;    // 16 or 32
    const ALIGN: usize;    // BYTES - 1
    type Mask: MoveMask;

    unsafe fn splat(byte: u8) -> Self;
    unsafe fn load_aligned(data: *const u8) -> Self;
    unsafe fn load_unaligned(data: *const u8) -> Self;
    unsafe fn movemask(self) -> Self::Mask;
    unsafe fn cmpeq(self, vector2: Self) -> Self;
    unsafe fn and(self, vector2: Self) -> Self;
    unsafe fn or(self, vector2: Self) -> Self;
}
```

Implementations for `__m128i`, `__m256i`, `uint8x16_t` (NEON), `v128` (WASM).
The `MoveMask` trait separately abstracts the scalar bitmask result
(different for NEON which produces 4-bits-per-lane vs x86's 1-bit-per-lane).

**bitpacking's approach** is simpler but effective: define `DataType` and
module-level free functions (`load_unaligned`, `op_or`, `op_and`, `set1`,
`left_shift_32`, `right_shift_32`) that differ between SSE3/NEON/scalar
modules. The core algorithm is written once in a macro and works with all
backends because it only calls these abstracted functions.

The scalar fallback uses `type DataType = [u32; 4]` - a plain array that
mirrors SIMD lane layout. All operations are element-wise loops. Same
macro-generated code works on both SIMD and scalar.

**For protohoggr:** We don't need a full Vector trait since varint-simd
handles the intrinsics. But if we hand-roll (Option 2), memchr's trait is
the right abstraction level. For portability to non-x86, bitpacking's
`DataType = [u64; 4]` scalar fallback is a good pattern - write the batch
decode algorithm once, it works with and without SIMD.

---

### E. Batch-then-Iterate Pattern (from bitpacking)

bitpacking's `Sink` trait bridges SIMD batch output to per-element
consumption:

```rust
pub trait Sink {
    unsafe fn process(&mut self, data_type: DataType);  // receives 4 decoded u32s
}
```

The `Store` sink writes each `DataType` to an output pointer and advances.
The `DeltaIntegrate` sink additionally performs prefix-sum integration
before storing.

Decode processes blocks in 32 unrolled iterations, each producing one
`DataType` (4 integers). There is NO tail handling - the API requires
exact block-size inputs and panics otherwise.

**For protohoggr:** Our chunked PackedIter is the equivalent of the Sink
pattern, but with built-in tail handling. The `[u64; 4]` stack buffer in
PackedIter plays the role of the Sink's output - SIMD fills it in batches,
Iterator drains it one at a time.

---

### F. Bitmask-First Architecture (from simd-json)

simd-json's core insight: **SIMD classifies bytes in bulk into scalar
bitmasks, then scalar code processes the bitmasks.** The two-stage pipeline:

1. **Stage 1 (SIMD):** Load 64 bytes, produce 64-bit bitmasks of structural
   characters / whitespace / quotes via `movemask` + "shufti" algorithm
   (`_mm256_shuffle_epi8` as 16-entry LUT).

2. **Stage 2 (scalar):** Walk the bitmask positions with `trailing_zeros()`
   + `bits &= bits.wrapping_sub(1)` (x86 BLSR instruction) to extract
   positions, then process each structural character.

**For varint decode:** Continuation bit scanning is *simpler* than JSON
structural char detection. We only need to test bit 7 of each byte:

```rust
// Load 16 bytes of packed varint data
let chunk = _mm_loadu_si128(ptr);
// Extract MSB (continuation bit) of each byte - one instruction
let mask = _mm_movemask_epi8(chunk) as u32;
// Invert: 1 = varint terminates here (MSB=0)
let term_mask = !mask & 0xFFFF;
// Count varints in this 16-byte window
let count = term_mask.count_ones();
```

No shuffle tables needed for the *scanning* step. PSHUFB is only needed for
the *rearrangement* step (gathering payload bytes, stripping continuation
bits) - and varint-simd already handles that.

**The `flatten_bits` pattern** for extracting positions from a bitmask:

```rust
while bits != 0 {
    let pos = bits.trailing_zeros();   // position of lowest set bit
    bits &= bits.wrapping_sub(1);       // clear it (BLSR)
    output[i] = base + pos;
    i += 1;
}
```

simd-json batches this 8-at-a-time with SIMD add + store for the position
array. The **write-past-end trick** (`reserve(N)` + unchecked writes +
`set_len(actual)`) eliminates bounds checks.

---

### G. Auto-Vectorization Hints (from zlib-rs)

zlib-rs's `slide_hash.rs` uses `#[target_feature]` on *plain Rust code*
(no intrinsics) to let LLVM auto-vectorize with wider registers:

```rust
#[target_feature(enable = "avx2", "bmi2", "bmi1")]
pub unsafe fn slide_hash_chain(table: &mut [u16], wsize: u16) {
    // Same generic Rust code, but compiler uses 256-bit ops
    super::generic_slide_hash_chain::<64>(table, wsize);
}
```

**For protohoggr encode:** Worth trying on `encode_packed_*` loops as a
free experiment. The encode loop (zigzag + shift + mask + push) might
auto-vectorize if the compiler can prove the loop body is independent
across iterations. Even if it doesn't fully vectorize, the target_feature
annotation enables other optimizations (e.g., BMI2 for shift-heavy code).

---

### H. Safety Discipline (consensus)

| Practice | zlib-rs | memchr | bitpacking | simd-json |
|----------|---------|--------|------------|-----------|
| `unsafe_op_in_unsafe_fn = "deny"` | Yes | No | No | No |
| `// SAFETY:` comments on unsafe blocks | Yes | Yes (thorough) | Minimal | Minimal |
| Safe public API wrapping unsafe | Yes | Yes (Option return) | Yes (trait) | Yes |
| `#[allow(unused_unsafe)]` for future compat | Yes | No | Yes (crate-level) | No |

memchr's `Option`-returning constructors are notable: `One::new(needle)`
returns `None` if the required SIMD features aren't available, making
misuse impossible at the type level.

**For protohoggr:** Add `unsafe_op_in_unsafe_fn = "deny"` to workspace
lints. Use `// SAFETY:` comments on every unsafe block (memchr standard).
The feature-flag approach (compile-time cfg, not Option constructors) is
more appropriate since we have a single target (x86_64 server).

---

### I. Module Organization (consensus)

All four crates follow the same pattern:

```
src/
  arch/  (or impls/)
    generic/        - algorithm parameterized by Vector trait or DataType
    x86_64/
      sse2.rs       - thin wrapper, #[target_feature], calls generic
      avx2.rs       - thin wrapper, #[target_feature], calls generic
    aarch64/
      neon.rs       - thin wrapper, #[target_feature], calls generic
    scalar.rs       - fallback, same interface, no SIMD
  cpu_features.rs   - centralized detection + caching
```

Architecture-specific modules are `#[cfg]`-gated at the `mod` declaration.
The generic code uses `#[inline(always)]` to be inlined into the
target_feature wrapper, inheriting its codegen.

**For protohoggr:** Same structure if we hand-roll (Option 2). With
varint-simd (Option 1), the structure is simpler - just a `cfg`-gated
SIMD path inside PackedIter's existing module.

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| varint-simd unmaintained | Low | Med | Feature-flag; can replace with hand-rolled |
| Unsafe memory read past buffer | Med | High | Conservative `remaining >= 16` check |
| No improvement (CPU OoO already fast) | Med | Med | Profile first, only implement if >30% |
| aarch64 needed later | Low | Med | Separate feature flag + NEON path |
| Breaks zero-dep guarantee of protohoggr | N/A | Low | Optional feature flag, off by default |

## Implementation Phases

**Phase 0: Profile** (this should happen first)
- Microbenchmark varint decode/encode in protohoggr
- Measure actual varint fraction in hotpath

**Phase 1: Decode-side SIMD in PackedIter** (highest value)
- Add `simd` feature to protohoggr
- Implement chunked decode buffer in PackedIter
- All consumers (DenseNodeIter, WayRefIter, etc.) benefit automatically
- Run protohoggr microbenchmark + pbfhogg read benchmark

**Phase 2: Encode-side SIMD in encode_packed_*** (lower priority)
- Replace encode_varint calls in encode_packed_{sint64,int32,sint32,uint32}
- Pre-reserve scratch buffers, use encode_to_slice
- Run pbfhogg write benchmark

**Phase 3: Scalar fast-path improvements** (complements SIMD, helps non-x86)
- Add 2-byte and 3-byte fast paths to Cursor::read_varint()
- Benefits all platforms including when SIMD feature is disabled
- Low effort, low risk, always-on

## References

### SIMD varint
- [varint-simd](https://github.com/as-com/varint-simd) - SIMD LEB128 for Rust (v0.4.1, stable, SSSE3)
- [Stream VByte (Bazhenov)](https://www.bazhenov.me/posts/rust-stream-vbyte-varint-decoding/) - SIMD integer compression (incompatible wire format, but good background)

### Reference crates analyzed
- [zlib-rs](https://github.com/trifectatechfoundation/zlib-rs) - feature detection, auto-vectorization hints, `unsafe_op_in_unsafe_fn` discipline
- [memchr](https://github.com/BurntSushi/memchr) - buffer tail safety, `Vector`/`MoveMask` trait abstraction, `#[target_feature]` propagation pattern
- [bitpacking](https://github.com/quickwit-oss/bitpacking) - batch-then-iterate Sink pattern, abstracted DataType ops, runtime enum dispatch
- [simd-json](https://github.com/simd-lite/simd-json) - bitmask-first architecture, `movemask` for byte scanning, `AtomicPtr` trampoline, write-past-end trick

### Prior analysis (deleted, recoverable from git)
- notes/perf-review/box3-wire-parsing.md (commit 1e90eb2^) - varint volume analysis
- notes/perf-review/box6-block-builder.md (commit 1e90eb2^) - encode-side floor analysis
- notes/perf-review/cross-reference-synthesis.md (commit 1e90eb2^) - P3-20 priority listing
