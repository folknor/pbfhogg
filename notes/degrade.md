# `degrade` - command design

New subcommand: produce a valid-but-adversarial PBF by stripping
properties or perturbing structure. A "make our lives difficult" tool
for exercising code paths that require less-optimised inputs
(unsorted, missing indexdata, scattered coords, etc).

Drafted 2026-04-24 as scaffolding before implementation. Will drift.

## Purpose

Production PBFs (Geofabrik, `planet.osm.org`) are well-formed, sorted,
indexed, and include all the hints pbfhogg is built to exploit. That
makes it hard to benchmark the non-optimal code paths that handle
real-world-ish edge cases:

- `sort` overlap-rewrite needs an unsorted input.
- `add-locations-to-ways` exercises nothing interesting on inputs that
  already have `LocationsOnWays` set.
- Commands with `--force` fallbacks (getid, sort, etc) need PBFs
  without indexdata to trigger those paths.
- `tags-filter` tag-index fast path needs to be measured against the
  no-tag-index fallback.

`degrade` is the knife-drawer of "take a clean PBF and make it
harder" transformations, unified under one command so the flags
compose.

Non-goals:

- No element filtering (`getid`, `tags-filter`, `extract`).
- No blob-size re-encoding (that's `repack`).

## API

```
pbfhogg degrade <input> -o <output> [--unsort]
                                     [--strip-locations]
                                     [--strip-indexdata]
                                     [--strip-tagdata]
                                     [--strip-bbox]
                                     [--recompress C]
                                     [--drop-ids N:SEED]
                                     [--compression C]
                                     [--direct-io]
                                     [--io-uring]
```

Flags are composable. Order of effects is stable (documented below)
so `--unsort --strip-locations` is equivalent to
`--strip-locations --unsort` on output.

## Transformations

Priority-ordered. v1 ships (1)-(3); the rest follow as need arises.

### (1) `--unsort` - produce unsorted input

Target: exercise `sort` pass 2's overlap-rewrite path (opp #3 in
`notes/sort.md` landed without a planet bench because we lacked an
unsorted planet). Output must:

- Have at least one overlapping blob pair per element kind (so
  `detect_overlaps` flags it).
- Remain a valid PBF (elements themselves unchanged, just blob
  grouping/order perturbed).
- Clear the `Sort.Type_then_ID` header feature.

Minimum-viable approach: take the input's sorted element stream,
rotate each kind's element-ID-sorted run by one blob's worth of
elements. This creates exactly one overlap band between adjacent
blobs per kind - enough to trigger the overlap path without
chaos-ifying the file.

Configurable chaos deferred: later `--unsort=[rotate|shuffle|reverse]`
or similar once the simple mode exists.

### (2) `--strip-locations` - remove LocationsOnWays

Target: `add-locations-to-ways` benchmarks on PBFs whose input
already carries inline coordinates (redundant starting point).
Output:

- Removes `LocationsOnWays` from header features.
- For each Way element: drop the inline node coordinates, keep only
  the `refs` list.

Simple structural transformation; no re-sorting needed.

### (3) `--strip-indexdata` - remove BlobHeader indexdata

Target: force commands into their `--force`/non-indexed fallback
paths (`sort`, `getid`, `tags-filter`, etc).

- Zeros or omits the BlobHeader `indexdata` field on every OsmData
  blob.
- Element bodies are bit-identical; only framing metadata changes.

Ideally does not re-compress blobs (if the output writer path can
reframe with the new header, it's nearly free - same idea as the
passthrough path `sort` uses today).

### (4) `--strip-tagdata` - remove tag-index hints (deferred)

Target: `tags-filter`'s no-hint fallback path.

### (5) `--strip-bbox` - clear HeaderBlock bbox (deferred)

Target: spatial-scan fallback in `extract`.

### (6) `--recompress C` - re-encode at a different codec (deferred)

Target: codec-boundary behavior; overlaps with `repack
--compression` but done without changing blob size.

### (7) `--drop-ids N:SEED` - introduce referential dangles (deferred)

Target: `check --refs`'s slow path and error recovery.

