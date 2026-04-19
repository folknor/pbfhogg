# `getid` include mode: pread-only header walk

## Summary

TODO.md's "Smaller items" entry claims `getid` include mode at planet can
drop from ~32 s to under 1 s by using "a header-only scan + pread of only
matching blobs". Measurement (2026-04-19) shows this requires a new I/O
primitive - the obvious `BlobReader` swap doesn't deliver because the
current path is 100 % kernel-I/O-bound, not user-CPU-bound, and the
`BufReader`-backed header walk reads the whole file anyway.

The fix is real but architecturally non-trivial. Documenting here so
future attempts don't re-walk the dead end.

## Measurement snapshot (2026-04-19)

Current planet include-mode baseline, UUID `5a44889d`, commit `aee7727`
(2026-04-18), dataset `planet-20260223-with-indexdata.osm.pbf`, IDs
specified as `n115722 n115723 n115724 w2080 w2081 w2082 ...` (small set):

```
GETID_SCAN_START   wall 43710 ms
                   user=0.0s  kern=0.2s
                   peak RSS 23 MB, peak anon 18 MB
                   disk read 88.2 GB, disk write 0
                   vol_cs 278,440  nonvol_cs 53  minflt 12,141  majflt 0
```

**Disk read equals the full file size.** User CPU ≈ 0. Process spends
its wall time blocked in `read()` syscalls waiting for the kernel to
pull the 88 GB in behind sequential readahead. Memcpy and allocation
overhead in `read_raw_frame` (which copies every blob body into a fresh
`Vec<u8>` before the filter decides to skip it) are invisible in the
profile because they run during kernel-wait slack.

## Why the obvious fix doesn't work

The natural idea is to replace the `read_raw_frame` loop in
`filter_by_id` (`src/commands/getid/mod.rs`) with the
`BlobReader::next_header_with_data_offset` pattern that multi-extract
uses (`src/commands/extract/multi.rs`): walk blob headers cheaply, pread
only the blobs whose indexdata ID range matches.

This would eliminate:
- The per-blob `Vec::with_capacity(frame_len)` allocation (~1.4 M allocs
  at planet).
- The 64 KB memcpy from `BufReader` into the frame `Vec` for every
  skipped blob (~90 GB of memcpy work at planet).

But both savings land in user-CPU time, which is already ~0 in the
profile. The kernel is still reading 88 GB from disk either way:
`BufReader::seek_relative` on a read+skip pattern does not stop
sequential readahead - the kernel prefetches based on the fd's access
history, and `posix_fadvise(SEQUENTIAL)` is set unconditionally in
`FileReader::buffered` (`src/read/file_reader.rs:37`).

Expected impact of the naive swap: **close to zero wall-time change at
planet**. Worth confirming once, but not a win on its own.

## What "under 1 s" actually requires

To hit the TODO's aspirational target, the I/O path itself has to
change. With a small ID set, only a handful of blobs actually match
(order 3-9 on planet for typical debug IDs), so the in-principle
minimum work is:

1. Walk the file's 1.4 M blob headers. Each header is ~100 B including
   the 4 B length prefix. Total ≈ 140 MB.
2. Pread the matching blobs' data bodies. 3-9 blobs × ~64 KB ≈ 192-576 KB.

At a ~2 GB/s disk throughput ceiling (observed under the current sequential
readahead path), 140 MB / 2 GB/s ≈ 70 ms. Plus the matching-blob preads,
plus decompress+filter on a handful of blobs. Well under 1 s total is
plausible - but only if the kernel does not pull the 63.9 KB of blob
body after every header.

Current kernel behavior defeats this: `posix_fadvise(SEQUENTIAL)` plus
the 256 KB `BufReader` capacity means each buffered read of a header
also pulls several following blob bodies into the page cache. The
application "skips" them but the disk bytes were already read.

**Real fix requires all of:**

- Open the input fd with `posix_fadvise(RANDOM)` (or a mode without
  `SEQUENTIAL` readahead) so the kernel does not prefetch blob bodies.
- Walk headers via explicit `pread` at known offsets rather than through
  a `BufReader`. A tiny pread-only helper that reads the 4 B length
  prefix, then the header bytes, then jumps by `header.datasize`, could
  live next to `BlobReader` but would not inherit its buffered shape.
- Preserve indexdata parsing (`WireBlobHeader::parse` with
  `parse_indexdata=true`) so the ID-range filter still works.
- For matches, pread the blob data body. Same primitive as multi-extract's
  worker loop - already wired into `FileReader`-less `File::read_exact_at`.

`O_DIRECT` is a sufficient alternative to `fadvise(RANDOM)` but imposes
page-aligned I/O, which is awkward for reading arbitrary header byte
ranges. `fadvise(RANDOM)` + normal preads is the less invasive choice.

Scope estimate: ~2-3 commits. First a new pread-only header walk
primitive (probably in `src/read/` as a sibling to `BlobReader`). Then
swap `filter_by_id`'s include path to use it. Then similar swaps could
apply to other commands with the same "walk + selectively decode" shape
(some are already on pread workers, so this primarily helps the
single-threaded commands).

## Secondary finding: 32.5 s → 43.7 s regression

The same include-mode workload regressed ~35 % between 2026-03-29 and
2026-04-18:

| UUID       | Commit  | Wall    | Date        |
|------------|---------|---------|-------------|
| `79100694` | `8ffee59` | 32.5 s | 2026-03-29 |
| `5a44889d` | `aee7727` | 43.7 s | 2026-04-18 |

Same host, same dataset, same IDs. The TODO.md entry cites the 32.5 s
figure; the current baseline is 43.7 s. This is orthogonal to the
pread-walker work but worth bisecting before or alongside any include-
path rework - the regression may already fix part of the gap for free,
or it may reveal that the pread-walker win needs to land against a
repaired baseline.

Quick-win audit hypothesis: one of the read-path restructure commits
between `8ffee59` and `aee7727` (source-tree moves, blob wire-format
split, BlobReader changes) disabled a fast path. Bisect with
`brokkr getid --dataset planet --bench 1 --commit <hash>` over the
~25 commits in that range.

## Decision

Not building this today. The actual payoff requires a new I/O
primitive, not a quick call-site swap, so it belongs in Milestone 2
(write-path / read-path throughput) rather than the "Smaller items"
list where the current TODO.md entry sits. Document the findings here
so future attempts start from the right premise.

If someone picks this up:

1. Bisect the 32.5 → 43.7 s regression first. Might shrink the gap
   for free.
2. Design the pread-only header walker as a small primitive with its
   own measurement (benchmark header walk on planet, confirm ~140 MB
   read not 88 GB).
3. Then swap `filter_by_id`'s include path. Measure the full win
   end-to-end. Likely extends to other single-threaded command paths.
