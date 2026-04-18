# BlobReader::seek_raw audit (2026-04-17)

> **RESOLVED 2026-04-18 (commit `aa3147c`).** Fix shape did not match the
> audit's "specialize `impl BlobReader<BufReader<R>>`" recommendation —
> Rust doesn't allow inherent method specialization on stable. Landed
> shape is a public `BlobReaderSource` trait with a default
> `skip_relative` overridden by the `BufReader` impl to call
> `BufReader::seek_relative`. See
> [`notes/seek-raw-fix-implementation.md`](seek-raw-fix-implementation.md)
> for the implementation, design rationale, and per-caller bench deltas.

Source-of-truth for the cross-cutting `BlobReader::seek_raw` BufReader-discard
fix. Three Explore agents produced parallel findings; this doc consolidates
the fix-shape recommendation, bench coverage, and the minimum-cost validation
plan. The instrumentation audit is excluded from this doc — its findings were
folded directly into the codebase in the same commit as the doc landed.

## The bug

`BlobReader::seek_raw` in `src/read/blob.rs` calls `self.reader.seek(pos)`,
which for `BufReader<File>` always calls `discard_buffer()` after the seek
(stdlib `Seek::seek` semantics). `BufReader::seek_relative` preserves the
buffer when the target is in-range, but it's BufReader-specific — not on the
`Seek` trait, so the generic `impl<R: Seek> BlobReader<R>` can't reach it.

Every caller that walks PBF blob headers pays proportional cost:

