# Incremental geocode index update

> **Scope.** This plan targets *avoiding* the full rebuild when a daily
> OSC lands - patch or merge against the existing index instead of
> re-running the 20-minute build. Complementary effort in
> [geocode-build-opportunities.md](geocode-build-opportunities.md)
> targets the full-rebuild wall itself (20 min → ~10 min). Even with a
> working incremental path, the full build remains the bootstrap and
> the periodic-compaction fallback, so both plans land value.

## Problem

Full geocode index rebuild from planet PBF takes ~1,255s (20.9 min) and
**29.5 GB peak anon RSS** in `GEOCODE_PASS1_5` (commit `7e9c2e9`,
2026-04-17). The 1,255 s wall is still carrying the
`has_indexdata` / `check_sorted_and_indexed` regression tax (live in
`4ce7e93..c0ae9a7`) because the planet rebench queued in `overnight.sh`
round 2 hasn't landed yet - see
[geocode-build-opportunities.md](geocode-build-opportunities.md) for
the wall-time opportunity stack and rebench status. RSS / phase-peak
data is unaffected.

The earlier 17.8 GB figure under-reported: brokkr previously suppressed
short-emitting phase markers from sidecar output, so PASS1_5's transient
peak never showed up. The peak itself has not changed - only its
visibility.

A daily diff changes ~5-30M elements, but most geocode-relevant data
(admin boundaries, street geometries, address points) is stable.
Rebuilding from scratch every day wastes 99 %+ of the work - that's the
case for this plan, independent of whether the full-rebuild wall ends
up at 20 min, 10 min, or 5 min.

## Current build pipeline (one-paragraph sketch)

