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

## TOOL-003 - verify sort rejects correctly preserved repeated IDs

Reported by: verify [P2].

Sort promises to keep every element (`blob_merge.rs`), so an indexed history
input with `n1 v1` and `n1 v2` can be correctly sorted by both tools while
keeping both records. `checks/sort.rs` uses the shared comparator, whose key
holds only kind and ID, and its strict advancement (`opl_compare.rs`) rejects
the second `n1`. The separate output-order check rejects it too. This imposes
a uniqueness that sorting does not promise. The hunter says the verifier
documentation's strict-uniqueness requirement is wrong for this command:
ordering and uniqueness need separate judgments.

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

## TOOL-006 - Unsupported comparison domains are reported as pbfhogg failures

Reported by: verify [P2].

The contract (`reference/verify.md`) assigns unsupported comparison domains to
`ERROR`, exit 2. The independent blob decoder (`pbf_frames.rs`) builds
`Error::Failed` for every decoding refusal. For example, sorting a claimless
indexed `LocationsOnWays` input with zstd data can succeed correctly and then
fail the verifier's planner prediction, because its decoder supports only raw
and zlib. That becomes `FAIL`, exit 1, blaming pbfhogg for a verifier
limitation. Repeated scalar IDs and wrapping dense deltas hit the same
misclassification, although both are documented comparison-domain exclusions.
`checks/getparents.rs` refuses an input holding a negative ID as
`Error::Failed` while its own message says getparents "cannot be compared on
it"; `checks/extract.rs` and `verify_extract_simple.rs` raise the same kind of
refusal as `Error::Tool`. A related open question: a reference run that dies
on an osmium limit (extract's "Out of memory" on a negative outside way ref,
VER-155) is `Error::Failed` by the reference-run contract in `error.rs`.

## TOOL-007 - verify sort never exercises the unindexed fallback

Reported by: verify (flagged as a coverage limitation).

`checks/sort.rs` never passes `--force`, so selecting an unindexed raw input
exercises the CLI's indexdata refusal rather than sort's supported fallback.

## TOOL-015 - Distinct OSC registrations silently collapse onto one sequence

Reported by: datasets-devkit [P2].

`generation` (`registry.rs`) parses string keys to `u64` and inserts them
without a duplicate check, so the valid TOML keys `011` and `11` become one
entry and silently discard a file and its pin. This bypasses the requirement
in `reference/datasets.md` that every registered donor exists, passes its pin
and agrees with other copies. Conversely, `select_osc` uses literal string
keys, so a sole `011` entry exists for acquisition but is missing for a
numeric range requesting sequence 11. The two devkit readers disagree, and
sequence interpretation needs one representation that refuses ambiguous
registrations.

## TOOL-016 - Acquisition silently ignores retired sha256 pins

Reported by: datasets-devkit [P2].

`file_entry` (`registry.rs`) reads `xxhash` and ignores `sha256`, so a file
carrying only the retired pin becomes unpinned in `DatasetView`, and
acquisition accepts it without checking the pin. The other reader,
`resolve_entry` (`crates/devkit/src/datasets.rs`), explicitly refuses the same
configuration, as the retirement contract requires. This is reachable through
the supported `--config` selection of another project's registry, even when
brokkr's own config is valid.

## TOOL-017 - A configured FIFO hangs digest resolution

Reported by: datasets-devkit [P2].

`file_digest` (`crates/devkit/src/datasets.rs`) opens the path before checking
its type. Opening a FIFO for reading with no writer blocks indefinitely, so
the intended refusal never runs. `resolve_input`, OSC selection and polygon
selection reach this directly, and a malformed registered input can hang the
tool while it holds brokkr's machine lock, instead of returning the promised
invalid-entry error. The fix belongs at the file-opening boundary, because a
metadata check alone would leave a replacement race.

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

## TOOL-020 - Omitting stream coverage disables stream enforcement

Reported by: library-protocol [P1].

