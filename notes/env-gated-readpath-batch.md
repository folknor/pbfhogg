# Env-gated read-path batch: plan of record (2026-07-11, rev 2)

The orchestration plan for landing the remaining read-path follow-up items as
one batch, measured in one overnight run. This document is the contract for
HOW the batch runs; per-item design content lives in the referenced documents
and, for the two large items, in dedicated specs written against
`reference/technical-implementation-spec.md`. Rev 2 folds the consolidated
findings of a dual review (codex xhigh + Fable, 2026-07-11); the biggest
corrections were miswired verdict cells that would have adjudicated code the
gates never executed, and the item-2 count-backstop semantics.

## The process deviation, stated up front

`reference/technical-implementation-spec.md` forbids env-var experiment
switches and requires each landing to carry its own keep/revert benchmark
verdict. **The user explicitly overrode both rules for this batch
(2026-07-11):**

- Every item is implemented behind an environment-variable gate,
  **default-off**: unset means today's shipped behavior, byte-identical;
  set means the new behavior. Gate-off IS the baseline, so every overnight
  A/B pair runs one binary, one commit - no `--commit` worktrees, no build
  thrash, no cross-day drift (`reference/performance.md` same-day A/B rule
  satisfied by construction).
- Correctness is proven on denmark at each landing. **No planet or europe
  gate blocks any landing.** All large-scale verdict cells are batched into
  `./overnight.sh`, which the user launches manually at night.
- The gates are scaffolding, not surface. After the morning read-out, each
  item's verdict flips the default and **deletes the gate** (keep), or
  deletes the gated code (revert). The end state has zero env vars,
  restoring the standing contract. Gate names appear only in this note,
  the code, and overnight.sh.
- The override does NOT waive: exact copy-pasteable gate commands, explicit
  `--variant` pins, or pre-registered keep/revert thresholds. Those hold
  (contract point 5) and are pinned below / in the item specs.

## Mechanics

- The harness blocks `VAR=x brokkr ...` command lines, so brick 0 is
  `scripts/envrun.sh` (`#!/bin/bash` + `exec env "$@"`), used for every
  in-session denmark gate: `scripts/envrun.sh PBFHOGG_X=1 brokkr ...`.
  brokkr must be invoked from the project root: envrun.sh never changes
  cwd, and every invocation - in-session and in overnight.sh - runs from
  the repo root.
- overnight.sh runs detached and uses `run env PBFHOGG_X=1 brokkr ...`
  directly; `run()` appends `--dry-run` to the full argv in dry-run mode,
  which composes correctly with `env`. The `set -x` trace plus the UUID(s)
  on the following line(s) map UUID -> configuration in the log.
- brokkr.toml `capture_env = ["PBFHOGG*", "MALLOC*"]`: brokkr captures
  matching env vars per run and stores them on the result row. Every gate
  is therefore named `PBFHOGG_*` (mandatory), and the morning read-out
  uses stored env metadata as primary, the log as backup. Baseline/gated
  cells stay adjacent anyway to control intra-night drift.
- **One `brokkr read` invocation stores FOUR UUIDs** (one per variant, in
  fixed order sequential/parallel/pipelined/blobreader). The read-out
  protocol pairs per-variant, fourth-to-fourth etc., never line-to-line.
- **`brokkr read` rows store no sidecar data** (the sidecar lands under
  `dirty` and each subsequent variant overwrites it - brokkr-side
  limitation, confirmed against results.db). Read-pair verdicts are
  therefore WALL-ONLY, from the stored elapsed per variant UUID.
  `brokkr sidecar --compare` is available for the command cells (getid,
  getparents, tags-filter, time-filter, altw), not for read cells. If a
  read-side RSS question survives the night, it gets an attended follow-up
  run; fixing brokkr's read sidecar is a separate brokkr-repo ask, not a
  brick here.
- **In-test gate coverage never uses `std::env::set_var`** (racy under the
  parallel test harness, unsafe in edition 2024). The pinned mechanism:
  the env var is read at exactly ONE entry point per knob and plumbed as a
  plain parameter below it; unit/equivalence tests exercise the parameter
  API directly, and integration tests that need the real env path drive
  the CLI via `CliInvoker` with the var set on the child `Command`.
- **Gate interactions:** the only supported combination is
  `PBFHOGG_BATCHED_PIPELINE=1 PBFHOGG_FUSE_TRANSFORM=1` (one overnight
  cell). All other combinations are declared unsupported for this
  experiment; the item-4 spec defines whether the rebuilt pipeline honors
  the byte knobs or ignores them (and says which in a code comment).
