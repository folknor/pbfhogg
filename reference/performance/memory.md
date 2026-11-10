# Memory

### The CLI no longer caps malloc arenas (bygg, 2026-11-01)

The CLI answered the library's cross-thread allocation hook with
`mallopt(M_ARENA_MAX, 2)` and a 32 MiB `M_MMAP_THRESHOLD` pin. Both are
gone, with the hook. Cells are unstored `--force` cells of one binary,
`7a99298f` plus a switch choosing which settings the hook applied (cap,
pin, both, none); apply-changes, which never requested the hook, took
the same settings from `GLIBC_TUNABLES` on the same binary. `--bench 1`,
32 threads, glibc 2.43.

Which setting. germany apply-changes (OSC 4705, `zstd:1`), streaming
phase: none 1.36 s, pin only 1.33 s, cap only 4.94 s, both 5.07 s;
kernel 3.0 cores without the cap, 18.4 with it. germany cat `--clean`
walls: none 7.4 s, pin only 7.4 s, cap only 17.1 s, both 17.2 and 17.5 s.
The cap is the whole cost; the pin is neutral. At planet (OSC 4913,
`zstd:1`) apply-changes with both settings still takes 2608.9 s
(`MERGE_STREAMING` 2603.0 s, user 3.4 and kernel 22.9 cores, peak anon
2,335 MB), against 126.9 s (`MERGE_STREAMING` 120.3 s, user 26.6 and
kernel 1.3 cores, 2,389 MB) in the stored cell `188db700` at
`26fe5a5e`: the 2606 s of the previous section reproduces after the
sequence and delivery windows.

Mechanism. `strace -f -c` of the germany apply-changes run: 1,125,285
`futex` calls with the cap against 186,934 without, while `madvise`
(138 against 164), `mmap`, `munmap` and `mprotect` did not grow, nor did
minor faults in the sidecar (about 96k either way), so the time is not
trimming, mapping or faulting. With two arenas, every allocation above
the tcache limit (about 1 KiB) from 30 workers and the writer threads
takes one of two arena mutexes (glibc's `lll_lock`: futex-backed, no
user-space spin), and for an arena-backed chunk `__libc_realloc` holds
the mutex across `_int_realloc`, including the allocate, copy and free of
a block it has to move (`malloc/malloc.c`). apply-changes allocates 12.3 GB on
germany (`--alloc`, 4.7 GB input), mostly framing (447 KB average per
call), block take-out (379 KB) and rewrite (230 KB) buffers, so its pool
runs as two lock convoys. Where in the kernel the extra 15 cores go was
not traced (no perf on the host); the futex count says it is contention
handling.

Planet (`planet-20260223-with-indexdata`), wall and peak anon, hook
(both settings) against none:

| command | with the hook | without |
|---|---:|---:|
| `cat --clean version` | 237.2 s, 971 MB | 150.6 s, 918 MB |
| `build-geocode-index` | 320.1 s, 13,296 MB | 175.5 s, 13,319 MB |
| `repack` | 268.9 s, 1,371 MB | 170.9 s, 1,934 MB |
| `degrade --strip-locations` | 268.0 s, 1,384 MB | 170.4 s, 1,932 MB |
| `cat --dedupe` | 258.6 s, 1,096 MB | 217.0 s, 1,101 MB |
| `time-filter` | 147.5 s, 802 MB | 134.1 s, 761 MB |
| `renumber` | 125.3 s, 3,159 MB | 124.3 s, 3,324 MB |
| `check --refs` | 36.4 s, 2,216 MB | 36.4 s, 2,226 MB |
| `check --ids --full` | 28.3 s, 2,318 MB | 28.3 s, 2,295 MB |

