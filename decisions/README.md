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
Numbers are monotonic and never reused. Filenames must be *timeless* -
no version numbers, release labels, or calendar dates. An ADR is a
permanent decision or it isn't an ADR. When an ADR is superseded, the
old one stays in place with status `Superseded by NNNN` and the new one
links back with `Supersedes NNNN`.

## Skeleton

The document is read top-to-bottom by someone with no conversational
context. Lead with the decision, not the backstory. Assume the reader
hasn't read the bug sweep, the ticket, or the email thread that led
here. Minimise preamble.

```markdown
# ADR-NNNN: Short title (imperative mood)

Date: YYYY-MM-DD
Status: Accepted | Superseded by NNNN | Rejected

## Decision

What we decided, stated as a rule or convention the reader can apply
immediately. One short paragraph. No history, no options survey, no
walk through how we got here - just the rule. If the decision needs
framing, one sentence of scope is enough.

## Alternatives considered

Bulleted list of the options that were on the table. For each, one
sentence on why it wasn't chosen. This is the most important section
for posterity: it documents the *rejected* options so future work
doesn't re-argue them from scratch.
```

The skeleton has exactly two sections. `Context`, `Consequences`, and
`Cross-references` are deliberately omitted:

- **No Context:** if the Decision needs a sentence of framing to
  stand alone, put it right under the Decision heading. Don't
  narrate the situation that led here.
- **No Consequences:** user-visible outcomes belong in
  `CHANGELOG.md`; commit history captures what landed; nobody will
  care about the rest in six months. If the decision creates a
  migration path or a conditional trigger for reversing the
  decision, put that single sentence at the bottom of the Decision
  section.
- **No Cross-references:** when another ADR or doc is relevant to
  the decision, reference it *inline* in the Decision or
  Alternatives where it's load-bearing - not in a footer that
  duplicates what the reader already has context for.

## Interaction with other docs

- **`DEVIATIONS.md`** owns the "how we differ from osmium" comparison
  tables. When a decision is *about* an osmium divergence, the ADR
  captures the rule and the DEVIATIONS entry captures the behavior
  comparison. The ADR references DEVIATIONS.md inline where it's
  load-bearing (usually a sentence in the Decision section).
- **`reference/`** owns architectural and behavioral reality (blob
  encoding, pipeline shapes, performance topology). ADRs reference
  these inline when relevant; they don't duplicate them.
- **`CHANGELOG.md`** owns user-visible release notes. ADRs are for the
  *why*, not the *what shipped*.
- **`CLAUDE.md`** owns rules for AI-assisted work. When an ADR
  establishes a coding rule, a one-liner in `CLAUDE.md` can mirror it
  with a pointer to the ADR.
- **`TODO.md`** cites ADRs from decision paragraphs at the top of each
  bug-sweep cluster. The ADR does *not* link back to `TODO.md` - the
  ADR is the permanent decision, the TODO cluster is a transient
  organizing artifact. An ADR also must not name a specific bug-sweep
  cluster or release label in its title or filename; cluster labels
  live in `TODO.md` and evolve, the ADR stays.

## Index

- [ADR-0002](0002-negative-ids-rejected-project-wide.md) - Negative
  input IDs rejected project-wide; hard reject in renumber and
  getid, `debug_assert` in diff / derive shard planners, silent
  inheritance through `IdSet` for tags-filter / altw-external.
- [ADR-0003](0003-error-path-hygiene-via-pathguard.md) - Error path
  hygiene via a shared `PathGuard` RAII primitive plus a
  counters-after-write rule.
- [ADR-0004](0004-defensive-input-errors-and-fixtures.md) -
  Defensive handling of adversarial or malformed input: promote
  indexdata/varint/sortedness-trust sites to hard errors at
  once-per-blob boundaries, seed a lying-input test fixture suite.
- [ADR-0005](0005-latent-invariant-debug-asserts.md) - Cheap
  insurance for latent-only invariants: triage rule of
  `debug_assert` + comment / doc-only / drop-from-triage, no
  blanket sweep.
- [ADR-0007](0007-injected-prepass-wire-extensions.md) - Private,
  opt-in PBF wire extensions (`pbfhogg.WayMembers-v1` BlobHeader
  field 5, `pbfhogg.SharedNodePins-v1` Way field 20) for
  `add-locations-to-ways --inject-prepass`; presence-is-validity
  superset semantics, rewriting commands must not carry the flags
  forward.