- Pre-registered noise floor: a planet `--bench 1` pair adjudicates KEEP
  only if the signal-cell improvement is **>= 3 %**; anything inside
  +/-3 % is "no effect" and, for the zero-confidence items (1, 5), that
  reads as revert-and-close. A regression > 3 % on any paired cell is an
  automatic revert for that item.

## Items

### 1. fadvise DONTNEED batching - gate `PBFHOGG_FADVISE_BATCH_BYTES`

- Problem: `src/read/blob.rs:686-705` issues `posix_fadvise(fd, 0, offset,
  DONTNEED)` after every blob - a cumulative ever-growing prefix, 1.45 M
  calls on the 8k planet encoding (1,453,433 blobs). Fleshed out in
  TODO.md ("Read-path follow-ups") and
  `notes/read-path-architecture-reports.md`; measured hint: bench-read
  blobreader planet-8k drifted 609.7 -> 647.6 s (`fa16ed5a` at `58743ba`
  -> `f943fd08` at `7532021`) when `from_path` enabled the eviction
  (confounded with ambient drift). In-tree precedent for batched eviction:
  altw external stage2/stage3 already batch their own DONTNEED calls.
- Change: track last-evicted offset; advise only when >= N new bytes have
  accumulated (var value = watermark in bytes; overnight value 67108864),
  advising only the delta range, final flush at EOF. Unset = today's
  per-blob cumulative call. Watermark bounds worst-case extra page-cache
  residency at N bytes by construction - no separate cache metric needed;
  the verdict axis is wall only.
- **User confidence: zero.** Measured or dead; inside the noise floor =
  revert-and-close, recorded in TODO as mispriced.
- Denmark gates: `brokkr check`;
  `scripts/envrun.sh PBFHOGG_FADVISE_BATCH_BYTES=67108864 brokkr read --dataset denmark`
  (execution smoke only - denmark cannot show the win).
- Overnight cells (planet-8k is the signal encoding; baseline cell shared
  with items 2/4, see layout):
  `run env PBFHOGG_FADVISE_BATCH_BYTES=67108864 brokkr read --snapshot 8k --dataset planet --bench 1`
- Verdict: blobreader + sequential variant walls vs the shared baseline.

### 2. Byte-aware buffer knobs - gates `PBFHOGG_READ_AHEAD_BYTES`, `PBFHOGG_DECODE_AHEAD_BYTES`, `PBFHOGG_BLOCK_QUEUE_BYTES`, `PBFHOGG_CMD_BATCH_BYTES`

- Problem: `read_ahead 16` / `decode_ahead 32` (`src/read/pipeline.rs`),
  `BLOCK_QUEUE 8` (`src/read/reader.rs`), `BATCH_SIZE 64`
  (`src/commands/mod.rs`) are count-based; memory-per-count differs ~25x
  between blob encodings. TODO.md; architecture reports.
- **Corrected semantics (rev 2):** when a var is set, the byte budget is
  the PRIMARY bound and the count backstop is RAISED to 16x today's value
  (emergency limit only - "counts as defenses" means large counts, not
  today's values; min(count, bytes) with today's counts would make gate-on
  a no-op on exactly the 8k encoding the item targets). Unset = today's
  count-only behavior, byte-identical. Byte budgets for the overnight run
  are sized to today's effective planet-PRIMARY footprint (implementer
  computes from blob sizes and records the arithmetic in a code comment),
  so primary cells read ~neutral and 8k cells admit ~25x more in-flight
  blobs - that asymmetry is the signal.
- Implementation note: `for_each_primitive_block_batch_budgeted`
  (`src/commands/mod.rs:61`) already takes `max_bytes: Option<usize>` and
  is dormant - wire it for CMD_BATCH, don't reinvent it.
- Denmark gates: `brokkr check`; equivalence tests extended to exercise
  the parameter API with tiny byte budgets (forcing the byte bound to
  bind); CLI smoke via
  `scripts/envrun.sh PBFHOGG_DECODE_AHEAD_BYTES=1048576 brokkr read --dataset denmark`.
