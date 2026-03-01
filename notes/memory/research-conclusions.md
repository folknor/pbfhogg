# Planet Memory Research Conclusions

## Scope
- This is a theory/research conclusion based on current pipeline structure and code paths.
- Focus is memory feasibility for planet-scale ingest/merge on 64 GB RAM.

## Executive Conclusion
- The strongest remaining memory bottleneck is the **diff representation (`DiffOverlay`)**, not raw blob read/write mechanics.
- The next bottleneck is **in-flight merge buffering policy** (fixed-size batches and phase materialization), especially in rewrite-heavy windows.
- Existing decompression/compression reuse is already strong, but there are still meaningful wins in **bounded retention** and **frame assembly strategy**.
- With targeted refactors (compact diff model + byte-budgeted in-flight processing), 64 GB operation looks realistic.

## Key Findings

### 1) Diff representation is likely the biggest RAM lever
- Current overlay uses hash maps/sets plus heap-rich entities:
- `HashMap<i64, OscNode/Way/Relation>` and `HashSet<i64>` deletes in [src/osc.rs](/home/folk/Programs/pbfhogg/src/osc.rs:51)
- `OscNode/Way/Relation` contain owned `String` and `Vec` fields in [src/osc.rs](/home/folk/Programs/pbfhogg/src/osc.rs:24)
- Multiple diffs are parsed then merged into one overlay in [src/osc.rs](/home/folk/Programs/pbfhogg/src/osc.rs:474)
- Conclusion: structural overhead (hash table + per-object allocations + duplicated strings) is the top likely RSS contributor on backlog merges.

### 2) Merge working-set is throughput-optimized, not memory-governed
- Merge uses fixed `BATCH_SIZE=64` in [src/commands/merge.rs](/home/folk/Programs/pbfhogg/src/commands/merge.rs:1016)
- Per batch it can hold:
- raw frames
- classify outputs
- rewrite jobs (with per-job inline upsert vectors)
- rewrite outputs before write drain
- See the 4-phase structure in [src/commands/merge.rs](/home/folk/Programs/pbfhogg/src/commands/merge.rs:932)
- Conclusion: this design is fast but can over-hold memory during large-blob or high-rewrite segments.

### 3) Good reuse exists, but retention is not bounded
- Read side has pooled decompression buffers in [src/read/blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs:33)
- Buffers are returned on drop, but pool retention has no explicit cap policy in [src/read/blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs:57)
- Conclusion: can retain large capacities after spikes; this hurts tail RSS predictability.

### 4) Writer still does per-blob final frame allocation
- `frame_blob_into` assembles a full output frame in a new `Vec` in [src/write/writer.rs](/home/folk/Programs/pbfhogg/src/write/writer.rs:733)
- Conclusion: not the top bottleneck, but a clear allocator-pressure optimization target.

### 5) Small but real hot-path allocation waste in inline upserts
- Per rewrite blob, inline upserts are copied into a new `Vec` via `to_vec()` in [src/commands/merge.rs](/home/folk/Programs/pbfhogg/src/commands/merge.rs:1118)
- Conclusion: secondary optimization; helpful especially in rewrite-heavy runs.

## Priority Recommendations

### P1. Rebuild diff model for memory density (highest ROI)
- Use arenas + `id -> offset` indexes.
- Intern strings globally for diff load.
- Store coords as `i32` decimicro at parse.
- Expected impact: largest peak RSS reduction.

### P2. Switch merge from fixed-count batching to byte-budgeted in-flight control
- Cap live in-flight bytes instead of fixed blob count.
- Add adaptive backpressure based on memory budget.
- Expected impact: stabilize worst-case RSS spikes.

### P3. Stream rewrite outputs instead of full phase materialization
- Push rewrite outputs incrementally toward writer ordering stage.
- Reduce simultaneous ownership of raw + parsed + rewritten payloads.
- Expected impact: meaningful peak reduction in rewrite-heavy windows.

### P4. Add bounded buffer pool retention policy
- Keep size-classed caps for pooled decode buffers.
- Drop oversized return buffers beyond cap.
- Expected impact: better RSS recovery after outliers.

### P5. Remove per-job inline upsert copies
- Replace owned per-job vectors with shared-range views.
- Expected impact: lower allocation churn; modest RSS gain.

### P6. Writer framing without full-frame concatenation
- Use segmented/vectored output path where possible.
- Expected impact: allocator pressure reduction on write path.

## What Not to Expect
- Compression backend tuning alone is unlikely to solve the 64 GB problem.
- Minor micro-optimizations in element wrappers/iterators are unlikely to move peak RSS materially.
- Planet reliability will not come from a single tweak; it requires coordinated data-model + in-flight policy changes.

## Risk Assessment
- Lowest risk: P4/P5 (bounded pools, removing inline copy vectors).
- Medium risk: P2/P3 (pipeline behavior/backpressure/order handling).
- Highest risk but biggest upside: P1 (diff model redesign).

## Decision Statement
- If only one major effort is funded first: **do P1 (compact diff model)**.
- If two efforts are funded: **P1 + P2** gives best path to deterministic 64 GB operation.
- If peak still exceeds target after P1/P2/P3: evaluate architectural fallback modes (disk-backed diff or two-pass planner/executor).

## Related Plan
- Detailed execution matrix is in [PLANET_MEMORY_EXPERIMENT_MATRIX.md](/home/folk/Programs/pbfhogg/PLANET_MEMORY_EXPERIMENT_MATRIX.md).
