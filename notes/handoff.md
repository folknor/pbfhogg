# Handoff: draining notes/hygiene.md and notes/bugs.md

Written 2026-10-26 at the end of a session that drained BUG-191 and
HYG-053 and moved HYG-037 / HYG-109 out. Read this before taking the next
entry. It records what the maintainer said and what the previous session
learned that neither drain file states. Delete it when the drain is done.

## What the maintainer has made clear

- **Every open entry is owed a resolution now.** "Skipped for now" in a
  Maintainer paragraph only means the 2026-10-25 session did not take it.
  Those paragraphs are guidance on how to resolve the entry, not rulings that
  it stays. The triage paragraph in hygiene.md says so. BUG-009 is the
  maintainer's own project: leave it alone.
- **pbfhogg is optimized for speed on well-formed protobuf data, not for
  adversarial input validation.** A check that only refuses malformed input
  and costs per-byte or per-element work on a hot read path is the wrong
  trade. BUG-191 was implemented that way first and then reverted on this
  ground: the narrowing it guarded against is protobuf's own specified
  decoding. Prefer ending such entries as comments plus a reference-doc
  correction (rule 3), especially high-anger ones.
- **protohoggr is the maintainer's own crate** (`~/Programs/protohoggr`). Read-side narrowing there follows protobuf and is
  correct. The encoder bug found here (`zigzag_encode_32` sign-extended to
  ten bytes) was fixed there in 0.5.0. A defect in protohoggr is
  fixed in protohoggr, not worked around in pbfhogg; pbfhogg's reader must
  still accept forms that old files carry.
- **The CLI surface redesign is deferred past 0.6.0.** HYG-037 (`--direct-io`
  reach) and HYG-109 (`--json`) moved into `notes/todo.md`'s "CLI surface
  redesign" section with the output-contract audit and the UX decision pair.
  Do not pick them up in this drain.
- `TODO.md` now lives at `notes/todo.md`. Nothing durable (code comments,
  `reference/`, `docs/`, `decisions/`, tests) may cite anything under
  `notes/`; `scripts/check_doc_links.py` enforces it. Durable text carries its
  context inline.
- **HYG-182 is open like every other entry.** Its Maintainer paragraph once
  read as "dedupe's indexing is not to be touched in this drain"; the
  maintainer clarified (2026-10-26) that this only meant the 2026-10-25
  session would not take it. Changing dedupe's indexing is in scope.
- **pbfhogg is on protohoggr 0.5.0** (2026-10-26, bz `pbfhogg#15`). Its
  encoder fix changed pbfhogg's output bytes: `BlockBuilder` output shrinks
  for dense uid / user_sid deltas of magnitude `2^30` or more. The tests
  (`dense_node_deltas_decode_at_signed_boundaries` among them) and
  `brokkr verify all --dataset denmark` passed on the bump. The reader keeps
  accepting the old ten-byte form (`reference/truncation-handling.md`).
