# Migration Plan: scripts/ → dev/

Phased implementation plan for replacing all bash scripts with a `dev/` binary crate.
pbfhogg first (most scripts, most complex), then elivagar and nidhogg follow the same pattern.

Each phase is self-contained: delivers working subcommands, allows deleting specific scripts.
Phases are ordered by dependency — later phases depend on infrastructure from earlier ones.

References: CLI.md for problem descriptions and solutions.

---

## Phase 1: Scaffold + Foundation

Create the `dev/` crate and core infrastructure that every subcommand depends on.

**Build:**
- `dev/Cargo.toml` — workspace member. Depends on `pbfhogg` with `default-features = false, features = ["zlib-ng", "commands", "linux-io-uring", "linux-direct-io", "libdeflater"]` (zlib features are mutually exclusive — can't enable both `rust-zlib` and `zlib-ng`). Also depends on `clap`, `toml`, `serde`. (`rusqlite` added in Phase 2 when db.rs is built.)
- Add `"dev"` to workspace `members` in root `Cargo.toml`.
- `dev/src/main.rs` — clap dispatch
- `dev/src/config.rs` — parse `dev.toml`, hostname lookup, defaults for unknown host
- `dev/src/lockfile.rs` — `flock(LOCK_EX | LOCK_NB)` on `{scratch}/.dev.lock`, print holder PID on conflict
- `dev/src/build.rs` — `cargo_build(BuildConfig)` with features/profile, binary path from `--message-format=json`
- `dev/src/output.rs` — `[build]`, `[run]`, `[result]`, `[error]` prefixed output, subprocess capture
- `dev/src/preflight.rs` — requirement checks: `Check::binary()`, `Check::file()`, `Check::disk_space()`, `Check::kernel_param()`. Run all checks before any work, report all failures at once.
- `dev/src/env.rs` — collect kernel, governor, available memory from `/proc/`. Drive types from config. Used by `dev env` and later by the bench harness for per-run environment snapshots.

**Subcommands:**
- `dev check` — clippy + test. No lock. Builds implicitly.
- `dev env` — dump hostname, kernel, governor, memory, drives, tool versions, dataset status.
- `dev run [args]` — build release CLI, exec with passthrough args. No lock.

**Config:**
- `dev.toml` — committed, with `[datasets.*]` and `[plantasjen]`/`[dm6]` sections. Real data, real hashes.

**Delete:**
- `scripts/build.sh`
- `scripts/clippy.sh`
- `scripts/test.sh`
- `scripts/run.sh`

**Update:**
- CLAUDE.md — replace references to deleted scripts (`build.sh`, `clippy.sh`, `test.sh`, `run.sh`) with `dev check`, `dev run`. Keep references to not-yet-migrated scripts. Update incrementally each phase to avoid stale instructions.

**Verify:** `dev check` passes. `dev env` prints correct info. `dev run cat --help` works.

---

## Phase 2: Bench Harness + SQLite

The timing/storage infrastructure that all bench subcommands share. No benchmarks yet — just the harness, database, and query tool.

**Build:**
- `dev/src/harness.rs` — `BenchHarness` with three timing modes (internal, external, distribution). Best-of-N. Dirty-tree detection (stdout-only if dirty, SQLite if clean). Uses `env.rs` (Phase 1) for per-run environment snapshots. Acquires lockfile for all bench/verify/hotpath/profile subcommands uniformly — individual subcommands don't manage locking.
- `dev/src/db.rs` — SQLite schema (`runs` table), create/migrate, insert, query. `dev results` formatting. Add `rusqlite` dependency.
- `dev/results.db` — created on first run, committed in git. Add `.gitattributes` entry to mark as binary.

**Subcommands:**
- `dev results` — query SQLite: `--commit`, `--compare`, `--command`, default last 20.

**Verify:** `dev results` works on empty database. Harness compiles and integrates with lockfile + config + output.

---

## Phase 3: First Benchmarks (Read + Write)

First real benchmarks, proving the harness end-to-end. All bench output goes to `{scratch}/`, not `data/bench-tmp/` or `$CARGO_TARGET_DIR`.

**Subcommands:**
- `dev bench read` — fold `examples/bench_read.rs` logic. Internal timing mode. Modes: sequential, parallel, pipelined, mmap, blobreader.
- `dev bench write` — fold `examples/bench_write.rs` logic. Internal timing mode. Modes: sync, pipelined. Compression variants.

**Delete:**
- `scripts/bench-self.sh`
- `scripts/bench-self-write.sh`
- `examples/bench_read.rs`
- `examples/bench_write.rs`

Note: `examples/gen_test_pbf.rs` stays — it's a test fixture generator, not a benchmark. The `examples/` directory is not deleted, only the `bench_*.rs` files are removed across Phases 3-4.

**Verify:** Run `dev bench read`, confirm `[result]` output, confirm row in `results.db`, confirm `dev results` shows it. Run on dirty tree, confirm no database write.

---

## Phase 4: Bench Merge + Bench Commands

The two most important remaining bench subcommands. Merge is the most complex (io_uring, passthrough, compression variants). Commands covers all CLI subcommand benchmarks.

**Subcommands:**
- `dev bench merge` — fold `examples/bench_merge.rs`. Internal timing. Variants: buffered+zlib, buffered+none, uring+zlib, uring+none, uring+sqpoll+zlib, uring+sqpoll+none. `--uring` flag triggers io_uring variants + preflight (RLIMIT_MEMLOCK, kernel). io_uring features are compiled into the dev binary (Phase 1); the flag selects the runtime code path.
- `dev bench commands [command|all]` — external timing mode. Runs pbfhogg CLI N times, measures wall-clock. Commands: cat-way, cat-relation, tags-count, tags-count-way, tags-filter-way, tags-filter-amenity, tags-filter-twopass, getid, removeid, add-locations-to-ways, extract-simple, extract-complete, extract-smart, node-stats. `all` runs the full suite. Compares against osmium where applicable.

**Delete:**
- `scripts/bench-merge.sh`
- `scripts/bench-uring.sh` — redundant with `dev bench merge --uring` (same test, crude version without structured output)
- `scripts/bench-commands.sh`
- `examples/bench_merge.rs`

---

## Phase 5: Remaining Bench Subcommands

Smaller, more specialized benchmarks.

**Subcommands:**
- `dev bench extract [--pbf file]` — replaces bench-extract-japan.sh. External timing. Uses configured dataset or explicit `--pbf`.
- `dev bench allocator` — replaces bench-allocator.sh. Cycles through jemalloc/mimalloc/system, rebuilds each time.
- `dev bench blob-filter` — replaces bench-blob-filter.sh. Indexdata vs non-indexdata comparison.
- `dev bench planetiler` — replaces bench-planetiler.sh. External tool preflight: auto-download JDK + Planetiler JAR if missing. Compiles Java benchmark class.
- `dev bench all` — combined suite, replaces bench.sh. Runs read + write + merge + commands sequentially. Builds and runs `bench/osmpbf-baseline/` for comparison.

**Build:**
- `dev/src/tools.rs` — external tool download/build/cache (JDK, Planetiler for now). Version files. Called from preflight. `dev bench planetiler` compiles `bench/planetiler-baseline/BenchPbfRead.java` against the downloaded JAR.

**Delete:**
- `scripts/bench-extract-japan.sh`
- `scripts/bench-allocator.sh`
- `scripts/bench-blob-filter.sh`
- `scripts/bench-planetiler.sh`
- `scripts/bench.sh`

---

## Phase 6: Verify Subcommands

All 10 verify scripts follow a pattern: build both tools, run both, diff output. The verify harness captures this.

**Build:**
- Verify harness in `dev/src/verify.rs` — run pbfhogg + reference tool (osmium/osmosis/osmconvert), capture output, compare via `pbfhogg diff` or element counts. Structured pass/fail `[result]` output. Acquires lock.

**Subcommands:**
- `dev verify merge` — 4-tool comparison (pbfhogg, osmium, osmosis, osmconvert). Osmosis needs JDK preflight.
- `dev verify sort`
- `dev verify cat` — all 3 type filters.
- `dev verify extract` — simple + complete-ways.
- `dev verify derive-changes` — 3-step roundtrip.
- `dev verify diff`
- `dev verify add-locations-to-ways`
- `dev verify tags-filter` — 3 expressions.
- `dev verify getid-removeid`
- `dev verify check-refs`
- `dev verify all` — run all sequentially.

**External tools:**
- Add osmosis download/cache to `tools.rs` (needs JDK).
- osmium, osmconvert: preflight check only (system packages, not auto-downloaded).

**Delete:**
- `verify/*.sh`
- `verify/lib.sh`
- `verify/` directory

---

## Phase 7: Hotpath + Profile + Data Management + Cleanup ✓

**DONE.** All 8 shell scripts migrated, `scripts/` and `benchmarks/` directories deleted.

Implemented: `cargo dev hotpath`, `cargo dev profile`, `cargo dev download`, `cargo dev clean`.
CLAUDE.md updated to remove script references.

---

## Phase 8: Elivagar dev/ crate (separate project)

Same pattern, independent crate. Elivagar-specific concerns on top of the shared infrastructure pattern.

**Foundation** (same as pbfhogg Phase 1):
- `dev/` crate scaffold, config, lockfile, build, output
- `dev.toml` with elivagar datasets + hostname sections
- `dev check`, `dev env`

**Run subcommands** (elivagar's primary workflow):
- `dev run [--mem N] [--skip-to X] [--no-ocean] [--compression-level N] [args]` — build + run elivagar. `--mem` wraps `systemd-run --scope -p MemoryMax`. Ocean shapefile auto-detected (preflight downloads if missing). `--tmp-dir` from scratch config. `HOTPATH_METRICS_SERVER_OFF` set for hotpath. Acquires lock.

**Bench subcommands:**
- `dev bench [--compare]` — self benchmark, optional comparison vs planetiler/tilemaker.
- `dev bench node-store` — fold example binary.
- `dev bench pmtiles` — fold example binary.

**Other:**
- `dev compare-tiles <a> <b>` — fold example binary.
- `dev pmtiles-stats <file>` — Rust rewrite of pmtiles-stats.py (Tier 2, can defer).
- `dev hotpath` / `dev hotpath --alloc`
- `dev profile --tool perf|samply` — builds with `--profile profiling`.
- Ocean shapefile management in preflight/tools.
- Tilemaker download/build in tools (for `--compare`).

**Delete:**
- All `scripts/*.sh`
- `scripts/lib.sh`
- `scripts/pmtiles-stats.py`
- `examples/` (folded into dev/)

**Keep:**
- `scripts/bisect-test.sh` — standalone, git bisect needs it as executable.

---

## Phase 9: Nidhogg dev/ crate (separate project)

Same pattern, plus server lifecycle.

**Foundation:**
- `dev/` crate scaffold, config, lockfile, build, output
- `dev.toml` with hostname sections (port, paths)
- `dev check` (sets `CARGO_TARGET_TMPDIR`), `dev env`

**Server lifecycle:**
- `dev/src/server.rs` — PID file management, health check polling, graceful shutdown.
- `dev serve [--foreground] [--tiles] [db]`
- `dev stop`
- `dev status`

**Ingest + update:**
- `dev ingest [pbf]` — build + run ingest. Acquires lock.
- `dev update` — build + run `nidhogg-update` binary.

**Bench:**
- `dev bench api` — distribution timing mode (min/p50/p95). Requires running server. Acquires lock.
- `dev bench ingest` — internal timing. Acquires lock.

**Integration tests:**
- `dev test integration` — start server if needed, run batch + geocode + readonly suites, stop server if we started it. Replaces test-batch.sh, test-geocode.sh, test-readonly.sh.

**Utilities:**
- `dev query [args]` — hit server API.
- `dev geocode [query]` — hit geocode endpoint.

**Delete:**
- All `scripts/*.sh`

---

## Order of Operations

**pbfhogg Phases 1-7** sequentially. Each phase is one planning+implementation session.

**Elivagar and nidhogg** (Phases 8-9) come after pbfhogg is done — the pattern is proven, the second and third implementations go faster because the design decisions are settled.

Within pbfhogg, **Phase 1 is the critical path.** Everything else depends on the foundation being right. Spend the most time getting config parsing, lockfile, build infrastructure, and output formatting solid. The bench/verify/hotpath phases are mechanical once the harness exists.

**Total: 9 phases.** 7 for pbfhogg, 1 each for elivagar and nidhogg.
