# Document-convention audit

Audit of every markdown file in the repository against the document-folder
conventions in `AGENTS.md` ("Document folders"). Recorded 2026-08-05 at
`d977bfe1`. Nothing in this audit has been acted on. This file is deliberately
untracked scratch: fold its conclusions into the real docs and delete it.

## Status quo

52 tracked markdown files across six locations.

| Location | Files | Verdict |
|---|---|---|
| root | 9 | 4 exempt, 5 questionable |
| `reference/` | 10 | correct folder, content violations |
| `docs/` | 15 | correct folder, 3 stale forks |
| `notes/` | 6 | all live arcs, clean |
| `decisions/` | 9 | folder not covered by the convention at all |
| `cli/README.md` | 1 | exempt |
| `.plans/` | 8 | gitignored, but cited from source |

Root files: `AGENTS.md`, `CLAUDE.md`, `README.md`, `CHANGELOG.md` (exempt),
plus `CORRECTNESS.md`, `DEVIATIONS.md`, `LIBRARY.md`, `LLM.md`, `TODO.md`.

`notes/` holds `altw.md`, `altw-external.md`, `altw-optimization-history.md`,
`geocode-build-opportunities.md`, `geojson-export-design.md`,
`geoparquet-export.md`. All are live arcs. No action needed there.

## 1. Durable files cite `notes/`

`AGENTS.md`: "nothing durable may cite `notes/` - not a code comment, not
`docs/`, not `reference/`." Roughly 30 violations. `docs/` is clean; every
violation is in `reference/` or in code.

- `reference/performance-history.md` is the worst offender. Lines 994-1036 are
  a 26-line list of retired `notes/*.md` filenames kept as the provenance
  record for conclusions whose source files were deleted. Plus inline
  citations at `:7`, `:37`, `:400`, `:596-597`, `:925`.
- `reference/blob-density.md`, 7 citations, including a live markdown link
  `[notes/altw.md](../notes/altw.md)` at `:365`, and `:179` making a truth
  claim conditioned on notes content. Also `:250`, `:279`, `:427`, `:431`,
  `:436`.
- `reference/technical-implementation-spec.md` at `:98`, `:100`, `:125`
  instructs spec authors to survey `notes/*.md`. This is the document that
  defines what a specification must contain, so it propagates the violation to
  every spec written from it.
- `reference/pipeline.md:239`.
- `reference/performance.md` at `:299`, `:311`, `:521`, `:530`.
- Code comments, the case `AGENTS.md` calls out by name because a comment
  outlives the note: `src/commands/altw/mod.rs` (4 sites),
  `src/commands/altw/sparse.rs` (2), `src/commands/altw/passthrough.rs`,
  `examples/partition_stats.rs`, and the test files
  `tests/cli_add_locations_to_ways.rs`, `tests/cli_apply_changes.rs`,
  `tests/cli_apply_changes_invariants.rs`, `tests/cli_sort.rs`.
- `src/commands/altw/external/mod.rs:567` cites `.plans/merry-watching-lake.md`,
  a gitignored local-only file. Dangling for every other host and user.
- `src/geocode_index/reader.rs` (2 sites), `src/commands/extract/common.rs`,
  and `tests/cli_sort.rs` cite `TODO.md`, transient by the same argument.

Note the coupling with item 4: `performance-history.md`'s citation block exists
precisely because the notes were deleted, so the citation preserves provenance
that no longer resolves. Fixing it means deciding whether a pointer to a
deleted file is worth keeping at all, or whether git history is the right
pointer.

## 2. `TODO.md` is `notes/todo.md` by the convention's own wording

The `AGENTS.md` table literally names `todo.md` as `notes/` content: "work
items (`todo.md`)". It sits at root at 1218 lines, and is cited from
`reference/testing.md`, `reference/blob-density.md`,
`reference/performance-history.md`, `decisions/README.md` and four source
files, so moving it is entangled with item 1.

## 3. `CORRECTNESS.md` and `DEVIATIONS.md` belong in `reference/`

Both are must-be-true, both describe how the thing is built and why, and
neither is a tooling-found root file, so neither qualifies for the root
exemption.

They are cited from 31 places including 14 source and test files, and some
citations are anchored: `reference/cli-reference.md:293` uses
`../DEVIATIONS.md#diff-content-equality-vs-version-ordering`, and `:315` and
`reference/osmium-parity.md:21,126` do the same. A move needs anchor-preserving
link rewrites, not just path edits.

Ruling on the other two root files: `LIBRARY.md` and `LLM.md` are exempt.
`LIBRARY.md` is `readme = "LIBRARY.md"` in `Cargo.toml`, so it is
tooling-found, which is exactly the exemption's stated rationale. `LLM.md` is a
provenance and licensing statement, kin of `LICENSE`.

## 4. Three stale forks of must-be-true content in `docs/`

`reference/` and `docs/` differ by subject, not by being copies. Three pairs
are copies that have drifted apart.

| Pair | Drift |
|---|---|
| `CORRECTNESS.md` to `docs/guide/correctness.md` | 217 lines deleted, 20 added |
| `DEVIATIONS.md` to `docs/cli/deviations.md` | 214 deleted, 35 added; the published copy is missing the `export` entry entirely |
| `reference/cli-reference.md` to `docs/cli/commands.md` | 60 deleted, 28 added |

Two documents that must be right, disagreeing with each other.
`docs/guide/correctness.md` and `docs/cli/deviations.md` are both in the
VitePress sidebar (`docs/.vitepress/config.ts`), so the stale halves are the
published ones.

`reference/osmium-parity.md` versus `docs/cli/osmium-parity.md` is NOT in this
class. One is a comparison matrix, the other a migration guide. That pair is
the reference/docs split working as intended and should be left alone.

## 5. `decisions/` is a fifth folder the convention does not name

Nine ADRs, durable and binding, cited from `src/path_guard.rs`,
`reference/technical-implementation-spec.md`, `reference/blob-density.md`,
`reference/performance.md`, `DEVIATIONS.md`, `TODO.md` and three notes.
`decisions/README.md` documents its own relationship to `DEVIATIONS.md` and
`TODO.md`, so it is a deliberate, load-bearing folder that the `AGENTS.md`
table simply predates.

Decision needed: either the table gains a `decisions/` row, or the ADRs move
under `reference/decisions/`.

## 6. `.plans/` still exists while the convention says `plans/` is retired

Eight markdown files plus one `.rs`. Gitignored, so invisible to CI and to
other hosts, but cited from source (see item 1).

## 7. Source line-number citations

"avoid citing source line numbers - they drift fast." Five instances:

- `reference/testing.md:131` cites `tests/merge.rs:1271-1295`.
- `reference/performance-history.md:670` and `:674` both cite `extract.rs:2813`.
- `reference/pipeline.md:356,358,366` cite line ranges in
  `~/Programs/nidhogg/`, a different repository on one developer's machine.

## Suggested order of work

Items 1, 2 and 3 are one entangled link-rewrite job and are best done in a
single pass: move `TODO.md`, `CORRECTNESS.md` and `DEVIATIONS.md`, then fix
every citation including the anchored ones and the source comments, then
resolve the `notes/` citations left over in `reference/`.

Items 4 and 5 are independent and each need a decision before any editing:
whether the `docs/` forks become includes, redirects, or genuinely
subject-split rewrites; and whether `decisions/` is named in the table or
relocated.

Items 6 and 7 are small and can be done any time.
