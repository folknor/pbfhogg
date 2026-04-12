# Geocode index builder: remaining opportunities

## Interpolation endpoint resolution: flatter spatial index

The interpolation endpoint resolution phase builds a transient
`FxHashMap<u64, Vec<u32>>` mapping S2 cell IDs to byte offsets into
`addr_points.bin`. At planet scale this is ~1 GB heap (~150M address
points across ~10M distinct S2 cells, each with an individually allocated
Vec).

A flatter representation — sorted `Vec<(u64, u32)>` with binary search,
or a compact CSR-style array (one contiguous offset array + one contiguous
values array) — would reduce allocator overhead and pointer chasing.
The structure is short-lived (created during resolution, dropped
immediately after), so the win is peak heap reduction, not throughput.

Current planet peak heap during this phase: ~2.5 GB. The transient index
is the largest contributor (~1 GB). A CSR-style layout could cut that
roughly in half.
