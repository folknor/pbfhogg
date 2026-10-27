# Pre-release: what stands between here and 0.6.0

The working document from the end of the hygiene drain (2026-10-26) to the
0.6.0 release: what still has to be measured, decided or done, and what
someone picking it up must know. It replaces `notes/hygiene.md`, the review
audit of its "Review once drained" list and the drain's handoff; git history
keeps all three.

This is a transient note. Nothing in it is guaranteed true, and nothing
durable (code comments, `reference/`, `docs/`, `decisions/`, tests) may cite
it. When an item is done, remove it entirely; no historical record stays here.
When the file is empty, delete it.

The framing, from the maintainer: the release is gated on verifying pbfhogg
used as a library, which brokkr has never done. Every measurement so far went
through the CLI, one command per process, and the `Host` work of the last
weeks serves library use. The drain is finished, so its rule that nothing is
measured until it drained has lifted, and the measurement campaign below comes
next.

---

## Release gates

### Library use is verified (cross-repo, nothing recorded yet)

brokkr runs the `pbfhogg` binary, one command per process. Nothing has ever
driven the library the way an embedding caller would, so every behaviour that
only exists under library use is unverified:

- several calls in one process, one after another and concurrently: whether
  their outputs match the CLI's, whether any process-global state still leaks
  between them, and that each call's counters (`Invocation::for_each_counter`)
  and instrumentation stream (its own `debug::InstrumentSink`) are its own;
- a caller-supplied `Host`: `Resources` figures instead of the machine probe,
  the page-cache policy, the executor, a caller tripping its `Invocation`;
- a process whose soft fd limit nobody raised (the CLI raises it; the
  library may not, ADR-0016): cat's admission refusal below, and altw
  external stage 1's fd allowance, which has the same shape (bz `pbfhogg#11`
  records that stage 1 opens thousands of shard files at once);
- no stderr from the library (diagnostics only through `host::Diagnostics`).

Undecided: what brokkr ships for this. The likely shape is a small harness
binary depending on pbfhogg as a library, running a scripted sequence of calls
in one process (sequential and concurrent), with a brokkr subcommand that runs
it, records walls and stalls per call, and compares its outputs with the CLI's.
Nothing about it is designed; brokkr's source has not been readable from the
sandboxes this work ran in, so the brokkr half needs a host where it is.

### cat's descriptor admission for library callers

Reopened 2026-10-26 because the release is gated on library use; undecided.
Since input pinning (`56f8d9a9`), plain cat and `cat --dedupe` pin every
input for the whole run and refuse up front when the host allows fewer
descriptors than the inputs plus `CAT_EXTRA_FDS` (`ProcessActions::
fd_allowance`). The CLI raises its soft limit first; the library cannot, so a
library caller at a soft limit of 1024 can no longer cat about 1000 files,
which it could before pinning (it held one input at a time). A library-only
regression the CLI hides. Candidate fixes: pin each input only while cat reads
it, or take an fd budget from the caller on the `Host`. Whichever is chosen
should be checked against altw external stage 1's allowance, which has the
same CLI-raises, library-cannot shape.

### io_uring on real data

`notes/todo.md` "Release gate: the io_uring writer is unexercised on real
data" holds this gate and now also names the drain's io_uring changes that
have never run end to end (the `ReadSink` passthrough read in
`write/copy_range.rs`, the shared ordered-delivery loop, the short-pread
acceptance, the pool allocated after the output open) and what run reaches
them. Before this edit it only partly covered them: it asked for "one
`--io-uring` command", which need not take a passthrough copy and so need not
touch `ReadSink`. The bench host's `RLIMIT_MEMLOCK` refuses io_uring
(ADR-0014); the run needs a host with a raised limit, and the limit is not
raised from inside a work session.

### Other 0.6.0 items

`notes/todo.md` "0.6.0 headline items" holds the rest (GeoParquet export,
the two brokkr cross-repo checks on exit code 1 and the missing `check-index`
subcommand, the `renumber` / geocode eager bitset, GeoJSON v2, multi-extract
v2). The CLI surface redesign is deferred past 0.6.0.

---

## Open decisions and smaller work