Pin only: time-filter 134.0 s, 811 MB; repack 169.5 s, 1,697 MB. Removing
the cap cut the wall of every measured command that encodes or builds per
blob by 9 to 45 % (the cap added 10 to 82 %), and in each the hooked
cell's work phases carried more kernel time (up to 1.7 cores in
time-filter, up to 12 to 17 in cat `--clean`, repack, degrade and the
geocode builder, against at most 1 without); the read-only checks and renumber's splice rewriters were
unaffected. The memory the cap was added for no longer
appears: renumber's call site recorded about 26 GB uncapped and the
geocode builder's 25 GB, against 3.3 GB and 13.3 GB measured here. The
one peak the cap still lowers is repack's and degrade's relation phase
(about 4 s of the run), 1.38 GB against 1.93 GB, which is left to the
phase itself rather than bought with 98 s of wall. Not measured at
planet: `check --locations`, the sort sweep on an input with runs, and
`check --ids` without `--full`.

### Idle executor threads take exited workers' arenas (bygg, 2026-11-04, against `a82e23bc`)

**Mechanism.** glibc gives a thread its malloc arena at the thread's first
allocation, and `arena_get2` takes an arena off `free_list` (arenas whose
every thread has exited, pushed by `__malloc_arena_thread_freeres`) before
it creates one (`research/glibc/malloc/arena.c`). Every rayon worker
allocates as it starts (rayon-core 1.13: `main_loop`, `WorkerThread::from`,
`JobFifo::new`, a crossbeam `Injector` block, before the worker reports
itself started). So the CLI's executor, rayon's global pool started at its
first job, takes whichever arenas a just-ended worker pool left, free
memory and all, if its first job comes after that pool ended.

**Attribution** (uncommitted diagnostic: repack dumped per-arena
`malloc_info` at the node phase's end, after an `executor.spawn` of an
empty job there, and at the way phase's midpoint; planet
`planet-20260223-with-indexdata`, `--bench 1`, sidecar sessions `7dc1d843`
and `ec9d7037`):

- At the node phase's end 30 node-worker arenas held 316 MB, 314 MB of it
  free.
- Lazy start (the CLI as it is): at the way midpoint those 30 arenas were
  unchanged to the byte, held by the 32 idle executor threads, and 30 new
  arenas had grown to 21-36 MB each for the way workers: 1,130 MB held in
  65 arenas. Way phase peak anon 1,055 MB, relation phase 974 MB, wall
  147.7 s.
- The CLI building the global pool before dispatch (`build_global`, which
  waits until every worker has started): the 32 executor arenas stayed at
  0.13 MB each, the way workers took the 30 node arenas (about 11 to 26 MB
  each) and no arena was created: 787 MB held. Way peak anon 758 MB,
  relations 689 MB, wall 147.3 s.

That is the 180 MB of the section above, measured there at `1f93e674`.

**Starting the pool early is no general cure.** The same early start
measured on planet `build-geocode-index` (interleaved, `a82e23bc` from
brokkr's worktree `410c8832`, `87ea7677`, against unstored sessions
`7b56aa31`, `32ba68ea`): walls 173.8 and 171.1 s against 171.7 and
173.2 s, peak anon 13,299 and 13,302 MB against 13,309 and 13,323 MB, but
every phase from `GEOCODE_PASS2_ADMIN_ASSEMBLY` (the builder's first
executor job, after its pass 2 scan pools ended) to the end peaked 320 to
420 MB higher in RSS, p50 anon 4.67 and 4.78 GB against 5.07 and 5.18 GB.
There the lazily started executor inherits the pass 2 workers' arenas and
does the heavy parallel work in them; started early it grows its own while
those sit unused. Which order wins depends on whether the pool that starts
after a worker set ends then works or idles, so the CLI keeps the lazy
start and the `Executor` rustdoc states the consequence for library
callers.

**The change**: what the executor was started for at repack's phase
boundaries was one block, the phase's residual after its pool ended. On an
input whose blobs hold exactly the cap, the last input blob of each kind is
below it, so its elements go to the merge thread's builder and its final
flush was the executor's first job. The merge thread now frames that
residual itself and writes it raw. Output is byte-identical
(`denmark-20260220-seq4704-with-indexdata`, at the default cap and at
20000). degrade has no such block outside its unsort modes. Under those
every element goes through the merge thread's builder, which usually starts
the executor during the first phase; an input whose first phase completes
no block before the final flush still starts it after the pool ends. Not
changed: degrade's unsort modes produce adversarial test inputs.

