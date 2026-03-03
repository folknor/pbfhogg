# add-locations-to-ways Dense Index Safety Investigation (2026-03-03)

## Why This Note Exists

`add-locations-to-ways` is part of production. During the consolidation deep-dive, I found a potential memory-safety issue in pass 1 index construction that should be explicitly evaluated before any refactor.

This document is investigation-only: no code change is proposed here.

## Executive Summary

- The pass-1 dense node index writer uses raw pointer writes from rayon threads with `unsafe impl Send + Sync`.
- Current safety argument depends on a global invariant: node IDs are unique in input data.
- If that invariant is violated (corrupt input, malformed merges, unexpected upstream behavior), two threads can write the same slot concurrently.
- In Rust memory-model terms, concurrent non-atomic writes to same location are a data race and therefore undefined behavior (UB), even if values are identical.
- Probability on healthy OSM snapshots is likely low; impact is high because UB in production pipelines is unacceptable for world-class reliability.

For your concrete pipeline (`cat` -> `merge` -> `add-locations-to-ways`):
- `cat` currently preserves duplicates (it does not validate or deduplicate IDs).
- `merge` is structurally likely to keep/produce unique final IDs when base + OSC are valid, but this is not an explicit enforced invariant at file level.
- `check-refs` validates references, not uniqueness, so it cannot mitigate this UB class.

## Relevant Code Areas

- Dense index + sentinel design:
  - `src/commands/add_locations_to_ways.rs:27`
  - `src/commands/add_locations_to_ways.rs:44`
- Unsafe parallel writer:
  - `src/commands/add_locations_to_ways.rs:135`
  - `src/commands/add_locations_to_ways.rs:142`
  - `src/commands/add_locations_to_ways.rs:143`
  - `src/commands/add_locations_to_ways.rs:149`
- Parallel indexing call chain:
  - `src/commands/add_locations_to_ways.rs:380`
  - `src/commands/add_locations_to_ways.rs:410`
- `cat` behavior relevant to mitigation:
  - `src/commands/cat.rs:43`
  - `src/commands/cat.rs:113`
  - `src/commands/cat.rs:175`
- `check-refs` behavior relevant to mitigation:
  - `src/commands/check_refs.rs:89`

## Current Design

Pass 1 builds a dense mmap index keyed by `node_id`:

- 8 bytes per slot (`lat i32`, `lon i32`) in mmap file-backed storage.
- Capacity default is 16B IDs (`128 GB` virtual address space).
- Parallel fill:
  - blocks are processed in rayon (`batch.par_iter().for_each(...)`),
  - each node executes `SharedDenseWriter::insert(...)`,
  - writer computes slot offset and performs two `copy_nonoverlapping` writes (4B + 4B).

Safety comments assert disjointness from node ID uniqueness.

## How `cat` and `check-refs` Affect This Risk

## `cat` in step 1

### What `cat` currently guarantees

- Adds/propagates indexdata in the filtered path.
- Preserves file-level element order semantics needed by downstream commands.

### What `cat` does not guarantee

- No duplicate ID detection.
- No uniqueness enforcement for node/way/relation IDs.
- No "strict sorted unique IDs" validation before emitting output.

### Net effect on this safety issue

`cat` neither increases nor reduces duplicate-ID risk by itself. It mostly normalizes format/indexdata and preserves whatever ID-quality exists in source inputs.

## `check-refs` as mitigation

### What it checks

- Missing node refs in ways.
- Missing way/node/relation members in relations.

### Why it does not mitigate this issue

- Uses set insertions for seen IDs (`RoaringTreemap::insert`), so duplicate IDs are collapsed silently.
- Referential integrity can be "OK" while duplicate IDs still exist.

### Net effect

`check-refs` is valuable for topology integrity, but orthogonal to duplicate-ID / UB prevention for dense index writes.

## Pipeline-Specific Mitigation Options

## Option 1: Add duplicate-ID verification to `cat` (strict ingest mode)

### Concept

