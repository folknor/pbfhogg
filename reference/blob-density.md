# Blob density - scaling signal for PBF workloads

PBFs from different producers pack very different numbers of elements per
blob. On commands that do per-blob fixed work (`HeaderWalker` preads,
decompress setup, block parse prologue, schedule construction), **blob
count - not byte size - is the scaling signal**.

Drafted 2026-04-24 after `getparents` `HeaderWalker` conversion revealed
a sharp europe vs planet asymmetry. This doc captures the insight and the
measurement plan; concrete numbers fill in once the `repack` command
(`notes/repack.md`) can produce same-corpus-different-encoding pairs.

## The asymmetry

Two representative datasets, both "indexed OSM PBFs", measured via
`brokkr inspect`:

| scale  | source                         | bytes  | blobs   | avg blob   | elements | elements/blob |
|--------|--------------------------------|--------|---------|------------|----------|---------------|
| europe | Geofabrik (osmium defaults)    | 35 GB  | 522 168 | ~67 KB     | 4.18 B   | ~8 000        |
| planet | `planet.openstreetmap.org`     | 92 GB  | 50 816  | ~1.8 MB    | 11.6 B   | ~228 000      |

Per-kind breakdown on the same data:

| kind       | europe blobs | europe elems/blob | planet blobs | planet elems/blob |
|------------|--------------|-------------------|--------------|-------------------|
| DenseNodes | 464 447      | ~8 000            | 32 835       | ~318 000          |
| Ways       | 56 692       | ~8 000            | 17 529       | ~66 500           |
| Relations  | 1 029        | ~8 000            | 452          | ~31 250           |

Europe uses the default PBF encoder cap of 8 000 elements/block (the
osmium/osmosis interop default). The official planet dump uses a custom
encoder that packs ~40x more elements per block, amortising per-blob
fixed costs over ~40x more payload.

## Why it matters

The ratio flips per-byte performance expectations for every command
that does per-blob work outside the main decode loop.

### Commands with per-blob fixed cost

Every `HeaderWalker`-based path pays a QD=1 pread per blob for its
header scan:

- `sort` pass 1 (`src/commands/sort/mod.rs::build_blob_index`)
- `getid` include mode (`src/commands/getid/mod.rs::filter_by_id`)
- `getparents` (`src/commands/getparents/mod.rs`) - new
- `inspect` index-only (`src/commands/inspect/scan.rs::try_index_only_scan`)
- `apply-changes` scanner (`src/commands/apply_changes/scanner.rs`)
- `check --refs` / `check --ids` via `build_classify_schedules_split`
- `extract --smart` / `--complete` via `pread_execute`
  (`src/commands/extract/common.rs`)
- `tags-filter` via its own schedule scan
- `build-geocode-index`
- `renumber_external`

On a 522 k-blob europe PBF, that's 522 k × ~50-70 µs of QD=1 NVMe
latency per scan, even before any payload work. On a 50 k-blob planet
PBF, the same header scan is ~40x cheaper.

### Measured consequences

- `sort` pass 1, commit `1f97fae`: europe +21 % wall regression,
  planet -9 % wall win. The "planet wins" framing silently assumes
  50 k blobs.
- `getparents` `HeaderWalker` path, commit `783970a`: planet -46 % wall
  (44.8 s → 24.4 s), europe +68 % wall (26.4 s → 44.2 s). Same encoder
  asymmetry, bigger magnitude because getparents has no pass-2
  cache-warmth offset.

### The pattern we kept seeing

Blob density retroactively explains a repeating observation from
prior optimization work: "change X regressed europe wall but won on
planet, probably an I/O or memory effect". Every one of those prior
cases involved a `HeaderWalker`-style per-blob code path. The win on
planet and the regression on europe were the *same* effect viewed
from opposite sides of the ~40x blob-count ratio - not two
independent phenomena being reconciled, but one phenomenon with a
two-scale blind spot in our measurement setup.

