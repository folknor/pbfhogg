# Testing

Test placement, validation tiers, and conventions for pbfhogg.
Cross-ref `reference/performance.md` for perf baselines and
TODO.md's "Important: ignored tests" for the runbook on tests that
don't run by default.

## Test placement

Tests go where they couple least.

- **Inline unit tests** in `src/**/*.rs` `#[cfg(test)] mod tests` -
  module internals; die with the module on rewrite, which is the
  point.
- **Stable-API integration tests** in `tests/<topic>.rs` (e.g.
  `tests/roundtrip.rs`, `read_paths.rs`, `edge_cases.rs`) - use
  only the stable allowlist below.
- **CLI integration tests** in `tests/cli_*.rs` - drive the
  `pbfhogg` binary via `CliInvoker`; internal modules are invisible.
- **Fault-injection tests** in `tests/fault_*.rs` (one per binary) -
  own their `PANIC_AT_*` hooks per-process. Partially coupled to
  internal hook surfaces by design.
- **External cross-validation** in `brokkr verify`
  (`verify_<command>.rs` modules in brokkr) - compares pbfhogg
  output against osmium / osmosis / osmconvert.

Placement and tiering are independent axes.

## Validation tiers

Runtime-ranked. Each tier subsumes the cost of the previous; higher
tier = more expensive = run less often. `#[ignore]` is only a Cargo
mechanism, not the tiering model.

| Tier | Cost | When | Driven by |
|---|---|---|---|
| 1. Fast contracts | seconds | Every edit | `brokkr check` (default) |
| 2. Command slice | tens of seconds | While working on that command | `brokkr check --profile <cmd>` |
| 3. Full in-project | minutes | Before merge | `brokkr check --profile full` |
| 4. Scale/perf | hours | Performance work, release evidence | `brokkr bench`, `brokkr suite` |
| 5. External cross-validation | depends | Release gate | `brokkr verify` |

Tiers 1-3 are `brokkr check` profiles. Tiers 4-5 are separate
brokkr commands.

`brokkr check` enforces a fixed 20 s per-test watchdog with no
override, on every profile including `--profile full`. Tests slower
than that can never pass a `check` profile, so `brokkr.toml`'s
`[test.profiles.full]` carries a by-name `skip` list for the
over-watchdog tests (`merge_cross_validate_osmium`,
`sort_cross_validate_osmium`, `roundtrip_denmark`, and the six
`geocode_index` real-data tests) - without it the profile could
never pass, by construction. Those tests are `#[ignore]`d out of
`check`'s reach and must be exercised individually via `brokkr test
<name> --timeout <secs>`, which raises the per-test timeout up to
280 s. See TODO.md's "Important: ignored tests" for the current
by-name list and runbook.

`mod platform` and `mod serial` are orthogonal config overlays, not
tiers. Same for `#[ignore = "external"]` (escape hatch for in-tree
osmium checks).

### Development contract

- **During refactor:** edit code + inline tests + any fault test
  whose hook moved. Run `brokkr check`. Green ⇒ refactor
  structurally landing. Tiers 2-5 stay silent during iteration.
- **Before merge:** run tiers 2-3 to confirm CLI behaviour through
  the stable surface.
- **Before release:** run tier 4 for performance regression and
  tier 5 for external parity.

Tier 1 must be both fast AND structurally complete: every internal
contract a refactor could break needs a tier-1 test (inline unit OR
fault hook). Tier 1 must not carry real-dataset cost (a 54 s
Denmark roundtrip belongs in tier 3).

Tiers 2-5 are by construction internal-refactor-immune: every test
goes through the stable allowlist or the CLI surface, so a rewrite
of `src/commands/<X>/` cannot break them by type changes alone.
That is the load-bearing property of the test reorg.

### Stable allowlist

Imports from this set do not couple a test to an internal module
shape:

- `pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData, Metadata}`
- `pbfhogg::writer::{PbfWriter, Compression}`
- `pbfhogg::{Blob, BlobDecode, BlobError, BlobReader, BlobType, Element, ElementReader, ErrorKind, HeaderBlock, HeaderOverrides, MemberId, MemberType, Way}` (including `Blob::way_members`, `Blob::way_member_count`, `BlobReader::set_parse_waymembers`, `Way::shared_node_pins`, and `HeaderBlock`'s injected-prepass feature accessors)

Everything else is non-stable and requires CLI conversion.

### Per-command CLI test split

Each `tests/cli_<command>.rs` splits intent inside the file:

- **Root or `tier1` module**: small CLI contracts; runs in
  `brokkr check`.
- **`tier2`**: expanded matrices, `-j` parity, adversarial
  fixtures, scratch cleanup, command-local fault injection.
- **`tier3`**: slow self-contained correctness, larger fixtures.
- **`platform`**: `--direct-io`, `--io-uring`, MEMLOCK, feature
  surface - keep out of tier 1 unless cheap and reliable on the
  reference host.
- **`serial`**: requires `--test-threads=1` or static
  fault-injection state.

`tests/cli_sort.rs` is the canonical template for new `cli_*.rs`
files.

## External cross-validation

Lives in `brokkr verify`, not the in-tree test crate:

- The `VerifyHarness` template (`run_pbfhogg`, `run_tool`,
  `diff_pbfs`, `check_sorted`, dataset config, variant matrix,
  results storage) already exists. Each new comparison is
  `verify_<command>.rs`, ~50 lines.
- External tools (osmium, osmosis, osmconvert) are operationally a
  brokkr concern. Contributors don't need them installed for clean
  `brokkr check`.
- An in-tree `mod external` tier would duplicate brokkr's verify
  machinery and act as a gravity well.

**Two in-tree tests are migration candidates:**

1. `tests/merge.rs::merge_cross_validate_osmium` - real Denmark
   data, same inputs as `brokkr verify merge`. Retire once
   `verify_merge.rs` handles the version-vs-unconditional-delete
   tolerance that `tests/merge.rs:1271-1295` does explicitly
   (osmium uses version-based deletes; pbfhogg/osmosis/osmconvert
   delete unconditionally, so osmium-only elements that fall in
   the OSC delete set are not real failures).
2. `tests/cli_sort.rs::sort_cross_validate_osmium` - handcrafted
   overlapping-blob fixture; `brokkr verify sort` runs against
   real data only and doesn't exercise the streaming sweep merge's
   overlap-run path. Migration requires `brokkr verify <command>
   --input <path>` plus an `examples/overlapping_fixture.rs`
   builder, then move the comparison into `verify_sort.rs`.

**Escape hatch.** A contributor mid-PR can write an osmium check
next to a fixture for the duration of that PR as
`#[ignore = "external"]` in-tree with a runbook comment, then
convert to a `verify_*.rs` PR against brokkr afterward. Permitted
exception, not a tier - should not survive across many PRs.

## Conventions

- **`test-hooks` Cargo feature.** Gates fault-injection hooks across
  every parallel pipeline. Off by default; enabled under
  `--all-features` (which `brokkr check` uses). Release builds
  never see the hook code.
- **Two hook shapes.** Per-instance field on a public config struct
  (race-free with sibling tests) vs. process-global static atomics.
  Picker: per-instance when the pipeline has a public config struct
  on its entry path, static atomics otherwise. Per-binary
  isolation via the fault-injection split makes static-atomic hooks
  race-free without `#[ignore]` or `--test-threads=1`.
- **CliInvoker for CLI-driven tests.** `tests/common/cli.rs`. Every
  new `tests/cli_*.rs` goes through it. 60 s default wall-clock
  timeout, override via `.timeout(Duration)`. Platform-skip
  predicates `is_o_direct_unsupported`, `is_uring_unsupported`.
  Binary located via `BROKKR_TEST_BIN_DIR` (set per sweep) with
  fallback to `CARGO_TARGET_DIR + cfg!(debug_assertions)`.
- **Adversarial fixtures.** `tests/common/adversarial.rs` provides
  byte-level mutation primitives: `locate_blobs`,
  `mutate_blob_header_indexdata`, `mutate_blob_payload`,
  `truncate_to`. Hand-rolled varint reader, no internal pbfhogg
  imports.
- **Negative-id generators.** `tests/common/mod.rs` provides
  `generate_nodes_with_negatives`, `generate_ways_with_negatives`,
  `generate_relations_with_negatives` for mixed-sign fixtures (per
  `decisions/0002-negative-ids-rejected-project-wide.md`).
- **Scratch tracking.** `tests/common/mod.rs` exports `snapshot_dir`
  and `assert_scratch_unchanged` for before/after comparisons
  around error paths, plus `with_tracked_scratch_dir(scratch_root,
  expected_new_paths, f)` wrapper.
- **Property tests.** `tests/proptests.rs`, 64 cases per property.
  `proptest-regressions/` is gitignored to avoid committing
  case-specific reproducers.
- **Hook consolidation (explicitly don't).** Static-atomic submodules
  across parallel_writer / parallel_gzip / uring_writer /
  diff-parallel / derive-parallel / altw-stage3 / geocode-pass3
  must stay per-module. Per-binary isolation depends on each binary
  owning its own copy of the atomics.
- **Policy proposal (not-yet-adopted).** Every new parallel pipeline
  should ship with three tests: a worker-panic test, a `-j N` vs
  `-j 2` parity test, and a scratch-leak test. Worth considering as
  a CI gate.
