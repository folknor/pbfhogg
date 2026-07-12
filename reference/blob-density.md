# Blob density - scaling signal for PBF workloads

Status: the getparents and getid include-mode decision is resolved by
[`ADR-0006`](../decisions/0006-blob-count-threshold-dispatch.md): a bounded
blob-count estimate dispatches those commands between the walker and a
full-file scan (pipelined reader for getparents, sequential streaming for
getid). Other consumers are priced by shape (see "Two command shapes"
below): tags-filter is confirmed pure parallel-classify and *gains* on
dense encodings; check-refs and the rest of the
`build_classify_schedules_split` family carry a single-threaded schedule
walk that regresses on high blob count (check-refs +185 % at planet-8k),
but with near-zero production bite since the production planet dump is
50 k blobs. Landing gate measurements, including the cross-day I/O
environment shift that made this file's absolute 8k walls unusable as
bounds (~45 % swing in steady sequential read rate between sessions,
same code, same file), are in `performance.md` "Blob-count threshold
dispatch landing gates".

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

### Two command shapes, not one

The original framing of this note lumped every `HeaderWalker`-touching
command together as "per-blob fixed cost". Measurement (2026-07-10 ->
2026-07-12) split that list into two shapes that move in **opposite**
directions with blob density. Getting the shape right is the whole
decision: only the first shape needs blob-count dispatch.

**Shape 1 - selective header-walk (latency-bound, REGRESSES on
density).** These walk blob headers with a QD=1 pread per blob and
decode almost nothing; the walk is pure per-blob NVMe latency
(~45 µs/blob measured), so wall grows linearly in blob count. On a
522 k-blob europe PBF the walk alone is 522 k × ~45-70 µs before any
payload work; on a 50 k-blob planet PBF it is ~40x cheaper. These are
the commands that win on planet primary and lose on Geofabrik / 8k
encodings:

- `getid` include mode (`src/commands/getid/mod.rs`) - **dispatched**
- `getparents` (`src/commands/getparents/mod.rs`) - **dispatched**
- `removeid` raw-frame passthrough - walker-only (unmeasured full-scan;
  see ADR-0006)
- `sort` pass 1 (`src/commands/sort/mod.rs::build_blob_index`) - **not
  dispatched**, separate seek-skip mechanism (see below)
- `inspect` index-only (`src/commands/inspect/scan.rs::try_index_only_scan`)

**Shape 2 - pure parallel classify (decompression-bound, IMPROVES or
stays flat on density).** Build a per-kind schedule cheaply, then
decompress matching bodies across a rayon pool. Throughput-bound, not
per-blob-latency-bound, and the indexdata prescreen gets *more*
selective as blobs shrink (narrow per-blob ID ranges skip more
decompression), so several get *faster* on dense encodings. No dispatch
needed:

- `tags-filter` (its own schedule scan) - **measured faster on 8k**
  (see below)
- `apply-changes` scanner (`src/commands/apply_changes/scanner.rs`)
- `build-geocode-index`
- `renumber_external`

**Shape 3 - hybrid (serial schedule walk THEN parallel classify).**
The `build_classify_schedules_split` family runs a **single-threaded
`HeaderWalker` loop** (one QD=1 pread per blob, shape-1 latency) to
build the per-kind schedule, then hands it to the parallel classify
phase (shape-2 throughput). The serial walk is invisible at 50 k blobs
(~3.5 s) but dominant at 1.45 M (~102 s of a 153 s wall - see the
check-refs cell below), so these **regress on high blob count** via the
walk term even though their payload phase is shape-2. Callers of
`build_classify_schedules_split` (`src/scan/classify.rs`):

- `check --refs` (`src/commands/check/refs.rs`)
- `check --ids` (`src/commands/check/verify_ids.rs`)
- `cat --clean` (`src/commands/cat/mod.rs`)
- `repack` (`src/commands/repack/mod.rs`)
- `degrade` (`src/commands/degrade/mod.rs`)
- `extract --smart` shares the pattern
  (`src/commands/extract/common.rs::pread_execute`)

The serial walk cannot be trivially parallelized: PBF has no top-level
blob index, so blob N+1's offset is only known after reading blob N's
header. Flattening it needs io_uring-batched header probes (the
non-pursued getparents lever) or a dispatch to a buffered sequential
walk on high blob count - both unmeasured, and low production priority
because the production planet dump is 50 k blobs where the walk is
negligible. tags-filter escapes this because its own scan is not the
serial `build_classify_schedules_split` walk.

The rest of this note's asymmetry evidence is about shape 1 and the
shape-3 walk term. Shapes 2/3 are covered by the tags-filter and
check-refs cells below.

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
- **`BATCH_SIZE` (extract unsorted pass 2)**: since the fused command
  transforms landed (ADR-0009, 2026-07-12) the command batch helpers are
  gone; this constant survives only in `extract/simple.rs` and no longer
  drives any pipelined command path.

### Audit targets for threshold-based dispatch

