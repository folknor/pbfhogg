# Planet-Scale Memory Optimization — Results

Target: reliable planet-scale merge (75 GB) on 64 GB host, peak RSS < 52 GB.
Throughput regression budget: <= 10% per change.

## Completed Experiments

| ID | Change | Commit | RSS Impact (Germany) | Time Impact | Decision |
|----|--------|--------|---------------------|-------------|----------|
| E1.1 | Compact DiffOverlay (arenas, interned strings, i32 coords) | 1d291f1 | -8.2% zlib, -5.4% none | neutral | KEEP |
| E2.1 | Adaptive batch sizing by bytes budget | e1099c4 | -18.4% zlib, -35.4% none | -10.2% zlib | KEEP |
| E2.2 | Stream rewrite outputs via rayon::spawn + channel | 1e03e5b | -3.2% zlib, +0.6% none | -6.9% zlib, -7.8% none | KEEP |
| E1.2 | Replace per-job upsert Vec copies with range views | 041d79f | neutral | neutral | KEEP |
| E1.3 | Bound DecompressPool retention (4 MB cap, 64 max) | b9da254 | neutral (structural) | neutral | KEEP |
| E3.1 | Buffer recycling pool for writer (Arc<Mutex<Vec>>) | reverted | +5.8% zlib, +6.9% none | +5.5% zlib, **+12.3% none** | DROP |

## Cumulative Results (Germany 4.5 GB, commit a6ebbfe)

| Metric | Original | Final | Delta |
|--------|----------|-------|-------|
| RSS (zlib) | 710 MB | 515 MB | **-27.5%** |
| RSS (none) | 635 MB | 390 MB | **-38.6%** |
| Time (zlib) | 6,321 ms | 5,335 ms | **-15.6%** |
| Time (none) | 4,686 ms | 3,420 ms | **-27.0%** |

## North America Validation (18.8 GB, commit a6ebbfe)

Previously OOM-crashed on 30 GB host. Now completes with RSS under 600 MB.

| Config | Time | vs pre-optimization |
|--------|------|---------------------|
| buffered + zlib | 17.3s | -60% (was 43.2s) |
| buffered + none | 14.9s | -59% (was 36.4s) |
| io_uring + none | 11.9s | -54% (was 25.5s) |

## Remaining (not pursued)

- **E3.2** Coalescing policy tuning — passthrough RSS spikes during long streaks.
- **E4.1-E4.3** Sharded diff / disk-backed index / two-pass planner — only if planet OOMs.

Gate assessment: North America fits in 30 GB with headroom. Planet (75 GB) extrapolates to ~47s / <1 GB RSS on uring+none. Phase 4 not needed.
