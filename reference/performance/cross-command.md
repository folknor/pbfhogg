# Cross Command

### Current planet baselines (plantasjen)

The consolidated headline table. All rows `--bench 1` unless noted.
Rows marked `dcc445e` were re-baselined in the 2026-07-14 overnight
suite; the rest still carry their older commit in the Notes column.

> **Read the April-to-July deltas as environment, not code.** The
> 2026-07-14 suite proved the host drifted ~+8 % on byte-identical code
> (`16e3694` ALTW: 546.0 s in April, 589.9 s today - see the ALTW drift
> verdict below). Every pre-July baseline on this page is therefore ~8 %
> optimistic relative to today's drive, and a "+11 % regression" against
> one is not a finding. Commands that came back *faster* despite that
> headwind (apply-changes, build-geocode-index) are understated wins.

| Command | Mode | Wall | UUID | Notes |
|---|---|---:|---|---|
| cat (indexdata generation) | `--bench 1` | **87.2 s** | `f2c1e220` | commit `dcc445e`, 2026-07-14; was 86.5 s `5d90623f` (`aee7727`). A same-day re-run at the very end of a second suite measured **108.1 s** (`cdf95e0d`, `fb743f6`, +24 %) - identical code, exhausted drive. **Treat that spread as the honest error bar on any single planet `cat` number**, not as a regression |
| cat --type way | `--bench 3` | 45.3 s | `2fe62148` | |
| cat --type relation | `--bench 1` | 47.7 s | `fba6e13e` | |
| cat --clean version | `--bench 1` | **333.8 s (5m34s)** | `f2315551` (4fc8e35, 2026-04-27 overnight) | `parallel_classify_phase` + reorder-buffer streaming, 750 MB peak anon |
| cat --dedupe | `--bench 1` | **7,981 s (133m)** | `1794f8a6` | single-threaded MERGEPBF path - see callout below |
| check --refs | `--bench 1` | **60.2 s** | `f75bedc6` | commit `dcc445e`, 2026-07-14; was 53.8 s `7d9f5dfd`. +11.9 % on a single sample, inside the ~8 % drift band. Briefly re-opened 2026-07-14 on a `4776b92` suspicion that the getparents phase split then killed; **stays closed**. If ever re-examined, check the schedule-phase disk read first - check-refs walks headers too |
| check --ids (streaming, default) | `--bench 1` | **56.6 s** | `55e87d85` | commit `dcc445e`, 2026-07-14; was 56.4 s `b1fc4d2e` (4fc8e35) - flat |
| check --ids --full | `--bench 1` | **63.2 s** | post-`01c67da` | |
| getid (include mode) | `--bench 1` | **6.8 s cold / 0.2 s warm** | `264d9dbf` / `12e74756` | dispatch landing `19d3a62`, 2026-07-11; cache-state dominated, see ADR-0006 gates below |
| getid --invert | `--bench 1` | 91.0 s | `40f5bd52` | |
| getparents | `--bench 3` | **22.8 s** | `39570d10` | commit `fb743f6`, 2026-07-14. Matched same-day pair vs `--commit 2306fd9` 24.4 s (`da60663d`): **HEAD is 6.6 % faster**. The 19.0 s `a7c064eb` figure is retired - it was a warm-header-walk artifact, see flag below |
| inspect default (index-only) | `--bench 1` | **6.5 s** | `c146f2bb` | |
| inspect --nodes `-j 16` | `--bench 1` | **49.4 s** | post-`01c67da` | |
| inspect --tags `-j 16` | `--bench 1` | **168.3 s** | `00e00846` (`de8daf1`) | serial-merge era; superseded on bygg by "inspect tags counts into a sharded table" below |
| inspect --tags --type node | `--bench 1` | 71.3 s | `047ac2f9` | |
| inspect --tags --type way | `--bench 1` | 82.9 s | `959bda7c` | |
| inspect --tags --type relation | `--bench 1` | 8.8 s | `8daf5f04` | |
| inspect --extended | `--bench 1` | **820.7 s (13m41s)** | `19db1512` | full-decode era; superseded on bygg by "Inspect --extended: validator-projection scan" below |
| sort (already-sorted input) | `--bench 1` | **147.0 s** | `e278faff` | commit `dcc445e`, 2026-07-14; was 132.3 s `b9c10a41` (`16e3694`). Regression flag CLOSED - see below |
| sort `--io-uring` | `--bench 1` | 126.8 s | `9ce80125` (`16e3694`) | **not re-pinned**: host `RLIMIT_MEMLOCK` soft limit is 8 MB, io_uring preflight needs >= 16 MB. Raise via `/etc/security/limits.conf` and re-queue |
| tags-filter -R | `--bench 1` | 51.8 s | `cf116a6b` | |
| tags-filter (transitive) | `--bench 1` | **108.2 s** | `7e4301f9` | |
| tags-filter --invert-match | `--bench 1` | 461.2 s (7m41s) | `6665605a` | 4.3× the match path; ~99 % of ways kept |
| tags-filter --remove-tags | `--bench 1` | 111.8 s | `44d96d0a` | two-pass with tag-stripping in pass 2 |
| tags-filter --input-kind osc (osc 4913) | `--bench 1` | 6.2 s | `37f360d2` | OSC parse + filter only |
| extract --simple (Europe bbox) | `--bench 1` | **221.9 s** | `e43bb19f` | |
| extract --complete (Europe bbox) | `--bench 1` | **222.7 s** | `91fd90b4` | |
| extract --smart (Europe bbox) | `--bench 1` | 267.5 s | `07dcdae3` | |
| multi-extract --simple (5 regions, Europe bbox) | `--bench 1` | **883.6 s** | `68cecf88` | |
| multi-extract --smart (5 regions, Europe bbox) | `--bench 1` | 837.6 s | `2c842414` | |
| add-locations-to-ways `--index-type external` | `--bench 3` | **592.2 s** | `b65fcad0` | commit `dcc445e`, 2026-07-14, first cell of a suite on a rested drive - **this is a best case, not a typical wall**. Six same-day `--bench 1` cells at `fb743f6` measured **621.8 - 655.0 s** on identical code as the drive tired (`4ff288fb` / `8cc542eb` / `2205c7d5` flag-OFF). Quote the range, not a point |
| add-locations-to-ways `--index-type external --compression zstd:1` | `--bench 1` | 648.4 s | `0e9d93cc` | commit `dcc445e`, 2026-07-14. **Total wall is drift-confounded; the finding is the phase split** - see the planet compression ceiling callout below |
| apply-changes (daily diff, `--osc-seq 4920`) | `--bench 1` | **465.7 s** | `aaed129a` | commit `dcc445e`, 2026-07-14; was 756.3 s `8e940f71`. **-38.4 % despite the ~8 % drift headwind - understated win, unattributed** |
| repack (default cap, planet -> planet) | `--bench 1` | **374.6 s** | `33f2c448` | commit `dcc445e`, 2026-07-14; was 377.5 s `8027765b` (`8c1cf03`) - flat |
| time-filter (snapshot, cutoff 2024-01-01) | `--bench 1` | **263.8 s** | `8781ad14` | commit `dcc445e`, 2026-07-14; was ~270 s (`83183fb`-era) - flat |
| tags-filter (default two-pass, `w/highway=primary`) | `--bench 1` | **115.2 s** | `cd401093` | commit `dcc445e`, 2026-07-14; was ~117 s (`cf116a6b`-era) - flat |
| renumber | `--bench 3` | **191.1 s** | `43041dd4` | commit `dcc445e`, 2026-07-14; was 204.5 s `abd74459`. +10 s flag CLOSED - see below |
| diff-snapshots text `-j 16` | `--bench 1` | **227.5 s** | `22a5eb55` | |
| diff-snapshots osc `-j 16` | `--bench 1` | **293.8 s** | `cdcaa4f1` | |
| build-geocode-index | `--bench 1` | **347.0 s** | `57878b0f` | commit `dcc445e`, 2026-07-14; was 424.8 s `2b412af4` (2026-04-26). **-18.3 % despite the ~8 % drift headwind - understated win, unattributed** |
| merge-changes (planet, `--osc-seq 4913`, 1-OSC) | `--bench 1` | 44.2 s | `941a5784` (2026-04-28) | not re-pinned in the 2026-07-14 suite |
| merge-changes (planet, `--osc-range 4914..4920`, 7-OSC) | `--bench 1` | **53.8 s** | `f4de3f8d` | commit `dcc445e`, 2026-07-14; was 54.7 s `b6e964cc` - flat. Parallel-drain landed earlier (was 267.2 s `bef0f1fa`) |
| merge-changes (planet, `--osc-range 4914..4920 --simplify`, 7-OSC) | `--bench 1` | **73.7 s** | `3e3ef119` (2026-04-28) | parallel parse + parallel write_simplified landed (was 262.2 s `c0d140b6`) |

