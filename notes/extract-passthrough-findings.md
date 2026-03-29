# Extract performance: final state

## Current architecture (commit `b95e5ab`)

Three strategies, each planet-safe with bounded memory:

**Simple (3-phase barrier pipeline with parallel classify + raw frame passthrough):**
Each phase classifies blobs in parallel, then writes matching raw frames via
pread workers. No decode+re-encode — matching blobs are written as raw bytes.
- Phase 1: parallel node blob classification (bbox test) + pread write
- Phase 2: parallel way blob classification (ref scan against frozen bbox_node_ids) + pread write
- Phase 3: sequential relation classify + pread write
- Beats osmium: 4.4s vs 7.2s at Japan (1.6x faster)

**Complete (two-pass):** Pass 1 classification via `collect_pass1_generic`
(sequential BlobReader). Pass 2 write via `pread_write_pass` (workers own
full PrimitiveBlock lifecycle).

**Smart (three-pass):** Pass 1 same as complete. Pass 2 way closure
(sequential, mutates extra ID sets). Pass 3 write via `pread_write_pass`.

## Results

### Europe (32.4 GB, full-continent bbox, commit `b95e5ab`)

| Strategy | Time | Anon peak | Planet-safe |
|----------|------|----------|-------------|
| simple | **100s** | 2.7 GB | Yes |
| complete | ~390s | ~4 GB | Yes |
| smart | ~460s | ~5 GB | Yes |

Simple phase breakdown (Europe):

| Phase | Classify | Write | Total |
|-------|----------|-------|-------|
| Nodes | 13s | 11s | 24s |
| Ways | 6s | 40s | 46s |
| Relations | 13s | 2s | 15s |

Way write dominates (40s / 40% of total) — raw frame I/O for the largest
element type. Node and relation classify are 13s each. Relation write is
trivial (2s) because few relations match a bbox extract.

### Japan (2.4 GB, Tokyo bbox, commit `b95e5ab`)

| Strategy | pbfhogg | osmium | Ratio |
|----------|---------|--------|-------|
| simple | **4.4s** | 7.2s | **1.6x faster** |
| complete | ~13s | 11.0s | 1.2x slower |
| smart | ~15s | 13.4s | 1.1x slower |

### Improvement arc

| Strategy | All-sequential | Pread (prev) | Parallel classify (current) |
|----------|---------------|--------------|----------------------------|
| simple (Europe) | 362s | 350s | **100s** (-72%) |
| complete (Europe) | 553s | ~385s | ~390s (unchanged) |
| smart (Europe) | 633s | ~450s | ~460s (unchanged) |

The parallel classify + raw frame passthrough architecture delivered a 3.5x
speedup for simple extract. Complete and smart are unchanged — they require
full PrimitiveBlock decode for element-level filtering and re-encoding.

## How simple extract beats osmium

The key insight: for simple bbox extract on sorted PBFs with indexdata, the
classification decision (include/exclude) can be made at the blob level
without decoding elements. Node blobs have bbox metadata in indexdata; way
blobs need only a lightweight ref scan (no string table, no tags, no metadata
decode). Matching blobs are written as raw compressed frames — zero
decompression, zero re-compression. osmium must decompress every blob to
inspect individual elements, then re-compress matching groups.

## Remaining opportunities

- **Relation classify parallelization:** 13s at Europe (13% of simple total).
  Could parallelize but marginal return.
- **Raw group passthrough for other commands:** cat --type, tags-filter, getid,
  renumber, time-filter still fully decode+re-encode. Extending the raw frame
  approach would help, but these commands need element-level filtering within
  groups (partial matches), making blob-level passthrough less applicable.
- **Complete/smart write path:** Still decode+re-encode via BlockBuilder.
  Raw group passthrough would help for groups where all elements are selected.

## Infrastructure shipped

- **Inline string table entries** (wire.rs) — unified inline-only path, branchless
  get()/group(). Zero Box allocations per PrimitiveBlock.
- **Pool-recycled inline path** (blob.rs) — pipeline uses pooled inline constructor.
- **Per-worker DecompressPool** in pread_write_pass — zero alloc churn after warmup.
- **Shared pread_write_pass helper** — schedule building + worker dispatch + reorder.
  `pread_execute` for multi-phase use (no flush), `pread_write_pass` for single-use.
- **build_blob_schedule** with ElemKind tags — partition by element type for phased execution.
- **BlobBbox::contains** — blob-level full containment check.
- **Parallel blob classification** — rayon par_iter over blob schedule with
  lightweight scanners (node bbox test, way ref scan).

## Experiments tried (for the record)

1. Sequential BlobReader — works, planet-safe, 30-118% slower
2. decode_ahead=4 — no help (retention is cumulative)
3. Inline entries (no pool) — eliminated Box retention, buffer retention remained
4. Inline entries + pool — 27 GB anon, barely survived simple, OOM'd complete
5. Hybrid pipelined simple/complete + sequential smart — fragile at 27 GB
6. Pread-from-workers write passes — 30% faster for complete/smart, planet-safe
7. 3-phase barrier pipeline for simple — marginal improvement over sequential, planet-safe
8. Parallel classify + raw frame passthrough — 3.5x speedup for simple, beats osmium
