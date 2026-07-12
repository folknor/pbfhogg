# Technical implementation specification

The single document from which an open TODO item is built to completion without
re-deriving its design. Two implementers working from it independently produce
the same artifact.

Specifications are saved to the ./notes folder.

## What it is

1. **Every brick.** It lays each step on the road from the current code to the
   finished item. No step is left to discover during implementation.
2. **Obstacles resolved inline.** Anything blocking the road is solved in the
   document, as part of it. An unresolved obstacle is a missing brick.
3. **No deferral.** Nothing in the originating TODO is pushed to "later" -
   deferred work is a hole in the road. (Work that belongs to a genuinely
   separate TODO is named and excluded; that is not deferral.)
4. **No shoehorning.** We do not fit the work into existing abstractions,
   structures, or conventions because they already exist. The structure that
   best serves the end goal is the one we build; whatever stands in its way is
   ripped out and rebuilt. Pre-1.0, breaking any internal API is legal.

## What it must also pin (or it is aspiration, not a spec)

5. **Verification per brick.** Every change names its gate, matched to what
   the change can break: `brokkr check` (gremlins + clippy + the full test
   suite) for anything touching read/write semantics, wire encoding, the CLI
   surface, or an invariant; `brokkr verify <command> --dataset denmark
   --variant indexed` (cross-validates the command's output against osmium /
   osmosis / osmconvert) for anything touching a command's element output,
   with zero diffs the bar save the parity exceptions already documented in
   `reference/osmium-parity.md`; the ignored roundtrip suite
   (`brokkr check --profile full`, or a named `brokkr test <file> <name>`)
   for changes to the reader, writer, `BlockBuilder`, or `PbfWriter`, where
   re-encoding a whole PBF and reading it back is the check no smaller test
   makes; a `brokkr <command> --dataset <D> --bench` run for anything
   claiming or risking a performance effect (the win, its neutrality, or -
   when a feature knowingly pays for capability with throughput - the
   accepted cost, stated as an explicit bound the keep/revert verdict is
   read against), with peak anon RSS read from `brokkr sidecar <UUID>
   --human` when memory is the axis; named unit tests for behavior no oracle
   reaches, placed and tiered per `reference/testing.md` (which pins the
   test-placement axes, the validation tiers, and the stable allowlist a
   test imports through). A brick whose load is unproven is not laid. Per
   gate, the spec contains the EXACT command to run - copy-pasteable, flags
   and all, not "run the relevant tests" or "verify the output". Gate
   commands pin the
   dataset AND the variant explicitly (`--dataset`, `--variant`); "the
   denmark gate" without a variant is underspecified. The variant is not a
   performance knob - it changes the input and therefore the output: `raw`
   carries no blob indexdata, `indexed` does, `altw` carries
   locations-on-ways, and many commands require `indexed` and either error
   or drop to a slower fallback on `raw`. A verify or bench read across
   mismatched variants is not a verdict. Choosing each gate's dataset is a
   decision the spec author makes explicitly, every time: the smallest
   dataset that can actually answer THIS gate's question, and no larger.
   There is no fixed ladder to look up - the author reasons from what the
   change can break to the cheapest input that would expose it, and the spec
   states which dataset the gate uses and why it is sufficient. A correctness
   or wiring question is usually settled by a tiny dataset (malta,
   greater-london) or denmark; only a question genuinely about scale (a
   memory ceiling, blob-count asymmetry, throughput at size) pulls in japan
   or europe, and planet only when nothing smaller can exercise the 30 GB-RAM
   ceiling - planet is an explicit user decision per
   `reference/performance.md` and costs minutes to hours. Running a larger
   dataset than the question needs - a planet bench to "be extra sure" of a
   change denmark already proves - is waste, not rigor. If no command exists
   that can verify a gate (no test pins the behavior, no tool decodes the
   artifact), building that instrument is itself a brick of the spec -
   specified to the same standard and laid before the brick it gates. A spec
   justified by an estimated volume leads with the instrument that prices
   it: the counter (or equivalent) is the first landing, and the spec states
   an explicit proceed/close threshold the reading is judged against - below
   it, the item closes as mispriced and the rewrite is never laid. The
   estimate motivates the spec; only the measurement justifies the landing.
