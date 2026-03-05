# pbfhogg TODO

## Needs review

- `f41ff7a` docs: record OSM ID ordering benchmark results
- `3b496c4` docs: record P2-13 extract pass1 regression attempt
- `300fdee` clean stale investigation notes and update TODO
- `6f1c9fa` diff: add --quiet and --output flags

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` — it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    brokkr check -- --ignored

`sorted_flag_but_unsorted_nodes_panics` in `tests/read_paths.rs` is `#[ignore]` — it
verifies the debug monotonicity assertion fires on unsorted nodes when `Sort.Type_then_ID`
is declared. Requires `debug_assertions` to be enabled in the test profile. Nightly 1.95
(2026-02-25) has a regression where `debug_assertions` is off in test builds.

## Performance

- [ ] **Rayon alternatives for slice-based parallelism** — Wild linker discussion
  ([davidlattimore/wild#1072](https://github.com/davidlattimore/wild/discussions/1072)) surveys
  alternatives (`paralight`, `orx-parallel`, `chili`, `forte`, `spindle`).
  Revisit only if rayon becomes a proven bottleneck.

- [ ] **Extract sorted pass1 (`37b7c19`): benchmark and clean up.** Parallelizes
  way/relation ID collection for sorted PBFs by batching blocks and using
  `par_iter` with thread-local Vecs. Algorithm is correct but has open issues:
  1. **No benchmark data.** Never measured — no results in brokkr at this commit.
     Two prior attempts regressed 14x and 33-43x respectively. Must run
     `brokkr bench extract` (Denmark + Japan, indexed) before and after to
     validate the optimization actually helps.
  2. **~300 lines of duplication** between `collect_pass1` and `collect_pass1_smart`.
     The sorted path, unsorted fallback, and batch-flush logic are near-identical.
     Extract shared helpers or a generic pass1 driver.
  3. **`Mixed | Empty` handler is a full sequential fallback** that defeats the
     optimization. A single Mixed block flushes both batches and processes all
     element types sequentially. Correct but fragile — rare in practice.
  4. **Vec-per-block allocation in batch helpers.** Each `par_iter` task creates
     new Vecs for local IDs. For 64 way blocks with ~8000 ways each, the
     `local_node_ids` Vec could hold millions of entries per batch.
  5. **`decode_threads(1)` may under-utilize.** Reduces pipeline decode to one
     thread since the consumer does its own parallelism. Sensible tradeoff but
     may leave the I/O thread idle waiting for the single decoder.

- [ ] **Run Germany full profiling suite** (4.5 GB, ~500M elements). Currently only
  merge timing exists — missing read baselines (`tags-count`, `check-refs`),
  decode+write (`cat --type`), and allocations. Run:
  `brokkr profile --dataset germany`

## Release prep

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Publish to crates.io
- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Add a CHANGELOG.md before first tagged release
- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

## Missing commands (osmium-tool parity)

- [x] **`merge-changes`** — merge multiple OSC files, optionally simplifying
  (keep only the last change per object by type+id; later input files win). Relevant upstream:
  [osmium-tool#262](https://github.com/osmcode/osmium-tool/issues/262) (duplicate IDs
  from broken input),
  [#282](https://github.com/osmcode/osmium-tool/issues/282) (same-version delete
  ambiguity with overlapping extracts),
  [osmosis#150](https://github.com/openstreetmap/osmosis/issues/150) (duplicate
  same-version updates abort simplify),
  [osmosis#72](https://github.com/openstreetmap/osmosis/issues/72) (simplification
  must not merge distinct action types with same ID).
- [ ] **`merge` (multi-PBF)** — merge multiple sorted PBF inputs into one output,
  deduplicating by highest version per object (distinct from `merge` apply-changes).
- [ ] **`getparents`** — reverse lookup: given IDs, emit ways/relations referencing
  them (`--id-file`, optional `--add-self`).
- [ ] **`renumber`** — reassign IDs (node/way/relation), with stable mapping and
  configurable start IDs.

## Missing flags on existing commands (osmium parity)

- [ ] **`add-locations-to-ways --ignore-missing-nodes`** — optionally continue instead
  of failing when a way references missing node coordinates.
- [x] **Relation-member nodes preserved by default** — untagged nodes referenced
  by relation members are always kept when dropping untagged nodes. No flag needed
  (osmium requires `--keep-member-nodes`; pbfhogg does this unconditionally).
- [ ] **`derive-changes --keep-details`** — include tags/refs/members on deleted
  objects in generated OSC.
- [x] **`diff --quiet`** — exit-code-only mode for CI/scripts without full textual diff.
- [x] **`diff --output <file>`** — write diff report to file instead of stdout.
- [ ] **`getid/removeid --default-type`** — allow bare numeric IDs by assigning a
  default object type.
- [ ] **`tags-filter --invert-match`** — inverse selection mode (drop matching objects,
  keep non-matching + required references).
