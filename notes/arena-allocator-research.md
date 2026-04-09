# Arena Allocator Research

## Problem Statement

PrimitiveBlock decoding allocates many small, short-lived objects per block
(string table entries, group range vectors, tag slices, ref vectors). All
allocations die together when the block is done. At planet scale (520K blobs),
`parse_and_inline` generates ~14 GB of cumulative alloc churn through
malloc/free cycles.

Alloc profiling (commit `ec43a8b`, Japan, 30K blobs):

| Function | Cumulative alloc | % of total | Per-block avg |
|----------|-----------------|------------|---------------|
| `parse_and_inline` | 829 MB | 41% (tags-filter) | ~27 KB |
| `take_owned` (BlockBuilder) | 76-292 MB | 3-4% | ~2-9 KB |
| `frame_blob_into` (zlib) | 48-113 MB | 1-2% | ~1-3 KB |
| Worker totals (per worker) | 29-60 MB | — | Pool reuses effectively |

The worker threads are efficient (~6 MB retained each) thanks to
`DecompressPool` buffer reuse. The main thread retains 1.9-4.6 GB
from IdSetDense sets (monotonic growth, not an arena target).

## Current State: parse_and_inline IS a Hand-Rolled Arena

`WireBlock::parse_and_inline` in `src/read/wire.rs` already uses an arena
pattern: it appends string table entries and group ranges as raw LE bytes
to the decompressed `Vec<u8>` buffer. Element types (`Node`, `Way`,
`Relation`) are zero-copy views into this buffer. Tag/ref/member iterators
are lazy — no heap allocation during element iteration.

The remaining per-block allocations are:
1. Two temp `Vec<(u32, u32)>` for `st_entries` and `group_entries` in
   `parse_and_inline` (~line 129-130 of `src/read/wire.rs`). These are
   built during parsing, then copied into the buffer as inline LE bytes.
   Freed on the same thread after the copy. 30K blocks × ~27 KB avg =
   ~810 MB — this is the bulk of the 829 MB.
2. The decompressed buffer `Vec<u8>` — already pool-recycled via
   `DecompressPool`.
3. Downstream command allocations (BlockBuilder scratch, tag/ref buffers
   passed to `add_node`/`add_way`/`add_relation`).

## Rust Arena Allocator Landscape

### Tier 1: Production-ready

**bumpalo (v3.20.2)** — The standard Rust arena. Zero dependencies.
Stable Rust 1.71+. Heterogeneous types via `bump.alloc(value)`.
Reset/reuse via `bump.reset()`. Used by wasm-bindgen, swc, etc.
With `allocator-api2` feature, implements the `Allocator` trait on
stable, enabling `Vec<T, &Bump>`.

```rust
let bump = Bump::with_capacity(1_500_000);
let s: &str = bump.alloc_str("highway");
let slice: &[u32] = bump.alloc_slice_copy(&[1, 2, 3]);
bump.reset(); // O(1), keeps backing memory
```

Catch: collections carry `'bump` lifetime (`Vec<'bump, T>`,
`String<'bump>`). This would propagate into any struct holding them.
Values allocated with `bump.alloc()` do NOT get Drop run on reset —
fine for Copy/wire-format data.

**bump-scope (v2.2.0)** — Scoped sub-allocations with auto cleanup.
Zero dependencies. Stable Rust 1.85+. Claims slightly more optimized
than bumpalo (callgrind benchmarks). Provides `BumpPool` for thread
pools. Scoped allocations auto-reset when scope exits.

```rust
let mut bump: Bump = Bump::new();
bump.scoped(|bump| {
    let text = bump.alloc_str("hello");
    // freed when scope exits
});
```

Catch: two lifetime parameters on collections (`'a` scope + `'bump`
allocator). Higher MSRV than bumpalo.

### Tier 2: Viable alternatives

**scoped-arena (v0.4.1)** — Runs destructors on reset (unlike bumpalo).
Sub-scopes for nested lifetime regions. Zero dependencies. By zakarumych
(author of `allocator-api2`). Less popular than bumpalo.

