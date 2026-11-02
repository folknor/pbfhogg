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
(apply-changes). The release itself still needs WRK-001, WRK-003 to WRK-005 and the
maintainer's rulings in WRK-137 and WRK-146.

---

## Release

### WRK-001: Version bump and CHANGELOG date

CHANGELOG.md's section is headed `## 0.6.0 - 2026-10-30`; set the real date
when it ships. Both `Cargo.toml`s still say 0.5.0, and no tag after `v0.5.0`
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
  post-split cells at `627883d7`. `check-index` never ran on it.
- Denmark, germany, europe and the other regional extracts: every stored
  check cell predates the exit-code split (`854f9465`), so exit 1 there is
  ambiguous. A Geofabrik cut may carry relation members outside the region,
  so `check-refs` may genuinely report violations. One answer run each on
  post-split pbfhogg settles it.
- `diff` per OSC: a primary diffed against itself with its next OSC applied
  should be `differs` (denmark 4705, planet 4913, and so on).
- `expect.diff-snapshots`: planet base against `20260411`
  (`3ce64c66...` + `7facb638...`) is two different planets, `differs`.
- Deliberately broken snapshots: `unsorted` / `test-unsorted`
  (`bc689783...`) should give `check-ids = "violations"`, `test-no-index`
  should give `check-index = "violations"`. Confirm each with one run first;
  brokkr#71 notes `--unsort` breaks ID order only on large enough inputs.

### WRK-137: Maintainer ruling on inspect tags' key-only rows

A decision for the maintainer before 0.6.0. osmium's `tags-count`
(`research/osmium-tool/src/command_tags_count.cpp`) counts the key alone
when an expression has no value matcher (`add_matcher` routes it to
`m_keys_filter`), and with no expression at all it sets
`m_keys_filter = osmium::TagsFilter{true}` and lists keys. pbfhogg's
`inspect tags` always counts key=value pairs, three tab-separated fields
per row: `inspect tags highway` prints one row per `highway` value where
osmium prints one `highway` row.

The docs now describe today's behaviour as a deliberate deviation
(DEVIATIONS.md "inspect tags always counts key=value pairs", both
osmium-parity pages), written 2026-11-02 without a ruling. Either confirm
it, and this entry goes, or adopt osmium's key-only rows: a CLI output
change (key-only rows need their own row shape, a two-field row or an
empty value), a CHANGELOG entry, and those three doc passages rewritten.
The counting engine (the sharded table landed in `befcab8e`) counts per
key=value text; key-only rows would need a per-key tally beside it or a
fold of each key's rows at output.

### WRK-146: Maintainer ruling on ADR-0005 and release-mode invariant panics

A decision for the maintainer before 0.6.0.
`decisions/0005-latent-invariant-debug-asserts.md` (Accepted) makes
`debug_assert!` plus a comment the default disposition for a latent
internal invariant and rejects runtime enforcement for its per-blob cost.
The maintainer's standing line in this file ("What the maintainer has made
clear") is the opposite: for a broken internal invariant, a release-mode
panic over a `debug_assert!`. The ADR is the durable record a reader will
trust, and nothing amends or supersedes it. Rule on how the two reconcile
(for example: release-mode checks wherever the check is off the
per-element path, `debug_assert!` only where it would cost per element);
an amended or superseding ADR follows.

---

## Planet RAM pass against the README table (2026-10-30)

Reference material for WRK-003 and WRK-004, not an item. The blocker
question was whether every command is still RAM-safe at planet on a 28-ish
GB host. Answer: yes. The highest peak is `inspect --tags -j 16` at 17.5 GB,
unchanged from the README. (Since superseded for `inspect --tags`: 36 s and
10.8 GB peak anon at default `-j` on bygg, reference/performance.md
"inspect tags counts into a sharded table"; `build-geocode-index` is now the
highest row below.)

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

## Residual memory above the README

### WRK-010: cat --clean at 824-838 MB against 750 MB

