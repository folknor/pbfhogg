# Bugs from the verification hunt

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

Findings are as reported by the hunters and have not been verified. Costs and
behaviour are derived from source reading unless an entry says otherwise.

## BUG-009 - Geocode Pass 3's span includes the header write and smoke test

Lateral finding from the BUG-006 scout, not yet judged. The geocode builder's smoke
test has no marker pair of its own, and `GEOCODE_PASS3_END` fires after it, so
Pass 3's sidecar span also counts the header write and the smoke test. Timing
attribution only, not correctness.

Fix: end the Pass 3 span before the header write and give the smoke test its own
START/END pair.

## BUG-008 - A lint comment says nothing builds against musl

Lateral finding from the BUG-004 scout, not yet judged. The comment on the
`glibc-allocator-calls-live-in-debug` rule in `brokkr.d/lints.toml` says nothing
here builds against musl, while `brokkr.d/checks.toml` defines a `musl` sweep. The
rule's rationale may rest on the false statement.

Fix: establish whether the rule still holds under the musl sweep and reword the
comment to what is true.