**arena-b (v1.0.0)** — Feature-rich: checkpoint/rewind, `virtual_memory`
(mmap backing), `slab` (size-class caching), `thread_local` mode (20-40%
faster claimed), `debug` (use-after-rewind detection). Very new, unknown
author. Claims ~5ns per alloc, 10x faster than Box for small allocs.

**fastarena (v0.1.3)** — Transaction-oriented with RAII rollback.
`ArenaVec<T>` for growable arena-backed vectors. Optional drop-tracking.
Very new.

### Tier 3: Not suitable

**typed-arena (v2.0.2)** — Single-type only, no reset. Not applicable.
**toolshed (v0.8.1)** — No reset support. 6 years stale.
**stable-arena (v0.2.0)** — Rustc port, no reset. DroplessArena is
interesting conceptually but missing the reset capability we need.
**safe-bump (v0.2.1)** — Zero unsafe, but MSRV 1.93 (nightly-only).
**copy_arena (v0.1.1)** — Copy-only, no reset. Minimal.

### Hand-rolled (~50 lines)

A minimal bump allocator using `Vec<u8>` as backing:

```rust
pub struct BumpArena {
    buf: Vec<u8>,
    pos: usize,
}

impl BumpArena {
    pub fn with_capacity(cap: usize) -> Self {
        Self { buf: vec![0u8; cap], pos: 0 }
    }

    pub fn alloc<T: Copy>(&mut self, val: T) -> &T {
        let align = std::mem::align_of::<T>();
        let size = std::mem::size_of::<T>();
        self.pos = (self.pos + align - 1) & !(align - 1);
        let start = self.pos;
        self.pos += size;
        assert!(self.pos <= self.buf.len(), "arena overflow");
        unsafe {
            let ptr = self.buf.as_mut_ptr().add(start) as *mut T;
            ptr.write(val);
            &*ptr
        }
    }

    pub fn reset(&mut self) { self.pos = 0; }
}
```

Pros: zero dependencies, full control, 50 lines, auditable in minutes.
Cons: no community review, no fuzzing (bumpalo has had many soundness
fixes from wide usage). Only Copy types without a drop registry.

### Allocator API (nightly)

`allocator_api` remains unstable (75 open issues in the working group).
The `allocator-api2` crate (v0.4.0, 250M+ downloads) provides a stable
polyfill. Bumpalo with `allocator-api2` feature enables
`std::vec::Vec<T, &Bump>` on stable. Same lifetime pollution as
bumpalo's own collections.

### String Interning

**lasso (v0.7.3)**, **string-interner (v0.19.0)** — OSM tag keys are
repetitive (~50-200 unique per block). But the current design already
avoids allocating tag key strings: `TagIter` lazily resolves via
`WireStringTable::get()` returning `&str` into the decompressed buffer.
Interning would only help if commands materialize tag keys into owned
Strings (e.g., `FxHashMap<String, ...>` in tags_count). Command-level
optimization, not per-block decode.

### C++ Protobuf Arenas (Reference)

Google's C++ protobuf library uses per-message arenas: all sub-objects
are arena-allocated, freed as a unit when the message is destroyed.
Reports 40-60% reduction in malloc calls. The gold standard for this
pattern. See https://protobuf.dev/reference/cpp/arenas/

## The Lifetime Pollution Problem

Every arena-based approach in Rust faces the same fundamental tension:
arena-allocated references carry the arena's lifetime (`'bump`), which
propagates into any struct holding them. For PrimitiveBlock, this would
mean `PrimitiveBlock<'bump>` → `Element<'bump>` → classify closures
capture `'bump` → etc.

The current codebase avoids this via lifetime erasure: `WireBlock<'static>`
uses `Bytes` (reference-counted) to erase the decompressed buffer's
lifetime. The inline entries pattern (appending to the buffer) works
within this model. A full arena would require either:

1. **Lifetime parameter propagation** — add `'bump` to PrimitiveBlock
   and all consumers. Large API surface change.
2. **Unsafe lifetime erasure** — same `'static` trick used for `Bytes`.
   Sound if the arena outlives all references (guaranteed by per-block
   scoping in workers).
3. **Index-based access** — return offsets/indices instead of references.
   Already done for inline entries. No lifetime params needed.
4. **Scoped closures** — the arena exists only within a closure scope,
   and all references are consumed before the closure returns.

Option 3 (index-based) is what `parse_and_inline` already does. Option 4
(scoped) maps well to `parallel_classify_phase` workers where the block
is created, classified, and dropped within one iteration.

## Practical Approach: Incremental, Bottom-Up

### Step 1: Thread-local scratch Vecs — DONE

Added `parse_and_inline_with_scratch` and `from_vec_pooled_with_scratch`
that accept caller-provided `Vec<(u32, u32)>` scratch buffers. Buffers
are cleared per block but retain capacity across blocks.

Measured impact (Japan, tags-filter):
- `parse_and_inline`: 829 MB → 48 MB (**94% reduction**)
- Worker alloc per thread: 29 MB → 21 MB
- Planet estimate: ~14 GB → ~800 MB churn eliminated

Coverage: all `parallel_classify_phase` workers, `pread_execute` workers,
sequential BlobReader loops (node_stats, tags_count). Every
PrimitiveBlock construction path uses scratch.

### Columnar decode codegen analysis (commit e0b0780)

Assembly inspection (`RUSTFLAGS="-C target-cpu=native" --emit=asm`)
of `collect_matching_ids_bbox` (on `DenseNodeColumns`) confirms:
- **No autovectorization.** LLVM emits pure scalar: 4× `cmpl` + `jg`/`jl`
  conditional jumps per element. No `vpcmpgtd` or `vpand`.
- **Branchless `&` trick undone by LLVM.** The `(cmp) as u8 & (cmp) as u8`
  pattern is optimized back to short-circuit conditional jumps. LLVM
  sees through the `as u8` conversion.
- **`out.push(ids[i])` prevents vectorization.** The conditional push
  involves potential Vec reallocation — LLVM can't speculate past it.

Conclusion: explicit AVX2 intrinsics are the only path to vectorization
for this loop. However, the theoretical max gain is small:
- 8000 nodes × 500K blobs × ~5 cycles/element = ~6.5s total
- SIMD 8-wide would reduce to ~1s → saving ~5.5s
- Out of 198s Europe complete: **2.8% max improvement**
- Not worth the complexity (unsafe intrinsics, target_feature gates,
  fallback paths for non-AVX2) for 2.8%.

The columnar architecture is the right foundation — the win comes
when the classify loop is a larger fraction of total time (e.g., if
the write path is optimized and classify becomes the bottleneck),
or when we add more consumers that benefit from contiguous arrays
(multi-region classification, polygon PIP).

### Step 1a: Pre-reserve buffer capacity — DONE

Added `buf.reserve((st_entries.len() + group_entries.len()) * 8)` before
Phase 2 appending. This was expected to eliminate the 12.1 GB alloc in
`parse_and_inline_with_scratch`, but alloc profiling showed the 12.1 GB
is actually the decompression buffer's initial allocation (from
`pool_get_pub` or `decompress_pooled`), not growth during `extend`.
The inline entries (~2 KB) are tiny vs decompressed data (~50 KB) —
the pool-returned buffers already have sufficient capacity.

**Conclusion:** `parse_and_inline` has no remaining optimization
opportunity. The alloc churn is the decompression buffer itself,
which is already pool-recycled. Arena allocation would not help here.
pbfhogg is at libosmium parity for the decode path.

### Step 1b: write_single_* tag Vec — DONE (iterator API)