- ~10× file-size amplification at the default 256 KB buffer (~350 GB of
  reads for Europe's 35 GB, ~15 s wall, page-cache-served).
- ~670× amplification at 16 MB buffer (measured 14.8 → 426 s Europe
  regression when the buffer was bumped without fixing the seek; reverted
  in commit `86761d6`). The current 256 KB buffer masks the bug rather
  than avoiding it.

The multi-extract instrumentation landed earlier this session (commit
`1e8d37b`) made this concrete: `MULTI_SCHEDULE_SCAN` shows 26 s out of
800 s wall on Europe — a previously-invisible phase, and it's the same
header-walk work as every other caller does.

## Callers

Nine callers total, up from the eight tracked in TODO.md (the ninth —
`IndexedReader::create_index` — is a library-API caller; bundled here for
completeness but the production-pipeline impact is negligible).

| # | Caller | Site | Command(s) |
|---|--------|------|------------|
| 1 | `build_classify_schedule` | `src/commands/mod.rs` | extract simple (fallback), various |
| 2 | `build_classify_schedules_split` | `src/commands/mod.rs` | check-refs, check-ids, extract (PASS1), tags-filter |
| 3 | `tags_filter_single_pass` header scan | `src/commands/tags_filter.rs` | tags-filter `-R` |
| 4 | `tags_filter_two_pass` header scan | `src/commands/tags_filter.rs` | tags-filter (default) |
| 5 | `extract_simple` single-pass | `src/commands/extract.rs` ~2007 | extract --simple |
| 6 | `extract_complete_ways` schedule build | `src/commands/extract.rs` ~1515 | extract --complete |
| 7 | `extract_smart` schedule build | `src/commands/extract.rs` ~2616 | extract --smart |
| 8 | `try_extract_multi_single_pass` | `src/commands/extract.rs` ~695 | multi-extract |
| 9 | `scan_blob_metadata` | `src/commands/altw/blob_meta.rs` ~31 | add-locations-to-ways external |
| 10 | `build_all_blob_schedules` | `src/commands/renumber_external.rs` ~560 | renumber |
| — | `IndexedReader::create_index` | `src/read/indexed.rs` ~126 | library API (low priority) |

All callers use the same shape: `next_header` (4-byte len) →
`read_exact(header)` → `seek_raw(SeekFrom::Current(+data_size))` to skip
the body. Reader type in every case is `BufReader<File>` opened via
`seekable_from_path`. No absolute-seek or backward-seek callers exist in
the codebase.

## Fix shape: specialize `impl BlobReader<BufReader<R>>`

Three fix shapes were evaluated (see TODO.md Performance section for
original sketches):

| Option | Works for all callers? | Call-site changes | Lines | Future-proof | Verdict |
|--------|------------------------|-------------------|-------|--------------|---------|
| **1. Specialize `impl<R: Read + Seek> BlobReader<BufReader<R>>`** | ✓ all use BufReader | ✗ zero | 15-25 | ⚠ BufReader-only | **Land this** |
| 2. Add `SeekRelative` trait with `BufReader` + `File` impls | ✓ | ✗ zero | 40-60 | ✓ | Future-proof, not needed yet |
| 3. Open-code in-buffer cursor bump inside `seek_raw` | ✓ | ✗ zero | 20-30 | ✗ duplicates stdlib | Avoid |

**Verified generic-bounds compatibility:** `src/read/blob.rs` already has
a specialized `impl BlobReader<BufReader<File>>` block (containing
`seekable_from_path`). A second specialized impl `impl<R: Read + Seek>
BlobReader<BufReader<R>>` can coexist with the generic
`impl<R: Read + Seek + Send>` — Rust's coherence rules allow it because
the specialization is strictly more specific.

**Commit shape:** one new impl block with an override of `seek_raw` that
calls `self.reader.seek_relative(offset)` for `SeekFrom::Current(offset)`
and falls back to `Seek::seek` for other seek targets (absolute or too
far to fit in the buffer). Zero call-site changes.

Option 2 becomes the right move the day a non-`BufReader` seekable reader
is introduced; until then it's speculative abstraction.

## Bench coverage

Every caller-command pair has at least one recent bench baseline. Full
UUIDs with commit hashes (as of 2026-04-17, plantasjen):

| Command | Japan | Europe | Planet | Published baseline |
|---------|-------|--------|--------|--------------------|
| check-refs | `4a347e3b` 2.1 s | `70ff6c5d` 33.6 s | `862547e4` 72.5 s | README:45, perf.md:296-337 |
| check-ids --full | — | `31ca231d` 52.7 s [TAINTED] | `2f52252d` 93.2 s [TAINTED] | perf.md:339-379 |
| tags-filter | `0b2db566` 4.9 s | `9562d82b` 112.2 s | `d71445a6` 153.2 s [TAINTED] | README:43, perf.md:726-741 |
| extract --smart | `397da7c1` 4.7 s | `48ca6bbb` 181.4 s | `2d028196` 279 s | README:49, perf.md:590-657 |
| multi-extract | `08fefe51` 7.7 s | `c1ff6ec9` 799.9 s | `1cd62e90` 965 s | perf.md:659-725 |
| add-locations-to-ways external | `1d4913fc` 39.7 s | `85464a37` 293.1 s [TAINTED] | `123f70f1` 698.1 s [TAINTED] | README:51, perf.md:126-287 |
| renumber | `2ee186c7` 6.1 s | `873dfdfe` 78.4 s | `f9098cab` 194.2 s | README:48, perf.md:489-588 |

## Blast-radius expectations

Ordered by likely observable wall-time win from the seek_raw fix:

| Caller / Command | Expected win | Rationale |
|------------------|-------------:|-----------|
| `scan_blob_metadata` → ALTW external | **10-15%** | Metadata scan ~10% of wall at Europe (~30 s / 293 s); single-pass over 50K+ blobs; buffer-discard cost dominates |
| `extract` all strategies | **5-15%** | 3× header scans pre-PASS1-reuse; each scan ~10% of wall pre-reuse |
| `check-refs` / `check-ids` | **3-8%** | Schedule scans 15-17 s of 72-93 s total planet wall |
| `try_extract_multi_single_pass` | **2-4%** | SCHEDULE_SCAN 26 s / 800 s wall = 3.3%; near-full recovery if fix is perfect |
| `tags-filter` | **1-3%** | Schedule is a small fraction; tag-index filtering reduces blob set before scan |
| `renumber` | **1-2%** | Schedule-scan ~16 s of 194 s planet wall; amortized across 11.6 B-element rewrite |

The ALTW external and extract families are the high-value targets. The
rest are rounding-error wins but still win in the same commit.

## Regression-check plan

Minimum-cost sequence to validate the fix across all nine callers.
Total bench cost: **~22-25 min** serial on plantasjen.

1. **Europe `extract --smart`** (3-4 min) — UUID `48ca6bbb` baseline
   181.4 s. Hits three extract callers simultaneously (simple, complete,
   smart share `build_blob_schedule_with_passthrough`).
2. **Planet `add-locations-to-ways --index-type external`** (12 min) —
   UUID `123f70f1` baseline 698.1 s [TAINTED]. Largest absolute expected win.
   Exercises `scan_blob_metadata`.
3. **Europe `tags-filter highway=primary`** (2 min) — UUID `9562d82b`
   baseline 112.2 s. Both single-pass and two-pass paths.
4. **Planet `renumber`** (3-4 min) — UUID `f9098cab` baseline 194.2 s.
   Exercises `build_all_blob_schedules`.

Check-refs / check-ids / multi-extract not in the minimum set — they
share `build_classify_schedules_split` with tags-filter and extract, so
validating those two covers the shared path. A full
`brokkr suite pbfhogg --bench` run is the belt-and-braces option for
post-fix documentation, ~60 min.

## Next steps

1. (this commit) instrumentation sweep — fill the gaps identified by
   the audit across the 9 callers so each has a header-walk marker
   bracket, a schedule-size counter, and hotpath annotation. Gaps are
   fixed in-place in the same commit; no separate doc needed.
2. Specialized `impl<R: Read + Seek> BlobReader<BufReader<R>>` with
   the `seek_relative`-first override of `seek_raw`. Single commit,
   self-contained, zero call-site changes.
3. Run the 4-step regression-check plan above. Update
   `reference/performance.md` for each caller where the phase delta
   is meaningful (>1 s wall or >3% of phase).
