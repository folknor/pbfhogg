# Bug hunt: spatial and node-location resolution

Hunt date 2026-08-12. Single Opus hunter, scope: `src/geo.rs`, `src/scan/`,
`src/commands/spatial.rs`, `src/commands/extract/`, `src/commands/altw/`, and
`src/idset.rs` as used from this scope. Findings only, unverified.

## `reframe.rs` writes decimicrodegree way coordinates while copying the source block's granularity/offsets

`src/commands/altw/reframe.rs` - `parse_block_top` copies PrimitiveBlock fields
17-20 verbatim into the output block (`scratch.scalar_fields`), and
`splice_way_locations` then writes fields 9/10 from `NodeIndex::get`, which
returns decimicrodegrees (`scan/node.rs` normalizes with
`(lat_offset + granularity*raw)/100`).

The reader side (`read/elements.rs::WayNodeLocationsIter`) reconstructs
`lat = lat_offset + granularity * value`. So the round trip is only correct when
the way blob has `granularity == 100` and zero offsets. An input with
`granularity = 1000`, or a non-zero `lat_offset`, produces way locations that are
10x off or doubly-offset - silently, with no warning and no validation anywhere
on this path. `external/stage4.rs` does the same "append scalar fields" thing.

Note the internal inconsistency: `write/block_builder.rs` deliberately omits
17-20 and relies on the defaults, so altw's decode-all fallback is correct on the
same input where the production reframe path is wrong. Fix: either normalize
(drop fields 17/19/20 from the reframe output, keeping only `date_granularity`)
or reject non-default granularity up front.

Confidence: high on the code reading; requires a non-default-granularity producer
to trigger.

## Unsorted-input fallbacks classify in a single file-order pass, silently dropping ways and relations

`src/commands/extract/simple.rs::extract_simple_two_pass` and
`src/commands/extract/smart.rs::collect_pass1_generic` (`if !is_sorted` branch)
both build `bbox_node_ids`, then `matched_way_ids` (from
`w.refs().any(|r| bbox_node_ids.get(r))`), then `matched_relation_ids`, in one
sequential pass over blobs in file order. That is only sound if nodes precede
ways precede relations - i.e. if the file is sorted. These are precisely the code
paths taken when it is not.

Failure: any way whose blob appears before the blob holding its bbox nodes is
never matched, and is silently absent from the output; likewise relations whose
members appear later. Same problem inside a single `BlockType::Mixed` block,
since elements are processed in stream order. This affects `simple`, `complete`,
and `smart`. The fix is real work: the unsorted fallback needs to be genuinely
multi-pass (nodes pass, then ways pass, then relations pass), not one pass with
three branches.

## altw sparse drops out-of-range coordinates silently

`SharedSparseWriter::insert` (`src/commands/altw/sparse.rs`) silently returns
when `byte_offset + 8 > capacity_bytes`, and `SparseArrayIndex::get` silently
returns `None` past the end. Any rank/capacity mismatch therefore turns into
silently dropped coordinates, then `missing_locations`, then `(0,0)` in the
output, with no error anywhere. The writer bounds violation should be a panic,
not a no-op: by construction the rank can never exceed the allocation, so
reaching that branch is a bug that must be loud.

Confidence: high.

## `point_in_ring` (not the antimeridian-aware variant) used for hole ownership

`src/geocode_index/builder/admin.rs` uses
`crate::geo::point_in_ring(hp.0, hp.1, &outer_f64)` to decide which outer ring
owns each hole. Everywhere else in the codebase the antimeridian-aware
`point_in_ring_with_antimeridian` is used. For an admin relation crossing +/-180
degrees (Russia, Fiji, Antarctica-adjacent), the outer ring's longitudes are
discontinuous and holes will be assigned to the wrong outer or dropped entirely.
Also, the test point is `hole[0]` - the hole's first vertex, which for admin
boundaries is very often a node shared with the outer ring, i.e. exactly on the
boundary, where ray casting is undefined. A centroid or an interior point would
be far more robust.

## `scan/node.rs` does unchecked arithmetic on attacker-controlled `granularity`

`granularity = cursor.read_varint()? as i64` with no validation (zero, negative,
or huge all accepted), then `lat_offset + granularity * cum_lat` unchecked -
panic in debug, wraps in release. `blob_meta/scan_ids.rs` explicitly uses checked
arithmetic here and comments on why; `scan/node.rs` - used by ALTW pass 1,
external stage 2, and simple-extract node classify - does not. Same class:
`parse_way_refs` / `parse_way_tagged_refs` accumulate `cum += delta` unchecked
with no cap on ref count.

## `build_classify_schedule` EOF check can overflow

`src/scan/classify.rs`: `meta.data_offset + meta.data_size as u64 > file_size`.
On a corrupt header advertising a huge `data_size`, this wraps in release and the
check that exists specifically to catch corrupt blobs passes. Use `checked_add`.

## Missing node locations become `(0, 0)` with no way to tell them apart

`altw/mod.rs::process_block` and `reframe.rs` both push `(0,0)` for unresolvable
refs and bump `missing_locations`. `(0,0)` is a valid coordinate (Null Island),
and the sparse index's "unwritten" sentinel is also `(0,0)`, so a genuine node at
exactly 0/0 is reported as missing and a missing node is written as if it were at
0/0. This is documented in several places as accepted, but the consumer-visible
effect - geometry silently teleported to the Gulf of Guinea - has no CLI-level
guard (no `--ignore-missing-nodes` / hard-fail switch, unlike osmium). Worth an
explicit flag rather than a comment.