Iterator-based BlockBuilder API (commit `bb15e66`) eliminated the
per-element `tags.collect::<Vec>()`. Callers pass `element.tags()`
directly. Dual-buffer single-pass encoding for way/relation tag fields.
See [notes/blockbuilder-iterator-api.md](blockbuilder-iterator-api.md).

`write_single_node/way/relation` in `elements_pbf.rs` allocate
`tags.collect::<Vec<(&str, &str)>>()` per element. Called from sort
(sweep merge) and merge_pbf (k-way merge) — ~10B elements at planet.
Scratch reuse across calls fails: `Vec<(&str, &str)>` borrows `&str`
from different `OwnedNode`/`OwnedWay` each iteration. The compiler
can't verify that `.clear()` releases the old borrows before the next
call fills new ones.

Fix options:
1. Change `add_node`/`add_way`/`add_relation` in BlockBuilder to accept
   iterators (`impl Iterator<Item = (&str, &str)>`) instead of slices.
   Eliminates the intermediate Vec entirely. Larger API change.
2. Use `SmallVec<[(&str, &str); 16]>` to stack-allocate for typical
   tag counts (<16 tags). Avoids heap for the common case.
3. Use `bumpalo` per-element arena (overkill for this pattern).

### Step 2: Remaining scratch opportunities

See TODO.md "Scratch buffer reuse audit" for the full list:
- ~~Remaining `PrimitiveBlock::new()` call sites~~ — DONE (commit `ea1ab6e`)
- Geocode pass 3 bucket merge (3 Vecs × 20M iterations, ~1.4 GB)
- `scan_block_ids`/`scan_block_tags` per-blob Vecs
- Merge per-element Vecs (same lifetime issue as step 1b)
- Scanner group_starts (tiny, low priority)

### Step 3: Evaluate bumpalo for broader per-block allocation (medium risk)

After steps 1-2 eliminate the known allocation hotspots, re-run alloc
profiling to see what remains. If significant churn persists in
`parse_and_inline` or downstream, prototype bumpalo:

- Create a `BumpPool` (Vec<Bump>, analogous to DecompressPool)
- Each worker gets a Bump from the pool, uses it for per-block
  allocations, resets it after classification, returns it to the pool
- Use scoped closures (option 4 above) to contain the `'bump` lifetime
  within the worker's per-blob iteration

This is the path to the C++ protobuf arena model. The scoped closure
approach avoids lifetime pollution on PrimitiveBlock's public API.

### Step 4: Arena-backed columnar layout (research)

With a per-block arena in place, columnar decode becomes natural:
allocate contiguous `&[i64]` (IDs), `&[i32]` (lats), `&[i32]` (lons)
from the arena. The classify closure operates on these arrays directly.
This is the prerequisite for SIMD (Milestone B in the research roadmap).

### Step 5: Re-evaluate pipelined reader (research)

If arenas eliminate cross-thread retention (the arena is allocated and
freed as a single unit, no per-object free on a different thread), the
pipelined reader could be re-enabled for commands currently forced to
sequential. This would recover 30-100% throughput for diagnostic
commands (node_stats, tags_count) and potentially improve tags-filter
pass 2 (currently ~8s slower than pipelined due to pread overhead).

## Key References

- [Arenas and Rust](https://blog.reverberate.org/2021/12/19/arenas-and-rust.html)
  — Josh Haberman (protobuf team). Lifetime pollution analysis.
- [Arenas in Rust](https://manishearth.github.io/blog/2021/03/15/arenas-in-rust/)
  — Manish Goregaokar. typed-arena vs bumpalo comparison.
- [C++ Arena Allocation Guide](https://protobuf.dev/reference/cpp/arenas/)
  — Google protobuf. 40-60% malloc reduction benchmark.
- [Bump allocation chapter](https://rust-hosted-langs.github.io/book/chapter-simple-bump.html)
  — Writing Interpreters in Rust. Hand-rolled bump allocator walkthrough.
- [Allocator Designs](https://os.phil-opp.com/allocator-designs/)
  — Phil Opp. Bump, linked-list, slab design patterns.
