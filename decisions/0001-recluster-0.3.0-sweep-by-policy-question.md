# ADR-0001: Recluster the 0.3.0 pre-release bug sweep by policy question

Date: 2026-04-24
Status: Accepted

## Context

The 0.3.0 pre-release bug sweep in `TODO.md` accumulated ~35 open
findings from a multi-agent Opus audit of 0.3.0 high-churn areas
(apply-changes, renumber, altw external, diff / derive-parallel,
geocode, read path, write path, smaller commands). The findings were
originally organized by subsystem, mirroring how the audit was
conducted.

Reviewing the residue a day after the six headline items landed
(2026-04-23), a pattern emerged: most remaining findings weren't
subsystem-specific bugs needing subsystem-specific fixes. They were
sites of cross-cutting *policy questions*. For example:

- Four findings across renumber + diff + derive-changes were really
  the single question "how do we handle negative IDs project-wide?"
- Four findings across renumber + geocode builder were really the
  single question "what is our posture on counters and partial output
  when a command errors mid-stream?"
- Ten findings were "works correctly today under current callers and
  inputs but would regress on a future refactor or adversarial input"
  and wanted the same treatment (debug_assert + invariant comment).

Organizing the residue by policy question exposes this structure. One
decision disposes of 2-10 findings at once; the bug sweep becomes a
small number of policy calls rather than a long list of one-off items.

## Decision

Reorganize the "0.3.0 pre-release bug sweep (2026-04-23)" section of
`TODO.md` into seven policy clusters plus a "Straight fixes" section
(items with a clear fix direction and no open policy question) and a
"Per-site items" section (items with no cross-cutting theme).

The seven clusters:

1. Negative-ID / mixed-sign handling policy
2. Defensive handling of adversarial or malformed input
3. Error path hygiene - counter accuracy and partial output
4. Panic propagation in parallel pipelines
5. Drop-path error swallowing
6. Error ordering - downstream error masks root cause
7. Latent-only-on-future-refactor or pathological-input items

Each cluster opens with the policy question it asks, the decision
options on the table, and (once decided) a pointer to the ADR that
records the call.

Preserve every file:line pin so the per-subsystem view is recoverable
with `grep`.

## Alternatives considered

- **Keep the subsystem grouping.** Minimal churn, but we'd have to
  re-discover the cross-cutting patterns every time we revisited the
  list. Loses the "one decision disposes of N items" affordance.
- **Mixed grouping with tags.** Every finding tagged with its
  subsystem *and* its policy cluster, surfaceable both ways. More
  accurate but more overhead per entry and no clear reader benefit
  over choosing one primary axis.
- **Split into two documents.** A subsystem-indexed bug list and a
  policy-indexed decision list. Doubled surface for no obvious gain.

## Consequences

- `TODO.md` section rewritten 2026-04-24 (commit `a4699e4`).
- Landing commits for each cluster cite the cluster by number
  (`0.3.0 sweep: land cluster 1 (negative-ID policy) as option (c)`),
  making the git log searchable by policy call.
- Future audits on later releases can use the same pattern: list
  findings by subsystem, then recluster by policy question once the
  initial pass is in.
- The "Straight fixes" and "Per-site items" sections absorb the
  residue that doesn't fit a cluster, so clusters stay focused.

## Cross-references

- `TODO.md` > "0.3.0 pre-release bug sweep (2026-04-23)" - the
  reclustered section.
- Commit `a4699e4` - the reclustering landing.
