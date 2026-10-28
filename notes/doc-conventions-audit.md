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
| `docs/` | correct folder, 3 stale forks |
| `notes/` | live arcs and hunt output, clean |
| `decisions/` | folder not covered by the convention at all |
| `cli/README.md` | exempt |
| `.plans/` | gitignored |

Root files: `AGENTS.md`, `CLAUDE.md`, `README.md`, `CHANGELOG.md` (exempt),
plus `CORRECTNESS.md`, `DEVIATIONS.md`, `LIBRARY.md`, `LLM.md`. Items 1 and 2
(durable files citing `notes/` and the root `TODO.md`, and moving the latter to
`notes/todo.md`) are resolved; `scripts/check_doc_links.py` enforces both.

## 3. `CORRECTNESS.md` and `DEVIATIONS.md` belong in `reference/`

Both are must-be-true, both describe how the thing is built and why, and
neither is a tooling-found root file, so neither qualifies for the root
exemption.

They are cited from about 31 places including 14 source and test files, and
some citations are anchored: `reference/cli-reference.md` uses
`../DEVIATIONS.md#diff-content-equality-vs-version-ordering`, and
`reference/osmium-parity.md` does the same. A move needs anchor-preserving link
rewrites, not just path edits, plus the `DURABLE_ROOT_DOCS` set in
`scripts/check_doc_links.py` (which scans these two root files) and the
GitHub URLs the site's correctness and deviations pages link to.

Ruling on the other two root files: `LIBRARY.md` and `LLM.md` are exempt.
`LIBRARY.md` is `readme = "LIBRARY.md"` in `Cargo.toml`, so it is
tooling-found, which is exactly the exemption's stated rationale. `LLM.md` is a
provenance and licensing statement, kin of `LICENSE`.

## 4. `docs/cli/commands.md` is a stale fork of `reference/cli-reference.md`

The pair are copies that have drifted apart (content deleted and added since
the fork, and `reference/cli-reference.md` also documents unreleased
changes), and both are must-be-true. `docs/cli/commands.md` is in the
VitePress sidebar, so the stale half is the published one. Edits today land
twice by hand (the `diff -v` paragraph, the exit-code tables). Both open with
"Generated from `pbfhogg --help` output", but no generator exists in the
repository: the phrase records where the text came from, not a mechanism, and
should go whichever way this is settled.

The CORRECTNESS and DEVIATIONS forks were settled by keeping the root files as
the single source and reducing the site pages to pointers at their GitHub
rendering, with `scripts/check_doc_links.py` resolving GitHub URLs and
fragments against the checkout. The same shape would apply here; the cost is
larger, since the commands page is the site's main CLI reference and its
search index content.

`reference/osmium-parity.md` versus `docs/cli/osmium-parity.md` is NOT in this
class. One is a comparison matrix, the other a migration guide. That pair is
the reference/docs split working as intended and should be left alone.

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

Item 4 needs a decision before any editing: whether the commands fork becomes
a pointer, an include, or a genuinely subject-split rewrite.

Items 6 and 7 are small and can be done any time.
