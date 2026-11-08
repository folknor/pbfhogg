# Pre-release: work items, and what stands between here and 0.6.0

The working document for the 0.6.0 release and everything workable found on
the way to it. It absorbs `notes/bugs.md` and the four research notes of
2026-10-30 (cat --dedupe memory, merge-changes memory, pool-heavy RSS,
inspect --extended wall), plus the residue of the retired library-harness
note; git history keeps them.

This is a transient note. Nothing in it is guaranteed true, and nothing
durable (code comments, `reference/`, `docs/`, `decisions/`, tests) may cite
it, so durable text never cites a WRK identifier either. Every workable item
has a `WRK-NNN` identifier, whether or not anyone means to do it. An
identifier is never reused or renumbered; when an item is done or dropped it
is removed entirely, with no historical record left here. When the file is
empty, delete it.

---

## Status

The library-use gate is certified (`brokkr library --all --dataset denmark`
at `627883d7`: 12/12 scenarios, 17/17 capabilities). io_uring is exercised on
real data and its writer fixed. Multi-region extract admits its descriptors.
The planet regressions found by the RAM pass below are resolved:
`13ec0927` (cat --dedupe), `418739bd` (inspect --extended), `c97b9985`
(merge-changes), `5bfa37ab` (pool-heavy decode buffers) and `b1f522f7`
(apply-changes). The release itself still needs WRK-001 and WRK-003 to
WRK-005.

---

## Release

### WRK-001: Version bump and CHANGELOG date

CHANGELOG.md's section is headed `## Unreleased`; retitle it
`## 0.6.0 - <date>` when it ships. Both `Cargo.toml`s still say 0.5.0, and no tag after `v0.5.0`
exists. The 0.6.0 section was rewritten to net change against v0.5.0, one
line per entry; keep it that terse.

### WRK-003: Stored cells for the table's unstored and stale rows

Four rows cite `--force` cells that were never stored: `cat --dedupe`,
`apply-changes` (4913, `zstd:1`), both `merge-changes` rows and
`inspect --extended`. A stored HEAD cell each gives the table a UUID to cite.
Done for all of them and for the two `diff` rows, at `179f1b7e`
(2026-10-31, bygg); this item stays open only until WRK-004 consumes them.

### WRK-004: The README planet table

The maintainer may move the README table to bygg; the bygg columns below are
a draft of it. Its walls would need best-of-N cells on a quiet host first.
Rows known wrong at HEAD regardless of host: both merge-changes rows (2 MB;
no stored cell ever paired the 7-OSC "54 s" with "2 MB": the 54 s matches
`b6e964cc` / `f4de3f8d`, both about 950 MB anon, and the 2 MB cells are the
267-273 s serial era), `cat --dedupe` (the old serial engine on another
host), `inspect --extended` (no commit or host on the row), and the rows
whose README figure predates the fixes listed in Status.

### WRK-005: Expect declarations in brokkr.toml (brokkr#71)

brokkr `b68f3e3` (installed) fails an answer run whose verdict contradicts a
declaration: `pbf.<variant>.expect` with `check-refs`, `check-ids`,
`check-index` = `clean` or `violations`, and `diff` keyed by OSC seq =
`same` or `differs`; dataset-level `expect.diff-snapshots` keyed by the two
files' xxhash digests in either order. Absent keys store both answers.
`brokkr man datasets` has the syntax. Each declaration must be what the file
actually gives, not an assumption:

- Planet `pbf.indexed`: `check-refs = "clean"` (`0accad98`, exit 0) and
  `check-ids = "clean"` (`4edab6b2`, and `f6ba05d4` for `--full`), both
  post-split cells at `627883d7`. `check-index = "clean"` on the reindexed
  planet (`e7f431ab...`, both hosts).
- Denmark, germany, europe and the other regional extracts: every stored
  check cell predates the exit-code split (`854f9465`), so exit 1 there is
  ambiguous. A Geofabrik cut may carry relation members outside the region,
  so `check-refs` may genuinely report violations. One answer run each on
  post-split pbfhogg settles it.
- `diff` per OSC: a primary diffed against itself with its next OSC applied
  should be `differs` (denmark 4705, planet 4913, and so on).
- `expect.diff-snapshots`: planet base against `20260411`
  (`e7f431ab...` on both hosts, + `7facb638...`) is
  two different planets, `differs`.
- Deliberately broken snapshots: `unsorted` / `test-unsorted`
  (`bc689783...`) should give `check-ids = "violations"`, `test-no-index`
  should give `check-index = "violations"`. Confirm each with one run first;
  brokkr#71 notes `--unsort` breaks ID order only on large enough inputs.

