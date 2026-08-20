# Bug hunt: change stream (`src/osc/`, apply-changes, merge-changes, diff, time-filter)

Hunt date 2026-08-12. Single Opus hunter. Two fix rounds applied plus a
close pass on 2026-08-20; what remains below was left open deliberately,
with the reason stated. Everything else is fixed and removed from this
file - `git log` has the history.

## Resource shape: arena garbage in `CompactDiffOverlay`

Every `push_node` / `push_way` / `push_relation` appends a fresh record and
orphans the previous one; the index is overwritten but the bytes are never
reclaimed. Accumulating N OSCs that repeatedly touch the same objects grows
the arena by the sum of all versions, not the live set.

**Left open deliberately.** The version-regression gate added in round 2
takes most of the pressure off: the accumulation pattern that produced the
worst garbage - the same objects touched over and over by out-of-order or
repeated diffs - is now refused outright, so the remaining growth is bounded
by genuine forward version churn across a catch-up range. Reclaiming the
orphaned bytes means either compaction or a free list threaded through the
arena, both of which touch the offset-width work landed in round 1
(`388eb4b3`) and neither of which can be justified without a planet-scale
`load_all_diffs` measurement showing the arena is actually the ceiling. Do
that measurement first.

## Resource shape: `merge-changes --simplify` is fully in-memory

`build_simplify_stream` materialises every `Change` (owned `String` tags,
refs, members) for all inputs before dedupe. The code's own comment cites
26.3 M changes at planet 7-OSC. Since the dedupe key is `(u8, i64)` and only
the last change per object survives, a two-pass shape - scan for
last-occurrence offsets, then re-parse and emit - would cut peak RSS by
roughly the whole stream.

**Left open deliberately.** This is a genuine rewrite, not a tweak, and per
AGENTS.md it needs an interleaved matched A/B at planet scale to justify
itself rather than an asserted improvement. Round 2 was a correctness round
and did not have a measurement budget. Note that the two-pass shape now has
to preserve the version-precedence gate, which currently runs during the
single dedupe pass and would move into pass one.

## Absent-versus-zero survives in the PBF metadata representation

Found while unifying the OSC parsers in round 2, outside the original hunt.

`osc::compact::ArenaMeta` and `write::block_builder::Metadata` have no
per-field presence bit. An OSC element carrying `version="4"` and nothing
else is written to the PBF with `uid = 0`, `changeset = 0` and
`timestamp = 0` - real values, indistinguishable from a genuine user 0 or a
genuine epoch timestamp. This is the same collapse already fixed for
coordinates, for `push_info_attrs`, and (in round 2) for OSC `version`; it
survives here because absence and zero share one representation all the way
through the PBF write path.

`tests/cli_apply_changes.rs::merge_metadata_preservation` currently pins
this behaviour. It has been re-documented so that it records the collapse
rather than endorsing it, and its assertion message says what must change -
but until the representation gains presence bits, a test does assert the
bug.

**Left open deliberately.** The fix is per-field presence in both types plus
`Option`-shaped fields through `BlockBuilder`, which is a PBF-write-path
change rather than a change-stream one, and is wide enough to deserve its
own round.
