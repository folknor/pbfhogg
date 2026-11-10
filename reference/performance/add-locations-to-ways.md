# Add Locations To Ways

### Claim checks in add-locations-to-ways and time-filter (bygg, 2026-10-27, commits `5587582a` to `2d4e3404`)

Slice 3 holds every blob add-locations-to-ways decodes to its claim and
time-filter to the sort claim. All cells `--bench 1`, interleaved against
the parent `d0aea1d2` (built through `--commit`), bygg, untrimmed. Worker
counters are summed worker elapsed seconds, not CPU; span CPU is user
core-seconds summed over every sidecar segment of the span
(`scripts/sidecar_span_totals.py`; `scripts/altw_cells.py` tabulates the
counters).

**External, planet: the first commits cost CPU, removed before the final
one.** On `5587582a` stage 2's node extraction, where the shared
`ClaimWalk` ran per tuple, took 101.8 and 102.4 worker-seconds against
85.9 and 86.4 (`1bac7795`, `98b44690`, `2fc0c5da`, `e1b62c8a`), and on
`a4ffa7b3` stage 4's node re-encode, the same walk per node, took 720.8 and
727.3 against 608.2 and 633.7 (`7cbfa307`, `d13ba24d`, `7be2bd68`,
`a228bdb2`), the same sign in every pair. Both now use `AscendingTally`
(one raw compare, a store and a count per ID, decisive because external's
node claims are non-negative).

**Final code, planet external, settling sequence.** Earlier final-code
cells were confounded: a parent cell came first after each idle gap (a
rebuild, the test suite), and pass A, which writes 147 GB of id shards,
was 100 to 110 s on those cells and 150 to 200 s on cells run back to
back, with the same bytes written and less CPU. The sequence below was
agreed to settle it: one conditioning run (`56d0a576`, parent), then
parent and change alternating back to back with builds done beforehand.

| Cell, run order | Wall (s) | Pass A (s) | Stage 2 (s) | Streaming (s) | Streaming user core-s | Stage 2 extract (worker-s) | Stage 4 node re-encode (worker-s) |
|---|---|---|---|---|---|---|---|
| `43a495ea` parent | 571.8 | 173.3 | 206.6 | 183.6 | 2630.0 | 86.5 | 626.5 |
| `439cc375` change | 581.7 | 174.6 | 206.4 | 192.6 | 2636.6 | 89.1 | 643.1 |
| `20efc913` parent | 579.0 | 170.9 | 213.8 | 186.1 | 2717.1 | 86.2 | 634.9 |
| `56736441` change | 622.7 | 180.6 | 223.0 | 211.0 | 2656.3 | 89.0 | 629.4 |
| `cc799657` parent | 594.3 | 186.5 | 214.3 | 185.3 | 2626.2 | 86.3 | 618.9 |
| `50823a3d` change | 564.9 | 168.1 | 206.6 | 182.1 | 2749.2 | 91.6 | 626.5 |

Wall medians 579.0 (parent) against 581.7 s (change), +0.5 %, inside the
noise. Pass A, stage 2 and streaming medians move by +0.8 %, -3.4 % and
+3.9 %, and their user CPU by -2 %, +0.3 % and +0.9 %. The one cost the
data resolves is stage 2's node extraction, 86.3 to 89.1 worker-seconds
(+3.2 %, every change cell above every parent), about half a second of
wall at its 6 workers. Stage 4's node re-encode moves +0.5 % on the
medians, unresolved. The slow change cell `56736441` is slow in streaming
(211.0 s against 186.1 s for the parent before it) with 2.2 % less user
CPU, the same bytes read and written, and read and write throughput about
11 % lower; stage 3 reads, the coordinate-payload preads and stage 4's
preads rise 13 to 20 %: an I/O slowdown, not check CPU. Not explained:
stage 1's way scan, which now runs a `ClaimWalk` per way, is lower on the
change in every cell (median 58.2 against 50.9 worker-seconds).

**Sparse, north-america** (auto routes it to sparse here: store estimate
18.98 GB against an 80 % share of a 27.46 GB budget). Run order parent,
change, change, parent: walls 94.9 (cold page cache: its auto estimate walk
took 33 s), 61.1, 53.7 and 67.9 s (`9c85e66a`, `9943a40e`, `ebab1c11`,
`23b988fc`). Pass 1, which now tallies every node tuple, used 75.5, 75.2,
78.3 and 73.8 user core-seconds and took 10.7, 9.6, 9.6 and 10.8 s: CPU
level, and a wall 1.1 s lower on both change cells that the figures do not
explain (its kernel time is 1 to 3 core-seconds lower). Pass 2 used 501,
498, 495 and 486 user core-seconds; its walls (42.3, 41.9, 36.6, 49.8 s)
follow the store's page-cache residency, not CPU.

**time-filter, planet** (snapshot path, one key compare per element):
175.9, 174.9, 178.2 and 175.4 s, run order parent, change, change, parent
(`1cadc624`, `9bcc192d`, `13aaad48`, `176d004e`), medians +0.5 %.

**diff `-c`**, which now counts single-sided runs from the decoded block:
planet did not fit brokkr's stdout capture when these cells ran, so germany with OSC
4705 stands in: 3.2, 3.1, 3.1 and 3.1 s (`d0e29e64`, `bd8e6362`,
`f4204986`, `9bcdff49`).

