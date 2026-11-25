# Documentation contradictions

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

Places where the 2026-11-17 defect hunt found a durable document or doc
comment contradicting the code, and the hunter judged the document to be the
wrong side. Every finding was made by one codex hunter per scope reading
source at `63d4ec1d`. Where a hunter judged the code wrong instead, the
finding is filed in the matching `notes/bugs-*.md` document. Entries are in no
order of importance.

## DOC-007 - Rebuild metadata docs describe behaviour the encoder dropped

Reported by: write-encoding, sort-renumber, passthrough-rewrite.

`docs/cli/commands.md` (the block-rebuilding description) says rebuilt nodes
become dense, versionless metadata is discarded and missing fields become
defaults. `CORRECTNESS.md` says metadata-less nodes get zero-filled DenseInfo
padding next to other nodes. cat's emitted normalization note
(`cat/mod.rs`), and the fallback's comments, repeat these claims. The current
rebuild path uses `element_metadata` and `add_node_partial`, keeps partial
fields, and can emit plain nodes: a versionless node carrying only a
timestamp keeps that timestamp. `begin_dense_node` (`write/block_builder.rs`)
seals the run when metadata presence changes. The code agrees with the newer
"absent metadata fields stay absent" deviation, so the older passages are
wrong.

## DOC-008 - The copy_range transport is described as kernel-space for io_uring

Reported by: write-backends.

`supports_copy_range` (`write/options.rs`) describes kernel-space splicing and
includes `Uring`, but `UringSink::copy_range` (`write/uring_writer.rs`) reads
through userspace into registered buffers. `reference/pipeline.md` also
wrongly attributes `copy_file_range` to `write_raw` and `write_raw_chunks`.
The implementations preserve bytes, so the descriptions should change.

## DOC-009 - threads.rs promises an unjoined result is destroyed on its worker

Reported by: concurrency-host.

The doc in `threads.rs` promises that an unjoined result is destroyed on its
worker thread. The result lives in an `Arc<Mutex<Option<T>>>`, so if the
worker finishes before the caller drops its handle, the caller destroys it.
The implementation gives no guarantee about which thread does the destroying.

## DOC-010 - IdSet promises an exact pre_allocate bound it does not enforce

Reported by: shared-primitives.

The doc in `idset.rs` says every ID above `pre_allocate(max_id)` panics, and
that the checked variants reject such IDs. `dense_for_atomic` checks only
whether the ID's dense chunk exists: after `pre_allocate(1)`,
`set_atomic_checked(2)` succeeds and `set_atomic(2)` does not panic, and
automatic promotion through ordinary inserts makes a chunk atomically
writable without preallocation. These methods check available dense storage,
not the original bound. The hunter found no caller that depends on the
stronger promise.

## DOC-011 - pipeline.md says decoded kind claims cannot be checked

Reported by: shared-primitives.

`reference/pipeline.md` says `repack` and `degrade` cannot tell indexed blobs
from replicated unindexed blobs at the worker boundary. The schedule now keeps
that distinction, and `ScheduledBlob::buffer` (`scan/classify.rs`) passes the
claim into block validation, which refuses foreign kinds. The explanation is
obsolete and contradicts `reference/indexdata-trust.md`.

## DOC-013 - CORRECTNESS.md says every OSC consumer drops delete children

Reported by: osc-parsing, apply-merge.

merge-changes and OSC tags-filter deliberately preserve delete bodies,
consistent with the CLI contract in `docs/cli/commands.md`. `CORRECTNESS.md`
should distinguish overlay consumption from OSC re-emission.

## DOC-014 - merge-changes "nothing the element carries is dropped" overstates re-emission

Reported by: osc-parsing.

`parse_nd` (`osc/xml_parse.rs`) keeps only `ref`, so the emitter
(`merge_changes/emit.rs`) strips any supplied `<nd lat="..." lon="...">`
locations. The preservation claim in `docs/cli/commands.md` needs narrowing,
unless preserving those fields is intended.

## DOC-015 - diff quiet-mode documentation

Reported by: diff.

`docs/cli/commands.md` says quiet diff makes the same refusals as text mode
without `-c`, but quiet mode enables byte-equal skipping and can accept
corrupt bodies, and believe false claims on a skipped pair, that the
ordinary text diff rejects.

## DOC-016 - The indexdata trust table says renumber refuses relation count mismatches

Reported by: sort-renumber.