Add `cat --verify-unique-ids` (or `--strict`) that:
- validates strict monotonic increasing IDs per type for sorted inputs, or
- tracks seen IDs (bitset/hash) for unsorted/multi-input cases.

### Pros

- Catches unsafe inputs early at ingest boundary.
- Fits your current step-1 ownership point.

### Cons

- For unsorted inputs, full duplicate detection can be expensive at planet scale.
- For sorted inputs, cheap monotonic check catches duplicates but assumes sortedness.

### Planet-scale estimate

- Sorted monotonic mode: low overhead (~+1% to +4%).
- Full set-based uniqueness mode: moderate/high overhead (+5% to +25%, memory-sensitive).

## Option 2: Extend `check-refs` to optionally check uniqueness

### Concept

Add a flag like `check-refs --check-duplicate-ids`.

### Pros

- Reuses existing validation workflow.
- One extra command run in pipeline can gate downstream safety.

### Cons

- `check-refs` currently messages/semantics are about references; mixing concerns may reduce clarity.
- Duplicate check cost may dominate runtime if enabled by default.

### Planet-scale estimate

- If implemented as strict monotonic check on sorted files: low overhead.
- If implemented as full seen-ID sets: can add significant memory/time.

## Option 3: New command `verify` (recommended architectural direction)

### Concept

Create dedicated validation command family, e.g.:
- `pbfhogg verify ids`:
  - per type: sorted, strictly increasing, no duplicates
  - optionally `--full` for unsorted duplicate detection
- `pbfhogg verify refs`: wraps current check-refs
- `pbfhogg verify all`: runs IDs + refs (+ optional indexdata presence, header invariants)

### Why this is preferable

- Keeps validation concerns explicit and composable.
- Lets production pipelines choose strictness profile.
- Avoids overloading `cat` and `check-refs` responsibilities.

### Planet-scale estimate

- `verify ids` (sorted strict mode): near-streaming, low memory, low overhead.
- `verify ids --full`: higher memory/time, should be optional.

## Option 4: Safety hardening inside `add-locations-to-ways` regardless of upstream checks

Even with perfect upstream validation, hardening pass-1 writes (atomic or safe mode) removes UB class by construction. Upstream verify then becomes defense-in-depth instead of sole protection.

## Risk Characterization

## 1) Memory Safety Risk

### Condition that causes UB

UB can happen if two threads write the same slot at the same time:

- same `node_id` appears in two concurrently processed blocks,
- both take the same `offset`,
- both do non-atomic writes to overlapping bytes.

This violates Rust’s data-race rules regardless of expected semantic equality.

### How realistic is the condition?

- Canonical OSM planet/history snapshots are expected to have unique node IDs in a final snapshot.
- But production systems eventually see edge data:
  - truncated/corrupt files,
  - buggy upstream merge/sort runs,
  - partial updates stitched incorrectly,
  - future format/user inputs outside strict assumptions.

Given live revenue dependency, low-probability + high-impact UB should still be treated as `P0 safety`.

## 2) Data Correctness Risk (even without UB)

Independent from UB, current index uses `(0,0)` sentinel for "unset". Valid nodes at exactly Null Island are interpreted as missing by design (already documented in code). This is a correctness compromise, not memory safety, but it should be considered in strict pipelines.

## Performance/Memory Context At Planet Scale

Current design is extremely fast because it is:

- lock-free,
- direct slot indexing,
- mostly sequential writes within each block.

Any safety fix must preserve throughput and keep memory bounded.

## Candidate Mitigations (No Code Yet)

## Option A: Atomic packed slot writes (`AtomicU64`)

### Idea

Store packed `(lat,lon)` as `u64` and use atomic store on each write.

### Safety

Removes data-race UB even if duplicate IDs exist.

### Throughput estimate (planet-scale)

- Expected overhead: +3% to +12% in pass 1 due to atomic stores and potential cache-line contention on duplicates.
- Duplicate-free normal case likely near lower end.

### Memory impact

- No significant additional memory over current dense mmap footprint.

### Notes

- Needs careful alignment guarantees for atomic access across mmap buffer.
- Implementation complexity is moderate.