> **Sort `+6-7 %` regression flag - CLOSED 2026-07-14, not a code
> regression.** The flag tracked default and `--io-uring` sort drifting
> slower at `16e3694` vs the 2026-04-24 `1f97fae` / `68e1ba0`
> baselines. The 2026-07-14 suite settles it from the environment side:
> ALTW's `16e3694` binary re-ran at +8.0 % on byte-identical code, so
> the host itself moved by more than the flagged delta. Sort's fresh
> `--bench 1` (`e278faff`, 147.0 s) is +11.1 % on the same single-sample
> basis - fully absorbed by that band plus normal single-shot noise on a
> sub-150-s wall. The `4fc8e35` hotpath (`d64932d2`, **115.4 s** total;
> 108.6 s / 94 % in `pbfhogg::write::writer::flush`, 6.77 s / 6 % in
> `build_blob_index`) already sat *below* both baselines on the same
> writer-flush-dominated mix and pointed the same way. The truncation
> commits (`436998b`, `12699db`) are exonerated. **Do not chase this;
> re-open only against a matched-commit `--bench 3` pair.**

> **ALTW drift flag - CLOSED 2026-07-14: drive state, not code.** The
> matched `--bench 3` pair settles it on the same-commit comparison:
> `16e3694` measured **546.0 s in April and 589.9 s today**
> (`dc62a437`), +8.0 % on byte-identical code. HEAD is 592.2 s
> (`b65fcad0`), 0.4 % away - and conservative, since the April cell ran
> late in the suite on a tired drive and still matched rested HEAD.
> No bisect needed; the 546.0 s baseline is retired, not regressed
> against. The mechanism was caught inside the same suite: the zstd:1
> (`0e9d93cc`) and zlib:6 (`ed5dd6c5`) cells are the same binary an hour
> apart, `--compression` never touches stage 1, both wrote **exactly**
> 149,225,518,932 bytes of id shards - and `s1a_id_shard_write_ms` was
> 1140.7 s vs 1495.6 s cum, **+31 % write time on identical bytes**.
> Drive is 87 % full (469 G free of 3.6 T) and each ALTW planet run
> pushes ~705 GB of scratch through it. **Planet single-sample walls on
> this host carry a ~5-8 % environmental band and run order within a
> suite is a real variable.**

> **Drive health + trim state (checked 2026-07-16).** Follow-up to the
> drift verdict, from a SMART + fstrim pass on the 990 PRO:
>
> - **The drive is healthy. "Drive state" throughout these docs means
>   trim debt and garbage collection on a near-full consumer NVMe -
>   normal firmware behaviour - never hardware degradation.**
>   `smartctl -x /dev/nvme0`: 0 media and data integrity errors, 0
>   error-log entries, 100 % available spare, 4 % percentage-used,
>   142 TB written of the 4 TB 990 PRO's 2400 TBW rating (~6 % of
>   rated endurance; roughly 3,200 more ALTW planet runs at
>   ~705 GB/run). The 970 EVO Plus is likewise clean (its 2,520
>   error-log entries are all benign "Invalid Field in Command" from
>   tooling probing unsupported NVMe 1.3 log pages).
> - **A manual `fstrim -av` reclaimed 518 GiB** on the data drive
>   (`/dev/nvme0n1p1`, 2026-07-16). The weekly `fstrim.timer` cadence
>   cannot keep up with ~705 GB of scratch churn per ALTW planet run:
>   dead blocks accumulate for up to a week, and the controller saw
>   ~97 % namespace utilization (3.89 of 4.00 TB allocated) during the
>   2026-07-14 suite even though `df` reported 469 G free. That trim
>   debt - GC shuffling dead scratch blocks with almost no free flash
>   to work with - is the mechanism behind the +31 % same-bytes write
>   drift.
>
> Consequences: (1) **trim before any matched A/B suite** (and between
> cells of a long one) - it does not replace interleaving, but it
> shrinks the band interleaving has to cancel; (2) **every 2026-07-14
> planet baseline above was measured under worst-case trim debt** -
> post-trim runs may come in systematically faster for drive-state
> reasons, so a new-vs-2026-07-14 improvement needs a same-day matched
> pair before it is credited to code. The ~5-8 % band stands until
> re-measured post-trim.

> **`--inject-prepass` A/B - SETTLED 2026-07-14: cost is under 1 % at
> planet. Stop measuring this.** Six interleaved `--bench 1` cells at
> `fb743f6`, ON leading, alternating:
>
> | pair | ON | OFF | delta |
> |---|---:|---:|---:|
> | 1 | 627.4 s (`8267419a`) | 621.8 s (`4ff288fb`) | ON +0.90 % |
> | 2 | 644.6 s (`8333cbca`) | 645.4 s (`8cc542eb`) | ON -0.12 % |
> | 3 | 646.3 s (`2a5b109a`) | 655.0 s (`2205c7d5`) | ON -1.33 % |
>
> **Median ON 644.6 s vs OFF 645.4 s = 0.12 %, sign flipping across all
> three pairs.** ADR-0007's planet regression gate closes for good;
> record the bound as **<1 %**.
>
> Why the two earlier attempts failed, and the method that worked: drift
> across this suite was large and monotonic (walls climb 627 -> 655 s,
> +4.5 % in an hour) and is proven on byte-identical work - first cell
> `s1a_id_shard_write_ms` 1,174,295 ms vs last cell 1,515,132 ms, **+29 %
> over exactly 149,225,518,932 shard bytes both times**. A blocked pair
> hands that drift to whichever arm runs last: that is the +5.5 %
> (2026-07-14 blocked `--bench 3`) and the -5.3 % (2026-07-13
> `--bench 1`) - same question, opposite signs, both pure run order.
> Interleaving alternating `--bench 1` cells samples both arms across the
> same drift and cancels it. **Matched sample counts are necessary but
> not sufficient; `--bench N` is best-of-N within one cell and cannot
> cancel between-cell drift.**
> Injection counters (2026-07-13, first end-to-end exercise, all
> plausible): `altw_member_ways` 37.2 M, `altw_pinned_refs` 2.63 B (21 %
> of the 12.44 B way refs), `altw_field20_ways_emitted` 535.3 M (46 % of
> 1.166 B way messages), `altw_field5_bytes` 145.8 MB.

