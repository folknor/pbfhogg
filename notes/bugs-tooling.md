# Dev tooling defects

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.
```

Defects from the 2026-11-17 defect hunt in the dev-only crates:
`pbfhogg-verify`, `pbfhogg-datasets` with `pbfhogg-devkit`, `pbfhogg-library`
with `pbfhogg-harness`, and `pbfhogg-bench` with its Lua scripts. Every
finding was made by one codex hunter per scope reading source at `63d4ec1d`.
None was built, run or reproduced, and the bracketed priority is the hunter's
own label. The geocode-query optimizer sink is recorded under BUG-028.
Entries are in no order of importance.

## TOOL-002 - verify extract compares different regions for some bbox spellings

Reported by: verify [P2].

`checks/extract.rs` passes the original bbox text straight to osmium.
`DEVIATIONS.md` records a deliberate difference in exponent quantization:
`0.000000001e2` becomes coordinate column 1 in pbfhogg and column 0 in
libosmium. With `--bbox=0.000000001e2,-1,1,1`, a node at longitude zero is
correctly excluded by pbfhogg and included by osmium, and verify reports a
membership failure. This breaks the claim that extract compares membership in
the same region. The verifier must translate the intended integer edges into
an equivalent reference bbox, or explicitly refuse that comparison domain.

## TOOL-005 - verify geocode builds its osmium reference from unsorted input

Reported by: verify [P2].

`checks/geocode.rs` filters the original input and feeds the result straight
to osmium add-locations-to-ways without sorting it. For a valid indexed file
with a named highway way before its node blobs, pbfhogg's separate builder
passes can resolve the nodes. Osmium's location handler meets the way first,
and `--ignore-missing-nodes` leaves its coordinates absent. The verifier then
rejects the model for containing an unlocated street way, although the source
has every required node. This breaks the check's claim to model the input's
geometry, because the missing locations come from reference construction.

## TOOL-047 - A reference that cannot read the dataset input reports FAIL

Reported by: the spar on the verifier's error classification (2026-11-24).

The verifier's own frame reader now classifies a refusal by who wrote the
file (`pbf_frames::Origin`), but a reference run is still judged by the
reference-run rule alone: any unaccepted exit is `Error::Failed`, apart from
SIGKILL and osmium's `bad_alloc` line. So a dataset input in a compression
libosmium refuses (zstd: `PBF error: zstd blobs not supported`) fails
`verify sort` with exit 1, because `osmium sort` reads the input before any
verifier decode does. The same holds for every check whose first reference
reads the input. A stderr match alone cannot fix this. An osmium run can also
read a pbfhogg output, and there a zstd blob may be a writer defect: `repack`
is contracted to re-encode with zlib. Classifying correctly needs per-operand
provenance on `ToolRun` inputs, where `PbfArgv::writes()` already names
pbfhogg's outputs. It also needs evidence tying the refusal to the dataset
operand, not just to some input of a multi-input run.

## TOOL-007 - verify sort never exercises the unindexed fallback

Reported by: verify (flagged as a coverage limitation).

`checks/sort.rs` never passes `--force`, so selecting an unindexed raw input
exercises the CLI's indexdata refusal rather than sort's supported fallback.

## TOOL-018 - Planet acquisition prints instructions that register the wrong sequence

Reported by: datasets-devkit [P2].

The post-download note (`download.rs`) tells every source to read
`header.replication.sequence` and copy it into the registered PBF entries.
The planet contract in `reference/datasets.md` says that header sequence is
minutely, while this tool downloads daily diffs. Following the instructions
configures the wrong sequence namespace, which `base_seq` then trusts. The
documentation and refusal policy are correct. The operational guidance is
what is wrong.

## TOOL-019 - replace_file promises a rollback it cannot give after the rename

Reported by: datasets-devkit [P3].

The public contract of `replace_file` (`registry.rs`) says a failure leaves
the destination unchanged. But the rename succeeds before the directory
open/fsync, either of which can fail, and the function then returns `Err`
with the new registry already installed. The hunter says the documentation is
what is wrong. The result contract should distinguish failure before
replacement from a replacement whose durability could not be confirmed. The
same hunter's structural note: give publication one complete registry
ownership inventory and one shared acquisition and recovery protocol, because
parsing pins, sequence keys, aliases and ownership separately in each path is
already producing contradictory answers.

## TOOL-022 - Clock validation uses absolute times where the contract says relative

Reported by: library-protocol [P2].

`run.rs` compares absolute `entry_us`, `return_us`, `seal_us` and diagnostic
timestamps with process wall time plus a tolerance, and FIFO validation makes
the same comparison. The contract in `reference/library-protocol.md` requires
elapsed differences, because the FIFO clock's origin is unreliable. A short
call whose readings start at 10,000,000 us is rejected even when every
duration and stream boundary is valid. Durations should be checked relative
to `entry_us`, with stream checks kept against the call's recorded interval.

## TOOL-027 - geocode-query quantizes the requested sampling box

Reported by: bench.

The instrument's bbox argument (`crates/bench/src/main.rs`) goes through the
extraction bbox parser, which deliberately rounds every edge to
decimicrodegrees (`extract/mod.rs`). The valid box
`0.00000001,0.00000001,0.00000002,0.00000002` becomes four zeroes, every
generated query targets `(0, 0)` outside the supplied box, and the Lua record
keeps the original bbox text. This breaks the instrument's promise to draw
points uniformly from the supplied box. Query sampling needs validation
without quantization.

## TOOL-028 - diff-text accepts raw inputs that bypass its advertised engine

Reported by: bench.

The registry (`brokkr.d/benches.toml`) and the script describe an
indexed-input benchmark that runs through block-pair work units, but both
variant selectors accept raw inputs, and `benches/diff/diff-text.lua` does no
indexdata probe. Raw variants complete successfully through the sequential
fallback (`diff/mod.rs`), so the measurement does not exercise the documented
workload. The benchmark should establish its indexed-input precondition, as
diff-unindexed establishes the opposite one.

## TOOL-029 - verify derive roundtrips assume osmium apply precedence matches

Reported by: verify (2026-11-17 hunt), narrowed 2026-11-20.

The apply-changes check now judges pbfhogg against its own change model.
The derive roundtrips (`crates/verify/src/checks/derive_changes.rs`) still
apply derived OSCs with osmium and compare with `new` under `CONTENT`. A
derived upsert whose version equals the base's with an earlier valid
timestamp, or a claimless one, would be dropped by osmium's apply
precedence and surface as a content difference blamed on pbfhogg. Traced
from source only; derive emits new's versions, so whether real derives
reach it is unchecked.

## TOOL-032 - Refusal tests that accept any error can pass on a panic

Reported by: the release-sweep retirement (commit `e716ad49`).

A caught worker panic is now `ErrorKind::Panicked` and exits 101, so a
test naming its refusal kind, or a CLI test requiring exit 2, fails on one.
What remains: tests that accept any error still pass on a panic (dev
overflow checks panic where release wraps). `CliInvoker::assert_failure`
and about 26 direct `!status.success()` sites accept 101; library and unit
tests holding bare `is_err()`, discarded `unwrap_err`/`expect_err`, or
`Err(_)` patterns accept `Panicked`; and no source-policy rule keeps new
ones out. The settled rulings and the remaining links (CLI refusal bar,
source-policy rule, sweep, green agent) are in `notes/tool-032-design.md`.

# From the 2026-11-21 verifier performance hunt

Defects the performance hunters found while reading the code at `e8377a78`
(see the PERF-064 section of `notes/performance.md` for the hunt). None was
reproduced.

## TOOL-046 - A moved input turns a verifier error into ENVIRONMENT

Reported by: the PERF-066 spar (codex), verified at the code.

`Harness::run_check` (`crates/verify/src/harness.rs`) keeps `Interrupted`
and `Unsettled` from the check, but for every other result computes
`validation.and(result)`: when the final input revalidation fails, its
`Environment` replaces the check's own error. A `Tool` error (for example a
stderr overflow, which reference/verify.md says is a verifier error, exit 2)
becomes ENVIRONMENT and `all` continues. Decide which wins; the contract
reads as if the verifier error should, so `Tool` probably belongs in the
pass-through arm beside `Interrupted` and `Unsettled`. Left for the user
because it changes which outcomes stop a suite.