## Option B: Per-thread local maps then serial merge

### Idea

Each rayon thread accumulates `(id -> coord)` locally; merge into dense index afterward.

### Safety

Safe by construction (no concurrent writes to shared slots).

### Throughput estimate

- Likely slower for planet-scale due to large local maps and merge phase.
- Estimated +10% to +40% pass-1 wall time depending on allocator pressure and merge strategy.

### Memory impact

- Potentially very large transient memory (many GB) if local structures grow big.

### Notes

Not attractive unless correctness needs dominate and atomic approach is rejected.

## Option C: Single-thread fallback for untrusted input

### Idea

Keep current fast parallel path for "trusted snapshot" mode; add `--strict-safe-index` (or inverse) to force sequential insertion.

### Safety

Sequential mode fully avoids race.

### Throughput estimate

- Sequential pass-1 slowdown likely significant: +40% to +200% (dataset and machine dependent).

### Memory impact

- Minimal change.

### Notes

Operationally useful as immediate risk-control knob.

## Option D: Keep parallel path + detect duplicates before writing

### Idea

Pre-check for duplicate IDs in a prior pass.

### Safety

Could gate into safe fallback, but pre-check itself is non-trivial and may be as expensive as indexing.

### Throughput estimate

- Worst for production throughput due to extra full scan.

### Notes

Not recommended as default strategy.

## Recommended Investigation Path

1. Confirm production input guarantees:
- Are duplicate node IDs impossible by contract in your upstream artifacts?
- Do you ever process partially merged/custom PBFs where this may not hold?

2. Decide where to enforce invariants in your pipeline:
- `cat` strict mode, `verify` command, or both.
- If you prefer command separation, implement `verify ids` first and gate step 3 on it.

3. Benchmark safety candidate with minimal blast radius:
- Prototype `AtomicU64` slot writes in pass 1 only.
- Measure with `brokkr bench commands add-locations-to-ways` on at least Denmark/Japan/North America variants.
- Compare:
  - pass-1 time,
  - end-to-end command time,
  - RSS peak.

4. Decide policy:
- If overhead is acceptable, make atomic writes default.
- If not acceptable, provide explicit "trusted-fast" and "strict-safe" modes with clear docs and default selected by your risk posture.

## Suggested Acceptance Criteria

- No UB path remains under malformed duplicate-ID inputs.
- End-to-end regression on main production dataset:
  - target <= 5% preferred,
  - hard ceiling <= 10% unless safety requirement mandates more.
- No meaningful RSS increase (>200 MB) at North America scale.

## Open Questions — Resolved (2026-03-03)

1. **Do you treat malformed/custom PBF inputs as in-scope for production, or only canonical snapshot artifacts?**
   Only canonical snapshots (Geofabrik/planet). This limitation must be documented
   frankly in README.md and the project website. Custom/third-party PBFs are
   not a supported production input.

2. **Is a small steady-state slowdown (for safety hardening) acceptable, and if so what budget?**
   Cannot answer without measuring. Implement `AtomicU64` first, benchmark on
   Denmark (at minimum), then decide based on actual numbers.

3. **Do you want a runtime mode split (`safe` vs `fast`) or a single always-safe default?**
   Single always-safe default. No `--fast-unsafe` flag — atomic writes are the
   only mode. If the overhead is acceptable (expected to be near-zero on x86
   with `Relaxed` ordering on disjoint slots), there is no reason to offer an
   unsafe alternative.

## Decision: AtomicU64 (Option A)

Chosen approach: replace `copy_nonoverlapping` in `SharedDenseWriter::insert`
with `AtomicU64::store(Relaxed)` and pair with `AtomicU64::load(Relaxed)` in
`DenseMmapIndex::get`. Removes UB by construction with no mode split.

Next steps:
1. Implement atomic slot writes and reads.
2. Benchmark pass-1 and end-to-end on Denmark via `brokkr bench commands add-locations-to-ways`.
3. If overhead is acceptable, ship as the only mode.
