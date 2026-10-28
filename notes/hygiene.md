# Hygiene

Small enforcement and tidiness gaps. Transient; nothing durable cites this.

## The static-fault-hook textlint covers one file

`brokkr.toml` rule `no-static-fault-hook-state` bans `static` items only in
`src/test_hooks.rs`. Test-hook types also live with the code they reach
(`PublishFault` and `PublishStep` in `src/output.rs`, the `test-hooks`
branches in `src/host.rs`), and a `#[cfg(feature = "test-hooks")] static`
there would be process-global fault state that no rule catches. The intent
(test pause and fault machinery rides the invocation token) is stated in
`src/test_hooks.rs` and `reference/testing.md`; the rule could instead ban a
`static` on any line gated by `cfg(feature = "test-hooks")` across `src/`,
if textlint can express a preceding-line condition, or the gated items could
be required to sit in `src/test_hooks.rs` only.
