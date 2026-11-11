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

## BUG-010 - Profiling features never build for musl

The `musl` and `musl-all` sweeps in `brokkr.d/checks.toml` cover default
features and the `all` shape (test-hooks plus both platform features) on
x86_64 musl. Code behind `hotpath` and `hotpath-alloc` still never compiles for
musl, and the allocator textlint exempts `crates/lib/src/debug.rs` and
`crates/lib/src/process.rs` wholesale, so an ungated glibc call added there
behind a profiling feature escapes both guards. The profiling features stay off
test sweeps on purpose (their instrumentation disturbs the CLI tests), so this
belongs with VER-098's compile-only profiling shapes in
`notes/verification.md`. Mixed platform shapes (one of `linux-direct-io`,
`linux-io-uring` without the other) are built on neither target.

Fix: a compile-only musl shape with the profiling features, landed with
VER-098's.
