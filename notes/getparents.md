# `getparents` - optimization plan

> **STATUS 2026-07-13: headline resolved; residuals moved to TODO.md.**
> The europe-vs-planet dispatch question was ratified as
> [ADR-0006](../decisions/0006-blob-count-threshold-dispatch.md)
> (150k-blob threshold). Of the two residual opportunities below, #3's
> instrumentation has since landed (`getparents_schedule_blobs` /
> `getparents_blobs_skipped` counters in
> `src/commands/getparents/mod.rs`) - what remains of #3 and #4 is
> tracked in TODO.md's getparents entry. This note stays as the
> measurement record behind the ADR (and is cited from
> `reference/blob-density.md`); update TODO.md, not this file.

Target: `pbfhogg getparents` - whole-file scan listing the ways and
relations that reference a given ID set. Input is a sorted PBF and
a set of IDs; output is the IDs of parent elements (and optionally
the parents' own elements via `--add-self`).

Drafted 2026-04-23 from a fresh read of
[`src/commands/getparents/`](../src/commands/getparents/) against
the modern pipeline primitives documented in
[`reference/pipeline.md`](../reference/pipeline.md) and
[`reference/pipelined-reader-paths.md`](../reference/pipelined-reader-paths.md).

## Current state (2026-04-24)

Europe bench `aa5dcf26` (commit `b891514`, yesterday): 26.4 s wall.
Hotpath `dc0e0998`: 19.3 s with 25 % in decompression, 7 % in block
parse, remaining 68 % in `run_pipeline` machinery (I/O wait + thread
scheduling - the pipelined reader pulls all blob bytes even when
`BlobFilter` then skips their decompression).

Planet baseline on current HEAD `68e1ba0`: **44.8 s** (`70df6046`).
Stale 2026-04-16 planet bench on `7e9c2e9` was 51.9 s. Both run a
3-ID query (`n115722 n115723 w2080`).

Disk read on the 44.8 s planet baseline: 74.8 GB of a 92 GB file -
we pay for node-blob body reads that `BlobFilter` then discards
at decompress time.

## Opportunity #1 landed as experiment (2026-04-24)

Rewrote the primary path to `HeaderWalker` + `parallel_classify_phase`.
Schedule covers only the blob kinds whose bodies can contribute
matches (ways for node-ref queries, relations for any non-empty
query, nodes only under `--add-self`). Workers pread + decompress +
scan; a `ReorderBuffer` delivers owned blocks to the writer in file
order.

Measured results vs baselines above:

| scale | baseline | new      | wall      | disk read            | peak RSS          |
|-------|----------|----------|-----------|----------------------|-------------------|
| planet| 44.8 s   | **24.4 s** | **-46 %** | 74.8 GB → **30 GB**  | 4.3 GB → **532 MB** |
| europe| 26.4 s   | **44.2 s** | **+68 %** | 26.1 GB → 16.5 GB    | 1.20 GB → 108 MB    |

Planet win anatomy (run `dirty` sidecar):

- Schedule scan (`HeaderWalker` header preads): 6.68 s, 669 MB disk
  read (headers only).
- Decode phase (`parallel_classify_phase` preads + decompress):
  17.69 s, 29 GB disk read, 19.7 avg cores.
- 17 981 blobs in schedule, 32 835 node blobs skipped (65 %).

Europe regression anatomy (same layout):

- Schedule scan: **38.57 s**, 522 197 vol_cs, single core. This is
  the cold-cache QD=1 pread cost: europe has 522 168 blobs (~67 KB
  avg) vs planet's 50 816 (~1.8 MB avg). Same pattern as `sort`
  pass 1 on europe.
- Decode: 6.84 s, 12.4 GB disk read, 19 avg cores. Fast.

**This regression is rooted in the europe encoder packing 40x more
blobs per byte than the planet encoder.** See
[`reference/blob-density.md`](../reference/blob-density.md) for the
cross-cutting insight and affected-command audit.

### Decision resolved

The threshold-dispatch decision was ratified on 2026-07-10 and is
implemented by [`ADR-0006`](../decisions/0006-blob-count-threshold-dispatch.md).
`getparents` estimates OSMData blob count from a bounded header probe,
uses `HeaderWalker` below 150,000 blobs, and uses the pipelined reader
at or above that threshold. The measurements below are retained as the
record that led to the ADR, not as open work.

### Crossover measured (2026-07-10, plantasjen) - data supports threshold-dispatch

The 8k-packed planet exists (`snapshot.8k`, 1,453,433 blobs, 98.4 GB).
Third matrix cell, same corpus, same host:

| encoding | blobs | full scan (pre-`783970a`) | HeaderWalker | winner |
|---|---:|---:|---:|---|
| planet primary | 50,816 | 44.8 s | **23.5 s** (`11bc44dc`) | HW, -46 % |
| europe Geofabrik | 522,168 | **26.4 s** | 44.2 s | scan, HW +68 % |
| planet 8k | 1,453,433 | **52.8 s** (`2b3e496e`, `--commit 68e1ba0`) | 82.7 s (`425d1f1e`) | scan, HW +57 % |

8k HeaderWalker phase split: schedule walk 64.8 s (single-threaded,
QD=1, ~45 us/blob), decode 17.8 s (19 cores, encoding-invariant vs
primary's ~18 s). The walk term is linear in blob count; the decode
term is byte-bound. Dispatch rule: HeaderWalker wins iff
`blob_count x ~45 us < bytes_skipped / scan_rate`; the getparents
crossover sits between 51 k and 522 k blobs. Blob count is known
before committing to either path (indexdata scan / file size over
average blob size estimate), so a single `if` at entry suffices.
io_uring-batched header probes remain the lever that would flatten
the walk term entirely if threshold-dispatch ever feels unsatisfying -
that primitive, and every other call site that shares it, is
consolidated in [`notes/header-walk-batching.md`](header-walk-batching.md).

The measured bracket was ratified as a 150,000-blob threshold in
[`ADR-0006`](../decisions/0006-blob-count-threshold-dispatch.md).

**Experiment commit: `783970a`** (the HeaderWalker rewrite whose
high-blob-count regression prompted the dispatch). Superseded by
ADR-0006: both scan paths are now kept behind the threshold, so the
once-contemplated `git revert 783970a` escape hatch no longer applies.

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

### 1. `HeaderWalker` blob-level fast path [EXPERIMENT LANDED 2026-04-24]

See the "Current state" section above for the implemented and
measured version. Summary of the deltas from this original plan:

- **Blob-range pre-screening (`IdSet::any_in_range`) does not apply.**
  The notes' mental model assumed getid's pattern would transplant
  directly - it doesn't. Blob indexdata stores `(min_id, max_id)` of
  the elements in the blob (way IDs for way blobs), not the ref/member
  IDs the query actually cares about. The typical getparents query
  ("find ways that reference query nodes") can't pre-screen way blobs
  by way-ID range - every way's refs could be any node ID, and
  indexdata doesn't capture a refs-range.
- **The real win is IO reduction, not blob skipping.** The
  implemented path skips only the blob kinds structurally incapable of
  producing matches (node blobs unless `--add-self`, way blobs unless
  node IDs present, etc). That alone cuts planet disk read from 74.8 GB
  to 30 GB.
- **Headline win is ~1.8x at planet, not 4-8x.** The original estimate
  assumed getid-style range pre-screening which turned out not to
  apply.
- **Europe regresses 1.7x** due to the 40x blob-count asymmetry
  between europe (522 k blobs) and planet (50 k blobs). Blob-density
  discovery lives in
  [`reference/blob-density.md`](../reference/blob-density.md).

Original estimate (4-8x) was based on applying getid's pattern
directly. Actual win comes from IO byte reduction, not blob skipping.

Requires indexdata. Non-indexed input falls back to the existing
pipelined-decode path (or is rejected behind `--force`, matching
getid's shape).

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

### 4. Result-set pre-sizing (micro) - CLOSED 2026-07-14 as SKIP

`refs_buf` / `members_buf` grow dynamically inside the hot loop.
Pre-sizing based on typical refs-per-way or members-per-relation
counts would avoid a few `Vec` reallocs.

<1 % wall; decompression dominates per the c912e4d evidence. Skip
unless allocator profile shows churn.

**The gating profile ran** (planet `--alloc`, UUID `f83c8cf1` at
`dcc445e`, 2026-07-14 - the first allocation profile of the
post-`783970a` HeaderWalker path). **No churn**: `getparents::getparents`,
the frame owning both buffers, is 2.2 MB exclusive = **0.07 %** of
allocations. `decompress_blob_raw` is 2.0 GB / 59.9 %. The prediction
("decompression dominates, likely outcome skip") was exactly right.
Closed in TODO.md; do not re-open.

## Things that deliberately do not change

- **No pipelined-to-sequential conversion.** The c912e4d regression
  is the gate.
- **`IdSet` stays as the lookup primitive.** Already the modern
  chunked sparse bitset.

## Prerequisites - both satisfied

1. ~~**Planet baseline**~~ landed 2026-04-24; superseded several times
   since. Current planet number lives in `reference/performance.md`, not
   here: **22.2 s** `--bench 3` (`ca49bcdf`, `dcc445e`, 2026-07-14),
   carrying an open **+16.8 % regression flag** vs 19.0 s (`a7c064eb`,
   `2306fd9`). Dispatch is not the cause (skip rate unchanged at 64.6 %);
   the suspect is the decode path between those commits. Tracked in
   TODO.md + the performance.md flag - not here.
2. ~~**`--alloc` allocator breakdown**~~ ran 2026-07-14 (`f83c8cf1`);
   the profile was clean and #4 is closed as SKIP - see #4 above.

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
