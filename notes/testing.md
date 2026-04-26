# Testing

Live tracker for pbfhogg's open test-infrastructure work. Cross-ref
`reference/performance.md` for perf baselines and TODO.md's
"Important: ignored tests" for the runbook on tests that don't run
by default. The historical narrative (what landed when, which
reorgs happened) lives in `git log -- notes/testing.md`.

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
- `pbfhogg::{BlobDecode, BlobError, BlobReader, BlobType, Element, ElementReader, ErrorKind, HeaderOverrides, MemberId, MemberType}`

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

## Open work

Work item IDs are stable. Cite by ID in commits / ADRs / other
notes.

### T02 - Indexdata-trust sites without regression tests

Three cluster-2 fixes ship regression tests; seven additional
indexdata-trust sites still lack direct coverage. Each can pick up
a regression test using the byte-level adversarial primitives in
`tests/common/adversarial.rs` when a regression appears or that
area gets touched:

- `renumber/pass1.rs:179`
- `renumber/wire_rewrite.rs:272`
- `renumber/stage2.rs:226-231`
- `altw/external/stage4.rs:438-478`
- `apply_changes/scanner.rs:162,188`
- `apply_changes/streaming.rs:496`
- `commands/inspect/show_element.rs:53-57`

**Sub-item: `count_varints_strict` surgical mutation primitive.**
`renumber_rejects_truncated_relation_blob_payload` currently chops
the last byte of the relation blob's PrimitiveBlock; renumber
rejects via the upstream protobuf walk before reaching
`count_varints_strict` (`src/commands/renumber/wire_rewrite.rs:556`).
To pin `count_varints_strict` specifically, extend
`tests/common/adversarial.rs` with a
`truncate_relation_memids(pbf, blob_idx, relation_idx)` primitive
(~30 lines walking PrimitiveBlock -> PrimitiveGroup -> Relation
field 9), then strengthen the test to assert the
`reframe_relations: ... memids|types` substring.

### T05 - Deferred parity surfaces

Three sites need lib-API plumbing of `jobs` before parity tests
can be added:

- ALTW external stage 4 worker count (currently hard-coded)
- Geocode Pass 1.5 / Pass 3 Stage A parallel degree
- `check --refs` (T09 below)

A future PR can plumb a `jobs` arg through whichever pipeline is
the focus and add the matching parity test then.

### T07 - Proptest backlog

Three deferred extensions:

- `parse_osc_file` proptest - the symbol takes a `&Path`, not
  bytes; needs a wrapper that writes bytes to tempdir and parses,
  or a different entry point.
- apply/derive inverse property - needs more scaffolding than fits
  the current batch.
- Header flag combinations - small additional set, low priority.

### T08 - Boundary-twin scan across modules

Practice memo, no discrete deliverable. When landing a fix in one
module, add one regression test per twin site in the same commit:

- `commands/sort/mod.rs:178-181` is the same overlap-run
  kind-boundary bug as the just-fixed `cat/dedupe.rs:225`.
- `write/parallel_writer.rs` and `write/parallel_gzip.rs` both
  silently swallow `Drop`-path errors.
- The kind-placeholder-on-non-indexed pattern from apply-changes
  recurs in altw, extract-multi, getid, cat, tags-filter.

Cheaper than chasing each finding individually; prevents the next
regression of the same pattern.

### T09 - check --refs parity

`check_refs` has no `jobs` override in its public signature
(`src/commands/check/refs.rs:141`); a parity test needs either a
plumbed `jobs` argument or a CLI-level worker-count probe (hard to
observe from outside). Not urgent - worker-count-independent
correctness is implicitly covered by existing single-blob tests.
Unblocks the final entry in T05.

### T10 - Fuzz testing via `cargo-fuzz`

Optional follow-up to T07; only worth the setup if someone wants
to run weekend campaigns. PBF parsing, OSC parsing, and wire-format
decoders all accept untrusted input. Targets at those entry points
would catch panics, OOM, and logic errors on malformed data.

**Cost:** `fuzz/corpus/` grows to hundreds of MB, low GB per target
over long campaigns; `fuzz/target/` is ~500 MB - 1 GB of build
artifacts. Both must be gitignored.

**Schedule:** smoke runs (60 s) only verify the harness; real
bug-hunting needs hours to days per target. Skip until T07 exposes
a gap that only coverage-guided fuzzing can fill.