Recognising this up-front changes how we size future changes: a
"planet-only" win claim should be read as "win on low-blob-density
input, unknown on high-blob-density input" until both are measured.

### Rule

**Per-byte performance claims on planet do not generalise to Geofabrik
extracts at equivalent byte size.** Any "planet takes X seconds"
benchmark on `planet.openstreetmap.org`-sourced data must be read as a
"50 k-blob planet takes X seconds" claim. A hypothetical 500 k-blob
planet (produced by running a Geofabrik-style extract of the full
planet) would behave very differently for header-walk-dominated
commands.

## Consequences for the codebase

### Upstream reference implementation

Osmium (the reference C++ OSM/PBF library) takes no blob-density-aware
action either. The writer hardcodes `max_entities_per_block = 8000` with
no configuration knob; the reader submits each blob to a thread pool
without branching on blob count, size, or density. The 40x asymmetry is
a blind spot in the reference implementation too, which means
threshold-based dispatch here is new ground.

### Silently-wrong documentation

- `README.md` "Planet scale" table: every entry is measured on the
  `planet.osm.org`-packed blob layout. The table needs a one-liner
  caveat once measurements on the other layout land.
- `reference/performance.md` planet sections: same.
- `notes/*.md` "N seconds at planet scale" predictions: likewise.

### Decisions that need revisiting

- **File-size thresholds**: several commands branch on `file_size`.
  Blob count is the right signal for header-walk-bound work.
- **`parallel_classify_phase` thread count**: fixed at
  `available_parallelism() - 2`. Per-blob coordination cost
  vs per-blob payload work balances differently across the two
  encodings.
- **`BATCH_SIZE` in pipelined batches**: likely needs to scale with
  blob size.

### Audit targets for threshold-based dispatch

Commands that are planet-favourable on large-blob PBFs but may
regress on Geofabrik-style packing:

- `sort` pass 1 (landed as HeaderWalker on planet wins)
- `getparents` (HeaderWalker path, landing)
- Any other `HeaderWalker`-based path from the list above

Each is a candidate for `if blob_count > N { pipelined_decode }
else { header_walk }` dispatch, gated on measurements.

## Measurement plan

Needed to turn this doc from "insight" to "insight + evidence":

1. **Produce a 8k-packed planet** via `pbfhogg repack --elements-per-blob 8000`
   (see `notes/repack.md`). This is the same-corpus-different-encoding
   control that doesn't exist today.
2. **Register it in `brokkr.toml`** as a new dataset variant alongside
   the existing `planet/indexed` (the osm.org-packed one).
3. **Run the matrix** for each header-walk command:
   - `planet/indexed` (50 k blobs, current baseline)
   - `planet/packed-8k` (~6-7 M blobs, Geofabrik-style packing)
   - Record wall, peak RSS, disk read, phase split.
4. **Fill in the "Decisions that need revisiting" section** with
   concrete data: which commands hold up, which need threshold
   dispatch, which need structural rework.

Measurements will also confirm or refute the prediction that
`HeaderWalker` scan scales linearly with blob count (versus, say,
sublinearly thanks to page cache effects).

## Cross-references

- [`notes/repack.md`](../notes/repack.md) - command that produces the
  alternate-packing planet for measurements.
- [`notes/degrade.md`](../notes/degrade.md) - command for producing
  adversarial test PBFs; the `--unsort` mode exercises `sort`'s
  overlap-run path which is orthogonal to but motivated alongside
  this work.
- [`notes/sort.md`](../notes/sort.md) - the `HeaderWalker` pass-1
  trade-off that first surfaced the asymmetry.
- [`notes/getparents.md`](../notes/getparents.md) - second instance
  of the same asymmetry, larger magnitude.
- [`reference/pipelined-reader-paths.md`](pipelined-reader-paths.md) -
  existing callers of `into_blocks_pipelined`, candidates for
  threshold dispatch.
- [`src/read/header_walker.rs`](../src/read/header_walker.rs) - the
  primitive whose per-blob cost is the source of the asymmetry.
