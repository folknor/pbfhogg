# `getparents` - optimization plan

Target: `pbfhogg getparents` - whole-file scan listing the ways and
relations that reference a given ID set. Input is a sorted PBF and
a set of IDs; output is the IDs of parent elements (and optionally
the parents' own elements via `--add-self`).

Drafted 2026-04-23 from a fresh read of
[`src/commands/getparents/`](../src/commands/getparents/) against
the modern pipeline primitives documented in
[`reference/pipeline.md`](../reference/pipeline.md) and
[`reference/pipelined-reader-paths.md`](../reference/pipelined-reader-paths.md).

## Current state (2026-04-23)

No `.brokkr/results.db` rows at planet scale yet.
`overnight.sh:276-278` runs `brokkr getparents --dataset planet
--bench 1`, `--hotpath`, `--alloc` tonight, so the baseline lands
morning of 2026-04-24. Denmark 400 ms, Japan 2.06 s
([`reference/performance.md:776`](../reference/performance.md#L776),
line 812).

Architecture today:

- **Pipelined decode** via `ElementReader::into_blocks_pipelined`
  (per
  [`reference/pipelined-reader-paths.md:99-107`](../reference/pipelined-reader-paths.md#L99)).
  Retention is solved by `DecompressPool`; this doc does not
  revisit that.
- **Blob-kind filtering** via `BlobFilter` to skip node-only blobs
  when the query doesn't need nodes. Per the 0.3.0 CHANGELOG entry:
  "~85 % of blobs at planet scale". The kind filter is element-type-
  only; no tag or bbox pre-screen.
- **Modern `IdSet`** (chunked sparse bitset, O(1) lookup). Correct
  primitive, no change needed.

The command is not "never optimised" - it was touched during the
0.3.0 pipeline reshape and uses the right primitives for its shape.
The remaining headline win is a blob-level fast path, not a
primitive swap.

**Critical history gate**: commit `c912e4d` tried converting the
pipelined reader to sequential decode and regressed 4.7x on Denmark
(1400 ms vs 300 ms). This is the load-bearing evidence for the
"do not convert pipelined-to-sequential" rule in
`pipelined-reader-paths.md`. *That rule targets sequential decode,
not pread-worker parallelism* - see opportunity #2 below.

## Opportunities

Ranked by expected headline impact.

### 1. `HeaderWalker` + `any_in_range()` blob-level fast path

Current: every way and relation blob is decompressed, every element
scanned, `IdSet::get` per ref. Most blobs contribute no matches.

Proposed: walk blob headers first with
[`HeaderWalker`](../src/read/header_walker.rs), use indexdata
`(min_id, max_id)` to pre-screen each blob against the query set via
`IdSet::any_in_range(min, max)`, and only pread + decompress blobs
that can contain a ref. Non-matching way / relation blobs are
skipped entirely.

This is exactly the pattern
[`getid`](../src/commands/getid/) shipped in 0.3.0 for its include
mode: planet `44 s → 7 s`, a 6.2x win. The per-element work in
`getparents` is lighter than `getid`'s (ID-set lookup only, no
re-encode except for matched-parent output), so the proportional
win could be as large or larger.

Requires indexdata. Non-indexed input falls back to the existing
pipelined-decode path (or is rejected behind `--force`, matching
getid's shape).

Estimated **4-8x at planet**, gated on the overnight baseline
landing tomorrow.

1-2 days scope: adapt `getid`'s
[`src/commands/getid/mod.rs`](../src/commands/getid/mod.rs) walker
+ range-overlap logic, wire the match set into the existing
element-scan callback, keep the non-indexed fallback.

### 2. `parallel_classify_phase` instead of pipelined decode

`getparents`'s per-block work is a pure function of one blob (ID-set
lookup + optional output emit, no cross-blob state). The decision
tree in
[`pipelined-reader-paths.md:154-170`](../reference/pipelined-reader-paths.md#L154)
says: "If per-blob work is a pure function of one blob, prefer
`parallel_classify_phase` - lower memory ceiling, no
oversubscription."

The c912e4d rule blocks sequential-decode conversions, not
pread-worker conversions. `parallel_classify_phase` decompresses in
the worker thread via pread, which keeps decompression concurrent
the way pipelined decode does - but without the oversubscribed
double rayon pool.

Estimated 10-20 % wall win on the sizes where thread oversubscription
matters (planet where decode is a large fraction of the wall). Might
be neutral if decompression throughput is already saturated.

Bench-gated; not safe to land without measurement. The c912e4d
Denmark regression came from stripping parallelism entirely; this
opportunity keeps the decode parallel but reorganises where the
parallelism lives. Different risk profile, same family of concern.

Days scope (bench + implement + re-bench). Only worth pursuing if
opportunity #1 doesn't subsume the win by skipping most blobs
outright.

### 3. Verify the "~85 % of blobs" blob-filter claim

`getparents` passes `BlobFilter::new(need_nodes, true, true)` where
`need_nodes` depends on whether the query contains node IDs (see
`src/commands/getparents/mod.rs`). The CHANGELOG's "~85 % of blobs
at planet scale" claim assumes `need_nodes = false` (the typical
"find ways that reference these nodes" query) and relies on planet
having ~85 % node blobs.

Verification work, not code change:

- Confirm the filter correctly identifies when node blobs can be
  skipped (e.g. a query that asks for parent relations of a node
  still needs way blobs; does the filter handle that?).
- Measure actual skip rate on tomorrow's planet run via the sidecar
  counters (or add a counter if none exists).

Hours scope. Cheap, diagnostic, unblocks any future work that
assumes the number.

### 4. Result-set pre-sizing (micro)

`refs_buf` / `members_buf` grow dynamically inside the hot loop.
Pre-sizing based on typical refs-per-way or members-per-relation
counts would avoid a few `Vec` reallocs.

<1 % wall; decompression dominates per the c912e4d evidence. Skip
unless allocator profile in tomorrow's `--alloc` run shows churn.

## Things that deliberately do not change

- **No pipelined-to-sequential conversion.** The c912e4d regression
  is the gate.
- **`IdSet` stays as the lookup primitive.** Already the modern
  chunked sparse bitset.

## Prerequisites before shipping anything

1. **Planet baseline** scheduled for `overnight.sh:276-278` tonight.
   Baseline lands 2026-04-24.
2. **`--alloc` allocator breakdown** (also in `overnight.sh`) to
   confirm whether #4 is worth touching.

Both prerequisites satisfy in a single overnight run. Tomorrow's
morning sidecar + alloc data is enough to size #1 and #2 properly
and to discard #4 if the allocator profile is clean.

## Cross-references

- [`reference/pipeline.md`](../reference/pipeline.md) - getid /
  getparents entry under Command Pipelines.
- [`reference/pipelined-reader-paths.md`](../reference/pipelined-reader-paths.md) -
  line 99 getparents caller entry; line 154 decision tree; line 42
  "do not convert to sequential" rule with the c912e4d evidence.
- [`reference/performance.md`](../reference/performance.md) -
  Denmark (line 776) and Japan (line 812) baselines.
- [`src/commands/getparents/mod.rs`](../src/commands/getparents/mod.rs) -
  entry point.
- [`src/commands/getid/mod.rs`](../src/commands/getid/mod.rs) - the
  include-mode HeaderWalker pattern to transplant for opportunity #1.
- [`src/read/header_walker.rs`](../src/read/header_walker.rs) - the
  HeaderWalker primitive.
- [`src/scan/classify.rs`](../src/scan/classify.rs) -
  `parallel_classify_phase` / `_accumulate` for opportunity #2.
- [`src/idset.rs`](../src/idset.rs) - `IdSet::any_in_range` +
  `IdSet::get` primitives.
