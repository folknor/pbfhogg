# Bug hunt: passthrough and transform commands

Hunt date 2026-08-12. Single Opus hunter, scope: `src/commands/` (cat, sort,
repack, degrade, renumber, getid, getparents, inspect, tags_count, tags_filter,
check, export, mod.rs), `src/blob_meta/`, `src/tag_expr.rs`. Findings only,
unverified.

## HIGH

### Overlap-run detection is pairwise-adjacent, not transitive - `sort` and `cat --dedupe` can emit unsorted output stamped `Sort.Type_then_ID`

`src/commands/sort/mod.rs::detect_overlaps` and the identical copy in
`src/commands/cat/dedupe.rs::detect_overlaps` mark `overlaps[i]` /
`overlaps[i+1]` only when adjacent entries in the min-key-sorted list overlap.
Overlap is not transitive under that test.

Concrete failure (same kind, sorted by first key): A=[1..1000], B=[2..3],
C=[500..600].

- A/B overlap, both flagged. B/C: `B.max=3 < C.min=500`, so C is not flagged.
- The run is `[A,B]`. `sweep_merge_local` flushes the heap "below the next
  entry's first id" and, at end of run, flushes everything remaining - emitting
  A's ids 500..1000.
- C is then written as a passthrough blob (ids 500..600) after those.
- Output is not sorted, yet `build_output_header(..., |hb| hb.sorted())`
  unconditionally stamps `Sort.Type_then_ID`.

In `cat/dedupe.rs` this additionally means A/C duplicates are never deduped, so
the "exact duplicates removed" contract silently fails. This is realistic for
`merge` of extracts, where per-blob ID ranges are wide and sparse. `degrade
--unsort` only produces one adjacent overlapping pair per kind, so the test
corpus cannot reach this case.

Fix: replace the pairwise scan with a running-max sweep
(`run_max = max(run_max, entry.last_key)`; extend the run while
`next.first_key <= run_max`), kind-bounded as today.

Confidence: high (derived from the code, not executed).

### `check --ids --full` panics on IDs outside hardcoded ceilings - including negative IDs and near-future way IDs

`src/commands/check/verify_ids.rs::verify_ids_full_parallel` pre-allocates
`node_ids.pre_allocate(14_000_000_000)`, `way_ids.pre_allocate(1_500_000_000)`,
`relation_ids.pre_allocate(25_000_000)`, and workers call
`IdSet::set_atomic_if_new`, which does `let id = id as u64` and panics
(`chunk_for_atomic`) when the chunk was not pre-allocated.

- Any negative ID (JOSM staging IDs, which `getid` explicitly rejects with a
  friendly message) becomes a huge `u64`, so guaranteed panic.
- The way ceiling is 1.5e9. Live OSM way IDs are already ~1.4e9.
  `check --ids --full` on a future planet will panic rather than report.
- A degraded or hostile file with any out-of-range ID panics a worker rather than
  producing a violation report - which is exactly the file `check` exists to
  diagnose.

The verifier should clamp or bounds-check and report an `IdViolation` (or grow
the set), never panic.

Confidence: high.

### Mixed-kind within a single PrimitiveGroup is undetected - indexdata and tag index both lie, and five commands act on it

`src/blob_meta/scan_ids.rs::scan_primitive_group` returns at the first element
field it encounters (`(3,2)` Way to `scan_repeated_element_ids(..., 3, Way)`), so
a group containing e.g. ways and relations is indexed as `kind=Way` with `count`
= ways only. The mixed-kind guard in `scan_block_ids_checked` only compares
across groups, so it never fires. `tag_index.rs::scan_group_tag_keys` has the
same shape (returns after the first kind), so the tag index omits the off-kind
elements' keys.

Downstream consequences, all silent:

- `cat --type way` passes the blob through raw, so relations appear in a way-only
  output. `cat --type relation` skips it, so those relations vanish.
- `sort`: `compute_overlap_run_local`'s kind-gated `extract` closure drops
  off-kind elements (`_ => {}`) if the blob lands in an overlap run; if not, it
  passes through raw with a wrong index.
- `repack`: the blob is only in the way schedule, so its relations are never
  emitted.