- Overnight cells (rev 3 - the post-commit review proved two knobs INERT
  in read cells: BLOCK_QUEUE_BYTES binds only in `into_blocks_pipelined`
  and CMD_BATCH_BYTES only in command batch loops, neither of which any
  read variant calls; a combined-vars read cell would have adjudicated
  them as false-neutral):
  - Read pairs adjudicate READ_AHEAD + DECODE_AHEAD only:
    `run env PBFHOGG_READ_AHEAD_BYTES=<N1> PBFHOGG_DECODE_AHEAD_BYTES=<N2> brokkr read --dataset planet --bench 1`
    `run env <same two> brokkr read --snapshot 8k --dataset planet --bench 1`
  - CMD_BATCH_BYTES gets its own command pair (shares the item-3 baseline):
    `run env PBFHOGG_CMD_BATCH_BYTES=<N4> brokkr getid --dataset planet --snapshot 8k --add-referenced --bench 1`
  - BLOCK_QUEUE_BYTES is adjudicable (fix-run report: `into_blocks_pipelined`
    is consumed by getid, getparents, PBF tags-filter, and altw's
    decode-all fallback) and gets its own dedicated pair on the cheapest
    consumer (shares the item-3 getparents 8k baseline):
    `run env PBFHOGG_BLOCK_QUEUE_BYTES=<N3> brokkr getparents --dataset planet --snapshot 8k --bench 1`
- Verdict: pipelined variant walls for the read pairs (the parallel
  variant uses its own fixed PAR_INFLIGHT_BUDGET and is a no-regression
  control only), getid wall for the CMD_BATCH pair; primary inside the
  noise floor both directions, 8k must clear +3 % to keep.
- Interaction: the rebuild (item 4) may subsume decode_ahead/BLOCK_QUEUE;
  if both verdicts are keep, morning adjudication decides which knobs
  survive inside the new pipeline.

### 3. Command-transform fusion - gate `PBFHOGG_FUSE_TRANSFORM=1` - REQUIRES SPEC

- Problem: getid `--add-referenced` pass 2 (`src/commands/getid/mod.rs`,
  `getid_with_refs`), getparents full-scan arm
  (`src/commands/getparents/mod.rs`), tags-filter single-pass
  (`tags_filter_single_pass`, the `-R` variant), and altw decode-all
  (`write_output_decode_all` - the NO-INDEXDATA fallback, not the external
  backend) each decode in one rayon stage, materialize 64-block batches
  (~90 MB), and re-dispatch. TODO.md; architecture reports; precedent:
  `parallel_classify_phase`.
- **Path-reachability corrections (rev 2), binding on the spec:**
  - getid: plain include mode never runs pass 2, and the 8k include arm is
    deliberately sequential streaming. Every fusion cell MUST use
    `--add-referenced`.
  - getparents: ADR-0006 dispatches planet primary (~36 k blobs) to the
    walker arm; the fused arm runs only on high-blob-count encodings. The
    8k pair is the signal; the primary pair is a labeled no-regression
    control, nothing more.
  - tags-filter: default brokkr bench is the two-pass path; every fusion
    cell MUST pin `-R`.
  - altw decode-all: reached only on non-indexed input with a non-external
    index type. Its cells use `--variant raw --index-type sparse`, europe
    scale (planet non-indexed sparse is an unknown wall and OOM risk; the
    spec sizes this cell and may substitute denmark-raw if europe-raw is
    not configured).
- Spec loop: Fable authors `notes/fusion-spec.md`; codex xhigh critiques;
  codex implements. One command at a time internally, one gate. The spec
  survey MUST reconcile with the rebuild spec (item 4) - same seam -
  before either implements.
- Denmark gates (gate off AND on via `scripts/envrun.sh`): `brokkr check`;
  `brokkr verify getid-removeid --dataset denmark` (name verified live
  2026-07-11, PASS 12 s) plus a `CliInvoker`-driven equivalence test per
  fused command comparing gate-off vs gate-on output byte-for-byte
  (`--add-referenced`, `-R`, raw-sparse altw, and a forced-low-threshold
  getparents full-scan fixture).
- Overnight cells (all planet unless noted, baseline first then gated):
  - `run brokkr getid --dataset planet --add-referenced --bench 1` +
    gated twin; same pair with `--snapshot 8k`.
  - `run brokkr getparents --dataset planet --snapshot 8k --bench 1` +
    gated twin (signal); primary pair as labeled control.
  - `run brokkr tags-filter --dataset planet -R <expr> --bench 1` + gated
    twin; same pair with `--snapshot 8k`. (Exact expr pinned by the spec
    to whatever brokkr's tags-filter axis accepts for `-R`.)
  - altw decode-all pair per the spec's sizing decision (europe raw
    sparse).
- Verdict: command walls (>= 3 % on at least the 8k signal cells to keep)
  and per-phase RSS via `brokkr sidecar --compare` (the ~90 MB batch
  materialization should drop).

### 4. Ordered-pipeline batch rebuild - gate `PBFHOGG_BATCHED_PIPELINE=1` - REQUIRES SPEC

