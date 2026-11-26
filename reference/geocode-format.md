# Geocode index format

The first four bytes of `geocode_header.bin` are `GIDX`. The next four
bytes hold the little-endian format version. Version 10 is current. Readers
require an exact version match. Rebuild an older index with
`build-geocode-index`.

## Interpolation in version 10

`interp_ways.bin` stores one 40-byte record per resolved section with an
interior house number. Unresolved ways still count toward build diagnostics
and contribute no interpolation records or cell entries. Sections in the
same source way can share a split node in `interp_nodes.bin`.

| Bytes | Type | Field |
| --- | --- | --- |
| 0-7 | u64 | Byte offset into `interp_nodes.bin` |
| 8-11 | u32 | Parent name offset into `strings.bin` |
| 12-15 | u32 | Start house number |
| 16-19 | u32 | End house number |
| 20-21 | u16 | Node count |
| 22 | u8 | Interpolation mode |
| 23 | u8 | Parent kind (street or place) |
| 24-31 | u64 | Byte offset into `interp_prefixes.bin` |
| 32-39 | f64 | Total section length in radians |

Each coordinate in `interp_nodes.bin` is a little-endian i32 latitude
followed by an i32 longitude, both in decimicrodegrees. Each section has
`node_count - 1` little-endian f64 entries in `interp_prefixes.bin`. An
entry is the length in radians from the section's first node to the start
of that segment. The first prefix is zero. Prefixes and the total are built
by adding segment lengths in section traversal order, using the same f64
distance calculation that the reader uses for the partial segment between
its start and the snap point. Accumulation restarts at every section.

Fine and coarse interpolation cell entries still reference an interpolation
record and a segment index within it. The reader adds the stored prefix to
the partial segment length and divides by the stored total before applying
the section's numbering rule.
