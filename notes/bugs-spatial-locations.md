# Bug hunt: spatial and node-location resolution

Hunt date 2026-08-12. Single Opus hunter, scope: `src/geo.rs`, `src/scan/`,
`src/commands/spatial.rs`, `src/commands/extract/`, `src/commands/altw/`, and
`src/idset.rs` as used from this scope. Findings only, unverified.

## `build_classify_schedule` EOF check can overflow

`src/scan/classify.rs`: `meta.data_offset + meta.data_size as u64 > file_size`.
On a corrupt header advertising a huge `data_size`, this wraps in release and the
check that exists specifically to catch corrupt blobs passes. Use `checked_add`.

## `parse_geojson` silently uses only the first feature of a FeatureCollection

`src/commands/extract/mod.rs::extract_geometry` - `features.first()`. A user
handing in a 5-polygon FeatureCollection gets a 1-polygon extract with no
warning. Either union all features into a `MultiPolygon` or error out.

## `BboxInt::from_bbox` and `Bbox::contains` disagree at the boundary

`BboxInt::from_bbox` widens by floor/ceil while `Bbox::contains` (f64) does not,
so the two containment paths disagree by up to 1 decimicrodegree at the edge of
the box. Filed standalone when the parent finding (no lat/lon domain validation
on bbox input) was fixed: the domain check landed, this did not. The widening is
deliberate on the blob-filter side, where being conservative is correct, so any
fix has to separate "which blobs might contain a match" from "does this point
match" rather than changing `from_bbox` for both. `brokkr verify extract`
compares byte-wise against osmium, so the boundary rule is load-bearing.

## `--polygon` geometry bypasses the bbox domain check

Filed 2026-08-20, one layer up from the `parse_bbox` domain fix. `extract --bbox`
now rejects a latitude outside -90..90 or a longitude outside -180..180, but
`Region::Polygon` derives its `Bbox` from the GeoJSON ring coordinates rather
than from `parse_bbox`, so a `--polygon` file containing out-of-domain vertices
still reaches `BboxInt::from_bbox` and still saturates. Same symptom the bbox fix
removed: an empty extract with exit code 0. The fix is to validate the ring
coordinates as they are parsed, which is also where the FeatureCollection
handling above needs work, so the two are probably one change.

## `apply-changes --locations-on-ways` still uses the `(0, 0)` Null Island sentinel

Filed 2026-08-20 while fixing the same defect in `add-locations-to-ways`. That
command now carries an explicit presence bit in its sparse store, counts
resolution on presence rather than on a non-zero coordinate in its external
backend, and refuses by default to write an output containing unresolvable
references. `apply-changes --locations-on-ways` was untouched and still
substitutes `(0, 0)` for an unresolvable ref, reporting the total post-hoc as
`loc_missing`. `reference/osmium-parity.md` documents the current behaviour and
now says plainly that bringing it into line is open work. osmium has the same
`(0, 0)` fallback there, so any change is a deliberate divergence and needs a
DEVIATIONS entry.

## The geocode index builder still filters way coordinates on `(0, 0)`

Same defect, third site, also filed 2026-08-20. The builder's way-coordinate
collection drops `(0, 0)` pairs as "unset", so a way vertex genuinely at Null
Island is discarded. `docs/guide/correctness.md` now scopes its Null Island entry
to this site alone.

## The PBF read path does not validate coordinate ranges

Filed 2026-08-20, alongside the bbox domain check. `parse_bbox` now rejects a
latitude outside -90..90 or a longitude outside -180..180, and the sparse node
store refuses to index a node whose coordinates are outside the same domain. The
general read path (`read/elements.rs`, the dense node iterators) still hands out
whatever the file says. Closed-document territory - recorded here because the
bbox work made the asymmetry visible, not as a request to change the read path.

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

**Test defect found 2026-08-20 (round 1 audit).** `signed_area_ccw` and
`signed_area_cw` were asserting the bug as correct: both fixtures were labelled
"CCW square" / "CW square" under the `(x, y)` reading the function performs
internally, which is the reading no caller uses. That labelling is why the
inverted sign was never noticed from the test suite. Both are renamed to say
which reading they pin, and
`signed_area_sign_is_inverted_for_real_assemble_rings_output` now pins the
actual behaviour for a ring in `assemble_rings` order, so correcting the
convention has to be a deliberate change that fails a test rather than a silent
one. The convention itself is still unfixed and still belongs to this entry.

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

## Sparse index temp file: ENOSPC becomes SIGBUS

`altw/sparse.rs` - `set_len(total_bytes)` on a sparse file then `MmapMut` and
store into it. If the scratch filesystem fills mid-pass (europe ~29 GB, planet
~60 GB), the store faults and the process dies with SIGBUS and no diagnostic.
There is no free-space precheck and no `fallocate`. Also
`drop(std::fs::remove_file(&temp_path))` discards the unlink error, so on the
unlikely failure path a multi-GB file leaks silently; and `create_new(true)` on a
pid-named path turns a stale file from a crashed run into a hard failure with a
confusing message.

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
