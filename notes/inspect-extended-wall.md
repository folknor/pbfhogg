# `inspect --extended` planet wall (research note, transient)

Status: transient research note, 2026-10-30. Research only. No code was
changed and no new cells were run. It covers the `notes/pre-release.md` bullet
"Runtime lead: `inspect --extended`" under "What moved". On planet, host bygg
(Ryzen 9950X3D2) at `627883d7` took 13m22s (`9b49e8fb`). The README figure is
13m41s on plantasjen (5900X) at `16e3694` (`19db1512`). That is -2.3 %, while
every other row ran 10-60 % faster on bygg.

Inputs:
- the scout inventory `target/spar/scout-inspect-extended.md`, with its
  load-bearing claims checked against source;
- stored sidecars;
- the source at `16e3694` (read with `git grep <rev>`, not the git log);
- two exchanges with a codex `read` spar session
  (`01a18d97-526e-7ec2-9c6c-281413d9b521`), which converged.

---

## 1. Established facts

- **The pass is one thread and CPU bound.** `9b49e8fb` averages 1.0 core,
  about 97 % user and almost no kernel time. Reads run at about 110 MB/s, with
  0 majflt and peak RSS 31.6 MB. `peak_threads=2`, and the second thread is
  the sidecar sampler. I/O is not the limit: the same binary read about 87 GB
  in 100 s for `inspect --tags -j 16`.
- **Every stage runs serially on the calling thread** (`full_decode_scan` /
  `scan_data_blob` in src/commands/inspect/scan.rs): a 256 KiB BufReader read,
  frame parse with a fresh `Vec` per frame, zlib inflate, a
  `PrimitiveBlock::from_vec_with_scratch` that appends offset tables,
  `finish`, the full-metadata `block.elements()` iteration, and the
  per-element `process_element` + `update_extended_for_element`. `-j` never
  reaches this path.
- **What changed in this path between the two commits** (checked in the source
  at `16e3694`):
  - `PrimitiveBlock::finish` did not call `validate_groups_strict` then. It
    does now, for every blob.
  - The whole-stringtable UTF-8 pass existed at both commits.
  - Full-metadata iteration, zipped dense info columns and the per-node
    `keys_vals` rescans existed at both commits.
  - The old scan copied the decompressed payload twice. The current one moves
    the `Vec` into the block with no copy, so `decompress_buf` is allocated
    fresh for every blob.
- On plantasjen, same host, a sequential full decode with no metadata access
  (`bench-read sequential`, `f7d78f56`, `a65cecc`) takes 565 s.
  `par_map_reduce` over the same input takes 60 s. The extended pass costs
  820 s. **About 45 % of the cost is extended-specific consumer work on top of
  a full decode**, and all of it is serial.

## 2. Root cause and hypotheses

| # | Hypothesis | Confidence |
|---|---|---|
| H1 | The structural cause of the 13 minutes is serialization. Inflate, construction, validation and billions of element visits share one core. The report needs none of this to be serial. | **High** (agreed with codex) |
| H2 | The missing host speedup is mostly the per-blob `validate_groups_strict` pass added after `16e3694`. On dense blobs it walks the id, lat, lon and denseinfo columns and the `keys_vals` runs before iteration walks them again. On ways and relations it validates refs, locations and members, which the extended stats never read. The removed payload copies partly offset it. | **Medium.** It is the only new per-blob cost found in this path. "A quarter to a third of current CPU" is plausible, but counting passes does not prove it. Validation mostly checks varint framing rather than reconstructing values. Only the `keys_vals` walk truly duplicates iteration. The zlib share and the per-core host gain on this workload are both unmeasured. |
| H3 | The host has less single-thread advantage on this branchy varint and zlib workload than the other rows suggest, because the other rows are mostly parallel. | Medium-low. It may contribute, but cannot be separated from H2 without a cell. |
| H4 | The fresh per-blob `decompress_buf` allocation and the per-frame `Vec` cost materially. | Low. minflt is only 17k across 800 s. |

