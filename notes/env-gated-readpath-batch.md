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
- **`brokkr read` sidecar: FIXED in brokkr `ab1762e` (2026-07-11).** The
  old limitation (sidecar landed under `dirty`, each variant clobbering
  the last) is gone for fresh runs: read rows now carry sidecar data
  under their real UUIDs, so `brokkr sidecar <uuid>` and `--compare`
  adjudicate read pairs directly. Wall remains the primary verdict axis;
  per-phase RSS is now available as supporting evidence. UUIDs stored
  before the fix stay empty - only overnight-run rows (all fresh) get
  sidecar data.
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
    sparse). The brokkr `--force-altw` brick this pair needs LANDED
    2026-07-11 (brokkr `5f6ce56`), so the pair is unconditional; the
    spec's pre-flight argv check stays as cheap insurance.
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
  `run brokkr check-refs --dataset europe --variant indexed --bench 3`
  + gated twin;
  `run brokkr tags-filter --dataset europe --variant indexed --filter w/highway=primary --bench 3`
  + gated twin. (Registered revision 2026-07-12, pre-launch: these
  four cells moved from bench 1 to bench 3 to align with the
  batch-wide command-cell rule - best-of-three costs ~11 extra
  minutes at europe walls and removes single-shot noise from a
  zero-confidence verdict.)
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

Total (final wiring, post spec rev 3 and the fusion spec's bench-3
command repetitions, per the pre-launch review): roughly 5-7 h before
the rider - the europe-raw altw pair alone is priced at 36-75 min per
the fusion spec. The sparse-planet rider has no baseline and accepts
OOM risk, so the complete script is NOT bounded to the night; it is
deliberately last so an out-of-time or OOM rider costs nothing. No
`--hotpath`/`--alloc` modes; those are follow-up nights for whichever
items survive.

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

## Read-out results (2026-07-12)

Overnight run completed on plantasjen (23 GB RAM host, commit `a65cecc`):
started 05:35, ended 11:33, 5h58m, 37 commands. Log
`overnight-logs/plantasjen-20260712-053514.log`. All walls read from
`.brokkr/results.db`; pairing by `capture_env` metadata + `cli_args`
mode, never by position. Read invocations store four UUIDs in
completion order sequential/parallel/pipelined/blobreader (confirmed:
`57999b93`=sequential, `8a0119f2`=pipelined, both in `cli_args --mode`).

### Failures (no verdict-bearing data)

- **Item 3 altw europe-raw signal pair (RUN 29 + 30): both OOM-killed.**
  `pbfhogg killed by signal` on europe-raw sparse decode-all + full
  re-encode. The `--force-altw` brick worked (argv accepted, child ran
  then died mid-execution); this is anon-memory exhaustion on a 23 GB
  host, not a missing flag. Item 3's verdict therefore reads from its
  three surviving signal cells (getid-8k, getparents-8k, tags-filter-R
  8k), per the fusion spec's own brick-4 fallback. altw fusion stays
  correctness-proven (the CLI byte-compare test) but performance-
  unmeasured; a follow-up night on a bigger host owes the pair.
