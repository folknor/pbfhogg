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
The planet regressions found by the 2026-10-30 planet RAM pass are resolved:
`13ec0927` (cat --dedupe), `418739bd` (inspect --extended), `c97b9985`
(merge-changes), `5bfa37ab` (pool-heavy decode buffers) and `b1f522f7`
(apply-changes). The release itself still needs WRK-001, WRK-005 and
WRK-191.

---

## Release

### WRK-001: Version bump and CHANGELOG date

CHANGELOG.md's section is headed `## Unreleased`; retitle it
`## 0.6.0 - <date>` when it ships. Both `Cargo.toml`s still say 0.5.0, and no tag after `v0.5.0`
exists. The 0.6.0 section was rewritten to net change against v0.5.0, one
line per entry; keep it that terse.

### WRK-191: README numbers moved to bygg (planet, vs osmium, library)

Decided: README ends up with three tables, in order: a planet CLI table of
every command that runs within 28 GB, the vs-osmium table, and a new
library-mode table, all on bygg. README is already restructured that way
(at HEAD `6fdde3f4`). Every number in it is `pending`. The specifics
around the tables were reworded too: the merge-changes "55 s" paragraph,
the altw temp-file size, the host lines, and a sentence on altw's sparse
default. The work left is to run the cells and fill the tables (commit and
host on every row), then the open decisions below.

**Where things stand.** The move had not been done: the planet table was
the plantasjen one (5900X), and the osmium table (`b355badd`, bygg) was
stale. No stored bygg cell is at or after `6fdde3f4`. So by the rule
(fresh cell when the stored one predates a change that moved the command,
the reindex, `25172fc3`, or `6fdde3f4`), every row needs a fresh cell. The
2026-10-30 RAM-pass draft column (`627883d7`, pre-cap, pre-reindex input)
and the `179f1b7e` cells for the formerly unstored rows are stale too.
Planet never opens a header-walk prefetch episode (`69bb00b9` /
`7766bd50`, 6.8 s both), so if the maintainer waives `6fdde3f4` for planet
only, three cells stand: altw external `6bf4f504` (`098c89cb`, 276.4 s,
11.6 GB, first after a trim), apply-changes zstd:1 `87e8b13c` (`eda3b670`,
94.3 s, 1.90 GB, predecessor state unknown) and apply-changes `-l`
`41ba96ca` (`eda3b670`, 262.2 s, 2.82 GB). Merge-changes 7-OSC
`16d34181` (`6a9807d9`, 3.7 s, 137 MB) is fine on the evidence (OSC
input), but it is cheap, so rerun it. Everything else fails a second test.

