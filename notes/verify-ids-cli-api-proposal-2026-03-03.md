# Proposal: `pbfhogg verify ids` (2026-03-03)

## Goal

Provide a strict, machine-gatable ID integrity check before production steps like `add-locations-to-ways`, with a fast path for sorted indexed planet files.

Checks:
- per-type ordering (`nodes -> ways -> relations`)
- per-type strict ID monotonicity (no duplicates, no decreases)
- optional full duplicate detection for unsorted inputs

## CLI Proposal

New subcommand:

```bash
pbfhogg verify ids <file> [flags]
```

### Flags

- `--direct-io`
  - Same semantics as other commands.
- `--strict-order`
  - Require global block/type order `nodes -> ways -> relations`; fail if mixed/out-of-order.
- `--strict-ids`
  - Require strictly increasing IDs within each type (`id > prev_id`), fails on duplicate or decrease.
  - Enabled by default for sorted inputs.
- `--full`
  - Full duplicate detection regardless of sortedness (exact set-based detection).
  - Uses more memory/time; intended for untrusted/unsorted data.
- `--type <node|way|relation|all>`
  - Limit verification scope; default `all`.
- `--max-errors <N>`
  - Stop after N violations (default `100`).
- `--json`
  - Emit machine-readable summary.
- `--quiet`
  - Exit-code-only mode.

### Suggested Defaults

If header says sorted and indexdata is present:
- run fast path (`--strict-ids` semantics) automatically.

If not sorted:
- run streaming checks that still detect decreases;
- duplicates only guaranteed with `--full`.

## Exit Codes

- `0` = verification passed.
- `1` = verification failed (violations found).
- `2` = usage/config error (bad args).
- `3` = I/O/parse/runtime error.

Rationale: keep `1` as pipeline gate failure; separate operational errors.

## Output Contract

Human output (default):
- file summary (sorted flag, indexed flag, total elements scanned)
- violation counts:
  - `type_order_violations`
  - `id_decrease_violations`
  - `duplicate_id_violations`
  - `mixed_block_violations` (optional)
- sample violations (up to `max-errors`)

JSON output (`--json`):

```json
{
  "ok": false,
  "sorted_header": true,
  "indexed": true,
  "scanned": { "nodes": 123, "ways": 45, "relations": 6 },
  "violations": {
    "type_order": 0,
    "id_decrease": 2,
    "duplicate_id": 2,
    "mixed_block": 0
  },
  "samples": [
    { "kind": "node", "id": 123, "reason": "duplicate", "prev_id": 123, "block": 7743 }
  ]
}
```

## Rust API Proposal

Module:
- `src/commands/verify_ids.rs`

Public entrypoint:

```rust
pub struct VerifyIdsOptions {
    pub direct_io: bool,
    pub strict_order: bool,
    pub strict_ids: bool,
    pub full: bool,
    pub type_filter: TypeFilter,
    pub max_errors: usize,
}

pub struct VerifyIdsReport {
    pub sorted_header: bool,
    pub indexed: bool,
    pub nodes_scanned: u64,
    pub ways_scanned: u64,
    pub relations_scanned: u64,
    pub type_order_violations: u64,
    pub id_decrease_violations: u64,
    pub duplicate_id_violations: u64,
    pub mixed_block_violations: u64,
    pub samples: Vec<IdViolationSample>,
}

pub fn verify_ids(path: &Path, opts: &VerifyIdsOptions) -> Result<VerifyIdsReport>;
```

`IdViolationSample` should include `kind`, `id`, `prev_id`, `block_number`, `reason`.

## Fast-Path Algorithm (sorted indexed planet files)

Preconditions:
- `HeaderBlock::is_sorted() == true`
- first OsmData blob has indexdata (or all blobs indexed if we enforce it)
- no `--full`

Algorithm:
1. Stream via `ElementReader::into_blocks_pipelined()` (or block-level iterator).
2. Track:
   - `current_type_rank` (node/way/relation)
   - `prev_id_node`, `prev_id_way`, `prev_id_relation`
3. For each element:
   - verify non-decreasing type rank (`strict_order` => fail on decrease/mixed)
   - check `id > prev_id` for that type:
     - `id == prev_id` => duplicate
     - `id < prev_id` => decrease
   - update `prev_id`.
4. Stop early after `max_errors`.

Complexity:
- Time: `O(n)` single pass.
- Memory: `O(1)` plus small sample buffer.

Why this is enough on sorted inputs:
- strict monotonic `id > prev_id` per type guarantees no duplicates globally within type.

## Full Mode Algorithm (`--full`)

For unsorted/untrusted data where monotonic check is insufficient:
1. Stream all elements.
2. Insert IDs into per-type sets; detect duplicates on failed insert.
3. Still track decreases/order for diagnostics.

Set backend options:
- `RoaringTreemap` (memory-efficient for dense ranges).
- Hash set fallback if needed for sparse workloads.

Complexity:
- Time: `O(n)` average.
- Memory: high (planet-scale dependent; can be multiple GB).

## Implementation Notes

- Reuse existing `TypeFilter` and `BlobFilter` to skip irrelevant blobs.
- Reuse `read_raw_frame`/header helpers to determine indexed/sorted metadata early.
- Keep verification logic separate from `inspect`:
  - `inspect` remains observational,
  - `verify ids` becomes policy/gate with stable exit codes.

## Pipeline Recommendation

For production ingest:

1. `pbfhogg cat ...` (index normalization)
2. `pbfhogg verify ids <pbf> --strict-order --strict-ids`
3. `pbfhogg check-refs <pbf> --check-relations`
4. `pbfhogg merge ...`
5. `pbfhogg add-locations-to-ways ...`

For untrusted upstreams:
- add `--full` at step 2.

## CLI Surface Consolidation Recommendation

Given current command breadth, add `verify` as a validation namespace and
consolidate validation-only commands there, while preserving compatibility.

Recommended structure:

- `pbfhogg verify indexed <file>`
  - alias for current `is-indexed`
- `pbfhogg verify refs <file> [--check-relations]`
  - alias for current `check-refs`
- `pbfhogg verify ids <file> [...]`
  - new command from this proposal
- `pbfhogg verify all <file> [...]`
  - new aggregated validation gate (indexed + ids + refs)

Migration policy:

1. Keep current top-level commands as aliases for at least 1-2 releases.
2. Print deprecation warnings only for interactive use (or behind env/flag).
3. Keep exit-code semantics identical between alias and canonical commands.
4. Update docs/brokkr workflows to prefer `verify ...` commands first.

Scope note:

- Do not rename core transform commands now (`cat`, `sort`, `extract`, `merge`,
  `add-locations-to-ways`, etc.). They are already intent-clear and likely used
  in automation.
- Optionally, later consolidate informational commands under `stats`:
  - `node-stats` -> `stats node`
  - `tags-count` -> `stats tags`
