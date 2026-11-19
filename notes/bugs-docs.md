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

## DOC-004 - Deliberate protobuf refusals are missing from DEVIATIONS.md

Reported by: read-framing, sort-renumber.

The project says every deliberate deviation from protobuf is recorded in
`DEVIATIONS.md`, and `docs/cli/deviations.md` points there. Missing entries:
the multi-payload Blob refusal, which `reference/truncation-handling.md`
acknowledges as a compatibility departure; and renumber's refusal of legal
unpacked fields and split packed segments (`renumber/wire_rewrite.rs`), also
described only in `truncation-handling.md`. ALTW's undocumented framing
refusal is filed as CMD-014 in `notes/bugs-commands.md`.

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

## DOC-015 - diff count and quiet-mode documentation

Reported by: diff.

`docs/cli/commands.md` says single-sided counts are claimed counts, but the
implementation counts decoded elements (`osc/merge_join.rs`). It also says
quiet diff makes the same refusals as text mode without `-c`, but quiet mode
enables byte-equal skipping and can accept corrupt bodies that the ordinary
text diff rejects.

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

## DOC-025 - CORRECTNESS.md describes two frame-check funnels

Reported by: the FMT-001 scout (2026-11-18), not verified by an
implementer.

The "BlobHeader.datasize cap" section of `CORRECTNESS.md` still describes
two separate frame-check funnels, while `crates/lib/src/read/frame.rs` has
had a single frame state machine since `307982d7`.

## DOC-028 - The exit status of a publication-time HoldsInput refusal is undocumented

Reported by: the FMT-023/GEO-001/GEO-002 implementer (2026-11-19), not
verified.

`build-geocode-index` now refuses at final inspection when replacing the
index directory would delete the input PBF (`OutputError::HoldsInput`).
`docs/cli/commands.md` describes the refusal but not its exit status; the
CLI's mapping of that error kind was not checked. Separately, the
`IndexScratch` report text still says the old index's buckets are deleted
with it, without "when replacement succeeds".

## DOC-026 - apply-changes comments describe a timed receive and a relation-first fixture that is not

Reported by: the CMD-001 scout and implementer (2026-11-18), not verified
further.

- The single-worker comment in
  `crates/lib/src/commands/apply_changes/rewrite.rs` still says "within one
  receive timeout"; the drain now blocks on its channel and is woken by a
  message.
- The `relation-first` fault fixture in
  `crates/lib/tests/fault_apply_changes.rs` contains base nodes, so its name
  promises a shape it does not build. The relation-only shape is covered by
  `merge_writes_node_and_way_upserts_before_a_relation_only_base_with_locations`
  in `crates/cli/tests/cli_apply_changes.rs`.

## DOC-027 - diff input checks promise a claim refusal the one-sided shortcut skips

Reported by: the CMD-010 spar partner (2026-11-19), checked against the
code by the CMD-010 implementer, not fixed.

- `docs/cli/commands.md` "diff", "Input checks", says that where a block
  holds elements past its claimed last ID the merge refuses the claim when
  it meets them. `block_pair_merge_phase` (`osc/merge_join.rs`) emits a
  block whose claim lies entirely before the peer's as one run without
  walking it against the claim, so old `[2]` against new `[1, 2]` claiming
  `[1, 1]` can print `+n2` then `-n2` with no refusal. The refusal holds
  only where a decoded pair walk meets the contradiction. Either the
  shortcut checks the run's last element against the claim, or the
  paragraph says where the check runs.
- The same paragraph lists the byte-identical skip conditions without the
  rest: both blobs fresh (no residual on either side) and neither crossing
  its work unit's window. A pair that matches but misses those is decoded,
  so the corrupt-body exception is narrower than stated.
