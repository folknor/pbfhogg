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

## BUG-007 - bench-read relation mode turns a role decode error into an empty role

Lateral finding from the BUG-002 scout, not yet judged. The `bench-read` relation
mode in `crates/cli/src/lib.rs` reads member roles with
`m.role().ok().unwrap_or_default()`, so a role that fails to decode counts as an
empty string and the run succeeds. A revision that breaks role decoding can then
complete less work and look faster, the same shape as the geocode-query error
swallowing.

Fix: fail the run on a decode error. Enforcement: an instrument test with a
deliberately failing role lookup.

## BUG-008 - A lint comment says nothing builds against musl

Lateral finding from the BUG-004 scout, not yet judged. The comment on the
`glibc-allocator-calls-live-in-debug` rule in `brokkr.d/lints.toml` says nothing
here builds against musl, while `brokkr.d/checks.toml` defines a `musl` sweep. The
rule's rationale may rest on the false statement.

Fix: establish whether the rule still holds under the musl sweep and reword the
comment to what is true.