> **Planet ALTW streaming is NOT compression-bound (2026-07-14,
> `dcc445e`).** The single planet compression cell licensed by the
> do-not-retry reopen condition has been spent, and it inverted the
> prior model. Matched HEAD `--bench 1` pair, zstd:1 (`0e9d93cc`) vs
> zlib:6 (`ed5dd6c5`): `writer_compress_ns` **3855.2 s -> 767.8 s cum
> (-80 %, -3087 core-seconds)** while streaming wall (s3+s4) moved only
> **252.8 s -> 240.0 s (-5.1 %)**. Deleting 80 % of compression CPU
> bought at most 12.8 s. Under zstd:1 the constraint moves to stage-3 ->
> stage-4 payload readiness (`s4_readiness_wait_ms` 667.0 -> 1395.2 s
> cum, ~63 s per decode thread of a 240 s phase) while `s4_send_ms`
> collapses 705.2 -> 59.3 s cum; output grows 91.17 -> 95.20 GB. **The
> europe -13.9 % zstd:1 result does not transfer to planet** - a
> europe measurement is not evidence about planet's regime. Read the
> direction, not the magnitude: total wall says zstd:1 is *worse*
> (648.4 s vs 628.2 s) purely because +31.7 s of drive drift in the
> codec-independent stages 1+2 swamped the streaming win, which is also
> the cleanest illustration on file of why planet single-sample
> total-wall comparisons cannot be trusted here. Consequence: the
> ceiling-gated items (ALTW S2 DenseNodes wire filter, the
> A2/output-executor family) lose their gating premise - freed stage-4
> CPU has nowhere to go under *either* codec. The measured streaming
> constraint is the stage-3 -> stage-4 payload handoff, which is where
> any further planet streaming work has to aim.

> **`cat --dedupe` planet 133-minute wall.** Single `MERGEPBF` phase,
> peak anon RSS only 1.4 GB, avg cores 1.3 - the path was essentially
> single-threaded for the full 87 GB input. `cat --type way`
> passthrough is 45.3 s on the same input, so the dedupe overhead is
> ~175× the passthrough wall. The bench merges the planet with itself
> (brokkr passes the same path twice), so every blob sits in a two-blob
> overlap group and the whole file is decoded, heap-merged and
> re-encoded on one thread; it never was a `BTreeMap` pass, as this
> callout used to say. Since that run, `cat --dedupe` shares `sort`'s
> engine (`src/commands/blob_merge.rs`): pass 1 is the `HeaderWalker`
> walk, passthrough is coalesced `copy_file_range`, and each overlap
> group is swept on a worker thread with bounded streaming. The 133-minute
> row predates all three; the callouts below hold the cells measured since.
> A cell now has more to read than that row: `MERGEPBF_*` now nests
> `MERGEPBF_PASS1_*`, `MERGEPBF_PASS2_*`, `MERGEPBF_WRITE_LOOP_*` and
> `MERGEPBF_FLUSH_*`, the `cat_dedupe_*` counters give the plan's and the
> write's blob, run and duplicate counts, and `--stalls` attributes pass 2:
> `ordered_pool_result_recv` is the write loop waiting on a sweep,
> `ordered_pool_result_send` a sweep waiting on the write loop, and
> `ordered_pool_job_lock` plus `ordered_pool_job_recv` sweep workers idle
> for want of an issued run.

> **`cat --dedupe` planet with itself, borrowed-cursor sweep (2026-10-30,
> bygg).** Baseline `d271f04f` (`627883d7`, owned-element heap sweep,
> 4-block channel per run): **53m43s, peak anon 7.57 GB**, 3.2 avg cores,
> `ordered_pool_result_send_wait_ns` 88,527 s (about 27 of 30 workers
> parked behind the write loop holding their heaps). Variant (the commit
> after `42ea567f`, which replaced the heap with a k-way merge over
> borrowed cursors and framed each run's blocks on its own worker, sent in
> 2 MiB chunks), measured uncommitted with `--force` so not stored:
> **5m10s (310.6 s), peak anon 1.46 GB**, write loop at 27.3 avg cores,
> result send wait 0, result recv wait 303 s (the write loop now waits on
> the sweeps). 10.4x faster and 5.2x less peak anon. Same plan on both
> (50,816 two-blob runs, 0 passthrough). No fstrim before the variant cell.

> **What the planet self-merge sweeps hold (2026-11-04, bygg, `fda05956`
> plus the change that added the `overlap_run_*` counters, measured
> uncommitted with `--force`).** The counters (`src/commands/blob_merge/sweep.rs`,
> "Resource counters") on `cat --dedupe` planet with itself, cell
> `10f5ec37` (dirty sidecar): largest run charge
> `overlap_run_pinned_high_water_bytes` **12.5 MB** (two twin blobs, about a
> fifth of `SWEEP_PINNED_BYTES`), `overlap_run_demoted_cursors` 0,
> `overlap_run_chunks` 63,466 over 50,816 runs, largest chunk 2,097,145
> bytes (the 2 MiB budget), largest completed output held before shipping
> 3.9 MB. Matched pair after `fstrim`, interleaved `--bench 1`: HEAD
> `8827bc7f` 207.9 s and `78832185` 205.6 s against the variant 206.6 s and
> 208.1 s, signs mixed, write loop +1 % in `--compare`, peak anon about
> 1.0 GB on both: the bookkeeping costs nothing measurable.

> **Large frames sent in chunk-sized pieces (2026-11-04, bygg, `8a2c8ad2`
> plus the change that frames an over-chunk frame straight into pieces of
> at most `OVERLAP_RUN_CHUNK_BYTES`, measured uncommitted with `--force`).**
> `cat --dedupe` planet with itself, one `--bench 1` cell each: HEAD
> `2202ef70` 201.5 s, variant `c57ba39b` (dirty sidecar) 206.3 s. The
> write loop, the only phase the change touches, is 1 % faster in the
> variant (198.9 s against 200.7 s); the wall difference is pass 1, which
> read 654 MB from disk in the variant against 19 MB (a cold header walk,
> code the change does not touch). Planet zlib output has no frame above a
> chunk, so every frame is one piece and the counters are identical
> (63,466 chunks, largest 2,097,145 bytes): no cost on normal input.