Planet `cat --clean version` peaks at 824 and 838 MB (bygg, 2026-11-01,
the encode-storage change: one builder per worker for the run, one exactly
sized frame per block, the string table on one byte arena), against
898-913 MB at `5789830e` and the README's 750 MB. The 750 MB baseline
(`f2315551`) predates the single ordered pass and ran under the two-arena
cap. What holds the remaining 75-90 MB is unattributed: the 64-task window
of owned frames is the obvious candidate, and which phase peaks has not
been re-read since `5bfa37ab` put it in relations.

### WRK-115: repack and degrade relation phase at 1.8 GB

With glibc's default arenas (the CLI's two-arena cap is gone; see
reference/performance.md, "The CLI no longer caps malloc arenas") planet
repack and degrade `--strip-locations` peaked at 1.93 GB in the
relations phase, against 1.37 and 1.38 GB under the cap. The
encode-storage change (workers keep one builder, string table on one
byte arena; reference/performance.md, "Encode storage kept across
blocks") brought them to 1.79-1.80 and 1.78-1.80 GB; the rest of this
entry describes the cells before it and has not been re-read since. In repack's
uncapped cell that phase ran about 4 s at 8 cores with 65 threads, after
a ways phase peaking at 1.19 GB; pinning `M_MMAP_THRESHOLD` alone gave
repack 1.70 GB. Repack's ordered pool admits its default window (8 tasks
per worker, at least 32) and its consumer batches `FRAME_BATCH` blocks;
which holds the extra half GB, and whether it is retention across the
threads' arenas or live data, is unmeasured. Nothing in the phase sizes
against `Resources::memory_available`.

---

## Memory and throughput: structural work

### WRK-147: apply-changes' unattributed persistent live growth

At planet (zstd:1, OSC 4913, 30 workers, `eeb7ab6c`) a diagnostic that
counted the bytes of every pipeline stage (candidates, decode buffers in
use, worker `read_buf`, idle decode-pool buffers, rewrite frames, the
drain channel, the reorder map, the drain's current item, frames past the
writer handoff) left live heap (`mallinfo2` `uordblks`) beyond their sum
rising from about 0.53 to 0.80 GB over the late node phase and at the
node-to-way transition, and staying there to the end. The starting figure
holds whatever of the parsed diff (about 530 MB) does not sit in mmapped
chunks, which `uordblks` excludes; that split was not read. The growth is
what matters. Candidates, none measured: the 30 workers' `BlockBuilder`
buffers (each released only past `MAX_RETAINED_SCRATCH_CAPACITY`, several
per builder), the thread-local
framing scratch of the workers and of the 32 executor threads built at the
first create batch, `RefreshScratch` and the group scratch vectors.
Count them the same way before choosing anything; with the drain-channel
backlog gone this is the largest unexplained live share of the peak. The
diagnostic's patch is reproducible from the description in
reference/performance.md, "apply-changes' drain channel held the node
phase's backlog".

### WRK-123: Dense-node block body copied twice in the encoder

`BlockBuilder::encode_dense_nodes_group` encodes the DenseNodes message
into `group_buf`, copies it into `elem_scratch` to wrap it as
PrimitiveGroup field 2, then copies that into `encode_buf` as
PrimitiveBlock field 2: two full copies of the node body (the largest
message in a node block) per block, on every node-writing command. The
nested lengths are computable before writing (the packed fields'
lengths are known once encoded), so the body could be written once into
`encode_buf` behind both length prefixes. Not measured; establish the
cost per node blob at planet first, since it may sit under the 2-3 %
floor.

### WRK-127: apply-changes' parallel create runs build a fresh builder per block

`CreateEncoder::emit` (`src/commands/apply_changes/stream_output.rs`)
encodes a run longer than one block with `par_chunks(cap)` on the
executor and makes a `BlockBuilder::new()` per chunk, so up to
`APPLY_CHANGES_CREATE_BATCH_BLOCKS` fresh builders per batch, each
preallocating its dense vectors for the element cap: the per-block
fresh-builder shape the re-encoding commands' workers left. At planet it
is about 300 blocks at the node-to-way transition, so the cost is bounded
and one-time; whether it shows in the transition's live burst is not
measured. A builder per executor thread (thread-local, or `map_init`)
would follow the worker rule, if the measurement says it matters.