### Truncation handling alignment [LANDED 2026-04-26]

Stance: [`reference/truncation-handling.md`](../reference/truncation-handling.md).
Every truncation shape except a clean cut at a frame boundary
(0-3 leftover bytes from an incomplete next length prefix) is a
hard error.

Aligned contract sites:

- `BlobReader::next` header short-read check (`blob.rs:400`)
- `BlobReader::next` payload short-read check (`blob.rs:528`)
- `BlobReader::next` length-prefix tolerance for 1-3 leftover
  bytes (`blob.rs:381`)
- `BlobReader::skip_blob_body` post-skip 1-byte sentinel read
  (`blob.rs:697`) - keeps the BufReader seek optimization for
  in-range targets, hard-errors on shape 4
- `HeaderWalker::next_header` probe-pread `UnexpectedEof =>
  Ok(None)` removal (`header_walker.rs:161`)
- `HeaderWalker::next_header` payload-extent check
  (`header_walker.rs:200`)
- `FileReader::skip` post-skip 1-byte sentinel read
  (`file_reader.rs:71`) - the `read_blob_header_only` caller-
  side path used by `has_indexdata`, `diff`, `cat::dedupe`, and
  `altw::passthrough` was originally missed; the audit narrowed
  scope to four primitive sites and the post-pass review
  surfaced this seventh site as a silent shape-4 hole. Same
  fix shape as `skip_blob_body`.

`read_raw_frame` already aligned (uses `read_exact` end-to-end);
gold-standard pattern.

`PrimitiveBlock::new` already aligned via the Cursor-based
protobuf walk in `WireBlock::parse_and_inline`.

Test coverage layered:

- Reader-level unit tests
  (`tests/read_paths.rs::trailing_partial_length_prefix_*`,
  `tests/corrupt_input.rs::truncated_header_size`,
  `truncated_header_data`) pin the `BlobReader::next` tolerance
  contract directly.
- `tests/cli_truncation_sweep.rs` is shape-aware: tolerated
  offsets (within 0-3 bytes of any frame_start) pin no-panic +
  bounded stderr only at the command level (sort may
  legitimately reject a tail truncation even when the reader's
  contract holds); shape-2-4 offsets assert non-zero exit.
- Truncation errors at shapes 3 and 4 in `BlobReader::next` /
  `skip_blob_body` include the byte offset and shape in stderr
  per the reference doc.

Caller-side defensive checks (e.g. `src/scan/classify.rs:90`)
remain as belt-and-braces but no longer load-bearing.

**Open follow-up: broaden the command-level sweep.** The current
`cli_truncation_sweep.rs` covers `cat`, `inspect`, and `sort`.
The reader contract is pinned by unit tests so every other
command inherits it for free, but three commands would add real
value if extended:

- `getid` (with `--` + `n1`) - exercises the
  `next_header_skip_blob` sentinel-read on the dominant
  HeaderWalker fast-path that all the other scan-style commands
  also use.
- `add-locations-to-ways` - exercises `altw::passthrough`'s
  `FileReader::skip` path (the seventh contract site fixed in
  commit `12699db`).
- `renumber` - exercises the full-read + pass-2 reframe walker
  through every shape.

Each is one line in the sweep loop. Estimated cost: ~25 s
additional sweep wall-time, pushing the per-sweep run from
~65 s to ~90 s. Within tier-1 budget but not free, hence
deferred.

### IdSet + CLI silent-rejection cleanup

`IdSet::set` (`src/idset.rs:45`) and `set_if_new`
(`src/idset.rs:65`) silently no-op on negative ids. Correct for the
storage layer (IdSet is bitset-backed, can't represent negatives)
but hides project-wide negative-id rejection from callers:

- getid produces a confusing "no IDs specified" when negative ids
  were correctly supplied (resulting set is empty).
- tags-filter's parallel-classify silently drops negative-id ways.
- cat/sort/inspect silently pass them through.

Tracked in TODO.md "Promote silent passthrough/drop to clean
error". The fix has two layers: surface negative-id rejection as a
typed error from `IdSet::set`; and add explicit `<id> < 0` guards
at each non-renumber command's first blob-element-walk site (per
the renumber error shape: `"<command> requires non-negative input
ids. Input contains <kind> id <id>. ..."`).

`tests/cli_negative_id_invariants.rs` pins the current state for
each command; when this lands, those tests fail deliberately and
prompt the migration to clearer error shapes.