Only shape-1 (selective header-walk) commands are candidates; shape-2
parallel-classify commands do not regress on density and are off this
list. Current status:

- `getid` include mode - **RESOLVED** (ADR-0006, 150 k-blob dispatch)
- `getparents` - **RESOLVED** (ADR-0006, 150 k-blob dispatch)
- `sort` pass 1 - **not converted.** Excluded from ADR-0006: pass 1
  uses a third mechanism (seek-skip index build), not the walker vs
  full-scan dichotomy the ADR prices. Its europe +21 % regression is
  unaddressed on paper, but production input (Geofabrik / planet) is
  already sorted, so pass 1 stays on the header-only fast path and the
  regression has near-zero real-world bite. Revisit only if an
  unsorted large-blob workload becomes real.
- `inspect` index-only - separately priced, unmeasured on 8k. Header-
  only by design; low stakes.

The dispatch rule for the resolved commands: `if blob_count >
150_000 { full_scan } else { header_walk }`, from a bounded head-of-
file estimate (`estimate_blob_count`, `src/read/header_walker.rs`).

## Measured evidence (2026-07-10, plantasjen)

The same-corpus control exists: `snapshot.8k` on the planet dataset
(`brokkr repack --dataset planet --elements-per-blob 8000 --as-snapshot
8k`, UUID `8027765b` at `8c1cf03`). 98.4 GB, **1,453,433 blobs** (nodes
1,305,968 / ways 145,699 / relations 1,766) - 28.6x the primary's
50,816. Denmark-scale controls in the other direction:
`snapshot.1k` / `snapshot.64k` / `snapshot.320k`.

### getparents three-cell matrix (the dispatch decision data)

| Encoding | Blobs | Full scan | HeaderWalker | Winner |
|---|---:|---:|---:|---|
| planet primary | 50,816 | 44.8 s | **23.5 s** (`11bc44dc`) | HW, -46 % |
| europe Geofabrik | 522,168 | **26.4 s** | 44.2 s | scan, HW +68 % |
| planet 8k | 1,453,433 | **52.8 s** (`2b3e496e` at `68e1ba0` via `--commit`) | 82.7 s (`425d1f1e`) | scan, HW +57 % |

Phase split of the 8k HeaderWalker run: schedule walk **64.8 s**
(single-threaded, 0.1 avg cores, 1.45 M voluntary context switches),
decode **17.8 s** (19 cores). The decode phase is byte-bound and
encoding-invariant (~18 s on both planet encodings); the walk is pure
per-blob QD=1 latency at **~45 µs/blob**, confirming the linear-scaling
prediction. Consistency check: europe 522 k × 45 µs ≈ 23 s walk +
~20 s decode ≈ the measured 44.2 s.

The dispatch rule this supports: HeaderWalker wins iff
`blob_count × ~45 µs < bytes_skipped / scan_rate`. Crossover for
getparents-shaped workloads sits between 51 k and 522 k blobs.
io_uring-batched header probes would flatten the walk term entirely
(known non-pursued lever in `notes/getparents.md`).

### getid include mode confirms, steeper (2026-07-10)

Full three-cell matrix (scan arm via `--commit 51c662e`, the parent of
the `bb16193` HeaderWalker landing):

| encoding | blobs | scan | HeaderWalker | winner |
|---|---:|---:|---:|---|
| planet primary | 50,816 | 43.7 s | **6.1 s** | HW, -86 % |
| europe | 522,168 | **17.9 s** (`bc96d15d`) | 40.2 s (`57ffbf49`) | scan, HW +125 % |
| planet 8k | 1,453,433 | **33.2 s** (`c0d89d8f`) | 102.6 s (`aa5bc158`) | scan, HW +209 % |

Two findings beyond the getparents confirmation. First, getid's arms
move in OPPOSITE directions with blob density: the scan arm gets
FASTER on dense encodings (33.2 s on the 98 GB 8k file vs 43.7 s on
the 92 GB primary) because narrow per-blob ID ranges make the
indexdata prescreen skip decompression almost everywhere, leaving
near-pure sequential read; the walk arm degrades linearly as always.
Second, the divergence is therefore much steeper than getparents'
(+209 % vs +57 % at 8k), which argues for placing the shared dispatch
constant toward the LOW end of the 51 k-522 k bracket (~150 k blobs):
getid pays more for a wrong high-side call than getparents pays for a
wrong low-side one. getid and getparents are the two current dispatch
consumers; sort pass 1 remains a separately-priced follow-on. Cross-epoch
caveat: scan cells ran the April tree via
`--commit`; margins of 2.2-3x dwarf any plausible tree drift.

### tags-filter and check-refs: shape 2 vs shape 3 (2026-07-12)

Both measured at HEAD (`a65cecc` / `5dc07c4`, plantasjen). Same axis,
opposite verdicts - the reason shape matters.

