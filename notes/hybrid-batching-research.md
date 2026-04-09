# Hybrid batching for pread workers

## Problem statement

`parallel_classify_phase` and the tags-filter pass 2 write path use
a shared work queue (`Mutex<Receiver>`) for blob dispatch. Each worker
locks the mutex, receives ONE descriptor, releases the lock, then
processes the blob (pread + decompress + classify/write). This
lock/unlock happens once per blob per worker.

Flagged by 4/6 reviewers (2026-03-29 sweep). The TODO claims ~8s
regression from pipelined reader to pread workers conversion.

## Current architecture

```
Dispatcher thread              Workers (N threads)
    |                              |
    for &item in schedule {        loop {
        desc_tx.send(item)            let d = rx.lock().recv()  // 1 mutex per blob
    }                                 pread(d)
                                      decompress(d)
                                      classify/write(d)
                                      result_tx.send(r)
                                  }
```

Contention points:
1. `rx.lock()` — all N workers compete for the same mutex
2. `guard.recv()` — channel receive under lock
3. `result_tx.send()` — bounded channel, can block if consumer is slow

## Measured baselines (Europe, 35 GB, commit `75ad21d`, plantasjen)

tags-filter-twopass: 105s total
- Pass 1 (classify): 34s — `parallel_classify_phase`
- Gap (closure + deps): 33s
- Pass 2 (write): 37s — inline pread worker loop

Pre-pread (commit `1e6e70c`): 366s (sequential BlobReader).
The pread conversion was a 3.4x improvement. The "~8s regression"
claim needs clarification — it may be relative to a theoretical
optimal, not a measured baseline.

## What hybrid batching means

Instead of one lock per blob, workers drain N items at once:

```rust
loop {
    let batch: Vec<_> = {
        let guard = rx.lock()...;
        let mut batch = Vec::with_capacity(BATCH_SIZE);
        for _ in 0..BATCH_SIZE {
            match guard.recv() {
                Ok(d) => batch.push(d),
                Err(_) => break,
            }
        }
        batch
    };
    if batch.is_empty() { break; }
    for item in batch {
        // pread + decompress + classify
    }
}
```

This reduces mutex acquisitions from `blobs/worker` to
`blobs/worker/BATCH_SIZE`. At BATCH_SIZE=16 with 500K blobs and
6 workers, that's ~5200 lock ops instead of ~83000.

## Analysis: is mutex contention the bottleneck?

With 6 workers and ~400µs per blob (pread + decompress + classify):
- Workers complete ~2500 blobs/second collectively
- Mutex lock/unlock: ~100ns on Linux (uncontended futex)
- Lock contention rate: 6 threads × 100ns / 400µs ≈ 0.15% of time

At Europe scale (500K blobs):
- Total mutex time: 500K × 100ns = 50ms
- Even with 10x contention overhead: 500ms

This suggests mutex contention is NOT the 8s regression. The 8s is
more likely from:
1. Per-blob channel send/recv overhead (sync_channel bookkeeping)
2. Per-blob Vec allocation for results (now addressed by scratch reuse)
3. Consumer throughput (ReorderBuffer + writer)

## Recommended approach

1. Measure first: run hotpath on tags-filter pass 2 to see where
   the 37s actually goes. If decode+write is 35s and overhead is 2s,
   batching can recover at most 2s — not 8s.

2. If measurement shows overhead > 5s: implement batch drain.
   BATCH_SIZE = 8-16 (tuned by blob count / decode_threads / 4).
   The result channel stays per-blob (consumer needs ordering via
   ReorderBuffer).

3. If measurement shows overhead < 2s: close the TODO item as
   "not worth the complexity" and document the finding.

## Scope

Applies to:
- `parallel_classify_phase` (mod.rs) — classify paths
- `parallel_classify_accumulate` (mod.rs) — accumulate paths
- Tags-filter pass 2 write workers (tags_filter.rs)
- Multi-extract write workers (extract.rs) — `multi_extract_pread_write`

All share the same Mutex<Receiver> dispatch pattern.

## Questions for reviewers

1. Is the ~8s claim from the original reviewer findings substantiated?
   The Europe bench shows 366s → 105s for the full conversion. Where
   does the 8s come from?
2. Should we profile pass 2 specifically, or is the 37s already
   well-understood from existing hotpath data?
3. Is there a simpler approach than batch drain — e.g., crossbeam
   channel (lock-free) instead of Mutex<mpsc::Receiver>?