- **bz issues filed in this work** (run `bz show pbfhogg#N` for the full
  text):
  - `#17` (p2, parked): after a worker panic in altw external stage 4 the run
    lingers until stage 3 publishes the slot the parked worker awaits.
    Correctness intact, only promptness. Undecided between a drop guard on
    the stage 4 worker body that aborts the router, and a worker-panic hook
    in `run_ordered` that calls the consumer's `abandon`. Needs a test-hooks
    fault case to pin it.
  - `#18` (p3, waiting on brokkr's source): whether `brokkr sidecar
    --compare` matches a repeated phase name to its first occurrence only.
    No stored run repeats a marker name, so existing data cannot settle it.
    Matters for multi-input cat `--clean` per-kind rows.
  - `#20` (p3, parked): `diff -v` compares relation members as sets, so a
    reorder or a dropped duplicate prints the modification with no member
    lines. Open: diff member order (osmium's CRC is order-sensitive), or say
    "member order or multiplicity changed".
  - `#21` (p2): `decimicro` in `scan/node.rs` (altw and `apply-changes -l`
    coordinate extraction) checks the product and the add separately in
    `i64`, so it refuses the cancelling encoding `check --index`, the scanner
    and the element accessors accept. Not blocked on `#23`: it needs its own
    planet A/B of altw (and `apply-changes -l`) before a fix lands. The
    fallback-only variant (checked `i64` first, `i128` on `None`) leaves the
    hot path unchanged. Low priority: no real producer emits such offsets.
  - `#22` (p2, blocked by `#23`): the columnar granularity-100 fast path and
    the element accessors give different garbage when the true nanodegree
    value overflows `i64`. Rejecting at block construction was considered
    and turned down earlier (`7de3b144`). Resolved by `#23`'s outcome: below
    noise, delete the fast path and the paths agree by construction; above
    it, record the divergence durably. Either way add a test pinning
    columnar, accessors and scanner on the cancelling encoding.
  - `#23` (p2): measure that fast path; cell in the campaign below.
- **`packed_varint_count`** (`e61e0ad7`): ruled measure first, then decide;
  the decision tree is with its cell below.
- **diff `-j 1`**: if its cell shows it clearly worse, give one-shard runs a
  streaming read back, without restoring a second engine.
- **Counter meanings that changed and are not in `reference/performance.md`**,
  which the observability ruling says should note changes affecting how
  stored numbers read: `merge_streaming_blobs_forced_rewrite` now also counts
  unindexed blobs re-encoded on `apply-changes --force` (`f703b02b`); cat's
  `blobs_decoded` and the "Decoded N blobs" summary count input blobs, not
  framed output blobs (`8da26b23`); diff and derive runs now feed
  the `pipeline_decompress_pool_*` counters (`126cbce2`); `publish_sync_calls`
  / `publish_sync_ns` are a tally of the publication's own syncs rather than a
  delta of the run's `writer_sync_all_*` (same values for a CLI run, which
  never publishes concurrently). (`merge_header_read_ms` widening at
  `a3f1a1bc` was ruled to need no entry; the comment at its timer says it.)
- **`check` small-file sizing.** The change reverted on 2026-10-26
  (letting a small file pre-allocate less than the per-kind floor) is safe
  to retry since `23c53a0b` moved overflow IDs into a side set, if anyone
  wants the memory back.
- **brokkr and `cat --clean --force`**: since `8da26b23`, `--clean --force`
  without `--type` is refused by clap. Unchecked whether any brokkr cell
  passes that combination; check before the cat `--clean` cells.

---

## The measurement campaign

### How to measure

From AGENTS.md and the record, the rules that matter most here:

- **Interleave matched cells.** `--bench N` is best-of-N within one cell and
  cannot cancel drift between cells. Alternate `--bench 1` cells of the two
  arms, compare medians, weigh sign consistency across pairs above the gap.
  Adjacent cells do not share drive state.
- **Noise floor: ignore differences below 2-3 %.** Not a regression, not a
  win, not a reason to run more cells. Planet single-sample walls carry a
  5-8 % environmental band on the bench host.
- **Read the phase split before calling any delta.** `brokkr sidecar <uuid>
  --human`, and `--compare` for a pair (check its host-difference annotation
  first: available RAM has explained more deltas than code). For
  getid/getparents-style commands compare the schedule phase's disk read
  before the walls (cold versus warm header walks).
- **Trim before a matched suite** (`sudo fstrim -av`).
- **One cell at a time**, never in parallel.
- Old commits build through `--commit <ref>`; the first build per commit is
  a cold rebuild, then reused.
- Numbers written into markdown carry commit hash and hostname.

### Reading rows across these commits

- Rows from `56a50e0d` up to `c2409c45` (the ordered pools' result channel
  as deep as the window) do not compare on send-side counters (`s4_send_ms`,
  `WAIT_S4_SEND`, `WAIT_P2_SEND`, `altw_pass2_send_ms`) or on consumer-bound
  memory; `reference/performance.md` says so. A pair whose base lies in that
  range runs the defective channel on every ordered pool.
- `6592af1f` (protohoggr 0.5.0) changed output bytes: `BlockBuilder` output
  shrinks for dense uid / user_sid deltas of magnitude `2^30` or more. Pairs
  straddling it are not byte-identical in output.
- Markers that went with their phases: `CAT_NODES_*` / `CAT_WAYS_*` /
  `CAT_RELATIONS_*` are absent from `8da26b23` until `168e581a` (only
  `CAT_FILTERED_*`), and since then mean write-side kind transitions, not
  barriers, so per-kind rows against the `f2315551` baseline are approximate;
  `SORT_OVERLAP_PARALLEL_*`, `DIGEST_ENVELOPE_*` and `writer_write_ns` are
  gone for good.
- Hotpath names moved: getparents, time-filter, repack, degrade and
  multi-extract's writer report under `parallel_classify_phase_ordered`
  (`reference/performance.md` cites `parallel_classify_phase` for
  getparents); command pipelines report under `run_pipeline` instead of
  `run_pipeline_fused`; cat `--clean`'s walk under
  `build_classify_schedule_blob_filtered_accepting_partial`.
- Every ordered pool now records `ordered_pool_*_wait_ns`, every unordered
  classify pool `classify_unordered_*_wait_ns`, so `--stalls` sees them.
  Each set is one process-wide running total emitted after every pool run;
  the step between emissions inside a phase is that run's share.
  `classify_unordered_desc_lock_wait_ns` is the shared-receiver mutex's own
  cost only while `classify_unordered_desc_recv_wait_ns` stays near zero.
  `ordered_pool_job_lock_wait_ns` is about the job receive wait times the
  idle peers (the holder keeps the lock across its receive).

### Prerequisites

- Before the altw cells: `notes/todo.md` "Stall-counter follow-ups", first
  bullet, the `s4_readiness_wait_ns` twin, so `--stalls` sees the stage 3 to
  4 handoff `reference/performance.md` names as planet's streaming
  constraint. A small code change; the rest of that entry stays backlog.
- Variant builds that are code changes, each its own commit: the shared
  walker descriptor variant, `#23`'s fast-path-removed variant, `#21`'s fix,
  and, depending on its first cell, the `packed_varint_count` masked fast
  path.
- Inputs: the planet `pbf.altw` variant for `apply-changes -l` (Denmark has
  none, so `brokkr verify merge` never runs `-l`; whether brokkr's
  apply-changes passes `-l` is unchecked); a Denmark unsorted snapshot for the
  sort cell (`unsorted` and `test-unsorted` are registered); an unindexed
  Europe or planet input for `sort --force` (`pbf.raw`).
- Already done, no longer owed: the merge engine's pass split and stall
  counters (`MERGEPBF_PASS1`, `_PASS2`, `_WRITE_LOOP`, `_FLUSH`,
  `cat_dedupe_*`, `ordered_pool_job_*`, from the bz `pbfhogg#16` fix);
  the ordered pools' channel cap and engine-wide stall set (`c2409c45`).

### Cells

Merged from the audit's per-slice orders and the handoff's planet-risk
order; reorder freely. "A vs B" means matched, interleaved `--bench 1` cells
with `--commit`. Each item names what it measures against; when a pair
moves, bisect inside it.

**1. Read path.** The cheapest broad check, run first.

- `brokkr read --dataset planet`, `ce6f20d9^` vs `307982d7`: covers the
  `BlockBuilder` cold refusal (`ce6f20d9`), the raw scanners and
  `packed_varint_count` (`e61e0ad7`), the single `run_pipeline` engine
  (`dd312160`) and the one frame decoder (`307982d7`: `BlobReader`'s prefix
  through one `fill` and the walk's bookkeeping; `HeaderWalker` copying each
  header into its buffer, which would show on warm-cache dense inputs).
- **`packed_varint_count`, ruled, required whatever the range shows:**
  `brokkr read --dataset planet`, `e61e0ad7^` vs `e61e0ad7`, plus one
  decode-bound command (`cat --clean` on europe). The per-byte position
  counter probably stops the loop vectorizing (estimated 2-5 % on
  decode-bound planet commands); BUG-191's width check was rejected
  (`bc38ad55`) for exactly that cost. Inside noise: keep. Above: keep the
  exact refusal but restore the vectorizable terminal-byte count, with a
  masked eight-bytes-at-a-time check for a run of nine continuation bytes
  and the per-byte scan only for an array that has one. The maintainer may
  instead revert to the plain count (a malformed tenth byte slips through);
  the measurement decides. `packed_uint32_count` tracked position before.
- The validated-invariant trio: `brokkr read --dataset planet` plus one
  tag-heavy re-encoding command (`tags-filter` or `inspect --tags`),
  `5a9d40e4` vs `85c1428c`. Covers `e28fdf69` (`take_message` slot check per
  element parse, elements parsed twice), `9c85d37e` (a cold panic call per
  element and two per dense tag varint, through a generic `#[inline]`
  `#[track_caller]` helper; equal branch counts do not guarantee equal
  codegen) and `85c1428c` (a `Result` per kept element on every re-encoding
  command, a push loop instead of `extend` for roles). Bisect only above 3 %.
- The partial-index guard (`84f50b31`): `brokkr read --dataset planet`,
  `84f50b31^` vs `84f50b31`. An armed frame now parses indexdata (a 42-byte
  copy and a deserialize) on reads that skipped it (the pipelined reader
  without a blob filter, `par_map_reduce`) and compares once per data frame
  everywhere.

**2. Ordered pools (`56a50e0d`)**, the top target of its slice: windows
where there were none (`default_window`, 8 per worker, floor 32, never
tuned), claim workers instead of a dispatcher, `WILLNEED` advice moved from
the dispatcher to the consumer (one `posix_fadvise` per consumed item on the
serial writer thread, further ahead than before; it can enter reclaim at
planet scale), failures in schedule order, passthrough prefixes copied
between decode results. Risks the audit named: `OrderedGate::advance` takes
the mutex and `notify_all`s on every consumed item (a wake syscall even with
no waiters, about 1 us per blob, more on the 8k encoding; a thundering herd
when the window is full, stage 4's steady state); stage 1 workers parking
behind a slow head for no memory gain; with `--keep-untagged-nodes`, stage 4
copying the node prefix only after the first way result. The audit specified
`60e1b691` vs `56a50e0d`; that pair measures the window-deep channel the fix
removed, so pair `60e1b691` against `c2409c45` (the shape that ships) and
bisect inside if it moves. Phase split first, in this order:

1. `add-locations-to-ways --index-type external` on planet, zlib:6: stage 1
   and stage 4 walls, peak anon, `s4_*`.
2. `extract --strategy smart` (Europe bbox) on planet: peak anon (the record
   has the arena-residency history) and wall.
3. `multi-extract --regions 5` on Japan and Europe: peak RSS. Its region
   writer gained `WILLNEED` here for the first time (measured a win on
   europe only); ruled kept, measured here.
4. `repack` on Europe or planet.
5. tags-filter two-pass and getparents on planet: `WILLNEED` on the
   consumer.
6. time-filter on planet.

Also here: extract simple's two pools moved onto the unordered driver and
gained `WILLNEED` (`8928de18`; ruled kept, measured in the campaign):
`extract --simple` on planet (a recorded row exists) and europe,
`8928de18^` vs `8928de18`. And `#23`: `extract --strategy smart` (Europe
bbox) and `multi-extract --regions 5` on planet, HEAD vs the variant with
the columnar granularity-100 fast path and its `offset % 100` guard removed;
under noise, delete it and resolve `#22`.

**3. sort and `cat --dedupe` on one engine** (`79d2e923`, then `060c2f74`
and `8928de18`). Pass 1 moved to `HeaderWalker` preads (sort's measured
trade: a planet win, a dense-europe pass-1 regression); passthrough to
coalesced `copy_file_range`; overlap runs to dedicated sweep threads sized
like the decode pools, admitted two runs per worker ahead of the write head,
so dedupe holds up to `min(window, workers)` sweep heaps plus `window x 4`
re-encoded blocks, and its threads can oversubscribe the cores beside the
writer's compression on the host executor; pass-2 workers pread one shared
descriptor per input with no advice. `8928de18` moved pass 2 onto
`run_ordered_streams` (one thread fewer; a sweep checks cancellation every
`SWEEP_CHECKPOINT_REMOVALS` removals). Cells:

- `cat --dedupe` on Denmark and Japan, `79d2e923^` vs `79d2e923`, then HEAD.
- `sort --dataset planet`, `79d2e923^` vs `79d2e923` (expect no change: no
  runs, no threads, one coalesced copy).
- sort on the Denmark unsorted snapshot, same pair.
- One planet `cat --dedupe` cell (planet merged with itself, every blob in a
  two-blob run; 133 min single-threaded before), watching peak RSS. It now
  has the pass split and the job/result wait counters.
- `sort --force` on an unindexed Europe or planet input, `060c2f74^` vs
  `060c2f74`: the `SORT_INDEX_BUILD` wall and peak RSS. Every unindexed blob
  now pays the combined ID-and-tag scan in serial pass 1, and its serialized
  key set is held to pass 2 (up to GBs at 8k-encoding blob counts). `plan`
  adds four per-frame `Vec<bool>`s and about 60 B per blob inline; only
  matters at 8k-encoding blob counts.

**4. diff and derive.** One block-pair engine at every `-j` (`2c12a955`),
ruled kept. Claimed `index.count` on single-sided runs accepted (ADR-0013).

- Planet `brokkr diff` (base against the apply-changes output),
  `2c12a955^` vs `2c12a955`: should show the restored byte-equal skip on
  the default path (japan `diff -c` went 50.0 s to 6.8 s with it, before
  the July default flip lost it). Same cell for `diff -q`, which now takes
  the skip too (`f0fe01fc`), and for the text drivers' optional writer
  (`dfe45a35^` vs `dfe45a35`, one predictable branch per write site, at the
  default `-j` and `-j 1`).
- Planet `diff-snapshots --from base --to 20260411`, text and osc,
  `2c12a955^` vs `2c12a955` (expected unchanged). Recorded sequential planet
  figures, which cannot be re-measured: text 2134 s (`a9d430f2`), osc
  2225.6 s, peak anon about 28-41 MB per phase; header walk about 15 s cold.
- A `-j 1` europe cell for wall and peak anon. `-j 1` now does a whole-file
  header walk of both inputs first, then preads per blob from the walker's
  descriptor re-advised `SEQUENTIAL`; it lost page-cache eviction,
  `--direct-io` and `MergeBuffers` recycling. Verbose diff now shards, so
  its output goes through scratch and a copy. If clearly worse: see "Open
  decisions".
- Unindexed (sequential) diff: `brokkr diff --dataset denmark --variant raw`
  (europe if it shows), with and without `--type way`, `8881415e^` vs
  `8881415e` (per-element order check, per-block kind classification,
  excluded kinds dropped unmerged; expected faster under `--type`, slightly
  slower per element otherwise). Then `94743e65^` vs `94743e65` separately
  (borrowed elements instead of owned conversion, expected faster; the two
  sides' decoded-block peaks can now coincide) and `126cbce2` (one
  `DecompressPool` per merge: an `Arc` and an uncontended mutex per block,
  idle buffers bounded by 4 MiB each and 64 buffers). No measurement of the
  unindexed path exists at all. Check that brokkr's `diff` takes
  `--variant raw`.

**5. cat.**

- Plain `brokkr cat --dataset planet`, `307982d7^` vs `307982d7`: the
  raw-frame walks still zero-fill (`FileReader` implements only `read`, so
  std's default `read_buf` zero-initialises) and an unsized
  `Take::read_to_end` starts at 8 KiB and doubles, about 4 read calls per
  1.8 MB payload and an extra copy of a few hundred KiB through the
  BufReader; estimated 25 us per blob, probably hidden if I/O-bound. If it
  costs: implement `read_buf` on `FileReader` (which would also remove a
  memset of every payload on every `BlobReader` path, an older cost worth
  its own cell) or go back to `resize` plus `read_exact`. The same cell
  covers cat's fd admission (`56f8d9a9`: two `getrlimit`, one `setrlimit`,
  one `/proc/self/fd` listing per run) and the reframe's
  `MAX_BLOB_DATASIZE` compare (`f9625b94`, one per reframed blob).
- `cat --clean version` on planet, `a05b3d25` vs `8da26b23` (one ordered
  pass per input instead of three per-kind phases, window 64, sized for
  reorder buffers and not tuned here): wall, peak anon, stall counters, and
  whether workers stall behind a slow head blob. Before `56a50e0d` moved it
  onto `run_ordered`, so the two do not blur; then repeat at HEAD, which has
  the per-kind rows back (approximately). Baseline `f2315551`: 333.8 s, 750
  MB peak anon.

**6. Writer cluster.** One A/B of `ae646053^` vs `52690f1e` covers the
per-block dispatch (`ae646053`), the shared pipeline spawn (`3ab1b4ce`), the
checked frame header encoder (`f9625b94`), the one-meaning writer counters
(`62046948`: a stopwatch per chunk on the pwrite pool's dispatcher, a
stopwatch and atomics per raw write on multi-extract's synchronous writer),
the shared ordered-delivery loop (`7abba0df`) and the OSC gzip writer's
`ReorderBuffer` (`52690f1e`). The audit named no command; a block-encoding
command on europe or planet (repack or cat `--clean`) plus planet
`diff --format osc` for the gzip writer would cover it. Bisect only if it
moves.

**7. add-locations-to-ways.**

- Raw scanners read fallibly (`e61e0ad7`: three `read_varint`s in lockstep
  instead of zipped `PackedSint64Iter`s; protohoggr's iterator swallowed
  errors on purpose): `e61e0ad7^` vs `e61e0ad7`, external on planet. The
  audit asked for sparse on planet too, but forced sparse at planet thrashes
  for hours (`notes/todo.md`, "altw sparse path"); use europe or
  north-america for sparse. This pair cannot separate the scanners from
  `packed_varint_count`.
- Shared versus separate input descriptors: about eight commands hand the
  `HeaderWalker`'s `FADV_RANDOM` descriptor to their pread workers
  (classify, tags-filter, extract smart and multi, getparents, geocode pass
  1.5, diff's shard walk, altw sparse passthrough), while altw external and
  apply-changes keep separate descriptors on an argument (`a3f1a1bc`) never
  measured. The split may track access pattern (selective classify readers
  with `FADV_RANDOM` plus the `WILLNEED` prefetch, against stage 4 reading
  every blob), so measure both shapes: one matched `--bench 1` A/B per side,
  for example altw external stage 4 at Europe scale with a variant that
  hands the walker's descriptor to the workers. Renumber is not on the
  shared-walker side (its one shared body descriptor is a different
  question), and `InputFront` is a probe closed before any pass.
- `#21`'s fix, when written: planet altw (and `apply-changes -l`), HEAD vs
  the fix.

**8. apply-changes.**

- `-l` on the planet `pbf.altw` base, `ff0b0a37^` vs `ff0b0a37`: the node
  boundary rides the shared 256-slot drain channel and publishes from one
  readiness check (can publish after a contiguous run of node items, and a
  late boundary up to one 25 ms timeout earlier). Probably neutral.
- Ride along on planet apply-changes: the OSC arena's `str::get` instead of
  `from_utf8` per key, role and user (`20d10610`, argued cheaper).
- Optional: what `-j 1` (accepted since `9744a9fc`) costs in wall and
  memory; nothing is known.

**9. Lower priority.**

- `build-geocode-index` on planet, `d510bbd7` vs `067be2d8`, covering both
  codec commits (`64141296`, `067be2d8`); planet baseline 347 s.
- digest's walk folded into its decode pass touched the shared fused engine
  (`c0f67d9e`, ADR-0009): `getid` or `tags-filter -R` on planet,
  `c0f67d9e^` vs `c0f67d9e`. Expect noise.
- Decode-buffer growth (comment-only commit `1f572f14`; measures existing
  behaviour). Decompressors size a fresh buffer to `raw_size` exactly and
  the inline-metadata `reserve` in `parse_and_inline_with_scratch` doubles
  it, possibly moving it, on every fresh-buffer path (diff and derive
  shards, apply-changes' streaming workers, getid, cat `--clean`, inspect,
  the sequential reader, altw's relation scan). The pooled classify and
  extract paths differ from the first account: `pool_get` reserves 4x the
  compressed size, so any blob over 1 MiB compressed gets a guess past the
  4 MiB retain cap and is dropped on return; on primary planet (about 1.6
  MB compressed per blob) those pools may retain nothing, which makes the
  886.6 MB `parse_and_inline_with_scratch` share in the planet getparents
  alloc profile (`f83c8cf1`) a shaky attribution. First read
  `DECOMPRESS_POOL_METRICS`' oversize-drop counter on a planet classify
  command; then compare a fresh-buffer path (diff shards) before and after a
  pooling or `reserve_exact` experiment. Rejected without measurement:
  `reserve_exact` in the parse (it also hits pooled first use and changes
  what the pool keeps) and a guessed headroom at decompression. A per-shard
  pool would retain up to two buffers per shard (up to 128 MiB at `-j 16`).
- The `Mutex<Receiver>` dispatch question (`reference/performance-history.md`,
  moved here from `notes/todo.md`): read `classify_unordered_desc_lock_wait_ns`
  against `classify_unordered_desc_recv_wait_ns` on a planet tags-filter or
  extract run.
- Observability check, not performance: compare `brokkr sidecar <uuid>
  --counters` name sets from an altw external Denmark run at HEAD and at
  `9dcf53a7^` (and an apply-changes and a renumber run for `13dd02aa`).

### Bisect map: unmeasured changes with no cell of their own

Each was argued neutral or below noise and judged so by the audit. They are
listed so a bisect that lands near one knows what it changed.

- Write side: `frame_in_scratch` (`1db71d8a`, `f9625b94`: the sync path's
  trim inside `compress_ns`, an oversize `compress_buf` freed before the
  write); the permit taken through `acquire_permit` before spawn
  (`1db71d8a`); `fadvise::advise` on every `posix_fadvise`, one extra match
  per call on the per-blob eviction path (`d510bbd7`); the copy loops made
  two (`bd2de78d`: an `Option` branch per `copy_file_range` call, a zeroed
  `DirectReader` buffer once per reader).
- Read side: `BlobReader::next` as `next_frame` plus `into_blob`, eviction
  before the envelope parse (`8881415e`); `OrderWatermark`'s mixed-sign
  `min_id` (`8881415e`); one `statx` per direct open (`ea17fd86`); the
  scratch borrows in the fused engine ending before the transforms
  (`c0f67d9e`); the OSC arena reads (`20d10610`).
- Counters: one struct per altw stage instead of stack-local atomics, which
  changes which counters share a cache line (`9dcf53a7`); `record_max` on
  the two production `fetch_max` sites; apply-changes' and renumber's sets
  on the same machinery (`13dd02aa`); the unordered pools' gated `try_*`
  then blocking stall helpers (`003fe717`, two clock reads and a relaxed add
  only when blocked); read metrics emitted after the scope joins, plus the
  fsync funnel (`8384fa29`).
- Pools: the accumulate pool's stop flag, one relaxed load per blob on
  extract simple, smart and multi, tags-filter's closure, getid and
  `inspect --nodes` (`95eea09b`); the unordered classify pools on one driver
  (`00ed5044`); the altw way reframes' shared block walk inside stage 4's
  `parse_block_ns` (`285df56d`).
- Commands: `apply-changes --force` reframing untouched unindexed blobs in
  the worker instead of splicing (`f703b02b`, forced path only); degrade's
  sort-preservation guard checking `pending` (`9cce24b3`); the
  `expect_used` restructurings (`11becae7`: altw stage 1's shard writers
  built all-or-nothing, the per-blob compressor caches in `write/framing.rs`,
  `geo::assemble_rings_search` argued traversal-identical, degrade's unsort
  hold); `check --ids`' extra type-order comparison (`4c254750`); input
  pinning and its adoptions (a few syscalls per pass, none per blob;
  `926d5183` onwards).

---

## What the maintainer has made clear

Standing stances, from the handoff and the 2026-10-26 rulings:

- **Performance on normal input is what matters; corrupt input is not.**
  pbfhogg is optimized for speed on well-formed protobuf data. A check that
  only refuses malformed input and costs per-byte or per-element work on a
  hot path is the wrong trade (BUG-191's width check was reverted on that
  ground; altw external's intra-blob order check was not added because a
  violation already fails the run). Such findings end as comments plus a
  reference-doc correction.
- **A silent wrong answer is worse than a refusal.** An empty user or role,
  a default metadata block, fewer nodes: refuse instead. For a broken
  internal invariant, a release-mode panic over a `debug_assert!` (a release
  fallback is the silent wrong answer).
- **Follow protobuf's specified decoding where it costs nothing**, with
  osmium as the parity reference (wrong-typed optional blob-header fields
  are skipped as unknown). Deviations are deliberate and recorded in
  DEVIATIONS.md (a repeated `Info` / `DenseInfo` is refused, since no
  producer emits it and keep-last silently lost fields).
- **Commands fail loudly and point at `cat`.** Every command refuses a
  partially indexed input except `cat` (every mode), `check --index` (a
  violation) and `inspect`'s descriptions. **`cat` is the recovery
  command**: `cat --clean user` does not resolve the user index it drops,
  so a file with a bad one can be rescued. `notes/todo.md` holds the wider
  "`cat` as the universal cleaner" and "refusals name both remedies"
  projects.
- **Inputs are regular files in every mode**; FIFOs, sockets and devices
  are refused at pinning.
- **Indexdata is believed (ADR-0013)**, including the header's
  `Sort.Type_then_ID` claim; `check --index --ids` is the admission test.
  Counts taken from claims are accepted.
- **Observability is a contract.** Counter names, units and the `WAIT_*`
  spans survive refactors; check the `counter_set!` blocks and `StallGauge`
  uses before and after. Meanings are documented on their definitions;
  `reference/performance.md` notes changes that affect reading stored
  numbers. Removing a phase may remove its markers. `_ms` counters truncate
  per sample on purpose (`Stopwatch` doc); keep the unit and recording point
  when moving timing code (`ordered_pool::Timer` exists for it).
- **`reference/performance-history.md` is a record of numbers, not of
  machinery.**
- **Do not remove ignored `direct_io` parameters.** They are deliberate
  plumbing for where the flag will reach; the drain removed some twice and
  both were put back. The flag's reach is the deferred CLI-surface redesign.
- **One engine over two.** diff keeps one block-pair engine; a slow
  one-shard case gets a streaming read, not a second engine back.
- **The record informs, it does not rule.** Most of what looks arbitrary
  was measured into shape (`POOL_SIZE`, worker caps, the walker's RANDOM
  plus WILLNEED trade), often on one axis at one scale, and Europe findings
  often do not transfer to planet. Memory moves rather than vanishing: an
  "obvious" win that removes an allocation, a copy or a pass is unproven,
  and several measured as losses (fusing altw stage 1's passes; a pipelined
  stage 4 reader holding 27 GB against 1.5 GB; direct `IdSetDense::set()`
  29x slower; the batched read pipeline). Before such a change search
  `git log`, the performance record and deleted notes for an earlier
  attempt; if one was reverted, say why the new one differs. But code,
  toolchain and models have moved, so an old failure may have been its
  execution, not its premise.
- **Commits stay granular** so `--commit` bisects land on one decision. The
  maintainer expects the drain to have regressed some commands at planet
  and expects that to be easy to recover by bisecting.
- **protohoggr is the maintainer's crate.** A defect there is fixed there,
  not worked around; pbfhogg's reader still accepts forms old files carry.
- **BUG-009** (`notes/bugs.md`) is the maintainer's own project.

## Facts learned the hard way

- `BlockBuilder` holds `Rc<str>` and is not `Send`; the ordered pool builds
  per-worker local state on the worker's own thread for this. A fresh
  `BlockBuilder` per blob in cat `--clean`, time-filter and getparents is
  deliberate and measured.
- **glibc realloc** (`research/glibc/malloc/malloc.c`): an mmapped chunk
  grows by `mremap`; an arena chunk grows in place into top or a free
  neighbour, else malloc, copy, free. Freeing an mmapped chunk raises the
  process-wide `mmap_threshold` to its size (32 MiB cap) and
  `trim_threshold` to twice that. "malloc already recycles it" is plausible,
  never guaranteed. Read the source rather than reasoning from memory.
- **`par_fold_blobs` budget rule** (`read/reader.rs`): the pump reserves a
  blob's bytes before flushing its own partial batch, so a budget below a
  full partial batch plus one maximum blob blocks forever. Compile-time
  asserted for production constants; a test shrinking the budget with more
  than one blob per batch hangs.
- **The claim-counter pools** (altw external stages 2 and 3, geocode pass
  1.5) lock a first-error mutex on every claim; stage 3's two error stores
  serve different recipients. Their only cross-thread wait worth counting is
  stage 2's slot-bucket writers.
- **apply-changes `-l` barrier**: the node boundary is a
  `DrainMessage::NodeBoundary` on the drain channel (exclusive end; zero
  means no nodes). The 25 ms `DRAIN_RECV_TIMEOUT` only wakes the drain to
  notice a published failure or cancellation. On unsorted input a node
  after a way misses publication and is refused by the output-order guard,
  not hung. Fault-plan hooks cover every barrier ordering
  (`tests/fault_apply_changes.rs`).
- **Premises drift.** Several drain entries were wrong when checked against
  the code; read the code before building on any claim here.
- **Sparring found real bugs** each time codex was asked to look past the
  proposal; ask it to flag anything. Mutation-check concurrency tests.
  `review` refuses stdin over 20,000 bytes (have codex read `git diff
  <paths>` itself), and a session idle for about an hour is refused as
  cache-cold.
- `brokkr verify all --dataset denmark` is the correctness check to run when
  a change touches command output.
- Shell here: `mv` is not allowlisted (`cp` then `rm -f`), and `rm` prompts
  without `-f`. A subagent working alongside reports your own uncommitted
  edits as someone else's.
