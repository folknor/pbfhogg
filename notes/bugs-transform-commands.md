# Bug hunt: passthrough and transform commands

Hunt date 2026-08-12. Single Opus hunter, scope: `src/commands/` (cat, sort,
repack, degrade, renumber, getid, getparents, inspect, tags_count, tags_filter,
check, export, mod.rs), `src/blob_meta/`, `src/tag_expr.rs`. Findings only,
unverified.

## Filed 2026-08-20 from this document's own round-1 fix pass

### `check --refs` drops an unrecordable node ID silently, then reports its refs as missing

`src/commands/check/refs.rs` populates `node_ids` / `way_ids` / `relation_ids`
with `IdSet::set_atomic_checked` and discards the returned `bool`. `false` means
the ID fell outside the pre-allocated range - now bounded both by the file's
indexdata and by `check::id_alloc_bound` - so the element is never entered into
the set and every ref pointing at it is subsequently reported as a missing ref.

That is a deliberate over-report rather than a panic (the previous behaviour),
and the report is not wrong in the sense of hiding anything. But it is
indistinguishable from a genuinely dangling ref, so a user reading the output
has no way to tell "your file references a node that isn't in it" from "your
file contains a node whose ID this check could not track". `verify_ids` faces
the identical situation and does report it, as `IdViolation::IdOutOfRange`;
`check --refs` should say something equivalent, at minimum a count on stderr.

Confidence: high (read from the code; the discarded `bool` is explicit).

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

The hunter would do one thing structurally, breaking:

1. Add one verification path - `pbfhogg check --index` plus a global
   `--distrust-metadata` - instead of each command inventing its own
   `require_indexdata` / `--force` pairing. `degrade` already exists to generate
   the adversarial corpus; there is currently nothing on the other side to catch
   what it produces.