## Add-locations-to-ways

The Europe and Planet figures directly below are historical: commit
`69a127f` ran the dense mmap index (16-byte slots, 128 GB of virtual
address space, only touched slots resident), which has since been
removed. The index types that exist now are compared in "Dense vs Sparse
vs External index" and the sections after it.

Commit `69a127f`, plantasjen (30 GB RAM, 8 GB swap).

### Europe (33.6 GB indexed, 4.2B elements)

3.7B nodes read, 149M written, 3.57B dropped. 453M ways, 8.2M relations.
1029 passthrough blobs, 521K decoded. 0 missing locations.

| I/O Mode | Time |
|----------|------|
| Buffered | **2565s** (42m45s) |
| `--direct-io` | 2611s (+2%) |

### Planet (87.7 GB indexed, 11.6B elements)

10.4B nodes read, 285M written, 10.2B dropped. 1.17B ways, 14.1M relations.
452 passthrough blobs, 50K decoded. 0 missing locations.
Output: 88.4 GB (+0.7% from embedded way-node coordinates).

| I/O Mode | Time |
|----------|------|
| Buffered | **5773s** (96m) |

Planet on 30 GB host with 8 GB swap - memory-latency-bound (page faults on
sparse mmap index), not compute-bound. Production host (64 GB RAM) should be
well under an hour.

`--direct-io` provides no benefit for ALTW - workload is compute/memory-bound,
not I/O-bound. Sequential I/O benefits from page cache prefetch.

### Dense vs Sparse vs External index (plantasjen, historical)

The `dense` index type has since been removed (`b70dd8c`): the rank-indexed
flat sparse index beat it at every measured scale and also works where dense
could not. `--index-type dense` now errors. The Dense column below is the
historical measurement from when it existed, kept as a record; the Sparse
column is the pre-rank-flat sparse, so those numbers are also superseded.
Current sparse and external figures are in the sections that follow.

| Dataset | Dense | Sparse | External | Commit |
|---------|-------|--------|----------|--------|
| Denmark (465 MB) | **6.8s** | 14.1s | 14s | `ee9b19f` |
| Japan (2.4 GB) | **42s** | - | - | `b3e8bf7` (node scanner) |
| Europe (33.6 GB) | 2,940s (49m)* | 6,453s (107m) | **400s (6.7 min)** | `3d977a0` |
| Planet (87.7 GB) | 5,773s (96m)* | - | **953s (15.9 min)**, 8.7 GB peak anon | `3d977a0` |

*Dense at Europe scale thrashes on 30 GB host (mmap working set ~16 GB > available
RAM). Japan 42s is with node-only scanner for pass 1 (commit `b3e8bf7`, previously
72s with pipelined PrimitiveBlock). Europe 2,940s is also with node scanner but
mmap thrashing dominates.

*Planet with dense thrashes on 30 GB host (memory-latency-bound).

Dense was fastest when the working set fit in RAM. External uses ~1.6 GB
anon RSS at Europe scale via 4-stage radix join pipeline (node-only wire
scanner for stage 2, scatter buffer for stage 3, sequential reader for
stage 4).

External's anon figures exclude the per-way ref-count sidecar, which
stages 3 and 4 read through a read-only mmap of the scratch file. Its
pages are file-backed and reclaimable: they show in RSS as file pages,
not in peak anon, and under memory pressure they are dropped and
re-read rather than swapped. The anonymous part of that index is one
byte offset per way blob. Before the mmap the whole encoded sidecar
was read into heap and counted in anon, so anon peaks recorded before
the change and after it are not directly comparable on this term.
The blob-order slot-start prefix vector (one `u64` per way blob) is
still anonymous.

Current Europe external baseline on `main` (post-A1 commit `0dc8ae1`):
**270.8 s** (`4m31s`, UUID `0b89f986`, `--bench 1`). A1 (rankless
node-ID bucketed stage 1+2, landed 2026-04-25) cut europe from
291.6 s at `6d71053` (-7.1 %) by replacing the two-pass way scan +
rank index with single-pass IdRecord emission and a streaming
merge-walk.

**Historical crossover point** (dense no longer exists): between Japan
(2.4 GB, dense 2x faster) and Europe (33.6 GB, external 7.4x faster). At
Europe scale, dense's mmap working set (~16 GB) exceeded available RAM,
causing thrashing. External's sequential I/O stays bounded.

### Sparse `MADV_RANDOM` on the coord store (europe, plantasjen, 2026-07-16, commit `25cb0df`)

Pass 2's random lookups no longer pay kernel readahead speculation: the
store mapping is advised `MADV_RANDOM` once pass 1 has populated it.
Adjudicated on the matched quiet pair (cells adjacent, same day,
post-trim). Four further same-day cells (`53763e55`, `7027481c`,
`9a72b914`, `6b9942e5`) ran under host load and adjudicate nothing
arm-vs-arm on wall, though every probe cell, the memory-pressured one
included, read fewer pass-2 bytes than the best baseline cell:

| | probe `3027542f` (`25cb0df`) | baseline `c45b45ee` (`13910d2`) | delta |
|---|---:|---:|---:|
| pass 2 minus schedule walk | 231.2 s | 260.8 s | **-11.3 %** |
| pass-2 disk read | 260 GB | 600 GB | **-57 %** |
| pass-2 KB per major fault | ~4.8 | ~43 | mechanism confirmed |
| total wall | **359 s** | 384 s | -6.5 % |