> **Same-ID groups ship block by block, dedupe lookup indexed past a
> threshold (2026-11-08, bygg, `5903b4cd` plus the change, measured
> uncommitted with `--force`).** `cat --dedupe` reindexed planet with
> itself, interleaved `--bench 1`, no fstrim: HEAD `33e06a0f` 201.9 s,
> variant 201.4 s (dirty sidecar `b01f27ad`), HEAD `a6d92ca1` 200.3 s,
> variant 200.8 s (dirty sidecar `33f2ce2b`). Signs mixed, under 1 %.
> Counters unchanged (12.5 MB pinned, 63,466 chunks, staged 3.9 MB):
> planet groups are two copies, below the index threshold and inside one
> block, so normal input takes the old path.

> **Denmark and Japan across the engine move (2026-11-08, bygg, kernel
> 7.2.14).** `cat --dedupe` of each dataset's indexed file with itself,
> `dd312160` (the old engine, parent of the move) against `79d2e923` (the
> move: `HeaderWalker` pass 1, overlap runs swept on dedicated threads)
> against HEAD `4e0eaa50`, every arm a `--commit` worktree build (so
> brokkr#105's build offset cancels). Script bench `dedupe-self`
> (`benches/cat/dedupe-self.lua`), one iteration per cell, the input's page
> cache fixed and fincore-checked as the merge starts: cold (evicted) or
> warm (wholly resident). Arms rotated across rounds, denmark cold all six
> orders, the other conditions three. Walls in seconds; per cell, range
> across cells:
>
> | Condition | `dd312160` | `79d2e923` | `4e0eaa50` |
> |---|---|---|---|
> | denmark cold (6 cells) | 28.1-28.9 | 2.8 | 1.3-1.4 |
> | denmark warm (3) | 27.9-28.9 | 2.1-2.2 | 1.2 |
> | japan cold (3) | 141-142 | 13.0-13.1 | 6.1-6.5 |
> | japan warm (3) | 140-141 | 9.4-9.6 | 5.2-5.7 |
>
> The move made the self-merge about 10x faster on Denmark and 11-15x on
> Japan; everything after it to HEAD (the borrowed-cursor sweep `13ec0927`
> among it, not isolated here) halved it again. Cold costs the old engine
> nothing measurable (its cold and warm walls overlap); at HEAD, cold
> Japan's pass 1 is 0.92 s against 0.04 s warm (`2bcd9315` against
> `91bf8ad9`), almost all of that cell pair's 0.89 s gap; pass 2 took
> 5.61 s against 5.60 s although it read from disk cold. Japan
> peak anon: 89 MB, 1.18 GB, 368 MB for the three arms (`1524888a`,
> `83fc73eb`, `91bf8ad9`); the move's sweep threads bought the speed with
> memory, and HEAD gives most of it back. Output is the same size on every
> arm (denmark 495,905,546 bytes, japan 2,417,611,089), and `--check yes`
> runs (`19f2ce5f`, `89f2ce1e`, `f6d7708e`) diff each arm's denmark output
> against its input with no difference. The iteration wall includes about
> 12-15 ms of fincore and eviction before the merge, about 1 % of the
> fastest (denmark HEAD) cells and less elsewhere.
> What this does not measure: the self-merge puts every blob in a two-blob
> run, so passthrough and its coalesced `copy_file_range` never run; the
> next callout does.

> **Denmark passthrough across the engine move (2026-11-09, bygg, kernel
> 7.2.14, ext4, `sudo fstrim -av` first).** Script bench `dedupe-pair`
> (`benches/cat/dedupe-pair.lua`): denmark base merged with snapshot
> `one-relation`, a 436-byte file holding only base's highest-id relation
> (`getid r20243795`). One overlap run of two relation blobs; the other
> 7,395 base frames pass through, at HEAD as one coalesced copy (counters:
> 1 copy call, 7,394 frames coalesced, 486,570,984 bytes, 0 fallback
> ranges), at `dd312160` as a read and `write_raw` per frame. Same three
> arms, HEAD now `f49dd460`, all `--commit` worktrees, one iteration per
> cell, arm order rotated across three cold rounds and two warm ones.
> Iteration walls in ms (the cat child is about 25 ms less):
>
> | Condition | `dd312160` | `79d2e923` | `f49dd460` |
> |---|---|---|---|
> | cold (3 cells) | 389, 327, 333 | 1069, 1073, 1045 | 513, 518, 460 |
> | warm (2) | 215, 212 | 183, 200 | 193, 211 |
>
> Warm, the three arms overlap: the coalesced copy neither wins nor
> loses wall on this file, as the sort coalescer `244c6ec` did not on
> Europe. Cold, HEAD is about half again the old engine's wall (median
> 513 against 333 ms) though far below the move's. The cat child of
> HEAD cell `e4484a26` (435 ms) splits into pass 1 at 177 ms (the header
> walk) and pass 2 at 253 ms, 244 of it the copy's flush; the old cell
> `4f376168` (305 ms) is one span at 0.6 s user and 0.6 s kernel. The
> suspected cause, an inference not shown by disk-read attribution: the
> old pass 1 skipped payloads through a 256 KB `BufReader` with
> sequential readahead, which prepared the cache for its pass 2, while
> HEAD's header-only walk leaves the copy to fetch the file cold. Output
> is 486,848,104 bytes on every arm, and `--check yes` cells on each arm
> (HEAD `4f3830d1` dirty-tree, `e069805a`, `0900f601`) diff it against
> base with no difference. `--with ways-only` is the mixed shape (every
> way blob overlaps, nodes and relations pass through: 6,568 frames, 2
> copy calls at HEAD); it is not timed here.

> **Renumber `+10 s` - CLOSED 2026-07-14, no regression.** The matched
> `--bench 3` pair the flag asked for has run: HEAD **191.1 s**
> (`43041dd4`) vs `--commit cb99106` **200.4 s** (`9755cafc`), same day.
> HEAD is faster than the pre-drift commit, and also faster than that
> commit's own historical 194 s. The `cb99106` re-run landing at 200.4 s
> vs its historical 194 s (+3.3 %) is the same environmental drift seen
> everywhere else in this suite. The 204.5 s `abd74459` figure is retired.
>
> **Read the direction, not the 4.6 % gap.** The two cells ran 2h47m
> apart (HEAD 06:10, `cb99106` 08:57) because the suite grouped worktree
> cells under what turned out to be an **obsolete** build-thrash rule
> (brokkr has isolated worktree target dirs since `38e20a6`, 2026-07-07 -
> see AGENTS.md). So `cb99106` was measured on a much more tired drive
> and its 200.4 s is inflated; matched, the two are probably near-equal.
> The conclusion is safe in the conservative direction - the old commit
> got the *worse* conditions and still did not beat HEAD - but the
> magnitude is not a real 4.6 % code win. Do not cite it as one.

