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

## DOC-002 - truncation-handling.md says partial DenseInfo column sets are rejected

Reported by: read-framing, read-block-decoding.

The paragraph in `reference/truncation-handling.md` is stale. The validator
(`read/block.rs`), its tests and the current deviation entry deliberately
accept arbitrary column subsets. Both hunters say the code is correct.

## DOC-003 - claims.rs says negative versions yield no claims

Reported by: read-framing.

The doc in `read/claims.rs` says negative versions yield no claims. The
implementation keeps available non-padding metadata.

## DOC-005 - Way::node_locations claims to consult the header

Reported by: read-block-decoding.

The doc on `Way::node_locations()` (`read/elements.rs`) promises an empty
iterator without `LocationsOnWays`. The implementation reads whatever
coordinate arrays the way carries. The doc should describe payload presence.

## DOC-006 - The block APIs point to an ordering assertion that does not exist

Reported by: read-pipeline.

Their documentation (`read/reader.rs`) directs callers to `for_each_pipelined`
for a debug monotonicity assertion, and that method performs no ordering
check. The earlier statement in the same doc, that callers must verify
ordering, is accurate.

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

## DOC-012 - BlobFilter::with_node_bbox says spatial filtering needs v2 indexdata

Reported by: shared-primitives.

The doc (`blob_meta/mod.rs`) says spatial filtering is effective only with v2
indexdata. Deserialization also supplies spatial bounds from v3, which is what
current writers emit. It should say v2 and v3.

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

## DOC-018 - The extract API says simple multi-extract reads the input once

Reported by: extract [P3].

The public API in `extract/mod.rs` says simple multi-extract reads the input
once. Node classification (`extract/multi.rs`) finishes before a separate node
write pass, and ways and relations repeat that. Single-extract simple does the
same. Verified raw copies avoid the second decompression but still reread the
frame. The mechanism and its redesign are PERF-017 in `notes/performance.md`.

## DOC-019 - DEVIATIONS.md says export assembles boundary relations

Reported by: export.

Export's element dispatch (`export/mod.rs`) ignores every relation, which
matches the CLI documentation and the deferred-work notes. The deviation
rationale is wrong, and missing relation support is not an implementation
defect.

## DOC-020 - pipeline.md says geojson is buffered into one FeatureCollection

Reported by: export.

`FeatureWriter` (`export/writer.rs`) writes each feature immediately and keeps
only the current feature buffer. The architectural description in
`reference/pipeline.md` is wrong.

## DOC-021 - check --index partial-index exceptions are documented as passing

Reported by: check-inspect.

`docs/cli/commands.md` and `reference/indexdata-trust.md` say `check --index`
passes an unindexed empty or mixed blob among indexed blobs. The code reports
a coverage violation (`check/index.rs`), and `crates/cli/tests/cli_check_index.rs`
explicitly pins that behaviour.

## DOC-022 - indexdata-trust.md says no command verifies header feature claims

Reported by: check-inspect.

`check --locations` now verifies the LocationsOnWays claim within its
documented limits.

## DOC-023 - osmium-parity.md still recommends inspect -e

Reported by: check-inspect.

`reference/osmium-parity.md` recommends `inspect -e`. That flag now refuses
and names `--extended` instead.

## DOC-035 - osmium-parity.md pairs inspect -g header.bbox with a key osmium lacks

Reported by: the spar accounting of the inspect --distrust-metadata refusal.

`docs/cli/osmium-parity.md` and `reference/osmium-parity.md` show
`osmium fileinfo -g header.bbox input.pbf` as the counterpart of
`pbfhogg inspect -g header.bbox input.pbf`. osmium fileinfo has no
`header.bbox` key and refuses it as an unknown `--get` value; its key is
`header.boxes` (the known-key list in
`research/osmium-tool/src/command_fileinfo.cpp`). The example should name
`header.boxes` on the osmium side.

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

## DOC-036 - The Blob raw_size cross-check is stricter than libosmium with no DEVIATIONS.md entry

Reported by: the spar accounting of the ALTW coordinate-framing conversion,
from its scout's lateral (2026-11-24).

`reference/truncation-handling.md` (the Blob-envelope bullets) records that
once decompression has run, a declared `Blob.raw_size` that disagrees with
the payload actually produced is refused as corruption, and says pbfhogg is
"stricter than libosmium in one direction": libosmium sizes its output
buffer to `raw_size` and calls `uncompress`, so an oversized `raw_size`
succeeds there and yields a payload zero-padded to the declared length,
where pbfhogg refuses. AGENTS.md says every deliberate deviation from
osmium is recorded in DEVIATIONS.md, and the multi-payload Blob refusal
described next to it in the same document now has its DEVIATIONS.md
section, but `DEVIATIONS.md` has no entry for the `raw_size` cross-check
(searched for "raw_size" and "raw size"). Record the deviation in
DEVIATIONS.md, with the libosmium behaviour checked against
`research/libosmium/include/osmium/io/detail/pbf_decoder.hpp`.