### WRK-128: apply-changes' worker scratch does not follow the release rule

`worker_loop` (`src/commands/apply_changes/streaming.rs`) keeps
`read_buf`, the group scratch vectors, `node_groups_scratch`,
`local_coords` and `RefreshScratch` for the run without
`commands::release_scratch`, so each worker pins the largest blob or
block it ever met in each. The ordered-pool workers release past
`MAX_RETAINED_SCRATCH_CAPACITY` per task. At planet blob sizes (p99 about
2.7 MB) the bound is small; an input with outlier blobs would hold 30
outliers per buffer for the rest of the run. Apply the rule per
candidate, or record why these differ. Measured on bygg at `eeb7ab6c`
(30 workers): the `read_buf`s hold 150-170 MB together on planet, and
under `-l` (altw base) 157 MB in the node phase and 351 MB once way blobs
start, about 11.7 MB per worker; `pread_into`'s `resize` grows capacity
geometrically, so a buffer can hold up to twice the largest frame it
read. A plain 4 MiB release rule would drop and regrow most of them on
every planet blob, so the rule needs measuring, not just applying.

### WRK-022: Byte-budgeted fused task engine

One execution model for decode, transform, encode and frame under one byte
budget, replacing the unordered classify pool, `run_ordered`, the writer's
separate rayon framing dispatch and apply-changes' private
scanner/worker/drain pipeline. Admit tasks in sequence order against the
budget; reserve a completion envelope before a task runs (piecemeal credit
acquisition deadlocks: every worker holds input credit while waiting on
output credit and the earliest task starves); release credit when storage
becomes reusable (ordering reservation at ordered delivery, output lease at
I/O completion). Makes transient memory a chosen constant independent of
worker count. Moving storage from workers to tasks does not lower peak by
itself at saturation; the budget value is the memory control, and fusion
shrinks the envelope so the same budget sustains more concurrency. Risks:
the admission protocol's liveness around the earliest task, oversized and
hostile blobs, cancellation and error paths that release leases, and a byte
ledger does not bound pages glibc retains. apply-changes now bounds outstanding seqs by count
(`apply_changes/window.rs`, 512, non-binding at planet with in-flight
peaks of 300-410); a byte budget here would replace that window, not sit
beside it.

### WRK-116: getid --add-referenced follows way refs only

`getid --add-referenced` pulls in the nodes of requested ways and nothing
else. osmium's `--add-referenced` also follows relation members (relations
in relations, then the nodes and ways of those relations) before the way
pass (`research/osmium-tool/src/command_getid.cpp`,
`find_referenced_objects`). docs/cli/commands.md describes pbfhogg's
narrower behaviour, but DEVIATIONS.md has no entry. Decide: implement the
relation closure (a relation pass on the projection pools, then the way
pass over the closed set) or record the deviation.

### WRK-131: inspect tags with its default thresholds at planet is unmeasured

Parked behind WRK-137 (maintainer, 2026-11-02). Low value on its own: a
bare planet run prints about 187 M rows, gigabytes of text no realistic
caller wants, and the 10.8 GB counting table is the same at any
threshold, so only the row build and serial sort are unmeasured. If
WRK-137 adopts osmium's key-only rows, a bare run lists keys and this
entry goes; if it confirms key=value rows, decide then whether the
default is worth a measurement.

Every brokkr `inspect --tags` cell passes `--min-count 999999999`, so no
measured run builds output rows. With the default `--min-count 1` every
distinct pair becomes a row: at planet 187.4 M `TagCount`s (counter
`tags_count_distinct_pairs`), each owning a key and a value `String`
(`table::CountTable::into_counts` in `src/commands/tags_count/`), then one
single-threaded `sort_by` over them and the formatter. The rows plus the
table still held for the shards not yet drained may exceed a 30 GB host;
the sort alone is a long serial tail. Measure a default-threshold planet
run first (it needs a brokkr cell that does not pass `--min-count`). If it
does not fit, the shape is to sort and print from the shards without
owning the strings, which the public `Vec<TagCount>` return type rules
out: a streaming or borrowed-row API.

