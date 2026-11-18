# ADR-0028: Negative IDs are handled like any other ID

Date: 2026-11-18
Status: Accepted
Supersedes 0002

## Decision

An OSM object's identity is its kind plus its signed `i64` ID, and every
element ID, way ref and relation member of either sign is a valid identity.
Every command compares, selects, joins, ranks, renumbers and reports a
negative ID by the same rules as a non-negative one:

- wherever a command orders an object stream or validates its order, the
  order is canonical `id_order` (`0`, the negative IDs by magnitude, then the
  positives; `crates/lib/src/osm_id.rs`), within each action group for an
  OSC that groups by action;
- indexdata ranges, and report fields named numeric, are signed numeric
  extrema;
- admission requirements (sorted input, indexdata, no history) and the
  metadata-trust contract of
  [ADR-0013](0013-blob-metadata-is-trusted-by-default-and-verified-on-demand.md)
  are sign-neutral, and so are deliberate removals (filters, deletes,
  `--drop-ids`, dedupe).

Any input osmium accepts that meets those sign-neutral admission
requirements, pbfhogg accepts and answers correctly. Where osmium
folds an ID onto its magnitude (`positive_id`, `positive_ref`), pbfhogg keeps
the signed identity, and `DEVIATIONS.md` "Negative IDs: pbfhogg answers by
the signed ID" records each such difference. A command that drops,
misplaces or refuses an ID because of its sign has a bug; there are no
exceptions. The rule's executable inventory is the negative-ID registry
(`crates/cli/tests/cli_negative_ids.rs`) and the source-policy `id_signs`
rule, described in `reference/testing.md`; both are evidence for the rule,
neither is proof of it.

## Alternatives considered

- **Keep ADR-0002 and fix its wording only.** Rejected: the commands it
  named as silently dropping negative IDs (`tags-filter`, external
  `add-locations-to-ways` retention) gave wrong answers, and a wording fix
  leaves wrong answers in place.
- **Refuse at the silent-drop sites and keep the rest as it was.** Rejected:
  it turns a wrong answer into a refusal of a file osmium processes, which
  fails the goal that anything osmium accepts pbfhogg answers correctly.
- **Refuse negative IDs everywhere, with `renumber` as the named remedy.**
  Rejected: renumbering is no remedy for `diff`, `derive-changes`,
  `apply-changes` and `merge-changes`, which relate two inputs and would
  lose the correspondence between them, and it turns answers pbfhogg
  already gave correctly into refusals.
- **Refuse per operand where an unsigned index cannot hold the ID.**
  Rejected: the index's representation is pbfhogg's choice, not a property
  of the input; a signed set (two bitmaps, the negatives keyed by `!id`)
  costs nothing when no negative ID is present, as measured on the commands
  it touched.
- **Fold IDs onto their magnitude as osmium does.** Rejected: `n-7` and
  `n7` are distinct objects, and folding them selects, removes and joins
  the wrong one.
