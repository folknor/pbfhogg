# ADR-0004: Defensive input handling via boundary errors + fixture coverage

Date: 2026-04-24
Status: Accepted

## Context

Cluster 2 of the 0.3.0 bug sweep consolidated five findings where
pbfhogg trusts producer-side invariants on hostile or corrupt input:

- `renumber/mod.rs:240` - `max_node_id = pass1_schedule.last().max_id`
  assumes the last node blob has the global max ID. True under
  `Sort.Type_then_ID`; on a header that lies about sortedness, a
  later blob's ID can overshoot the pre-allocated `IdSet` and
  `set_atomic` panics in `chunk_for_atomic` with "pre_allocate only
  covers...".
- `altw/external/stage1.rs:269-273` + `stage2.rs:459-493` - the stage
  2 blob-local rank counter trusts indexdata `(min_id, max_id)` to
  tightly bracket actual node IDs. Loose bounds in release silently
  produced skewed ranks, scrambling the join. (The tail check at
  stage2.rs:488 was promoted to hard `Err` on 2026-04-23 in commit
  `ab01438`, closing most of this; stage 1 still needed a
  sanity-check on `max_id < min_id`.)
- `blob_meta/scan_ids.rs:192-202` - the decimicrodegree bbox
  conversion multiplies `granularity * raw_lat` as `i64` without
  overflow checking. Adversarial `granularity` (e.g. `i32::MAX`)
  combined with extreme deltas wraps silently in release and
  serializes a bogus bbox into indexdata that every spatial filter
  downstream trusts.
- `renumber/wire_rewrite.rs:486-491` - `memids_count` and
  `types_count` were derived by counting varint-terminator bytes.
  A malformed trailing varint or extraneous trailing byte miscounts
  and causes the subsequent decode loop to misalign rather than
  error cleanly.
- `apply_changes/rewrite_block.rs:103` - upsert slicing assumes
  `Sort.Type_then_ID` order. The `is_sorted()` check fired only for
  `--locations-on-ways`; the general path silently accepted
  unsorted headers and could drop creates whose IDs crossed block
  boundaries in an unexpected order.

An osmium audit (ADR-0002 context) also surfaced that osmium's own
`derive-changes` has a symmetric defensive gap at
`command_derive_changes.cpp:184`. The class of bug - "trust the
producer, fail obscurely when the producer lies" - is ecosystem-wide,
not pbfhogg-specific.

Four options were on the table:

- **(a) Defend every read.** Check every producer invariant
  unconditionally on the read side.
- **(b) Promote `debug_assert` -> hard `Err` where cheap.** Target
  only sites where the check is once-per-blob or once-per-transition.
- **(c) Document "inputs must be tight" and invest in the test-shape
  gap.** Keep runtime behavior as-is; build the fixture
  infrastructure so CI catches regressions.
- **(d) Hybrid of (b) + (c).** Promote the five findings AND build
  the fixtures.

## Decision

Option **(d)**. Promote each of the five findings to a hard error at
the earliest once-per-blob or once-per-transition checkpoint, AND
introduce `tests/cluster2_defensive_input.rs` as the seed for a
lying-input fixture suite.

Code rule going forward: when a site reads a producer-controlled
field that drives a pre-allocation, a pointer-arithmetic step, or a
structural assumption, verify it at the boundary and surface a
specific error naming the offending field and blob. `debug_assert!`
is fine for per-element invariants where release-mode cost would be
visible; it is not fine for per-blob or one-shot invariants where
the release-mode failure is silent data corruption.

Fixture rule going forward: when a new defensive check is added, a
regression test exercising the malformation pattern lands in the
same commit if feasible, or an entry is added to
`tests/cluster2_defensive_input.rs`'s TODO list if byte-level
mutation is required and the helper doesn't yet exist.

## Alternatives considered

- **(a) Defend every read.** Safest but most expensive. Per-element
  checks on hot paths would measurably regress planet-scale runs,
  and most of the value is captured by the (b) subset - hot-path
  reads that are currently unchecked are typically either already
  defended upstream or are of a type (e.g. tag string table
  lookups) where a defensive check is as expensive as the decode
  itself. Rejected: too much cost, too little marginal defensiveness
  over (b).