- `renumber`: `build_all_blob_schedules` files it under Way; the wire-splice
  rewrites only way IDs, leaving off-kind elements with unrenumbered IDs in the
  way stream.
- `tags-filter`: the incomplete tag index can cause an over-eager blob skip
  (missing matches).

The scanner already knows how to say "I cannot index this" - return `None` for a
group that contains more than one element field, exactly as the cross-group case
does.

Confidence: high on the scanner behaviour; the downstream consequences follow
mechanically.

### `TagIndex::serialize` truncates to `u16` and produces an unparseable or undersized index

`src/blob_meta/tag_index.rs`: both `self.keys.len() as u16` and
`key.len() as u16` truncate silently.

- A key longer than 65535 bytes writes a wrapped length followed by the full
  bytes, so `deserialize` desynchronizes and yields garbage keys from that point,
  or bails and returns `None`.
- More than 65535 distinct keys in one blob writes a wrapped count, so
  `deserialize` returns a truncated key set that still looks valid.
  `tags_filter::classify_blob_filter_check` then skips blobs whose keys were
  dropped, so `pbfhogg tags-filter` produces silently incomplete output with a
  cheerful "N blobs skipped by tag index" line.

Neither bound is checked and neither is reachable from OSM data today, but
`degrade`'s whole premise is that inputs may be hostile. The writer should emit
no tag index at all when it cannot encode one faithfully.

### `renumber` derives ID assignment from unverified indexdata

`src/commands/renumber/schedule.rs` takes `element_count` and `max_id` straight
from `BlobIndex` and `mod.rs` uses them to precompute each blob's `base_new_id`
before any decode, and to size the node `IdSet` (`pre_allocate(max_node_id)`).