**Row verdicts** (reasons: A pre-cap build, R pre-reindex input
`planet-20260223-with-indexdata`, C a later command-moving commit, W
taken before the classify WILLNEED fix; worktree `--commit` cells carry the old commit's code):

- geocode `82d91ad9` (C: format 5 landed after this baseline).
- cat `dc7ccb05` / `907f94f4` (C: `b58e8ef3`, `f49dd460`).
- cat --clean `55c10efb` (R, C, W).
- cat --dedupe `33e06a0f` (C: `5432aac7`, `6fdde3f4` in the blob-merge engine).
- cat --type way `3d4a36a8` (A, R).
- check --ids `39471fc0` (R, W).
- check --ids --full `bf61b8f6` (C: `4109d1c4`, 2.28 to 0.45 GB).
- check --refs `7592610c` (R, W).
- diff text `5e782b56` (A, R, C).
- diff osc `d17d94db` (A; `307982d7` is the pre-spill engine, 255 s
  against 156 s after it, not a regression).
- extract complete `80089cbc`, smart `05bbc210` (C: `5903b4cd` cut peaks
  to 2.76 / 2.87 GB, W).
- extract simple `2bc9cc4a` (C, W).
- getid `67d66e4e`, getid --invert `7ee6c776`, inspect `3ec07278` (A, R).
- getparents `4ca09dc3` (R, C, W).
- inspect --extended `5b8e6c29` (A, R).
- inspect --nodes `c4ad8414` (R, C: projection).
- inspect tags `97bd775a` (pairs; keys since `9c4d6c81`, 0.40 GB).
- merge-changes 1-OSC `9d1e9170` (A).
- multi-extract simple `a89f3c1e` (C: shared membership, 9.8 to 3.9 GB, W).
- multi-extract smart `d4d8d0dc` (A, R, C, W).
- renumber `cd477c21` (R).
- sort `15393169` (R, C: `5432aac7`).
- tags-filter two-pass `9406cf2a` (C, W).
- tags-filter -i `9bf96ab2` (A, R, W).
- tags-filter -R `55f84dee` (A, R).
- time-filter `bb2517a3` (R, W).

**Memory-safe under 28 GB.** Every README row qualifies on measured
peaks. The highest are geocode 13.45 GB, altw external 11.6 GB, and
`tags-filter -i` and multi-extract smart around 7 GB (old cells). Others
qualify but are not in the table: check --locations (about 18 GB; the
store follows available memory), merge-changes --simplify (12.0 GB),
inspect tags `'*=*'` (10.8 GB), repack and degrade (about 0.8 GB), digest
(1.42 GB RSS), check --index (334 MB), getid --add-referenced (about
0.46 GB), the base-against-apply-output diff (1.81 GB) and tags-filter
--input-kind osc (131 MB). altw with its default `sparse` at planet does
not qualify; it was not measured on bygg and is expected to thrash. Not
measured at planet: unsorted sort, dedupe of two different planets, and
`degrade --unsort`.

**Classify WILLNEED fix exposure.** The classify pools (every
`parallel_classify_phase`, the unordered per-blob driver, the ordered
classify consumer) cover extract (all three), multi-extract, getparents,
time-filter, cat --clean, tags-filter two-pass and -i, check --refs and
--ids, repack and degrade. inspect --nodes / --extended and getid --invert
are possibly exposed. The fix moved planet check --refs 31 to 26 s and
left cat --clean unchanged (reference/performance.md "Classify WILLNEED
in honoured calls"); the rest are unmeasured.

**vs-osmium method** (`110f0520`): script bench `vs-osmium`, denmark
`pbf.indexed` read by both tools, `osc.4705`, osmium 1.19.1. The preflight
evicts each arm's input and fincore-checks 0 bytes resident. osmium gets
`--fsync` and `--keep-member-nodes`. Per row: three alternating
single-iteration pairs, medians compared (`scripts/vs-osmium.sh 3`, 63
invocations, 10-15 min). It needs a fresh campaign: `f20b2543` and
`6fdde3f4` changed exactly the cold walk these rows pay (the prefetch
fires on Denmark). README's old inspect-tags-way row was a dirty-tree
number in a `b355badd` table.

**Library table.** `brokkr library` builds `pbfhogg-harness` and runs its
13 scenarios. `--bench N` stores a scenario row and per-call rows. Only
`isolation-cat` times library against CLI: arms interleaved, inputs evicted
and mincore-checked, CLI timed by its `CLI_COMMAND_*` interval, median
paired ratio printed. The others give library walls only; most are
correctness probes. Calls run on Keep with fixed parallelism, the CLI on
Evict with probed sizing; say so in the caption. No library row is stored
yet.

**Run list.** It is queued in `overnight.sh` (gitignored), cells 1-40:
1. One trim, then the read-only planet block: inspect and getid
   (`--bench 3`, quote iteration 1 as cold), check-refs, check-ids,
   `--full`, `--extended`, `--nodes`, `--tags`, getparents, getid
   `--invert`, merge-changes 4913..4913 and 4914..4920 (`--bench 3`),
   tags-filter -R highway=primary, tags-filter, extract x3
   (`-25,34,45,72`), multi-extract simple and smart (`-10.0,35.0,32.0,72.0`),
   geocode, diff-snapshots text and osc (`-j 16`, base against `20260411`).
2. Then `sudo fstrim -av` before each of: cat, cat --type way, sort,
   cat --clean, time-filter, renumber, tags-filter -i, cat --dedupe,
   apply-changes zstd:1, apply-changes `-l` keep-untagged, altw external.
   Between the two blocks, WRK-005's answer runs (no README row):
   check-refs, check-ids and check-index on every regional dataset
   (denmark, germany, japan, north-america, norway, switzerland, malta,
   greater-london, europe), then check-ids on denmark `unsorted` and
   check-index on denmark `test-no-index`. Read each exit as WRK-005's
   declaration input.
3. Then the 63 vs-osmium lines.
4. Then library: isolation-cat denmark `--bench 5` and planet `--bench 3`,
   repeat-cat, concurrent-cat, fixed-resources-executor (`--bench 3`), and
   `--all --dataset denmark`.

About 80 min planet plus trims, 15 min osmium, 12 min library. It needs a
clean tree. `brokkr bench` and `brokkr library` take no `--dry-run`
(`run_nodry` lists them unvalidated). Optional planet rows:
check-locations, repack, degrade --strip-locations, check-index, digest,
merge-changes --simplify.

**Script benches beside it** (no README table; all predate `6fdde3f4`;
`--iterations 1`, three invocations per condition, after the planet suite):
- dedupe-pair `--with one-relation`, cold x3, warm x2: `6fdde3f4` is the
  dirty variant measured at `bd2408d5`, so there is no clean cell.
- dedupe-self, denmark and `--dataset japan`, cold and warm x3, plus one
  `--cache warm --check yes`: `5432aac7` and `6fdde3f4` changed the engine.
- sort-cold `--dataset europe --force yes` x3 after a trim: `5432aac7`
  was only ever an uncommitted variant, and europe is dense enough for the
  prefetch. Add typed `brokkr sort --dataset europe --bench 1` x2.
- diff-unindexed `--new 20260420`, all and `--type way`, x3:
  `b58e8ef3` is on its read path.
- diff-text denmark `--new 320k` x3, and germany `--new 20260420 --verbose
  yes --common no` x3: `3286a9c5` landed with no clean cell.

Param spellings are not validated. `brokkr bench NAME --help` lists them.

**Before the run, in order:**
1. Done: the classify read-advice fix (WILLNEED in honoured calls) has
   landed, so every row marked W predates it and is rerun.
2. Tree clean outside `*.md` and `.brokkr/results.db`: brokkr refuses a
   dirty tree, and `--force` cells are not stored. `overnight.sh` is
   gitignored and does not count. Never `git stash` with a dirty
   `results.db`; stashes have dropped stored cells.
3. Settle the `20260411` decision below. To reindex: `cat --reindex` the
   snapshot into a new file, point `snapshot.20260411.pbf.indexed` in
   `brokkr.d/hosts/bygg.toml` at it, and take its xxhash from
   `brokkr env`. Without that, cells 21 and 22 (the two diff rows) read
   the old-indexdata file against the reindexed base.
4. `brokkr --version`: a stale installed brokkr is the usual cause of
   brokkr misbehaving; `brokkr install` from `~/Programs/brokkr` fixes it.
5. `./overnight.sh --dry-run` from the project root. The file has never
   been syntax-checked (`bash -n` was refused in the tool shell).
6. Free scratch space: an altw external planet cell writes about 360 GB
   of scratch (shards alone 99.5 GB), and a trim after two untrimmed
   cells freed 828 GiB and took over two minutes.
7. memlock is not needed: no cell uses `--io-uring`. If one is added,
   first run `! sudo prlimit --pid $PPID --memlock=unlimited:unlimited`
   (RLIMIT_MEMLOCK is 8 MB on bygg; `--io-uring` refuses otherwise).

`sudo fstrim -av` runs without a password (allowed 2026-11-09), so the
script's trims need no one at the keyboard. The read-only block starts
right after a trim so its first header walk is cold, as a user's would
be.

**After the run:**
1. `brokkr history --failed --since <date>` for any cell that did not
   complete; rerun those alone.
2. Per cell, `brokkr results <uuid>`: read per-iteration walls, not the
   best. For inspect and getid (`--bench 3`), iteration 1 is the cold
   figure to publish; later iterations hit cached headers (cold about 6 s,
   warm 0.1-0.2 s at planet). Peak anon from `brokkr sidecar <uuid>
   --stat anon`.
3. Before calling any row worse than its old figure, read `brokkr sidecar
   --compare` (host annotations first) and the phase split.
4. vs-osmium: medians of the three pairs per row. Library: `brokkr
   results <uuid>` on the isolation-cat rows prints the CLI/library
   ratio after the rows.
5. Fill README's three tables, commit and host on every row; record the
   cells in `reference/performance.md`; then run the script benches
   below; then work the open decisions.

**Open decisions for the maintainer:**
- altw defaults to `--index-type sparse` (`cli/src/lib.rs`), which
  undercuts the "every command on a 32 GB machine" headline. Make `auto`
  the default, or keep the README sentence.
- bygg's `snapshot.20260411.pbf.indexed` is still
  `planet-20260411-with-indexdata` (`7facb638...`), never reindexed. Both
  diff rows read it. Reindex and re-register first if that disqualifies
  them.
- The README read-throughput table (Denmark, `90df51f`, plantasjen,
  osmpbf / Planetiler rows) is a fourth table, and brokkr cannot
  reproduce its competitor rows. Drop it, or keep it labelled plantasjen.
- vs-osmium has no extract row, so README's Tokyo extract line was
  dropped. Add a row to `benches/compare/vs-osmium.lua` if it should
  come back.
- Which optional rows join the planet table (the list above, and inspect
  tags `'*=*'`).
- Stale figures under docs/: the guide's osmium, extract and apply-changes
  tables and its System line, `docs/guide/indexdata.md` (planet cat
  86.5 s, `aee7727`), and `docs/index.md` (0.31 s / 12 s).
- `brokkr man library isolation-scenarios` still lists "the capped malloc
  arenas" in the CLI interval (gone since `25172fc3`). That is a brokkr
  doc fix.

This item stands alone. The agent's working copy at
`.plans/readme-refresh.md` (gitignored) adds the per-row stored walls and
peaks, but nothing the run needs.

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

## The measurement campaign

Matched A/B cells planned at the end of the hygiene drain to bisect what the
drain changed. Today's regression fixes did not need them, but they remain
the way to find any regression the planet cells cannot see. "A vs B" means
matched, interleaved `--bench 1` cells with `--commit`; when a pair moves,
bisect inside it. Prefer stored cells where they already answer the
question.

### WRK-111: altw external pass A exposure to sustained-write state

Residue. A short trim before a cell restores pass A (about 23 s against
36-41 s without one) and launch-time Dirty/Writeback is negligible
(reference/performance.md, "pass A after a trim or after another cell",
`098c89cb`); exposure scales with scratch bytes ("Packed id-shard
records"). Left: an idle-only rest cell and no-trim gaps matched to the
trim's duration (the harness refused `sleep`, so neither ran), so trim
is not isolated from elapsed time; in-pass dirty throttling and
device-side state need per-cell device stats (brokkr#74). Shared
per-bucket shard files were built and refused (pass A +27 % trimmed,
+55 % untrimmed, all in append-lock waits; reference/performance.md,
"Shared per-bucket id shards, refused"). No implementation lever on the
exposure is left in hand: the unbuilt shapes there (a bounded descriptor
cache over private files, striped buckets) bound descriptors, not write
exposure.

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
  total emitted after every pool run. The desc lock wait includes peers
  queued behind a holder blocked in its receive (reference/
  performance-history.md, "Mutex<Receiver> blob dispatch is not a
  bottleneck", which records the planet reading).
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
  every `posix_fadvise` (`d510bbd7`); the copy loops made two (`bd2de78d`);
  the `307982d7..52690f1e` writer cluster on the `pwrite` pool and io_uring
  backends and on borrowed `write_primitive_block` calls (the buffered
  backend measured neutral, reference/performance.md "Writer cluster
  across six commits").
- Read side: `BlobReader::next` as `next_frame` plus `into_blob`, eviction
  before the envelope parse (`8881415e`); `OrderWatermark`'s mixed-sign
  `min_id` (`8881415e`); one `statx` per direct open (`ea17fd86`); the OSC arena reads
  (`20d10610`); `BlobReader::leading_header` judging the first frame before
  its body on every `ElementReader` (`418739bd`).
- Counters: one struct per altw stage (`9dcf53a7`); `record_max` on the two
  production `fetch_max` sites; apply-changes' and renumber's sets
  (`13dd02aa`); the unordered pools' gated stall helpers (`003fe717`); read
  metrics emitted after the scope joins, plus the fsync funnel (`8384fa29`).
  The `9dcf53a7` and `13dd02aa` name sets are checked (reference/
  performance.md, "Counter names across the counter-machinery commits");
  their wall cost is not.
- Pools: the accumulate pool's stop flag (`95eea09b`); the unordered
  classify pools on one driver (`00ed5044`); the altw way reframes' shared
  block walk (`285df56d`).
- Commands: the `apply-changes -l` node boundary on the drain channel
  (`ff0b0a37`; reference/performance.md, "Node boundary on the drain
  channel, not measured"); `apply-changes --force` reframing untouched unindexed blobs in
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
  keep the unit and recording point when moving timing code. The durable
  statement is the `src/counters.rs` module doc; check a refactor with
  `scripts/counter_names_diff.py`.
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
