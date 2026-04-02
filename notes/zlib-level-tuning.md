# Zlib compression level tuning

## Current state

Default: `Compression::Zlib(6)` — matches osmium's `Z_DEFAULT_COMPRESSION`.
User-configurable via `--compression zlib:N` (0-9). Backend: `zlib-rs`
(pure Rust, faster than zlib-ng for decompression, 15-19% slower for
sync compression — but pipelined mode is decode-bound so no difference).

## The question

For pipelined write, zlib compression runs on rayon workers in parallel.
The writer thread is I/O-bound (sequential writes). The pipeline stalls
when compression is slower than I/O — which happens at higher zlib levels.

At what level does compression become the bottleneck? What's the
compression ratio vs throughput tradeoff?

## Expected behavior by level

From zlib documentation and general knowledge:

| Level | Strategy | Speed | Ratio |
|-------|----------|-------|-------|
| 0 | store (no compression) | fastest | 1.0x |
| 1 | fast (greedy match) | ~3-5x slower than 0 | ~3-4x |
| 2-3 | fast+ (short matches) | ~10-20% slower than 1 | ~5-10% better |
| 4-5 | balanced | ~2x slower than 1 | ~10-15% better |
| 6 | default | ~3x slower than 1 | ~15-20% better |
| 7-8 | thorough | ~5-8x slower than 1 | ~2-5% better |
| 9 | maximum | ~10x slower than 1 | ~1-2% better |

The jump from 6 to 9 is ~3x slower for ~2% better ratio. The jump
from 1 to 6 is ~3x slower for ~20% better ratio. Level 1-3 is the
sweet spot for throughput when output will be re-read (pipeline internal).

## When to use what

**Level 6 (default):** archival PBFs, distribution, long-lived files.
Matches osmium output. Good interop.

**Level 1-3:** pipeline internal PBFs that will be immediately re-read:
- `cat` output → `add-locations-to-ways` input
- `extract` output → `build-geocode-index` input
- `merge` (apply-changes) output → next merge input

At planet scale (87 GB), level 6 → level 1 could save 30-60% of the
compression wall time. For a 4m27s pipelined write (North America),
if compression is ~50% of worker time, level 1 could save ~1-2 minutes.

**Level 0 (none):** internal pipeline where disk space is not a
constraint. The PBF is ~3-4x larger but write is I/O-limited.

## Research needed

1. **Benchmark levels 1, 3, 6, 9** on Denmark/Japan with `brokkr`:
   `brokkr cat --dataset denmark --compression zlib:1 --bench`
   Compare wall time, output size, compression CPU time.

2. **Measure rayon worker utilization** at each level — are workers
   idle (I/O-bound) or saturated (CPU-bound)?

3. **Consider adaptive level:** start at level 6, if worker queue
   backs up (>N items in flight), drop to level 1. Self-tuning
   for the current hardware.

4. **Consider per-command defaults:** `merge` always produces a
   distribution PBF → level 6. `cat` (internal pipeline) → level 1.
   Let the user override with `--compression`.

## Auto-compression

The `--compression` flag already supports `zlib:N`. The research
question is whether the default should change for pipeline-internal
commands, or whether we should add `--compression fast` as a named
preset for `zlib:1`.

## zstd comparison

zstd level 3 decompresses 3-5x faster than zlib at equivalent ratios.
For internal pipelines where both writer and reader are pbfhogg, zstd
is strictly better than zlib. The compatibility warning only matters
for files read by third-party tools.

A strong argument for `--compression zstd` as the default for internal
pipeline commands, with `zlib:6` only for final distribution output.
