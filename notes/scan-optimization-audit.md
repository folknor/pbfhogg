# Scan-optimisation audit (2026-04-20)

Reconciled candidate list for two patterns landed this session that
ought to be applied more broadly:

1. **Buffered header walk → `HeaderWalker`** — sites that walk blob
   headers via `BlobReader::seekable_from_path`, `next_header_with_data_offset`,
   `read_blob_header_only`, or `FileReader::skip` and don't actually
   need the skipped blob bodies. On cold cache these read ~50 % of
   file size as wasted page-cache fill; `HeaderWalker`
   (`src/read/header_walker.rs`, pread-only with
   `posix_fadvise(POSIX_FADV_RANDOM)`) reads ~header size only.
2. **Sequential `BlobReader` + per-blob work → `parallel_classify_*`** —
   sites that loop `for blob in &mut reader` doing order-independent
   per-blob work, often with a `// Sequential ... avoid cross-thread
   retention` justification comment that was obsoleted by
   `DecompressPool` (commit `8f6999b`).

## Already shipped

- Pattern 1: `inspect` index-only path (`8c7f34d`), `getid --include`
  (`bb16193`), `diff` + `derive_changes` parallel shard walkers
  (`dae9a0f`, `06628d8` — own inline copy, known follow-up to
  consolidate onto `HeaderWalker`).
- Pattern 2: `inspect --nodes` and `inspect --tags` / `tags_count`
  (`b7d8aea`, `6ea0d94`).

## Sources

Four independent reviews fed this list:

- **Agent A** — Explore subagent, HeaderWalker applicability.
- **Agent B** — Explore subagent, sequential-to-parallel candidates.
- **Review 1** — external review, HeaderWalker applicability.
- **Review 2** — external review, sequential-to-parallel candidates.

Confidence shown below combines cross-source agreement with how
cleanly the pattern fits; high = all reviewers agreed and shape is
obvious, medium = some disagreement or a correctness caveat, low =
flagged but the win is marginal.

## Pattern 1: HeaderWalker migration candidates

### Tier S - single highest-leverage target

- **`src/scan/classify.rs::build_classify_schedule` and `_split`**.
  The shared primitive used by 10+ commands (extract all strategies,
  tags-filter, check --refs, check --ids, getid parse-ids, inspect
  --nodes, inspect --tags, geocode pass 2, apply-changes prefill,
  renumber, ALTW relation scan, multi-extract). Today it walks
  `BlobReader::seekable_from_path` + `next_header_with_data_offset`
  and skips bodies via `BufReader::seek_relative`. One internal
  rewrite to use `HeaderWalker` gives every caller the cold-cache
  I/O reduction without touching the callers themselves. Agent A
  called this out; Review 1 listed per-command wrappers instead.
  Confidence: high. **This is the one to do first.**

### Tier 1 - per-command header-only sites

- **`src/commands/extract/common.rs:98 build_blob_schedule_with_passthrough`** —
  scans all OsmData headers into `BlobDesc` + raw-passthrough flags.
  Bodies are read later by a separate pread pass. Callers: simple /
  complete / smart extract. Confidence: high.
- **`src/commands/extract/smart.rs:200 collect_pass1_generic`** —
  sorted path builds node / way / relation schedules plus
  `pass3_blob_schedule`. Bodies consumed in later phases. Callers:
  `extract_complete_ways`, `extract_smart`. Confidence: high.
- **`src/commands/extract/multi.rs:112 try_extract_multi_single_pass`** —
  per-type schedules + node passthrough metadata. Bodies consumed
  later by multi-region pread / raw-frame writers. Caller:
  `extract_multi` simple strategy. Confidence: high.
- **`src/commands/tags_filter/mod.rs:568` and `:737`** — the
  `tags_filter_two_pass` path does two full schedule scans using
  only header index / tagdata. Bodies unused in both scans; later
  pread classify / write phases read them. Confidence: high.
