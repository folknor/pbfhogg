# glibc's mmap threshold: a per-command policy for the CLI

Transient note. The measured cells, the mechanism and the CLI-wide verdict
are durable and live in reference/performance/memory.md, "glibc's sliding mmap
threshold across commands", and in the `set_mmap_threshold` doc in
`src/process.rs`. This note holds what is not settled.

## The problem

glibc raises its mmap threshold at the first free of a blob-scale mapped
buffer, after which such buffers come from the arenas and their freed
interior memory stays resident. Every multi-phase command pays it, and not
only between phases. At planet a threshold pinned at 128 KiB cut peak anon
by 4 to 41 %, at 1 to 5 % wall on the commands that allocate a blob-scale
buffer per blob. A CLI-wide fixed threshold was measured at 128 KiB, 1 MiB
and 4 MiB and none passes both the wall guard and the no-memory-regression
bar on every command, so the CLI sets no policy.

## Idea: choose the threshold per command

The CLI-wide verdict needs one value that wins everywhere; a per-command
policy does not. Read off the bygg planet cells at `3c5b8d3a` (2026-11-04),
the candidates would be:

| Pin | Commands | Peak anon saved | Wall |
|---|---|---|---|
| 128 KiB | repack, degrade `--strip-locations` | -29 % | +2.0 %, +0.1 % |
| 128 KiB | extract `--simple` | -18 % | +1.6 % |
| 128 KiB | tags-filter two-pass and with the relation closure | -11 to -12 % | noise |
| 128 KiB | check `--refs`, time-filter | -9 % | noise |
| 128 KiB | check `--ids --full` | -7 % | +2.1 % |
| 4 MiB | renumber | -30 % | neutral |
| 4 MiB | getid `--add-referenced` | -15 % | neutral |
| 4 MiB | apply-changes | -9 % | neutral (medians over six cells each) |
| 4 MiB | extract `--complete` | -5 % | +1.2 % |
| none | cat `--clean` (4 MiB raises its peak 12 %, 128 KiB costs 2.7 % wall), build-geocode-index (-3 % only), commands holding no blob-scale buffers across a pool | | |

Alternatives inside the table: 128 KiB for apply-changes (-26 %, about
+3 % wall) and renumber (-29 %, +3.1 %).

Why this is not ready:

- Planet only, one host. The saving and the wall cost both scale with blob
  count, worker count and the input's blob sizes; regional extracts
  (denmark to europe), other hosts (plantasjen's 24 threads against bygg's
  32) and repacked inputs (the 8k snapshot, packed denmark snapshots) are
  unmeasured.
- Most 128 KiB rows are single cells, and every wall delta in the table sits
  inside the 2-3 % noise floor, which reads either way.
- A command's threshold has to hold for all its modes and flags, not only
  the one benched (tags-filter `-R` and `-i`, extract strategies sharing
  code, apply-changes `-l`, cat's other modes).
- `src/process.rs` rules that no fixed threshold is re-measured until the
  per-blob allocate-and-free of blob-scale buffers is gone, the blob
  reader's compressed frames first. That ruling was written against a
  CLI-wide value; whether it binds a per-command one is the maintainer's
  call.

## Shape, if it lands

The CLI's dispatch calls `process::set_mmap_threshold` with the command's
value before any work, and skips it when the user has set an allocator
policy (any `MALLOC_*` variable or a `glibc.malloc.*` entry in
`GLIBC_TUNABLES`), as the `set_mmap_threshold` doc already requires. The
library sets nothing (ADR-0016). brokkr does not record `GLIBC_TUNABLES`,
so measurement arms on tunables run unstored with the tree dirty.

## Structural route

Independent of any threshold: remove the per-blob allocate-and-free of
blob-scale buffers, so a low threshold stops costing wall.

- `BlobReader::next_frame` (`src/read/blob.rs`) allocates every compressed
  frame (about 1.8 MB at planet) fresh; it is the 1 MiB arm's getid cost.
  Recycling has to follow the frame's last `Bytes` slice, which raw
  payloads carry past decompression (`PooledBuffer` in
  `src/read/decompress.rs` is the ownership precedent).
- extract `--complete`'s way classify and write phases (minor faults up
  twentyfold under a 128 KiB threshold) churn somewhere unattributed.

Removing a site removes one source of the wall cost; it does not by itself
promise a low threshold becomes free. Bounded pooled storage retains idle
memory, so it has to be weighed against the saving.
