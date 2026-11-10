# Workspace crates and in-repo dev tools

Plan, not a spec. Raised 2026-11-10 from brokkr#145, which the brokkr
maintainer closed as not a bench concern: promoting an output into the
dataset graph is dataset bookkeeping, not measurement, and the suggested
direction is that pbfhogg owns its own download and promotion tooling,
after which brokkr's typed `cat`, `repack`, `degrade` and `download` are
removed rather than ported. `brokkr verify` is pbfhogg-specific too and
should leave brokkr the same way. brokkr#147 (declined) asks for the
merged base-plus-OSC input of `diff-applied` to become a registered
snapshot instead of a per-iteration rebuild.

The shape and order below were argued with a codex partner on 2026-11-10
until it had no remaining correctness objection; the points it won are
folded in, not listed separately.

## Layout

Every member lives under `crates/`; the root `Cargo.toml` becomes a
virtual manifest. Package names do not change.

| Directory | Package | Published | Role |
|---|---|---|---|
| `crates/lib` | `pbfhogg` | yes | the library, with its `src/`, `tests/`, `examples/` |
| `crates/cli` | `pbfhogg-cli` | yes | the `pbfhogg` binary |
| `crates/harness` | `pbfhogg-harness` | no | `brokkr library` harness |
| `crates/devkit` | `pbfhogg-devkit` | no | shared library for the tools; links no `pbfhogg` |
| `crates/datasets` | `pbfhogg-datasets` | no | bin: promote, derive, download |
| `crates/verify` | `pbfhogg-verify` | no | bin: cross-validation against osmium, osmosis, osmconvert |

Stays at the repo root: `docs/` (also the VitePress site: root
`package.json`, `.github/workflows/docs.yml`), `reference/`,
`decisions/`, `notes/`, `benches/` (Lua), `brokkr.toml`, `brokkr.d/`,
`scripts/`, `bench/` (the excluded `osmpbf-baseline` crate and the
planetiler Java file), every root convention file, `clippy.toml` (clippy
searches the manifest dir and its ancestors, so the root file still
applies to every member), `.brokkr/`, `data/`.

The library stays one crate. Splitting `src/` was considered and dropped:
a published `pbfhogg` can only depend on published crates, the hot paths
(wire encoding, block building, decode iterators) would cross crate
boundaries and risk losing inlining, and `pub(crate)` across a boundary
becomes public API. The one split with a case of its own is
`geocode_index` as a `pbfhogg-geocode` crate (own `s2` dependency, own
consumer through nidhogg, off the hot paths), and only if build times say
it pays.

## What assumes the current layout

Surveyed 2026-11-10. Each item names where the move must touch it.

**Cargo manifests.**
- Root `Cargo.toml` is both `[package] pbfhogg` and `[workspace]`. The
  package's `edition = "2024"` gives resolver 3 implicitly; a virtual
  manifest has no edition, so without an explicit `resolver = "3"` it
  silently drops to resolver 1.
- `[profile.release]` (fat LTO, one codegen unit) is only read from the
  workspace root: it stays in the virtual root. So do
  `[workspace.dependencies]`, `[workspace.package]`, `[workspace.lints]`
  and `exclude = ["bench/osmpbf-baseline"]`.
- No `default-members`: bare cargo at the root then selects every member,
  the new tools included. That ends the "root clippy lints only the root
  package" blind spot on purpose; the package-scoped sweeps stay scoped.
- `cli` and `harness` depend on `pbfhogg` by `path = ".."`; becomes
  `path = "../lib"`.

**Path mounts and manifest-relative reads in Rust.**
- `cli/tests/all.rs`, `harness/tests/all.rs`: `#[path =
  "../../tests/common/mod.rs"]` becomes `"../../lib/tests/common/mod.rs"`.
  `harness/tests/all.rs`'s `#[path = "../../cli/tests/guard.rs"]` still
  resolves unchanged.
- `src/path_guard.rs` and `src/test_support.rs` mount
  `../tests/common/*.rs`: unchanged, since `tests/` moves with `src/`.
- `cli/src/test_scratch.rs`, `harness/src/test_scratch.rs`:
  `CARGO_MANIFEST_DIR/../data/test-scratch`. After the move this is
  `crates/data/test-scratch`, which passes silently: `.gitignore`'s
  unanchored `data` pattern hides it. Switch both to the Cargo.lock
  ancestor walk `tests/common/scratch.rs` already uses.