`reference/indexdata-trust.md` promises that renumber refuses a decoded count
mismatch. The node and way stages do, but relation assembly
(`renumber/relations.rs`) never compares `outcome.count` with
`task.element_count`, so a relation blob that claims zero while containing
one relation succeeds. The hunter says the documentation is wrong here,
because relation numbering uses observed ranks and the claimed count does not
control the IDs.

## DOC-017 - The overlap trust description describes the retired algorithm

Reported by: sort-renumber.

`reference/indexdata-trust.md` says intra-entry order is unchecked and
repaired by the heap, with claims verified before later output. The current
sweep refuses lazy-cursor descents and checks the final count and range when a
cursor is exhausted. The page should describe the current streaming
validation.

## DOC-019 - DEVIATIONS.md says export assembles boundary relations

Reported by: export.

Export's element dispatch (`export/mod.rs`) ignores every relation, which
matches the CLI documentation and the deferred-work notes. The deviation
rationale is wrong, and missing relation support is not an implementation
defect.

## DOC-021 - check --index partial-index exceptions are documented as passing

Reported by: check-inspect.

`docs/cli/commands.md` and `reference/indexdata-trust.md` say `check --index`
passes an unindexed empty or mixed blob among indexed blobs. The code reports
a coverage violation (`check/index.rs`), and `crates/cli/tests/cli_check_index.rs`
explicitly pins that behaviour.

## DOC-022 - indexdata-trust.md says no command verifies header feature claims

Reported by: check-inspect.

`check --locations` now verifies the LocationsOnWays claim within its
documented limits. `docs/guide/indexdata.md` repeats the same stale claim
("the header's feature flags ... are not checked at all").

## DOC-024 - diff test comments still describe a header walk and ID-range shards

Reported by: the negative-ID documents commit's spar (codex).

`crates/lib/src/commands/diff/units.rs` describes one engine at every `-j`:
a planner streams each input once through a blob cursor and cuts work units
in canonical key windows; no header is walked ahead of the merge, and `-j`
is a worker limit, not a shard count. Test comments still describe the
older shape, sign-neutrally: the module doc of
`crates/cli/tests/cli_merge_join_trust.rs` (a header walk running
`BlobOrderGuard` over both whole files, then shards) and its comments on
the one header walk every `-j` runs, on reading every header up front and
on the excluded kind's blob; and the shard vocabulary of
`crates/lib/tests/roundtrip_invariants.rs` (one shard or several, `-j 4`
planning four node shards). The negative-ID fallback wording in these files
is already corrected; the rest needs checking against the planner's actual
cuts before rewording, since some assertions may depend on how many units a
fixture is cut into.

## DOC-032 - Unknown extension blob types are skipped with no DEVIATIONS.md entry

Reported by: the BUG-002 implementer (2026-11-23), from the scout's lateral.

pbfhogg accepts and skips a blob whose type is neither `OSMHeader` nor
`OSMData`, given a positive datasize (`read/frame.rs` keeps the existing
skip policy for unknown extension blobs, and
`reference/truncation-handling.md` says the low-level APIs keep it).
libosmium refuses any data blob that is not `OSMData`. `DEVIATIONS.md`
records every deliberate deviation from osmium, but has no entry for this
one (searched for "unknown blob", "extension blob", "unknown type").
`CORRECTNESS.md` was already narrowed to claim reference-reader acceptance
only for the upper size bound. Either record the deviation or change the
policy.

## DOC-031 - Undeclared way coordinate arrays have no documented behaviour

Reported by: the TOOL-045 refusal (bf9266ed). Needs a maintainer decision.

A way can carry lat/lon coordinate arrays while the header does not declare
`LocationsOnWays`. `docs/cli/commands.md` says nothing about what `cat`,
`sort`, `getid --invert` and `cat --dedupe` do with such arrays. Today's
behaviour, read from source at bf9266ed: blobs copied whole keep the arrays
as bytes; re-encoded ways drop them; `cat`'s rebuild of a blob it cannot
classify keeps them. pbfhogg never declares `LocationsOnWays` on output when
the input does not, so the verifier compares these outputs without
coordinates and the arrays are outside every verdict. `repack` drops them by
design (pinned by `repack_strips_coords_when_input_flag_absent`) and
`degrade` always drops them; both are already settled.

The decision: either document the per-command behaviour as it is, which
makes byte-carry on passthrough a contract; or document that undeclared
arrays are unspecified and may be kept or dropped; or make the commands
treat them uniformly (always drop, as repack does). The first two are doc
edits; the third changes passthrough output and needs measuring.

## DOC-033 - OSC child-read differences from libosmium are not in DEVIATIONS.md

