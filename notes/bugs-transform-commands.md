# Bug hunt: passthrough and transform commands

Hunt date 2026-08-12. Single Opus hunter, scope: `src/commands/` (cat, sort,
repack, degrade, renumber, getid, getparents, inspect, tags_count, tags_filter,
check, export, mod.rs), `src/blob_meta/`, `src/tag_expr.rs`. Findings only,
unverified.

## MEDIUM

### `pbfhogg check --index` does not exist

Filed 2026-08-20 from this document's round-2 fix pass, which closed the
opt-out half of the blob-level-trust finding but not the verification half.

There is still no command that decodes every blob and diffs what it finds
against what the blob's own `BlobHeader` claims - `indexdata` (kind, count,
min/max ID, bbox) and `tagdata` (the tag key set). Every consumer of that
metadata now either trusts it or can be told not to (`--distrust-metadata` on
`tags-filter` and `inspect`), but nothing can answer "is this file's metadata
true?", so a file with a lying hint is only detectable by running two commands
and comparing.

The adversarial corpus for such a command now exists: `degrade
--corrupt-tagdata`, `--corrupt-indexdata` and `--corrupt-indexdata-kind`
produce well-formed hints that lie about, respectively, the tag key set, the
count and ID range, and the element kind. All three are already used by the
`--distrust-metadata` tests, and `tests/cli_degrade.rs` verifies against an
independent decoder that what they write is well-formed rather than garbage -
so a `check --index` built on them would be exercising belief, not rejection.

Shape: `pbfhogg check --index` reporting per-blob mismatches, routed through
`ExitStatus::AnswerNegative` (exit 1) for "found mismatches" rather than a new
exit code, and added to the exhaustive `validate_input_output_aliases` match in
`cli/src/main.rs`.

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