- `harness/src/describe.rs` reads `CARGO_MANIFEST_DIR/../brokkr.toml`
  (loud on failure).
- `cli/src/lib.rs` reads `CARGO_MANIFEST_DIR/../docs/cli/commands.md`
  twice; `cli/src/io_reach.rs` has
  `include_str!("../../docs/cli/commands.md")`. All three are test-only.
- `cli/src/library_text_rule.rs` scans `CARGO_MANIFEST_DIR/../src` and
  asserts more than 50 files (loud).
- Doctests and unit tests open `tests/test.osm.pbf` relative to the cwd,
  which cargo sets to the package dir; `examples/gen_test_pbf.rs` writes
  `CARGO_MANIFEST_DIR/tests/test.osm.pbf`. Both stay correct when `tests/`
  and `examples/` move with the library. `src/write/file_writer.rs` and
  `src/write/io_buffer_contract.rs` read manifest-relative files that move
  with it too.

**brokkr.d and scripts.**
- `brokkr.d/lints.toml`: about 45 `[[textlint]]` rules with root-relative
  globs (`src/**`, `cli/src/**`, `harness/src/**`, `tests/**`,
  `cli/tests/**`, `harness/tests/**`) and exact paths in `exclude` and
  `must_scan`. A glob that selects nothing is neither a failure nor a
  warning in an ordinary check; only `brokkr check --audit` reports it
  (`brokkr man conventions shared-convention-path-selectors`). This is the
  main way a half-done move goes green.
- `scripts/check_source_invariants.py` globs `src/**/*.rs` and
  `cli/src/**/*.rs` (an empty glob is silent) and reads `cli/src/lib.rs`
  and `src/scratch.rs` (loud). `scripts/check_doc_links.py` special-cases
  `rel.startswith("src/")` and derives its top-level directory set from
  tracked files.
- `brokkr.d/checks.toml`: every sweep lists `packages` except `doctests`,
  which is workspace-shaped with `test_exclude_packages`; the gate
  profile certifies complete coverage. `brokkr.toml` `[member_check]`
  compiles every member in package mode.
- `brokkr.d/benches.toml` names the target by bin name (`pbfhogg`): no
  path.

**brokkr itself.** Finds pbfhogg by package and target name only
(`Project::cli_package` = `pbfhogg-cli`, `[pbfhogg.library.harness]
package`, `[bin] default` and `install`, cargo-metadata target discovery,
bench `target`). No hard-coded `cli/`, `harness/` or `src/` path was found
in its operational code. `find_config_dir` looks in the cwd and one level
up only, so brokkr must be run from the repo root (from `crates/cli` it
finds no config).

**Publishing.** The library's `include` allowlist ships `/reference/*.md`,
`/decisions/*.md`, `/docs/**/*.md`, `DEVIATIONS.md`, `CORRECTNESS.md`,
`LLM.md`, `CHANGELOG.md`, the licenses and `LIBRARY.md` (its readme). Once
the package root is `crates/lib` these are outside it and cannot be
included by pattern. `LIBRARY.md` links `CORRECTNESS.md` and
`DEVIATIONS.md` relatively; `cli/README.md` links `../LICENSE-*`;
`src/read/frame.rs` has a rendered relative Markdown link into
`reference/`.

**Citations.** About 300 path citations in `reference/`, `decisions/`,
`docs/`, `DEVIATIONS.md`, `CORRECTNESS.md` and `AGENTS.md` (51 in
`reference/testing.md` alone), and about 170 in `.rs` comments, name
`src/...`, `cli/...`, `harness/...` or `tests/...`. Not all are stale
after the move: `tests/test.osm.pbf` in a doctest is package-relative and
stays right.

**Not affected.** CI is the docs deploy only. `.cargo/config.toml` holds
an empty alias table. `Cargo.lock` is gitignored and stays at the root.

## The move

Three commits. A one-pass mechanical move was tried and abandoned; what
it lacked was step 1, which makes every layout assumption loud before
anything moves. Moving one crate at a time does not help: the coupling
is all in the library (shared `tests/common`, citations, lints), so it
only reorders the hard commit to last.

