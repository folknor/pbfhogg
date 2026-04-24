# Architecture Decision Records

This directory holds the project's policy and architecture decisions, one
decision per file, in Architecture Decision Record (ADR) format.

The intent is to have a permanent, searchable, time-ordered record of
*why* pbfhogg is shaped the way it is. Code shows *what*; git log shows
*when*; these ADRs capture the *why*, in a form that survives the
context in which the decision was made.

## When to add an ADR

Add one when any of the following holds:

- The decision establishes a project-wide convention (e.g. "all commands
  with file output must wrap the output path in a `PathGuard`").
- The decision is a policy call with multiple defensible alternatives and
  the rationale is non-obvious from the code.
- The decision changes or cements how pbfhogg diverges from a reference
  implementation (e.g. osmium).
- Future you, reading the code in six months, would plausibly ask "why
  was this done this way?" and the answer isn't derivable from the code.

Don't add one when:

- The change is a one-line bug fix, a local refactor, a test addition,
  or a performance tweak that doesn't shift policy.
- The rationale is already captured in an existing `reference/` doc,
  `DEVIATIONS.md`, or a rich commit message, and no cross-cutting
  decision is being made.
- You're documenting *behavior* rather than a *decision*. Behavior goes
  in `reference/`. Decisions go here.

## File naming and numbering

`decisions/NNNN-short-kebab-title.md`, zero-padded to four digits.
Numbers are monotonic and never reused. When an ADR is superseded, the
old one stays in place with status `Superseded by NNNN` and the new one
links back with `Supersedes NNNN`.

## Skeleton

```markdown
# ADR-NNNN: Short title (imperative mood)

Date: YYYY-MM-DD
Status: Accepted | Superseded by NNNN | Rejected

## Context

What situation or findings motivated this decision. Link to relevant
code, TODO items, external references. Keep it tight: enough for a
future reader with no prior context to understand the problem.

## Decision

What we decided, stated as a rule or convention. One short paragraph.

## Alternatives considered

Bulleted list of the options that were on the table. For each, one
sentence on why it wasn't chosen. This is the most important section
for posterity: it documents the *rejected* options so future work
doesn't re-argue them from scratch.

## Consequences

What changes as a result of this decision. Usually:
- Code changes that landed (with file:line pins or commit hashes).
- Project-wide rules or invariants established.
- Known limitations or follow-up work the decision creates.
- Migration path if we ever reverse the decision.

## Cross-references

Pointers to related ADRs, `DEVIATIONS.md` sections, `reference/` docs,
or code that enforces the decision. Optional; include when a reader
would benefit from the adjacent context.
```

## Interaction with other docs

- **`DEVIATIONS.md`** owns the "how we differ from osmium" comparison
  tables. When a decision is *about* an osmium divergence, the ADR
  captures the decision and the DEVIATIONS entry captures the behavior
  comparison. Cross-reference both ways.
- **`reference/`** owns architectural and behavioral reality (blob
  encoding, pipeline shapes, performance topology). ADRs reference
  these; they don't duplicate them.
- **`CHANGELOG.md`** owns user-visible release notes. ADRs are for the
  *why*, not the *what shipped*.
- **`CLAUDE.md`** owns rules for AI-assisted work. When an ADR
  establishes a coding rule, a one-liner in `CLAUDE.md` can mirror it
  with a pointer to the ADR.
- **`TODO.md`** cites ADRs from decision paragraphs at the top of each
  cluster; the ADR cites TODO.md back only if the follow-up list is
  substantial.

## Index

- [ADR-0001](0001-recluster-0.3.0-sweep-by-policy-question.md) -
  Organize the 0.3.0 pre-release bug sweep by policy cluster, not
  subsystem.
- [ADR-0002](0002-negative-ids-rejected-project-wide.md) - Negative
  input IDs rejected project-wide; hard reject in renumber,
  `debug_assert` in diff / derive shard planners.
- [ADR-0003](0003-error-path-hygiene-via-pathguard.md) - Error path
  hygiene via a shared `PathGuard` RAII primitive plus a
  counters-after-write rule.
- [ADR-0004](0004-defensive-input-errors-and-fixtures.md) -
  Defensive handling of adversarial or malformed input: promote
  five indexdata/varint/sortedness-trust sites to hard errors at
  once-per-blob boundaries, seed a lying-input test fixture suite.
