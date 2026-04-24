# ADR-0001: Organize bug-sweep residue by policy cluster

Date: 2026-04-24
Status: Accepted

## Decision

When a multi-agent bug sweep produces a residue large enough that
the open findings don't fit in a single pass of individual fixes,
recluster the residue by the policy question each group asks rather
than by subsystem. One policy decision should dispose of multiple
findings at once; the residue becomes a small number of policy calls
rather than a long list of one-off items.

Each cluster opens with the policy question it asks, the decision
options on the table, and (once decided) a pointer to the ADR that
records the call. Preserve every file:line pin so the per-subsystem
view is recoverable with `grep`. A "Straight fixes" section absorbs
items with a clear fix direction and no open policy question; a
"Per-site items" section absorbs items with no cross-cutting theme.

## Alternatives considered

- **Keep the subsystem grouping.** Minimal churn, but the
  cross-cutting patterns have to be re-discovered on every revisit.
  Loses the "one decision disposes of N items" affordance.
- **Mixed grouping with tags.** Every finding tagged with both its
  subsystem and its policy cluster, surfaceable both ways. More
  overhead per entry and no clear reader benefit over choosing one
  primary axis.
- **Split into two documents.** Subsystem-indexed bug list plus
  policy-indexed decision list. Doubled surface for no obvious gain.

## Consequences

- Bug-sweep sections in `TODO.md` are organized by policy cluster,
  not subsystem. Each cluster names its policy question at the top
  and links to the ADR that answered it.
- Landing commits for each cluster cite the cluster by number and
  the policy option selected, making the git log searchable by
  policy call.
- "Straight fixes" and "Per-site items" sections absorb the residue
  that doesn't fit a cluster, so clusters stay focused on their
  policy question.

## Cross-references

- `TODO.md` - the live bug-sweep section is organized to this
  pattern.
