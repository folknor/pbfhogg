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
rewrites, not just path edits.

Ruling on the other two root files: `LIBRARY.md` and `LLM.md` are exempt.
`LIBRARY.md` is `readme = "LIBRARY.md"` in `Cargo.toml`, so it is
tooling-found, which is exactly the exemption's stated rationale. `LLM.md` is a
provenance and licensing statement, kin of `LICENSE`.

## 4. Three stale forks of must-be-true content in `docs/`

`reference/` and `docs/` differ by subject, not by being copies. Three pairs
are copies that have drifted apart.

| Pair | Drift |
|---|---|
| `CORRECTNESS.md` to `docs/guide/correctness.md` | content deleted and added since the fork |
| `DEVIATIONS.md` to `docs/cli/deviations.md` | the published copy is missing the `export` entry entirely |
| `reference/cli-reference.md` to `docs/cli/commands.md` | content deleted and added since the fork |

Two documents that must be right, disagreeing with each other.
`docs/guide/correctness.md` and `docs/cli/deviations.md` are both in the
VitePress sidebar (`docs/.vitepress/config.ts`), so the stale halves are the
published ones.

`reference/osmium-parity.md` versus `docs/cli/osmium-parity.md` is NOT in this
class. One is a comparison matrix, the other a migration guide. That pair is
the reference/docs split working as intended and should be left alone.

## 5. `decisions/` is a fifth folder the convention does not name

Durable and binding, cited from `src/path_guard.rs`,
`reference/technical-implementation-spec.md`, `reference/blob-density.md`,
`reference/performance.md`, `DEVIATIONS.md` and several notes.
`decisions/README.md` documents its own relationship to `DEVIATIONS.md` and
the work-item list, so it is a deliberate, load-bearing folder that the `AGENTS.md`
table simply predates. `scripts/check_doc_links.py` already treats
`decisions/` as a path root it validates.

Decision needed: either the table gains a `decisions/` row, or the ADRs move
under `reference/decisions/`.

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
- Source comments such as `coord_payloads.rs:398` and `drain.rs:408-410`
  (tracked in `notes/hygiene.md`).

## Suggested order of work

Item 3 is a link-rewrite job: move `CORRECTNESS.md` and `DEVIATIONS.md`, then
fix every citation including the anchored ones and the source comments.

Items 4 and 5 are independent and each need a decision before any editing:
whether the `docs/` forks become includes, redirects, or genuinely
subject-split rewrites; and whether `decisions/` is named in the table or
relocated.

Items 6 and 7 are small and can be done any time.