| command | encoding | blobs | wall | vs primary |
|---|---|---:|---:|---:|
| tags-filter `-R` | planet primary | 50,816 | 49.5 s | - |
| tags-filter `-R` | planet 8k | 1,453,433 | **42.7 s** | **-14 %** |
| check --refs | planet primary | 50,816 | 53.8 s (`7d9f5dfd`) | - |
| check --refs | planet 8k | 1,453,433 | **153.5 s** (`1851f73a`) | **+185 %** |

**tags-filter is pure shape 2** - it gets *faster* on the dense
encoding, same mechanism as getid's scan arm (indexdata prescreen
skips more decompression at narrow per-blob ID ranges). No dispatch
needed; this is the clean positive the note originally predicted for
"parallel classify" commands.

**check-refs is shape 3, and refuted that prediction.** Phase split of
the 8k run (`1851f73a`): `SCHEDULE_SCAN_LOOP` **101.9 s**
(single-threaded, 0.1 avg cores, 1.47 M voluntary context switches -
one per blob, ~70 µs/blob), then `CHECKREFS_NODES` 33.4 s and
`CHECKREFS_WAYS` 18.1 s (both ~21 cores, parallel). The serial schedule
walk - the shape-1 preamble inside `build_classify_schedules_split` -
is 66 % of the wall at 8k. On planet primary the same walk is ~50 k ×
70 µs ≈ 3.5 s, invisible against the 53.8 s wall. The parallel phases
behave like shape 2 (flat-to-faster); the regression is entirely the
serial walk term. This is the same asymmetry as getid/getparents,
found a third time, now in the shared scanner rather than a
command-private walk. **Instrument-first vindicated again:** the
structural "check-refs is parallel classify, expect it fine on 8k"
inference was wrong by 2.85x because the shared scanner hides a
serial shape-1 loop.

Disposition: real regression, low production bite (production planet is
50 k blobs). Not chased. If a high-blob-count workload becomes real,
the fix axis is the serial `build_classify_schedules_split` walk (shared
by six commands), not the parallel classify phases.

### Full shape-3 sweep (2026-07-12, all six callers, plantasjen `5dc07c4`)

Benched every `build_classify_schedules_split` caller on `--snapshot
8k`. The serial walk (`SCHEDULE_SCAN_LOOP` / `SMART_PASS1_SCHEDULE_SCAN`)
is a rock-steady **~97-109 s** everywhere - it is the same 1.45 M-blob
loop regardless of command, and the spread is pure I/O noise. What
differs is the *fraction*, which cleanly splits the family:

| command | 8k wall | serial walk | walk % | README (50 k) | ratio |
|---|---:|---:|---:|---:|---:|
| `check --refs` | 153.5 s | 101.9 s | **66 %** | 54 s | 2.85x |
| `check --ids` | 149 s | 100.5 s | **67 %** | 57 s | 2.6x |
| `extract --smart` | 373 s | 108.6 s | 29 % | 268 s | 1.39x |
| `cat --clean` | 526 s | 100.0 s | 19 % | 334 s | 1.57x |
| `repack` | 548 s | 99.9 s | 18 % | ~383 s | ~1.4x |
| `degrade --strip-loc` | 544 s | 96.5 s | 18 % | 383 s | 1.42x |

UUIDs: `1851f73a` / check-ids dirty / `4b82686f` / `3f4c222c` /
`8f275ebf` / `6f8a3e94`.

**The lever's payoff is narrower than "six commands" implied.** The two
**read-only** callers (`check --refs`, `check --ids`) are ~2/3 walk, so
flattening it roughly halves their 8k wall - and they are the same two
whose selective-scan cousins (getid, getparents) already got dispatch.
The four **re-encoding** callers spend only 18-29 % in the walk; their
8k regression is dominated by the inherent cost of framing + writing
1.45 M tiny blobs instead of 50 k fat ones, which the walk fix does not
touch. So a perfect io_uring walker would take check-refs/check-ids from
~2.7x down to ~1.3x, but leave cat/repack/degrade/extract-smart roughly
where they are. If the walker primitive is ever built, prioritise it for
the read-only pair; do not expect it to rescue the write-heavy four.

Side finding (`check --ids` run): the 8k snapshot has 7 non-monotonic
relation violations (repack relation-blob ordering; nodes/ways clean).
Timing-neutral - the walk is ID-order-independent - so the sweep numbers
stand. Tracked under the repack entry in TODO.md.

### Correctness across the encoding axis

`brokkr verify all --snapshot 1k|64k|320k` (denmark, 2026-07-10): every
element-shaped command (sort, cat, extract, tags-filter, getid, altw
all three backends, check-refs, renumber) passes identically on all
three re-encodes and on primary. The four suite failures reproduce
bit-identically on primary - harness-side or pre-existing, none
encoding-sensitive.

### New encoding-sensitive finding: `read` parallel variant OOM

`brokkr read --dataset planet --snapshot 8k --bench 1`: 3 of 4 variants
completed; the **parallel variant was killed by signal** on the 8k
encoding. It survives primary planet (50.8 k blobs), so per-blob memory
accumulation at 28.6x blob count is implicated. Uninvestigated; tracked
in TODO.md.

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
