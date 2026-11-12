# Diff

## Diff between independent snapshots

The pbfhogg CLI command is `diff` (single command). Its performance
shape depends on the inputs: two PBFs with byte-level blob overlap
(e.g. one derived from the other via `apply-changes`) trigger a
byte-equal fast-path that skips decode for unchanged blobs; two
independent snapshots (e.g. Geofabrik planets weeks apart) have no
byte-level overlap and require full decode on both sides. This
section captures the full-decode scenario.

Brokkr distinguishes the two input shapes via `brokkr diff` (which
applies an OSC to the base first so the fast-path engages) vs
`brokkr diff-snapshots` (which feeds two independent PBFs). Same
pbfhogg CLI underneath; different measurement.

### Planet (87 GB input, 93 GB output snapshot, 47-day apart)

`from=base` is the 2026-02-23 planet; `to=20260411` is the
corresponding April-11 snapshot registered under the planet dataset's
snapshot key.

Shard-based block-pair merge (`-j/--jobs N`, commit `06628d8`
2026-04-20, `--bench 1`; sharding was opt-in via explicit `-j N` at
measurement time, and became the default with `-j` defaulting to `0`
in 0.5.0 - see CHANGELOG.md). Both text and `--format osc` paths
parallelise over the same ID-range shard plan.

| UUID | Args | Wall | Peak anon RSS | vs sequential |
|---|---|---:|---:|---:|
| `22a5eb55` | `-j 16` (text, temp-file shape) | **227.5 s (3m48s)** | 586 MB | **9.5×** |
| `cdcaa4f1` | `-j 16 --format osc` (commit `16e3694`, 2026-04-26) | **293.8 s (4m54s)** | n/a | **7.6×** |

The "vs sequential" ratios divide the wall by the old single-threaded
streaming engine's planet wall, which the tree no longer contains (one
block-pair engine now serves every shard count, so the ratios cannot be
re-measured at a later commit): 2134 s for text (recorded at commit
`a9d430f2`) and 2225.6 s for `--format osc` (the same engine's recorded
planet run; its commit is not named in the tree). Dividing these by the
walls above gives 9.4 and 7.6, so the text ratio's printed 9.5 is
approximate.

Both paths stream shard output to per-shard scratch temp files
(`BufWriter<File>`) and concatenate in shard order; peak anon is just
in-flight decoded blocks. OSC's lower speedup is the serial
`assemble_osc` gzip + concat of ~45 GB of XML fragments (32.8 s,
~10 % of wall).

Phase split (planet, both paths): NODE ~73 %, WAY ~26 %, REL rounding
error. Walker phase ~15 s (pread-only via `HeaderWalker` with
`posix_fadvise(RANDOM)`; disk read 45 GB → 2.6 GB). Shard balance on
germany within 1.03× max/min. Avg cores on planet text `-j 16`: NODE
14.7, WAY 12.9, REL 14.5 out of 16 (86-92 %).

### Unindexed `.osc.gz` assembly (bygg, 2026-11-04, at `80e0b8f0`)

The one path that still assembles through `ParallelGzipWriter`: an
`.osc.gz` from inputs without indexdata, so the comparison is the
sequential element stream and the gzip pool runs on every host thread
(32 here) after it. `diff-snapshots --dataset germany --from base --to
20260420 --variant raw --format osc --bench 1`, `217680f2`, 104.3 s:

| Span | Duration | Peak anon | Avg cores |
|---|---:|---:|---:|
| node, way, relation scan | 103.8 s | 14 MB | 1.0 |
| assembly (`DERIVECHANGES_WRITE`) | 0.36 s | 71 MB | 12.5 |

About 1.8 GB of fragment XML in, 362 MB written. The pool never waited
for a sequence permit (`writer_parallel_gzip_reorder_blocked` 0,
reorder high water 9 slots), and its 12.5 cores of 32 put the limit on
the fragment copy feeding it, so this run used little of the writer's
envelope (about 292 MiB of payload capacity at 32 workers; the bound
and what it assumes are at `ParallelGzipWriter::new`). That envelope
is reached only under backpressure (slow output, a straggling early
chunk, a pool slower than its producer) and grows with host threads.
The assembly is nonetheless this command's peak: the scan's readers
and decode pool are dropped before it, so it does not stack on the
scan's working set. 0.36 s is three 100 ms samples, so 71 MB is a
sampled peak, not the instantaneous one.