**359 s (`3027542f`) is the current europe sparse reference** (quiet
host, post-trim, `--bench 1`), superseding 363.1 s (`4ac11326`) as
bookkeeping only - the cells are not comparable. Caveat carried from
the adjudication: under severe external memory pressure the
no-readahead trade multiplies synchronous fault round-trips (104 M vs
54 M faults for identical work in the contaminated cell) and the wall
win can invert; inputs whose store fits comfortably in RAM are
unaffected either way.

### Auto routing threshold: sparse store versus page-cache budget (plantasjen, commit `dcc445e`, 2026-07-14)

The measurements behind `--index-type auto` and `AUTO_STORE_BUDGET_PCT`
(the routing rule itself is in [pipeline.md](../pipeline.md)). Sparse versus
external walls:

| Dataset | sparse store | sparse | external | winner |
|---|---:|---:|---:|---|
| germany | 3.32 GB | 25.7 s | 25.2 s | tie |
| north-america | 18.75 GB | **116.4 s** | 158.3 s | sparse, -26 % |
| europe | 28.94 GB | 363.1 s (`4ac11326`) | **285.7 s** (`cdfa9453`) | external |

The germany and north-america rows are matched `--bench 1` pairs. The
europe row is not matched (sparse `--bench 1`, external `--bench 3`), so
its margin is overstated; the ordering is not in doubt. The walls are
non-monotonic in store size, so the threshold is set from where sparse
pass 2 goes nonlinear rather than from where the walls cross. Per-ref
cost of sparse pass 2's random store lookups (`way_reframe_ms`
cumulative over refs):

| Dataset | store | refs | per ref |
|---|---:|---:|---:|
| north-america | 18.75 GB | 2.60 B | 0.145 us |
| europe | 28.94 GB | 4.37 B | **0.772 us** |

5.3x the per-ref cost for 1.68x the refs. With roughly 2-3 GB of anon
held outside the store, the measuring host's page-cache budget was about
25 GB: north-america's store sat near 75 % of it and stayed linear,
europe's near 116 % and thrashed. These two points bracket the knee
without locating it; 80 % is a choice inside the bracket biased toward
external, because routing a smallish input to external wastes seconds
while routing an oversized one to sparse costs minutes. The threshold is
relative to runtime available memory, never a byte constant fitted to
this host.

### Compression axis (Europe external, plantasjen, `--bench 1`)

| Compression | Wall | Peak anon | UUID | Commit | Δ vs zlib:6 |
|---|---:|---:|---|---|---:|
| `zlib:6` (default) | 270.8 s | n/a | `0b89f986` | `0dc8ae1` (post-A1) | - |
| `none` | **246.8 s** (`4m07s`) | 6.5 GB | `16c35911` | `4fc8e35` | -24.0 s, -8.9 % |
| `zstd:1` | **233.3 s** (`3m53s`) | 6.6 GB | `e2fba1bf` | `4fc8e35` | -37.5 s, -13.9 % |

The zlib:6 row was measured at an earlier commit (`0dc8ae1`); the
none/zstd:1 rows pin the compression-axis comparison to `4fc8e35`
(2026-04-27 overnight) but the cross-commit zlib:6 reference is not
co-pinned. Order of magnitude matches the prior 2026-04-14 finding
(419 s zlib:6 → 379 s zstd:1, -9.5 %, at the older `f3c53a34`/`66e43a11`
baselines): zstd:1 wins by relieving consumer/compression saturation
in stage 4, with similar output size.

> **This table is Europe-only and does not generalise to planet.** The
> 2026-07-14 planet cell (`0e9d93cc` vs `ed5dd6c5`, both `dcc445e`)
> found the planet streaming phase is *not* compression-bound: -80 % of
> compression CPU bought <=5 % of streaming wall, and the constraint
> moved to stage-3 -> stage-4 payload readiness instead. The "relieving
> consumer/compression saturation" mechanism above is real at europe and
> absent at planet. See the planet compression-ceiling callout below the
> planet baselines table before quoting -13.9 % at any other scale.

### altw external raw scanners read fallibly (bygg, 2026-10-31)

`e61e0ad7` replaced the external join's zipped `PackedSint64Iter`s (which
swallowed decode errors) with three `read_varint`s in lockstep. `brokkr
add-locations-to-ways --dataset planet --index-type external --bench 1`,
`--commit` cells, after `sudo fstrim -av`, two pairs in opposite order:

| Pair | `2ba3e170` (parent) | `e61e0ad7` | Delta | UUIDs |
|---|---:|---:|---:|---|
| 1 (parent first) | 369.9 s | 388.7 s | +5.1 % | `b3cc51d6` `ae2ba447` |
| 2 (change first) | 360.1 s | 352.3 s | -2.2 % | `2cda149e` `764b7ba2` |