## `parse_geojson` silently uses only the first feature of a FeatureCollection

`src/commands/extract/mod.rs::extract_geometry` - `features.first()`. A user
handing in a 5-polygon FeatureCollection gets a 1-polygon extract with no
warning. Either union all features into a `MultiPolygon` or error out.

## No lat/lon domain validation on bbox input

`parse_bbox` and the config `bbox` parser only check `min < max`.
`BboxInt::from_bbox` then does `(v * 1e7).floor() as i32`, and Rust float-to-int
casts saturate: `--bbox 200,0,300,1` yields `min_lon = i32::MAX` and an empty
extract with exit code 0. Also `BboxInt::from_bbox` widens by floor/ceil while
`Bbox::contains` (f64) does not, so the two containment paths disagree by up to 1
decimicrodegree at the boundary.

## Sparse index temp file: ENOSPC becomes SIGBUS

`altw/sparse.rs` - `set_len(total_bytes)` on a sparse file then `MmapMut` and
store into it. If the scratch filesystem fills mid-pass (europe ~29 GB, planet
~60 GB), the store faults and the process dies with SIGBUS and no diagnostic.
There is no free-space precheck and no `fallocate`. Also
`drop(std::fs::remove_file(&temp_path))` discards the unlink error, so on the
unlikely failure path a multi-GB file leaks silently; and `create_new(true)` on a
pid-named path turns a stale file from a crashed run into a hard failure with a
confusing message.

## `simplify_ring` is O(20 n depth) with unbounded recursion

`geo.rs` - the epsilon binary search runs `dp_count` 20 times over the full ring,
then a 21st pass for `dp_mark`. Both `dp_count_range` and `dp_mark` recurse with
worst-case depth O(n); admin boundary rings can be 100k+ vertices, so a
pathological (monotone) ring stack-overflows. And DP can introduce
self-intersections that flip containment near the boundary - relevant because
these simplified rings are what the geocode reader does point-in-polygon against.
An explicit stack plus a single-pass "keep the top-k by DP significance"
(priority-queue DP) would replace 21 passes with one and remove the recursion.

## `signed_area`'s documented sign convention is wrong for its only call site

`geo.rs::signed_area` documents "positive = counter-clockwise (exterior)". Its
rings come from `assemble_rings`, documented as `(lat_e7, lon_e7)` - i.e.
`(y, x)`, which flips the shoelace sign. Today nothing notices because
`admin.rs` immediately calls `.abs()`, but it is a landmine for the next caller.
Separately, the shoelace accumulates products up to ~1.6e18 in f64 (ulp ~256)
with full cancellation; subtracting a ring-local origin first would recover ~8
orders of magnitude for free.

## `multi-extract` re-probes every region per element, and compresses single-threaded

`extract/multi.rs`: the way classify does
`for i in 0..n { w.refs().any(|r| bbox_node_ids[i].get(r)) }` - refs are
re-iterated N times and each `get` is a random probe into a different multi-GB
bitmap; the node write phase does `iter().any()` and then `for i in 0..n` again.
At the documented 500-region cap this dominates. The natural shape is one pass
over refs producing an N-bit region mask, carried forward to the write phase
instead of re-derived. Additionally, multi-extract uses sync-mode
`PbfWriter<BufWriter<File>>` on the consumer thread, so all N outputs are
compressed serially by one thread, unlike every single-region path which uses the
parallel writer.

## Redundant header walks in the sorted simple path

`extract_simple_single_pass` builds `full_schedule` from a header walk, then
phase 3 calls `build_classify_schedule(Relation)` - a second full header walk,
which also discards the spatial filter it already had. `notes/altw.md` P4
documents that removing redundant walks does not pay on the altw path (they ride
warm), but this one is between two heavy phases rather than adjacent, and the
schedule it rebuilds is already in hand.

## Smaller notes

- `IdSet::merge` and `IdSet::merge_from` are the same function twice (one by
  value, one by reference). One should go.
- `IdSet::rank(id)` panics on negative/out-of-range ids while `count_below`
  clamps. Asymmetric and undocumented at the call site.
- `pread_execute` (`extract/common.rs`) duplicates its entire consumer body twice
  (in-loop drain and post-loop drain); `multi_extract_pread_write` does the same.
  Both are copy-paste that will drift.
- `assemble_rings` is greedy with no backtracking (documented) and permanently
  consumes segments into failed chains, so one ambiguous junction can destroy
  several otherwise-valid rings. An endpoint-degree-aware assembler would fix
  this properly; the current comment asserts OSM boundary relations are
  well-formed in practice, which is not true for damaged or partially-extracted
  data.
- `ring_crosses_antimeridian` uses `windows(2)`, so a crossing on the implicit
  closing segment (last to first) of an unclosed ring is missed; and any
  legitimate >180 degree longitude step (coarse world-scale ring) is falsely
  reported as a crossing.
- `AGENTS.md` says external uses "~256 GB (planet)" temp disk; `external/mod.rs`
  says "~224 GB". One of them is stale.
