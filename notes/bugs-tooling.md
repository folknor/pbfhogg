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

## TOOL-004 - A settled reference environment failure poisons later checks

Reported by: verify [P2].

Reference staging names (`reference_cache.rs`) contain RUNID and the reusable
`ToolRun.id`, with no launch identifier. If a reference descendant outlives
its leader, lifecycle settlement correctly returns `Environment`, but
`result?` then leaves the staging directory behind. The suite continues as
promised, and its next use of that run ID, for example `opl-compare.1`, fails
`create_dir` with `AlreadyExists`. A recoverable environment outcome becomes a
fatal verifier error, breaking the explicit promise to continue past
environment outcomes. Launch-unique staging would stop later checks from
depending on earlier failures.

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

## TOOL-007 - verify sort never exercises the unindexed fallback

Reported by: verify (flagged as a coverage limitation).

`checks/sort.rs` never passes `--force`, so selecting an unindexed raw input
exercises the CLI's indexdata refusal rather than sort's supported fallback.

## TOOL-008 - Dataset recovery can attribute one source's bytes to another

Reported by: datasets-devkit [P1].

`bind_identity` (`crates/datasets/src/download.rs`) rewrites the dataset-wide
identity when the current target is absent, without checking older
recoverable raw files. Sequence: acquisition of `asia/georgia` publishes its
raw file on day A and dies before registration. On day B, acquisition of
`north-america/us/georgia` uses a different filename, rewrites the identity
and registers successfully. Acquiring snapshot A for the US dataset then
adopts the leftover Asian raw file, because the identity now matches the US
source. This breaks the source identity and safe recovery contract in
`reference/datasets.md`. One mutable identity cannot describe every retained
unregistered publication.

## TOOL-009 - Generating a missing indexed PBF can overwrite another dataset's file

Reported by: datasets-devkit [P1].

`generate_indexed` (`download.rs`) checks filenames only within the selected
dataset, by literal string equality, and ordinary primary acquisition and new
snapshot acquisition do not call the host-wide destination check. If
Denmark's registered raw file is `shared.osm.pbf` and another dataset
registers `shared-with-indexdata.osm.pbf`, downloading Denmark with its
indexed entry missing replaces the other dataset's bytes and leaves its pin
unchanged. Absolute and alternate path spellings also get past the local
check. This violates registered-file preservation and the promise that
acquiring a snapshot leaves existing generations unchanged. Every acquisition
path needs the same ownership checks.

The final-name checks that do exist (refresh, OSC, promotion) compare
canonical endpoints, which is the wrong question for a rename: `rename`
replaces the destination entry, so a registration that traverses the final
name as a symlinked directory is broken while the endpoints differ. The
staging removal already asks the right question (`layout::stage`, does
resolving a registration look up this entry); publication should share it.
Final names also coincide across datasets (`foo-bar` snapshot `baz` and
`foo` snapshot `bar-baz` both publish `foo-bar-baz.osm.pbf`) under
different dataset locks, so only Brokkr's machine lock serialises them.

## TOOL-012 - The alias resolver does not follow dangling symlinks

Reported by: datasets-devkit [P2].

`publication_path` (`crates/datasets/src/layout.rs`) treats `canonicalize`
returning `NotFound` as an absent filename. If `alias -> destination` exists
but `destination` does not, it returns the path of `alias` instead of its
target. Refresh and promotion can then fill a destination already registered
through that alias, bypassing collision protection and possibly installing
bytes inconsistent with the registered pin. This contradicts the helper's
documented behaviour and promotion's promise in
`reference/dataset-promotion.md` not to fill absent registered destinations.

## TOOL-013 - The collision inventory leaves out polygons and PMTiles

Reported by: datasets-devkit [P2].

`DatasetView` and `Generation` (`registry.rs`) keep only PBF and OSC entries,
and download and promotion use that view as their inventory of registered
paths. A configured polygon at a promotion staging path is invisible to the
protection check (`promote.rs`), so promotion deletes it. An absent registered
polygon destination can likewise be filled. PMTiles registrations have the
same gap. This breaks the unqualified registered-path protection contract.

## TOOL-014 - Recovery registers files without making them durable first

Reported by: datasets-devkit [P2].

Fresh publication syncs the file and its directory, but raw adoption
(`download.rs`), OSC adoption and promotion reuse (`promote.rs`) do no
equivalent sync. If a process dies after the rename but before the directory
fsync, the retry sees the complete file and registers it without first making
the directory entry durable. A later power failure can keep the registry
commit and lose the publication. Promotion can also reuse an existing equal
file whose bytes were never synced. This breaks the documented ordering that
durable publication comes before durable registration.

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

## TOOL-023 - The worker accepts malformed JSON Unicode escapes

Reported by: library-protocol [P2].

`crates/harness/src/json.rs` parses the four bytes after `\u` with
`u32::from_str_radix`, which accepts a leading `+`, while JSON requires four
hexadecimal digits. `"scenario":"repeat-c\u+061t"` is accepted and decoded as
`repeat-cat`, so with otherwise valid bindings malformed JSON reaches workload
execution. This breaks the protocol's strict JSON boundary. The hunter
suggests replacing the handwritten lexer with strict deserialization.

## TOOL-024 - Worker request refusals omit the promised document paths

Reported by: library-protocol [P3].

The contract in `reference/library-protocol.md` promises a document path for
schema and integer refusals. The worker reports duplicate keys and fractional
numbers by byte offset only (`json.rs`), and a missing required field produces
only "the request lacks a required key" (`request.rs`). These inputs are
refused, but callers cannot identify the offending field as promised.

## TOOL-025 - Quadratic work in run allocation and the worker string parser

Reported by: library-protocol (maintenance and performance observations).

Run allocation (`crates/library/src/run.rs`) searches the permanently reserved
IDs from zero on every allocation, so N runs make O(N^2) failed create
attempts. The worker's string parser revalidates the whole remaining input as
UTF-8 while consuming one character, so long request strings parse in
quadratic time.

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
