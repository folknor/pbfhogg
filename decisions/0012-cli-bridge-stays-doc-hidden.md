# ADR-0012: Keep the CLI bridge `doc(hidden)` until the command layer is restructured

Date: 2026-08-20
Status: Accepted

## Decision

`lib.rs` continues to re-export the command layer plus `path_guard`,
`tag_expr` and `HeaderOverrides` as `#[doc(hidden)] pub` so the separate
`pbfhogg-cli` crate can call them. We accept the known costs for now:
every internal refactor of `commands/` is technically semver-visible,
and library consumers can reach a large undocumented API if they ignore
the `doc(hidden)` signal. No third crate is introduced and nothing is
promoted to documented-public before 1.0.

The named right shape, when the restructure happens, is a
`pbfhogg-commands` crate between the format library and the binary - or
promoting the command entry points to a documented public API. Either is
a deliberate API-design project with its own review, not a bug-fix
deliverable: the split decides which types cross the boundary
(`WriteOpts`-shaped options structs, stats types, `HeaderOverrides`) and
therefore what the library's 1.0 surface is. The trigger for doing it is
the 1.0 API freeze; until then, pre-1.0 semver (0.x minor bumps may
break) makes the `doc(hidden)` disguise honest enough.

## Alternatives considered

- **Introduce `pbfhogg-commands` now.** Rejected for timing, not shape:
  there is no defect behind the porosity, the workspace is mid-0.x with
  the command layer still being reshaped (option-struct grouping, output
  guarding, exit codes all moved this cycle), and a crate boundary drawn
  now would be drawn again at 1.0.
- **Promote the re-exports to documented public API.** Rejected: it
  cements today's command signatures - 8-to-10-parameter functions still
  carrying `too_many_arguments` allows - as a supported surface, which is
  the opposite of what pre-1.0 freedom is for.
- **Hide the bridge harder (e.g. `#[doc(hidden)] pub mod __internal`
  with unstable naming).** Rejected as cosmetic: it changes how loudly
  the signal is stated, not the semver reality, and churns every
  `pbfhogg-cli` call site for no consumer-visible gain.