### WRK-125: Sequential decode loops hand their only buffer to each block

The unsorted single-pass extract paths (`src/commands/extract/simple.rs`,
two sites, and `src/commands/extract/smart.rs`) and ALTW external stage 4
(`src/commands/altw/external/stage4.rs`) build each block with
`PrimitiveBlock::from_vec(std::mem::take(&mut decompress_buf))` and no
pool, so the loop's reusable buffer leaves with every block and the next
blob decompresses into a fresh allocation that grows to size. One
allocation, regrowth and free per blob on those paths. A
`DecompressPool` (as the unordered pools use) would return the buffer
when the block drops. Unmeasured; establish the per-blob cost at planet
before changing it, since the stage 4 path may sit under the noise floor.

### WRK-126: Raw-scan pool buffers escape the decode retention policy

`parallel_scan_blobs_raw` (`src/scan/classify.rs`) gives each worker a
plain `Vec<u8>` decompress buffer, not a `DecompressPool` buffer, so the
retention limits the block and projection pools apply
(`DECOMPRESS_POOL_MAX_RETAINED_CAPACITY` and its siblings in
`src/tunables.rs`) do not reach it: each worker keeps the
capacity of the largest blob it has decompressed for the life of the
phase. Bounded by the blob size limit per worker, so a worst case of
workers times the largest blob on a hostile file. Decide whether the raw
pool should take a pool buffer (it hands the caller `&[u8]` either way)
or whether its phases are short enough that retention does not matter.

### WRK-027: Membership track for set-dominated peaks

The rows dominated by ID sets (extract, multi-extract, tags-filter two-pass,
check `--ids --full`, the apply-changes overlay): fewer coexisting sets,
release each set after its last dependent phase, a shared region-membership
representation for overlapping multi-extract regions, partitioned or
spillable dependency state where a hard ceiling matters. "Swap `IdSet` for a
compressed bitmap" is too shallow, since `IdSet` already has sparse and
dense chunks. Bolder for check `--ids --full`: on input proven strictly
sorted, strict monotonicity already rules out duplicates, so the dense sets
can go, with a fallback for unsorted input.

### WRK-029: Collapse byte-equal twin frames in dedupe

For entries with equal kind, range and frame length across inputs, verify
payload byte equality and collapse to one representative (and recognise the
same file passed twice by identity); the representative plans as a lone
entry and is copied through. Makes a self-merge a copy and keeps way
enrichments a rewrite drops. Deliberately not done in `13ec0927`: it is
benchmark-shaped, since two different snapshots share no frames. Semantics:
internal duplicates inside a passed-through twin survive, as they already do
in any lone blob.

### WRK-030: Hard memory bound for giant dense overlap components

A dense overlap component still pins the decoded blocks of its live overlap;
sparse tails are demoted to owned elements past `SWEEP_PINNED_BYTES`, dense
ones cannot be. A hard bound needs bounded sorted runs plus an external
merge, or an osmium-style per-source stream merge with globally
byte-bounded prefetch inside oversized components. Only `sort` of
adversarially unsorted input reaches this.

### WRK-031: Budget for oversized frames in the overlap sweep

The per-run chunk bound is `window x depth x max(OVERLAP_RUN_CHUNK_BYTES,
largest frame)`. A frame near the 32 MiB format limit (`--compression none`
on a huge block) travels alone and is not otherwise budgeted.

### WRK-032: Dedupe pass 2 buffered under --direct-io --io-uring

Pass 2 (sort and `cat --dedupe`) now reads `O_DIRECT` under `--direct-io`
unless the output splices and the plan copies some frame through
(`blob_merge::open_sources`). That case - `--direct-io --io-uring` with any
passthrough - still reads the sweep buffered. Closing it needs a second,
direct descriptor per argument for the sweep beside the buffered one the
splice uses, which changes the descriptor grant (`CAT_EXTRA_FDS`, the
`FdPool` doc, the open-count tests in `tests/fault_cat_input.rs` and
`tests/fd_pool.rs`); or a per-source choice (direct only for inputs with
no copied frame). Belongs with the deferred `--direct-io` reach redesign in
`notes/todo.md`.