Reported by: the OSC trailing-attribute fix (scout).

pbfhogg refuses a `<tag>` without `k` or `v` ("missing attribute 'k'");
libosmium's `get_tag` (`xml_input_format.hpp`) reads an absent `k` or `v` as
the empty string. pbfhogg ignores `lat`/`lon` on an `<nd>`, while libosmium
parses and validates them as a location (DOC-014 covers the re-emission
drop). Neither difference is recorded in DEVIATIONS.md.

## DOC-034 - No durable document says when inspect JSON bumps schema_version

Reported by: the spar accounting of the inspect tagged_nodes null change
(window 2).

`inspect --json` carries `schema_version` (3 at the time of filing, hard-coded
in `crates/lib/src/commands/inspect/json.rs`). Its history follows an unwritten
policy: removing `ordering.sequence` bumped it, adding the key
`data.has_negative_ids` did not, and turning `elements.tagged_nodes` from a
number into a nullable number bumped it. The rule applied in that last change
was "changing an existing field's type, including to nullable, bumps the
version; a purely added key does not". It is recorded only in a transient
session ledger. Neither `docs/cli/commands.md` (the `--json` row names only
the current version), the doc comment in `json.rs`, nor CHANGELOG states it,
so a JSON consumer cannot tell what a version change promises, and the next
schema edit has nothing to follow.

## DOC-040 - Decide how to account for renumber copying invalid key indices

The rationale above `Outcome` in
`crates/lib/src/commands/renumber/wire_rewrite.rs` says corrupt key indices
pass through as on the read side. Renumber copies the fields without checking
their indices, while `validate_tag_arrays` and `validate_dense_keys_vals`
in `crates/lib/src/read/block.rs` reject out-of-range indices during block
construction.

Decide whether to document this difference as intentional at the copy sites
or to align renumber with the reader. Moving the existing rationale would
carry its stale claim to the copy sites.

## DOC-041 - Versions below -1 are accepted with no DEVIATIONS.md entry

Reported by: the scout of the dense-claims doc fix, verified 2026-11-24.

libosmium refuses a version below -1 ("object version must not be
negative", `pbf_decoder.hpp`, plain Info and DenseInfo alike). pbfhogg
accepts it and reads it as "no version" for claims; only CORRECTNESS.md's
inspect --extended section says so, and DEVIATIONS.md has no entry. The two
encodings also differ on re-encode: plain Info maps only -1 to absent
(`WireInfo::parse`), so -2 stays raw behind `claimed_version`, while the
dense re-encode (`reencoded_version` in `owned.rs`) writes -2 as decoded.
Either the deviation is recorded, or the input is refused as libosmium does.

## DOC-042 - Partial way coordinate arrays are refused with no DEVIATIONS.md entry

Reported by: the scout of the node_locations doc fix, verified 2026-11-24.

libosmium's `decode_way` (`pbf_decoder.hpp`) decodes refs and inline
coordinates while all three arrays still have entries, so a way with lat
present and lon missing or shorter yields a shortened (possibly empty) node
list. pbfhogg refuses such a block at construction
(`validate_way_geometry_arrays`, `read/block.rs`). DEVIATIONS.md has no
entry for this difference.

## DOC-043 - The reading guide implies field 20 needs an opt-in

Reported by: the renumber LocationsOnWays item (2026-11-24), spar
session 01a20ea3-2302-7893-b81b-e3d577f46209.

`docs/guide/reading.md` "WayMembers-v1 / SharedNodePins-v1 (opt-in)" says
reading the two extensions back requires opting in. `set_parse_waymembers`
gates only BlobHeader field 5; `Way::shared_node_pins()` returns field 20
whenever the bytes are present, whatever the header declares. ADR-0007
makes field 20 authoritative only where `pbfhogg.SharedNodePins-v1` is
declared, and splice outputs (apply-changes, renumber) carry it
undeclared, so the guide should tell a caller to check
`HeaderBlock::has_shared_node_pins_v1()` before using the bitmap.

## DOC-044 - The advanced guide says multi-extract runs in a single pass

Reported by: the spar accounting of the fused sorted simple extract change,
from its scout's lateral (2026-11-24).

`docs/guide/advanced.md` introduces the config form with "Extract multiple
regions in a single pass using a JSON config file". The library doc of
`extract_multi` (`extract/mod.rs`) and `multi.rs` now say what the simple
multi-extract does: one run over the input for all regions, each kind
visited twice (classify, then write). Complete, smart and cleaning runs go
one region at a time. The guide should say what the run shares rather than
"single pass". The I/O reach table's "config single-pass outputs" names the
route, is test-pinned in `crates/cli/src/io_reach.rs`, and is not this
claim.

## DOC-045 - multi.rs says multi-extract is simple-only for memory reasons

Reported by: the spar accounting of the fused sorted simple extract change,
from its scout's lateral (2026-11-24).

The doc of `try_extract_multi_single_pass` (`extract/multi.rs`) says
"Simple strategy only (no per-region way/relation ID tracking beyond what
fits in memory)". Since the shared `Membership` (`extract/membership.rs`)
replaced per-region ID sets, membership no longer scales with per-region
sets the way that sentence implies; the restriction is the strategy's
dependency shape. `notes/todo.md` "Multi-extract v2 complete/smart
strategies" repeats the per-region `bbox_node_ids` / `all_way_node_ids`
premise. Both should state the current reason.