### Step 1: make the old layout's assumptions loud (no files move)

1. `resolver = "3"` in `[workspace]`.
2. Both `test_scratch.rs` copies, `describe.rs` and the three docs reads
   find the workspace root by the Cargo.lock ancestor walk instead of a
   fixed `..`.
3. `check_source_invariants.py` fails on any glob that matches no file.
4. A script in `scripts/` that, for every `[[textlint]]` rule, records
   the set of files it admits (after `exclude`, plus its `must_scan`
   list) as a manifest file. Run it now and keep the output for step 2.
5. A citation rewrite script in `scripts/` with a dry-run listing. It
   classifies every path-shaped token as repo-root (rewrite through the
   move map), package-relative (keep: `include_str!`, doctest inputs,
   manifest paths, `CARGO_MANIFEST_DIR` joins), external-project or
   historical (keep), and prints each class.

Gate: `brokkr check --gate` green; `brokkr check --audit` reports no dead
selector that was not there before; the dry-run listing reviewed by hand
and its unclassified bucket empty.

### Step 2: the move

1. `git mv` the three members into `crates/lib`, `crates/cli`,
   `crates/harness` (the library takes `src/`, `tests/`, `examples/`).
2. Root `Cargo.toml` becomes the virtual manifest (members, resolver,
   exclude, profile, workspace tables). `crates/lib/Cargo.toml` takes the
   `[package]`, features, dependencies, `[[test]]` and `[lints]` blocks.
   Path deps become `../lib`.