Planet written at 8000 elements per blob (`--snapshot 8k`), `--bench 1`,
`a82e23bc` from brokkr's worktree (stored) against the change (unstored
sessions), run order HEAD, change, change, HEAD:

| arm | walls (s) | peak threads, way phase | way phase peak anon (MB) | peak anon (MB) |
|---|---|---|---|---|
| `a82e23bc` (`7d437f6d`, `41ab9a1c`) | 255.4, 256.8 | 65 | 583, 496 | 687, 619 |
| change (`7ac2425b`, `40ef3705`) | 260.0, 255.7 | 33 | 479, 458 | 580, 593 |

The node, way and relation phases run the same time in all four (88.3 to
89.0, 51.4 to 51.8 and 1.33 to 1.36 s); the 4.4 s wall difference in the
first pair is the schedule scan (114 against 118 s), which the change does
not touch. Germany (`germany-20260224-seq4704-with-indexdata`), one cell
each, the lazy trigger at HEAD (`17a4601d`) against an early-started pool
(session `ecf95439`): way phase peak anon 224 against 206 MB.

**The other executor users** (surveyed 2026-11-05 at `d919ab21`). Where
the executor's first job falls after a worker set ends:

- Its whole run or final pass, working throughout: `cat` (passthrough and
  `--type`, no worker set before), `time-filter` history, `getid`,
  `getparents`, two-pass `tags-filter`, `add-locations-to-ways` (every
  index type's output pass), `renumber` (every stage frames every block
  on it), unsorted simple extract (no worker set before its batches).
  `merge-changes` and the OSC `tags-filter` run their whole body inside
  it. `sort`, `cat --clean` and `time-filter` snapshot hand it nothing.
- Between worker sets, then idle: extract's pread write pools, which simple
  extract on sorted input alternates with its classify pools (node
  classify, node write, way classify, way write, relation classify,
  relation write). The executor started at the node write, after the node
  classify workers exited, and sat idle through each later classify phase.
  A node write whose blobs are mostly verified raw copies would start it
  for a few rebuilt blocks, the repack shape.

**The change**: `pread_execute` (simple's three write phases and the
complete and smart write passes) frames each block on its worker through
`FramedBlocks` and writes the frames raw, so extract's pread passes hand
the executor no job. Planet (`planet-20260223-reindexed`), `--strategy
simple --bbox=-25,34,45,72`, `--bench 1`, plantasjen, order HEAD, change,
HEAD:

| arm | walls (s) | peak threads, write / classify phases | peak anon (MB) |
|---|---|---|---|
| `d919ab21` (`bb82cdd8`, `4a447ac3`) | 223, 224 | 49 / 50 | 2,842, 2,855 |
| change (unstored session) | 226 | 25 / 26 | 2,400 |

Every phase runs the same time in both (node write 100.5 to 102.1 s, way
write 56.6 to 58.2 s); average cores in the write phases fall from about
23 to 21.5, the framing pool's threads that ran beside the workers. Output
passes `brokkr verify extract` on denmark; the frames carry the same
indexdata, tagdata and compression as the writer's (the builder's blocks
carry no WayMembers). degrade's unsort modes are left as above.

## glibc's sliding mmap threshold across commands (bygg, 2026-11-04, at `3c5b8d3a`)

Question: does glibc's dynamic mmap threshold leave freed memory resident in
later work, which command pays it and how much, and does a fixed threshold
set at process start buy the memory back without wall cost. Answer: every
multi-phase command surveyed pays it, by 4 to 41 % of peak anon, and none of
the three fixed thresholds measured is a uniform win, so the CLI still sets
no allocator policy. Do not re-run this survey; the cells are below.

