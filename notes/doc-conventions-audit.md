# Document-convention audit

Audit of every markdown file in the repository against the document-folder
conventions in `AGENTS.md` ("Document folders"). Recorded 2026-08-05 at
`d977bfe1`, updated since as items closed. This file is tracked, like the rest
of `notes/`. Resolved items are kept only as a pointer to what the remaining
items depend on; trim them when this file is next edited, and delete the file
when every item is done.

## Status quo

Counts and verdicts below are as of the original audit and have drifted; the
numbered items carry the current state.

| Location | Verdict |
|---|---|
| root | 4 exempt, 5 questionable |
| `reference/` | correct folder, content violations (mostly closed, see item 1) |
| `docs/` | correct folder, no forks left |
| `notes/` | live arcs and hunt output, clean |
| `decisions/` | covered by the convention since AGENTS.md gained its row |
| `cli/README.md` | exempt |
| `.plans/` | gitignored |

Root files: `AGENTS.md`, `CLAUDE.md`, `README.md`, `CHANGELOG.md` (exempt),
plus `CORRECTNESS.md`, `DEVIATIONS.md`, `LIBRARY.md`, `LLM.md`. Items 1 and 2
(durable files citing `notes/` and the root `TODO.md`, and moving the latter to
`notes/todo.md`) are resolved; `scripts/check_doc_links.py` enforces both.

The `docs/` forks are settled: `CORRECTNESS.md` and `DEVIATIONS.md` are the
single source with pointer pages on the site, and the CLI reference lives in
`docs/cli/commands.md` alone, held to the CLI's command tree by a test.
`reference/osmium-parity.md` versus `docs/cli/osmium-parity.md` is not a
fork: one is a comparison matrix, the other a migration guide, the
reference/docs split working as intended.

## 3. `CORRECTNESS.md` and `DEVIATIONS.md` belong in `reference/`

Both are must-be-true, both describe how the thing is built and why, and
neither is a tooling-found root file, so neither qualifies for the root
exemption.

They are cited from about 31 places including 14 source and test files, and
some citations are anchored: `reference/osmium-parity.md` uses
`../DEVIATIONS.md#diff-content-equality-vs-version-ordering`, and the CLI
reference `docs/cli/commands.md` links two DEVIATIONS sections by GitHub URL.
A move needs anchor-preserving link rewrites, not just path edits, plus the
`DURABLE_ROOT_DOCS` set in `scripts/check_doc_links.py` (which scans these two
root files) and the GitHub URLs the site's pages link to.

Ruling on the other two root files: `LIBRARY.md` and `LLM.md` are exempt.
`LIBRARY.md` is `readme = "LIBRARY.md"` in `Cargo.toml`, so it is
tooling-found, which is exactly the exemption's stated rationale. `LLM.md` is a
provenance and licensing statement, kin of `LICENSE`.

## 6. `.plans/` still exists while the convention says `plans/` is retired

Gitignored, so invisible to CI and to other hosts. The citation half is
resolved (nothing durable cites it, and the `doc-links` check enforces that).
What remains is housekeeping: fold or delete the local files.

## 7. Source line-number citations

"avoid citing source line numbers - they drift fast." The `reference/testing.md`
citation is gone. Remaining:

- `reference/performance-history.md` cites `extract.rs:2813` twice.
- `reference/pipeline.md` cites line ranges in `~/Programs/nidhogg/`, a
  different repository on one developer's machine.
- Test comments: `tests/geocode_index.rs`, `tests/getid.rs`,
  `tests/inspect.rs`, `tests/read_paths.rs` and
  `cli/tests/cli_defensive_input.rs` cite source line numbers. No such
  citation remains under `src/` or `cli/src/` (checked 2026-10-26).

## Suggested order of work

Item 3 is a link-rewrite job: move `CORRECTNESS.md` and `DEVIATIONS.md`, then
fix every citation including the anchored ones and the source comments.

Items 6 and 7 are small and can be done any time.
