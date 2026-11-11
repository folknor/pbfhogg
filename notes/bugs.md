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

## BUG-010 - Feature-gated code never builds for musl

Lateral finding from the BUG-008 work, not yet judged. The `musl` sweep in
`brokkr.d/checks.toml` builds default features only, so code behind
`linux-direct-io`, `linux-io-uring`, `test-hooks` and the profiling features
never compiles for musl. The allocator textlint exempts `crates/lib/src/debug.rs`
and `crates/lib/src/process.rs` wholesale, so an ungated glibc call added inside
either file behind one of those features escapes both guards. Source reading
suggests the Linux features build on musl (`direct_io.rs` already has a
non-gnu `statx` fallback), but no build has confirmed it. The profiling features
are kept off broad sweeps on purpose because their instrumentation disturbs the
CLI tests.

Fix: add musl coverage for the platform and test-hooks features, compile-only
if running them is not wanted.
