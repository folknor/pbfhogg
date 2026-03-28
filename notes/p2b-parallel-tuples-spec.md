# P2b: Parallel tuples for external join stage 2

## Goal

Parallelize zlib decompression in external join stage 2 (node join).
Previously single-threaded at 301s Europe, zlib-bound. The merge-join
consumer is inherently sequential (global cursor through sorted bucket
pairs), so only decompression + wire parsing can be parallelized.

## Results

### P2b-v2 (commit `80e227b`, plantasjen, sidecar `070086bb`)

**Europe (32.4 GB): 866s (14.4 min), down from 901s (-4%).**

| Stage | Baseline (901s) | P2b-v2 (866s) | Anon peak | Delta |
|-------|----------------|--------------|----------|-------|
| Stage 1 | 82s | 126s | 70 MB | +54% (sequential reader) |
| Stage 2 | 301s | **216s** | **1.4 GB** | **-28%** |
| Stage 3 | 73s | 91s | — | +25% |
| Stage 4 | 392s | 432s | 2.1 GB | +10% |

Denmark (465 MB): 13.8s, 0 missing locations.

Stage 2 anon 1.4 GB is the bucket sort data — irreducible minimum for
this algorithm. Planet extrapolation: ~3.9 GB. Safe on 32 GB host.

### P2b-v1 (commit `3051dd7`, superseded)

Europe: 836s. Stage 2: 215s / **20.4 GB anon**. OOM risk at planet
scale (~56 GB extrapolated). Fixed by v2's pread-from-workers.

## Root cause: cross-thread Blob data + non-affine pool

Two sources of cross-thread alloc/free in the current P2b implementation:

1. **Compressed Blob data**: IO thread allocates `Vec<u8>` (~32 KB per
   blob), wraps in `Bytes`, sends through channel. Rayon workers free it
   after decompression. 500K blobs × 32 KB = ~16 GB cross-thread churn.
   Same retention pattern as PrimitiveBlock.

2. **Global tuple pool**: `Arc<Mutex<Vec<Vec<NodeTuple>>>>` doesn't
   preserve worker affinity. Consumer returns buffers, any worker pops
   them — still cross-thread free from the allocator's perspective.

Both identified by perf-Claude and perf-Codex reviews.

## Architecture (current, commit `3051dd7`)

```
IO thread: BlobReader reads raw compressed blobs
  │ skip non-OsmData, skip non-node via indexdata
  │ fadvise(DONTNEED) behind read head
  │ assign sequence numbers to node blobs
  ▼
sync_channel(16) — (usize, Blob) objects  ← PROBLEM: Blob crosses threads
  │
  ▼
Dispatch thread: receives raw blobs, spawns rayon tasks
  │
  ▼
Rayon workers (via rayon::spawn + thread_local!):
  │ thread-local: decompress_buf: Vec<u8> (reused)
  │ per-block: tuples from Arc<Mutex> pool  ← PROBLEM: non-affine pool
  │ blob.decompress_into(&mut decompress_buf)
  │ extract_node_tuples(&decompress_buf, &mut tuples)
  │ send (seq, Ok(tuples)) through channel
  ▼
sync_channel(32) — (usize, Result<Vec<NodeTuple>>)
  │
  ▼
Consumer: ReorderBuffer → merge-join → return tuples to pool
```

## Next: pread-from-workers (P2b-v2)

Reviewed by perf-Claude, perf-Codex, planet-Claude, planet-Codex. All
agree this is the right fix.

### Architecture

```
IO thread: BlobReader reads only headers (~50 bytes)
  │ skip non-OsmData, skip non-node via indexdata
  │ NO fadvise (workers handle their own pages)
  │ send (seq, blob_data_offset, datasize) through channel
  ▼
sync_channel(16) — (usize, u64, usize) lightweight descriptors
  │
  ▼
Workers (fixed threads, not rayon):
  │ shared: Arc<File> for pread (FileExt::read_at)
  │ thread-local: read_buf: Vec<u8> (~32 KB, reused)
  │ thread-local: decompress_buf: Vec<u8> (~2 MB, reused)
  │ thread-local: tuples: Vec<NodeTuple> (~128 KB, reused)
  │
  │ read_at(&mut read_buf, offset)  — all alloc/free thread-local
  │ parse WireBlob from read_buf
  │ decompress into decompress_buf
  │ extract_node_tuples into tuples
  │ fadvise(DONTNEED, offset, size)  — worker evicts own pages
  │ send (seq, tuples) through channel  — tuples Vec ownership transfer
  ▼
sync_channel(32) — (usize, Result<Vec<NodeTuple>>)
  │
  ▼
Consumer: ReorderBuffer → merge-join → drop tuples (same-thread as send)
```

### Key changes from current P2b

1. **No Blob objects cross threads.** IO thread sends lightweight
   descriptors (offset + size). Workers read their own data via pread.
   Zero cross-thread alloc/free for compressed data.

2. **Worker-local tuple Vecs.** Each worker reuses its own tuples Vec
   (clear between blocks). No shared pool needed. The Vec does cross
   to the consumer for merge-join, but with only ~32 Vecs in flight
   total (channel capacity), this is bounded.

3. **Worker-side fadvise.** Each worker calls DONTNEED after its own
   pread. IO thread does NOT call fadvise (would race with workers).
   Per planet reviewers: track completion watermark if needed, but
   per-blob DONTNEED from workers is the simplest safe approach.

4. **Fixed worker threads, not rayon.** Per perf-Codex: explicit
   `std::thread::scope` workers give cleaner buffer ownership than
   rayon::spawn with thread_local!. 4-6 workers.

5. **Bound in-flight by bytes, not just count.** Per planet-Codex:
   cap compressed bytes in flight to tens of MB to prevent page cache
   speculation.

### Prerequisites

Need a BlobReader method that returns blob data offset + datasize
without reading the blob body. Current `next_header_skip_blob` returns
the header but doesn't expose the data offset. Options:
- Add `next_header_with_data_offset() -> (BlobHeader, u64, usize)`
- Or compute from `offset + 4 + header_proto_size`

### Validation plan

1. Denmark correctness (diff against current output)
2. Denmark sidecar: anon stays < 100 MB throughout stage 2
3. Europe sidecar: anon stays < 1 GB in stage 2
4. Europe timing: stage 2 < 200s (same or better than current 215s)
5. Planet sidecar: anon stays bounded on 32 GB host

### Planet-scale concerns (from planet reviewers)

- Concurrent pread on shared fd is safe (pread is atomic, no seek)
- NVMe handles 4-6 concurrent 32 KB reads efficiently (deep hw queues)
- Kernel readahead may weaken with multiple streams on same fd —
  monitor majflt in sidecar, increase read_ahead_kb if needed
- Do NOT use O_DIRECT for this pattern (small reads, CPU-bound)
- NUMA: only matters on multi-socket, use numactl --interleave if needed

## History

- **901s** — sequential node-only scanner (commit `ee9b19f`)
- **836s** — P2b-v1 with IO-thread Blob transfer + global pool
  (commit `3051dd7`). Stage 2: 215s (-29%). But 20.4 GB anon from
  cross-thread retention. Not planet-safe.
- **866s** — P2b-v2 with worker-side pread (commit `80e227b`).
  Stage 2: 216s (-28%), **1.4 GB anon** (was 20.4 GB). Planet-safe.
