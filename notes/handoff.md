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
- **protohoggr is the maintainer's own crate** (`~/Programs/protohoggr`,
  published as 0.4.0). Read-side narrowing there follows protobuf and is
  correct. The encoder bug found here (`zigzag_encode_32` sign-extends to ten
  bytes) is being fixed there by the maintainer. A defect in protohoggr is
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
- **HYG-011 (unified pool) has history**: two earlier verdicts against it,
  now recorded in the entry's History section. The maintainer's question was
  whether resolving other entries reworks the same sites, and if so, to queue
  HYG-011 behind them. HYG-053 has since put every *ordered* pool on one
  engine (`src/ordered_pool.rs`); what remains of HYG-011 is the unordered
  family (see the entry). Judge it against the tree as it is now. An
  unordered mode for `ordered_pool` (claim counter and exit contract, no
  window or reorder) would absorb most of that remainder: the classify
  unordered trio, geocode pass1_5, and altw external stages 2 and 3 if
  bucket dispatch fits a task index. The diff and derive shard runs sit
  outside it. A design to spar, not a given.
- **HYG-182 carries a conflicting instruction.** Its Maintainer paragraph
  says dedupe's indexing "is not to be touched in this drain", which
  contradicts "every entry is owed a resolution now". Ask the maintainer
  before taking it rather than guess which wins.
- **The protohoggr encoder fix changes pbfhogg's output bytes** (filed as
  bz `protohoggr#1`). Once
  `zigzag_encode_32` writes canonical five-byte varints and pbfhogg bumps
  the dependency, `BlockBuilder` output shrinks for dense uid / user_sid
  deltas of magnitude `2^30` or more. Check any test or verify that pins
  output bytes when bumping; `dense_node_deltas_decode_at_signed_boundaries`
  (`src/read/dense.rs`) exercises exactly that case. The reader keeps
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

HYG-002 (tunables injectable) next: the window and depth knobs of every
ordered pool now live in one engine (`ordered_pool::PoolConfig`), so the
change is much smaller than the entry implies. The entry is also partly
stale: worker counts are already injectable through `Resources::parallelism`
(`host::Fixed`) and `Parallelism::Fixed`; what tests cannot reach is altw
external's 256 buckets per worker, the pool windows and the full-scan
threshold (getparents only through a hidden CLI flag). Then the rest of
HYG-011, HYG-081 (apply-changes barrier, its own design), HYG-036, HYG-182.

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