- The maintainer asked for commits once a slice is verified ("commit when
  satisfied"); each resolved entry got its own commit, with the drain notes
  bundled into it.

## Read the performance record first

Before taking an entry, read `reference/performance.md` and
`reference/performance-history.md` in full. The maintainer asked for this
explicitly: they set the tone for the drain. The code is not an accident;
most of what looks arbitrary or wasteful was measured into its current
shape. Takeaways:

- **Memory moves, it does not vanish.** In the maintainer's words, an
  "obvious" memory target (removing a `Vec` per blob, pooling buffers) has
  been found more times than they can count, and when measured the only
  effect was that memory moved somewhere in the pipeline nobody predicted
  and made things worse. The record has the cases: the extract planet
  blocker was a cold-arena-page residency cascade, not the mechanism a
  ten-reviewer review agreed on; per-worker accumulation is not planet-safe
  on dense paths; the descriptor-first apply-changes flip doubled peak RSS
  for a 6 % wall gain. Peaks in different stages can start to coincide.
  Do not argue a change as a win because it removes an allocation, a copy
  or a pass.
- **The obvious win often measured as a loss.** Direct `IdSetDense::set()`
  cut allocation 99.8 % and made node classify 29x slower. The batched
  read-pipeline rebuild, the "high-conviction" rewrite of two architecture
  reports, regressed getid by 9 % combined with fusion; per-blob seams
  cost less than the batch coordination that replaced them. Byte-aware
  buffer knobs improved nothing. Check the do-not-reattempt entries before
  proposing anything near them.
- **Constants were measured, often on one axis at one scale.**
  `POOL_SIZE=16` came from a sweep; worker caps, the arena cap, the
  `HeaderWalker` RANDOM plus WILLNEED trade, the sparse store's page-cache
  budget all carry history. Europe findings frequently do not transfer to
  planet (the zstd:1 streaming ceiling, the walker's pass-1 split by blob
  density). "This value looks arbitrary" is almost never true.
- **Measurements on this host are fragile.** Drive trim debt, header-cache
  state and available RAM have explained more cross-run deltas than code
  has (the same binary spanned 35 to 66 s in one session). This is part of
  why rule 8 holds: the drain makes no performance claims. A judgement
  call on a measured path is recorded as unmeasured on "Review once
  drained", never argued as neutral or a win.
- **The record informs, it does not rule.** An entry ends as a comment
  on historical grounds only when that specific thing was tried before
  and measured badly. The code, Rust, the toolchain and the
  infrastructure have moved a lot since most of these results were taken,
  so an old number may not describe today's implementation. Most of the
  project's history was also done with much older models (Opus 4.5 and
  older Codex models); the current ones (Opus 5.5, Codex Sol 6.1) are much
  more capable, so an idea that failed in an older attempt may have failed
  on its execution, not its premise. Read the record to know what was
  tried and why it failed, then judge the entry on its merits.
- **Why measurement is deferred, and why commits are granular.** The
  maintainer expects the drain to regress a large share of commands at
  planet scale, and expects that to be easy to recover: brokkr's
  `--commit` makes bisecting a performance regression straightforward
  (slow, but this host is dedicated to the work). That only works if each
  change is its own commit, named on "Review once drained" when it is a
  judgement call on a measured path. Keep commits granular: one resolved
  entry, or one separable step of one, per commit, so a bisect lands on a
  single decision.

## Suggested order

No findings remain in `notes/hygiene.md`; `notes/bugs.md` holds only
BUG-009 (the maintainer's). HYG-036 (the clippy exemptions) moved to
`notes/todo.md` ("Lint exemptions") on 2026-10-26: the maintainer wants it
resolved, but not in this drain. HYG-182 was resolved 2026-10-26 (see
below).

1. **The drain is done** (rule 4): review "Review once drained" before
   deleting `hygiene.md`, then the measurement campaign rule 8 has held back.
   Bisect candidates most likely to matter at planet, by how much hot path
   they touch: the unified frame decoder (`307982d7`), every ordered pool
   on one engine (`56a50e0d`), the `PbfWriter` per-block dispatch, the
   diff and derive one-engine merge, cat `--clean` on the ordered pool, and
   the decode-buffer doubling at the inline reserve (a measurement, not a
   bisect: it predates the drain). For the apply-changes barrier work,
   compare `-l` on a LocationsOnWays planet; Denmark has no such variant, so
   `brokkr verify merge` never runs `-l`.
2. **The stall-counter follow-ups** in `notes/todo.md` ("Performance"):
   the `s4_readiness_wait_ns` twin first, since stage 3 to 4 readiness is
   the named planet streaming constraint and `--stalls` cannot see it.

HYG-182 (`cat --dedupe` indexing) was resolved 2026-10-26 in one commit
after a spar: the merge engine shared by `sort` and `cat --dedupe` now
indexes as plain `cat` does (scanned tagdata on reframed passthroughs, an
empty block omitted, a mixed block merged per kind into type order, a
malformed payload its own error), with physical frames separated from
per-kind entries for the counts. The spar also found that `sort` under an
unclaimed-order header copied indexdata its own scan had contradicted; that
blob is now reframed. The judgement calls are on "Review once drained".

HYG-081 (the apply-changes barrier) was resolved 2026-10-26 in five commits
after a spar: comment corrections; the node boundary moved onto the drain
channel with one readiness check (the dead type-transition trigger gone);
leftover worker coordinates made an error; deterministic barrier tests
through new fault-plan hooks (late boundary, panics while the scanner
waits or is blocked, cancellation, forced owned route); and `-j 1`
accepted, since those tests pass at one worker. The separate node-phase
run the entry proposed was not taken (reason in `apply_changes/drain.rs`'s
module doc); BUG-009's note says why it need not be either.

HYG-011 was resolved 2026-10-26 in four commits after a spar: two real
bugs codex found on the way (altw external stage 4 kept unpacked stale
way locations; a failing `parallel_classify_accumulate` worker did not stop
its peers), the two per-blob classify pools on one private driver, and the
two altw way reframes on one block-level walk. No unordered mode on
`ordered_pool`: it would drop the dispatcher the measured `WILLNEED` advice
rides on. The pool-unification history now lives in
`reference/performance-history.md` ("Worker-pool unification"). The
unordered pools' `*_wait_ns` counters followed in `003fe717` (an Opus
agent, approved by the maintainer); its lateral findings are the
stall-counter follow-ups in `notes/todo.md`.