- A wrong `count` on a way or relation blob produces overlapping new ID ranges,
  i.e. duplicate IDs in output. Only the node total is cross-checked ("pass1 node
  count mismatch"); ways and relations have no equivalent guard.
- A `max_id` smaller than the true max makes `IdSet::set_atomic` panic with the
  "pre_allocate only covers..." message the code comment explicitly says it
  wanted to avoid (it defends against a false `sorted` claim but not against
  false indexdata).
- Negative node IDs give `id as u64` and the same panic. Unlike `getid`,
  `renumber` has no negative-ID rejection.

Add the same count-mismatch assertion for ways and relations, and reject negative
IDs up front.

## MEDIUM

### `check --ids` uses raw `<=` while everything else uses canonical OSM ordering

`verify_ids.rs` compares `id <= p` and `fi <= pl`. `src/osm_id.rs::osm_id_cmp`
defines canonical order as 0, -1, -2, ..., 1, 2, ... and is what `sort`,
`scan_block_ids_checked` and the blob overlap logic use. So on a file with
negative IDs, `sort`'s own correctly-ordered output is reported non-monotonic by
`check --ids`, and a raw-i64-sorted file passes `check` while `sort` considers it
unsorted. The verifier is the one place that must agree with the definition.

### `tag_expr` silently mis-parses osmium-compatible expressions instead of erroring

`src/tag_expr.rs::parse_tag_matcher` vs
`research/osmium-tool/src/util.cpp::get_string_matcher`:

- osmium supports `*` prefix/suffix/substring matching on keys and values
  (`highway=res*`, `name=*strasse`, `*:name`). pbfhogg supports only a key ending
  in `:*` - `highway=res*` becomes an exact match on the literal string `res*`
  and matches nothing. No error, empty output.
- osmium supports `*` alone as "any value": `highway=*`. pbfhogg matches the
  literal `*`.
- osmium supports comma lists on keys as well as values (`highway,railway`).
  pbfhogg only on values; `amenity,shop` becomes one literal key.
- osmium strips whitespace around key and value. pbfhogg does not, so
  `highway = primary` from an `--expressions` file yields the key `"highway "`.

Every one of these fails as "zero matches" rather than "bad expression". Given
`tags-filter` / `tags-count` are advertised as osmium equivalents, either
implement the matcher or reject unsupported syntax loudly. (The `#` inline-comment
stripping in `read_expressions_file` is osmium-faithful - the hunter checked; not
a finding.)

### Blob-level tag-index skipping is an unverifiable trust boundary with no opt-out

`tags_filter` skips whole blobs on `TagIndex` content in both the `-R` single
pass and two-pass pass 1. Nothing ever validates that a blob's tagdata matches its
payload, `degrade --strip-tagdata` only removes it (never corrupts it, so no test
covers a wrong index), and there is no `--no-blob-skip` escape hatch. Combined
with the mixed-kind and u16-truncation findings this is the most likely route to
silently-wrong output in this scope. At minimum there should be a global "do not
trust sidecar metadata" flag, and ideally a `pbfhogg check --index` that decodes
every blob and diffs the scanned index and tag index against the stored one. That
command does not exist today.

### `inspect`'s index-only fast path is not defeatable

`src/commands/inspect/scan.rs::try_index_only_scan` reports counts, ID ranges and
per-block stats purely from indexdata whenever `--locations` / `--extended` are
absent, and there is no flag to force a decode. `inspect` is the tool you reach
for when a file is suspect, and on a suspect file it reports the file's own claims
as measurements. Add `--no-index` (or make `--anomalies` imply full decode).

### `cat` of multiple files keeps only the first file's header

`cat_passthrough` / `cat_type_passthrough` / `cat_filtered` build the output
header from `files[0]` only. The bbox is not unioned across inputs and
replication fields are taken from an arbitrary file, so a multi-file `cat` output
advertises a bbox that does not cover its own data. `single_file` correctly drops
the sorted flag, so this is bbox/metadata only - but it is wrong output metadata,
not just cosmetics.

## LOW / lateral

- `cat --clean` on a non-indexed input decodes every blob three times.
  `require_indexdata` in `cat/mod.rs` only fires when `type_filter.is_some()`, so
  `--clean` alone reaches `cat_filtered`, and `build_classify_schedules_split`
  replicates unindexed blobs into all three per-kind schedules. Correct, but 3x
  the work with no warning. Same 3x applies to `check --ids` on unindexed input.
- `removeid` reports `idx.count` (unverified indexdata) as "elements written" for
  raw-passthrough blobs (`getid/mod.rs`); same for `sort`'s `count_entry`. Stats
  derived from metadata the command never read.
- `tags_count` ignores the tag index. It parses the same expressions as
  `tags-filter` but builds no `BlobFilter`, so `tags-count -e highway=*` decodes
  every blob at planet. Free win, same helper
  (`blob_filter_from_expressions`) already exists next door.
- `repack --elements-per-blob N` pre-allocates `2*N` entries per `BlockBuilder`,
  one per worker plus the central one; a pathological `N` is an unbounded
  allocation from a CLI integer.
- `getparents_walker` keeps decoding after a write error. `write_err`
  short-circuits the merge closure but `parallel_classify_phase` runs the full
  schedule to completion; on a planet-scale run a failed first write costs the
  entire decode before the error surfaces.
- `BlobIndex::deserialize` treats an all-zero bbox as "no bbox"
  (`blob_meta/mod.rs`). Harmless (conservative) today, but it means a genuine
  single-node blob at exactly (0,0) loses spatial filtering, and it makes the
  encoding unable to represent that box.

## Structural recommendation

The recurring shape across every finding except the overlap-run, `check --ids`
panic, and ordering ones is: sidecar metadata (indexdata kind/count/range, tag
index, header `Sort` flag) is trusted as fact, there is no code path that
verifies it, and every consumer re-implements its own trust decision inline.
`CORRECTNESS.md` documents the sortedness trust boundary carefully, but the
indexdata kind/count boundary - which `renumber` bases ID assignment on and
`cat --type` bases raw passthrough on - is not documented as a trust boundary at
all.

The hunter would do two things structurally, both breaking:

1. Make `scan_block_ids` refuse to index anything ambiguous (single-group mixed
   kinds), so "indexdata exists" means "kind and count are exact" as a
   scanner-level invariant rather than an assumption spread across five commands.
2. Add one verification path - `pbfhogg check --index` plus a global
   `--distrust-metadata` - instead of each command inventing its own
   `require_indexdata` / `--force` pairing. `degrade` already exists to generate
   the adversarial corpus; there is currently nothing on the other side to catch
   what it produces.