### Unindexed text diff across its three reshapes (bygg, 2026-11-09)

Script bench `diff-unindexed` (`benches/diff/diff-unindexed.lua`):
`diff -c` of germany `pbf.raw` (base, seq 4704) against germany
snapshot `20260420` `pbf.raw`, both without indexdata, so every arm
takes the sequential element stream. Inputs evicted and fincore-checked
at zero before each run (the 9.4 GB pair does not stay cached under the
bench's memory limit, so warm cannot be held). One iteration per
invocation, both arms `--commit` worktree builds, A B B A per pair;
walls are the diff child's own. Every arm of a pair wrote the same
output bytes and the same summary: type all 150641304 bytes, 7952543
differences, 495644074 common; `--type way` 31301484 bytes, 1601349
differences.

| Pair | Type | Before (s) | After (s) | Change |
|---|---|---|---|---:|
| `8881415e^` vs `8881415e` | all | 152.9, 151.5 | 156.5, 161.9 | +5 % |
| `8881415e^` vs `8881415e` | way | 153.6, 162.0 | 102.6, 103.5 | -34 % |
| `94743e65^` vs `94743e65` | all | 151.3, 151.7 | 97.4, 100.0 | -35 % |
| `126cbce2^` vs `126cbce2` | all | 98.5, 100.3 | 96.6, 96.6 | -3 % |

Uuids in run order: `fb596e36` `96fe93aa` `0eb42526` `6a9853f6`;
`214b4ee4` `f050f528` `ea2a5a3a` `2ae91082`; `5f2e009e` `e9d40917`
`72230dd3` `d8eecbd8`; `e15b137d` `4d441144` `992273d3` `af661a0e`.

- `8881415e` (per-element order check, blocks classified by their
  elements, excluded kinds dropped instead of converted and merged):
  slightly slower on the full diff, as expected; under `--type way` it
  saves a third, the node and relation drains no longer converting
  elements. The saving lies outside the `DIFF_PHASE_WAY` span, so judge
  `--type` arms on the whole wall.
- `94743e65` (borrowed elements instead of owned conversion): a third
  off the full diff, and peak anon 102 MB -> 21 MB (`5f2e009e` vs
  `e9d40917`); the two sides' block peaks coinciding costs nothing
  visible.
- `126cbce2` (one `DecompressPool` per merge): within the noise floor,
  peak anon 21 MB -> 24 MB (sampled); `5bfa37ab` later changed the
  pool's retention, so this is the change as landed, not as it is now.

Since bench version 3, `diff-unindexed` probes each input's first data
blob with `inspect --indexed` outside the measured diff and refuses an
input whose front carries indexdata, so a stored cell has taken the
sequential path. The probe does not prove that no later blob is indexed.
The cells above predate the probe; their inputs were `pbf.raw` throughout.

## Common lines cut by elements (bygg, 2026-11-09, against `71976fd7`)

`pbfhogg diff OLD NEW` without `-c` on Denmark indexed against its own
`cat --dedupe` output: equal elements in different framing, so no blob
pair is byte-equal and every one of 59,152,282 elements prints a common
line. The diff child of `dedupe-self --cache warm --check yes` cells, one
run each, input warm, 16 workers:

| Cell | Build | diff child | send wait | head wait |
|---|---|---|---|---|
| `89f2ce1e` | `79d2e923`, sharded engine, text through scratch (`--commit`) | 1.29 s | - | - |
| `f6d7708e` | `4e0eaa50`, work units (`--commit`) | 8.22 s | 128.8 s | 7.86 s |
| `2e89d66e` | `71976fd7` plus every frame counted (main tree, dirty) | 0.65 s | 0 | 0.43 s |

"send wait" is `ordered_pool_result_send_wait_ns`, "head wait"
`ordered_pool_result_recv_wait_ns`. At `4e0eaa50` a 32 MiB unit made about
30 MB of common lines against a channel of four 1 MiB chunks, so every
unit behind the head parked and the run formatted on one core. The
single-sided element cut (`DIFF_UNIT_LONE_ELEMENTS`) never fired, since
every frame overlaps the other input. Counting every taken frame's
claimed elements when common lines print cuts units of about 131k
elements, whose text fits the channel. The diff source is identical
between `4e0eaa50` and `71976fd7`; the variant arm is a main-tree build
against worktree builds (brokkr#105 measured worktree builds up to 18 %
slower in one CPU-bound phase), which does not touch a 12x difference.
No standard planet cell exercises the change: brokkr's `diff` and
`diff-snapshots` text cells pass `--suppress-common`, and `--format osc`
plans as before, so their units are unchanged.

The planet `diff-snapshots` A/B of `2c12a955^` against `2c12a955` was not
run: both arms are the sharded engine `587bfc12` replaced, and no code
decision rides on it. The stored `brokkr diff` pair at those two commits
(base against its apply-changes output, above) is a different workload
and is not a substitute for it.

## Text units hold pieces past a full channel (bygg, 2026-11-09, against `5432aac7`)

The text diff's sink now hands output over in exact `DIFF_OUTPUT_CHUNK_BYTES`
pieces and, once a unit's channel is full, holds further pieces in memory
against `DIFF_TEXT_OVERFLOW_BYTES` instead of blocking. `diff-text` script
bench (`benches/diff/diff-text.lua`), inputs warm, one iteration per cell,
diff child's wall; base arm a `--commit HEAD` worktree, variant the dirty main
tree:

| Input | Cell | Build | diff child | send wait | overflow chunks |
|---|---|---|---|---|---|
| denmark base vs `320k` repack, no `-c` | `66c7ae25` | `5432aac7` | 2.82 s | 0 | - |
| | `edea221b` | variant | 2.81 s | 0 | 0 |
| germany base vs `20260420`, `-c -v` | `dde2ffe5` | `5432aac7` | 5.51 s | 63 ms | - |
| | `bd12c886` | variant | 5.54 s | 3.6 ms | 0 |

No cost, and no gain: neither real input outruns a unit's channel, so the
allowance is never used. The denmark pair was meant as an oversized-frame
input and is not one: a 320k-element frame on one side is carried and
clipped at the other side's cuts, so units stay at the element cut's size
(493 node units); that run is limited by the planner and head (job lock wait
17 s, head recv wait 2.4 s summed), not by send waits. Two months of
germany changes under `-v` (3.46M modified, 440 MB of text) waited 63 ms
in sends at the base; the variant's exact pieces cut that to 3.6 ms. The
shape the mechanism exists for (many modified elements with long details, or
both inputs framed past any cut) is in no dataset, so its gain is unmeasured.

## Verbose sequence work budget (bygg, 2026-11-12, at `3b0e93f8`)

`diff-verbose-budget` script bench (`benches/diff/diff-verbose-budget.lua`):
`--ways` modified ways of each of two 30,000-ref shapes, disjoint (out of
budget, one replacement line) and every 60th ref changed (1000 edits,
itemized), built untimed; result is the `diff -v -c -j 1` child's wall,
best of the iterations. Recorded on a tree whose only change was the bench
itself, so the binary is `3b0e93f8`'s.

| `--ways` | Searches | Cell | diff child |
|---|---|---|---|
| 1 | 2 | `0a8e3a23` | 6 ms |
| 8 | 16 | `6386804f` | 27 ms |
| 12 | 24 | `4abb2fde` | 37 ms |

The slope is about 1.4 ms a modified way, sequence search plus decoding the
refs and formatting, against the 1.18 ms (disjoint) and 1.43 ms (scattered)
medians the removed in-process unit timing gave on 2026-11-10. The budget
keeps a worst-case way in low milliseconds; `VERBOSE_SEQUENCE_DIFF_MAX_WORK`
stays at `1 << 20`.

## Diff of a base against its apply-changes output (bygg, 2026-10-31)

`brokkr diff`: the base against the base with one daily OSC applied, run
as `pbfhogg diff OLD NEW --suppress-common --output FILE` (so the
byte-equal skip may fire), `--keep-cache`, `--bench 1` cells interleaved,
no `fstrim` (not allowlisted). All rows exit 1 (the inputs differ).

### Planet, default `-j` (one shard per core), OSC 4913

| UUID | Commit | Wall | DIFF_SCAN (header walk) | Node phase peak anon |
|---|---|---:|---:|---:|
| `789b28b4` | `79d2e923` | 401.8 s | 82.8 s | 937 MB |
| `07380793` | `2c12a955` | 397.2 s | 87.5 s | 692 MB |
| `7f085114` | `79d2e923` | 412.1 s | 91.0 s | 933 MB |
| `a43d9f58` | `2c12a955` | 403.6 s | 92.7 s | 690 MB |
| `e95703aa` | `f3e1c9b9` | 395.4 s | 95.1 s | 653 MB |

`2c12a955` gave the sharded default path the byte-equal skip it lacked.
On this workload it is worth -1.1 % and -2.1 % in the two pairs (medians
407.0 s against 400.4 s, below noise) and about a quarter of the peak
anon: the skip fires on 7,145 blob pairs (2.1 G elements), while 43,669
overlapping pairs (9.9 G elements) still decode, because one day of
edits touches most of planet's large blobs. The Japan figure that
motivated the skip (`diff -c` 50.0 s to 6.8 s, plantasjen, April) came
from the sequential engine on a Geofabrik-density file and does not carry
to planet. `diff -q` runs the same engine with the same skip and was not
measured separately (brokkr's `diff` never passes `-q`).

The serial header walk was a quarter of the wall: about 1.28 M `pread`s
and 10 GB read at 0.0 cores. Walking the two files on two threads was
tried (uncommitted, `f3e1c9b9` plus the change) and measured 98.8 s, no
gain, so it was not kept. The "new" input brokkr builds with
apply-changes is 174 GB, not 92 GB: the two inputs total 266 GB, so the
merge phases' 258 GB read was no over-read.

### Planet, streaming work units (bygg, 2026-10-31)

`4dbc2d47` (the engine landed in `587bfc12`) reads both inputs once as
sequential streams cut into work units on the ordered pool; no header
walk, and the text goes to the output in order. Matched `--bench 1`
cells, same OSC 4913 setup:

| UUID | Commit | Wall | Peak anon |
|---|---|---:|---:|
| `05d47098` | `4dbc2d47` | 284.1 s | 1.81 GB |
| `ec05b5d5` | `9afb9449` (header walk, shards) | 384.1 s | |
| `b7e58fb6` | `4dbc2d47` | 284.6 s | |

-26 %. The node phase runs at 29.6 cores with 248 GB read from the
266 GB of input, each byte once. Peak anon rose from about 0.65 GB: it
is the unit window, `workers + DIFF_UNIT_WINDOW_SLACK` units of
`DIFF_UNIT_TARGET_BYTES` frames plus the planner queue and decoded
blocks. An uncommitted 16 MiB target measured 295.4 s at 1.18 GB peak
anon, about 4 % wall for a third of the memory; 32 MiB was kept.

`diff-snapshots` (base against `20260411`, two independently cut
planets) shares the engine:

| UUID | Commit | Format | Wall |
|---|---|---|---:|
| `aaf851c9` | `4dbc2d47` | text | 116.5 s |
| `ca13a5b8` | `9afb9449` | text | 129.9 s |
| `9dc54efe` | `4dbc2d47` | osc | 210.5 s |
| `11143d70` | `9afb9449` | osc | 215.7 s |

Text -10 %; osc within noise.

### Derive units deflate and spill their own output (bygg, 2026-10-31)

`diff-snapshots --from base --to 20260411 --format osc -j 16`, matched
interleaved `--bench 1` cells, base `179f1b7e` (stored) against the
same tree plus worker-side gzip members and per-unit spill (unstored,
`--force`, landed in the commit following `179f1b7e`):

| Cell | Commit | Wall | Peak anon |
|---|---|---:|---:|
| `3660071f` | `179f1b7e` | 240.7 s | 5.00 GB |
| (unstored) | variant | 156.3 s | 1.25 GB |
| `955da7f4` | `179f1b7e` | 241.3 s | |
| (unstored) | variant | 156.6 s | |

-35 %, sign-consistent across both pairs. The base's phase split
(`c55bebc9`, same commit) had the merge at 15.6 cores for 101 s, then
37 s, 14 s and 15 s of node, way and relation tails at 1.1-1.4 user
cores and 0.1 kernel: not the calling thread's scratch writes but
head-of-line formatting. A unit the consumer was not reading could run
only `DIFF_OUTPUT_CHUNK_DEPTH` chunks ahead, and the 440 new-only node
blobs (109 M elements, about 34 MB of XML each) made each tail unit's
output hundreds of MB, so only the head unit's worker progressed. The
same blobs made each chunk a whole blob's XML, which was the 5.0 GB
spike as the tail began. In the variant those tails run at 12.2, 7.4
and 13.0 cores in 5.9 s, 4.1 s and 1.4 s, and the assembly is a 1.6 s
copy of compressed fragments instead of 8.2 s deflating 29 GB. The text
form, which shares the engine but neither change, is neutral: `c12221f5`
(`179f1b7e`) 146.3 s against 146.4 s unstored.

### Single-sided stretches cut by claimed elements (bygg, 2026-10-31)

`diff-snapshots --from base --to 20260411 -j 16`, matched interleaved
`--bench 1` cells in A B B A order, base `c7eed182` (stored, built by
`--commit`) against the same tree plus a unit cut once its single-sided
blobs claim `DIFF_UNIT_LONE_ELEMENTS` elements (unstored, `--force`,
landed in the commit following `c7eed182`):

| Form | Cell | Arm | Wall | Peak anon | Node tail | Way tail |
|---|---|---|---:|---:|---:|---:|
| text | `258cbdc4` | base | 145.6 s | 1.57 GB | 4.55 s | 2.13 s |
| text | (unstored) | variant | 141.8 s | 1.30 GB | 0.25 s | 0.25 s |
| text | (unstored) | variant | 142.3 s | 1.30 GB | | |
| text | `ed12c024` | base | 146.5 s | | | |
| osc | `6d806707` | base | 156.8 s | | | |
| osc | (unstored) | variant | 156.0 s | | | |
| osc | (unstored) | variant | 156.2 s | 1.26 GB | | |
| osc | `4b6f07ee` | base | 157.0 s | 1.25 GB | | |

Tails are the `--durations` overlap of a kind's END past the next
kind's START. The text base's anon sat at a 1.1 GB plateau (unit frames
and decoded blocks) through the node merge, then jumped by 0.43 GB at
the moment the tail began and drained linearly over 4.5 s at about
3 user cores: each 32 MiB tail unit held about 18 new-only planet blobs
(318k nodes each) of text, about 80 MB, against a channel of four
blob-sized chunks, so every unit behind the head parked full and the
head formatted alone. One-blob tail units fit their channel whole, so
the window formats them in parallel. Text -2.8 % wall and -0.27 GB
peak, sign-consistent across both pairs; osc, whose spill already
removed the shape, is neutral.

### Europe, `-j 1`, OSC 4715

| UUID | Commit | Wall | DIFF_SCAN | Peak anon |
|---|---|---:|---:|---:|
| `cae9f615` | `79d2e923` (streaming sequential engine) | 193.0 s | none | 14.6 MB |
| `ea3d4f41` | `2c12a955` (header walk, then one pread shard) | 270.4 s | 79.3 s | 106 MB |
| `9503a2ce` | `f3e1c9b9` | 255.3 s | 79.3 s | 96.9 MB |
| (unstored) | `f3e1c9b9` plus the `-j 1` stream, two cells | 177.1 s, 175.6 s | none | 18.0 MB |

`2c12a955` cost `-j 1` 40 %, all of it the up-front walk of both files:
the merge phases matched the streaming engine to within a second. One
shard now reads each input as one sequential stream, checking each
header as it arrives, which removes the walk and brings back
`--direct-io` and page-cache eviction at `-j 1`: -31 % against
`f3e1c9b9`. The unstored cells also carry `f3e1c9b9`'s cheaper block
construction, so they compare with `79d2e923` only approximately.