- **Item 6 rider (RUN 35): operator-killed** ~72 min into ALTW_PASS2
  (soft failure, not OOM - findings recorded in TODO.md "Latent
  same-shape risks"). Measurement-only, no verdict.

### Cell validity (execution-proof counters)

All gated signal cells passed the counter gate; a missing counter would
have read as INVALID, not neutral:

- FUSE cells `895184ee`/`f461f307`/`896b8ffc` (getid/getparents/
  tags-filter 8k): `fuse_transform_active=1` + `fuse_transform_blocks`
  climbing. VALID.
- FUSE getparents-primary `21ed8d7c` (inert control): NEITHER counter
  present, `walk_estimated_blobs=36063` -> walker arm, fusion never
  executed. Correctly inert.
- BATCHED cells `53a9e76a`/`46c1d50f`/`bc949ad7`: `pipeline_batches`
  present (29442/30977/29179). Engine dispatched.
- Combination `f1d76362`: BOTH `fuse_transform_active=1` AND
  `pipeline_batches=43564`. Genuine both-gates path.
- Output parity exact across every getid-8k arm (nodes=79, ways=3,
  relations=3): baseline, FUSE, BATCHED, BATCHED+FUSE all identical.
- Inert control drift +0.39% (< +/-3%): the night is VALID.

### Verdict table (Delta = gated vs same-night baseline; negative = faster)

| Item | Gate | Signal | Verdict |
|---|---|---|---|
| 1 fadvise DONTNEED batching | `PBFHOGG_FADVISE_BATCH_BYTES` | 8k seq +0.75%, blob +1.06% (all modes slower, all < 3%) | REVERT (mispriced) |
| 2 byte-aware buffer knobs | `PBFHOGG_READ_AHEAD/DECODE_AHEAD/BLOCK_QUEUE/CMD_BATCH_BYTES` | 8k pipe +3.13%, CMD_BATCH getid-8k +3.49% (regress); BLOCK_QUEUE +0.16% | REVERT (all four) |
| 3 command-transform fusion | `PBFHOGG_FUSE_TRANSFORM` | getid-8k -7.68%, getparents-8k -6.51%, tags-filter-R 8k -6.97% | KEEP |
| 4 ordered batched pipeline | `PBFHOGG_BATCHED_PIPELINE` | getparents-8k -6.67% (redundant); combination getid-8k +9.30% (regress) | REVERT |
| 5 europe prefetch WILLNEED | `PBFHOGG_PREFETCH_WILLNEED` | check-refs -6.16%, tags-filter -5.66% (both europe, corroborating) | KEEP |

Per-item detail:

- **Item 1** planet-8k bench-1 (base `57999b93.` / gated `5e108d35.`):
  seq 575.3->579.6s (+0.75%), blob 627.8->634.5s (+1.06%), par
  72.5->73.9s (+1.92%), pipe 266.2->273.4s (+2.68%). Every mode mildly
  slower, all inside +/-3%. Zero-confidence item inside the noise floor
  -> revert-and-close, mispriced (as predicted).
- **Item 2** read knobs (base `57999b93.`/`dbd205fd.`): 8k pipelined
  266.2->274.6s (+3.13%, `6021a581`), primary pipelined 254.1->257.9s
  (+1.51%); parallel controls +0.52%/-0.71%. CMD_BATCH getid-8k
  197.9->204.8s (+3.49%, `414a4501`). BLOCK_QUEUE getparents-8k
  63.0->63.1s (+0.16%, `98802fba`). No knob improves anything; two
  regress just past the floor. Revert all.
- **Item 3** (bench-3): getid-8k 197.9->182.7s (-7.68%, `895184ee`),
  getparents-8k 63.0->58.9s (-6.51%, `f461f307`), tags-filter-R 8k
  45.9->42.7s (-6.97%, `896b8ffc`). Executing controls also improved:
  getid-primary 96.3->83.9s (-12.88%, `35c57e36`) with GETID_PASS2 peak
  RSS 1.18 GB -> 596 MB (-50%, batch materialization gone) and pass-2
  wall -17% at HIGHER core occupancy (16.0->18.5); tags-filter-R primary
  52.8->49.5s (-6.25%, `6eb2a965`). getparents-primary inert control
  +0.39% (`21ed8d7c`). Clean keep.
- **Item 4** (bench-3 command, bench-1 read): read pipelined 8k
  +2.10% (`bc949ad7`), primary +1.59% (`ede99bd2`), europe -1.40%
  (`80df3af0`) - all within noise. getparents-8k 63.0->58.8s (-6.67%,
  `53a9e76a`) is the ONE floor-clearing win, but it is REDUNDANT: fusion
  delivers the identical win on the same cell (-6.51%), and in the keep-
  fusion end state getparents-FullScan runs the fused arm anyway. getid-8k
  isolation +0.05% (`46c1d50f`), tags-filter-R 8k -0.65% (`d2c1e77b`),
  tags-filter-R primary -2.84% (`7a8bbdae`) - neutral. Combination
  BATCHED+FUSE getid-8k 197.9->216.3s (+9.30%, `f1d76362`): the end-state
  candidate REGRESSES (fused batched worker runs deeper - reorder
  high-water 32 vs 14, 43564 batches vs 30977). Sharper still: the
  combination is +18.4% slower than FUSION ALONE (216.3 vs 182.7s) -
  and fusion-alone is what we ship, so that is the figure that kills the
  end-state candidate. Auto-revert on the >3% combination regression,
  and the engineering substance agrees: batching buys nothing on top of
  fusion and hurts when combined.
- **Item 5** (bench-3): check-refs europe 56.8->53.3s (-6.16%,
  `7d114432`), tags-filter europe 61.8->58.3s (-5.66%, `c8681230`). Both
  clear +3%, same direction, mutually corroborating. The "~14 s" estimate
  was unbacked; the real measurement is a solid ~6% on both consumers. A
  zero-confidence item that earned its keep.

### Resolution: four-state matrix -> STATE 3 (revert batching, keep fusion)

Items 3 and 4 share the `run_pipeline` seam. Fusion KEEPs, batching
REVERTs -> State 3 of the shared matrix (pipeline-rebuild-spec section 5,
fusion-spec section 5). Ordering: fusion's KEEP edits first (its default-
engine arm becomes the only arm; its both-gates arm dies with
`batched_pipeline.rs`), then batching's REVERT edits (delete the module +
dispatch arms). End state: `pipeline.rs` carries two ordered engines -
plain (read bench, geocode pass 1, time-filter history) and fused (the
four commands). Headline: fusion is the mechanism that delivers the high-
blob-count command wins; the batched engine buys nothing on top of it and
regresses the combination.

