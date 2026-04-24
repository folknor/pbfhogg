# Testing reorg audit

Input for the CLI-decoupled test reorg (plan in
[`testing.md`](testing.md) "Reorg: CLI-decoupled integration tests").
Goal: architectural rewrites (ALTW external, geocode builder passes,
diff shard shape, apply-changes pipeline) should not force edits
across the integration test suite. Today they do, because 18 of 30
`tests/*.rs` import internal module shapes.

The one deliverable that matters is the **import surface map** -
for each test file, which non-stable library symbols does it import?
Non-stable imports are the rewrite-coupling tax; a CLI-level
conversion removes them.

Test wall time is a separate, secondary question, and not one the
reorg is optimizing for.

## Landed infrastructure

- **`tests/common/cli.rs` / `CliInvoker`** (2026-04-24). Fluent
  `std::process::Command` wrapper; finds the binary via
  `CARGO_TARGET_DIR`/`CARGO_MANIFEST_DIR` + debug/release from
  `cfg!(debug_assertions)`. Smoke test in
  `tests/fixture_helpers.rs::cli_invoker_runs_version_command`. No
  new dev-dep.

## Stable allowlist (v1)

Imports from this allowlist do not couple the test to an internal
module shape. Everything else does.

- `pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData, Metadata}`
- `pbfhogg::writer::{PbfWriter, Compression}`
- `pbfhogg::{BlobDecode, BlobError, BlobReader, BlobType, Element, ElementReader, ErrorKind, HeaderOverrides, MemberId, MemberType}`

Rationale: these are the fixture-construction + output-verification
primitives. Every `cli_*.rs` integration test needs *some* of these
to produce and parse test PBFs; they're part of pbfhogg's stable
public library API (0.x semver) and changing them is a deliberate
breaking-change event, not a refactoring accident.

## Import surface map

Generated 2026-04-24 from `rg "^use pbfhogg" tests/`. 30 test files
(plus `tests/common/mod.rs` helper). 342 `#[test]` functions across
the flat files + 6 nested in `fault_injection.rs` = **348 total**.

Classification:

- **STABLE** - file touches only the allowlist. Conversion cost
  near zero; may stay library-level if it's specifically testing
  stable library behavior.
- **CMD** - file imports one or more command entrypoints (e.g.
  `apply_changes::merge`). Conversion means replacing the library
  call with a `CliInvoker` invocation + golden-output check.
- **DEEP** - file imports *nested* internal modules
  (e.g. `cat::dedupe::merge_pbf`, `tags_filter::osc::tags_filter_osc`,
  `diff::derive::derive_changes`). Highest rewrite-coupling risk;
  these are precisely the module shapes that change during an
  architectural rewrite.

| File | Tests | Class | Non-stable imports |
|---|---|---|---|
| add_locations_to_ways.rs | 18 | CMD | `altw::add_locations_to_ways` |
| apply_changes_invariants.rs | 13 | CMD | `altw::add_locations_to_ways`, `apply_changes::{merge,MergeOptions,MergeStats}` |
| cat.rs | 13 | CMD | `cat::{cat,CleanAttrs}` |
| check.rs | 1 | STABLE | - |
| cluster2_defensive_input.rs | 2 | CMD | `apply_changes::{merge,MergeOptions}`, `renumber::{renumber_external,RenumberOptions}` |
| corrupt_input.rs | 11 | STABLE | - |
| derive_changes.rs | 15 | DEEP | `apply_changes::{merge,MergeOptions}`, `diff::derive::derive_changes` |
| diff.rs | 30 | CMD | `diff::{DiffOptions,diff}` |
| edge_cases.rs | 9 | STABLE | - |
| extract.rs | 27 | CMD | `extract::{extract,extract_multi,parse_bbox,parse_geojson,ExtractStrategy,PolygonRings,Region,parse_extract_config,ExtractSlot}`, `cat::CleanAttrs` |
| fault_injection.rs | 6 | DEEP | `write::uring_writer_test_hooks`, `diff::parallel_test_hooks`, `diff::derive::derive_changes`, `diff::derive_parallel_test_hooks`, `geocode_index::builder::*`, `altw::*`, `write::parallel_gzip_test_hooks` |
| fixture_helpers.rs | 4 | STABLE | - |
| geocode_index.rs | 18 | STABLE | - (all `#[ignore]`d, ~154s) |
| getid.rs | 15 | CMD | `getid::{getid,parse_ids,removeid,GetidOptions}` |
| getparents.rs | 5 | CMD | `getid::parse_ids`, `getparents::{getparents,GetparentsOptions}` |
| inspect.rs | 10 | STABLE | - |
| merge.rs | 21 | CMD | `apply_changes::{merge,MergeOptions}` |
| merge_changes.rs | 8 | CMD | `merge_changes::merge_changes` |
| merge_pbf.rs | 8 | DEEP | `cat::dedupe::{merge_pbf,MergePbfOptions}` |
| non_indexed_parity.rs | 13 | STABLE | - |
| read_paths.rs | 20 | STABLE | - |
| renumber_external.rs | 12 | CMD | `renumber::{renumber_external,RenumberOptions}` |
| roundtrip.rs | 16 | STABLE | - |
| roundtrip_invariants.rs | 10 | STABLE | - |
| roundtrip_real.rs | 1 | STABLE | - (`#[ignore]`d, ~54s) |
| sort.rs | 12 | CMD | `sort::SortOptions` |
| tags_count.rs | 2 | CMD | `tags_count::{tags_count,TagCount,TagCountOptions,TagCountSort}` |
| tags_filter.rs | 20 | CMD | `tags_filter::{tags_filter,TagsFilterOptions}` |
| tags_filter_osc.rs | 3 | DEEP | `tags_filter::osc::tags_filter_osc` |
| time_filter.rs | 5 | CMD | `time_filter::time_filter` |