`crates/library/src/run.rs` computes every call's stream verdict but keeps it
only when the call declares a nonempty `stream` capability list. Otherwise
even a proven failure is discarded. A declaration that omits `stream` can pass
verification despite a missing terminal record, duplicate terminal records or
sealing values that disagree with its snapshot, and other calls can still
cover `stream.sealed.v1`, so certification passes too. This breaks the
contract in `reference/library-protocol.md` that stream accounting must pass.
Coverage should decide which capabilities a result supports, not whether
protocol violations fail the run.

## TOOL-021 - The nofile certifications do not exercise real descriptor limits

Reported by: library-protocol [P1].

`requirements.rs` defines the `nofile.*.v1` capabilities in terms of the soft
process limit, including external ALTW running with soft nofile 1024. The
worker instead supplies fixed FdPool grants of 1024, 6 and 896 descriptors
(`crates/harness/src/scenarios/nofile.rs`) and raises the soft limit to the
hard limit (`crates/harness/src/process.rs`). Those scenarios can pass without
exercising real-limit probing or the stated limit. The grant scenarios are
internally consistent, but the requirement meanings and coverage declarations
overstate what is certified. Either restore scenarios that prove those
meanings, or add new capability versions for grant-based admission and
sizing. Rewording the existing IDs would break the protocol's
meaning-versioning rule.

## TOOL-022 - Clock validation uses absolute times where the contract says relative

Reported by: library-protocol [P2].

`run.rs` compares absolute `entry_us`, `return_us`, `seal_us` and diagnostic
timestamps with process wall time plus a tolerance, and FIFO validation makes
the same comparison. The contract in `reference/library-protocol.md` requires
elapsed differences, because the FIFO clock's origin is unreliable. A short
call whose readings start at 10,000,000 us is rejected even when every
duration and stream boundary is valid. Durations should be checked relative
to `entry_us`, with stream checks kept against the call's recorded interval.

## TOOL-026 - merge-workload can take its timing from an input filename

Reported by: bench.

`benches/lib/workload.lua` takes the first `elapsed_ms=(%d+)` substring
anywhere in stderr. Before the instrument prints its measurement, the library
prints the OSC pathname (`apply_changes/rewrite.rs`), so a configured file
named `elapsed_ms=7.osc` makes `record.cells[].elapsed_ms` equal 7 whatever
the real timing. This breaks merge-workload's promise to keep the child's time
through publication start. The Lua consumer is wrong. It should parse a
unique, complete measurement record, ideally on a channel separate from
diagnostics. Brokkr's independently measured process wall is unaffected.

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

## TOOL-030 - The dataset lock file is opened following symlinks

Reported by: the scout of the download staging item (2026-11-20); not
re-read since.

`pbfhogg-datasets` opens its per-dataset lock file with an ordinary open,
which follows a symlink at that name. A symlink planted at the lock path can
make the lock open, and possibly create, a file elsewhere. The staging and
registry writers now refuse entries they do not own; the lock open does not.

The lock name also sits outside the publication-name rule every other
entry the tool changes now takes (`layout::claim`): a registration (of
any kind) naming `.KEY.download.lock`, or reaching it through a dangling
symlink, is filled by the lock's `create`. Its content is no longer
written (the recovery record moved to `.KEY.download.journal`), so a hard
link to a registered file is no longer corrupted, only locked. Raised by
the spar partner on the dataset-recovery work (2026-11-24).

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

## TOOL-047 - reference/verify.md says every fixture runs with --fresh-reference

Reported by: the PERF-072 scout, not yet verified at the code.

reference/verify.md "Decoding summaries" says summaries are neither read nor
written under `--fresh-reference` and that every fixture test runs in that
mode. The apply, change, dedupe, extract, getid, read and refs fixtures build
their cache with `ReferenceCache::new(.., false)`; only the encoding and diff
fixtures pass `true`. Either summaries can fire in those fixtures (their fake
osmium keys decodes by content marker, so a stored summary could decide a
comparison the fixture means to merge), or the doc's claim rests on some
other switch. Establish which and fix the doc or the fixtures.