6. **A keep/revert path.** The implementation unit is one coherent, fully
   intrusive change that lands and is then kept or reverted on its gate
   results - never a tiny gated probe or an env-var experiment switch. The
   sequence of such landings is ordered so `brokkr check` (and, where the
   item touches a command's output, `brokkr verify`) stays green at every
   boundary between them.
   Complete-but-unorderable is a failed spec. Benchmark discipline holds
   at every landing: commit first, then measure, then record numbers
   against the commit hash (never benchmark uncommitted code). For a
   before/after comparison, land the change, bench HEAD for the after
   number, and bench the baseline with `brokkr <cmd> --commit <ref> --bench`,
   which builds and benches a prior commit in its own worktree and stores the
   row tagged to it - so a baseline is available at any time from your own
   branch.
7. **The target as concrete artifacts.** "The ideal structure" is pinned to
   exact types, signatures, ownership, and data flow - buildable, not merely
   directional.
8. **A survey of the ground.** The current structure and everything depending on
   it is inventoried before the teardown, so the rip is precise and drops no
   load-bearing work. A survey that prices a hot path traces the premise
   through the actual caller ordering at the priced call site - what the
   structure admits is not what the callers do. For optimization work the
   survey includes the failure history: `notes/altw-optimization-history.md`
   and the per-command optimization notes (the "Don't re-attempt" /
   "Failed attempts" sections in `notes/*.md`) are the ledger of every tried
   and rejected approach, with the measured numbers and physical floors that
   sank them; a spec that re-proposes a logged failure without addressing
   why it failed is refuted by its own survey. The survey also checks whether
   the behavior the spec touches is already governed by a standing decision:
   `decisions/*` (the ADRs recording why pbfhogg is shaped as it is - e.g.
   negative IDs rejected project-wide, debug-assert invariants), `CORRECTNESS.md`
   (parser/encoder edge cases and representation limits accepted by design),
   and `DEVIATIONS.md` (intentional behavioral differences from osmium). A
   spec that changes one of these must say so and either honor the decision or
   argue explicitly for overturning it; a spec that "fixes" a documented
   deviation or breaks an accepted invariant unaware is refuted by the record.
   The reverse holds too: a spec whose own landing establishes a new policy or
   architecture decision - the kind `decisions/README.md` names as ADR-worthy -
   names the ADR it will add under `decisions/` as a landing deliverable, so
   the *why* is captured while it is fresh rather than reconstructed later.
   Specs authored as a batch reconcile their
   surveys against siblings covering the same ground before any is
   implemented; a sibling's survey may already state the fact that refutes
   this spec's premise.
9. **A stopping rule.** The rebuild has a bounded blast radius. Where the
   teardown stops, and what is out of scope, is stated explicitly.
10. **The standing references.** Every spec MUST cite, by path: this document
    (`reference/technical-implementation-spec.md`) as the contract it is
    written against, AND the document the spec was spawned from (the item's
    source naming the problem - e.g. the owning `notes/*.md` writeup),
    if it exists.
    A spec that adds, moves, or reorganizes tests also cites
    `reference/testing.md`, the placement-and-tier contract those bricks must
    satisfy. The measurement record is `reference/performance.md` plus
    `reference/performance-history.md` plus `.brokkr/results.db`:
    `performance.md` holds the current baselines and live per-command
    breakdowns, `performance-history.md` is the durable ledger of
    optimization arcs, superseded baselines, and regression retrospectives
    (the home where a landed-then-deleted plan or spec note's findings settle
    - the "history ledger" the loop points durable references at), and
    `.brokkr/results.db` is the raw stored runs. A spec that claims a
    performance effect, or whose changes touch a measured path, states the
    pre-change baseline (host + commit hash) the keep/revert verdict will be
    read against, and after landing records the post-change numbers the same
    way (commit, then benchmark, then write the hash-anchored numbers): the
    new current numbers replace the baseline in `reference/performance.md`,
    and the superseded baseline plus the arc narrative settle into
    `reference/performance-history.md`. Gate dataset choice, bench invocations, and
    noise bounds follow that document's reading rules - a verdict read off a
    single run, an instrumented-mode RSS figure, or a mismatched gate dataset
    is not a verdict. A spec off every
    measured path owes no benchmark update; it states that, and names the
    gate whose unchanged result confirms neutrality.

## Stance

- **Structural over micro.** The spec pursues the structural change that
  materially moves the goal - real throughput for performance work, real
  capability for feature work - not local tweaks. Full rewrites are labeled
  as such, distinct from local changes.
- **Cleanliness is a deliverable.** No env-var scaffolding, benchmark knobs, or
  temporary routing switches left as the way forward.
- **Unlimited resources, aggressive internal rewrites assumed.** Old
  abstractions earn no protection from age; shared writer abstractions and
  generic reuse are not goals. Correctness and maintainability of the *result*
  still hold.