### WRK-033: A sweep test at planet blob density

No test runs the overlap sweep at planet blob density (about 228k elements
per blob); nothing guards sweep memory. Resource tests should assert bounded
admission and progress, never a host's RSS.

### WRK-148: check accepts --direct-io where no selected check reads it

`check --refs --direct-io` and `check --locations --direct-io` (either
alone, or the two together) are accepted, and each check prints a `Note:`
that `--direct-io` has no effect on it (`run_check` in `cli/src/lib.rs`;
the check table in `docs/cli/commands.md` documents the Note). The command
line alone makes the flag ineffective there, which the Flag rules section
of the same document says is refused (exit 64) and "never warned about or
ignored", so the two passages contradict each other. Either refuse it in
`validate_check` when no ID or index check is selected (the rule `--max-errors`
now follows), or state the exception in Flag rules with its reason. Check
first what brokkr passes: a `--direct-io` variant of `check-refs` or
`check-locations` would turn into a usage error under a refusal, and
brokkr#88's rule refuses an axis flag only where no pbfhogg revision gives
it effect. The library parameters stay either way (ignored `direct_io`
parameters are not removed as dead).

### WRK-149: extract polygon membership is not osmium's integer rule

Bbox extract membership now matches osmium element for element (DEVIATIONS.md
"extract: per-strategy membership against osmium"); polygons do not.
osmium (`research/osmium-tool/src/extract/extract_polygon.cpp`) quantizes the
polygon's vertices to decimicrodegrees, returns inside for a node equal to a
segment endpoint, and decides the rest with an integer crossing test over
y-bands. pbfhogg keeps the rings in f64 and runs `geo::point_in_ring` (f64
even-odd, no vertex case) on `e7 * 1e-7`. A node on a vertex can therefore
be excluded where osmium includes it (square `(0,0),(1,0),(1,1),(0,1)`,
node `(1,1)`), cascading into ways and relations. Converging means
quantizing rings at parse (`parse_ring`), an integer crossing test with
the endpoint rule, and an antimeridian story for the integer domain
(pbfhogg unwraps longitudes past 180, osmium does not), and it interacts
with the recorded polygon differences already in the parity pages (every
FeatureCollection feature, ring validation). The prefilters in
`Region::contains_decimicro` stay sound under either rule. Once done, the
brokkr verify filing for exact membership can take polygon regions too.

### WRK-037: ParallelGzipWriter window size

At 32 workers the window admits up to 160 sequences x 2 MiB, about 320 MiB
of raw payload. Bounded, but not small; matters if anything reuses its
sizing.

### WRK-042: Per-blob summaries written by the producer

A versioned per-blob extension carrying measured counts, ID boundaries,
timestamp range, bbox and metadata coverage would let a repeated inspect
skip decompression. Stored summaries are claims, so this is a second
product ("a description from claims") beside the measurement; partial
coverage could be filled by scanning only the blobs without one. Needs a
design document first.

---


## Correctness

### WRK-052: Same-way fallback for unresolved refs in apply-changes -l

On a base without its untagged nodes, an OSC-modified way that keeps a ref
from its old version could take that ref's coordinate from the old version's
inline value, which sits in the blob the worker is already rewriting. That
would resolve many of the refs that make `-l` fail on such a base (about
2.8M on a europe daily diff), but not refs a way newly adds, and it carries
forward any coordinate an earlier run left stale. It needs its own
resolution and accounting (the global `loc_missing` would still count those
IDs). Not decided whether it is wanted at all, given the docs require
`--keep-untagged-nodes`. Background: the stale inline coordinates defect is
fixed (`81f3fbe4`), with `check --locations` (`669a31e6`) as its oracle.

### WRK-053: Ordered pool internal panic lingers behind parked waits