### Class totals

| Class | Files | Tests | % of tests |
|---|---|---|---|
| STABLE | 11 | 113 | 32 % |
| CMD | 14 | 192 | 55 % |
| DEEP | 4 | 32 | 9 % |
| fault (DEEP, special) | 1 | 6 | 2 % |
| helper / meta | 1 | 4 | 1 % |

### The rewrite-coupling tax, concretely

The 18 CMD + DEEP files import 25 distinct non-stable symbols across
14 command entrypoints and 5 deep internal modules. Any architectural
rewrite of one of those modules (ALTW stage split, diff shard shape,
cat dedupe reshape, tags_filter OSC path, geocode builder passes)
forces edits across the tests that import it. Specifically:

- `altw::*` - imported by 2 files (20 tests). ALTW external rewrites
  pay here.
- `apply_changes::{merge,MergeOptions,MergeStats}` - 4 files (51 tests).
  Apply-changes rewrites pay here.
- `diff::*` (including `diff::derive::derive_changes` and the two
  `*_test_hooks` submodules) - 3 files (51 tests). Diff/derive
  rewrites pay here.
- `extract::*` - 1 file (27 tests), but that one file exercises 9
  distinct extract symbols including two module-level helpers that
  are implementation details (`parse_bbox`, `parse_geojson`).
- `cat::{cat, dedupe::*, CleanAttrs}` - 3 files (34 tests). Cat/
  dedupe/extract share the `CleanAttrs` type.
- `renumber::*` - 2 files (14 tests). The module that just underwent
  the 2024-era external-join rewrite.
- `tags_filter::{self, osc::*}` - 2 files (23 tests). The OSC path
  is a nested submodule; that's the DEEP coupling.
- `geocode_index::builder::*` - 1 file (6 tests, fault-injection).
  Geocode Pass 1/1.5/2/3 rewrites pay here.

**Conversion priority ranking** (by rewrite-coupling × test count):
1. `merge.rs` + `apply_changes_invariants.rs` + `cluster2_defensive_input.rs` + `derive_changes.rs` → one `cli_apply_changes.rs`. 51 tests, highest-traffic rewrite surface.
2. `diff.rs` + `derive_changes.rs` overlap → `cli_diff.rs` + `cli_derive_changes.rs`. 45 tests.
3. `extract.rs` → `cli_extract.rs`. 27 tests, 9 non-stable symbols.
4. `add_locations_to_ways.rs` → `cli_altw.rs`. 18 tests, blocks ALTW rewrite.
5. Everything else (sort, cat, getid, getparents, tags_filter, tags_filter_osc, merge_changes, merge_pbf, renumber_external, tags_count, time_filter) - 126 tests across 11 files, each a small conversion.

The STABLE-class files (113 tests) need no conversion work for the
coupling axis - they already test against the stable surface.
