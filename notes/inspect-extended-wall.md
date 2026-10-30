# `inspect --extended` planet wall (research note, transient)

Status: transient research note, 2026-10-30, trimmed the same day after the
parallel summary scan landed. Planet `inspect --extended` on bygg went from
13m22s / 26 MB peak anon (`9b49e8fb`) to 41.4 s / 571 MB, at 29.3 average
cores (reference/performance.md, "inspect full-decode scan on the decode
pipeline").

What landed (the note's R1, on the existing engine rather than a new pump):
the full-decode scan runs on `read::pipeline::run_pipeline` through
`ElementReader::for_each_fused_blob`. Each data blob is decoded and scanned
on a worker into a fresh `ScanState`; the consumer appends those in file
order (`ScanState::append`, `TypeIdRange::append` against the previous
non-empty part of each type) and replays block numbering, the ordering
summary and the block list. The reader skips unknown frames opaque
(`BlobReader::skipping_unknown_frames`) and judges the first frame's type
before parsing its body (`BlobReader::leading_header`, now used by every
`ElementReader` constructor). What remains is below.

## Remaining ideas

### R2. A validating borrowed projection scanner as the worker body

After R1 the run is worker bound (the dispatcher waits on admission for
96 % of wall), so per-blob CPU now sets wall. The worker still constructs a
`PrimitiveBlock` (offset tables appended to the payload), runs
`validate_groups_strict`, then iterates `Element`s with per-node info
structs and `keys_vals` rescans. One fused pass over the decompressed bytes
would cut that:

- read block parameters first (protobuf field order is free);
- tight min/max/first/last loops over the id and coordinate columns;
- dense metadata in one lockstep loop (a negative-version slot suppresses
  that slot's other fields while its deltas still feed later slots);
- one `keys_vals` delimiter walk for tagged-node counts;
- plain Info parsed once; refs and members checked for structure only;
- UTF-8 check of the stringtable in a borrowed pass.

Validation is fused, not dropped: inspect keeps refusing every file it
refuses today. Traps: wrapping delta and scaling semantics (scaling raw
extrema is not equivalent under negative scales), repeated packed segments,
mixed-group iteration order, all-or-none dense info. Risk: a second wire
walker beside block.rs; share varint, packed iteration and group framing.
It would also shrink the per-worker envelope behind the 571 MB.

### R3. One scanner for every decoding inspect mode

Plain inspect's decode fallback, `--distrust-metadata`, `--locations`,
`--blocks` and `--extended` already share R1's engine; with R2 they become
projections over one scanner. Afterwards, if this works, the same engine
serves check `--ids` and getparents (notes/pool-rss-growth.md R2).

### R4. Per-blob summaries written by the producer (later)

A versioned per-blob extension carrying measured counts, ID boundaries,
timestamp range, bbox and metadata coverage would let a repeated inspect
skip decompression. Stored summaries are claims, so this is a second
product ("a description from claims") beside the measurement; needs a
design document first.

## Lateral findings (still open)

1. **The metadata census is not libosmium's `detect_available_metadata`,**
   although the code comment and CORRECTNESS.md say it is: dense user
   coverage tests `raw_user_sid() > 0` (a positive sid can resolve to an
   empty string); plain changeset, uid and user count field presence,
   including zero or empty values; suppressing negative-version dense slots
   is a pbfhogg policy. No test exercises the census on real metadata.
   Settle this before R2's differential tests bless the current behaviour.
2. `brokkr results --command inspect` misses the denmark `fileinfo
   --extended` row `e4f4508c`, stored under command `fileinfo`.
3. The hotpath rows stored as `inspect` are tags-count runs, so no profile
   of this path exists.
4. The README row has no commit or host.
5. Full-decode inspect refuses files whose bad structures (for example
   malformed way refs) do not affect any reported statistic. Possibly
   intended, undocumented as a choice.
6. The 41 s cell's 571 MB is the per-worker decode envelope (30 workers);
   the decompress pool drops every planet buffer as oversize
   (notes/pool-rss-growth.md L1/L2), so each blob is a fresh multi-MB
   allocation.