Mechanism (`research/glibc/malloc/malloc.c`, `__libc_free` and `sysmalloc`):
freeing an mmapped chunk of at most 32 MiB whose size exceeds the current
threshold raises the threshold to that chunk size and the trim threshold to
twice it. Some such free happens early in every command (a pool's decode
buffers at a phase end, any blob-scale buffer), and from then on blob-scale
buffers below the raised threshold come from the per-thread arenas, whose
freed interior memory stays resident (only `malloc_trim` releases interior
pages). `mallinfo2` shows it directly: check `--refs` after its way phase
holds a 148.5 MB arena with 0.2 MB in use (4.8 MB with the threshold
fixed); extract `--complete` ends with a 5.16 GB arena, 1.36 GB of it free,
its 4 MiB `IdSet` chunks arena-backed (`hblks` 3), against a 149 MB arena
and 1,025 mapped chunks with the threshold fixed. Fixing the threshold also
freezes the trim threshold, so a fixed-threshold cell mixes both effects.

Method: the arms differ only in `GLIBC_TUNABLES=glibc.malloc.mmap_threshold=N`
(glibc applies it through `do_set_mmap_threshold`, which disables the
sliding threshold exactly as `process::set_mmap_threshold` would; passed to
the child through `scripts/envrun.sh`). The default arm is the binary as
shipped. One binary, `--bench 1`, 32 threads, glibc 2.43, THP `always`,
`planet-20260223-with-indexdata`; brokkr records no `GLIBC_TUNABLES`, so
every fixed-threshold cell is an unstored `--force` cell, and so are the
later default cells interleaved with them. Peak anon is the sidecar's
`anon` max; the peak phase is the sidecar segment that holds it, named by
the marker that begins it. extract cells use `--bbox=-25,34,45,72`,
apply-changes OSC 4913 with `--compression zstd:1`.

### The survey: default against a threshold fixed at 128 KiB

128 KiB is glibc's starting value, so this arm is the process as if no free
had ever raised the threshold.

| command | arm | UUID | wall | peak anon | peak phase |
|---|---|---|---:|---:|---|
| `check --refs` | default | `7592610c` | 31.5 s | 2,183 MB | ways |
| `check --refs` | 128 KiB | `5a19bf9d` (unstored) | 31.4 s | 1,983 MB | ways |
| `tags-filter w/highway=primary` | default | `707198dd` | 75.6 s | 2,554 MB | pass 2 |
| `tags-filter w/highway=primary` | 128 KiB | `03b5f0b7` (unstored) | 76.0 s | 2,276 MB | pass 2 |
| `tags-filter r/route=bus` (runs the relation closure) | default | `4e5ca3bf` | 60.8 s | 2,592 MB | pass 2 |
| `tags-filter r/route=bus` | 128 KiB | `cdb0f07f` (unstored) | 54.1 s | 2,285 MB | pass 2 |
| `extract --simple` | default | `b0d3cb39` | 110.2 s | 3,127 MB | relation write |
| `extract --simple` | 128 KiB | `843a874f` (unstored) | 112.0 s | 2,549 MB | way write |
| `extract --complete` | default | `3ad0761a` | 119.8 s | 4,896 MB | `PREAD_WRITE_EXECUTE` |
| `extract --complete` | 128 KiB | `f3ed59b3` (unstored) | 124.2 s | 4,274 MB | `PREAD_WRITE_EXECUTE` |
| `extract --smart` | default | `12ebfc9c` | 133.6 s | 7,779 MB | pass 1 relation classify |
| `extract --smart` | 128 KiB | `53375530` (unstored) | 135.3 s | 7,442 MB | pass 1 relation classify |
| `repack` | default | `723e4963` | 148.3 s | 783 MB | ways |
| `repack` | 128 KiB | `b0024e40` (unstored) | 151.2 s | 559 MB | nodes |
| `degrade --strip-locations` | default | `7f7d260e` | 154.3 s | 778 MB | ways |
| `degrade --strip-locations` | 128 KiB | `70c3224b` (unstored) | 154.4 s | 552 MB | nodes |
| `cat --clean version` | default | `55c10efb` | 146.7 s | 776 MB | relations |
| `cat --clean version` | 128 KiB | `ff567413` (unstored) | 150.7 s | 690 MB | relations |
| `time-filter` | default | `bb2517a3` | 125.5 s | 668 MB | main scan |
| `time-filter` | 128 KiB | `bb5d2dbb` (unstored) | 126.7 s | 611 MB | main scan |
| `apply-changes` | default | `ed178984` | 96.4 s | 1,999 MB | after the way-to-relation transition |
| `apply-changes` | 128 KiB | `dba29045` (unstored) | 99.6 s | 1,471 MB | way phase |
| `check --ids --full` | default | `3dd9be05` | 29.2 s | 2,292 MB | ways |
| `check --ids --full` | 128 KiB | `6e479d57` (unstored) | 29.8 s | 2,126 MB | ways |
| `renumber` | default | `cd477c21` | 131.3 s | 3,271 MB | stage 2d |
| `renumber` | 128 KiB | `d62ada6b` (unstored) | 135.4 s | 2,307 MB | stage 2d |
| `getid --add-referenced` | default | `b0db023a` | 39.0 s | 461 MB | pass 2 |
| `getid --add-referenced` | 128 KiB | `62a4c1df` (unstored) | 41.2 s | 272 MB | ways read |
| `build-geocode-index` | default | `08eed2c8` | 172.4 s | 13,309 MB | pass 2b |
| `build-geocode-index` | 128 KiB | `76c97efb` (unstored) | 176.9 s | 12,874 MB | pass 2b |

