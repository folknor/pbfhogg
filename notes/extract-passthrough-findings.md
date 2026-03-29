# Extract performance: final state

## Current architecture (commit `774d3c0`)

Three strategies, each planet-safe with bounded memory:

**Simple (sorted single-pass):** 3-phase barrier pipeline. Each phase:
classify (lightweight scanner) → write (pread-from-workers).
- Phase 1: node-only scanner for bbox IDs + pread write matching nodes
- Phase 2: way-ref scanner against frozen bbox_node_ids + pread write
- Phase 3: sequential relation classify + pread write

**Complete (two-pass):** Pass 1 classification via `collect_pass1_generic`
(sequential BlobReader). Pass 2 write via `pread_write_pass` (workers own
full PrimitiveBlock lifecycle).

**Smart (three-pass):** Pass 1 same as complete. Pass 2 way closure
(sequential, mutates extra ID sets). Pass 3 write via `pread_write_pass`.

## Results

### Europe (32.4 GB, full-continent bbox)

| Strategy | Time | Anon peak | Planet-safe |
|----------|------|----------|-------------|
| simple | 350s | 2.7 GB | Yes |
| complete | ~385s | ~4 GB | Yes |
| smart | ~450s | ~5 GB | Yes |

### Japan (2.4 GB, Tokyo bbox)

| Strategy | Current | osmium | Ratio |
|----------|---------|--------|-------|
| simple | 18.1s | 7.2s | 2.5x |
| complete | ~16s | 11.0s | 1.5x |
| smart | ~18s | 13.4s | 1.3x |

### Improvement from all-sequential baseline

| Strategy | All-sequential | Current | Improvement |
|----------|---------------|---------|-------------|
| simple (Europe) | 362s | 350s | -3% |
| complete (Europe) | 553s | ~385s | -30% |
| smart (Europe) | 633s | ~450s | -29% |

## Remaining gap vs osmium

Simple extract: 2.5x at Japan. The gap is structural — osmium copies raw
protobuf group bytes for matching elements (zero-copy), pbfhogg fully decodes
and re-encodes via BlockBuilder. Closing this requires raw group passthrough
(different project from the pipeline/memory work).

Complete and smart are within 1.3-1.5x of osmium at Japan. The multi-pass
algorithm overhead is competitive.

## Infrastructure shipped

- **Inline string table entries** (wire.rs) — unified inline-only path, branchless
  get()/group(). Zero Box allocations per PrimitiveBlock.
- **Pool-recycled inline path** (blob.rs) — pipeline uses pooled inline constructor.
- **Per-worker DecompressPool** in pread_write_pass — zero alloc churn after warmup.
- **Shared pread_write_pass helper** — schedule building + worker dispatch + reorder.
  `pread_execute` for multi-phase use (no flush), `pread_write_pass` for single-use.
- **build_blob_schedule** with ElemKind tags — partition by element type for phased execution.
- **BlobBbox::contains** — blob-level full containment check.

## Experiments tried (for the record)

1. Sequential BlobReader — works, planet-safe, 30-118% slower
2. decode_ahead=4 — no help (retention is cumulative)
3. Inline entries (no pool) — eliminated Box retention, buffer retention remained
4. Inline entries + pool — 27 GB anon, barely survived simple, OOM'd complete
5. Hybrid pipelined simple/complete + sequential smart — fragile at 27 GB
6. Pread-from-workers write passes — 30% faster for complete/smart, planet-safe
7. 3-phase barrier pipeline for simple — marginal improvement, planet-safe