## DOC-046 - extract.md credits the sorted single-pass gain to the wrong commit

Reported by: the spar accounting of the fused sorted simple extract change,
from its scout's lateral (2026-11-24).

`reference/performance/extract.md` History "Historical (sorted pass1
optimization, commit `37b7c19`)" gives "Denmark -14% (2625->2259ms), Japan
-8% (12,619->11,643ms)" for simple. Those percentages are in `1fea0b77`'s
message (single-pass simple for sorted input: 2625 -> 2277 ms, 12619 ->
11948 ms); `37b7c19e` is the complete/smart pass-1 fast path. The heading,
commit and figures should match one measured change.

## DOC-047 - The sequential reader's "6x slower" has no recorded measurement

Reported by: the spar accounting of the parallel export change, from its
scout's lateral (2026-11-24).

`ElementReader::for_each` (`read/reader.rs`) says it is "6x slower" than
`for_each_pipelined` on large files; `reference/pipeline.md` ("~6x slower
than pipelined") and `docs/guide/reading.md` repeat it. The figure dates
from `a39d7fe7` (2026-03-02) and no cell in `reference/performance/` backs
it; the reader has changed many times since. Either record a measurement
with commit and host, or reword without the ratio.

## DOC-048 - check/mod.rs lists --ids only with --full

Reported by: the spar accounting of the fused default check pair, from its
scout's lateral (2026-11-25).

The `check/mod.rs` module doc lists "Four modes" and gives the ID check as
"`--ids --full`: ID uniqueness and ordering". Plain `--ids` (order only,
uniqueness proved only for an ordered kind) also runs `verify_ids`, and the
same doc's last paragraph names `--ids` alone as half the default pair.
Name `--ids` as the mode and `--full` as its duplicate pass.

## DOC-049 - check.md points "above" at an entry that sits below

Reported by: the spar accounting of the fused default check pair, from its
scout's lateral (2026-11-25).

`reference/performance/check.md`, section "check --ids --full proves an
ordered kind duplicate-free (bygg, 2026-11-08 ...)", says the plain order
scan "until the 2026-11-18 entry above, recorded negative IDs as
`IdOutOfRange`"; the 2026-11-18 section ("check handles negative IDs
exactly") follows it in the file. Reword to name the section rather than
its position.

## DOC-050 - README says no history dataset is configured or benchmarked

Reported by: the spar accounting of the parallel history time-filter
change, from its scout's lateral (2026-11-25).

`README.md`'s note under the planet table says history-shaped input is
outside the validation surface: "no history dataset is configured or
benchmarked". bygg's host config now registers a `planet` snapshot of a
two-version osmium merge, and `reference/performance/time-filter.md`
records history-path cells on it. `notes/todo.md` was rewritten to say a
real multi-version history file is still absent; the README needs the same
correction while keeping the scope statement.

## DOC-051 - time-filter's selection rule differs from osmium's unrecorded

Reported by: the spar accounting of the parallel history time-filter
change, from its scout's lateral (2026-11-25); read from both sources, not
run.

On a history file osmium (`command_time_filter.cpp`, libosmium
`diff_object.hpp` `is_visible_at`) keeps a version whose timestamp is at
or before the cutoff when the next record of the same object has a later
timestamp, with no version rule. pbfhogg keeps the last record at or
before the cutoff and refuses versions that do not strictly ascend. They
agree on well-formed files; they answer differently where a later version
carries an earlier timestamp, and pbfhogg refuses input osmium accepts
(descending, repeated or absent versions). `DEVIATIONS.md` records only the
sequence-number difference for time-filter.