Residual of the altw stage 4 worker-panic fix (`64e478f8`). Stage 4 workers
and their setup hold an `AbortOnDrop` guard, so a panic in caller-supplied
worker code aborts the coord-payload router at once. A panic inside the
ordered pool's own machinery (admission, the result send) is outside those
guards: the gate closes, but a worker parked in `wait_ready` for a way blob
slot stays until stage 3 publishes it or observes cancellation. That would
be a pool bug, not caller code; the Panics section of `src/ordered_pool.rs`
states the general behaviour. The pool-level alternative (a task panic as an
ordered failure that runs `abandon`) was rejected because `abandon` would
still wait on earlier tasks parked on the same router. Not decided whether
anything beyond the documentation is wanted.

---

## Documentation and comment corrections

### WRK-063: Resident bytes in the writer reorder buffer

`writer_reorder_high_water` (seq span) and `writer_reorder_filled_high_water`
(occupied slots) are documented and recorded, but no counter gives the bytes
held there, so memory attribution from them stays inferred. A byte peak needs
per-chunk accounting across `Raw` / `RawChunks` (allocated
capacity, not length), `CopyRange` (no bytes) and error slots, added on push
and subtracted on pop in `deliver_in_order`; it is per blob on every
pipelined writer, so it needs its own matched A/B.

---

## Tooling

### WRK-071: inspect rows stored under other command names

`brokkr results --command inspect` misses the denmark `fileinfo --extended`
row `e4f4508c` (`23862d1`), stored under command `fileinfo`. The hotpath
rows stored as `inspect` are tags-count runs, so no profile of the
full-decode path exists.

### WRK-072: Allowlist gaps met during measurement

`sudo fstrim -av` (AGENTS.md asks for it before a matched suite) and
`taskset` (to run bygg at a README-like worker count) are not allowlisted,
so implementers skipped both. On 2026-10-31 `! sudo fstrim -av` ran on
bygg without a password prompt, though cached sudo credentials may explain
that; if not, an exact `Bash(sudo fstrim -av)` allowlist entry is all
agents would need. The maintainer's call.

### WRK-073: Digest v2 cost

brokkr#61 landed `brokkr digest` (brokkr `cb83a5f`). Its single-sample
denmark indexed cells put digest at 503 ms on `d2d8836a` against 701 ms on
`4157567d`, about 39 % slower, after the digest `v2` change (`16154b0d`).
The ticket suggests a `--form content` run to separate drift from code.
The recorded "no cost" claim for v2 needs re-checking against that.

---

## The measurement campaign

Matched A/B cells planned at the end of the hygiene drain to bisect what the
drain changed. Today's regression fixes did not need them, but they remain
the way to find any regression the RAM pass could not see. "A vs B" means
matched, interleaved `--bench 1` cells with `--commit`; when a pair moves,
bisect inside it. Prefer stored cells where they already answer the
question.

### WRK-082: The validated-invariant trio

`brokkr read --dataset planet` plus one tag-heavy re-encoding command
(`tags-filter` or `inspect --tags`), `5a9d40e4` vs `85c1428c`. Covers
`e28fdf69` (`take_message` slot check per element parse, elements parsed
twice), `9c85d37e` (a cold panic call per element and two per dense tag
varint, through a generic `#[inline]` `#[track_caller]` helper) and
`85c1428c` (a `Result` per kept element on every re-encoding command, a push
loop instead of `extend` for roles). Bisect only above 3 %.

### WRK-083: The partial-index guard

`brokkr read --dataset planet`, `84f50b31^` vs `84f50b31`. An armed frame now
parses indexdata (a 42-byte copy and a deserialize) on reads that skipped it
(the pipelined reader without a blob filter, `par_map_reduce`) and compares
once per data frame everywhere.

### WRK-084: Ordered pools