> **`apply-changes` parallel_writer join A/B - CLOSED 2026-10-21 (bygg):
> no measurable regression; the +45 % was drive state.** Question: did
> `548395f7` (parallel_writer joins every pool thread before returning)
> regress `apply-changes` against its parent `f2321c21`? Europe,
> `--osc-seq 4715`, default output (parallel_writer), `--bench 1` cells.
>
> | round | cells (walls) | read |
> |---|---|---|
> | untrimmed, mixed builds (HEAD from main-tree `target/`, baseline via `--commit` worktree) | before `04947db2` 35.2 s / after `00826907` 50.6 s; after `11b2f1b2` 63.3 s / before `a63a61b2` 43.3 s | looks like +45 % |
> | untrimmed, both arms via `--commit` worktrees | after `d628550e` 51.6 s, before `d14c2ab8` 59.8 s, before `d4410440` 65.7 s, after `383efa61` 69.0 s | monotonic slowdown by position, either commit |
> | after `sudo fstrim -av`, alternating pairs | before `f30a2af9` 51.9 / after `0b5c18c4` 57.1; after `90bb07ee` 61.7 / before `c464569e` 63.1; before `5b392e5c` 56.3 / after `08cf9360` 62.2; after `ea5c26b1` 63.4 / before `540cd0db` 61.4 | second cell of every pair is ~2-3 s slower; order-corrected effect ~ +3 s, within noise |
>
> **The same binary spanned 35-66 s over the session.** Europe
> `apply-changes` moves ~35 GB in and ~35 GB out, enough to degrade the
> drive within one session even immediately after a trim, so successive
> cells slow by position, not by commit.
>
> **The stall-counter flip was a false mechanism.** The first pairs
> showed the counters change shape (`brokkr sidecar <uuid> --stalls`):
> `a63a61b2` (before, 43.0 s) `writer_pipeline_send` 14.7 s,
> `writer_recv` 23.6 s, `merge_drain_reorder_gap` 3.9 s versus
> `11b2f1b2` (after, 63 s) 0.07 s, 59.1 s, 13.3 s. That reads as "the
> join moved the writer's blocking point", but the other pair
> (`04947db2` before 35.0 s: send 0.08 s, recv 32.4 s, reorder 6.0 s;
> `00826907` after 50.0 s: send 0.08 s, recv 47.8 s, reorder 12.0 s)
> shows the same shape on both commits, with recv and reorder-gap just
> scaling with wall. `writer_recv` is 92-94 % of wall in every cell but
> the fast `a63a61b2` one: the writer is starved by a slower read/merge
> side, which is what a slow drive looks like. A counter-shape change
> between two cells that differ in drive state is not evidence of a code
> cause.
>
> **Page-cache-resident cross-check (germany, `--bench 3`, input fits in
> RAM, so drive state drops out).** Before: `8d80fefa` 3.2 / 3.1 / 3.6 s,
> `62f5a663` 3.5 / 3.9 / 3.7 s. After: `4b869314` 3.3 / 4.2 / 3.2 s,
> `e1d9bdb3` 3.3 / 3.7 / 3.2 s. Median 3.55 s vs 3.3 s: no regression
> (after is nominally faster).
>
> Lessons: (1) for a hot-path change, run a page-cache-resident dataset
> (germany) first - it isolates CPU and sync cost from drive state, and
> here it settled the question in under a minute; (2) europe
> `apply-changes` degrades the drive in-session even right after
> `fstrim`, so cell position dominates and only alternating `--bench 1`
> pairs with order correction are readable; (3) compare arms built the
> same way - HEAD from the main tree against a baseline from a `--commit`
> worktree adds a confound, so use `--commit` for both; (4) do not read a
> stall-counter shape flip as mechanism until the same flip survives a
> same-wall comparison.

> **Library-use B4 (executor and cooperative permit wait) - neutral,
> 2026-10-22 (bygg).** Question: does routing the writer's per-block
> framing through `Executor::spawn` and the permit wait through
> `recv_permit` cost anything on the CLI path? Germany indexed,
> interleaved `--bench 1` pairs, both arms via `--commit` worktrees. The
> B3 arm is `2416a0cb` (B1 and B2): B3 changes no code these two commands
> run under the CLI host (its eviction stays on), and the bricks were not
> yet committed, so the B4 arm is `cbce533a`, a snapshot commit of the
> B3-plus-B4 working tree on `2416a0cb`. `sudo fstrim -av` could not run
> (no passwordless sudo); germany's input stays page-cache resident, so
> the pairs read CPU and synchronization cost.
>
> | command | pair | B3 cell (wall) | B4 cell (wall) | delta |
> |---|---|---|---|---|
> | renumber | 1 | `c5e5aa39` 12401 ms | `f56af7b9` 6601 ms | void, cold cache |
> | renumber | 2 | `6ea722e2` 6301 ms | `30be229a` 6301 ms | +0.0 % |
> | renumber | 3 | `ddf1b865` 6401 ms | `d69725c7` 6201 ms | -3.1 % |
> | renumber | 1, rerun | `4f7c25bb` 6401 ms | `434a7cad` 6601 ms | +3.1 % |
> | repack | 1 | `b619a6f8` 17401 ms | `87f88b51` 17801 ms | +2.3 % |
> | repack | 2 | `1d9fd437` 17502 ms | `351ab291` 17701 ms | +1.1 % |
> | repack | 3 | `b07798a3` 17601 ms | `6d58956e` 17401 ms | -1.1 % |
>
> Renumber's first B3 cell was the session's first germany read
> (`brokkr sidecar --compare c5e5aa39 f56af7b9`: 482 MB, 2087 MB and
> 1809 MB of disk read in its first three phases, none in the B4 cell),
> so that pair was rerun once in the same order and the rerun replaces it.
> Medians: renumber +0.0 % (B3 spread over its three warm cells 1.6 %),
> repack +1.1 % (B3 spread 1.1 %); both within the +2.0 % keep bound.
> `writer_coop_wait_ns` read 0 on `30be229a` and `351ab291`: no CLI path
> drives a `PbfWriter` from a rayon worker. Every wall in the set sits
> within a millisecond of a 100 ms step, so single-pair deltas on renumber
> are one or two steps of that granularity; read the medians, not the
> pairs.

> **Library-use cumulative arc (B1 to B5) - neutral, 2026-10-22 (bygg).**
> `fa94c6c7` (the baseline every library-use identity gate uses) against
> `46675558`, a snapshot commit of the B1-to-B5 tree on `2416a0cb` (the
> bricks were not yet committed). Germany indexed, interleaved `--bench 1` pairs in the same
> cell order as the B4 pair, both arms via `--commit` worktrees, no
> `fstrim` (no passwordless sudo), input page-cache resident.
>
> | command | pair | `fa94c6c7` cell (wall) | B5 cell (wall) | delta |
> |---|---|---|---|---|
> | renumber | 1 | `c5566af6` 6501 ms | `065a72db` 6301 ms | -3.1 % |
> | renumber | 2 | `c815474b` 6501 ms | `3cc4753a` 6501 ms | +0.0 % |
> | renumber | 3 | `44fb7775` 6401 ms | `66cdec33` 6301 ms | -1.6 % |
> | repack | 1 | `1777f885` 17401 ms | `399f5526` 17501 ms | +0.6 % |
> | repack | 2 | `106a67b2` 17601 ms | `2c4cccf9` 17501 ms | -0.6 % |
> | repack | 3 | `28106592` 17701 ms | `f174c9f0` 17501 ms | -1.1 % |
> | cat --clean | 1 | `00560867` 17301 ms | `888814c0` 16901 ms | -2.3 % |
> | cat --clean | 2 | `69e935e1` 16601 ms | `8b073aad` 17401 ms | +4.8 % |
> | cat --clean | 3 | `74188a34` 17401 ms | `759654b0` 17701 ms | +1.7 % |
>
> Medians: renumber -1.6 % (`fa94c6c7` spread 1.5 %), repack -0.6 %
> (spread 1.7 %), cat --clean +1.7 % (spread 4.6 %). All three are within
> the +2.0 % keep bound, so no brick needed bisecting. cat --clean's
> baseline arm alone spans 16.6 to 17.4 s, wider than the bound, and two
> of its three deltas are positive; it is the command to re-pair first if
> a later change touches the classify or framing path.

