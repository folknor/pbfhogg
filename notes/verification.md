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