Pair 1 is confounded by cell position: stage 1 pass A took 32.4 s in the
first cell (the first after the trim) and 96.9 s in the second, more than
the whole wall delta, while stage 2 ran 13 % faster on the change. This
was first read as the relation scan swinging (20 s against 87 s); that
figure is the `--human` row labelled `EXTJOIN_RELATION_SCAN_END`, which is
the segment that begins at that marker, the rest of pass A. The scan's own
START/END span is 12.0 and 10.3 s. In pair 2 both cells follow another
altw cell, pass A is 65.7 and 62.7 s, and stage 1, 2 and 4 are level or
slightly faster on the change. The whole-command walls cannot resolve a
cost below the position effect; the phase split shows none. Next section
for the position effect.

### altw external pass A swings with run history (bygg, 2026-10-31)

`brokkr add-locations-to-ways --dataset planet --index-type external
--bench 1`, `8ccb3a2e`, two cells back to back (about 6 s apart) right
after a `sudo fstrim -av`:

| Cell | Wall | Pass A | Relation scan | `s1a_pread_ms` | `s1a_id_shard_write_ms` | UUID |
|---|---:|---:|---:|---:|---:|---|
| 1 (first after trim) | 316.9 s | 32.4 s | 9.9 s | 147,288 | 558,332 | `a12a2f7d` |
| 2 | 353.4 s | 73.5 s | 12.7 s | 598,979 | 1,318,963 | `4cd2eddf` |

Pass A explains the whole 36.5 s wall delta; stage 2 and stage 4 are
within a few seconds. Pass A's work is byte-identical in every cell: 29.2
GB pread in 17,529 calls and 149.2 GB of id shards written in about
573,000 calls; only the time inside those calls moves (thread-cumulative
ms above). The same split holds across the twelve planet cells of
2026-10-30/31: the relation scan's span is 7-13 s in every one, pass A is
32-33 s in the cells that follow a different command, a long idle or a
trim (`13668dda`, `cf58d02e`, `b3cc51d6`) and 51-111 s in every cell that
follows another altw external cell or a large writer.

It is not input page cache. Per-process `/proc` io for `b3cc51d6` (fast)
and `ae2ba447` (slow, 5 s later) shows both reading about 28 GB from disk
during pass A, the whole way section; the slow cell did not lose cached
input and the fast one did not reuse any (the host has about 32 GB of RAM).
What differs is the rate at which pass A dirties shard pages: about 4.6
GB/s against 1.5 GB/s, steady across the pass. That is a dirtying rate
under kernel throttling, not a measured device rate. Each planet cell
writes and deletes hundreds of GB of scratch on an ext4 mount without
`discard`, so the freed blocks stay untrimmed until the next `fstrim`.
Trim debt, an exhausted drive write cache, controller GC and inherited
writeback state are not separated by these cells; that would need a trim
between consecutive cells, with device throughput and Dirty/Writeback
recorded.