`56a50e0d` added windows where there were none (`default_window`, 8 per
worker, floor 32, never tuned), claim workers instead of a dispatcher,
`WILLNEED` advice moved from the dispatcher to the consumer (one
`posix_fadvise` per consumed item on the serial writer thread, further
ahead than before; it can enter reclaim at planet scale), failures in
schedule order, passthrough prefixes copied between decode results. Risks
named: `OrderedGate::advance` takes the mutex and `notify_all`s on every
consumed item (a wake syscall even with no waiters, about 1 us per blob; a
thundering herd when the window is full, stage 4's steady state); stage 1
workers parking behind a slow head for no memory gain; with
`--keep-untagged-nodes`, stage 4 copying the node prefix only after the
first way result. Pair `60e1b691` against `c2409c45` (the shape that ships;
`56a50e0d` itself carries the window-deep channel `c2409c45` removed) and
bisect inside if it moves. Phase split first, in this order: altw external
on planet at zlib:6 (stage 1 and stage 4 walls, peak anon, `s4_*`);
`extract --strategy smart` on planet with the Europe bbox; `multi-extract
--regions 5` on Japan and Europe (its region writer gained `WILLNEED` here,
measured a win on europe only, ruled kept); `repack` on europe or planet;
tags-filter two-pass and getparents on planet; time-filter on planet.

### WRK-085: extract simple on the unordered driver

`8928de18` moved extract simple's two pools onto the unordered driver and
gave them `WILLNEED` (ruled kept, to be measured): `extract --simple` on
planet and europe, `8928de18^` vs `8928de18`.

### WRK-086: cat --dedupe on Denmark and Japan across the engine move

`79d2e923^` vs `79d2e923`, then HEAD. Pass 1 moved to `HeaderWalker` preads,
passthrough to coalesced `copy_file_range`, overlap runs to dedicated sweep
threads; `13ec0927` has since replaced the sweep itself.

### WRK-087: sort on planet across the engine move

`sort --dataset planet`, `79d2e923^` vs `79d2e923`. Expected unchanged: no
runs, no threads, one coalesced copy.

### WRK-088: sort on the Denmark unsorted snapshot

`79d2e923^` vs `79d2e923` on snapshot `unsorted` (or `test-unsorted`, the
same file). Sort's recorded trade from the pass-1 move: a planet win, a
dense-europe pass-1 regression.

### WRK-089: sort --force on unindexed input

`sort --force` on an unindexed europe or planet input (`pbf.raw`),
`060c2f74^` vs `060c2f74`: the `SORT_INDEX_BUILD` wall and peak RSS. Every
unindexed blob now pays the combined ID-and-tag scan in serial pass 1, and
its serialized key set is held to pass 2 (up to GBs at 8k-encoding blob
counts). `plan` adds four per-frame `Vec<bool>`s and about 60 B per blob.

### WRK-091: diff-snapshots across the block-pair engine

Planet `diff-snapshots --from base --to 20260411`, text and osc,
`2c12a955^` vs `2c12a955`, expected unchanged. Recorded sequential planet
figures, which cannot be re-measured: text 2134 s (`a9d430f2`), osc
2225.6 s, peak anon about 28-41 MB per phase; header walk about 15 s cold.

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
blob. Then repeat at HEAD, which has the per-kind rows back
(approximately). Baseline `f2315551`: 333.8 s, 750 MB peak anon.

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
  io_uring item in `notes/todo.md`); the writer tests skip otherwise.
- **Filing in brokkr's bz from here:** the Bash hook rejects a command that
  names brokkr together with a redirect, a pipe or backticks, so pass the
  body with `-b '...'` in single quotes and no backticks.
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
  a default metadata block, fewer nodes: refuse instead. For a broken
  internal invariant, a release-mode panic over a `debug_assert!`.
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
  mmapped chunk raises the process-wide `mmap_threshold` to its size (32 MiB
  cap) and `trim_threshold` to twice that, which is why buffers that stop
  being freed can push later allocations onto mmap (time-filter paid 29 s of
  kernel time for it before `5bfa37ab` pinned the threshold, but only under
  the two-arena cap). Read the source rather than reasoning from memory.
- **Two malloc arenas are two locks** (2026-11-01): `M_ARENA_MAX=2` makes
  every allocation above the tcache limit from every pool thread take one
  of two arena mutexes, and an arena-backed `realloc` that moves copies
  under it. Every measured re-encoding command paid 10 to 82 % more planet
  wall for it, apply-changes 20x; the memory it was added for had already
  gone with the decode-side string tables.
  The CLI sets no allocator policy; measure before adding any.
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