HYG-002 was resolved 2026-10-26 by rejecting public tuning structs (the
reasons are in the `src/tunables.rs` module doc and `reference/testing.md`)
and hiding getparents' cross-crate test instrument. Sparring it turned up a
latent rule in `par_fold_blobs`: the pump acquires budget before flushing
its own partial batch, so a budget smaller than a full partial batch plus
one blob would wait forever. The production constants satisfy it; it is now
a compile-time assert plus a debug assert on the private seam.

The diff shard-buffer entry was resolved 2026-10-26 as a measurement
candidate ("Review once drained", decode-buffer growth item): codex and a
diagnostic test showed every fresh decode buffer typically doubles at the
inline-metadata reserve, so the pooling question is wider than the shards.
`research/glibc` holds glibc's source for allocator questions like that
one; read it rather than reasoning from memory.

## How the previous session worked, and should continue

- The maintainer invoked `orchestrate spar`: every substantive decision is
  sparred with codex before acting, through
  `echo '...' | review -p read --provider codex` (the one read-only profile;
  `review config` lists it), launched in the background with the five-hour
  timeout, continued with `review message <ID>`. A session idle for about
  an hour or more is refused as cache-cold: start a fresh one and restate the
  question, the positions and what was conceded. Do not touch the tree while
  a session is in flight.
- After implementation, a fresh codex session reviewed the diff against the
  agreed contract before the commit. Worth repeating for large changes.
- Rule 8 forbids measurement until both files drain, but `brokkr verify all
  --dataset denmark` is a correctness check and was run before committing
  the pool change. Use it when a change touches command output.
- Correct a drain entry in place when its premise turns out wrong (HYG-053
  claimed the read pipeline was unbounded; it was not). Check an entry's
  facts against the code before building on them: several had drifted.
- Every judgement call on a measured path goes on "Review once drained" in
  hygiene.md, phrased as what moved and what was not measured.
- Subagents need the maintainer's permission (project rule). The one used
  here was a read-only Opus research agent, approved explicitly.
- Another session sometimes holds the brokkr lock; `brokkr check` waits for
  it, which is harmless.

## Facts learned the hard way

- `BlockBuilder` holds `Rc<str>` and is not `Send`. The ordered pool takes a
  per-worker local state built on the worker's own thread for exactly this.
- A fresh `BlockBuilder` per blob in cat `--clean`, time-filter and
  getparents is deliberate and measured (time-filter's function doc records
  why builders surviving across blobs were worse). Do not "optimize" it.
- Sidecar `_ms` counters truncate per sample on purpose, for continuity with
  older result sets (`src/counters.rs`, `Stopwatch` doc). When moving
  timing code, keep the unit and the recording point; `ordered_pool::Timer`
  exists for this.
- Observability is a contract (anger 8): counter names, units and the
  `WAIT_*` marker spans must survive refactors. Check the `counter_set!`
  blocks and `StallGauge` uses before and after.
- Durable docs that state a number or name drifted often; reword rather than
  restate (the global rules say so).

## Learned 2026-10-26, not easily rediscovered

Performance and memory:

- **Every decode buffer typically doubles.** The decompressors size a fresh
  buffer to the blob's `raw_size` exactly (zlib and zstd alike, verified by
  a throwaway test), and `parse_and_inline_with_scratch`'s inline-metadata
  `reserve` on that full Vec takes amortized growth, so a 1.4 MB buffer
  becomes 2.8 MB, possibly moving. Pooled paths keep the doubled capacity
  (which is what lets a recycled buffer take the next block without a
  realloc); a final capacity past `DecompressPool`'s 4 MiB retain cap is
  dropped and re-grown every time, which hits payloads above about 2 MiB.
  `reserve_exact` is not a safe swap (it also hits pooled first use and
  changes which buffers the pool keeps). The 886.6 MB
  `parse_and_inline_with_scratch` share in the planet getparents alloc
  profile is probably this. Unmeasured; on the review list.
- **glibc realloc, from `research/glibc/malloc/malloc.c`:** an mmapped chunk
  grows by `mremap` (no copy); an arena chunk grows in place into the top
  chunk or a free neighbour, else malloc, copy, free. Freeing an mmapped
  chunk raises the process-wide `mmap_threshold` to its size (32 MiB cap)
  and `trim_threshold` to twice that. Process-wide, not per thread, and
  arenas can still trim; "malloc already recycles it" is plausible, never
  guaranteed.
- **`par_fold_blobs` has a budget rule** (`read/reader.rs`): the pump
  reserves a blob's bytes before flushing its own partial batch, so a
  budget below a full partial batch plus one maximum blob would block
  forever. Compile-time asserted for the production constants; a test that
  shrinks the budget with more than one blob per batch will hang.
- **The claim-counter pools** (altw external stages 2 and 3, geocode pass
  1.5) lock a first-error mutex on every claim; stage 3's two error stores
  serve different recipients (its caller, and stage 4 via the router).
  Their only cross-thread wait worth counting is stage 2's slot-bucket
  writers.
- **Reading the new stall counters:** `classify_unordered_desc_lock_wait_ns`
  is the shared-receiver mutex's own cost only while
  `classify_unordered_desc_recv_wait_ns` stays near zero. They are one
  process-wide set emitted as running totals after every pool run, so the
  step between emissions inside a phase is that run's share.

apply-changes `-l` barrier, as it now stands:

- The scanner's node boundary is a `DrainMessage::NodeBoundary` on the drain
  channel (exclusive end; zero means no nodes) and the drain has one
  publication check after each receive. The 25 ms `DRAIN_RECV_TIMEOUT` is
  no longer about the barrier: it is the only thing that wakes the drain to
  notice a published failure or a cancellation.
- On unsorted input a node after a way is dispatched past the boundary,
  misses publication, and is refused later by the drain's output-order
  guard. Not a hang.
- Fault-plan hooks exist for every barrier ordering
  (`apply_changes_late_boundary`, `_panic_waits_for_barrier_wait`,
  `_hold_workers_until_dispatch_blocked`, `_park_at_blob_seq`,
  `_force_owned_route`); `tests/fault_apply_changes.rs` shows the use.

Working practice:

- **Several entry premises were wrong when checked against the code**:
  stages 2 and 3 and pass 1.5 were never `Mutex<Receiver>` pools; worker
  counts were already injectable; the barrier's type-transition trigger
  could never fire; the `-j 1` refusal was a missing validation, not a
  known deadlock. Read the code before sparring the entry's own framing.
- **Sparring found real bugs each time** it was asked to look past the
  entry (altw external kept unpacked stale way locations; a failing
  accumulate worker drained the schedule). Ask codex to flag anything,
  not only to judge the proposal.
- **Mutation-check concurrency tests**: break the mechanism on purpose and
  confirm the test fails or hangs before trusting a first-time pass.
- **`review` refuses stdin over 20,000 bytes.** For a large diff, tell codex
  to read `git diff <paths>` itself instead of piping it.
- **Shell quirks here:** `mv` is not allowlisted (use `cp` then `rm -f`),
  and `rm` prompts interactively, so `rm -f`. A subagent working
  alongside you will report your own uncommitted edits as someone else's.