- **`src/geocode_index/builder/pass1_5.rs:153 build_pass2_schedules`** —
  node / way schedules plus `max_node_id`. Bodies consumed by
  geocode pass 1.5 and pass 2. Confidence: high.
- **`src/commands/altw/external/blob_meta.rs:28 scan_blob_metadata`** —
  collects kind / id / count / tagindex / frame metadata for every
  blob. Bodies consumed later by external-join stage 1, relation
  scan, and stage 4. Confidence: high.
- **`src/commands/renumber/schedule.rs:36 build_all_blob_schedules`** —
  per-kind `BlobTasks` with counts / ranges. Bodies consumed by
  pread rewriters in pass 1, stage 2d, R1, R2d. Confidence: high.
- **`src/commands/apply_changes/node_locations.rs:145 scan_node_blob_schedule`** —
  node-prefix header scan for needed-ID overlap. Bodies consumed
  later by `prefill_from_base`. Confidence: high. This is the
  follow-up already flagged in `notes/apply-changes-opportunities.md`.

### Tier 2 - correct-but-low-payoff probes

- **`src/commands/diff/mod.rs:44 check_sorted_and_indexed`** and
  **`src/commands/mod.rs:477 has_indexdata`** — O(1) probes that
  already short-circuit after the first OsmData blob. Bodies
  genuinely unused. HeaderWalker applies cleanly but the saving is
  tiny. Bundle with other walker work in the same files if
  opportunistic; don't do a dedicated pass.

## Pattern 2: Sequential → parallel migration candidates

### Tier 1 - clean fits

- **`src/commands/altw/mod.rs:325 collect_way_referenced_node_ids`** —
  scans way blobs, unions referenced node IDs into one `IdSet`.
  Order-independent: yes (set union). Per-worker IdSet can grow
  planet-wide, so use `parallel_classify_phase` (per-blob emit,
  main-thread merge), not `_accumulate`. Confidence: high.
- **`src/commands/altw/mod.rs:357 collect_relation_member_node_ids`** —
  unions node-member IDs across relation blobs. Relation-only state
  is sparse; `_accumulate` is safe. **Verify current state first**:
  the ALTW plan doc item #9 L1 (`6d71053`) says a metadata-driven
  relation scan landed that preads only relation blobs. That fix is
  orthogonal to the parallel migration — this audit item asks
  whether the scan itself is also parallel. Confidence: high if
  still serial; obsolete if parallel already.

### Tier 2 - correctness caveat

- **`src/commands/altw/dense.rs:191 build_node_index_dense`** —
  extracts `(id, lat, lon)` tuples and writes to dense mmap via
  `SharedDenseWriter`. Order-independent on canonical unique-node
  inputs but duplicate / corrupt node IDs make overwrite order
  observable. Safety shape: `_phase` (per-blob tuple result to
  main-thread writer). Confidence: medium.

### Tier 3 - uncommon paths

- **`src/commands/extract/simple.rs:173`** — unsorted pass 1
  spatial classify. Agent B flagged; Review 2 didn't. Fallback path
  for non-sorted inputs. Confidence: medium (low relevance — most
  inputs are sorted).
- **`src/commands/extract/smart.rs:146`** — unsorted
  `collect_pass1_generic` path. Same reasoning. Confidence: medium.

### Explicitly ruled out

- **`src/commands/altw/sparse.rs:164`** — globally order-dependent
  writer state (`prev_id` / `current_chunk` / `byte_pos`). Not
  parallelisable without restructuring the sparse index writer.
- **`src/commands/inspect/show_element.rs:40`** — early-exit on
  single match. Not a classify shape.
- Everything on the current-pipelined list in
  `reference/pipelined-reader-paths.md`: the 4.7× getparents
  sequential regression rules out converting those *back* to
  sequential; they're also not in scope for pattern-2 because the
  pattern is *sequential → parallel*, not *pipelined → pread*.

## Suggested ordering

1. **Migrate `build_classify_schedule` + `_split` to `HeaderWalker`**.
   Highest leverage, single contained change, touches ~10 downstream
   commands without per-caller edits. Do this first.