- Deterministically drop N elements' IDs from a given seed.
- Output is a PBF whose ways/relations reference missing nodes/ways.
- Useful for stress-testing `check --refs` and validating its error
  reports.

## Composability

When multiple flags are set, transformations apply in this order:

1. `--strip-indexdata` (structural, changes framing only)
2. `--strip-tagdata` (structural, changes framing only)
3. `--strip-bbox` (header-level)
4. `--strip-locations` (element-level, changes ways)
5. `--drop-ids` (element-level, removes elements)
6. `--unsort` (element/blob reordering, always last)
7. `--recompress` (during the final write, irrespective of prior
   flags)

This order ensures element-level transformations see consistent
input and that `--unsort` sees the final element set after drops.

## Implementation sketch

Unlike `repack`, which can be a single pipelined-read →
re-encode pass, `degrade` is transformation-driven. v1 does one pass
per transformation, materialising via BlockBuilder + PbfWriter.

For v1 (unsort + strip-locations + strip-indexdata):

- `--strip-indexdata`: a blob-level passthrough; reframe each blob
  with cleared indexdata. Mirrors `sort`'s passthrough path but
  drops the indexdata field.
- `--strip-locations`: element-level; decode each way, re-emit
  without inline coords, re-encode.
- `--unsort`: element-level; buffer a full rotation window per kind,
  emit rotated.

Flags can interact - `--strip-indexdata --unsort` means we need to
fully decode anyway (no blob-level passthrough possible when element
order changes). Detect combinations upfront and pick the minimal
decode path.

## Correctness criteria

**Output is a valid PBF.** Parseable by osmium and by pbfhogg's own
reader. `pbfhogg inspect` on the output succeeds.

**Only the declared property is changed.** Elements not targeted by
flags are bit-identical semantically. Tags multiset, ID set, and
per-element metadata all preserved.

**Header features reflect reality.** After `--strip-locations`, the
`LocationsOnWays` feature is gone. After `--unsort`, the
`Sort.Type_then_ID` feature is gone. Consumers that check these
features see the truth.

## Tests

Per-flag, minimal:

1. `--unsort` on a small sorted PBF → output has at least one
   overlap run (verifiable via `sort`'s own overlap counter when
   the output is piped back through sort).
2. `--strip-locations` → output header lacks `LocationsOnWays`;
   way elements have no inline coords.
3. `--strip-indexdata` → BlobHeader.indexdata is absent on every
   OsmData blob; `inspect --indexed` returns non-zero.

Round-trip:

4. `degrade --strip-locations` then `add-locations-to-ways` →
   element-by-element recovery of the original.
5. `degrade --strip-indexdata` then `cat` (which re-generates
   indexdata) → inspect --indexed succeeds on the recovered file.
6. `degrade --unsort` then `sort` → sorted output equivalent to the
   original.

Combination:

7. `--unsort --strip-indexdata` → output is unsorted + unindexed.

Benchmarks:

8. `degrade --unsort --dataset planet` produces a file suitable
   for `brokkr sort --dataset planet-unsorted --bench 1`. Register
   the degraded planet in `brokkr.toml` once generated.

## Scope for v1

- `--unsort`
- `--strip-locations`
- `--strip-indexdata`
- `--compression`

Everything else deferred. Flag set grows as consumers show up.

## Cross-references

- [`reference/blob-density.md`](../reference/blob-density.md) -
  the parallel insight about blob density; `degrade` is orthogonal
  but shares the "generate adversarial test data" framing.
- [`notes/repack.md`](repack.md) - companion command; shares input-
  read + output-write plumbing.
- [`notes/sort.md`](sort.md) - primary consumer of `--unsort` for
  opp #3 parallel-overlap benchmarking.
- [`src/commands/altw/`](../src/commands/altw/) - primary consumer
  of `--strip-locations` (once that path has a reason to land).
- [`src/write/block_builder.rs`](../src/write/block_builder.rs) -
  BlockBuilder; re-used for element-level transformations.
- [`src/commands/sort/mod.rs`](../src/commands/sort/mod.rs) -
  structural template for the `--strip-indexdata` passthrough path.