---

## Planet RAM pass against the README table (2026-10-30)

Reference material for WRK-003 and WRK-004, not an item. The blocker
question was whether every command is still RAM-safe at planet on a 28-ish
GB host. Answer: yes. The highest peak is `inspect --tags -j 16` at 17.5 GB,
unchanged from the README. (Since superseded for `inspect --tags`: 36 s and
10.8 GB peak anon at default `-j` on bygg, reference/performance.md
"inspect tags counts into a sharded table"; `build-geocode-index` is now the
highest row below. Also superseded: `extract --complete` 2.76 GB and
`extract --smart` 2.87 GB, `tags-filter w/highway=primary` 2.27-2.31 GB and
56 s, on the reindexed planet, reference/performance.md "ID sets that hold
only what the writer does not already have" and "Direct and member ways kept
apart".)

Setup: host bygg (Ryzen 9 9950X3D2, 16c/32t, 30.5 GiB, 29.1 GiB available),
pbfhogg `627883d7`, one `--bench 1` cell per command after `fstrim`, input
`planet-20260223-with-indexdata.osm.pbf`. Peak anon RSS is the sidecar's
`anon` max. The README table was measured on a Ryzen 9 5900X (12c/24t), so
walls do not compare across the two columns (bygg is the faster host) and
are listed for reference only.

| Command | Peak anon, bygg | README | Wall, bygg | README wall | UUID |
|---|---:|---:|---:|---:|---|
| `inspect --tags -j 16` | 17.5 GB | 17.5 GB | 1m40s | 2m50s | `7855f2a4` |
| `build-geocode-index` | 13.4 GB | ~25 GB | 5m20s | 5m47s | `c261bd3c` |
| `add-locations-to-ways --index-type external` | 10.7 GB | 12.0 GB | 5m21s | 9m52s | `13668dda` |
| `multi-extract --simple -c` (5 regions, `-10,35,32,72`) (n9) | 9.90 GB | 9.4 GB | 2m08s | 14m44s | `eded8718` |
| `extract --smart` (`-25,34,45,72`) | 8.2 GB | 11.17 GB | 2m20s | 4m28s | `d015bbe4` |
| `cat --dedupe` (n4) | 1.15 GB | 1.4 GB | 4m23s | 2h13m | `5faa7ff8` |
| `multi-extract --smart -c` (5 regions, `-10,35,32,72`) | 6.9 GB | 22.9 GB | 8m04s | 13m58s | `d4d8d0dc` |
| `extract --complete` (`-25,34,45,72`) (n9) | 4.87 GB | 4.7 GB | 2m02s | 3m42s | `b9dd077a` |
| `apply-changes --locations-on-ways` (4913, keep-untagged base) (n2) | 3.9 GB | 3.8 GB | 4m57s | 5m05s | `6e9a63df` |
| `extract --simple` (`-25,34,45,72`) (n7) | 3.09 GB | 3.0 GB | 1m52s | 3m42s | `6b3c88cc` |
| `renumber` | 3.19 GB | 3.3 GB | 2m00s | 3m11s | `ce4acd6a` |
| `apply-changes` (4913, `zstd:1`) (n8) | 2.34 GB | 2.4 GB | 2m08s | 4m29s | `fd30dcc9` |
| `tags-filter w/highway=primary` (two-pass) (n7) | 2.57 GB | 2.6 GB | 1m01s | 1m55s | `b8491cfd` |
| `check --ids --full` (n9) | 2.37 GB | 2.22 GB | 37s | 1m10s | `8c7a09a2` |
| `check --refs` | 2.30 GB | 2.17 GB | 35s | 1m0s | `0accad98` |
| `cat --clean version` (n7) | 994 MB | 750 MB | 4m01s | 5m34s | `a8a4384e` |
| `time-filter` (cutoff 2024-01-01) (n7) | 841 MB | 812 MB | 2m27s | 4m24s | `c33e5ddd` |
| `merge-changes` (4914..4920, 7 OSCs) (n6) | 137 MB | 2 MB | 3.0s | 54s | `bac3e736` |
| `getparents` (n7) | 288 MB | 506 MB | 13s | 22s | `dd453b3f` |
| `tags-filter -R highway=primary` | 659 MB | 688 MB | 38s | 52s | `55f84dee` |
| `diff --format osc -j 16` (base vs 20260411) (n10) | 1.25 GB | 634 MB | 2m36s | 4m54s | `c55bebc9` |
| `check --ids` (n7) | 566 MB | 504 MB | 31s | 57s | `4edab6b2` |
| `inspect --nodes -j 16` | 425 MB | 410 MB | 31s | 56.8s | `b1fc3106` |
| `getid --invert` | 58 MB | 102 MB | 56s | 1m31s | `7ee6c776` |
| `inspect --extended` (n5) | 300 MB | 34 MB | 26s | 13m41s | `5b8e6c29` |
| `getid` | ~13 MB (n1) | 27 MB | short (n1) | 6.1s | `67d66e4e` |
| `sort` (sorted input) | 11 MB | 476 MB | 41s | 2m27s | `6bbe8ca4` |
| `cat` (indexdata generation) | 10 MB | ~200 MB | 1m00s | 1m27s | `c7b51564` |
| `cat --type way` | 10 MB | 10 MB | 30s | 45s | `3d4a36a8` |
| `merge-changes` (4913, 1 OSC) (n6) | 19 MB | 2 MB | 2.0s | 44s | `9d1e9170` |
| `inspect` | <1 MB (n1) | 5 MB | short (n1) | 6.5s | `3ec07278` |
| `tags-filter -i w/highway=primary` (n3) | 7.49 GB | 7.0 GB | 5m16s | 7m57s | `9bf96ab2` |
| `diff -j 16` (text, base vs 20260411) (n10) | 1.30 GB | 586 MB | 2m22s | 3m48s | `5e782b56` |

- n1: Too short for the 100 ms sampler (three samples); approximate.
- n2: The README row for this one was measured on a 9950X3D2 like bygg, so
  it is the one row whose walls compare directly.
- n3: Measured later the same day on brokkr `a8605c879`, after its fixes for
  brokkr#67 and brokkr#68; every other row ran on the morning's brokkr build.
- n4: Stored at `179f1b7e`; the `--force` cell before it measured 1.46 GB
  and 5m10s. The RSS-pass cell was `d271f04f`, 7.57 GB and 53m43s.
- n5: Stored at `179f1b7e` (the validator-projection scan); the RSS-pass
  cell was `9b49e8fb`, 26 MB and 13m22s.
- n6: Stored at `179f1b7e` (batch-deflated output; the 1-OSC peak is near
  the sampler's floor). The RSS-pass cells were `f24c29af` (895 MB, 30 s) and `bf6db512` (1.6 MB,
  24 s).
- n7: Peak from a `--force` cell on `c97b9985` plus right-sized, recycled
  decode buffers; walls unchanged per work phase and left as stored. The
  RSS-pass peaks were cat 1.24 GB, time-filter 1.16 GB, getparents 705 MB,
  check `--ids` 646 MB, extract `--simple` 3.52 GB, tags-filter two-pass
  2.84 GB, inspect `--extended` 571 MB.
- n8: Stored at `179f1b7e`; the RSS-pass
  cell was `f2b8beed`, 3.12 GB and 2m07s, and 2.97 GB after the decode
  buffer change.
- n9: Re-measured at `179f1b7e` for WRK-002; the pre-`5bfa37ab` cells were
  check `f6ba05d4` (2.47 GB, 34 s), extract `ce0bdcc0` (5.27 GB, 2m02s) and
  multi-extract `4babae4d` (10.3 GB, 10m53s). The multi-extract row is
  `eded8718` at `44c62853` (worker-side compression); at `179f1b7e` it
  was `831424c0`, 9.86 GB and 10m58s.
- n10: Stored at `179f1b7e` (the streaming work-unit diff, `587bfc12`).
  Before it: text `f9fa4f6f` (605 MB, 3m04s), osc `607cd897` (653 MB,
  4m11s). The osc row's peak and wall come from two `--force` cells of
  the derive spill and worker-deflate change (1.25 GB; 156.3 s and
  156.6 s), interleaved with stored `179f1b7e` cells `3660071f` and
  `955da7f4` (240.7 s, 241.3 s, 5.00 GB); `c55bebc9` (5.00 GB, 4m03s) is
  the stored cell before it. The text row's peak and wall likewise come
  from two `--force` cells of the single-sided element cut (1.30 GB;
  141.8 s and 142.3 s), interleaved with stored `c7eed182` cells
  `258cbdc4` and `ed12c024` (145.6 s, 146.5 s, 1.57 GB); `5e782b56`
  (1.56 GB, 2m28s) is the stored cell before it. Numbers in
  reference/performance.md.

Measurement artefacts in the RSS pass's percentages, settled: per-worker
peak was the same on both hosts for getparents, check `--ids` and
time-filter (23.5 vs 23.0, 21.5 vs 22.9, 38.6 vs 36.9 MB per worker at 30
vs 22 workers), so those rows grew with bygg's worker count; the extract
rows compare different bboxes (README `-10,35,32,72`, bygg `-25,34,45,72`);
multi-extract is about 4 % in consistent units, not 10 %. RSS fell on
`build-geocode-index` (the README's ~25 GB was probably total RSS; bygg's
total including file-backed mmaps is 18.8 GB), `multi-extract --smart`,
`extract --smart`, external altw, `sort` and `cat`.

---

## Memory and throughput: structural work

### WRK-178: Same-ID groups in the overlap sweep are held whole

The sweep still gathers every copy of one ID before writing any
(`emit_group` in `src/commands/blob_merge/sweep.rs`), uncharged against
`SWEEP_PINNED_BYTES`, because a cursor delivers an ID's copies in block
order and `sort` accepts descending versions inside a blob. Output now
ships block by block inside a group and the dedupe lookup is indexed past
`SWEEP_GROUP_INDEX_THRESHOLD`, so only the hold remains. Removing it means
a merge keyed on (ID, version, tie) with an eager (ID, version) pass over
every overlap blob before its cursor opens (sorted views where it fails):
a second pass over normal input, and live cursor blocks and views are
still held. Only adversarial or many-version input reaches the hold.

---

## Tooling

---

## The measurement campaign

Matched A/B cells planned at the end of the hygiene drain to bisect what the
drain changed. Today's regression fixes did not need them, but they remain
the way to find any regression the RAM pass could not see. "A vs B" means
matched, interleaved `--bench 1` cells with `--commit`; when a pair moves,
bisect inside it. Prefer stored cells where they already answer the
question.

### WRK-084: Ordered pools

Residue. The gate now notifies only parked workers and passthrough runs
ahead of its decode result (reference/performance.md, "Ordered pool:
notify only parked workers"); the window measured as never binding, so
stage 1 parking is not a cost. The `60e1b691` against `c2409c45`
attribution is superseded: the callers were reshaped since (`7e7ede73`,
`9ebcfa97`, `44c62853`), so HEAD is the shape to measure. Left:

- `WILLNEED` on the consumer thread (one `posix_fadvise` per consumed
  item on the serial writer, further ahead than the old dispatcher; it can
  enter reclaim at planet scale) is unmeasured against any alternative
  placement. Shares the footprint of WRK-098; the dispatcher-side
  advice on extract simple's unordered pools measured neutral at HEAD
  (reference/performance.md, "WILLNEED on simple's node and way classify").
- Claim workers against a dispatcher: no cell isolates it.
- altw external stage 4 under `--keep-untagged-nodes`, the case the eager
  passthrough targets, is unmeasured: brokkr cannot forward the flag.

### WRK-086: cat --dedupe passthrough across the engine move

Residue. The self-merge is measured on both datasets across `79d2e923^`,
`79d2e923` and HEAD (reference/performance.md, "Denmark and Japan across
the engine move": about 10x faster at the move, halved again since). A
self-merge puts every blob in an overlap run, so the move's coalesced
`copy_file_range` passthrough is unmeasured. Measuring it needs an input
pair whose plan shows nonzero passthrough with adjacent ranges that
coalesce (check `cat_dedupe_blobs_passthrough` and
`cat_dedupe_copy_range_coalesced` at HEAD first), then the same pair on all
three arms; `benches/cat/dedupe-self.lua` is the shape to extend with a
second input.

### WRK-091: diff-snapshots across the block-pair engine

Planet `diff-snapshots --from base --to 20260411`, text and osc,
`2c12a955^` vs `2c12a955`, expected unchanged. Recorded sequential planet
figures, which cannot be re-measured: text 2134 s (`a9d430f2`), osc
2225.6 s, peak anon about 28-41 MB per phase; header walk about 15 s cold.

An observed, unbisected slowdown worth checking first: `pbfhogg diff` of the Denmark
indexed file against its own `cat --dedupe` self-merge (equal elements,
different framing) took 1.29 s at `79d2e923` (an ancestor of `2c12a955`)
and 8.22 s at `4e0eaa50`, both `--commit` worktree builds on bygg, input
warm, one run each (`dedupe-self --check yes` cells `89f2ce1e` and
`f6d7708e`, the diff child). Each arm diffed its own merge output, so a
matched pair on the same two files comes first; then bisect.

### WRK-093: Unindexed sequential diff

`brokkr diff --dataset denmark --variant raw` (europe if it shows), with and
without `--type way`, `8881415e^` vs `8881415e` (per-element order check,
per-block kind classification, excluded kinds dropped unmerged; expected
faster under `--type`, slightly slower per element otherwise). Then
`94743e65^` vs `94743e65` separately (borrowed elements instead of owned
conversion, expected faster; the two sides' decoded-block peaks can now
coincide) and `126cbce2` (one `DecompressPool` per merge). No measurement of
the unindexed path exists at all. Check that brokkr's `diff` takes
`--variant raw`.

### WRK-094: Plain cat across the one frame decoder

Plain `brokkr cat --dataset planet`, `307982d7^` vs `307982d7`. The raw-frame
walks still zero-fill (`FileReader` implements only `read`, so std's default
`read_buf` zero-initialises) and an unsized `Take::read_to_end` starts at
8 KiB and doubles, about 4 read calls per 1.8 MB payload plus an extra copy
through the BufReader; estimated 25 us per blob, probably hidden if
I/O-bound. If it costs: implement `read_buf` on `FileReader` (which would
also remove a memset of every payload on every `BlobReader` path) or go back
to `resize` plus `read_exact`. The same cell covers cat's fd admission
(`56f8d9a9`) and the reframe's `MAX_BLOB_DATASIZE` compare (`f9625b94`).

### WRK-095: cat --clean version across the single ordered pass

`cat --clean version` on planet, `a05b3d25` vs `8da26b23` (one ordered pass
per input instead of three per-kind phases, window 64, not tuned): wall,
peak anon, stall counters, and whether workers stall behind a slow head
blob. Baseline `f2315551`: 333.8 s, 750 MB peak anon (plantasjen). The HEAD
half is done: `6f9896db` at `1f93e674` on bygg, 146 s and 766 MB, no window
or send wait and the consumer idle 89 % of the wall, so at HEAD no worker
stalls behind a slow head (reference/performance.md, "cat --clean's peak
against the README row"). What is left is the pair across `8da26b23`
itself.

### WRK-096: Writer cluster

One A/B of `ae646053^` vs `52690f1e` covers the per-block dispatch
(`ae646053`), the shared pipeline spawn (`3ab1b4ce`), the checked frame
header encoder (`f9625b94`), the one-meaning writer counters (`62046948`),
the shared ordered-delivery loop (`7abba0df`) and the OSC gzip writer's
`ReorderBuffer` (`52690f1e`). A block-encoding command on europe or planet
(repack or cat `--clean`) plus planet `diff --format osc` covers it. Bisect
only if it moves.

### WRK-111: altw external pass A exposure to sustained-write state

Stage 1 pass A runs 32 s on planet as the first cell after a trim or rest
and 51-111 s after another scratch-heavy cell, on identical bytes (29.2 GB
pread, 149.2 GB of id shards into 30 x 256 buffered streams);
reference/performance.md, "altw external pass A swings with run history".
Not input page cache. Open: which of trim debt, drive write cache, GC or
inherited writeback is the mechanism (needs a trim between consecutive
cells plus Dirty/Writeback and device stats, see the brokkr filing), and
whether scratch bytes per reference, total scratch traffic across stages or
the stream topology (stage1.rs already names decoupling shard ownership
from worker count) make pass A more exposed than it must be. Fewer streams
might help locality without fewer bytes; compression might cut bytes for
CPU. Neither is an established remedy.

### WRK-098: Shared versus separate input descriptors

About eight commands hand the `HeaderWalker`'s `FADV_RANDOM` descriptor to
their pread workers (classify, tags-filter, extract smart and multi,
getparents, geocode pass 1.5, diff's shard walk, altw sparse passthrough),
while altw external and apply-changes keep separate descriptors on an
argument (`a3f1a1bc`) never measured. The split may track access pattern
(selective readers with `FADV_RANDOM` plus `WILLNEED` against stage 4
reading every blob), so measure both shapes: for example altw external
stage 4 at europe scale with a variant handing the walker's descriptor to
the workers. Renumber is not on the shared-walker side, and `InputFront` is
a probe closed before any pass.

### WRK-099: apply-changes -l node boundary on the drain channel

`-l` on the planet `altw-keep-untagged` base, `ff0b0a37^` vs `ff0b0a37`: the
node boundary rides the shared 256-slot drain channel and publishes from one
readiness check. Probably neutral. Ride along: the OSC arena's `str::get`
instead of `from_utf8` per key, role and user (`20d10610`, argued cheaper).

### WRK-100: apply-changes -j 1 cost

What `-j 1` (accepted since `9744a9fc`) costs in wall and memory; nothing is
known.

### WRK-101: build-geocode-index codec commits

Planet, `d510bbd7` vs `067be2d8`, covering `64141296` and `067be2d8`; planet
baseline 347 s.

### WRK-102: digest walk folded into the decode pass

`c0f67d9e` (ADR-0009) touched the shared fused engine: `getid` or
`tags-filter -R` on planet, `c0f67d9e^` vs `c0f67d9e`. Expect noise.

### WRK-103: The Mutex Receiver dispatch question

Read `classify_unordered_desc_lock_wait_ns` against
`classify_unordered_desc_recv_wait_ns` on a planet tags-filter or extract
run (`reference/performance-history.md` has the history). Stored cells may
already answer it.

### WRK-104: Counter name sets survive the refactors

Observability, not performance: compare `brokkr sidecar <uuid> --counters`
name sets from an altw external Denmark run at HEAD and at `9dcf53a7^`, and
an apply-changes and a renumber run for `13dd02aa`.

### Campaign reference: how to measure

- Baselines first from `brokkr results`: a recent stored cell on the same
  host is the baseline; run only the variant. Variants run uncommitted with
  `--force` and are not stored, so record their numbers in
  `reference/performance.md`.
- Interleave matched cells when both arms must run: `--bench N` is best-of-N
  within one cell and cannot cancel drift between cells.
- Noise floor: ignore differences below 2-3 %. Planet single-sample walls
  carry a 5-8 % environmental band on the bench host, and a cold schedule
  header walk adds about 6 s per walk (`notes/todo.md`).
- Read the phase split before calling any delta: `brokkr sidecar <uuid>
  --human`, and `--compare` for a pair (check its host-difference annotation
  first). `brokkr sidecar <uuid> --stat anon` gives peak anon cheaply.
- One cell at a time, never in parallel. Old commits build through
  `--commit <ref>`; the first build per commit is a cold rebuild.
- Numbers written into markdown carry commit hash and hostname.

### Campaign reference: reading rows across these commits

- Rows from `56a50e0d` up to `c2409c45` (the ordered pools' result channel
  as deep as the window) do not compare on send-side counters (`s4_send_ms`,
  `WAIT_S4_SEND`, `WAIT_P2_SEND`, `altw_pass2_send_ms`) or on consumer-bound
  memory.
- `6592af1f` (protohoggr 0.5.0) changed output bytes: `BlockBuilder` output
  shrinks for dense uid / user_sid deltas of magnitude `2^30` or more.
- `CAT_NODES_*` / `CAT_WAYS_*` / `CAT_RELATIONS_*` are absent from
  `8da26b23` until `168e581a` and since then mean write-side kind
  transitions, so per-kind rows against `f2315551` are approximate;
  `SORT_OVERLAP_PARALLEL_*`, `DIGEST_ENVELOPE_*` and `writer_write_ns` are
  gone for good.
- Hotpath names moved: getparents, time-filter, repack, degrade and
  multi-extract's writer report under `parallel_classify_phase_ordered`;
  command pipelines under `run_pipeline` instead of `run_pipeline_fused`;
  cat `--clean`'s walk under
  `build_classify_schedule_blob_filtered_accepting_partial`.
- Every ordered pool records `ordered_pool_*_wait_ns`, every unordered
  classify pool `classify_unordered_*_wait_ns`: one process-wide running
  total emitted after every pool run. `classify_unordered_desc_lock_wait_ns`
  is the shared-receiver mutex's own cost only while
  `classify_unordered_desc_recv_wait_ns` stays near zero.
- The encode-storage change (2026-11-01, the commit after `5789830e`)
  removed the writer's `Framed` chunk: every pipelined frame is now one
  owned buffer, so `writer_frame_ns` gains the header-and-body copy the
  writer thread used to do inside `writer_write_ns`, and multi-extract's
  region frames are counted as raw items where they were raw chunks.
  Writer counters split across it compare only with that in mind.
- `5bfa37ab` changed decode-buffer sizing and pool retention everywhere:
  peak anon of any pool command compares across it only with that in mind.
- Exit codes: `854f9465` split them (1 an answer, 2 a failure; before, both
  were 1); `50a9fc06` made `diff --format osc` exit 1 on differences (it
  always exited 0 before). brokkr `39d0988` stores exit 1 as an answer on
  builds with the split; read the stderr of a pre-split `--commit` cell that
  exits 1.

### Campaign reference: inputs

- The planet `altw-keep-untagged` variant for `apply-changes -l`, passed by
  brokkr's `--locations-on-ways` (the plain `pbf.altw` base is refused: OSC
  4913 references about 4.2M nodes it dropped as untagged).
- A Denmark unsorted snapshot (`unsorted` and `test-unsorted` are the same
  file) and an unindexed europe or planet input (`pbf.raw`).

### Campaign reference: unmeasured changes with no cell of their own

Each was argued neutral or below noise. Listed so a bisect landing near one
knows what it changed.

- Write side: `frame_in_scratch` (`1db71d8a`, `f9625b94`); the permit taken
  through `acquire_permit` before spawn (`1db71d8a`); `fadvise::advise` on
  every `posix_fadvise` (`d510bbd7`); the copy loops made two (`bd2de78d`).
- Read side: `BlobReader::next` as `next_frame` plus `into_blob`, eviction
  before the envelope parse (`8881415e`); `OrderWatermark`'s mixed-sign
  `min_id` (`8881415e`); one `statx` per direct open (`ea17fd86`); the
  scratch borrows in the fused engine (`c0f67d9e`); the OSC arena reads
  (`20d10610`); `BlobReader::leading_header` judging the first frame before
  its body on every `ElementReader` (`418739bd`).
- Counters: one struct per altw stage (`9dcf53a7`); `record_max` on the two
  production `fetch_max` sites; apply-changes' and renumber's sets
  (`13dd02aa`); the unordered pools' gated stall helpers (`003fe717`); read
  metrics emitted after the scope joins, plus the fsync funnel (`8384fa29`).
- Pools: the accumulate pool's stop flag (`95eea09b`); the unordered
  classify pools on one driver (`00ed5044`); the altw way reframes' shared
  block walk (`285df56d`).
- Commands: `apply-changes --force` reframing untouched unindexed blobs in
  the worker (`f703b02b`); degrade's sort-preservation guard (`9cce24b3`);
  the `expect_used` restructurings (`11becae7`); `check --ids`' extra
  type-order comparison (`4c254750`); input pinning (`926d5183` onwards).

---

## Handoff

What a session picking this up needs that is not written elsewhere:

- **brokkr state.** brokkr#65 landed (`39d0988`, exit 1 stored as an answer
  on split builds) and #71 landed (`b68f3e3`, declared answers; WRK-005).
  #64, #67, #68, #69 and #70 are answered. Stored short-run rows from before
  brokkr `a8605c879` carry up to ~100 ms of wall error.
- **io_uring on bygg** needs memlock raised for the session (the deferred
  io_uring item in `notes/todo.md`); the writer tests skip and
  `--io-uring` refuses at preflight otherwise (RLIMIT_MEMLOCK is 8 MB).
  A `!` shell's `$PPID` is the `claude` process, so the user runs
  `! sudo prlimit --pid $PPID --memlock=unlimited:unlimited`; shells and
  brokkr runs started afterwards inherit it (`! ulimit -l` confirms).
- **Running the binary directly:** only `./target/release/pbfhogg` (with
  the `./`) is allowlisted. brokkr deletes its bench output after a run, so
  comparing two outputs byte for byte needs direct runs into
  `data/bench-tmp/`; `xxhsum` is not installed and `diff` is broken in
  the tool shell, `git diff --no-index --stat A B` works (no output means
  identical).
- **Filing in brokkr's bz from here:** the Bash hook rejects a command that
  names brokkr together with a redirect, a pipe or backticks, so pass the
  body with `-b '...'` in single quotes and no backticks.
- **`sudo fstrim -av`** runs without a password, but after heavy scratch
  churn it can take over 2 minutes; give it a long Bash timeout.
- **Never `brokkr verify` above germany** (AGENTS.md). `brokkr verify` has no
  `inspect` or `merge-changes` subcommand.
- **Do not `git stash` with a dirty `.brokkr/results.db`:** a stash dropped a
  stored row on 2026-10-30.

## What the maintainer has made clear

- **Performance on normal input is what matters; corrupt input is not.** A
  check that only refuses malformed input and costs per-byte or per-element
  work on a hot path is the wrong trade (BUG-191's width check was reverted
  on that ground). Such findings end as comments plus a reference-doc
  correction.
- **A silent wrong answer is worse than a refusal.** An empty user or role,
  a default metadata block, fewer nodes: refuse instead. A broken
  internal invariant fails in release builds (ADR-0021).
- **Follow protobuf's specified decoding where it costs nothing**, with
  osmium as the parity reference. Deviations are deliberate and recorded in
  DEVIATIONS.md.
- **Commands fail loudly and point at `cat`.** Every command refuses a
  partially indexed input except `cat` (every mode), `check --index` (a
  violation) and `inspect`'s descriptions. `cat` is the recovery command.
  `notes/todo.md` holds the wider "`cat` as the universal cleaner" and
  "refusals name both remedies" projects.
- **Inputs are regular files in every mode**; FIFOs, sockets and devices are
  refused at pinning.
- **Indexdata is believed (ADR-0013)**, including the header's
  `Sort.Type_then_ID` claim; `check --index --ids` is the admission test.
- **Observability is a contract.** Counter names, units and the `WAIT_*`
  spans survive refactors. `_ms` counters truncate per sample on purpose;
  keep the unit and recording point when moving timing code.
- **`reference/performance-history.md` is a record of numbers, not of
  machinery.**
- **Do not remove ignored `direct_io` parameters.** They are deliberate
  plumbing for the deferred CLI-surface redesign.
- **One engine over two.** A slow special case gets a better path inside the
  one engine, not a second engine back.
- **The record informs, it does not rule.** Memory moves rather than
  vanishing, and several "obvious" wins measured as losses; but code,
  toolchain and models have moved, so an old failure may have been its
  execution, not its premise.
- **Commits stay granular** so `--commit` bisects land on one decision.
- **protohoggr is the maintainer's crate.** A defect there is fixed there.
- **BUG-009** is the maintainer's own project.
- **571 MB for `inspect --extended` at planet is fine** (2026-10-30): modest
  memory at planet scale is not a reason to hold back a large wall win.

## Facts learned the hard way

- **The two-arena penalty was the string table** (2026-11-01): the block
  string table's `Rc<str>` per unique string, freed in bulk at every block
  end, carried about nine tenths of what `MALLOC_ARENA_MAX=2` cost the
  re-encoding commands; per-blob buffers carried the rest. The earlier
  claim here that a fresh `BlockBuilder` per blob was "deliberate and
  measured" had no measurement behind it. Workers that re-encode every blob
  now keep one builder for the run; getparents still builds one per hit
  blob, because a query hits few blobs.
- **glibc realloc** (`research/glibc/malloc/malloc.c`): an mmapped chunk
  grows by `mremap`; an arena chunk grows in place or moves. Freeing an
  mmapped chunk raises the process-wide `mmap_threshold` to its chunk size
  (chunks up to 32 MiB; a larger free leaves it alone) and `trim_threshold`
  to twice that, which is why buffers that stop
  being freed can push later allocations onto mmap (time-filter paid 29 s of
  kernel time for it before `5bfa37ab` pinned the threshold, but only under
  the two-arena cap). Read the source rather than reasoning from memory.
- **Two malloc arenas are two locks** (2026-11-01): `M_ARENA_MAX=2` makes
  every allocation above the tcache limit from every pool thread take one
  of two arena mutexes, and an arena-backed `realloc` that moves copies
  under it. Every measured re-encoding command paid 10 to 82 % more planet
  wall for it, apply-changes 20x; the memory it was added for had already
  gone with the decode-side string tables.
  The CLI sets no allocator policy; fixed mmap thresholds were measured
  across commands at planet (reference/performance.md, "glibc's sliding
  mmap threshold across commands") and none was a uniform win.
- **brokkr does not see glibc tunables** (2026-11-04): a
  `GLIBC_TUNABLES` arm is invisible to `results.db` (not captured, so not
  greppable), and `--force` on a clean tree stores the cell as if it were
  the default. Run such arms with the tree dirty so they stay unstored.
- **Per-blob decode buffers at planet** (`5bfa37ab`): the old 4x-compressed
  reservation plus the table append put every buffer over the pool's cap,
  and glibc kept the freed memory resident. Raising the cap alone made it
  worse; right-sizing had to come with it.
- **`par_fold_blobs` budget rule** (`read/reader.rs`): a budget below a full
  partial batch plus one maximum blob blocks forever. Compile-time asserted
  for production constants.
- **The claim-counter pools** (altw external stages 2 and 3, geocode pass
  1.5) lock a first-error mutex on every claim.
- **apply-changes `-l` barrier**: the node boundary is a
  `DrainMessage::NodeBoundary` on the drain channel; the 25 ms
  `DRAIN_RECV_TIMEOUT` only wakes the drain to notice failure or
  cancellation. Fault-plan hooks cover every barrier ordering
  (`tests/fault_apply_changes.rs`).
- **Premises drift.** Research notes were wrong on several load-bearing
  points when checked against code (the pool-RSS cause, the pwrite frame
  size, apply-changes' real holder); read the code before building on any
  claim here.
- **Sparring found real bugs** each time codex was asked to look past the
  proposal. `review` refuses stdin over 20,000 bytes, and a session idle for
  about an hour is refused as cache-cold.
- Shell here: `mv` is not allowlisted (`cp` then `rm -f`), and `rm` prompts
  without `-f`. There is no `/usr/bin/time`; for the peak RSS of a run too
  short for the sidecar's sampler, the user runs
  `TIMEFMT='%M kB max RSS'; time <binary> ...` with `!`. Debug builds do not
  tell you about release residency. A subagent working alongside reports
  your own uncommitted edits as someone else's.