2. **Per-command Tier 1 schedule builders** (eight sites listed
   above). Mechanical migrations; each file gets a similar small
   diff. Do in one commit per command area (extract / tags-filter /
   geocode / altw / renumber / apply-changes) for clean `brokkr
   verify` gating.
3. **`collect_relation_member_node_ids` status check** — either
   confirm it's already parallelised (if so, strike from this list)
   or migrate to `_accumulate`.
4. **`collect_way_referenced_node_ids` → `parallel_classify_phase`**.
   Ships with measured planet impact from ALTW stage 1.
5. **Dense node index parallel migration** if ALTW dense path is
   still a priority workload (today the external path is the
   default; dense is legacy).
6. **O(1) probes** (`check_sorted_and_indexed`, `has_indexdata`) —
   opportunistic bundle only.
7. **Unsorted extract paths** — skip unless non-sorted inputs become
   a real workload.

## Consolidating inline `HeaderWalker` copies

Two existing sites already do HeaderWalker-style pread walks
inline instead of using the shared primitive:

- `src/commands/diff/parallel.rs::walk_file`
- `src/commands/diff/derive_parallel.rs::walk_file`

Both predate the `HeaderWalker` shared type. Noted as follow-ups
in the diff-snapshots TODO entry; migrating them to the shared
primitive is purely cosmetic (identical behaviour) and can ride
along with the Tier S migration if convenient.

## Planet baselines for audit commands

Stored `--bench 1` results on `plantasjen` before any of the
migrations in this doc have landed. Fresh pre-audit re-samples at
the current HEAD are queued in `./overnight.sh` and will be
stamped below when the run completes. Ages computed against
2026-04-20.

| Command | Prior UUID | Commit | Wall | Age | Post-audit UUID |
|---|---|---:|---:|---:|---|
| extract --simple (Europe bbox) | (no planet row) | `cadc3e6` | ≈96.3 s | 9d | *pending overnight* |
| extract --complete (Europe bbox) | (no planet row) | `cadc3e6` | ≈164.9 s | 9d | *pending overnight* |
| extract --smart (Europe bbox) | `2d028196` / `07dcdae3` | `cadc3e6` | 279 s / 267.5 s | 9d | *pending overnight* |
| extract --multi (5 regions) | — | — | — | — | *pending overnight* |
| tags-filter `-R w/highway=primary` | `f262f068` | (2026-04-18) | 51.8 s | 2d | *pending overnight* |
| check --refs | `64e9a394` | (2026-04-18) | 70.2 s | 2d | *pending overnight* |
| check --ids --full | `c498fff0` | `ef6ce09` | 69.5 s | 3d | *pending overnight* |
| inspect --nodes -j 16 | `c5edebe7` | `b7d8aea` | 56.8 s | today | *pending overnight* |
| inspect --tags -j 16 | `9d741341` | `6ea0d94` | 169.5 s | today | *pending overnight* |
| renumber | `abd74459` | (2026-04-18) | 204.5 s | 2d | *pending overnight* |
| diff-snapshots text -j 16 | `b02d86bc` | `06628d8` | 208.6 s | today | *pending overnight* |
| diff-snapshots --format osc -j 16 | `9b3fc2b9` | `06628d8` | 313.8 s | today | *pending overnight* |
| build-geocode-index | `b4b25c05` | `82db8ed` | 432.9 s | 2d | *pending overnight* |
| add-locations-to-ways (external) | `a406d77e` | `aee7727` | 661.2 s | 2d | *pending overnight* |
| apply-changes --osc-seq 4920 | `8e940f71` | `ef6ce09` | 756.3 s | 3d | *pending overnight* |

Nothing in the doc should be implemented until the overnight run
fills in the right-hand column — the 9-day-old extract rows in
particular sit across several commits' worth of unrelated changes
and aren't reliable comparators for post-migration measurement.
Once overnight completes, replace the *pending overnight* cells
with the stored UUID + commit + wall and use those as the "before"
reference in every subsequent migration commit.