Four full-file passes: Pass 1 collects admin relation boundaries, Pass
1.5 collects the node IDs referenced by geocode-relevant ways (streets,
interpolation, admin boundary ways) into an `IdSetDense`, Pass 2 is a
fused node+way scan that emits address points + street ways +
interpolation ways and resolves their coordinates through a compact
rank-indexed coord array, and Pass 3 buckets everything by S2 cell
(fine + coarse levels) into the final 19-file index. See
[geocode-build-opportunities.md](geocode-build-opportunities.md#current-architecture-for-reference)
for the full per-pass architecture - this plan only cares about what
each pass *contributes* to the index that a diff could invalidate:

- Pass 1 → admin boundary polygons (rare changes)
- Pass 1.5 → referenced-node ID set (churns with way edits, but the
  *set* is stable across diffs even when individual IDs change)
- Pass 2 → address points, street ways, interpolation ways, and the
  node-coord array (the bulk of what a diff actually mutates)
- Pass 3 → S2 cell buckets + spatial joins (re-triggered by any
  geometry change, even if the element itself isn't edited)

## What changes in a daily diff

From OSC analysis of typical planet daily diffs:

- **Admin boundaries:** rarely change. Boundary geometry changes are
  infrequent (maybe 1-5 per day globally). Admin name changes slightly
  more common.
- **Streets:** ~10K-50K way modifications per day (new roads, name
  changes, classification changes). ~80% are geometry-only changes
  (node moved, not street name changed).
- **Address points:** ~5K-20K node modifications per day. Most are new
  addresses being mapped.
- **Interpolation ways:** rare changes, ~100-500 per day.

## Approaches

### Approach 1: Diff-aware rebuild

Apply the OSC to the PBF first (`apply-changes`), then rebuild the
index from the updated PBF. This is the current workflow but doesn't
save any rebuild time.

### Approach 2: Index patching

Parse the OSC to identify which geocode entries are affected, then
patch the existing index files in place.

**For address points (nodes):**
- Deleted nodes: mark entry as deleted (tombstone)
- Modified nodes: update coordinates and/or tags
- Created nodes: append new entries

**For streets (ways):**
- Modified ways: recompute way geometry (need node coordinates for
  the updated refs). Update S2 cell assignments if geometry changed.
- Created ways: insert new entries.
- Deleted ways: mark as deleted.

**For admin boundaries (relations):**
- Modified boundaries: rare but expensive - need to reassemble the
  polygon from member ways, recompute S2 cell assignments, update
  the admin polygon entry + all affected cell entries.

**Challenges:**
1. **S2 cell reassignment:** if a street way moves to a different S2
   cell, the old cell must be updated (remove entry) and the new cell
   must be updated (add entry). The current format uses sorted arrays
   per cell - insertion/deletion requires shifting.
2. **String pool updates:** new street names, address values must be
   added to the string pool. Existing strings are referenced by offset
   - can't be moved.
3. **Compaction:** after many patches, the index files accumulate
   tombstones and fragmentation. Need periodic full rebuild to compact.

### Approach 3: Append-only index with merge

Instead of in-place patching, maintain a primary index (from the last
full build) and a delta index (from accumulated diffs). Queries check
both indices and merge results.

**Build delta index:**
- Parse OSC, extract geocode-relevant changes
- Build a small geocode index containing only the changed entries
- Store deletion markers for removed entries
- Delta index is tiny (minutes to build, < 100 MB)

**Query merge:**
- `Reader::query()` checks both primary and delta indices
- Delta entries override primary entries for the same element
- Deletion markers suppress primary entries

**Periodic compaction:**
- When delta index grows too large (>10% of primary), rebuild from
  scratch. This is the full 22-minute rebuild, done weekly or monthly.

**Advantages:**
- Delta build time: seconds to minutes (only changed elements)
- No format changes to the primary index
- Clean separation: primary is immutable, delta is append-only
- Reader changes are minimal (check two indices instead of one)

**Disadvantages:**
- Query overhead: two lookups per query instead of one
- Delta accumulation: many days of deltas degrade query performance
- Need to track which primary entries are superseded

### Approach 4: S2 cell-level incremental rebuild

Identify which S2 cells are affected by the diff (changed nodes/ways
whose old or new coordinates fall in that cell), then rebuild only
those cells. The unaffected cells are copied from the old index.

**Implementation:**
1. Parse OSC to identify affected element IDs
2. Map affected elements to S2 cells (from old index + new coordinates)
3. Read the affected cells from the old index
4. Rebuild only those cells from the updated PBF (or from the old index
   entries + OSC changes)
5. Write new index: unchanged cells copied, affected cells rewritten

**Advantages:**
- Proportional to diff size, not total data size
- No format changes
- No query-time merge overhead
- Works with the existing sorted-array-per-cell format

**Disadvantages:**
- Need bidirectional mapping: element → cell (requires reading the
  index) and cell → elements (the index already provides this)
- Must handle elements that move between cells (old cell needs update
  too)
- Admin boundary changes still require full rebuild of affected admin
  cells (potentially large area)

## Recommendation

**Approach 3 (append-only with merge)** for v1:
- Simplest to implement (delta index is just a small geocode index)
- No format changes to primary index
- Reader changes are minimal
- Natural compaction story (periodic full rebuild)

**Approach 4 (S2 cell-level rebuild)** for v2:
- Better query performance (no merge overhead)
- More complex but proportional to diff size
- Requires understanding cell-element mapping

## Prerequisites

- The geocode index format already has `replication_sequence` and
  `replication_timestamp` in the header - designed for this use case.
- `apply-changes` is already validated at planet scale (762s, 1.8 GB).
- OSC parsing exists in `src/osc.rs` (`parse_osc_file`).

## Effort estimate

- Approach 3 v1: ~2-3 days (OSC filtering for geocode-relevant changes,
  delta index builder, reader merge logic, compaction command)
- Approach 4 v2: ~5-7 days (cell-level diff computation, partial
  rebuild, cell copy infrastructure)

## Review feedback (April 2026, Opus reviewer)

### Approach 3: BLOCKING issue - no element ID dedup

The index format does NOT store OSM element IDs in any record type.
`AddrPoint` has (lat, lon, housenumber_offset, ...) but no node ID.
`StreetWay` has (node_offset, name_offset, ...) but no way ID.
Without element IDs, the reader cannot determine which primary entries
are superseded by delta entries. A modified address point would appear
**twice** in query results (once from primary, once from delta).

**Resolution options:**
1. Extend format (version 2) to include element IDs in every record.
   Adds 8 bytes per record (~15-20% size increase). Enables dedup.
2. Build a side-car "element → cell + record offset" mapping during
   index construction. Delta builder reads this to generate tombstone
   masks for the primary index.
3. Abandon Approach 3 and use Approach 4 (rebuilt cells are
   self-consistent, no dedup needed).

### Approach 4: NEEDS_DESIGN items

- **Cell boundaries not independently addressable.** Cell data is
  contiguous in shared files - rebuilding one cell shifts all
  subsequent offsets. Effectively requires rewriting every file
  end-to-end with selective changes. This is a filtered-copy, not
  an in-place patch.
- **No element-to-cell reverse mapping.** The index doesn't map
  "way 12345 → which S2 cells." Need either a side-car mapping
  built during construction, or re-derive cells from old coords.

### Other findings

- **Admin boundary changes:** 1-5/day globally - rare enough to
  trigger full rebuild on admin geometry change. Name-only changes
  are cheap.
- **Interpolation spatial join:** Delta builder must re-run the
  endpoint resolution join for affected interpolation ways +
  neighboring address points. Bounded to local S2 cell neighborhood.
- **Sequence consistency:** Use `replication_sequence` in header.
  Delta builder rejects if base_sequence doesn't match primary.
- **Delta size:** ~1.25 MB/day, ~38 MB after 30 days. Negligible.

### Updated recommendation

Approach 3 is blocked without format changes (element ID dedup).
Approach 4 is more viable but requires the filtered-copy infra
and a side-car element-to-cell mapping.

**Revised plan:** If incremental updates are pursued, start with a
format v2 that includes element IDs (needed for both approaches).
Then evaluate whether the simpler Approach 3 (delta index + merge)
or the better-performing Approach 4 (cell-level rebuild) is the
right implementation.
