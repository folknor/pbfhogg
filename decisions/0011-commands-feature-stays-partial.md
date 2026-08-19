# ADR-0011: The `commands` feature gates dependencies, not the command layer

Date: 2026-08-20
Status: Accepted

## Decision

The `commands` feature keeps its current, deliberately partial scope: it
gates exactly the modules that pull optional dependencies or clearly
optional surface - `check`, `export`, `extract`, `commands::spatial`,
`inspect::json`, and the geocode builder (with `serde_json` and `s2`).
The other fifteen command modules (`altw`, `apply_changes`, `cat`,
`degrade`, `diff`, `getid`, `getparents`, `inspect` minus JSON,
`merge_changes`, `renumber`, `repack`, `sort`, `tags_count`,
`tags_filter`, `time_filter`) compile in a `default-features = false`
build, and we do not extend the gate to cover `src/commands/` wholesale.

The feature's contract is therefore "no optional dependencies", not "no
command layer". `AGENTS.md` (Features section) describes this scope
accurately, and the `minimal` sweep in `brokkr.toml` compiles and lints
the no-features configuration on every `brokkr check`, so the
configuration cannot rot silently. Revisit only as part of a workspace
restructure that separates the command layer into its own crate (see
ADR-0012), where the question answers itself.

## Alternatives considered

- **Gate `src/commands/` wholesale behind `commands`.** Rejected: there
  is no defect behind the asymmetry - the ungated modules compile clean
  with zero optional dependencies - and the change would touch every
  command module, the `lib.rs` re-exports, and roughly 30 test files for
  a purely taxonomic gain. A library consumer who wants a lean build
  already gets one: the optional dependencies are what
  `default-features = false` exists to drop, and it drops them.
- **Gate nothing and remove the feature.** Rejected: `serde_json` and
  `s2` are real dependency weight, and the geocode builder is genuinely
  optional surface; the feature earns its keep exactly there.
