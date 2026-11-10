# Merge Changes

## Merge-changes

`pbfhogg merge-changes` squashes N OSC (gzip + XML) inputs into one OSC
output. Two production code paths on the same lexical readers:
**streaming** (`write_streaming`, the default) and **simplify**
(`--simplify`, collects every change as rendered XML, dedupes by a
parallel sort and splices the survivors' bytes).

### Run directory only where a merge may spill (bygg, 2026-11-01)

The streaming merge takes an optional scratch, given only for several
inputs at any `-j` but 1; the coordinator's spill site unwraps it once per
spilled input. Matched interleaved `--bench 1` cells on bygg, 7-OSC
`4914..4920`, default mode: baseline `6a9807d9` built in brokkr's
`--commit` worktree (stored), variant `6a9807d9` + change (`--force`,
unstored).

| Cell | Baseline | Variant |
|---|---|---|
| merge-changes 7-OSC (default), three pairs | 3.667, 3.726, 3.721 s (`3ea1430d`, `497ad8ea`, `16d34181`) | 3.693, 3.716, 3.695 s (median -0.7 %, noise) |

The phase split of the third pair is identical (parallel emit 3467 ms
against 3440 ms, drain 249 ms on both). One-input, `--simplify` and `-j 1`
runs lose only the run directory's creation and were not measured: brokkr
passes `--tmp-dir` to every merge-changes, which those command lines now
refuse.

### Simplify and OSC tags-filter on the batch readers (bygg, 2026-10-31)

`--simplify` used to parse each input on one thread into owned elements,
concatenate them, dedupe them serially through a `BTreeMap`, then re-emit
the survivors. It now runs on the streaming path's readers: workers render
each batch to plain XML and record per element its kind, ID, action,
version claim and byte range; a parallel sort of `(kind, id, position)`
replaces the map; survivors' bytes are spliced into the same per-worker
chunks, so the output bytes are unchanged (a differential test against the
owned path holds them, gzip members included). `tags-filter --input-kind
osc` moved onto the same readers, its workers rendering kept elements into
nine (action, kind) groups.

Matched interleaved `--bench 1` cells on bygg. Baseline `44c62853`: the
simplify cells were built in brokkr's `--commit` worktree, the others in
the main tree (stash, `--bench 1`, stored); variant `44c62853` + change
(`--force`, unstored). Spans from `--durations` START/END pairs.

| Cell | Baseline | Variant |
|---|---|---|
| merge-changes `--simplify` 7-OSC `4914..4920` | 20.9 s, 18.3 s; peak anon 22.5 GB (`0df5c0ad`, `2b1d6966`) | 5.44, 5.49, 5.41, 5.40 s; peak anon 12.0 GB (11.8 GB before element ranges widened to 64-bit offsets) |
| merge-changes 7-OSC (default), three pairs | 3.619, 3.657, 3.625 s (`0028f8ee`, `dc3aebc0`, `1849bf51`) | 3.636, 3.704, 3.656 s (median +0.9 %, noise) |
| `tags-filter --input-kind osc` 4913 `w/highway=primary` | 5.31 s, peak anon 244 MB (`68a6c81f`) | 2.46, 2.46 s; peak anon 131 MB |

Variant simplify spans: parse and render 3.18 s (was 6.9 s of owned parse
plus a 0.6-1.8 s concatenation), dedupe 0.23 s (was 4.3 s), emit 1.59 s,
drain 0.29 s. The peak is the rendered XML of all 26.3 M input changes
plus a 24-byte sort key and a 40-byte record per change, then the deflated
chunks during emit.

### Streaming path on lexical batches with spill (bygg, 2026-10-30)

The streaming path no longer makes one whole input the unit of work. One
reader thread per input (capped by the worker count, started in input
order) drives the OSC driver and records each element's raw tags into
batches of `MERGE_CHANGES_BATCH_BYTES`; pool workers replay a batch into
the owned sink, re-emit XML and gzip it as its own member; one
coordinator writes each member as soon as it is next in order for its
input, the head input's to the output and later inputs' to a spill file
in the run scratch, copied into the output when that input becomes the
head. A byte ledger (`MERGE_CHANGES_INFLIGHT_BYTES`) bounds admitted
batches. The single-input run takes the same path and is no longer
serial.

| Cell | Baseline (commit `627883d7`) | This change (`418739bd` + change, `--force`) |
|---|---|---|
| planet 7-OSC `--osc-range 4914..4920` | 30.0 s, peak anon 895 MB (`f24c29af`) | **9.9 s, peak anon 286 MB** (an earlier cell of the same design before review fixes: 10.0 s, 304 MB) |
| planet 1-OSC `--osc-seq 4913` | 24.0 s, peak anon 1.6 MB (`bf6db512`) | **2.74 s, peak anon 53 MB** |

Same change counts as the baselines (26,261,540 and 4,305,714). The
7-OSC output is 965.1 MB against 962 MB (one gzip member per batch).
Spill: about 787 MB written for inputs 2-7 and copied back. The readers
spent 15.6 s summed waiting for credit
(`merge_changes_credit_wait_ns`), so the 64 MiB budget bound at planet;
since `179f1b7e` the readers never wait on it (below). The 1-OSC peak rises
from a single-threaded stream to the batch working set, accepted.

### One deflate per batch, no owned elements (bygg, 2026-10-31)

At `82be10a4` the workers were the bound (30.7 cores, readers parked
16.8 s summed on the credit ledger), but not because of the XML: every
quick-xml event went straight into a `GzEncoder`, so deflate ran once
per tiny write. `179f1b7e` writes each batch's XML to a plain buffer and
deflates it in one call, and replays batches into a byte sink that
writes the owned writers' bytes without building owned elements. The
borrowed sink alone, measured first as an uncommitted cell, moved the
7-OSC wall only from 9.9 to 9.35 s; the deflate change took it to 3.7 s.

| Cell | `82be10a4` | `179f1b7e` |
|---|---:|---:|
| 7-OSC `--osc-range 4914..4920` | 9.93 s (`c8b6af17`) | 3.73, 3.70 s (`ba9e4ad7`, `444fd7fa`) |
| 1-OSC `--osc-seq 4913` | 2.61 s (`b0175d10`) | 2.40 s (`bd19099c`) |
| 7-OSC `--simplify` | 34.2 s (`2f49e9c1`) | 17.7 s (`3ae86540`) |
| `tags-filter --input-kind osc`, 4913 | 6.25 s (`e6749613`) | 5.24 s (`c1e30598`) |

The 7-OSC run is now reader bound: 19 cores, no credit wait, peak anon
123 MB from 295 MB. The plain-XML output of the 7-OSC merge is
byte-identical to `82be10a4`'s; the gzip members differ in deflate
chunking only.

### OSC driver enforces the element hierarchy (bygg, 2026-10-30)

The OSC driver now tracks a fixed-depth level (root, section, object,
leaf) and refuses misplaced elements; the `End` arm pops by level instead
of classifying the closing name. Matched interleaved `--bench 1` cells on
bygg, baseline clean `b1f522f7` (stored), variant `b1f522f7` + change
(`--force`, unstored):

| Cell | Baseline | Variant |
|---|---|---|
| merge-changes planet 7-OSC, pair 1 | 9.886 s (`05f37130`) | 9.921 s |
| merge-changes planet 7-OSC, pair 2 | 9.900 s (`6d76714f`) | 9.914 s |
| tags-filter `--input-kind osc` planet 4913 | 6.205 s (`9e459dcd`) | 6.008 s |

All deltas are inside noise (+0.4 %, +0.1 %, -3.2 %); the hierarchy check
costs nothing measurable on the reader-thread-bound merge path.

The sections below record the earlier whole-input shapes.

### Streaming-path planet 7-OSC: 5.0× speedup (commit `99057fa`, plantasjen, superseded)

| Stage | UUID | Wall | vs serial baseline |
|---|---|---:|---:|
| Serial baseline | `c612c5e6` (commit `fb1719c`) | 272.6 s | 1.00× |
| Parallel parse, serial drain | `07ee92ee` (commit `43dd620`) | 235.8 s | 1.16× |
| **Parallel-drain (current)** | **`b6e964cc`** | **54.7 s** | **5.0×** |
| 1-OSC fast path | `941a5784` | 44.2 s | (within noise of pre-parallel 43.1 s) |

The middle row is the abandoned intermediate shape. It correctly
parallelized the 12.6 s parse phase (21× phase speedup) but exposed a
223 s serial drain on the main thread - per-change `quick_xml::Writer`
emit + zlib level-1 gzip-compress of 26.3 M changes, one thread,
~118 K changes/s ceiling. The current shape moves emit + gzip onto the
worker threads: each worker runs the full per-input pipeline (parse +
re-emit + gzip-compress) into its own `OscWriter<Vec<u8>>` and returns
self-contained gzip bytes. Main thread writes a pre-built prelude
gzip member, the 7 worker chunks in input order, and a postlude gzip
member. Multi-member gzip is valid (osmium, osmosis, gzip CLI,
`MultiGzDecoder` all support it); output decompresses to the
concatenation of all members.

A later bounded window over whole-input members still held every
admitted input's compressed output in memory, so with fewer inputs than
threads peak memory was about the output size (895 MB in `f24c29af`).

Phase breakdown of the 54.7 s `b6e964cc` run:

- `MERGECHANGES_PARALLEL_EMIT`: **54.1 s** - 7 workers running parse +
  XML emit + gzip-compress in parallel. Worker completion order
  (from per-worker `merge_changes_decompress_ns` counter timestamps):
  31.2 s, 33.3 s, 36.5 s, 40.3 s, 41.4 s, 47.9 s, 54.1 s. The longest
  worker is OSC 3 (5.8 M changes, the heaviest input by a factor of
  ~2 over the others) - the wall is gated by the heaviest single OSC.
- `MERGECHANGES_DRAIN`: **0.59 s** - main thread concatenates prelude
  + 7 worker chunks + postlude through a `BufWriter<File>` raw-bytes
  copy. Drain is essentially free; the work it used to do is now
  spread across the workers.

### Cross-dataset matrix (commit `16e3694`, plantasjen, `--bench 1`)

Pre-parallel baselines, retained for cross-dataset shape:

| Dataset | OSC count | Default wall | `--simplify` wall | Per-OSC effective rate | UUIDs (default / simplify) |
|---|---:|---:|---:|---:|---|
| Germany | 1 (`--osc-seq 4705`) | **2.5 s** | - | 2.5 s | `1ba15f41` / - |
| Germany | 7 (`--osc-range 4706..4712`) | **18.0 s** | 16.2 s | 2.6 s/OSC | `91cb8465` / `638a4b99` |
| Europe | 7 (`--osc-range 4716..4722`) | **153.2 s** | 152.9 s | 21.9 s/OSC | `993ae62a` / `745ee521` |
| Planet | 1 (`--osc-seq 4913`) | **43.1 s** | - | 43.1 s | `76f78e8b` / - |
| Planet | 7 (`--osc-range 4914..4920`) | **267.2 s (4m27s)** | 262.2 s (4m22s) | 38.2 s/OSC | `bef0f1fa` / `c0d140b6` |

Planet 7-OSC walls have dropped substantially at `abd1d9e`: default
to **54.7 s** (UUID `b6e964cc` at `99057fa`), `--simplify` to
**73.7 s** (UUID `3e3ef119` at `abd1d9e`). The germany/europe rows
have not been re-benched; expected speedup at those scales is similar
in shape (max-per-OSC + small drain) but smaller in magnitude because
the heaviest OSC dominates and germany 7-OSC walls are already short.

### Simplify-path planet 7-OSC: 3.6× speedup (commit `abd1d9e`, plantasjen)

| Stage | UUID | Wall | vs serial baseline |
|---|---|---:|---:|
| Pre-parallel baseline | `c0d140b6` (commit `16e3694`) | 262.2 s | 1.00× |
| Parallel parse only | `37fbe5b5` (commit `488d1f0`) | 220.9 s | 1.19× |
| **Parallel parse + parallel write_simplified** | **`3e3ef119`** | **73.7 s** | **3.6×** |

Phase breakdown of the 73.7 s `3e3ef119` run:

- `MERGECHANGES_PARALLEL_PARSE`: **12.3 s** - same shape as the
  streaming-path parse (workers each call `parse_osc_into` into a
  local `ChangeStream`, main thread concatenates in input order).
- `MERGECHANGES_SIMPLIFY`: **6.9 s** - serial `BTreeMap` dedupe of
  26.3 M changes into 25.4 M deduped; cheap relative to parse + emit.
- `MERGECHANGES_PARALLEL_EMIT`: **49.4 s** - the new shape. After
  dedupe, each non-empty action group (creates / modifies / deletes)
  is split into chunks of size `group.len().div_ceil(num_workers)`
  via rayon's `par_chunks`. Each worker emits its chunk as a
  self-contained `<action>...</action>` gzip member and returns the
  bytes. Same multi-member gzip output shape as the streaming path.
  This phase replaces a previous ~197 s serial XML + gzip emit
  through a single OscWriter (4.0× phase speedup).
- `MERGECHANGES_DRAIN`: **0.33 s** - main thread concatenates prelude
  + per-chunk members in (group, chunk-index) order + postlude.

At `abd1d9e` the simplify wall was 19 s slower than the streaming wall
on the same input: the dedupe phase (6.9 s) plus a less efficient parse
fan-out (workers parsed to a separate owned buffer in simplify, vs the
streaming path's fused parse and emit of that time).

Pre-parallel observations from the matrix that still hold:

- **`--simplify` is a near-zero overhead** at every scale measured
  (planet −5.0 s / −1.9 %; europe −0.3 s; germany −1.8 s / −10 %
  inside single-shot variance). The `BTreeMap` dedupe is cheap
  relative to the parse cost; the simplify path's win comes when
  multiple OSCs touch the same object IDs (which dailies on planet
  do at low percentages).
- **Per-OSC rate scales with input size, not OSC count**. Germany
  2.6 s/OSC, europe 21.9 s/OSC, planet 38.2 s/OSC. Each OSC is
  roughly proportional to the dataset's daily-diff size; planet's
  ~140 MB/OSC takes ~38 s of gzip + XML parse on the main thread.

### Pre-flight measurements that locked in the implementation choice

The `c612c5e6` instrumented re-bench (commit `fb1719c`) added a
`TimedRead<R>` wrapper around the file reader to attribute gzip
decompress wall separately from the surrounding `quick_xml` machinery:
**gzip = 1.5 % of wall** (3.98 s of 272.6 s, 1.4-1.5 % per-OSC). That
killed the parallel-decompress + sequential-XML alternative outright -
no win to extract there - and forced the buffer-and-drain shape that
landed at `43dd620` and `99057fa`. The same pre-flight added
`merge_changes_changes_per_osc` (per-input change-count delta) which
sized peak per-worker `ChangeStream` residual at 5.8 M changes for
the heaviest OSC.

The `4fc8e35` `--hotpath` companion (UUID `ee108ec9`, 2026-04-27)
recorded 7 calls to `parse_osc_streaming` totalling 264.42 s = 100 %
of wall, with per-OSC avg 37.8 s and **P95 50.9 s**. The `--alloc`
companion (UUID `13615a4a`) attributed 62.4 GB cumulative
allocation across the 7 calls entirely to `parse_osc_streaming`;
no other function showed on the alloc table. The parser was the only
allocation hotspot pre-parallel.