Consequence for measurement: on this host, an altw external cell that
follows a scratch-heavy cell has run 30-80 s slower in pass A than one
that follows a trim or rest (11.5 % of wall in the pair above). A matched A/B must balance what precedes
each arm (predecessor workload and first-after-trim position), not only
alternate arms; when the arms write different scratch volumes, alternation
alone does not balance it. Read pass A against the rest of the command
before attributing a wall delta to code. Pass A's scratch volume was more
than it needed to be: the packed id-shard record below cuts it by a third.
The trim-between-cells chain further below ("pass A after a trim or after
another cell") narrows the mechanism.

### Packed id-shard records (bygg, 2026-11-09)

Pass A's id-shard record went from three `u32` fields (12 bytes) to one
`u64` word whose field widths are sized per run (`IdCodec` in
`src/commands/altw/external/mod.rs`; planet: 26-bit local id, 18-bit blob
ordinal, 19-bit slot, closure bit), with a second word only for a slot
that does not fit. Planet: 12,435,459,911 records, zero escaped, id shards
149.2 GB -> 99.5 GB written in pass A and read back in stage 2.

`brokkr add-locations-to-ways --dataset planet --index-type external
--bench 1`, baseline `0182566d` as a `--commit HEAD` worktree build
(brokkr#105 applies to it), variant uncommitted `--force` on the main tree,
five cells back to back, untrimmed (sudo refused in the session), in run
order:

| Cell | Wall | Pass A | Stage 2 | Shard write rate | ID |
|---|---:|---:|---:|---:|---|
| packed 1 (after unrelated cells) | 301.2 s | 22.3 s | 147.2 s | 4.5 GB/s | unstored |
| 12-byte 1 | 336.2 s | 41.5 s | 163.8 s | 3.6 GB/s | `39608b9a` |
| packed 2 | 335.9 s | 43.1 s | 159.2 s | 2.3 GB/s | unstored |
| 12-byte 2 | 370.9 s | 65.6 s | 169.9 s | 2.3 GB/s | `a0259d8c` |
| packed 3 | 367.0 s | 54.9 s | 164.3 s | 1.8 GB/s | unstored |

(Shard write rate is bytes written over the pass A span.) Every cell ran
slower than the one before it, by about 17 s, whichever arm it was: the
run-history drift of the section above. Against the mean of its two
neighbours, packed 2 is -17.6 s wall (-5.0 %) and -10.5 s in pass A;
across all five, drift-corrected, packed is about -19 s wall and -13 s
pass A (-25 %), and every adjacent pair has the packed cell faster or
equal. Stage 2 also gains (one third fewer bytes to read, and it now reads
and decodes one shard at a time). Peak anon 11.67 against 11.35 GB in the
last pair (stage 4's peak, not shards). The wall figure carries the
worktree-build confound; pass A, bound by scratch writes, is the clean
part of the result.

### altw external pass A after a trim or after another cell (bygg, 2026-11-09, at `098c89cb`)

`brokkr add-locations-to-ways --dataset planet --index-type external
--bench 1`, six stored cells in one chain, each preceded by the previous
cell, no `sync`. "Trim" is `sudo fstrim -av` between the previous cell
and launch; "no trim" launches a few seconds after the previous cell
ends. Drive: WD_BLACK SN850X 4 TB, ext4 without `discard`, 55 % used.
`/proc/meminfo` read at the boundaries showed about 12 MB Dirty and zero
Writeback. Seconds (shard write is `s1a_id_shard_write_ms`, summed over
threads):

| Cell | Before it | Trimmed | Pass A | Shard write | Stage 2 | Wall | UUID |
|---|---|---:|---:|---:|---:|---:|---|
| 1 | trim | 13.6 GiB | 22.6 | 163 | 129.9 | 276.4 | `6bf4f504` |
| 2 | no trim | - | 41.5 | 474 | 160.4 | 327.4 | `a2261b63` |
| 3 | trim, a few s | 600 GiB | 23.2 | 246 | 139.7 | 284.3 | `3b0b7eac` |
| 4 | no trim | - | 36.1 | 419 | 143.3 | 313.7 | `8bfccb49` |
| 5 | no trim | - | 41.2 | 641 | 150.9 | 315.9 | `32bd503e` |
| 6 | trim, over 120 s | 828 GiB | 29.3 | 349 | 145.0 | 290.4 | `6b7b97c5` |

Every trimmed cell's pass A is faster than every untrimmed one's, and
cell 5 shows the slow state persists through an untrimmed predecessor.
Cell 3 is the strongest contrast: a trim of a few seconds took pass A
from 41.5 to 23.2 s, where a few seconds without a trim left it at 36-41
s. Stage 2 moves the same way but less, so not the whole wall delta is
pass A. What this does and does not establish:

- A large inherited kernel dirty-page backlog at launch is ruled out by
  the boundary readings. Dirty throttling during pass A and outstanding
  device-side work are not.
- The no-trim arms were not given a gap matched to the trim's duration,
  and no idle-only rest cell was run, so trim is not isolated from
  elapsed time; cell 6 combines a trim with a gap over two minutes and
  apportions nothing between them. The finding is a short-gap
  trim-associated recovery, from one ordered chain.
- Host counters cannot see inside the drive, so trim debt and
  trim-enabled cache or GC recovery are not separated.

No code change follows: the library cannot trim (FITRIM needs
`CAP_SYS_ADMIN` and acts on the whole filesystem) and `discard` is host
mount policy. Reducing scratch bytes does reduce the exposure (the
packed records above). Sharing the shard files instead of keeping them
per worker was tried and refused (next section). Measurement consequence:
trim before every altw external cell of a matched A/B on this host, not
only before the suite.

### altw external from `098c89cb` to `53360af4`, matched (bygg, 2026-11-09)

The README campaign's altw external cell (`2bf49e1d`, 301.6 s) came in
25 s behind the 276.4 s of cell 1 above. Its `sidecar --compare` against
`6bf4f504` put the gap in the relation scan (11.1 to 21.0 s at half the
cores) and in stage-4 segments that read several GB more from disk, with
no host difference annotated: a cache- or drive-state shape, not a code
one. It had followed a trim, but after two scratch-heavy apply-changes
cells. Matched check: four `--bench 1` cells, A B B A, `sudo fstrim -av`
before each, A = `--commit 098c89cb`, B = HEAD.

| Order | Arm | Wall | Peak anon | UUID |
|---|---|---:|---:|---|
| 1 | `098c89cb` | 282.6 s | - | `7cb3f140` |
| 2 | `53360af4` | 292.8 s | 11.1 GiB | `d9bdc5d7` |
| 3 | `53360af4` | 295.7 s | 11.0 GiB | `72ee9cd9` |
| 4 | `098c89cb` | 296.5 s | - | `167ee35f` |

B averages 294.3 s against 289.6 s, 1.6 %, and the pairs disagree in sign
(B +3.6 %, then -0.3 %). Not a regression. The A arm alone spans 282.6 to
296.5 s on identical code, so the trimmed band at planet is about 280-300
s; 276.4 s and 301.6 s are its edges. The README row is the B median,
294 s.

### Shared per-bucket id shards, refused (bygg, 2026-11-09, against `09c86a56`)

The variant replaced pass A's per-worker x 256 id-shard files with one
file per id bucket shared by every worker: 256 files and a fixed
256 + 64 descriptor need whatever the worker count, worker-local batches
of 128 KiB per bucket appended with `write_all` under a per-bucket
mutex (wait counted as `s1a_id_shard_lock_wait_ns`), no `BufWriter`, and
stage 2 streaming each bucket's one file in 8 MiB chunks. Its pass A
batch RAM bound was 30 x 256 x 128 KiB = 960 MiB against about 1.9 GiB of
`BufWriter`s. Gate green and `brokkr verify add-locations-to-ways
--mode external` on denmark passed. Planet, `--bench 1`, 30 workers,
99.5 GB of shard bytes in every cell, baseline as `--commit HEAD`
(brokkr#105 applies to its CPU phases), variant uncommitted with
`--force`, so its cells are unstored. Seconds; shard write and lock wait
are summed over threads:

| Cell | Arm | Before it | Pass A | Shard write | Write calls | Lock wait | Wall | UUID |
|---|---|---|---:|---:|---:|---:|---:|---|
| 1 | baseline | trim (359 GiB), after check-refs | 22.2 | 225 | 383,414 | - | 282.6 | `c3ed0b50` |
| 2 | shared | trim (529 GiB), after cell 1 | 28.2 | 27 | 762,897 | 241 | 289.1 | unstored |
| 3 | baseline | no trim, after cell 2 | 22.6 | 232 | 383,362 | - | 288.3 | `ffdca8a8` |
| 4 | shared | no trim, after cell 3 | 35.1 | 38 | 762,849 | 386 | 303.5 | unstored |

Pass A lost 6.0 s (27 %) after a trim and 12.5 s (55 %) without one.
Whole-command wall lost 2.3 % and 5.3 %. Summed pread time also rose,
162 to 299 s in the trimmed pair. The time the baseline spent in shard
writes moved into lock waits: workers queue on common buckets. Not
separated: whether they queue because adjacent way blobs reference the
same node-ID buckets, because a holder stalls (dirty throttling inside a
write, being descheduled under the lock), or in the final drain, where
every worker walks the buckets in the same order. A buffered ext4 write
holds the inode lock for its copy, so appending at atomically reserved
offsets to the same shared files without a mutex would queue the same
way. Cell 3 also shows that untrimmed does not always mean slow: the
baseline's pass A was 22.6 s untrimmed after a trimmed cell.

Ruling: the shared-file topology is refused at this batch size and lock
granularity. What stays open: bounding descriptors with private files
opened on flush from a bounded descriptor cache, which keeps every inode
single-writer, and striping a bucket across several files. Neither was
built.

### External stage 1 worker cap (bygg, 2026-10-28)

Question: should external stage 1 pass A cap its worker count like stages
2 and 3 do? Uncapped it runs host parallelism minus 2, 30 workers on bygg,
each owning one 256 KiB writer per id bucket: 7,680 shard files and about
1.9 GiB of shard buffers. Arm A is `49a03ebb` (uncapped); arm B is
`fea271fd` (cap 8 applied before the descriptor request; a measurement
commit whose code was not kept). Planet, `--index-type external`,
`--bench 1`, alternating A then B, untrimmed. Seconds:

| Pair | Arm | UUID | Wall | Pass A | Stage 2 | Streaming |
|---|---|---|---:|---:|---:|---:|
| 1 | A uncapped | `2cf75884` | 546.7 | 114.6 | 210.9 | 212.4 |
| 1 | B cap 8 | `d05cddff` | 599.7 | 144.1 | 224.8 | 222.6 |
| 2 | A uncapped | `3ccbb592` | 708.0 | 226.0 | 238.9 | 234.9 |
| 2 | B cap 8 | `eed6408f` | 676.3 | 224.7 | 227.1 | 216.2 |
| 3 | A uncapped | `b302f622` | 728.0 | 225.0 | 248.1 | 246.5 |
| 3 | B cap 8 | `c83e69bd` | 701.3 | 221.2 | 244.0 | 227.7 |

Wall medians 708.0 (A) against 676.3 s (B), signs +, -, -. Pass A tied in
the slow-drive pairs and lost 26 % under the cap in the one fast-drive
pair, where at 8 workers each worker's serial pread, decode and emit chain
had less company to hide it (cumulative pread 461 s at 8 workers against
289 s at 30). Cumulative shard write time collapses with the cap (pair 2:
6,050 against 746 worker-seconds) because fewer workers sit in `write()`
at once; these are occupancy figures, not device service time. The cap's
wall wins in pairs 2 and 3 sit in stage 2 and streaming, opposite in sign
to pair 1 and smaller than the drift between consecutive cells.

Verdict: a cap of 8 did not show a reliable whole-command improvement, and
stage 1 stays uncapped on the CPU side; the descriptor cap still bounds it.
This does not show that 30 workers are free when the drive is slow, only
that capping did not pay. Decoupling file count and buffer RAM from the
worker count is the open alternative.

### Eager passthrough under `--keep-untagged-nodes` (bygg, 2026-11-09, at `b58e8ef3`)

The case the eager `before_item` above targets: altw external with
`--keep-untagged-nodes`, where stage 4 copies 33,287 node and relation
blobs (62.6 GB, `s4_passthrough_pread_ms` 51.4 s) between 17,529 decoded
way blobs. Arms: the tree's eager hook against an uncommitted lazy
variant that calls `before_item(k)` only once task `k`'s result is
popped, before its error is unwrapped (the pre-`e91e5f11` placement).
Both arms carry the read-ahead advice change of the next entry. Planet
reindexed input, `--bench 1`, `--force` (unstored), `sudo fstrim -av`
before every cell, order eager, lazy, lazy, eager. Seconds:

| Cell | Arm | Wall | Pass A | Stage 2 | Streaming (stages 3+4) | `WAIT_S4_SEND` total |
|---|---|---:|---:|---:|---:|---:|
| 1 | eager | 302.5 | 22.3 | 130.0 | 142.8 | 135.1 |
| 2 | lazy | 302.6 | 22.1 | 130.9 | 142.3 | 136.8 |
| 3 | lazy | 324.4 | 28.3 | 144.5 | 145.1 | 139.4 |
| 4 | eager | 297.5 | 22.4 | 129.1 | 138.7 | 133.3 |

`EXTJOIN_STAGE4` shares its START/END with `EXTJOIN_STAGE3`, so the
streaming span is stage 3's: stage 4's workers spend about 135 s of it
summed in `WAIT_S4_SEND`, and the consumer's own write time
(`s4_consumer_write_ms` about 64 s in both arms) fits inside the span
either way. Cell 3 is slow in every phase, pass A and stage 2 included,
which no stage 4 change reaches: drive state. Verdict: no measurable
effect; the passthrough copy is hidden behind stage 3 whether it runs
ahead of the next result or not. The eager hook stays, for its error
ordering and liveness contract, not for speed.

### Read-ahead advice after the credit (bygg, 2026-11-09, at `b58e8ef3`)

`ClassifyConsumer::on_advance` issues the sliding `POSIX_FADV_WILLNEED`
on the ordered pool's consumer thread. It ran before the consumed
result's credit was published, so a slow advice call held back workers
parked at the window. It now runs after `gate.advance`. Every task newly
admitted by a credit lies inside the byte window an earlier call already
advised, unless the schedule jumps more than `WILLNEED_WINDOW_BYTES`
between adjacent tasks, and advice issued in the instant before
admission gives such a task no lead either way. New counter
`fadvise_willneed_wait_ns`: the time the advising thread spends inside
the syscall (not the read-ahead I/O, not reclaim alone), emitted once per
advising pass. Variant (advice before credit, the old order) and tree
both uncommitted with the counter, `--force`, `cat --clean`. Seconds;
window and advice as % of wall:

| Dataset | Arm | Wall | `ordered_pool_window` | `fadvise_willneed` |
|---|---|---:|---:|---:|
| planet reindexed | after credit | 142.7 | 0 | 1.96 (1.4 %) |
| planet reindexed | before credit | 142.1 | 0 | 1.99 (1.4 %) |
| germany | before credit (first after planet, cold) | 8.9 | 1.07 (12 %) | 0.21 |
| germany | after credit | 7.0 | 0.91 (13 %) | 0.03 |
| germany | before credit | 7.0 | 1.01 (15 %) | 0.03 |
| germany | after credit | 7.1 | | |

An earlier planet cell of the tree arm before the counter was wired
measured 142.8 s. Verdict: no difference outside noise. The window binds
on germany (12 to 15 % of wall) in both arms, while warm advice costs
30 ms in total there, so what binds the window is the head blob's own
work, not the advice. On planet the consumer spends 2 s of a 143 s wall
in advice, 1.4 %, with `ordered_pool_result_recv` at 90 %: the consumer
placement is not a cost, and neither a worker-side cursor nor a dedicated
advisor thread has a cost to recover. Whether the advice pays at all on
the ordered pools is not decided by these cells.

## History

### Add-locations-to-ways

#### External join stage breakdown (Europe, commit `d3e13ed`, plantasjen)

Shared blob-metadata pass replaced three earlier separate scans
(`s1_way_schedule_build_ms` 24.8 s → 0.08 s,
`s1_node_map_build_ms` 30.9 s → 0.12 s,
`s4_schedule_scan_ms` 31.5 s → 0.14 s), collapsing ~87 s of repeated
work into ~31 s of shared scan.

| Phase | Time | RSS (anon) | Description |
|-------|------|-----------|-------------|
| Meta scan | 30.9s | 19 MB peak | Single reusable blob-metadata pass (`BlobMeta`) with tagdata state |
| Stage 1 (way pass) | 36.0s | 3.6 GB peak | Pass A + pass B; way schedule and node-blob mapping projected from metadata |
| Stage 2 (node join) | 92.9s | 7.15 GB peak | Parallel counting-sort per rank bucket, inline node-blob coord fill |
| Stage 3 (slot reorder) | 32.2s | 7.30 GB peak | Scatter 12-byte slot records directly into per-bucket `scatter_buf` |
| Finalize | 18.3s | 2.95 GB peak | Parallel `coord_payloads` tail |
| Relation scan | 14.3s | 0.85 GB peak | `collect_relation_member_node_ids()` between stage 3 and 4 |
| Stage 4 (assembly) | 90.6s | 3.25 GB peak | Way reframe + node decode/filter + relation passthrough |
| **Total** | **333s** | | |

Later at `e497e54` the finalize phase was replaced by an in-RAM
`BlobLocationRouter` routing table (finalize 18.3 s → 0.163 s on a
single-sample bench), trading a consolidated `coord_payloads` file for
95 MB of encoded straddler bytes in RAM plus ~21 GB of worker tmps kept
open across stage 4. See [notes-archive equivalent in commit history]
for the superseded `3d977a0` and `e497e54` breakdowns.

#### Europe ALTW phase breakdown post-seek_raw

`EXTJOIN_META_SCAN` is the only ALTW phase that walks blob headers; all
other stages use direct `pread` on the file (no `BlobReader`, no
`seek_raw`).

| Phase | Time (post-fix) | Time (pre-fix `ca6711e`) | Δ |
|-------|----------------:|-------------------------:|---:|
| Meta scan | 13.3s | 25.9s | **−12.6 s, −49 %** |
| Stage 1 (way pass) | 35.3s | 37.1s | −1.8 s |
| Stage 2 (node join) | 90.9s | 94.3s | −3.4 s |
| Stage 3 (slot reorder) | 32.9s | 33.2s | −0.3 s |
| Router build | 0.17s | 0.17s | 0 |
| Relation scan | 3.9s | 3.9s | 0 |
| Stage 4 (assembly) | 93.0s | 90.5s | +2.5 s (single-shot variance) |
| **Total** | **270.7 s** | **286.3 s** | **−15.6 s, −5.5 %** |

Meta scan delta is the direct effect of `BufReader::seek_relative`
preserving the buffer on every blob-body skip; the small downstream
deltas in stages 1+2 are page-cache benefit (header walk used to amplify
reads ~10× and push subsequent stages' working set out of cache).

#### External join planet (commit `aa3147c`, post-`seek_raw` fix)

Planet wall 700.6 s (UUID `e30f7ddc`, `--bench 1`), basically identical
to the pre-regression baseline of 698 s (within single-shot variance).
META_SCAN measures 17.5 s post-fix - vs the inferred ~30 s pre-fix on
the same code path, so the seek_raw fix saves ~12 s in that phase. As a
fraction of planet total wall (700 s), that's 1.7 %, comfortably inside
the noise floor of `--bench 1`.

The audit's 10-15 % wall prediction was based on Europe ratios where
META_SCAN is 9 % of total (28.5 / 320 s); planet's META_SCAN is only
2.5 % of total because Stages 1+2+4 dominate (~85 % combined). The
fix delivered exactly what it should at the phase level - wall just
doesn't move much because the targeted phase is small.

Phase breakdown (UUID `e30f7ddc`):

| Phase | Time |
|---|---:|
| META_SCAN | 17.5 s |
| STAGE1 (PASS_A + PASS_B) | 123.2 s |
| STAGE2 | 240.5 s |
| STAGE3 | 85.5 s |
| ROUTER_BUILD | 1.6 s |
| RELATION_SCAN | 6.3 s |
| STAGE4 | 223.3 s |
| **Total** | **700.6 s** |

#### External join optimization history

Planet `024422c..e30f7ddc`: original 256× re-read shape on Denmark was
302 s. Europe traced 2,060 s → 320 s across the intermediate landings
(single-pass merge, scatter buffer, P2b/P2c parallel assembly, external
radix, coord_payloads integration, shared blob metadata scan,
BlobLocationRouter).

The coord_payloads integration (2026-04-14) was pursued primarily for
non-wall benefits. Planet measured −29 s wall as a pleasant surprise;
Europe +7 s. The structural wins are: scratch peak 300 GB → 256 GB
(−44 GB), stage-4 major faults 555K → 3.2K (−99.4%), stage-4
delta-encode CPU 68 s cumulative → 0, no more 99 GB `coord_slots`
mmap thrashing across 6 workers.

Key earlier optimizations: node-only wire scanner (bypasses
PrimitiveBlock, eliminates 25 GB heap retention), scatter buffer
(eliminates sort + 4.69B pwrite calls, 15x speedup), BlobReader
fadvise(DONTNEED) (general infrastructure), deferred IdSetDense,
buffer reuse in bucket loads, shared blob metadata scan (collapses
three repeated header passes into one reusable `BlobMeta` vector).

A1 (rankless node-ID bucketed stage 1+2, landed 2026-04-25) cut europe
from 291.6 s at `6d71053` to 270.8 s at `0dc8ae1` (-7.1 %) by replacing
the two-pass way scan + rank index with single-pass IdRecord emission
and a streaming merge-walk.

---

#### ALTW external optimization arc (post-`3d977a0`)

Cumulative effect of the landed seam deletions (#8 `BlobLocationRouter`
`e497e54`, #4 stage-2 de-ranking `f1a4ada`, #9 L1 metadata-driven
relation scan `6d71053`, plus their predecessors).

| Commit | Change | Europe | Planet |
|--------|--------|-------:|-------:|
| `3d977a0` | Pre-structural-reports baseline | 400s | 953s |
| `4f059b67` | (pre-#8 planet baseline in structural reports) | - | 867.7s |
| `d3e13ed` | (pre-#8 Europe baseline in structural reports) | 333s | - |
| `e497e54` | #8 `BlobLocationRouter` (finalize consolidation removed) | 320.5s | - |
| `f1a4ada` | #4 stage-2 blob-local rank counter + drop rank index | 308.0s | - |
| `6d71053` | #9 L1 metadata-driven relation scan | 291.6s | - |
| `7904a95` | (current, #3/#11 attempted and reverted - bench `123f70f1`) | 291.6s | **698.1s** |

Planet drop `867.7s → 698.1s` (**−19.5%**) confirms the
stage-2/relation-scan wins scale more strongly with tuple count than
the Europe numbers suggest. Phase deltas vs `4f059b67` planet baseline:
stage 1 `148.5s → 112.8s` (−24%), stage 2 `266.6s → 235.2s` (−12%),
stage 3 `100.2s → 85.7s` (−14%), finalize/router `46.4s → 1.4s` (−97%,
all of #8), relation scan down to 6.0s (#9 L1), stage 4 `231.6s →
215.6s` (−7%).

