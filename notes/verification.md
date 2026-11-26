# Verification findings

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

Findings are as reported by the hunters and have not been verified. Hunters read
source; costs are derived from the code unless an entry says measured. "Reported
by" records which scopes raised a finding and is a fact about the reports, not a
verdict on the code. Entries are in no order of importance.

## VER-165 - No pinned-input fault cell drives sorted simple extract's classify-then-write route

Reported by: the spar accounting of the extract overlap-component item, from a
scout lateral (2026-11-26); read from source.

`crates/lib/tests/fault_extract_input.rs` pins literal open counts
(`Strategy::opens`, simple sorted 6) and transports (`Route::transports`) on
fixtures whose every kind the fused planner admits. Its module doc says a kind
the planner refuses "takes a classify and a write descriptor", but no cell
drives that route on sorted input: neither a wholly unindexed sorted file nor
an indexed one with an overlap that skips a blob or a component over
`EXTRACT_FUSED_COMPONENT_MAX`. That route has CLI and library parity tests
(`classify_then_write` in `crates/cli/tests/cli_extract.rs`,
`non_indexed_parity.rs`) but no pinned-input or cancellation cell, and real-data
verify cannot tell which route ran (reference/verify.md). Add a declared route
for it in the `pinned_cell::check` driver, single and multi.

Kind: coverage gap.

## VER-166 - No cancellation cell runs merge-changes --simplify

Reported by: the spar accounting of the merge-changes simplify item, from a
scout lateral (2026-11-26); read from source.

The `merge_changes` scenario in `crates/lib/tests/cancel.rs` runs
`MergeMode::Stream` only; `MergeMode::Simplify` appears in
`osc_parser_parity.rs`, the `merge_changes/mod.rs` unit tests and the CLI tests,
none of which trips cancellation. Simplify has its own stages (render jobs,
the dedupe sort and scan, per-action emit, the serial drain) and its own
`cancel.check()` points, and `docs/cli/commands.md` states its in-memory dedupe
has no cancellation point inside it. Add a simplify scenario so a cancel during
render or emit is held to the same no-publication outcome as the stream path.

Kind: coverage gap.

## VER-167 - No test pins that an input failure outranks a version regression in simplify

Reported by: the spar accounting of the merge-changes simplify item, from a
scout lateral (2026-11-26); read from source and tests.

`simplify.rs` documents that every input is read to its end before dedupe, so
an input failure always outranks a version regression. No test feeds simplify
an input that both regresses a version and is malformed or truncated later
(in the same or a later input): the truncation and refusal tests in
`cli_merge_changes.rs` and `osc_parser_parity.rs` carry no regression, and
`rendered_simplify_reports_the_owned_regression` and the verifier's
`expect_regression` pin only which regression is reported among several. Any
redesign that reduces before every input is read could invert the precedence
silently. Add a cell with a regression in input 1 and a malformed element in
input 2, expecting the input error.

Kind: coverage gap.