**The one decisive cell, if the attribution matters:**
`brokkr inspect --extended --dataset planet --bench 1 --commit 16e3694` on
bygg. It separates code from host. It would not attribute the difference
inside the code. Neither codex nor I think it is needed to choose the design.
The serial structure loses either way, and R1 below makes the attribution moot.

## 3. High-conviction architectural rewrites

### R1. A parallel per-blob summary scan with an ordered reduce (lead item)

- **Bottleneck.** All CPU work runs on one thread.
- **Why the structure causes it.** `scan_data_blob` folds elements straight
  into one `&mut ScanState` / `&mut BlockAccum`. The order-sensitive pieces
  have forced the whole loop to be serial: `TypeIdRange.monotonic/prev_id`,
  `OrderingSummary` and `block_number`.
- **Redesign.**
  1. One pump thread reads the stream once. This keeps the FIFO and no-pin
     contract.
  2. The pump owns the frame policy:
     - an initial header is required, and a later header is refused;
     - unknown frames are skipped and not counted;
     - data-blob sequence numbers are counted separately from frame numbers;
     - a partial index is accepted, and `is_indexed` keeps its "readable
       indexdata on every data blob" meaning;
     - errors keep their locations.
  3. Workers decompress into a persistent worker-owned buffer and reduce each
     blob to a `BlobSummary`:
     - per type: `count, min_id, max_id, first_id, last_id,
       strictly_increasing`;
     - tagged-node count;
     - timestamp extrema and metadata-coverage counts;
     - the node bbox;
     - block kind, frame and raw sizes;
     - location stats when they are requested.
  4. The main thread reduces summaries in sequence order. Monotonicity per
     type is `inc(A) && inc(B) && (A.last < B.first)` against the previous
     *non-empty* summary of that type, so a type missing from a blob does not
     reset the boundary. Repeated IDs across a blob boundary are caught. The
     reducer replays `OrderingSummary::observe` once per blob, which is cheap
     and keeps segment semantics exact. Mixed blocks stay non-standard and an
     empty block stays `Mixed`, unless the report is deliberately changed.
- **Not `par_map_reduce` as it stands.** It reduces partials in no fixed
  order, disables index parsing and has the wrong input policy. Borrow its
  pump, budget and cancellation mechanics.
- **Errors.** Workers return `Result` per sequence number, and the reducer
  reports the lowest-sequence error. When failure k appears, lower an atomic
  admission cutoff to k, but let every task already admitted below k finish.
  Never cancel globally, because that can hide the real earliest error. Pump
  and framing errors take a position in the same sequence. Panic, cancel and
  spawn failure must wake the reducer. Bound admission before execution, so a
  slow earliest task cannot pile up completed summaries or compressed input
  behind it. A pump blocked in `read()` on a FIFO cannot be interrupted
  promptly, so document that.
- **Payoff.** It removes the serial CPU ceiling outright. The comparable shape
  measured 60 s against 565 s on the slower host, but that run did lighter
  work, so it is not a prediction for this pass. Expect the limit to move to
  worker CPU or the pump. Either way the result is minutes to tens of seconds,
  and the H2 regression stops mattering.
- **Risks.**
  - Correctness of the summary algebra: split-point tests, repeated boundary
    IDs, mixed and empty blocks.
  - Liveness of the admission and cancel protocol.
  - Memory: the compressed-input budget does not bound decoded worker storage,
    so worker buffers must be accounted for too.

### R2. A validating borrowed projection scanner as the worker body

- **Bottleneck.** The work is: construct with offset tables appended into the
  payload, then validate every structure, then iterate through general
  `Element`/`DenseNodeIter` with per-node info structs and `keys_vals`
  rescans. Each column is walked two or three times.