### Execution checklist (next phase)

1. Fusion KEEP (fusion-spec section 5, state-3 half): flip the four
   commands to the fused arm unconditionally, delete `fuse_transform_from_env`
   + the `fused: bool` plumbing + `fuse_transform_active`, keep
   `run_pipeline_fused` (state 3 retains the default fused engine), delete
   the batch machinery (`for_each_primitive_block_batch*`, `BATCH_COUNT_BACKSTOP`,
   move `BATCH_SIZE` to `extract/simple.rs`), collapse `for_each_fused_block`
   to the bare `run_pipeline_fused` call, author `decisions/0009-fused-command-transforms.md`.
2. Batching REVERT (pipeline-rebuild-spec section 5, revert half): delete
   `src/read/batched_pipeline.rs`, the two dispatch arms, the `batched`
   config field + `bool_gate_from_env` gate read, the `#[doc(hidden)]`
   builder, `tests/fault_batched_pipeline.rs`, `tests/cli_batched_gate.rs`,
   the `batched_*` tests. Keep CliInvoker `.env()` and getparents
   `--full-scan-min-blobs` (durable instruments).
3. Items 1, 2 REVERT: delete the fadvise watermark gate + code, the four
   byte-knob gates + code (subject to the CMD_BATCH_BYTES / BLOCK_QUEUE_BYTES
   consumers surviving only if fusion's keep does not already delete them).
4. Item 5 KEEP: flip `PBFHOGG_PREFETCH_WILLNEED` to default-on, delete the
   gate, CHANGELOG the europe ~6% win.
5. TODO.md verdicts; settle kept numbers into `reference/performance.md`
   (new current) + this batch's arc into `reference/performance-history.md`;
   CHANGELOG for the two keeps (fusion command headlines, europe prefetch);
   delete this note; reconcile the doc riders (plan item 7).
