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
clean tree. Every line, `brokkr bench` and `brokkr library` included,
goes through `run`, so `./overnight.sh --dry-run` validates all of them
(brokkr#134, installed 2026-11-09): bench params are checked against
each param's declared values and the bench's `validate` hook, without a
build or the lock. `brokkr library --dry-run` builds the harness to learn
its scenarios and has never run end to end before tonight. Optional
planet rows:
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

Dry-run these lines too (`brokkr bench --dry-run ...`); `brokkr bench
NAME --help` lists each bench's params. dedupe-self, dedupe-pair,
vs-osmium and diff-text are v2 since brokkr#134: cache warming and
eviction moved from `preflight` into `bench()`, so their iteration wall
now includes that preparation. Compare on the child walls, which are
unchanged; v1 iteration walls do not compare with v2.

**Before the run, in order:**
1. Done: the classify read-advice fix (WILLNEED in honoured calls) has
   landed, so every row marked W predates it and is rerun.
2. Tree clean outside `*.md` and `.brokkr/results.db`: brokkr refuses a
   dirty tree, and `--force` cells are not stored. `overnight.sh` is
   gitignored and does not count. Never `git stash` with a dirty
   `results.db`; stashes have dropped stored cells.
3. Done: the `20260411` snapshot needs no reindex. `cat --reindex` at
   `f0b7728b` (2026-11-09, bygg) over `planet-20260411-with-indexdata`
   reported 51,274 blobs with indexdata changed in 0 and tagdata changed
   in 0. The copy was the same size and first differed at byte 134 (where
   it stops differing was not checked; the reindex rewrites no payload),
   so it was discarded and the registered file (`7facb638...`) stands.
   Cells 21 and 22 read true indexdata on both sides.
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