- **Redesign.** One pass over the decompressed bytes, with no `PrimitiveBlock`
  and no offset tables:
  - read block parameters first, because protobuf field order is free;
  - decode the id and coordinate columns into tight min/max/first/last loops,
    checking the counts;
  - walk dense metadata in one lockstep loop. It must stay lockstep because a
    negative-version slot suppresses that slot's other fields, while its
    deltas still feed later slots. Independent column loops would be wrong.
  - walk `keys_vals` once, counting key-position delimiters (a zero value is
    not a delimiter, an empty array means untagged, and a final run may lack
    its delimiter);
  - parse each plain element's Info once;
  - check plain tag arrays, way refs and relation members for structure only;
  - run the UTF-8 check on stringtable framing in a borrowed pass.

  **Validation is fused, not dropped**, so inspect keeps refusing every file it
  refuses today. Codex argued for this and I agree. Dropping validation would
  be a separate product decision ("a description that does not claim
  structural validity"), and it is not needed for the payoff.
- **Traps to keep:**
  - wrapping delta and scaling semantics. Scaling only the raw extrema is not
    equivalent under negative scales or wraparound.
  - repeated packed segments and duplicate messages;
  - the iteration order of mixed groups for per-type ID order;
  - the rule that dense info is all-or-none. A deliberate widening to
    libosmium's partial columns is optional.
- **Payoff.** Removes construction, the appended offset tables (the relocation
  noted in pool-rss-growth), the element abstraction and the duplicate
  parsing. Per-blob CPU then sits near inflate plus one varint pass. This is
  the pool-rss-growth R2 projection scanner, applied to the first command
  where it pays most.
- **Risks.** It is a second wire walker beside block.rs, so drift between the
  two must be prevented. Share the primitives: varint, packed iteration and
  group framing.

### R3. One scanner for every decoding inspect mode

Plain inspect's decode fallback, `--distrust-metadata`, `--locations`,
`--blocks`/`--anomalies` and `--extended` become projections over the R1/R2
scanner: a requested projection in, a summary out, with no element callback.
The plain index-only path stays as it is (201 ms, `3ec07278`). Afterwards, if
this works, the same engine serves check `--ids` and getparents, as in
pool-rss-growth R2.

### R4. Per-blob summaries written by the producer (later)

A versioned per-blob extension would carry measured counts, ID boundaries and
monotonicity, timestamp range, bbox and metadata coverage, so a repeated
inspect would never decompress. Skipping the read beats saturating it.
Stored summaries are claims, though, so they create a second product, "a
description from claims", alongside the independent measurement. Partial
coverage could be filled in by scanning only the blobs without a summary, with
the provenance reported. Ranked below R1/R2 because the measured scan must
exist anyway (agreed).

## 4. Medium-value local changes

| # | Change | Note |
|---|---|---|
| L1 | Keep `decompress_buf` across blobs: decompress into a reused buffer and borrow the block, or return the buffer | Small; H4 says the allocation is cheap at planet. R1 workers get this automatically. |
| L2 | Skip `validate_groups_strict` for structures the caller never visits, or fuse it into iteration | The direct fix for H2 inside the current design. R2 replaces it. |
| L3 | Count dense tags from one `keys_vals` delimiter walk instead of `tags().next()` per node over a rescanned run | Local version of part of R2 |
| L4 | Use `elements_skip_metadata` plus a separate lockstep denseinfo loop | Local version of part of R2 |
| L5 | A larger read buffer, or a reader thread | Not the bottleneck today. It becomes relevant once R1 makes the pump the limit. |

## 5. Prioritized list

1. **R1** parallel summary scan with an ordered reduce, using the current
   decoder in the worker. This makes the regression irrelevant.
2. **R2** validating projection scanner as the worker body.
3. **Settle the metadata-coverage semantics** before R2's differential tests
   bless the current inconsistencies (lateral finding 1).
4. **R3** one scanner for every decoding inspect mode.
5. **R4** producer-side summaries.
6. Retune allocation and the decompressor only after 1-2 (L1, L5).
7. L2-L4 only if R1/R2 are delayed.

## 6. Plan of action

Each step can be taken up on its own.

- **H2/H3 (attribution, optional).** One `--commit 16e3694` cell on bygg,
  then the HEAD cell, interleaved and one at a time. If HEAD is about 25-35 %
  slower on the same host, H2 holds. If they match, H3 holds. A `--hotpath`
  run on denmark at HEAD would show the share taken by `validate_groups_strict`.
- **R1.** Build it completely with the existing decoder in the worker, then
  run one planet cell. Read `--stalls`/`--durations` to see whether worker CPU
  or the pump binds.
  Tests:
  - summary concatenation at every split point;
  - equal IDs across a boundary;
  - mixed, empty and type-skipping blobs;
  - unknown frames, a partial index and FIFO input;
  - a delayed earliest worker;
  - an error at k with an earlier error at j < k;
  - cancellation while admission is blocked.

  The current serial implementation is the oracle.
- **R2.** Swap in the scanner and diff it against R1 output on generated
  fixtures, including the corrupt-input corpus. Cover metadata sentinels and
  padding, multiple dense groups, unusual granularity, scales and offsets, and
  tag-run edge cases. Every input the current route refuses must still be
  refused.
- **R4.** A design document first: the extension format, provenance in the
  report, and the partial-coverage fill.

## 7. Spar record

- **Agreed:**
  - Serialization is the structural defect.
  - Ordered *summaries* are the right shape, not an ordered element consumer.
    With an ordered consumer, billions of element visits stay serial, and the
    255 s pipelined figure is no bound for this pass in either direction.
  - The summary needs per-type first/last/min/max/monotonic, with boundaries
    tracked against the previous non-empty summary of each type.
  - The scanner leads; producer summaries follow.
  - The ordered-error protocol, with an admission cutoff that lets the earlier
    prefix finish.
- **Codex moved me on:**
  - **The attribution.** My first claim was that a whole bundle of costs grew
    between the commits: validation, UTF-8, dense iteration and allocation.
    The source at `16e3694` shows only strict validation is new. UTF-8 and the
    iterator costs were already there, and the old path made two more payload
    copies.
  - My arithmetic, "validation doubles the varint work", overstates it,
    because validation checks framing rather than reconstructing values.
  - Validation must be fused, not dropped.
  - Dense metadata needs a lockstep loop because of the version mask.
  - Block parameters can come after the groups on the wire.
  - Scaling raw extrema is unsafe.
  - Do not reuse `par_map_reduce` unchanged.
  - Do not cancel globally on the first error seen.
- **Still open, not a disagreement:** the numbers in H2/H3. Only the
  `--commit` cell settles them.

## 8. Lateral findings

1. **The metadata census is not libosmium's `detect_available_metadata`,**
   although the code comment and CORRECTNESS.md say it is:
   - dense user coverage tests `raw_user_sid() > 0`, but a positive sid can
     resolve to an empty string;
   - plain changeset, uid and user count field *presence*, including zero or
     empty values;
   - suppressing negative-version dense slots is a pbfhogg policy. libosmium
     normalizes -1 and still decodes the other columns.

   No test exercises the census on real metadata: the only fixture uses
   `meta: None`. (Codex found this; I did not check it independently.)
2. `brokkr results --command inspect` misses the denmark `fileinfo --extended`
   row `e4f4508c` (`23862d1`), which is stored under command `fileinfo`.
3. Scout lateral finding 3 holds: the hotpath rows stored as `inspect` are
   tags-count runs, so no profile of this path exists.
4. The `pipeline_decompress_pool_*` counters (all zero) are emitted by a path
   that never uses the pool.
5. The README row has no commit or host, against the project's
   performance-number convention.
6. Full-decode inspect refuses files whose bad structures (for example
   malformed way refs) do not affect any reported statistic. That may be
   intended, but nothing documents it as a choice.
