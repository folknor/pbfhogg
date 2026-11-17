# Geocode defects

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.
```

Defects from the 2026-11-17 defect hunt in the geocode index builder, the
index format and reader, and `geo.rs`. Every finding was made by one codex
hunter per scope reading source at `63d4ec1d`. None was built, run or
reproduced. "Reported by" names the scope or scopes, and the bracketed
priority is the hunter's own label. Findings that match an existing mechanism
were added to `notes/bugs.md` instead of being filed here: builder tag
truncation (BUG-004), coordinate narrowing in Pass 2 (BUG-025), candidate
ranking ties (BUG-026) and building placement at the antimeridian (BUG-027).
Entries are in no order of importance.

## GEO-001 - The index overwrite check accepts any directory containing geocode_header.bin

Reported by: geocode-builder [P1], filesystem-output [P2].

`replaceable_index_dir` (`geocode_index/builder/mod.rs`) accepts a nonempty
directory whenever an entry named `geocode_header.bin` exists. That entry can
be an empty file, an unrelated file, a symlink or even a directory. No header
contents or index structure are checked before publication replaces the whole
destination and deletes its backup. For example, an unrelated directory
holding an empty subdirectory called `geocode_header.bin` passes
`index_output(..., Replace, ...)`, and a successful build replaces it and
deletes its contents. This breaks the function's promise that overwrite
replaces an index or an empty directory, never an arbitrary one, and the
index-or-empty restriction in `docs/cli/commands.md`. `OutputDir` faithfully
carries out the authorization it is given, so the eligibility check is the
wrong end. The check should establish that the destination is an index, while
still allowing replacement of older formats.

## GEO-002 - The library index builder can delete its own input PBF

Reported by: filesystem-output (flagged as a boundary hazard).

`build_geocode_index_to` (`geocode_index/builder/mod.rs`) checks whether
scratch lies inside the destination, but not whether the input does. A library
caller can rebuild an existing index from `index/source.pbf` with scratch
outside the index. The build reads the input, and publication then replaces
the index directory and deletes `source.pbf` along with the old generation.
The CLI prevents this with its containment preflight. The documented refusal
belongs to the CLI contract, so the hunter flags the library behaviour
separately rather than claiming that contract covers this API. It is a
destructive difference between the two entry points.

## GEO-003 - Missing nodes produce invented street segments and admin rings

Reported by: geocode-builder [P1].

`resolve_way_into` (`builder/pass2.rs`) removes unresolved references with
`filter_map` and treats the remaining coordinates as one continuous way. For
refs `A, missing-B, C` it writes the segment `A-C`, and a query on that chord
can return a zero-distance street that does not exist in the source. For an
admin ring, removing missing vertices can leave a smaller closed polygon,
which admin assembly (`builder/admin.rs`) accepts. This breaks the
missing-geometry contract in `docs/cli/commands.md`, because absent nodes
contribute a newly constructed edge. The reader and cell cover correctly
process the geometry they are given. The hunter's proposal is to keep gaps
structurally: streets keep contiguous resolved runs, and admin assembly never
closes rings across unresolved references. Interpolation already carries
completeness explicitly.

## GEO-004 - A corrupt relation member role becomes an outer boundary

Reported by: geocode-builder [P1], read-block-decoding [P2].

`select_relation` (`builder/pass1.rs`) uses `m.role().unwrap_or("")`, and an
empty role means outer. Take an ordinary boundary relation with an outer ring
and an enclosed ring whose role index is out of bounds. The enclosed ring
becomes another outer polygon instead of causing a refusal, so queries inside
it can return the administrative area where a hole should have excluded it.
`CORRECTNESS.md` acknowledges this exact fallback. Both hunters say it
conflicts with the refusal contract: the decoder returns an error correctly,
the consumer discards it, and a failed lookup is not an empty role. The fix is
to propagate the role error.

## GEO-005 - The builder can publish an index with nothing queryable

Reported by: geocode-builder [P2].

The empty-index gate (`builder/mod.rs`) counts interpolation source records
before resolution. Use two untagged nodes and one way tagged only
`addr:interpolation=all`, with no streets, addresses or admin polygons. The
interpolation record passes the gate, resolution keeps it with zero endpoints
(`builder/interp.rs`), the reader correctly refuses to answer from it, and the
build succeeds anyway. This contradicts the gate's own comment and the promise
in `docs/cli/commands.md` to refuse an index with nothing queryable. The gate
should judge usable output after resolution.

## GEO-006 - A negative replication timestamp wraps to an enormous positive one

Reported by: geocode-builder [P2], geocode-reader [P2].

`build_generation` (`builder/mod.rs`) casts the header's signed timestamp
straight to `u64`. A timestamp of -1 becomes `18446744073709551615`, is
published, and is returned unchanged by `Reader::replication_timestamp()`.
This breaks the claim in `docs/guide/advanced.md` that this metadata describes
the input header. The builder checks representability for the neighbouring
replication sequence but not for the timestamp. Either keep signed timestamps
(a format change) or refuse values the format cannot represent.

## GEO-007 - Public geometry calls can hang while unwrapping longitudes

Reported by: geocode-builder [P2].

`unwrap_ring_longitudes` (`geo.rs`) repeatedly adds or subtracts 360. With an
infinite longitude subtraction never changes the value, and a large finite
longitude such as `1e308` behaves the same because subtracting 360 rounds back
to itself. `point_in_ring_with_antimeridian` with the ring
`[(0,0), (INFINITY,0), (0,1)]` never returns, and `point_in_polygon` and
`simplify_ring` reach the same code. These public signatures document no
finite-coordinate precondition and no nontermination. The builder's own
coordinates cannot reach this, so it is a public API fault. Fix: validate the
domain or use bounded normalization.

## GEO-008 - Integer longitude normalization overflows on its accepted type

Reported by: geocode-builder [P2].

`wrapped_longitude_delta_e7` (`geo.rs`) adds half a turn before reducing
modulo a turn. `i64::MAX` overflows that addition: with overflow checks it
panics, and without them it returns the wrong longitude, because wrapping
modulo `2^64` changes the later remainder. `normalize_longitude_e7` inherits
the problem, and its documented job is folding an unwrapped `i64` longitude
with no narrower precondition. Fix: reduce modulo the full turn before adding
the half turn. Current builder callers stay within a safe range.

## GEO-009 - Admin ranking area and containment use different longitude frames

Reported by: geocode-builder (flagged as surprising; the documented policy
defines ranking by stored planar area, so the hunter notes that changing it
would change policy).

The area calculation (`builder/admin.rs`) uses raw longitudes, while
containment unwraps antimeridian crossings. A wider dateline-crossing
rectangle can therefore store a smaller ranking area than a narrower one.

## GEO-010 - Snap and distance can describe opposite half-world paths

Reported by: geocode-reader [P2].

`reader.rs` rebuilds the snap point from an integer longitude delta, which
always resolves an exact 180-degree separation westward. The distance
calculation wraps independently converted radians and can resolve the same
separation eastward. The builder (`builder/cover.rs`) explicitly acknowledges
this difference and covers both paths. In an eastward case, a query at the
segment midpoint can get a distance of about zero but a reported snap 180
degrees away. This breaks the meaning of `snap_lat_e7` and `snap_lon_e7`, and
the function's own requirement to interpolate in the distance calculation's
frame. Projection should return the actual closest point, or both calculations
should share one longitude-frame representation.

## GEO-011 - Corrupt index entry references give successful, incomplete answers

Reported by: geocode-reader [P1].

Open deliberately leaves entry contents unchecked. During lookup
(`reader.rs`), an invalid address index is skipped, invalid street or
interpolation references become `SEGMENT_MISS`, and invalid admin indices are
also skipped. Changing a list's only address index to `addr_point_count` keeps
every validated file length and list span intact. Open succeeds, the query
returns `Ok` with no address or with a farther one, and the CLI prints that
answer as a success. Invalid polygon metadata gets through the same gap: a
positive NaN `area` passes open and is silently excluded by ranked lookup, and
an unsupported `admin_level` can appear in raw candidates while ranked lookup
drops it. This breaks the input-fault refusal contract and ADR-0004, because
these are detectable invalid references, not legitimate absence.
`QueryError` currently represents only bad query coordinates, so the fix needs
either complete validation at open or an index-error path through searches
and sinks.

## GEO-012 - Admin cells are binary-searched without validating their order

Reported by: geocode-reader [P2].

`validate_admin_tree` (`reader.rs`) validates list spans but never checks cell
ordering or duplicate keys, and both admin query paths binary-search the
table. The fine and coarse geo tables do check strict ordering. If three
distinct admin-cell records are reversed with offsets and lengths preserved,
open still succeeds, and binary search can miss existing cells and return no
containing area. This breaks the format's sorted-table contract and the
promise to refuse input faults rather than return shortened answers.

## GEO-013 - The empty-string sentinel check accepts a nonempty string

Reported by: geocode-reader [P2].

`reader.rs` claims to validate the sentinel by checking that offset zero
decodes, but never checks that it decodes to an empty string. A pool beginning
with the bytes `01 78` decodes offset zero as `"x"` and passes. Optional
address names go through `pool_name`, so an absent street stored at offset
zero can become `Some("x")`, and an address with both name offsets zero can
even pass the requirement that it names a street or place. This breaks
ADR-0025 and the format's requirement that offset zero hold the one-byte zero
sentinel.

## GEO-014 - Candidates carry no binding to the reader that produced them

Reported by: geocode-reader (flagged as an API hazard).

`Candidates::into_result(reader)` and `Reader::interpolate(candidate)` accept
candidates from another reader or another index generation without detecting
the mismatch. They can combine a street and distance from generation A with a
house number computed from generation B's unrelated record at the same index.
The required provenance relationship is neither encoded nor documented.
Binding candidates to their originating reader would also keep the polygon
identity that BUG-026 in `notes/bugs.md` needs.