- Problem: per-blob channel/task/permit/reorder seams in `run_pipeline`
  are structural overhead at high blob count; both architecture reports
  converge on byte-bounded batches with long-lived workers. TODO.md; full
  analysis in `notes/read-path-architecture-reports.md`.
- The standing user HOLD converts, by this plan, into: rebuild ships
  env-gated default-off; the production spine is untouched until the
  overnight verdict and the user's morning decision.
- **Hard spec constraint (rev 2): gate-off must be byte-identical.** The
  rebuild is a parallel path selected at the gate; it does not refactor
  the seams the default path runs through. If the implementation cannot
  satisfy that without unreasonable duplication, the spec must say so and
  the item returns to the user - denmark-only landing gates on the
  production spine are only acceptable under this constraint.
- **Cell corrections (rev 2):** `cat --type way` is raw-frame passthrough
  (deliberately non-pipelined) and default tags-filter is two-pass -
  neither exercises `run_pipeline`. Genuine consumers: the read bench's
  pipelined variant, `time-filter` (`for_each_pipelined`),
  `build-geocode-index` pass 1, getid `--add-referenced` pass 2,
  `tags-filter -R`.
- Spec loop: Fable authors `notes/pipeline-rebuild-spec.md` FIRST
  (fusion's spec layers on it); codex xhigh critiques; codex implements.
  Spec carries the full re-verification protocol from TODO: `verify all`,
  shutdown/early-exit/ordering tests, both gate states equivalence-tested
  (parameter API + CliInvoker, per the mechanics section).
- Denmark gates: `brokkr check`;
  `scripts/envrun.sh PBFHOGG_BATCHED_PIPELINE=1 brokkr verify all --dataset denmark`;
  pipelined equivalence tests in `tests/read_paths.rs` run both states.
- Overnight cells (rev 3 - the spec survey proved the planned time-filter
  pair inert: planet primary is a snapshot PBF, so time-filter dispatches
  to `parallel_classify_phase` and never enters `run_pipeline`; no history
  dataset exists in brokkr.toml, and the history path is covered by a CLI
  byte-compare test instead):
  - Read pairs: gated read cells on planet primary, planet 8k, and europe
    (baselines shared per layout below).
  - getparents 8k gated pair (genuine ADR-0006 FullScan `run_pipeline`
    consumer; shares the item-3 baseline) plus a cheap getid 8k
    single-gate isolation cell, per the spec.
  - tags-filter `-R` pairs are shared with item 3 (the baseline and the
    single-gate cells serve both items).
  - Combination cell:
    `run env PBFHOGG_BATCHED_PIPELINE=1 PBFHOGG_FUSE_TRANSFORM=1 brokkr getid --dataset planet --snapshot 8k --add-referenced --bench 1`
    - the joint shape is the actual end-state candidate.
- Verdict: pipelined-variant read walls and command walls, >= 3 % at 8k
  and/or europe to keep, primary inside noise both directions.

### 5. Europe prefetch reclaim (WILLNEED) - gate `PBFHOGG_PREFETCH_WILLNEED=1`

- Problem: the 2026-04-20 HeaderWalker swap (walker opens with
  `POSIX_FADV_RANDOM`, `src/read/header_walker.rs:205`) gave up the
  accidental page-warming the old buffered walk provided at europe scale;
  a deliberate `POSIX_FADV_WILLNEED` over the exact `(data_offset,
  data_size)` ranges the scan schedule flags would reclaim it without the
  old walk's I/O waste. TODO.md ("Reclaim europe's lost prefetch win").
  The "~14 s" figure there is an ESTIMATE - no hash-anchored europe A/B
  baseline backs that exact number (rev-2 finding); the overnight pair IS
  the first real measurement.
- **User confidence: zero.** Same rule as item 1.
- Denmark gates: `brokkr check`; smoke via
  `scripts/envrun.sh PBFHOGG_PREFETCH_WILLNEED=1 brokkr check-refs --dataset denmark`.
- Overnight cells (europe only - planet is bigger than RAM, prefetched
  pages evict before reuse; extract dropped for budget, TODO's third
  candidate can join a follow-up night if these two clear the floor):
  `run brokkr check-refs --dataset europe --bench 1` + gated twin;
  `run brokkr tags-filter --dataset europe <default expr> --bench 1` +
  gated twin.
- Verdict: wall, >= 3 % to keep.

### 6. Measurement-only riders

- **In the overnight (last, excluded from the budget arithmetic, OOM
  risk accepted - that is the audit's purpose):** altw
  `--index-type sparse` planet bench with sidecar (TODO "Latent same-shape
  risks"): first planet RSS profile of the sparse path; morning read-out
  reads the per-phase RSS table before concluding anything (shape != root
  cause).
  `run brokkr add-locations-to-ways --dataset planet --index-type sparse --bench 1`
- **MOVED OUT of the overnight (rev 2):** the two cross-disk riders
  (apply-changes cross-disk `--io-uring` re-measurement; altw cross-disk
  scratch). Cross-disk requires flipping the commented `scratch` line in
  tracked `brokkr.toml`: an uncommitted flip dirties the tree (bench
  refuses / `--force` doesn't store), and a pre-committed flip changes
  scratch placement for every other cell in the night. They become a
  separate short attended session (flip + commit, run the rows, revert +
  commit) - added to TODO as their own item.

### 7. Dropped from the batch: sort copy_file_range coalescing

Already landed (`try_extend_copy_run` + `flush_copy_run`,
`src/commands/sort/mod.rs`; commit `244c6ec`; denmark run `11062bdd`
coalesced 7387 passthrough blobs into 3 calls). `notes/sort.md` already
records the landing; only TODO.md's sort entry ("headline opportunity"
paragraph) is stale - TODO-only reconciliation rides with this batch.
Additional doc rider (rev 2): `reference/pipelined-reader-paths.md:134`
still claims getparents doesn't use `into_blocks_pipelined` - stale since
the ADR-0006 full-scan arm; reconcile in the same pass.

## Overnight layout and budget

Ordering rules: one binary at HEAD-at-launch, fully committed before the
run; baseline cell first, its gated twins immediately after (shared
baselines: the three 8k-read gated cells share ONE 8k baseline read cell;
same for primary and europe); highest-value pairs first; the sparse-altw
rider last; `brokkr clean` + `brokkr clean --worktrees` after everything.
`./overnight.sh --dry-run` MUST pass before the file is handed over.

Honest cell count (rev 2 - the rev-1 estimate undercounted ~2.5x):

- Read cells: 8k baseline + 3 gated (items 1, 2, 4) = 4 x ~26 min;
  primary baseline + 2 gated (items 2, 4) = 3 x ~20 min; europe baseline
  + 1 gated (item 4) = 2 x ~6 min. ~2.9 h.
- Command cells: getid-addref primary pair + 8k pair (4 cells), getparents
  8k pair + primary control pair (4), tags-filter -R primary pair + 8k
  pair (4, shared items 3/4), time-filter primary pair (2), altw
  decode-all pair (2, europe-raw), combination cell (+1 vs existing 8k
  baseline), item 5 europe pairs (4). ~22 command cells, most 1-10 min,
  tags-filter/time-filter the heavy ones. ~1.7-2.3 h.
- Rider: altw sparse planet, unknown wall, last.

Total ~4.6-5.2 h before the rider - fits the night with margin for lock
contention and the rider. No `--hotpath`/`--alloc` modes; those are
follow-up nights for whichever items survive.

## Staffing and sequence

Per the user's direction: no Opus agents on this batch. codex implements;
Fable authors the two specs and reviews diffs.

1. Brick 0: `scripts/envrun.sh` (orchestrator, trivial).
2. Items 1, 2, 5 serially: codex-implement (fresh run each, prompt cites
   this note's item section + the TODO text + the pinned test mechanism),
   Fable reviews the diff, orchestrator runs denmark gates, commits (one
   commit per item, gate documented in the commit body).
3. Item 4 spec (Fable author -> codex xhigh critique -> consolidate), then
   item 3 spec (same loop, survey reconciled against item 4's spec), then
   codex implements item 4, then item 3; denmark gates + commit each.
4. overnight.sh rewritten last (orchestrator): replace the current body
   (prior campaign, already served) with this plan's cells; `--dry-run`
   gate; codex xhigh reviews the final file against this plan's cell list
   (cheap insurance against exactly the miswiring class rev 1 contained);
   hand to user.
5. Morning after: read verdict pairs (env metadata on result rows primary,
   log backup, 4-UUIDs-per-read-cell pairing), apply the pre-registered
   thresholds, write verdicts into TODO.md, flip defaults / delete gates /
   delete reverted code, delete this note, settle numbers into
   `reference/performance.md` + `reference/performance-history.md`, and
   add CHANGELOG entries for kept items with user-visible effect (per the
   CHANGELOG bar in CLAUDE.md).

Standing constraints that survive the deviation: nothing regresses the
README planet-safe table at default settings (default-off makes this
structural until adjudication); `brokkr check` green at every commit
boundary; `.brokkr/results.db` + dirty markdown ride each commit.