3. `#[path]` mounts per the survey.
4. Packaging: file symlinks in `crates/lib` for `LIBRARY.md`,
   `CHANGELOG.md`, `LICENSE-MIT`, `LICENSE-APACHE`, and the licenses in
   `crates/cli` (cargo packages a symlink as the target's content).
   `reference/`, `decisions/`, `docs/`, `DEVIATIONS.md`, `CORRECTNESS.md`
   and `LLM.md` stop shipping: nothing that builds needs them, and
   citations of them read as repository paths. `LIBRARY.md`'s links to
   `CORRECTNESS.md` and `DEVIATIONS.md` become repository URLs,
   `cli/README.md`'s `../LICENSE-*` become `LICENSE-*`, and the rendered
   link in `src/read/frame.rs` becomes a plain repository-path citation
   (readable on docs.rs, not navigable: a deliberate choice).
5. Every textlint glob, exclude and `must_scan` path, the `doctests`
   sweep's exclusions, and every script glob, through the move map.
6. Run the citation rewrite for real across docs, reference, decisions,
   root documents, `AGENTS.md` and `.rs` comments.

Gates, all required:
- `brokkr check --gate` green.
- `brokkr check --audit`: no dead selector.
- The step-1 admitted-set manifest, mapped through the move, equals the
  set recorded now, rule by rule, `must_scan` included.
- Package-relative `cargo package --list` per package before and after;
  the only differences are the dropped documents (library) and the added
  licenses (CLI). Then an archive build of both packages, the CLI built
  against the packaged library (`cargo package --workspace` does this
  through its local overlay). This proves standalone compilation, not
  tests or rendered links.
- The citation script's checker mode: zero obsolete repo-root citations.
- `brokkr bench --dry-run` of every bench (config wiring only), one small
  real bench (`brokkr bench --iterations 1 copy-pbf --dataset denmark`)
  for binary discovery and launch, and `brokkr library --all`.
- `git status` shows nothing untracked under `crates/` (catches a
  scratch dir landing in the wrong place).

### Step 3: prose

`AGENTS.md` Workspace and Conventions sections and `reference/testing.md`
"Test binaries" rewritten for the new layout (the path corrections
themselves already landed in step 2). Gate: `check_doc_links.py`.

## devkit

A library with no dependency on `pbfhogg`, so `verify` never inherits the
implementation under test.

- **Dataset registry transaction.** A port of brokkr's `DatasetToml`
  (`src/pbfhogg/download/registry.rs`): parse with `toml_edit`, batch all
  edits, one atomic replace. It writes to whichever composed file owns the
  dataset table (resolve `brokkr.toml`'s `include` list), in either host
  namespace (`[host.H.datasets.D]` or legacy `[H.datasets.D]`), not
  "always the current host file". It writes only keys brokkr's host schema
  accepts (`origin`, `download_date`, `pbf.V.file`/`xxhash`, `osc.N.*`,
  `snapshot.K.*`, `expect.diff-snapshots`): the schema refuses unknown
  fields, and brokkr keeps reading these files.
- **Durability order.** Write the staged file, fsync it, publish by
  no-replace rename, fsync the destination directory (and any new parent),
  then atomically replace the config file and fsync it and its directory.
  An error after the config rename is "committed, durability uncertain":
  never reported as unchanged, never rolled back destructively.
- **Digests.** XXH3-128 printed as 32 lowercase hex, as brokkr's `Xxh128`
  prints it. Gate: devkit's digest of every registered file equals its
  pin.
- **Layout.** Where a primary, a variant and a `snapshot.KEY` file live;
  snapshot key rules (`[a-zA-Z0-9_-]+`, `base` reserved).
- **External tools.** Lookup of osmium, osmosis, osmconvert, curl.
- **Building pbfhogg-cli** for verify: `cargo build --release --locked -p
  pbfhogg-cli` from the invocation's build root (never verify's own
  `CARGO_MANIFEST_DIR`), executable path read from cargo's JSON
  `compiler-artifact` message, never `target/release/pbfhogg`. Built once,
  before any capability probe, never rebuilt into the same path during a
  run.
- **Run lease and process control.** Each run gets an id that is never
  reused, a staging dir on the destination filesystem
  (`DATA/.tools-staging/RUNID`) and a work dir `SCRATCH/tools/RUNID`
  (outside every name `brokkr clean` removes today; `tools/` is not
  among them). Their relationship and a `LEASE` file are made durable
  before any child starts. The tool holds `LEASE` with an exclusive flock,
  CLOEXEC cleared so children inherit the open file description, and
  releases it by closing, never `LOCK_UN` (an unlock through any duplicate
  releases it for all). The tool is a child subreaper; before hashing,
  publishing, accepting a reference-cache entry or issuing a verdict, and
  on every failure path, it kills and reaps descendants in a loop until
  none remain. The lease is cooperative evidence for a future gc, not
  proof that every user has exited. Local filesystems only.
- **Reclaims nothing.** No gc, no cache eviction, no startup cleanup of
  old staging or runs. A collision never replaces an artifact. Unregistered
  files are deleted by the maintainer by hand until a gc has a liveness
  proof that survives supervisor death and descendants closing the lease
  (brokkr's per-hold cgroup where delegated, or a brokkr-side liveness
  query that also covers brokkr's reservation store, whose orphaned
  readers this lease cannot see).

## datasets

Bin `pbfhogg-datasets`, depending on devkit and on `pbfhogg` (no extra
features).

- `promote --dataset D --key K --variant indexed|raw --from PATH
  [--same-as SNAPSHOT]`: what brokkr's `promote_snapshot` and
  `promote_artifact` do, minus every destructive step. The artifact gets a
  fresh immutable name (dataset, key, variant and a digest prefix; an
  existing name is compared in full and a different content picks another
  name). An existing key needs `--replace`, which re-points the
  registration and leaves the old bytes. `--same-as` writes the
  `expect.diff-snapshots` `same` pair repack promotion records today.
- `derive repack|degrade|cat|merged ... --key K`: runs the producer
  in-process through the library (`cat`, `repack`, `degrade` and
  `apply_changes` compile without `commands`) into the staging dir, then
  promotes. `derive repack` records the same pair. `derive merged
  --snapshot S --osc N` is brokkr#147's answer: base plus one OSC,
  registered once.
- `download`, `download --as-snapshot K`, `download --refresh [--force]`,
  `download --osc-seq N`: a port of `brokkr download` (Geofabrik and
  planet sources, OSC roll-forward, Last-Modified check, indexed variant
  through an in-process `cat` instead of a built binary), with the
  no-delete rule above.

Run as `brokkr run pbfhogg-datasets -- ...`, which holds the machine lock
for the whole program and records history (`brokkr man run`). A locked
brokkr command beneath it refuses, so the tool never calls brokkr.

## verify

Bin `pbfhogg-verify`, depending on devkit and never on `pbfhogg`: no
verdict may rest on the code under test. It keeps its own header walker
(brokkr's `pbf_frames`).

- A port of every pbfhogg subcommand of `brokkr verify` (`sort`, `cat`,
  `extract`, `multi-extract`, `tags-filter`, `getid-removeid`,
  `getparents`, `add-locations-to-ways`, `check-ids`, `check-refs`,
  `degrade`, `repack`, `merge`, `merge-changes`, `tags-filter-osc`,
  `derive-changes`, `renumber`, `diff`, `all`) with `--dataset`,
  `--variant`, `--snapshot`, `--input`, `--osc`, `--osc-seq`,
  `--direct-io`, `-v`, and the OSC resolution with its primary-table
  fallback for encoding-only snapshots. `brokkr verify`'s nidhogg and
  elivagar subcommands stay in brokkr.
- The binary under test: built by devkit with features from argv plus
  the host section, or `--pbfhogg PATH` (from `brokkr build --commit REF
  pbfhogg`, which replaces `--commit`). `--pbfhogg` is exclusive with
  feature options: the artifact decides its features. Per-subcommand
  capability probing of `--overwrite`, `--staging-dir`, `--tmp-dir` and
  `digest --form` is ported, so an old binary still verifies.
- Reference cache: the full key (run id, tool identity with its `ldd`
  closure, controlled environment, input digests and extensions, argv with
  placeholders) and the integrity checks ported unchanged, in a new
  namespace. Never caches a pbfhogg observation or verdict. Check outputs
  live under the run's work dir instead of `target/verify/`.
- The rule that verify never runs above germany moves with it, as a
  refusal.
- Verdicts are consumed by nobody but the terminal, the exit code and
  `brokkr history`. The `expect` tables in the host files are not verify
  output: they are read by brokkr's typed answer commands
  (`src/pbfhogg/expect.rs`) and by pbfhogg's own Lua benches
  (`params.variant.expect`, `params.dataset.expect` in `benches/diff/`,
  `check-ids`, `check-refs`, `check-index`, `check-locations`), and
  promotion writes one kind of them. Porting verify touches none.

## Order

1. Step 1, step 2, step 3 of the move, each committed once its gate
   passes.
2. devkit, then datasets `promote` and `derive`. Gate: devkit unit tests in
   a new sweep (the coverage audit must certify them; `minimal` and
   `geocode-reader` stay library-only); a promote round trip on denmark
   whose registration brokkr then resolves and hash-checks
   (`brokkr bench --dry-run` naming the new snapshot); a `derive merged`
   snapshot used by `diff-applied`, rebound to take two registered PBFs as
   ordinary inputs (both digests recorded) and keeping its
   `raw-vs-indexed` label. Then file the retirement of typed `cat`,
   `repack`, `degrade` and the merged-diff cache against brokkr; actual
   retirement waits until every caller has a replacement.
3. datasets `download`. Gate: a denmark `--as-snapshot` and an OSC
   roll-forward whose entries match what `brokkr download` would have
   written, digests included. Then file the retirement of `brokkr
   download`.
4. verify. Gate: `verify all` on denmark and germany agreeing with
   `brokkr verify all` check by check. brokkr's implementation stays
   runnable until this lands; then file its retirement.

AGENTS.md's brokkr sections shrink at each retirement.

## Open questions

- **Documents in the published crate.** The plan drops `reference/`,
  `decisions/`, `docs/`, `DEVIATIONS.md`, `CORRECTNESS.md` and `LLM.md`
  from the library archive. The alternative is per-file symlinks for each
  (cargo would flatten them), at the cost of many links to keep in step.
  The maintainer's call.
- **Derived-snapshot provenance.** A `derive merged` snapshot should record
  its base digest, OSC digest and producing commit so a stale one is
  refused, but the host schema refuses unknown fields. Either brokkr gains
  a provenance field, or the tool keeps a sidecar file and the bench
  cannot see it. Needs a brokkr ticket.
- **gc.** Deferred, see devkit. Needs the liveness proof named there.
- **Retiring `brokkr verify --commit`.** Replaced by `brokkr build
  --commit` plus `--pbfhogg PATH`; the printed path has to be passed by
  hand, since capturing it into a variable is off limits for agents.