- **(b) alone, no fixtures.** Fixes the five current findings but
  doesn't solve the audit-problem - the NEXT five unspotted
  sites get found only when a user reports a panic or silent bad
  output. Rejected: no CI coverage for the pattern.
- **(c) alone, no runtime changes.** Leaves known failure modes
  live in the field until a user triggers them. Rejected: fixtures
  without fixes is just "document how it breaks today."
- **(d) hybrid** *[chosen]* - fixes the five runtime holes now,
  starts building the fixture infrastructure so subsequent audits
  benefit from CI coverage.

## Consequences

- `src/commands/renumber/mod.rs` - max-node-id bound is now
  `pass1_schedule.iter().map(|t| t.max_id).max()`; a PBF that lies
  about `Sort.Type_then_ID` no longer panics in `IdSet::set_atomic`.
- `src/commands/altw/external/stage1.rs::build_node_blob_mapping` -
  per-blob sanity check rejects `max_id < min_id` with a specific
  error naming `data_offset` and the reversed range.
- `src/blob_meta/scan_ids.rs::scan_nodes` - nanodegree -> decimi-
  crodegree conversion uses `checked_mul`/`checked_add`. On
  overflow the blob's bbox is dropped (rather than serialized
  with wrapped values); id-range coverage is retained.
- `src/commands/renumber/wire_rewrite.rs` - added
  `count_varints_strict` helper that walks `protohoggr::Cursor`
  `read_varint()` over the data. Truncated varints or trailing
  partial bytes surface as a specific error per relation.
- `src/commands/apply_changes/rewrite.rs::build_header_bytes` -
  `is_sorted()` check promoted from `--locations-on-ways`-only
  to unconditional. Error message names the Sort.Type_then_ID
  requirement and the canonical ordering within each kind.
- `tests/merge.rs` - 12 tests updated to use
  `write_test_pbf_sorted` (previously used `write_test_pbf` whose
  content happened to be sorted but whose header didn't claim it).
- `tests/cluster2_defensive_input.rs` - new integration-test file
  with two seed tests (lying-sorted renumber survival; unsorted-
  header apply-changes rejection) and a TODO block documenting
  the byte-level fixture helpers still needed to cover the
  remaining three fixes.
- `CHANGELOG.md` - four new Bug-fix / behavior-change entries
  (unsorted-header apply-changes rejection; renumber out-of-order
  lying-sorted survival; combined entry for malformed varint,
  overflow granularity, reversed indexdata range).
- **User-visible behavior change:** `apply-changes` now rejects
  any base PBF whose header does not advertise
  `Sort.Type_then_ID`. Previously the general path accepted such
  input and could silently drop creates. Workflow fix:
  `pbfhogg sort` the base first, or use a producer that sets
  the sorted flag.
- **Performance:** no hot path touched. All five checks are at
  once-per-blob or once-per-transition boundaries. The `count_varints_strict`
  cost bump (~2-3x per byte vs terminator-counting) is bounded
  by relation memids byte count, not element count, and relations
  are sparse (~5k at planet scale).
- **Follow-up:** byte-level fixture helper
  (`tests/common/adversarial.rs`) still needed to close the
  direct regression coverage for the three fixes not yet tested
  (overflow granularity, truncated varint, reversed indexdata
  range). Tracked in TODO.md > Test-shape gaps > "Lying-indexdata
  fixtures (extended coverage)".

## Cross-references

- `tests/cluster2_defensive_input.rs` - the seed fixture file and
  the TODO list of extension points.
- ADR-0002 - osmium `id_order` context; the osmium
  `command_derive_changes.cpp:184` symmetric gap noted there is an
  ecosystem-wide example of the same defensive-gap class.
- Commit `ab01438` (2026-04-23) - the earlier debug_assert_eq! ->
  hard `Err` landing at `altw/external/stage2.rs:488` that set
  the precedent option (d) builds on.