## CLI commands against osmium, cold cache (bygg, 2026-11-08, at `b355badd`)

The guide's "CLI command benchmarks" table, re-measured: until this run its
pbfhogg column came from `6fc1283` cells on plantasjen and its osmium column
from February 2026 runs of an earlier script, and its inspect tags row timed
pair counting against osmium's key counting. Script bench `vs-osmium`
(`brokkr.d/benches.toml`), denmark `pbf.indexed`
(`denmark-20260220-seq4704-with-indexdata.osm.pbf`) read by both tools,
apply-changes against `osc.4705`, osmium 1.19.1 / libosmium 2.23.1.
`scripts/vs-osmium.sh 3`: per row three pairs of `--iterations 1`
invocations, order pbfhogg-osmium, osmium-pbfhogg, pbfhogg-osmium. Runs are
stored in brokkr's bench store, marked dirty only because the bench files
were uncommitted (the binary is HEAD's).

Cache state decides the comparison. libosmium advises its input out of the
page cache as it reads (`remove_buffered_pages`, `POSIX_FADV_DONTNEED`, on by
default), so a first campaign without eviction had pbfhogg slow after every
osmium run and fast after a pbfhogg run, by about 0.6 s on the light rows
(inspect 636 ms against 5 and 4 ms; getid 648 and 651 against 14 ms;
inspect tags --type way 690 and 696 against 49 ms), osmium flat. That
campaign is diagnostic only. The bench's preflight now drops each arm's
inputs (`dd iflag=nocache count=0`) and fails unless `fincore` finds none
resident, so every run below starts cold.

| Row | pbfhogg runs | osmium runs | Medians | Ratio |
|---|---|---|---|---:|
| sort, already sorted | 1.1, 1.0, 1.0 s | 7.5, 7.6, 7.5 s | 1.0 / 7.5 s | 7.5x |
| apply-changes | 785, 756, 774 ms | 4.4, 4.4, 4.5 s | 0.77 / 4.4 s | 5.7x |
| tags-filter -R w/highway=primary | 161, 164, 167 ms | 370, 361, 373 ms | 164 / 370 ms | 2.3x |
| tags-filter -R amenity=restaurant | 224, 227, 233 ms | 770, 815, 818 ms | 227 / 815 ms | 3.6x |
| cat --type way | 171, 162, 175 ms | 1.5 s x3 | 171 ms / 1.5 s | 8.8x |
| inspect tags, bare | 784, 769, 774 ms | 1.0 s x3 | 774 ms / 1.0 s | 1.3x |
| inspect tags --type way | 697, 689, 687 ms | 407, 392, 393 ms | 689 / 393 ms | osmium 1.8x |
| getid, 9 IDs | 644, 645, 646 ms | 589, 592, 581 ms | 645 / 589 ms | osmium 1.1x |
| add-locations-to-ways sparse | 1.9, 1.9, 2.0 s | 7.4, 7.5, 7.3 s | 1.9 / 7.4 s | 3.9x |
| add-locations-to-ways external | 2.5 s x3 | 7.3, 7.4, 7.4 s | 2.5 / 7.4 s | 3.0x |
| inspect | 638, 638, 639 ms | none | 638 ms | - |

Last-campaign run UUIDs (pbfhogg, osmium, in run order): sort `e4c201d9`
`44219ebb` `875dd38c` `b75e9538` `b72d7f0b` `01c875f2`; inspect tags --type
way `f1191e3a` `81cfc999` `373fa2c3` `83338ccf` `c4a028a7` `8433d981`;
inspect `1ef7bd64` `46c5d8da` `fdf00786`; the rest are in `brokkr results`
under bench `vs-osmium`.

The rows that answer from indexdata pay a cold header walk. Sidecar of
`f1191e3a` (inspect tags --type way): `SCHEDULE_SCAN_LOOP` 633 ms reading
47 MB with 5.8 K voluntary context switches and no measurable CPU, then 61 ms
of decode; bare inspect (`1ef7bd64`) is the same 637 ms walk alone, 56 MB
read. osmium's tags-count -t way (`81cfc999`) reads all 475 MB in 407 ms at
10.8 cores. The walk is latency-bound on one thread, a synchronous read per
blob, so on a cold cache it costs more than a sequential read of the whole
file. Warm, the same walk is the 5 to 49 ms above. The next section is the
fix; the inspect, inspect tags --type way and getid rows of the guide come
from it.

## README campaign (bygg, 2026-11-09, at `53360af4`)

Every README table re-measured at one commit, in one detached run of
`overnight.sh` (131 cells, 1h20m, no failures): the read-only planet
block after one trim, the regional check answer runs, then each
write-heavy planet cell after its own `sudo fstrim -av`, then the
vs-osmium campaign, then the library scenarios. Planet input
`planet-20260223-reindexed.osm.pbf`; peaks are `brokkr sidecar --stat
anon`. All planet cells are `--bench 1` except inspect, getid and the two
merge-changes rows (`--bench 3`).

Planet cells (wall, peak anon):

| Command | Wall | Peak | UUID |
|---|---:|---:|---|
| inspect, iteration 1 of 3 (cold) | 6.62 s | - | `95fe678d` |
| getid, after inspect (warm headers) | 0.10 s | - | `19b48473` |
| check --refs | 19.8 s | 2.06 GiB | `e744c657` |
| check --ids | 17.3 s | 432 MiB | `0081b181` |
| check --ids --full | 17.2 s | 451 MiB | `5dd906c2` |
| inspect --extended | 27.8 s | 314 MiB | `d5d23b24` |
| inspect --nodes | 18.7 s | 303 MiB | `136cca9b` |
| inspect tags | 24.3 s | 472 MiB | `2d636e3a` |
| getparents | 16.4 s | 298 MiB | `a4478ff5` |
| getid --invert | 61.8 s | 43 MiB | `4db57439` |
| merge-changes 4913 | 2.3 s | 21 MiB | `ab508211` |
| merge-changes 4914..4920 | 3.5 s | 141 MiB | `10de5c6b` |
| tags-filter -R highway=primary | 34.4 s | 404 MiB | `fcc9c1ee` |
| tags-filter, two-pass | 56.1 s | 2.23 GiB | `49cf423f` |
| extract simple | 108.1 s | 2.49 GiB | `a1a00fed` |
| extract complete | 115.3 s | 2.62 GiB | `e95f5c10` |
| extract smart | 127.9 s | 2.74 GiB | `cb72eaa0` |
| multi-extract simple | 118.5 s | 3.77 GiB | `3b04a4c7` |
| multi-extract smart | 431.9 s | 3.05 GiB | `1e45ddf7` |
| build-geocode-index | 176.5 s | 13.0 GiB | `c5960c1d` |
| diff-snapshots text, base vs `20260411` | 139.3 s | 1.20 GiB | `12a929fe` |
| diff-snapshots osc | 151.2 s | 1.17 GiB | `4bba79ab` |
| cat | 42.4 s | 72 MiB | `be808f35` |
| cat --type way | 31.8 s | 10 MiB | `0f29e808` |
| sort | 52.8 s | 9 MiB | `07c95720` |
| cat --clean | 140.1 s | 823 MiB | `013a844c` |
| time-filter | 120.4 s | 688 MiB | `78629c21` |
| renumber | 134.6 s | 3.09 GiB | `1101563c` |
| tags-filter -i | 297.7 s | 6.76 GiB | `185cdb38` |
| cat --dedupe | 205.6 s | 945 MiB | `37a17bb3` |
| apply-changes zstd:1 | 97.9 s | 1.79 GiB | `3608960f` |
| apply-changes -l, keep-untagged base | 255.8 s | 2.77 GiB | `e0fc0c25` |
| altw external | 301.6 s | 10.9 GiB | `2bf49e1d` |

The altw external row was re-checked in a matched A/B (Add-locations-to-ways
above); the README carries that check's 294 s. Multi-extract smart against
its last stored cell (`d4d8d0dc`, `627883d7`, 484.9 s, about 7 GB): 11 %
faster at under half the peak. The diff-snapshots cells exit 1 (the
planets differ), as they should.

vs-osmium, denmark `pbf.indexed`, `scripts/vs-osmium.sh 3` (three
alternating single-iteration pairs per row, inputs evicted and
fincore-checked). Since brokkr#134 the iteration wall includes the
eviction preparation, up to about 30 ms; the figures here are each tool
process's child wall, median of three:

| Row | pbfhogg | osmium | Ratio |
|---|---:|---:|---:|
| sort | 305 ms | 7681 ms | 25.2x |
| apply-changes | 308 ms | 4484 ms | 14.5x |
| cat --type way | 162 ms | 1488 ms | 9.2x |
| altw sparse | 1401 ms | 7486 ms | 5.3x |
| getid | 119 ms | 619 ms | 5.2x |
| inspect tags | 208 ms | 1029 ms | 5.0x |
| altw external | 1938 ms | 7479 ms | 3.9x |
| tags-filter -R amenity=restaurant | 229 ms | 802 ms | 3.5x |
| inspect tags --type way | 161 ms | 389 ms | 2.4x |
| tags-filter -R w/highway=primary | 162 ms | 360 ms | 2.2x |
| inspect | 111 ms | none | - |

Against the `b355badd` campaign above, the rows that pay a cold header
walk moved most: inspect 638 to 111 ms, getid 645 to 119 ms (osmium was
1.1x ahead, now 5.2x behind), inspect tags --type way 689 to 161 ms
(osmium was 1.8x ahead). sort 1.0 s to 305 ms and apply-changes 774 to
308 ms also moved; neither row was re-attributed here. The osmium column
is flat. Both altw rows time one osmium command, osmium having one mode.

Library, `brokkr library`: isolation-cat denmark `--bench 5` (`f819d165`)
library median 262 ms, CLI median 222 ms, CLI/library 0.86; repeat-cat
(`2a4070ac`) 154 and 140 ms; concurrent-cat (`d1548769`) 0.99 ms each;
fixed-resources-executor (`71eed863`) 6.96 and 6.21 s; `--all --dataset
denmark` passed. isolation-cat planet `--bench 3` (`d73fbee8`): library
43.1, 55.4, 54.4 s, CLI 55.9, 61.3, 69.5 s, ratio 1.28. Not published:
the CLI arm's best is 13 s behind the typed `cat` cell above on the same
file and commit, and that gap is unexplained.

## Clean cells for script benches whose engines moved (bygg, 2026-11-10, at `9d9a2a79`)

Cells the README campaign left without a clean run after their engines
changed (`5432aac7`, `6fdde3f4`, `b58e8ef3`, `3286a9c5`). Committed
tree, markdown only dirty, `--iterations 1`, run back to back after a
trim. dedupe-self, dedupe-pair and diff-text are ranked on their child
wall.

| Bench | Condition | Cells |
|---|---|---|
| sort-cold | europe, `--force yes`, first after trim | 29.1, 29.0, 28.8 s (`69539b76`, `0bed0d36`, `9131c95b`) |
| sort-pbf | europe, indexed | 25.6, 25.6 s (`2ae6efa4`, `d4b2931d`) |
| dedupe-pair | denmark, `--with one-relation`, cold | 311, 319, 318 ms (`19b87fb1`, `8d9cf48b`, `ab25a84f`) |
| dedupe-pair | warm | 214, 216 ms (`29b68bfd`, `f7b25682`) |
| dedupe-self | denmark, cold | 1.3, 1.3, 1.3 s (`f91248b5`, `387f91fd`, `0912639d`) |
| dedupe-self | denmark, warm | 1.2, 1.2, 1.2 s (`54dd84c5`, `3ab6aa20`, `54461ff8`) |
| dedupe-self | denmark, warm, `--check yes` | 1.9 s (`28dc5320`) |
| dedupe-self | japan, cold | 6.1, 6.1, 6.1 s (`c7716b46`, `eec16a11`, `15565620`) |
| dedupe-self | japan, warm | 5.7, 5.7, 5.7 s (`0f655be1`, `1973d6c9`, `09dae305`) |
| diff-unindexed | germany, `--new 20260420`, all | 95, 99, 95 s (`2118dcf8`, `9ca09674`, `17cab69a`) |
| diff-unindexed | `--type way` | 54.6, 54.6, 54.2 s (`21a629b3`, `3ef5b849`, `89223288`) |
| diff-text | denmark, `--new 320k` | 3.1, 3.2, 3.1 s (`1ba372b9`, `bf6db0c0`, `785b41b2`) |
| diff-text | germany, `--new 20260420 --verbose yes --common no` | 8.4, 8.3, 8.3 s (`f12c91a4`, `f2a69547`, `b67bbe41`) |

Every condition is tight, within 4 % across its cells. These are the
current baselines for those benches; no verdict on the engine moves
themselves, which had no clean before-cell.

The vs-osmium `extract` row's first japan run failed on every osmium arm
("wrong format for coordinate: '=139.5'"); `aba6ff4b` fixed the bbox
spelling. At `4b08aa07`, japan, dataset bbox, complete-ways on both
sides: pbfhogg 2.4, 2.4, 2.5 s (`cf923912`, `17377af1`, `d4566fdf`),
osmium 7.2, 7.2, 7.1 s (`900e5298`, `31503713`, `3672b775`), about 2.9
times faster.

## Changes with no cell of their own (2026-10-24 to 2026-10-30)

Each was argued neutral or below noise and never measured alone. Listed
so a bisect landing near one knows what it changed.

- Write side: `frame_in_scratch` (`1db71d8a`, `f9625b94`); the permit
  taken through `acquire_permit` before spawn (`1db71d8a`);
  `fadvise::advise` on every `posix_fadvise` (`d510bbd7`); passthrough
  copies on one copy loop and one read loop (`bd2de78d`); the
  `307982d7..52690f1e` writer cluster on the `pwrite` pool and io_uring
  backends and on borrowed `write_primitive_block` calls (the buffered
  backend measured neutral, "Writer cluster across six commits").
- Read side: `BlobReader::next` as `next_frame` plus `into_blob`, with
  eviction before the envelope parse (`8881415e`); `OrderWatermark`'s
  mixed-sign `min_id` (`8881415e`); one `statx` per direct open
  (`ea17fd86`); the OSC arena reads (`20d10610`, see "Node boundary on
  the drain channel, not measured"); `BlobReader::leading_header` judging
  the first frame before its body on every `ElementReader` (`418739bd`).
- Counters: one struct per altw stage (`9dcf53a7`); `record_max` on the
  production `fetch_max` sites; apply-changes' and renumber's sets
  (`13dd02aa`); the unordered pools' gated stall helpers (`003fe717`);
  read metrics emitted after the scope joins, plus the fsync funnel
  (`8384fa29`). The `9dcf53a7` and `13dd02aa` name sets are checked
  ("Counter names across the counter-machinery commits" under Reading
  rules); their wall cost is not.
- Pools: the accumulate pool's stop flag (`95eea09b`); the unordered
  classify pools on one driver (`00ed5044`); the altw way reframes'
  shared block walk (`285df56d`).
- Commands: the `apply-changes -l` node boundary on the drain channel
  (`ff0b0a37`, "Node boundary on the drain channel, not measured");
  `apply-changes --force` reframing untouched unindexed blobs in the
  worker (`f703b02b`); degrade's sort-preservation guard (`9cce24b3`);
  the `expect_used` restructurings (`11becae7`); `check --ids`' extra
  type-order comparison (`4c254750`); input pinning (`926d5183`
  onwards).

## History

### Multi-dataset throughput tables (commit-pinned)

### CLI commands (commit-pinned multi-dataset tables)

#### Denmark (487 MB indexed, 59M elements, commit `6fc1283`, plantasjen)

| Command | Time |
|---------|------|
| tags-filter-osc | 14 ms |
| inspect (indexdata) | 19 ms |
| cat --type relation | 85 ms |
| tags-filter highway=primary | 152 ms |
| inspect-tags --type way | 251 ms |
| sort (sorted, indexdata) | 366 ms |
| getid | 379 ms |
| getparents | 400 ms |
| tags-filter amenity=* | 438 ms |
| apply-changes | 517 ms |
| cat --type way | 239 ms |
| merge-changes | 107 ms |
| inspect-tags | 1.61s |
| inspect-nodes | 1.73s |
| check --ids | 1.87s |
| getid --invert | 0.5s |
| extract --simple | 2.48s |
| extract --complete | 2.40s |
| tags-filter two-pass | 2.62s |
| extract --smart | 2.65s |
| add-locations-to-ways (external) | 7.4s |
| check --refs | 6.83s |
| time-filter | 9.39s |
| cat --dedupe | 22.4s |
| renumber | 22.3s |

#### Japan (2.4 GB indexed, 344M elements, plantasjen)

Baseline commit `aacbe80`. Entries marked with † were improved by later
optimizations and show the latest measured value.

| Command | Time | Notes |
|---------|------|-------|
| inspect (indexdata) | 92 ms | |
| tags-filter-osc | 169 ms | |
| cat --type relation | 306 ms | |
| cat --type way | 0.7s | † raw passthrough, `c33e8cc` |
| tags-filter highway=primary | 840 ms | |
| sort (sorted, indexdata) | 1.33s | |
| getid --invert | 1.3s | † raw passthrough, `c33e8cc` |
| merge-changes | 1.62s | |
| getid | 1.94s | |
| getparents | 2.06s | |
| tags-filter amenity=* | 2.20s | |
| inspect-tags --type way | 2.43s | |
| apply-changes | 2.53s | |
| extract --complete | 4.4s | † parallel classify |
| inspect-tags | 4.82s | |
| extract --smart | 5.2s | † parallel classify |
| inspect-nodes | 9.14s | |
| extract --simple | 9.36s | |
| check --ids | 10.4s | |
| tags-filter two-pass | 13.7s | |
| check --refs | 38.7s | |
| time-filter | 43.8s | |
| add-locations-to-ways | 64.1s | |
| diff | 72.2s | |
| diff --format osc | 73.1s | |
| cat --dedupe | 102.2s | |
| renumber | 152.4s | |

#### Germany hotpath (4.7 GB indexed, ~496M elements, commit `1b10bfd`, plantasjen)

| Test | Time | RSS | Notes |
|------|------|-----|-------|
| inspect-tags | 23.9s | 1.6 GB | decompress_blob 28.7s cumulative (parallel), pipeline 12.1s |
| check-refs | 74.1s | 4.6 GB | 99.97% in pipeline, single-threaded consumer bound |
| cat --type (zlib) | 61.8s | 10.9 GB | frame_blob 193s cumulative (parallel zlib), add_node 22.6s (429M), add_way 22.8s (70M) |
| apply-changes zlib | 6.2s | 395 MB | classify 2.9s, rewrite+output 2.1s |
| apply-changes none | 4.4s | 252 MB | classify 1.2s, rewrite+output 1.9s |

Allocation profiling (same commit):

| Test | Net Alloc | Cumulative | Key finding |
|------|-----------|------------|-------------|
| inspect-tags | 3.0 GB | 25.7 GB | decompress_blob 5.1 GB, wire::parse 3.1 GB |
| check-refs | 2.4 GB | 4.0 GB | wire::parse 3.0 GB (126%), nearly all in block::new |
| cat --type (zlib) | 175 MB | 240 GB | take_owned 41 GB, add_way 14.8 GB, decompress 6.9 GB |
| merge zlib | 293 MB | 29.6 GB | rewrite_block_parallel 17.3 GB, read_raw_frame 4.4 GB |
| merge none | 293 MB | 31.7 GB | same pattern, RSS under 300 MB |

#### vs osmium (Denmark, commit `23862d1`)

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| sort (sorted, indexdata) | **0.14s** | 11.6s | **83x** |
| apply-changes (indexdata + zlib) | **2.7s** | 7.2s | **2.7x** |
| tags-filter w/highway=primary -R | **0.24s** | 0.56s | **2.3x** |
| cat --type way (indexdata, raw passthrough) | **0.24s** | 2.22s | **9.3x** |
| add-locations-to-ways | **8.3s** | 12.6s | **1.5x** |
| check --refs | **4.8s** | 4.5s | 0.94x |

---

