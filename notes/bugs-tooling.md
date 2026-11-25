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

## TOOL-051 - verify field-policies locates its slice from input order

Reported by: the spar accounting of the verify geocode input-order change,
from its scout's lateral (2026-11-24); traced from source, not reproduced.

`controls` in `crates/verify/src/checks/field_policies.rs` builds its slice
with `osmium getid -r` (and plain `getid` for relations) from the input
itself. It then runs `osmium add-locations-to-ways -n --ignore-missing-nodes`
over the pieces, without the osmium-sorted copy that `InputOrder` gives every
other check. `getid` keeps the input's order, and osmium locates a way only
from nodes stored before it. On input without the `Sort.Type_then_ID`
claim, a way stored before its nodes therefore comes out with undefined
locations. No verdict turns wrong: the way-location and "between" controls
just lose candidates and can end named as not exercised. But the module doc
promises that only a class with no candidate record on the input is named
that way. Reading the pieces from `InputOrder::of(..).reference_input`, as
geocode now does, would keep that promise.

## TOOL-047 - A reference run over a pbfhogg output fails by presumption

Reported by: the spar on the verifier's error classification (2026-11-24);
narrowed by the spar that gave every reference input its producer.

Every `ToolRun` input now names who wrote it, and an unaccepted reference
exit that read no pbfhogg output is a verifier error (exit 2), so a dataset
in a compression libosmium refuses no longer fails `verify sort` and its
kin. A run that read any pbfhogg output still fails its check on any
unaccepted exit, because osmium names no file in its refusal. Two cases
remain misjudged. First, a mixed run can be the first osmium read of the
dataset: `verify diff --format osc` on sorted input builds `new.osm.pbf`
with pbfhogg apply-changes (which passes the base's blobs through), and its
first osmium launch is `osmium derive-changes BASE NEW`, so a zstd base
fails it as pbfhogg's. A full osmium decode of the base before that launch,
as a run reading no pbfhogg output, would settle the prerequisite. Second,
an osmium exit independent of every input (an output write error, a crash)
on a run with a pbfhogg operand still reads as pbfhogg's; telling those
apart needs evidence the exit alone does not carry.

## TOOL-052 - verify merge blames pbfhogg when osmium disagrees with the model

Reported by: lateral of the reference-provenance change (2026-11-24); read
from source, not reproduced.

In `judge` in `crates/verify/src/checks/merge_changes.rs`, an `osmium
merge-changes -s` output that keeps other changes than the verifier's own
model of it predicts returns `Error::Failed` (exit 1), with the message
that the reading of the inputs cannot be trusted. Neither side of that
comparison is pbfhogg's: it is the reference against the verifier's model
over the dataset OSCs and verifier-written fixtures, so the outcome is a
verifier error (`Error::Tool`), not a pbfhogg verdict.

## TOOL-053 - A grouped OPL decode raises the first failure in plan order

Reported by: the spar on the reference-provenance change (2026-11-24); read
from source, not reproduced.

`acquire_missing` in `crates/verify/src/opl_compare.rs` settles a launch
group of osmium decodes, then propagates the first per-run error in plan
order. When both operands of a comparison fail to decode, one as a verifier
error (it read no pbfhogg output) and one as a check failure (a pbfhogg
output), operand order decides whether the suite stops with exit 2 or
records a FAIL. The contract gives a recorded failure precedence over
ENVIRONMENT only, so either outcome is defensible, but it should be chosen
explicitly and both diagnostics kept.

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

## TOOL-059 - Five verify checks have no fixture driving their run

Reported by: the geocode fixture spar (2026-11-25), confirmed by codex.

No test under `crates/verify/tests` drives the production `run` of
`merge_changes`, `getparents`, `inspect_tags`, `check_ids` or
`field_policies`; they have helper-level unit tests only, and the
executable fixture in `process.rs` drives `sort`, not these. Their flow,
verdict names and exit judging are exercised only by real-data runs. Each
wants a fake-tool fixture with one fault per verdict, as
`crates/verify/tests/geocode_fixture.rs` and `getid_fixture.rs` do.

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
