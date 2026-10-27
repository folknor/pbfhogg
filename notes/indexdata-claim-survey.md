# Indexdata claims that can publish a wrong answer: survey

Transient working note for the 2026-10-27 survey. Nothing durable may cite it;
delete it once its findings are in `reference/indexdata-trust.md` or decided
against.

The question: ADR-0013 makes blob indexdata (kind, ID range, count) and the
header's `Sort.Type_then_ID` claim believed by default, with `check --index
--ids` as the admission test, and `reference/indexdata-trust.md` records where
each command relies on what. A lying claim can therefore publish a wrong
output with exit 0, against the maintainer's ruling that a silent wrong answer
is worse than a refusal. The maintainer's other ruling bounds any fix: a check
that only refuses malformed input and costs per-byte or per-element work on a
hot path is the wrong trade. The candidate fix is a refusal only where a run
already decodes the blob, or already computes what would contradict the claim.

## Partial map from the first pass (unverified, Opus agent, stopped early)

### Shared machinery

- Every decode already sees kind, count and most IDs. All `PrimitiveBlock`
  constructors go through `finish`, which calls `validate_groups_strict`
  (`src/read/block.rs`): it parses every Node, Way and Relation message (each
  ID parsed on the way) and counts dense IDs with `packed_varint_count`. So
  the decoded kind set, the element count, and way, relation and plain-node
  IDs are free. A dense group's first ID is one varint; its last ID, and so
  dense min and max, need the deltas summed, which is per-element work.
- `BlockType` equality is the wrong kind test: `block_type()` reads only each
  group's first byte, and returns `Mixed` for dense plus plain nodes and for a
  changeset group. A kind test has to follow the kinds of the elements
  actually present.
- The typed error exists: `ErrorKind::IndexdataMismatch` with a reason, and
  the CLI already hints `check --index` for it. Current users:
  `osc/merge_join.rs` (diff), `altw/passthrough.rs` (inject_prepass),
  `check/locations/scan.rs`, `geocode_index/builder/pass2.rs`. The pools'
  `locate_blob` names the blob, so a refusal raised in a pool is located for
  free.
- DONE (slice 1a): `ScheduledBlob` now carries the claim under
  `KindClaims::Refuse`, `PrimitiveBlock` records its decoded kinds, and the
  three block pools and the pipelined decode step refuse a contradiction.
  Original: The classify pools do not carry the claim: `ScheduledBlob`
  (`src/scan/classify.rs`) holds none, and a size assert pins it at 24 bytes.
  `header_len` is under 64 KiB, so there are spare bits for a 2-bit kind
  claim. A partially indexed file is refused, so a schedule built with
  `kind_filter = Some(K)` on indexed input holds only blobs claiming K, which
  makes the kind claim a property of the whole schedule. Count or range
  checks in the pools need per-entry storage.
- The pipelined reader has the claim at decode: `src/read/pipeline.rs`
  `spawn_decode_task` holds `blob.index()` whenever a `BlobFilter` is set,
  next to the decode closure.
- Raw-bytes walkers (`parallel_scan_blobs_raw`, the `scan/way.rs` and
  `scan/node.rs` helpers) read every group field tag and skip unwanted ones,
  so spotting a foreign element field costs a flag per field already read.
  Callers: altw sparse, altw `mod.rs` pass 0, extract simple and multi, check
  `--locations`.
- No other producer writes this indexdata: libosmium only names BlobHeader
  field 2 and never writes it. Any indexdata that parses was written by
  pbfhogg or forged, so false positives can only come from pbfhogg's own
  producers, including older pbfhogg files whose scanner bugs under-claimed.

### Sites read so far

- apply-changes (`commands/apply_changes/streaming.rs`): splice and
  passthrough by claimed range, no decode (wrong answer, out of scope). The
  candidate path (`handle_candidate`) decodes; kind, range and upsert slice
  come from the claim. An understated range on a decoded blob is a wrong
  answer (an out-of-order create on the false-positive passthrough, a
  duplicate on the rewrite path). Free contradictions there: the decoded
  kind and count, `block_overlaps_diff` (which walks every element on a
  miss), and each rewritten block's `BlockBuilder` index. The `-l` refresh
  scan (`detect_stale_wire`) is unread for foreign group fields.
- getid: skips on claimed kind and range without decode (out of scope);
  decoded blobs go through `process_block`, which handles every kind by what
  it holds, so a lie there produces no wrong output for that blob.
- extract: `BlobDesc` (`commands/extract/common.rs`) carries claimed kind and
  bbox; skipping on them involves no decode. The raw-copy nominee is decoded
  and structure-checked; a payload holding ways or relations under a node
  claim becomes ineligible, not refused.
- Per-kind phases (`build_classify_schedule(Some(kind))`,
  `build_classify_schedules_split`, a pipelined `BlobFilter`): repack,
  degrade, tags-filter, tags-count, getparents, inspect `--nodes`, check
  `--ids` / `--refs`, time-filter, altw sparse and `mod.rs`, geocode pass1,
  pass1.5 and pass2, extract smart and multi. A mislabelled blob is invisible
  to the phase that needs it, and the phase that decodes it drops the
  off-kind elements: a wrong answer. A kind check after decode is free there,
  but catches the lie only when some phase decodes the claimed kind.
- Not yet read: sort and cat `--dedupe` (`commands/blob_merge.rs`), diff's
  indexed paths (`osc/merge_join.rs`, `diff/shard.rs`), altw external stages
  (stage1, stage2, stage4, `relation_scan.rs`), renumber (already refuses a
  count or kind mismatch), geocode pass2 schedules, `inspect/scan.rs` and
  `show_element.rs`, the `extract/multi.rs` raw scan.

### Leaning (not sparred)

- Exempt: `cat` in every mode (`cat --clean` re-encodes with fresh indexdata
  and is the remedy), `check --index` (reports rather than refuses),
  `inspect` descriptions, `sort` under an unsorted header (it already scans
  every blob and replaces false indexdata with a note). `degrade` undecided.
- ADR-0013 and `reference/indexdata-trust.md` say verification costs the
  decode the claim exists to skip; that holds for skipped and spliced blobs
  only. For blobs a run decodes anyway the comparison is free.
- DONE (slice 1a): the `IndexdataMismatch` hint now names `cat --reindex`,
  a new lossless repair (`cat --clean` strips an attribute and re-encodes,
  so it was never a clean repair). Original: No refusal names a remedy for a
  false claim: partial-index refusals point at `pbfhogg cat`, but plain
  `cat` passes an indexed blob through as it is; only `cat --clean` repairs
  one.
- DONE (slice 1a): ADR-0013 and `reference/indexdata-trust.md` no longer say
  verification costs the decode where the run decodes anyway (ADR-0013
  consequence 4).

## Survey results (four read-only Sonnet surveys, 2026-10-27, not re-verified)

Merged as reported. Each area agent read the code; rows they marked
inferred are marked so here.

### The structural facts the design turns on

- Every decode passes `validate_groups_strict`, which parses each element's
  ID and counts dense IDs, but retains nothing: using kind, count or IDs from
  it needs new fields and a return value. A dense group's min and max need
  summed deltas (per-element work). Changeset groups (field 5) are skipped
  uncounted.
- The raw-bytes walkers bypass validation entirely. They see every group
  field tag (a foreign kind is a flag per field) but no count and no IDs.
- `ScheduledBlob` carries no claim (24 bytes, at least 16 spare bits in
  `header_len` and 7 in `data_size`), so the classify phases cannot compare
  count or range though they hold the values. A per-kind schedule implies the
  kind claim for every entry, except `cat --clean`'s partial-accepting
  schedule and extract's `ScheduledBlob::at_frame` entries. altw external
  stage 2's `NodeBlobInfo` is the one structure that already carries the
  claimed range.
- The pipelined reader holds `blob.index()` next to the decode closure, but
  parses it only when a `BlobFilter` is set or a transform sets
  `PARSE_INDEXDATA`.
- Refusal vocabulary is split: `IndexdataMismatch` (which the CLI pairs with
  a `check --index` hint) at four sites; renumber, altw external
  `check_range` and stage 4 `assemble_block` use `invalid_data` or
  `new_unsorted` for the same kind of finding, and lose the hint. No refusal
  names `cat --clean`, the only repair.

### A. Wrong answers on blobs the run already decodes (contradiction free)

Slice 1b (2026-10-27) adopted the decode-time kind refusal at every per-kind
phase below except altw and time-filter: repack, degrade, the sort and dedupe
sweep, diff and derive, geocode, extract and multi-extract, tags-filter,
tags-count, `inspect --nodes`, getid and getparents. The kind rows of this
section are DONE for those commands; the range, order and count rows
(apply-changes, the sweep's pop order, diff `-c`, time-filter) and the altw
rows are not. The raw-walker adopters (extract simple, geocode Pass 1.5)
take the `KindSet` the `src/scan` node and way walkers now return from tags
they already read, so no adopter walks a blob twice.

- **DONE (slice 1a, 2026-10-27): check `--refs` and `--ids` (the admission
  test itself).** Both now stop and fail the file (exit 1) on a decoded
  element of another kind, through the shared `KindClaims::Refuse`
  machinery; `check --locations` follows the same outcome. Remaining gap,
  documented: a relation-claimed blob without `--check-relations`, and other
  kinds' blobs under `--ids -t`, are never decoded. Original finding: A way blob
  labelled node or relation is skipped by the way phase: dangling refs are
  never checked and duplicate or unsorted way IDs never examined, so the
  check *passes* a file it should fail. `check_type_order` uses the same
  claimed kinds. The evidence is the `_ => continue` / `None` arms the loops
  already reach. Only the kind claim can change either verdict (count and
  range only feed a clamped allocation hint).
- **Per-kind decode phases that drop off-kind elements:** repack and degrade
  (`worker_split_blob`, `worker_decode_kind`), the sort and `cat --dedupe`
  sweep (`Sweep::run`'s `_ => {}` arm, "silently drops off-kind elements"),
  diff and derive (`shard.rs` bucketing by claimed kind; derive's
  `write_delete` silently drops an off-kind delete), geocode passes 1, 1.5,
  2a and 2b, getid `--add-referenced` pass 2, tags-filter two-pass, extract
  smart and complete pass 3. Each already iterates the elements it drops. In
  geocode every kind is decoded by the pass of its claim, so a decode-time
  kind check there catches every kind lie; elsewhere it catches a lie only
  when some phase decodes the claimed kind.
- **apply-changes candidate path** (`handle_candidate`): an understated range
  on a decoded blob writes a create out of order under the sorted stamp
  (false-positive passthrough) or emits a modify twice (rewrite path).
  `block_overlaps_diff` walks every element on a miss and
  `rewrite_block_parallel` computes every `elem_id`, so actual first and
  last ID cost a compare per element in loops that already run; `Rewritten`
  could carry actual ranges to the drain, whose order guard checks only
  claimed ranges today. The `-l` refresh scan (`detect_stale_wire`) sees a
  foreign group field and every way ID for free.
- **apply-changes intra-blob order** under the header sort claim (inferred):
  in block `[5, 3]` with upsert 3, `rewrite_block_parallel` emits 3 as an
  early create and again at its element; `block_overlaps_diff`'s forward-only
  cursors can miss a pure delete. Free: one compare of `elem_id` with the
  previous.
- **sort / `cat --dedupe` sweep order:** an overstated claimed min makes the
  sweep pop out of order, undetected; `pop_one` and the write closure see
  every written element, so "not below the last written ID" is one compare.
- **altw sparse pass 1:** a node blob's tuples and every ID are visited in
  the rank loop; count and range checks are free if the claim reached the
  closure (it does not today). External stage 2 already reads `last()` and
  has the claimed range.
- **diff count-only paths:** `diff_stats` and `-c` single-sided counts use
  the claimed `index.count` even though the block is decoded (wrong number,
  right lines).
- **time-filter** trusts `Sort.Type_then_ID` for grouping versions: an
  unsorted file emits the same object more than once. The previous
  `(kind, id)` is already held, so the order check is one compare. No
  indexdata involved, and time-filter has no `--force`.
- **getid invert backstop:** a way blob mislabelled Node and re-encoded drops
  inline coordinates while the output keeps the enrichment declaration; the
  guard keys on `meta.index.is_none()` and could key on the claimed kind for
  free.

### B. Wrong answers on blobs the run never decodes (out of scope, document)

Skipped or spliced on the claim alone, with nothing free to check against:
apply-changes splice and passthrough; the sort and dedupe fast path
(`Walk::data_blob` under a sorted header); `cat --type` raw copy; getid
include, invert raw copy and pass 1 of `--add-referenced`; getparents;
tags-filter `-R`; tags-count and `inspect --tags --type`; `inspect --nodes`;
`inspect --show` (including its sorted early exit); extract and
multi-extract bbox-skipped node blobs (never decoded by any phase in simple
sorted and single-pass multi); altw raw copies (relation-labelled way blobs
copied without locations, `LocationsOnWays` still declared); altw external
stage 4's tagdata-and-range skip; diff's `blob_entirely_before` fast path
(no `check_claimed_max` there). A node bbox claim is checkable only by a
per-node region test.

Commands where no phase ever decodes a mislabelled blob: getparents,
tags-filter `-R`, tags-count, `inspect --nodes`, getid include and invert,
tags-filter two-pass in the nodes-only case, extract simple sorted and
single-pass multi for bbox-skipped node blobs.

### C. Existing refusals to reuse

renumber pass 1 and stage 2 (decoded count against claimed count,
`foreign_kind_error` in the wire rewrites); diff `check_claimed_max` (only
when two decoded blocks meet); altw `passthrough::decode_one` (only under
`inject_prepass`); `check --locations` (a way claim over another kind; the
opposite direction is routed on decoded kinds); geocode pass 2a's node-ID
bound; altw external `check_range` and `assemble_block`.

### D. Lateral findings (bugs or smells outside the claim question)

- altw `decode_one` (sparse; external stage 4 only when stage 1 also found
  zero ways) re-indexes a node blob labelled way as a Way blob with count 0
  and IDs 0..0: pbfhogg writes a false claim the next run believes. Free
  check: `stats.way_count` against `desc.count`.
- altw backends disagree on a way inside a non-way-claimed blob: external
  always refuses, sparse refuses only under `inject_prepass` and otherwise
  handles it, so `--index-type` changes what is accepted.
- `blob_filter_check` ignores `node_bbox`, though `uses_index()` counts a
  bbox as an index-backed filter (where a spatial `BlobFilter` reaches it is
  untraced).
- `ElementReader`'s sort-claim check is a `debug_assert` on node IDs only.
- `BlobIndex::deserialize` bounds neither `count` nor the ID range by kind;
  unreadable bytes on one blob count as unindexed and trip the
  partial-index refusal rather than a mismatch.
- `block_type()` returns `Mixed` for a changeset group too.
- `sort` under an unsorted header copies unverified tagdata when the index
  claim holds.
- repack's "cannot tell a mislabel from a replicated unindexed blob" comment
  looks stale now that partial indexes are refused.
- `inspect --show`'s sorted early exit compares numeric `min_id` against
  canonical (negative-first) order: on an honest file with negative IDs it
  can return "not found" (inferred, not run; negative IDs are invalid input
  under ADR-0002 anyway).
- `degrade --corrupt-indexdata` silently does nothing for a blob whose
  indexdata does not parse (inferred).
- A false sort claim is carried into extract's output header
  (`OutputFacts::carry`, inferred).