The r/route=bus pinned wall is not the change: its pass-2 schedule walk
ran from page cache (57 ms against 6.5 s); its work phases are equal.

Per command, what the sliding threshold costs (the 128 KiB arm's saving):

- Affected, in the phase after the first pool ends: check `--refs` -9 %
  (way phase), check `--ids --full` -7 %, repack -29 % and degrade -29 % (the
  way phase, their peak, falls below the node phase), tags-filter two-pass
  -11 % and with the relation closure -12 % (way-dependency and pass 2),
  extract `--simple` -18 %, `--complete` -13 %, `--smart` -4 % at the run
  peak and -21 % in its write phase, renumber -29 % (stage 2d), getid
  `--add-referenced` -41 % (after the edge store already left the arenas),
  apply-changes -26 %.
- Affected inside one phase: time-filter -9 % and cat `--clean` -11 %. The
  effect is not only between phases: any early free raises the threshold
  for the rest of the run.
- Barely affected: build-geocode-index -3 %; its peak is the builder's own
  stores, not blob-scale buffers.
- Not affected by construction, not measured: commands that hold no
  blob-scale buffers across a pool (plain `inspect`, `cat` passthrough, `cat
  --type`, `sort` on sorted input, `merge-changes`, plain `getid`), whose
  planet peaks are small next to every row above.

What it costs in wall: 1 to 5 % on the commands that allocate blob-scale
buffers per blob, through mapping, faulting and unmapping each one. extract
`--complete` way classify 16.7 to 19.4 s (minor faults 0.68 M to 10.6 M)
and write 76.4 to 79.3 s (0.19 M to 14.4 M), apply-changes streaming 61.7
to 64.7 s, getid pass 2 25.1 to 26.4 s, cat `--clean` +2.7 %, renumber
+3.1 %. check `--refs`, tags-filter, degrade and time-filter are within
noise.

### Higher fixed thresholds: 1 MiB and 4 MiB

Interleaved cells, all unstored, in run order (default, then the fixed
arm, or the reverse after the `fstrim`).

| command | arm | UUIDs | walls | peak anon |
|---|---|---|---|---|
| `getid --add-referenced` | default | `d2a31f1e`, `0bb8cec6`, `5d2fcb80`, `cd3167f1`, `0c110f7f` | 39.8, 41.0, 41.0, 41.0, 41.0 s | 411, 441, 442, 425, 417 MB |
| `getid --add-referenced` | 1 MiB | `b9f84be1`, `9d9ff652`, `7b0a2d89`, `eb501852` | 40.8, 41.8, 41.6, 41.9 s | 263, 278, 279, 261 MB |
| `getid --add-referenced` | 4 MiB | `25fdc5e1`, `0285b427`, `bc09f777` | 40.3, 40.6, 41.1 s | 373, 368, 373 MB |
| `extract --complete` | default | `bd8e67ab`, `0857e34d`, `e4333b70` | 120.4, 119.8, 119.7 s | 4,880, 4,894, 4,878 MB |
| `extract --complete` | 1 MiB | `477613f2`, `c55f7329`, `71e15b3f` | 121.4, 120.6, 120.9 s | 4,549, 4,509, 4,537 MB |
| `extract --complete` | 4 MiB | `a0a0f0d2`, `dc314727`, `3d8e733a` | 121.2, 121.3, 121.0 s | 4,660, 4,608, 4,611 MB |
| `apply-changes` | default | `134f91d8`, `3ff91a4f`, then after `fstrim` `e44bdbee`, `8e1629d1`, `a7211d20` | 95.4, 99.6, 102.7, 97.0, 100.5 s | 1,899, 1,938, 1,901, 1,869, 1,903 MB |
| `apply-changes` | 1 MiB | `ff41305a` | 95.7 s | 1,579 MB |
| `apply-changes` | 4 MiB | `2b90f120`, `974babea`, `07c0709d`, then after `fstrim` `a4071c51`, `594adc58`, `298c3b62` | 92.3, 104.7, 102.5, 92.5, 97.8, 99.1 s | 1,777, 1,834, 1,686, 1,737, 1,872, 1,689 MB |
| `renumber` | default | `5aa13b41`, `48a247a7` | 136.3, 136.3 s | 3,308, 3,250 MB |
| `renumber` | 1 MiB | `10aca9cd` | 137.4 s | 2,506 MB |
| `renumber` | 4 MiB | `0b726f57`, `f1ee10e1`, `5b4d01cc` | 127.6, 136.4, 138.6 s | 2,364, 2,322, 2,294 MB |
| `cat --clean version` | default | `54121254`, `bf32566e` | 150.4, 150.3 s | 770, 779 MB |
| `cat --clean version` | 4 MiB | `493a355c`, `1bb2d473` | 149.3, 149.6 s | 865, 864 MB |

- 1 MiB keeps most of the saving, but getid pass 2 runs about 4 % slower
  (26.3 to 28.2 s against 25.1 to 25.4 s; 21 M minor faults against 0.2 M).
  The sequential blob reader allocates each compressed frame, about 1.8 MB
  at planet, fresh per blob (`BlobReader::next_frame`, `src/read/blob.rs`),
  so a threshold under the frame size maps and unmaps every frame.
- 4 MiB is wall-neutral where measured (apply-changes medians 98.5 against
  98.3 s over six cells each, getid, renumber, extract `--complete` +1.2 %)
  and keeps a smaller saving (getid -15 %, apply-changes -9 %, renumber
  -30 %, extract `--complete` -5 %), but raises cat `--clean`'s peak by
  about 90 MB (12 %). A threshold fixed at 4 MiB from process start routes
  every allocation below it through the arenas, where the default would
  have mapped those above the value it slid to; that is a plausible reading
  of the cat regression, not an attributed one.
- No fixed value predicts a win everywhere: the request sizes of the
  churned buffers (frames, encode and frame output) and of the
  residue-forming ones overlap, and an explicit trim threshold governs only
  the arena top, not the interior holes.

Gaps, stated so they are not mistaken for absences: the 1 MiB and 4 MiB
arms ran only on the commands above (not check `--refs`, tags-filter,
extract `--simple`/`--smart`, repack, degrade, time-filter, check `--ids
--full`, the geocode builder); `multi-extract`, `inspect --tags`, `diff`,
`cat --dedupe`, add-locations-to-ways and check `--locations` were not
measured at any fixed value. Where the extract `--complete` way-classify
and write phases allocate what they free per blob is not attributed.

