# Performance findings

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

Findings from the 2026-11 performance hunt: 18 codex hunters (gpt-6.1-sol, high
effort), one per scope, reading code only. The original findings were source-derived hypotheses. Subsequent
measurements and decisions are recorded where available. "Reported by" names the scopes that raised a finding and is a fact
about the reports, not a verdict. "Kind" is the hunter's own label. Where
hunters proposed different redesigns for one bottleneck, the entry records
each. Entries are in no order of importance. Defects found on the way are in
`notes/bugs.md`.

Scope names: framing (framing and blob I/O), decode (decompression and block
decode), pipeline (pipelining and scheduling), encoding, output (compression
and output I/O), indexdata (indexdata and scanning), check (check, inspect and
digest), altw-sparse, altw-external, osc (OSC ingest), apply-core,
apply-locations, merge (merge-changes, blob merge and diff), sort (sort and
renumber), copy (copy-shaped commands and ID lookups), tags (tag selection),
spatial (extract and export), geocode.

## PERF-001 - Per-frame blob allocation and single outstanding read

Reported by: framing.

Mechanism: `BlobReader::next_frame` allocates a fresh `Vec` per body;
`Frames::next_raw` allocates a full frame and copies prefix and header in;
buffered input copies into those allocations; `DirectReader::fill_with` issues
one synchronous 256 KiB read and `append_with` copies it out before the next
read, so O_DIRECT has one outstanding request regardless of decode workers.

Redesign: read contiguous extents into a bounded pool of aligned owned buffers
with several in flight independently of decode; walk frame boundaries over
completed extents and hand out descriptors with leased payload ranges;
decompression borrows the lease and releases compressed storage right after
inflation; recycle an extent when its last user releases it. Decompression
must accept immutable payload storage plus metadata (`WireBlob::parse` already
slices without copying; `pool_wrap` already returns storage via
`Bytes::from_owner`). The scheduler must admit by retained extent bytes, not
blob count. Prefer owned extents over universal mmap: `PinnedInput` preserves
identity not immutability, truncation can fault a mapping, and mmap moves I/O
into decoder page faults.

Payoff: removes most compressed-byte copies and allocator traffic, lets direct
reads use storage concurrency; applies to every full-file command and pass;
largest where ingestion cannot keep decoders fed.

Risks: blobs crossing extents, oversized frames, short reads, cancellation with
outstanding I/O, premature reuse; raw uncompressed blocks whose lease outlives
decode.

Kind: high-conviction architectural rewrite.

## PERF-003 - Source ranges are not a first-class output representation

Reported by: output, framing, copy, altw-sparse, altw-external, apply-core,
merge, sort.

Mechanism: range copy exists (`PbfWriter::write_raw_copy`, blob merge
`CopyRun`), but many paths do not reach it: indexed `cat_passthrough` reads
complete frames and hands buffers to the writer; type-filtered cat skips
rejected bodies and buffers kept frames; header-only degrade reads every
payload and `reframe_raw` copies it again; cat reindex `reframe_with` and
`framing.rs` header-only reframing
build a new contiguous buffer copying the unchanged body; both ALTW output
routes (`Pass2Consumer::passthrough`, stage 4) read unchanged frames
synchronously on the ordered consumer thread; blob-merge `write_frame` builds
an owned reframed buffer then submits via `write_raw(&reframed)`, which copies
it. Where ranges are used, a coalesced run is one indivisible operation: in
apply-changes, coalescing has no byte bound, so a long untouched run waits
until a rewrite, create, type change or EOF flushes it and then becomes one
`CopyRange` executed serially by one parallel-writer worker. The io_uring
`CopyRange` (`UringSink::copy_range`) reads into userspace on its single
writer thread although `supports_copy_range()` includes io_uring.

Redesign: an ordered output recipe of owned byte segments and source-file
range references; a header rewrite becomes a small owned header plus a range
for the original body; unchanged runs stay ranges end to end; the scheduler
coalesces for planning and then splits into balanced, independently scheduled
extents (parallel range execution for buffered, parallel aligned read and
write for direct). Commands hand ranges and replacement segments instead of
reading a frame to call `write_raw_owned`. Target userspace traffic for indexed cat,
type-filtered cat and header-only degrade is roughly 0 % of payload; for
apply-changes, header and probe bytes plus compressed candidate payload bytes
(copy gives a per-command budget table in its report). For degrade, alternating
small headers and payload copies must be composed efficiently by the sink or
the benefit is lost.

Payoff: removes frame allocation, userspace payload reads, synchronous reads on
ordered consumers and transport of unchanged bytes through result queues; ALTW
already knows unchanged frame offsets.

Risks: preserving framing and truncation checks; source lifetime until
completion; backends without kernel copy; preserving which frames and header
fields carry forward; header-only scans must still detect malformed framing.
Kernel copy does not guarantee storage avoids the bytes.

Kind: high-conviction rewrite.

Buffered cat range copy was refused after these `copy-pbf --dataset planet`
cells. B included header skipping and submitted kept frames as ranges.

On bygg, A is HEAD `c0ec89ca` and B is the working tree on `c0ec89ca`
for the variant under test. Cells are interleaved, each with `--iterations 1`,
with one fstrim at the head of the planet suite.

| Range submission | Workload | Arm | Wall | Run |
|---|---|---|---:|---|
| Unbounded | `--type way` | A | 33.2 s | `8fa9c0f8` |
| Unbounded | `--type way` | B | 37.2 s | `61b84fcf` |
| Unbounded | `--type way` | A | 28.6 s | `7c2dc85d` |
| Unbounded | `--type way` | B | 30.0 s | `c0113a7e` |
| 16 MiB | `--type way` | B | 29.6 s | `48b40018` |
| 16 MiB | `--type way` | A | 31.5 s | `873d47a2` |
| 16 MiB | `--type way` | B | 28.3 s | `8840e914` |
| 16 MiB | `--type way` | A | 31.5 s | `52983ab5` |
| 16 MiB | Plain | A | 43.5 s | `118d9659` |
| 16 MiB | Plain | B | 64 s | `e0cc5b4f` |
| 16 MiB | Plain | A | 54.1 s | `16d20848` |
| 16 MiB | Plain | B | 65 s | `de159370` |

Buffered cat range copy was removed. Unbounded ranges lost on filtered
cat. Submitting at 16 MiB recovered the filtered gain, but plain passthrough
was substantially slower in both pairs. The likely cause is that the
buffered backend executes `copy_file_range` serially on its writer thread,
while buffered frame transfer overlaps producer reads with writer writes.
Revisit buffered cat range copy only after a named backend change such as
parallel range execution.

The final header-skip-only results are in the [cat performance record](../reference/performance/cat.md).

## PERF-004 - Header discovery and payload work are separate walks with a full-file barrier

Reported by: framing, pipeline, indexdata, apply-core, copy, altw-sparse.

Mechanism: `PreadSource::refill` reads probe windows that can contain payload,
but `HeaderWalker::next_header` exposes metadata only and `pread_data` reads the
payload again into another buffer. `schedule_walk` finishes the whole walk and
materializes `Vec<ScheduledBlob>` before workers start, and
`WillneedPrefetch` walks that vector again; workers then issue one positional
read per entry and block on it inside `run_unordered_per_blob`. Commands
rediscover framing repeatedly: sparse ALTW walks headers for its size
estimate, reference collection, node collection, relation members and output
schedule; `time_filter_snapshot` builds a full schedule although it schedules
every data blob. Coalescing prefetch advice is not coalescing reads.

Redesign (indexdata): one executing catalog per command (ordinal, offsets,
lengths, complete index claim, optional compact tag metadata, observed facts
kept apart from claims, views of blob IDs, explicit completion and failure)
that publishes bounded batches to workers while appending entries; later
phases plan from it without touching headers. Dense consumers build the
catalog during consumption; sparse ones keep probing and batch nearby ranges
into extent reads. Redesign (apply-core, copy): for repeated update chains or
repeated queries, the output side retains a compact blob directory (offsets,
lengths, kinds, counts, bounds) bound to the exact file so the next run need
not probe headers. Redesign (pipeline): a validated frame directory from the
I/O side would unlock parallel initial range discovery.

Payoff: removes whole dependent traversals and overlaps discovery with work.

Risks: the next offset depends on the current header, so the first walk stays
serial; global sizing may still need completion; partial-index refusals must
abort even after workers start; keep `InputFront` lazy precondition ordering
and separate file descriptions where advice needs isolation; a directory that
describes another file is a correctness failure; overreading gaps and pinning
windows behind slow consumers.

Kind: high-conviction architecture (dense scans); conditional for sparse; lower
conviction for the persisted directory on one-off runs.

## PERF-005 - frame_pool reads and folds on one thread

Reported by: framing, pipeline.

Mechanism: `frame_pool` uses `run_ordered_streams_from` with the frame reader as
source; `run_streams_core` fills the initial window before starting workers and
the consumer refills it between folds, so reads and folds cannot overlap, a
blocked read stops consumption of finished results and a fold blocked on writer
backpressure stops input. `issue_job` creates a fresh channel per task though
each produces one result. Affects cat reindex, repair and check-index.

Redesign: separate storage completion, worker execution and ordered
consumption with reusable result slots.

Payoff: overlap of input and fold on dense lightweight passes.

Risks: changing which failure reaches the caller first.

Kind: high-conviction (part of PERF-004 architecture).

## PERF-006 - Validation, then a second walk to decode what was validated

Reported by: decode, indexdata, check, geocode, tags, osc, merge, apply-locations.

Mechanism: `validate_groups` (block.rs) parses every ordinary element and its
`Info`, checks every packed array and fully decodes dense `keys_vals`, then
discards most of it; iteration parses messages again, `info()` again; group
framing is walked by validation and again by separate searches for each kind.
Dense tags are decoded three times (validation, `DenseNodeIter::next` to find
runs, the tag iterator). `elements_skip_metadata` still pays construction-time
metadata validation. `GroupProjection` / `BlockBuffer::project` hooks run after
validation scanned the arrays, so `KindIds`, `WaySpans` and `IdScratch::each`
re-decode spans. Raw scanners avoid this but materialize: node scans build all
`(id, lat, lon)` tuples, way scans always build a full reference `Vec`, and
callbacks cannot return errors. `blob_meta` `Walk::run` does nearly the same
work for ID-only and tag-only requests. Examples of the second walk: smart
extract classification (count arrays, build block, rediscover dense, decode
three columns, classify), ID checks, reference checks, inspect tagged-node
count, extended inspect metadata, location checking's separate raw scanners,
geocode pass 2 dense address extraction, tags-filter dense tags, diff element
iteration that decodes dense metadata content equality ignores. Single-pass
`tags-filter` dense output still advances coordinates and metadata before
selection, including for rejected nodes.

Redesign: one block scan establishes framing, string table and group layout;
the consumer states which fields need values and which only validation; values
are decoded and consumed in the validating traversal into bounded numeric
batches (prefix sums, coordinate conversion, classify, reuse), with fallible
sinks, reference folds without a mandatory `Vec`, summaries accumulated in the
same decode, and results kept provisional until the block passes. Per-consumer
projection table in the decode report (IDs; IDs and coordinates; way refs;
typed members; numeric tag pairs; timestamp and visibility; selected address
keys; selection plus retained wire fields). Separate required validation from
requested summaries (`packed_varint_count` can check cardinality without
reconstructing values). Batch cumulative decoding in `protohoggr` is unused; a
strict batch decoder that validates, counts and prefix-sums together is the
stronger kernel, with SIMD inside it.

Payoff: removes repeated interpretation across billions of elements and enables
the fused passes in other entries; ID-only and coordinate-only consumers stop
advancing unrelated streams.

Risks: dense-first order in mixed groups; wrapping deltas, narrowing, metadata
presence; provisional results discarded on late failure; descriptor-per-node
memory; raw scanners and the full validator accept different malformed inputs,
so unifying them must not change refusals; do not replace cheap validation of
an unused column with full reconstruction.

Kind: high-conviction architectural rewrite; medium conviction on whole-command
payoff (indexdata).

## PERF-007 - Transforms decode and rebuild fields they do not change

Reported by: decode, encoding, tags, spatial, copy, merge, sort, apply-core,
apply-locations, altw-sparse.

Mechanism: `BlockBuilder` accepts semantic values, so commands lose source
provenance before choosing representation. For an unchanged retained way:
decode ref deltas, materialize a `Vec`, resolve SIDs to strings, hash into a
new table, delta-encode again, rebuild metadata and nested messages.
Paths: tags-filter `filter_block_parallel` and `filter_block_pass2`, extract
`extract_block_pass2` and smart's writer, cat clean `process_block`, repack
shrink, time-filter, sort sweep `Output::encode`, sparse ALTW filtered node
output (`process_block`), apply-changes surviving elements. `pre_seed`
copies and hashes every source string table entry, including unused ones.
Sparse selections also flush at every input-block boundary, producing output
blocks sized by input partition.

Precedents already in the tree: raw accessors on ways and relations
(`elements.rs`), apply-changes raw surviving-base writers, ALTW `reframe.rs`,
renumber `wire_rewrite.rs`, `pre_seed_string_table` and raw-way/raw-relation
builder methods (the sweep and filters do not use them).

Redesign: carry source ownership, dictionary identity, framing and field spans
to output. Selection produces retained positions or runs; cleaning produces
retained elements with specific metadata replacements; the output worker copies
unchanged fields and encodes replacements. Way ref and relation member deltas
reset per element so they survive reordering. Dense: unchanged groups keep
packed columns; contiguous retained runs copy interior deltas and repair the
first delta of each column; sparse selections re-delta but keep tag bytes.
Dictionaries: borrow and preserve IDs for single-source output; for
multi-source output a lazy `(source dictionary, SID) -> output SID` map built
once per source and output block; compact referenced entries for sparse output;
pack survivors across contiguous input ranges. Clean rewrites only selected
`Info` / `DenseInfo` fields. For multi-region extract, encode each distinct
selection once and share its frame.

Payoff: removes hashing, reference reconstruction, metadata reconstruction and
memory traffic for partial selections and modifications where whole-blob
passthrough cannot help; broadest plausible payoff in encoding's reading.

Risks: whole dictionaries retained for tiny subsets enlarge output and
compression work; block-level coordinate and timestamp framing; dense metadata
presence, signed wrapping, empty keys, duplicate strings, slot zero; pinned
source buffers must be charged; recovery behaviour such as `cat --clean user`
not resolving a discarded bad user SID; structural validation does not prove
SIDs valid, so raw copy must not bypass the checks needed for valid output;
commands that deliberately strip way locations must still strip them;
multi-source merges flushing per source would create tiny blocks.

Kind: high-conviction architectural rewrite.

## PERF-008 - apply-changes rebuilds every survivor of a touched blob and multiplies its string table

Reported by: apply-core, apply-locations.

Mechanism: `handle_candidate` decompresses, builds a validated `PrimitiveBlock`,
runs a precise overlap walk, then `rewrite_block_parallel` walks again; every
surviving dense node passes through accessors, tag decode, builder columns and
re-encoding. Output blocks cap at 8,000 elements and every block holding base
survivors is pre-seeded with the source blob's whole string table, so a source
of string table size S split into r blocks serializes about r * S table bytes
and hashes it r times (`element_writes.rs` flush and reseed). In locations mode
`detect_stale_wire` stops at the first stale way and keeps only a verdict; the
rewrite then runs `evaluate_way` again and `refreshed_locations` decodes refs
and probes the OSC again; one stale coordinate sends every surviving way in the
blob through `BlockBuilder`.

Redesign: an editor whose unit is the decompressed source blob. Validate and
discover edits together into runs of retained rows, replacements, deletions and
insertions; keep source string slots and append only new strings via a diff-SID
to PBF-SID map (duplicate string contents are legal); copy surviving ordinary
messages as spans; edit dense columns by retaining packed runs and repairing
deltas at edit boundaries; size output blocks by encoded bytes and PBF limits.
Merge OSC edits and refresh edits in one traversal: refresh hits keep positions
and replacement coordinates; a refresh-only blob keeps tags, metadata, refs,
IDs, count, string table and tag index and changes only coordinate fields and
enclosing lengths. Compression should accept retained spans plus generated
fragments backed by a stable source owner. Return the original compressed frame
when inspection finds no effective edit. Precedents: `validate_in_place`,
`BlockBuffer::project`, renumber wire rewriter, ALTW reframe (which rejects
non-default coordinate framing, so it cannot be called unchanged).

Payoff: removes repeated tables, repeated survivor encoding, temporary dense
columns and excess frames; raising the builder cap addresses only part.

Risks: delta repair, partial metadata, string-index validation, non-default
coordinate and date framing, repeated refs, blob-size limits, exact output
sidecars (regenerated indexdata and tagdata, stale-enrichment dropping); clean
refresh scans must keep their light path; dirty blobs need validation without
restarting the edit traversal.

Kind: high-conviction full rewrite.

## PERF-009 - Encode and compress cross an ordered boundary between them

Reported by: pipeline, output, encoding, altw-sparse, sort, tags, apply-locations, apply-core.

Mechanism: in several routes a worker encodes, the uncompressed `OwnedBlock`
goes through an ordered pool to a serial consumer, which reserves a writer
window token and dispatch permit and spawns a `FramingJob`; the writer then
reorders again. Examples: single-pass tags-filter (decode + filter + encode ->
ordered consumer -> writer dispatch -> compress + frame -> ordered writer),
tags-filter pass 2, ALTW decode-all and both scheduled ALTW output consumers,
sparse ALTW `passthrough.rs` worker output, renumber's rewrite stage
(`pool.rs`), apply-changes drain submitting each frame to `PbfWriter`.
`take_owned` gives the builder allocation away (`lend_encoded` is the
alternative). The stronger kernel already exists in `FramedBlocks`
(repack worker output, apply-changes worker framing, extract, cat clean)
and `frame_blob_pipelined`.

Redesign: one task reads, decompresses, transforms, encodes and compresses and
returns finished segments or ranges with statistics; multi-blob tasks stream
pieces keyed by `(input task, piece ordinal)` with an end marker so zero-output
and one-to-many transforms work; one coordinator advances the frontier and
assigns offsets; ordering is restored once, when committing bytes. Keep
asynchronous compression behind genuinely serial encoders. In apply-changes the
core plans ordered work units, including gap and trailing creates, before
encoding; workers publish frames under those identities to one coordinator that
combines ordering, copy-range coalescing and offset assignment.

Payoff: fewer ownership transfers, less queued uncompressed data, no barrier
before compression, builder buffers stay with workers. Compression is already
parallel, so this is medium value per path (altw-sparse, sort) and strongest
where ordered consumers mediate much encoded output.

Risks: load imbalance at expensive levels; large expansions; deadlock if
workers block holding resources others need; admission must bound bytes;
moving blocking writer calls inside each encode closure is an incomplete
rewrite; apply-changes window solves real shutdown and progress problems that
the replacement must keep.

Kind: high-conviction (pipeline, output); medium-value (altw-sparse, sort).

## PERF-010 - Several independently sized CPU pools instead of one scheduler

Reported by: pipeline, output.

Mechanism: `run_pipeline` builds a private rayon decode pool
(`pipeline.rs`); ordered and classify pools spawn scoped OS workers; writer
framing runs on `Host::executor`; the CLI supplies `Executor::Ambient` sized
independently; `sized_workers` subtracts a fixed reserve from machine
parallelism rather than sharing a budget. Two pools each asking for about all
cores compete.

Redesign: one execution context per operation spanning its readers and
writers; CPU workers run ready kernels from any stage; commands hand block or
key-range kernels with worker-local state, projection requirements, output
destinations and dependencies; per-element `FnMut` stays only as an adapter.
Blocking reads, backend writes and caller callbacks get separate treatment.
Operation-owned task groups replace a fresh worker set per phase while keeping
`JoinedPool` / `threads::scope` completion guarantees.

Payoff: capacity follows the actual bottleneck, locality kept; sharing threads
alone does not guarantee a win.

Risks: moving today's blocking loops onto one rayon pool creates starvation and
deadlock; teardown semantics.

Kind: high conviction in architecture; workload-dependent gain; full
execution-engine rewrite.

## PERF-011 - Task-count backpressure instead of memory-aware admission

Reported by: pipeline, apply-core.

Mechanism: the read pipeline admits at most 32 tasks regardless of workers or
result size and holds permits through ordered consumption; the ordered pool
uses `max(32, workers * 8)`; a multi-extract result can hold a blob's output for
every region. Writer constants prescribe 16 write workers, eight queued
operations per worker, 64 framing permits and a 512-item window
(`tunables.rs`), counting unlike things. In apply-changes the sequence window
charges a cheap passthrough descriptor and an enormous rewritten result the same
token, returned only after ordered drain, so one slow blob stops discovery.

Redesign: account CPU work and retained bytes separately; completed frames
release decode storage but keep output credit until committed; compact
summaries run further ahead than expanded output; admission suspends or leaves
tasks unissued rather than parking workers; prioritize the earliest unfinished
output and its dependencies; split large groups into child tasks; reserved
progress capacity and an oversized-item rule. The unordered reader's byte
budget (`reader.rs`) is a local precedent. Separate the bounded metadata plan
from bounded payload storage in apply-changes.

Payoff: strongest on many-core hosts, skewed blobs and expanding transforms.

Risks: memory accounting can deadlock if later tasks consume what the head
needs; one zlib stream is not internally parallel; a slow head can still idle a
bounded stream.

Kind: high conviction that the bounds describe the wrong resources.

## PERF-012 - Output frames are flattened and packed by one owner

Reported by: output.

Mechanism: compression writes worker scratch; `frame_blob_into` allocates a
contiguous frame and copies parts in; `DirectWriter::write` copies into its
aligned buffer; `UringState::write` copies into registered buffers on one
thread. The parallel writer dispatches round-robin to private bounded queues
(`parallel_writer.rs`), so one full queue stalls dispatch while others are idle.

Redesign: compression writes into an allocation whose ownership travels to I/O
(reserved prefix space, or separate small headers plus payload with vectored
I/O); `Compression::None` sends the encoder allocation directly; buffered output
uses positional vectored writes; direct output schedules aligned destination
extents after offsets are known, each gathering its segments into one aligned
buffer, boundary fragments owned once, only the tail padded; shared ready work
of byte-sized extents also splits giant ranges (PERF-003); pool and admission
belong to the output execution with per-destination ordering. Descriptor budgets
are not the limit: positional workers share one descriptor.

Payoff: removes full-output memory passes and the single-thread packing ceiling.

Risks: buffer lifetime through async completion, short writes, non-overlapping
direct extents, never publishing holes; keep an explicit proof every extent
completed.

Kind: strongest rewrite for low-cost-compression or uncompressed output.

## PERF-013 - Dense node builder stages integer columns before packing

Reported by: encoding.

Mechanism: `add_node_columns` and `add_dense_metadata` push deltas into integer
vectors; `encode_dense_nodes_group` walks them into packed scratch, copies into
enclosing messages, then into the block. A tagless node with full metadata
holds about 57 bytes of numeric columns before slack. Way and relation encoders
(`encode.rs`) stage packed fields, copy into the element, then the group, then
the block. The optional partial-node path builds both a plain shadow and a
dense candidate and keeps the smaller.

Redesign: compute each delta once, append its varint to a packed byte column,
track lengths, compute enclosing lengths, assemble directly into the final
block buffer once; a bulk path from decoded columns (the decoder's
`DenseNodeColumns` lacks metadata, dictionary IDs and tag spans); assemble way
and relation spans directly or reserve prefixes and compact once; choose the
partial-node representation from accumulated sizes without encoding both.
Encoding should write into the buffer compression reads; `lend_encoded` and
`FramedBlocks` already provide that, so there is no universal builder to
compressor copy to remove. Once the representation is packed bytes, a bulk
varint writer with one capacity reservation (instead of protohoggr's scalar
pushes) becomes worth trying; not before. Iterator or column inputs for way
refs and relation members belong here too (OSC-way temporaries in
`add_osc_way` alone were refused as too rare to show).

Payoff: removes work on every genuinely rebuilt node.

Risks: interleaved column writes may behave worse than separate loops; prefix
backpatching can cost as much as the copies; size accounting for outliers;
partial metadata semantics.

Kind: high-conviction encoder rewrite.

Measurement on bygg, HEAD `b25180b3` against the working tree on
`b25180b3`: planet `copy-pbf --clean yes` was about 3 % faster with packed
columns, at the noise floor. Re-encoding is likely compression-bound.
The prototype did not land. The earlier eager width-prepass was also
refused. A third encoder shape needs a profile showing encode time first.
See the [encoder measurements](../reference/performance/write-pipeline.md#packed-column-encoder-neutral-bygg-baseline-b25180b3).

To resurface: the code is in
`notes/stashed-perf-013-packed-column-encoder.patch` (made against
`b25180b3` on bygg). It edits `write/block_builder.rs` and
`write/encode.rs` and needs no tunables.

## PERF-014 - Degrade unsort still funnels elements through one central encoder

Reported by: copy, decode, encoding.

`worker_decode_kind` sends selected elements as owned records to the central
builder for the adjacent swap in each kind. Settle the swap and its boundary
records in a small coordinator and encode the resulting partitions on workers.
`--unsort-intra` can keep input boundaries away from the splice. `--unsort`
needs a prefix-count plan for continuous packing to the cap.

Per-source string ID (SID) caching also remains open in repack grow. Cache
translations from each source string table within the output partition's
dictionary so repeated source IDs can reuse their translations.

2026-11-21 verifier hunt (heavy-codex, other-codex): `verify degrade` and
`verify check-ids` still pay for the unsort funnel when building
dataset-sized fixtures for a local swap.

## PERF-015 - Sort and blob merge serialize each overlap component on one worker

Reported by: sort, merge, encoding, copy.

Mechanism: the planner groups intersecting blob ranges into connected
components and `write_plan` gives each to one `Sweep::run`, which reads,
decompresses, keeps the cursor heap, resolves duplicates, encodes and
compresses on its worker. Parallelism is only between components. Scrambled
input or two files with different blob boundaries chain into one component
covering a whole kind. There is no external spill: cursor demotion only turns
mostly consumed cursors into owned tails, the same-ID group is accumulated
whole, so dense live overlap is unbounded. Unindexed or untrusted input is
decompressed in pass 1 to establish ranges and order, discarded, then reread;
mixed-kind frames can be decoded per kind. A single internally ordered blob
with two far apart IDs connects a long section and forces all of it through
decode and re-encode.

Redesign (sort): one adaptive engine - trusted ordered extents stay compressed;
small disorder is repaired in bounded memory; large disorder forms bounded
sorted runs (compact arenas and columns, small key-and-offset records, spill
when the budget fills) merged in parallel over disjoint key ranges with bounded
fan-in and skew splitting; the pass-1 scan produces the run representation;
the merge frontier supports whole-blob transfer when a blob's full range
precedes every competing head and unopened range, so only blobs with actual
insertion points are rebuilt, and the sweep emits source extents as well as
framed chunks. Redesign (merge): range-partitioned executor - decode
contributing frames once in parallel into records of key, version, provenance
and payload handle; assign bounded output key ranges to independent merge
workers; compress independently; assemble in range order; partition between
IDs of one kind (a giant single-ID group can split by version after
normalizing). Both readings require decode-once sharing so ranges do not each
decompress wide blobs. Rebuilt blobs should use dictionary translation rather
than semantic round trips (PERF-007). Already sorted indexed input keeps
today's coalesced raw copy.

Payoff: largest in the sort scope - removes single-core execution of the
largest component, a redundant decompression pass, and gives unsorted planet
input a defined memory bound.

Risks: scratch amplification; skew; intermediate representation that expands
PBF (use dictionaries, packed payloads and dense columns, not serialized owned
elements, and not repeated PBF intermediates); duplicate precedence (earliest
input, then frame, then occurrence); versions out of order within a blob; OSM
ordering of negative IDs; equal IDs at boundaries; partial metadata; sort must
retain every occurrence independently of merge deduplication.

Kind: high-conviction full rewrite.

Status: the sort half is stashed against `5a8af88b`. Large overlap components
above `SORT_RANGE_MIN_RECORDS` (1 million records in the patch tested on bygg)
decode once, spill packed records and merge disjoint key ranges in parallel.
On bygg, the Denmark `unsorted` A/B cells on `5a8af88b` and the patched B cells
for Denmark `unsorted-intra` and planet `history2` raw with `--force-sort`
emitted neither range span. No component crossed the threshold. The walls
and run IDs are in [sort performance](../reference/performance/sort.md#parallel-key-ranges-for-large-sort-overlap-components-stashed).
The new path was unmeasurable on this host and remains outside the landed
implementation. The blob-merge redesign remains open.

To resurface, create a bench input whose disorder forms a large overlap
component, such as an element-level shuffle across the whole file. No such
bench input exists yet. Apply
[notes/stashed-perf-015-sort-parallel-key-ranges.patch](stashed-perf-015-sort-parallel-key-ranges.patch)
against `5a8af88b` and confirm both `SORT_RANGE_PREPARE` and `SORT_RANGE_MERGE`
spans before comparing walls. The patch carries `blob_merge.rs`, `sweep.rs`,
`sweep/ranges.rs` and `sweep/ranges/`, `sort/mod.rs`, `tests/merge_pbf.rs`,
and the `SORT_RANGE_MIN_RECORDS`, `SORT_RANGE_TARGET_RECORDS` and
`SORT_RANGE_TARGET_BYTES` tunables.

## PERF-016 - Broader raw passthrough for cat --dedupe

Reported by: copy, merge.

Passthrough currently covers isolated raw dense-node frames with strictly
ordered unique IDs, no tags or metadata, and framing equal to the sweep's.
Those frames are rare in real files. Extending passthrough requires proving
that the retained records and framing match the sweep's output. Indexdata
alone cannot prove uniqueness. See the
[self-dedupe measurements](../reference/performance/cat.md).

## PERF-019 - Multi-region and unindexed extract still decode selected node blobs twice

Reported by: spatial, framing.

The retained-node-frame path covers indexed single-region smart and
complete-ways extract. Multi-region and unindexed input remain open. For
multi-region, work and byte admission would have to account for frames
retained for multiple destinations. For unindexed input, establish isolation
against every node extent before reusing a frame. Reuse in either case also
requires ruling out completion IDs for the destination.

## PERF-020 - Polygon extract measurement and coordinate-column classification

Reported by: spatial.

Real-data polygon verification passed for extract and multi-extract on
Denmark and Germany at `d5074628` on bygg. The Planet comparison on bygg
between `8fa7af9e` and `d64b3548` was neutral for `berlin-concave`.
See `reference/performance/extract.md` for the results.

Measure subdivision on a complex administrative boundary polygon with
thousands of segments and on a multi-extract workload with many regions.

Polygon classification still uses general element iteration on paths
where bbox classification can consume coordinate columns. Extend those
column paths to polygon regions while preserving exact membership.

## PERF-021 - Multi-extract writes all destinations from one consumer

Reported by: spatial, pipeline, output.

Mechanism: `write_regions` drains every destination serially from one
aggregate result through synchronous writers; one slow destination stalls
others; admission credit covers an input blob's whole fan-out. `MultiLocal`
flushes every region's builder at each input-block boundary, making small
blocks for sparse matches. The multi-region complete-ways and smart engine
(`extract/multi_dep.rs` `write_pass`) has the same shape: its ordered-pool
consumer writes every region's frames for a blob through each region's
synchronous writer in turn (found by the 2026-11-26 spar accounting).

Redesign: per-destination ordered frontiers and write queues under one shared
byte budget and one shared output scheduler (not a full pool per region); pack
output across input blocks.

Payoff: removes artificial coupling between unrelated files and poor
compression from tiny blocks.

Risks: aggregate budget fairness.

Kind: high conviction (as part of PERF-011); local change (output).

Measurement on bygg: planet `region-extract --mode multi` with five simple
strips was about 2 % faster (noise), and one `--strategy complete` pair
was about 4 % faster, comparing the working tree on `b4e65632` with HEAD
`b4e65632`. The prototype did not land. The gains on this workload did
not justify changing rebuilt block boundaries. A retry needs a slow or
skewed destination workload, such as many regions with one huge region.
No registered bench has that shape. See the [multi-extract measurements](../reference/performance/extract.md#multi-extract-destination-queues-and-tail-packing-bygg-b4e65632).

To resurface: the code is in
`notes/stashed-perf-021-multi-extract-destination-queues.patch` (made against
`b4e65632` on bygg). It edits `extract/multi.rs`, `extract/multi_fused.rs`
and `extract/multi_dep.rs` and adds `extract/multi_output.rs`. Restore
`mod multi_output;` in `extract/mod.rs`, which the saved patch omits.
Restore the tunables used by the measured prototype in `tunables.rs`:
`EXTRACT_MULTI_OUTPUT_BYTES = 64 MiB`, `EXTRACT_MULTI_OUTPUT_WORKERS = 4`
and `EXTRACT_MULTI_PACK_BYTES = 256 KiB`.

## PERF-023 - Export should consume the ALTW join directly

Reported by: spatial.

Mechanism: export requires inline way locations and currently does no lookup;
producing an ALTW PBF first pays delta encoding, compression, intermediate I/O,
decompression and decoding to cross a command boundary. Extract of a located
input now keeps the locations, so extract followed by export works, but still
pays that boundary twice.

Redesign: put the export sink right after coordinate resolution - sparse
exposes the completed coordinate store to geometry workers, external delivers
per-way coordinate spans before PBF field conversion; filter before join
requests; include member ways needed for relation geometry; a future GeoParquet
writer builds geometry and property columns on the same workers with parallel
row groups and an ordered assembler, not via GeoJSON.

Risks: the join seam must expose missing-coordinate status and positional
association with refs; ALTW's output policy must not silently become export's
invalid-geometry policy.

Kind: architectural direction.

## PERF-025 - Reference membership is inserted by one consumer

Reported by: indexdata, altw-sparse, tags, check.

Sparse ALTW pass 0 is done: workers insert word-batched and atomically over a
claim-sized reservation (planet pass 0 36.5 s to 15.3 s,
`reference/performance/add-locations-to-ways.md`), and tags-filter pass 1
took the same shape (planet invert-match 4m45s to 3m03s,
`reference/performance/tags-filter.md`); `check --refs` replays
word-batched too (planet within 3 %, `reference/performance/check.md`).
What remains: under `--inject-prepass` the reservation factor is halved
across the two sets, so a dense extract leaves its top chunks to the
serial union (Japan spilled 97 M of 323 M refs, 617 ms serial). Raising it
is a memory envelope decision, not a tuning one.

Chunk-partitioned owners were considered for pass 0 and not built:
atomic insertion over a bounded reservation removed the serial union with
no second channel layer; owners stay the answer only if a sparse-ID input
needs occupancy-driven chunks with parallel insertion.

Kind: high conviction.

## PERF-028 - ALTW routing models only sparse's page-cache store

Reported by: altw-sparse.

Sparse already skips reference collection when its all-node store fits.
Germany peak anonymous memory rose from 3.36 GB at HEAD `31d469b5` to
7.27 GB on the working tree on `31d469b5`, on bygg. See
[the sparse all-node measurement](../reference/performance/add-locations-to-ways.md#sparse-all-node-store-skips-reference-collection-when-it-fits-bygg).

Mechanism: auto charges only sparse's file-backed store (8N up front, 8U on
the recheck) against available memory minus a fixed 3 GiB anon allowance.
Neither backend's input-dependent anon is modelled: external grows with
reference occurrences per bucket at capped concurrency (peak anon about
3 GB germany, 6.9 GB europe, 11.6 GB planet, sidecars 975f18d9, 36c31139,
d9bdc5d7, no host budget), and reference-selected sparse pass 0 grows with the referenced-ID
set's shape (scattered negative IDs promote 4 MiB bitmap chunks before any
recheck; below the allowance no recheck is armed). Until both footprints
are modelled the below-allowance fallback to sparse stays (a monotone rule
toward external was refused, reasoning at `page_cache_budget_bytes`), and a
sorted file with many standalone nodes and some ways still routes external
on 8N far above 8U (a no-way file now stays sparse; up-front pass 0 refused,
reasoning at `store_estimate_bytes`).

Redesign: account for each regime's anonymous memory in routing, including
the resident all-node store and the reference-selected sparse path. Near the
boundary or with skewed input, consider resident partitions plus a bounded
spill join publishing into one ordered assembly. Sparse and external converge
in lifecycle and assembly, keeping different physical joins.

Risks: the hybrid is medium-confidence (duplicates, skew, scratch lifetime,
backpressure, keeping resident and spilled results in sync); choosing resident
versus bounded correctly is higher confidence.

Kind: high conviction (routing); medium (hybrid).

## PERF-030 - External ALTW request deduplication and fragment formats

Reported by: altw-external.

Landed: the bounded stage 2 coordinate table streams requests in way order.
`--inject-prepass` retains the sorted pin-counting path. On bygg, planet
external `embed-locations` improved about 5-8 % wall and 13 % in stage 2,
comparing HEAD `b25180b3` with the working tree on `b25180b3`. See the
[external stage 2 measurements](../reference/performance/add-locations-to-ways.md#external-stage-2-coordinate-table-bygg-baseline-b25180b3).

Remaining: dedupe requests by node ID and output group with per-group
coordinate dictionaries. Explore fragment formats with blob identity and
count once per fragment, a local node ID column, delta- or run-encoded
destinations, and separate closure flags. Reply order within a fragment
could make per-coordinate slot fields redundant and allow framed delta
or bit-packed coordinates.

A split bucket rereads its request shards for each lookup partition.
`s2_request_scans` counts those scans, including the initial scan for
each bucket. Check it against the bucket count when evaluating whether
rescans matter for a workload.

## PERF-031 - External ALTW materializes a third scratch stream between stage 3 and 4

Reported by: altw-external, encoding.

Mechanism: stage 3 reads a slot bucket, scatters into an 8-byte-per-slot
array, encodes per-blob payloads, `pwrite`s them, publishes locations through a
router, and stage 4 `pread`s them back; concurrency does not remove the
storage. Stage 3 emits interleaved latitude and longitude varints
(`coord_payloads.rs`); stage 4 walks every reference to find varint boundaries
and copies into separate buffers, then into way, group and block. Stage 3 holds
the whole 12-byte record bucket beside the scatter array.

Redesign (altw-external): a bounded group of whole way blobs, sized by
reference count with boundaries between blobs, is the assembly unit; one worker
reconstructs its coordinates, reads the way blobs, splices and compresses; no
straddler halves or readiness router; an oversized group has an explicit
bounded path; removes the 2P round trip and stops page cache acting as an
implicit queue. Payload format, even if the split stays: per-way entries that
are complete appendable PBF fields (9 packed lat deltas, 10 packed lon deltas,
20 optional pin bitmap), so stage 4 appends one range per way and computes
enclosing lengths directly into the block. Redesign (encoding): stage 3 emits
per-way lat and lon spans with lengths and optional field headers. Local:
chunked reads scatter straight into the destination array.

Payoff: removes a materialization, billions of varint checks and several copies;
highest-conviction change in the external scope.

Risks: byte-bounded admission across coordinates, decompressed input,
assembled blocks and compression queues; ordering behind a slow early group;
stale-location stripping, pin semantics, framing checks, publication; straddler
assembly and way alignment.

Kind: high-conviction architectural change.

The packed per-way payload variant remains stashed and uncommitted. On bygg
against HEAD `d64b3548`, the working tree on `d64b3548` showed no consistent
effect on the streaming span, and drive-state drift dominated planet wall
time. The worst-case precheck rose from about 267 GB to 348 GB, with about
5 GB of expected actual payload growth. A new matched suite needs equal
predecessor workloads and a fresh trim at its start. See the
[external payload measurements](../reference/performance/add-locations-to-ways.md#external-packed-per-way-payloads-inconclusive-bygg-baseline-d64b3548)
for the cells and verification results.

To resurface: the code is in `notes/stashed-perf-031-external-altw-payload.patch`
(made against `d64b3548`; it touches `altw/external/` coord_payloads, space,
stage3 and stage4, plus the id_signs inventory count) and, on bygg only, in
the git stash named "PERF-031 external ALTW payload experiment, inconclusive
on planet". Its `STAGE3_SLOT_READ_CHUNK_BYTES` is a local const that moves to
`tunables.rs` if it lands.

## PERF-032 - External ALTW scratch file fleets and shared appenders

Reported by: altw-external, pipeline.

Mechanism: stage 1 opens 256 buffered shard files per worker, so CPU
parallelism is capped by descriptor allowance and buffers scale workers x
buckets; stage 2 has one shared writer per bucket, flushing under a mutex that
can be held across a file write; `finish` serially flushes, `sync_data`s and
evicts every bucket before stage 3. Stage 1 copies each way's refs into a
blob-wide `blob_node_ids` vector and walks it again.

Redesign (altw-external): one or a few append-only private scratch files per
producer, large framed extents and a partition directory; readers `pread`
committed extents; publish completion per output group from the request
manifest so assembly starts when its contributors finish, without a global
sync-and-evict; cache management follows extent lifetime; intermediate files
need no durability. Emit fragments from the scanner callback with a blob-local
slot counter. Redesign (pipeline): a bounded descriptor cache over private
shard files to decouple CPU concurrency from open files (conditional value on
descriptor-constrained hosts).

Payoff: removes descriptor-bound utilization, append serialization and a forced
storage barrier.

Risks: poorly sized extents fragment reads; publication, short transfers,
cancellation, errors; a single shared arena reintroduces contention; an output
group may depend on nearly every partition.

Kind: high-conviction (altw-external).

## PERF-033 - External ALTW rereads node blobs for output

Reported by: altw-external.

Mechanism: stage 2 extracts coordinates; stage 4 rereads and decompresses
selected node blobs, builds blocks, filters and re-encodes; with
`--keep-untagged-nodes` it rereads all node frames for passthrough.

Redesign: produce retained-node output in the stage 2 node work, reusing
decompressed bytes; for all-node passthrough an input task hands the original
frame to output while the payload feeds the scanner; one output owner per source
blob at partition boundaries; untouched node blobs stay scheduled if they hold
retained nodes.

Risks: benefit depends on surviving node output; ordered node output must not
queue unboundedly behind a slow partition.

Kind: structural follow-up, more conditional payoff.

The stage 2 output experiment was refused. On bygg against `5973bfc4`,
the experimental working tree on `5973bfc4` was slower in both default
planet pairs, and neutral with `--keep-untagged-nodes`. Filtering and
re-encoding on stage 2's capped workers cost more than skipping the
later node reads saved. See the
[external node output measurements](../reference/performance/add-locations-to-ways.md#external-node-output-from-stage-2-refused-bygg-baseline-5973bfc4).

To resurface, schedule node encoding on workers outside stage 2's cap.
The code is in `notes/stashed-perf-033-external-altw-stage2-node-output.patch`,
against `5973bfc4`. It covers `mod.rs`, `stage2.rs`, `stage2_table.rs`,
`stage4.rs` and the new `node_output.rs` under `altw/external/`, plus its
tunables.

## PERF-034 - apply-changes location mode decodes every node blob and forces a whole-way refresh scan

Reported by: apply-locations, apply-core.

Mechanism: scanner routing (`scanner.rs`) forces every node blob through
decompression in location mode; each node is coordinate-decoded and checked
against `needed_set`, then the blob is fully parsed and classified, even when no
OSC way needs a base coordinate. `NodeLocationIndex` builds needed IDs in sorted
order and keeps them as a hash set, so the scanner cannot exclude node ranges.
Coordinates move through a local hash map, a worker slot map and a merged map;
`add_osc_way` collects refs and coordinates into fresh vectors. Any node
upsert enables `refresh_ways`, sending every indexed way blob through
decompression and probing; the blocked Bloom filter makes misses cheaper but
not the population-wide work. Way indexdata describes way IDs, not referenced
node IDs.

Redesign: give each distinct requested node an ordinal and each OSC-way ref a
slot; intersect sorted requests with node-blob ranges; decode a node blob only
if actual node changes or coordinate requests intersect it, resolving its
request slice during the core merge traversal into an ordinal array with
presence; OSC ways consume slots as prepared location payloads. For the
whole-way search, maintain a compressed exact reverse index from node IDs to
stable way or logical block identities plus an identity-to-range directory,
published atomically with the PBF, falling back to scanning when absent or
mismatched (apply-locations calls this the largest asymptotic opportunity;
apply-core regards it as a separate architectural project). `WayMembers-v1` is
per-way relation membership, not this index. Reuse ALTW join techniques
(positional payload encoding), not the ALTW pipeline: surviving ways need an
overlay patch, only created or modified OSC ways need a join.

Payoff: avoids node decompression where no request needs it (less when requests
spread over most blobs); refresh discovery proportional to changed-node
dependencies.

Risks: duplicate-node winners, missing-node accounting, geographic checks,
(0,0) versus absent; the current path rejects malformed coordinates on
unreferenced nodes, so skipping them changes refusals; `RankedIdSet` stores
nonnegative IDs only; reverse-index construction cost and size, scattered
changes still touch many blobs; Bloom summaries per blob can admit most blobs.

Kind: high conviction (requested join); larger storage redesign (reverse index).

The selective node-blob decode experiment is stashed and uncommitted.
It decoded a blob only when a node change or coordinate request intersected
its range. On bygg, HEAD `c0ec89ca` ran in 4m22s (`50507100`) and 4m21s
(`05c983ef`). The prototype on `c0ec89ca` ran in 4m20s (`7c9e0669`) and
4m17s (`cba32b06`). The result is neutral. A daily planet OSC touches almost
every node blob for coordinates. A retry needs a small or geographically
concentrated change workload, which no bench currently registers, and a
decision on the relaxed refusals for malformed contents in skipped blobs.

To resurface: the code is in
`notes/stashed-perf-034-apply-requested-node-join.patch` (made against
`c0ec89ca`; it touches `apply_changes/` and `crates/lib/tests/fault_apply_changes.rs`)
and, on bygg only, in the git stash named "PERF-034 apply-changes -l
requested-node join, neutral on planet daily OSC". It carries a fix worth
keeping in mind for any retry: requests sort zero, then negatives by
magnitude, then positives, while blob index ranges are numeric, so a naive
range intersection admits negative requests outside the blob.
The prototype fixed its own intersection bug caused by sorting requests
zero first, negatives by magnitude, then positives, while blob ranges use
numeric order. The ordinal join and whole-way refresh redesign remain open.
See the [apply-changes performance record](../reference/performance/apply-changes.md).

## PERF-035 - apply-changes way work waits for node output to drain

Reported by: apply-locations, apply-core.

Mechanism: the scanner stops at the first non-node descriptor until the drain
has processed every earlier node item and merged coordinate maps; workers
publish coordinates before sending node output, so output backpressure delays
way decompression and refresh discovery. Output then passes workers retaining a
source blob's frames, drain reordering and gap creates, per-frame `PbfWriter`
submission, the writer's sequencing, and the backend's offsets. Creates are
synchronous batches inside the drain's type-transition logic
(`stream_output.rs`).

Redesign: replace the global node-to-way barrier with task dependencies -
created and modified ways wait for their coordinate slots, surviving-way refresh
needs only the OSC probe, relations need no coordinates; coordinate publication
follows extraction, not output submission; create encoding becomes scheduled
work; one output coordinator (PERF-009).

Payoff: better overlap, smaller completion units, less retained output.

Risks: deadlock-free admission, cancellation, failure propagation, exact create
placement.

Kind: coherent topology rewrite.

## PERF-036 - apply-changes cannot apply a backlog of OSCs natively

Reported by: osc, apply-core.

Mechanism: apply-changes takes one OSC and parses it serially on the calling
thread (one tokenizer, its inflate overlapped on one helper thread) before
any pipeline thread starts. For a
single daily planet OSC that does not pay: ingest ended at 4.8 s of 203.7 s
on bygg (e765fb59, 67bcd55c, zlib output) and 5.3 of 97.9 s on a zstd:1 cell
(3608960f). Parallel ingest and `CompactSink` staging reuse were declined on
those numbers, with comments at `merge_to` and `CompactSink`. The residue is
catch-up: a merged multi-day OSC keeps every change (26.26 M for planet
4914..4920 unsimplified), and apply reparses all of it through one tokenizer
after merge-changes rendered and compressed it.

Redesign: apply-changes takes an ordered list of OSCs, reads them with
merge-changes' concurrent per-input readers and lexical batches, replays the
batches with apply semantics into position-keyed records, and sorts and
reduces them straight into `SortedDiff`.

First step: measure a valid chain (base at 4913, OSCs 4914..4920, default
unsimplified merge then one apply): merge-plus-apply wall, apply ingest wall,
peak anon during ingest and across the run. No measurement exists yet.

Constraints: reduce by kind and ID in source order (input ordinal, position),
carrying the version floor as well as the winner - modify v9 then a claimless
delete must still refuse against base v10, which merge --simplify then apply
loses; batch-local winners must not hide a cross-batch regression (v8, then
v7 and v9 in the next batch). Validate before reducing, hidden upserts as
deletes with their children checked, since a superseded malformed change still
refuses; decide which of competing errors is reported. Match ordered merge
followed by one apply, not successive applies (v8 then v11 passes against base
v10 combined, v8 alone refuses), and document that. Count retained payloads,
sort descriptors, scratch and the frozen set at their overlap for peak memory.

Kind: capability with an unmeasured payoff (catch-up).

## PERF-037 - merge-changes --simplify holds a 64-byte descriptor per change

Reported by: osc, merge. Narrowed 2026-11-26.

Residue: each change carries a 40-byte `Element` record (`usize` offsets) and,
during dedupe, a 24-byte sort key: about 1.7 GB at the planet week's 26.3 M
changes, beside the rendered XML. A packed descriptor (u32 offsets within the
batch, the key derived instead of copied) would cut resident data with mostly
unique changes. Measured 2026-11-26 (bygg, `876d9dd7`, `--alloc` cell
`6325ad69`): planet 4914..4920 simplified peaks at 7.3 GB RSS in 4.0 s, so
the descriptors are about a quarter of the peak and a packed form would save
about 1 GB. Memory only, on a command that takes seconds; build it when a
host's memory makes the 7.3 GB matter.

Declined, with reasons at the `simplify.rs` module doc and the cells in
reference/performance/merge-changes.md: streaming chunks to the output in
order (planet .osc.gz neutral), and rendering only survivors from compact
reduced records (3.2 % of elements superseded on the one measured workload;
bounded memory would need scratch, which --simplify refuses).

Kind: memory, unmeasured payoff.

## PERF-041 - Changed OSC output revalidates blocks and unindexed diff still decodes serially

Reported by: decode, osc, merge.

Indexed diff and derive now compare borrowed views and keep one cursor per
side across block pairs. See the [indexed comparison measurements](../reference/performance/diff.md#borrowed-comparison-views-for-indexed-diff-and-derive-bygg-b4e65632).

The first full-element read for changed OSC output copies and revalidates
the block payload. Its forward cursor walks the prefix before a clipped
window to restore dense metadata deltas. Reuse the validated bytes for
full reads while preserving the dense delta state.

Unindexed `StreamingBlocks` still decompresses, compares full elements and
emits on one thread. The diff changes adapt its sink actions and leave that
execution model in place. Parallel decode and comparison planning over
actual ranges remain open. The production side overlaps PERF-076.

## PERF-042 - Tags-count merges accepted rows through shared shards

Reported by: tags, decode.

After block-local selection and tallying, workers still merge accepted
rows into shared mutex-protected shards. Partition-owner counting remains
an unmeasured option.

## PERF-046 - Location checking still rereads ways across coordinate partitions

Reported by: check.

The compressed coordinate store is implemented. On bygg, planet
`altw-keep-untagged` went from 6 partitions at HEAD `c0ec89ca` to 4 in the
working tree on `c0ec89ca`, with about 16.1 GB of encoded store per
partition and about 15 % lower wall time.
See the [check performance record](../reference/performance/check.md).

Every partition still decompresses every way blob and scans every ref.
Later partitions skip coordinate decoding for ways they do not own.
Whole-blob skipping and spilling observations to scratch were already
ruled out in the check performance record.

A single partition on this host needs denser exact storage within the
same budget. A node-side lookup could instead read the way blobs once,
using node blobs as backing storage, a bounded cache of decoded coordinate
projections and a rank-indexed mismatch bitset across batches. Measure
node read amplification for that design. Preserve duplicate precedence,
exact first-coordinate or disagreement state for absent IDs under
`--agreement`, violation order and the reported partition and blob-decode
counts.

Kind: scaling payoff (check).

## PERF-050 - Renumber lookups are scalar and touch separate arrays

Reported by: sort.

Pass 1 and stage 2d already run at host width with a shared way set.
See the [host-width measurements](../reference/performance/renumber.md#host-sized-rewrite-stages-planet-baseline-c73f2a73-bygg-2026-11-26).

Mechanism: lookup reads a bitmap, separately allocated prefix arrays and
popcounts up to seven words (`rank_if_set` already combines membership and
directory); the way rewriter resolves each ref right after decoding it and
encodes before the next, a dependent chain.

Redesign: an interleaved `(membership_bits, rank_before_word)` pair per word so
one cache line and a masked popcount answer; small upper directory; sparse and
full pages specialized; rewrite in bounded batches (decode old IDs, issue
independent lookups, compute, delta-encode); optional per-worker line cache.
Local: flatten wire-patch assembly (dense payloads copied through
`other_fields`, element, group, block); decode relation member arrays once in
lockstep; collect relation IDs in parallel with node work instead of a
sequential R1 tail; sort keys as compact records instead of `Element` views.

Risks: roughly double bitmap storage; cache and TLB footprint; batch scratch;
orphan detection and per-kind shift and overflow for missing refs.

Second consumer: sparse add-locations-to-ways pass 2 (`SparseArrayIndex::get`,
`numeric_rank_if_set` per way ref) shares the `RankedIdSet` layout, so judge
the layout for it too: resident-store CPU (rank sits on the dependency chain
of every coordinate load while the store is cached, e.g. japan) separately
from near-residency page traffic (a larger index can evict store pages; past
residency pass 2 is fault-bound). This consumer was never measured with the
interleaved layout.

Kind: strong hypothesis.

Measurement on bygg, HEAD `b25180b3` against the working tree on
`b25180b3`: on planet `id-remap`, batching 32 lookups was neutral and
enabling the interleaved layout moved wall time by roughly 2-3 %, within
noise. In `c9509d73`, lookups took 196.6 ms of 397.6 ms sampled rewrite
time over 18 blobs, with a 23.6 s stage 2d span. Lookup accounts for about
half of sampled rewrite time, but stage 2d may be bound by work neither
change touches. The prototype did not land. See the
[renumber measurements](../reference/performance/renumber.md#batched-lookups-and-interleaved-ranks-neutral-bygg-baseline-b25180b3).

To resurface: the code is in
`notes/stashed-perf-050-renumber-batched-lookups.patch` (made against
`b25180b3` on bygg). It edits `renumber/` and `idset.rs`. Restore the
omitted tunables `RENUMBER_REF_BATCH = 32`,
`RENUMBER_INTERLEAVED_RANKS = false` (the bool switch), and
`RENUMBER_LOOKUP_SAMPLE_STRIDE = 1024`.

## PERF-053 - Geocode admin lookup reconstructs polygons per query

Reported by: geocode.

A containment test on a boundary (unflagged) entry still allocates and
rebuilds all of the polygon's rings from mmap coordinates (plus an unwrapped
copy at the antimeridian) and scans every vertex. A prepared per-cell edge
index would use the existing crossing expression on the edges of each polygon
that cross the cell. The query's ray also crosses edges outside the cell, so
each cell entry would carry a base parity per ring and per longitude retry.
This would change the index format.

Risks: holes, even-odd self-intersections, implicit closing edges, antimeridian
retries, simplification, stored f32 area, polygon-index ties, and the cover
rules at `AdminEntry`.

Kind: highest-conviction query rewrite.

## PERF-054 - Geocode build still has a serial pass 2 consumer and a largest-bucket sort

Reported by: geocode, pipeline.

Pass 3 stage B now splits fine and coarse buckets at cell boundaries,
encodes in parallel and writes at disjoint prefix offsets. The largest
bucket still sorts on one core. Splitting that sort by record volume needs
to keep each cell together and preserve the order used to break ties.
See the [stage B measurements](../reference/performance/geocode.md#pass-3-stage-b-encodes-and-writes-cell-ranges-in-parallel-bygg-b4e65632).

Pass 2 parallelizes node processing and way resolution, but one consumer
still interns strings, encodes records, assigns offsets, writes geometry,
collects interpolation metadata and inserts admin geometry. Workers could
emit fragments keyed by source position, remap local string offsets and
assign contiguous IDs and offsets with prefix sums. A retry needs to
preserve equal-distance tie order, duplicate admin-way replacement and
interpolation identities and string equality.

## PERF-055 - Geocode street lookup chases references across source-ordered files

Reported by: geocode.

Mechanism: lookup goes cell table -> segment reference list -> way record ->
coordinate file per candidate, geometry in source way order; segments examined
through several cells; fine and coarse indexes duplicate references; search
walks empty grid cells.

Redesign: one bulk-loaded page-packed hierarchy over address points and
original street and interpolation segments, leaves holding coordinates, internal
nodes with conservative bounds and class presence, best-first search with
per-class best distances; ranked lookup needs one search within
`max(fine_radius, coarse_radius)` (true even if coarse is smaller), so one
physical index suffices; raw candidate API keeps its per-class rules via compact
occurrence metadata outside the hot path. Target: small resident directory,
page-sized leaves, local strings, compact admin refs, overflow handling; one or
two data pages for ordinary queries.

Risks: conservative bounds for the latitude-cosine metric, poles and periodic
longitude; keep original endpoints; equal-distance ties; raw API exposes
duplicates, ordering and interpolation indices.

Kind: larger end-to-end rewrite.

## PERF-056 - Geocode build should consume resolved ALTW coordinates

Reported by: geocode.

Mechanism: the indexed build reads relations, ways (writing a compact store),
nodes, then resolves ways from scratch; about one full payload decode plus a
separate whole-file digest read; unindexed classification adds decoding. The
rank-indexed join resembles sparse ALTW.

Redesign: consume resolved way batches and produce spatial fragments
immediately; for bounded memory adapt the external ID-bucket join and slot
reordering, stopping before PBF encoding. Do not materialize an ALTW PBF.

Risks: geocode needs addresses from every node blob; straddling blobs must not
duplicate address extraction; geocode rejects conflicting duplicate
coordinates (sparse does not compare); geocode drops missing coordinates and
tracks interpolation completeness (zero substitution changes results); external
requires sorted indexed input.

Kind: reuse join algorithms; generalize later.

## PERF-057 - Geocode local changes

Reported by: geocode. Narrowed 2026-11-26: format 10 persists the
interpolation prefixes (germany query -12 %), pass 2 classifies address
keys once per string entry, and the CLI answers in ordered parallel
batches (germany CLI call 3.9 s to 1.1 s; reference/performance/
geocode.md).

Residue: fusing dense validation with decode and address extraction is not
done. It needs a validator projection in `read/`, and the build wall did
not move with the per-entry classification alone (germany 6.2 s both
arms), so the payoff is unlikely to be large.

## PERF-059 - Read-side local changes

Reported by: framing, pipeline. Narrowed 2026-11-26 to the two bullets no
measurement covers; both unmeasured.

- Raw (uncompressed) blobs are copied into a mutable `Vec`
  (`decompress_parsed_blob_body`, `decompress_blob_raw` Raw arms) because
  block construction appends its lookup tables to the payload. The
  table-append measurement (`read-pipeline.md`, "Cost of appending the offset
  tables") prices the append, not this copy, so it does not settle it. No
  registered dataset is uncompressed; a measurement first needs one
  (`repack --compression none`, promoted as a snapshot).
- The shared `Mutex` `DecompressPool` takes its lock on checkout and on
  return for every pooled decode, including fused projections that finish
  with the buffer on the worker. The escaping lifetime is real for `Blocks`
  and the owned iterator (the `Bytes` keeps the allocation and the pool's
  permit alive past the worker), not for every projection; a worker-local
  buffer needs a borrowing transform contract first. The 2026-03 pool revert
  (`a6ebbfe5`) was a different pool and gives no number for this one.

## PERF-061 - Copy-shaped local changes

Reported by: copy, osc, merge. Narrowed 2026-11-26: plain cat's unindexed
scan now runs on the frame pool, the dedupe sweep's loser validation stages
no members, the interner and tags-count kind-filter proposals were declined
at their code sites, and the `CompactSink` staging item joined PERF-036.
Further narrowed 2026-11-26: forced `degrade --drop-ids` selects in one
`KindIds` pass (denmark forced -20 %, reference/performance/
repack-degrade.md), the OSC writers and the text diff sink no longer
allocate or copy per value or piece (planet diff neutral,
reference/performance/diff.md), and dedupe winners stream relation members
(denmark neutral). What remains is unmeasured.

- Simplified OSC (`merge_changes/simplify.rs`) collects every chunk in
  `all_chunks` before the drain; for plain `.osc` that is a second XML copy of
  every survivor. An ordered windowed drain was measured on `.osc.gz` and is
  wall- and peak-neutral there (reference/performance/merge-changes.md,
  2026-11-26), so only the plain `.osc` case remains: a memory win there,
  now measurable with `merge-diffs --output-name merged.osc`, is the sole
  reason left to carry it.
- The serial diff planner reads both payload streams on one thread; the
  denmark common-output record already shows planner and head delays, with no
  evidence it limits planet today.

# pbfhogg-verify (2026-11-21 hunt)

Findings from the 2026-11-21 verifier performance hunt, prompted by `verify
all` on denmark growing from about 3 minutes to 12-20. Eight hunters, two per
scope (Opus at high effort as a harness subagent, and codex gpt-6.2-sol at high
effort), reading code at `e8377a78` only. Nothing was built, run or measured:
every count and payoff is a source-derived hypothesis, and where the hunters'
counts differ, the entry gives both. "Reported by" names scope and model and
states a fact about the reports, not a verdict. Defects found on the way are in
`notes/bugs-tooling.md`.

Scope names: orchestration (main, all, run, selection, harness, process,
stream, reference, reference_cache), engine (opl_compare, verify_opl, the diff
and listing readers, pbf_frames, osc, verify_reencode, verify_input_order,
verify_extsort, counts), heavy (degrade, add-locations, apply-changes, sort,
repack, cat, cat-dedupe, renumber, merge-changes, field-policies), other
(geocode, extract, multi-extract, check-ids, check-refs, getid, getparents,
diff, derive-changes, tags-filter, inspect-tags).

## PERF-064 - OPL decodings are reused within a run, but still serial, file-materialized and per policy

Reported by: all eight (orchestration, engine, heavy and other, both models).

Landed: a run-owned decoding store (`opl_compare::Decodings`, owned by
`run::Run`, files in `RUN/decodings/`) keyed by the file's identity stamp
(dev, ino, size, mtime) plus its name from the first dot, and the policy.
Every comparison, order scan, counts call, reencode reference and
apply-changes model read takes its decoding from it; order and counts are
recorded per source once any decoding of it was read whole for its keys.
Idle decodings are bounded by `DECODING_BUDGET` with LRU eviction; failed
calls keep theirs. reference/verify.md "Decoding reuse" is the contract.
Measured once on plantasjen, denmark `verify cat-dedupe` PASS: 11 osmium cat
runs (the hunt counted 13-16 at e3782a4f), wall 3m5s; no matched baseline
on this host.

What remains:

- A call's missing decodes now run together as one launch group, but they
  are still written to files and read back, never streamed (`-o -`): a pipe
  needs the launch group to hand out stdout pipes and the merge to read
  while the group runs, which barrier settlement forbids (a settled
  ruling of the launch-group work: no member output is read before its
  whole group settles).
- Across runs only summaries are reused, not decodings (PERF-065).
- One decoding per policy: NO_LOCATIONS and CONTENT are never projected
  from FULL, since no field-policies control holds such a projection. A
  file compared under two policies decodes twice. cat --dedupe's input
  (CONTENT order scan, then the merge policy) stays that way deliberately:
  the saving is one cold decode, under the noise floor, and a CONTENT scan
  keeps reporting an order fault before a metadata the OPL writer refuses;
  the reasoning is at the order gate in checks/cat_dedupe.rs.
- Byte-identical files at distinct inodes decode apart (the stamp key was
  chosen over a content digest: the encoding fixture's fake osmium answers
  by name over byte-identical PBFs, and no fresh output is hashed).
- The budget bounds retained idle bytes, not scratch headroom for the decode
  in flight or for producers.

Redesigns (as filed, for the residue):

- orchestration-opus: a run-wide, single-flight `Decodings` service keyed by
  osmium identity, policy and content. It streams `osmium cat -o -`, derives
  order, counts and first/last key once per decoding, and stores zstd OPL
  chunks with per-chunk summaries (about 1 GB for denmark FULL).
  - Blob granularity: chunks are keyed by (osmium identity, format, header
    features, Blob payload digest). Synthetic PBFs of the missing blobs are
    decoded and the output split per blob by `pbf_frames::blob_ids` counts,
    refusing the split if first/last keys disagree. Passthrough outputs then
    decode only their rewritten blobs, and the merge skips whole chunks whose
    stored digests match.
  - Proposed sequencing: local fixes first, then an in-run file-granular memo
    plus the fixture store, then streaming plus the concurrent launcher, and
    blob granularity plus persistence last.
- engine-opus: one streamed decoding per distinct content digest, always at
  FULL (`opl,locations_on_ways=true`).
  - NO_LOCATIONS and CONTENT become field-level projections of the FULL line.
  - A `Summary` is folded while streaming: per-kind counts, the strict-order
    verdict with its first violation (never an early error), XXH3-128 stream
    digests per policy, and the header facts. Hashing is chunk-parallel on
    worker threads.
  - A comparison is decided from the summaries when both sides are ordered and
    the digests are equal. Otherwise it runs a concurrent streamed merge that
    writes today's listings unchanged.
  - Record adapters replace the files written only to be read back
    (apply-changes expected, derive omission model, kind filters).
  - API sketch: `Engine { identity, summary, records, compare }`,
    `Summary { counts, order, digest {full, no_locations, content},
    locations_declared }`.
- heavy-opus: plan-then-execute inside each check. Producers run first. The
  check declares its decodings as (file, policy), memoized by content digest,
  and its relations: sequence, strict, against_expected, order and counts. An
  executor starts one piped `osmium cat` per distinct decoding (pipe enlarged
  with `F_SETPIPE_SZ`) and broadcasts line-aligned `Arc<[u8]>` chunks over
  bounded channels to consumer threads that run today's walk bodies unchanged.
  The wait graph is acyclic. Measured against this design, degrade goes 14 to
  7 decodes, add-locations to 3, and cat --dedupe to one pass of the input plus
  6 outputs.
- other-opus: a `Decoded` handle per (content, policy), with keys, counts and
  order as by-products, and an N-way merge that writes every pair's listing
  byte-identical to today. derive-changes becomes 4 decodes instead of 10,
  extract 6 per region instead of 16 (with `present_members` turned into a
  sorted intersection), getid-removeid 4 instead of 9 full-size, and getparents
  16 instead of 32. Order becomes a recorded verdict that keeps scanning, after
  which the same error is raised.
- codex-orchestration: one decoded record source per immutable PBF. It feeds
  order, duplicates, counts and comparisons from one ordered scan, streams
  single-use outputs into their judges, and keeps a replayable block-oriented
  form for high-fan-out artifacts (base, updated PBF, roundtrips, locations
  reference). The decoder is libosmium-backed: it first emits the OPL
  projection through bounded streams, then structured records once equivalence
  is shown.
- codex-engine: libosmium as the semantic decoder through a thin batch adapter
  over `osmium cat`'s reader. It produces compact batches holding identity,
  source position, normalized metadata, coordinates, ordered tags, refs and
  members, with key-only projections for counts. Two batch cursors accumulate
  order and counts and deliver typed pairs to the judges, and OPL is generated
  only for diagnostics. It copies the architecture of pbfhogg's
  `into_blocks_pipelined` and its fused transforms without linking them. It
  also sorts compact identity indexes for comparison-only sorting, and turns
  repack's stable kind partition into a projection instead of 3 filtered cats
  plus a join.

The hunters disagree in four places:

- **Wire format.** engine-opus keeps OPL as the wire and calls a
  verifier-owned decoder a later two-tier option at most. Both codex hunters
  propose a libosmium-backed typed decoder.
- **Projections from FULL.** engine-opus derives NO_LOCATIONS and CONTENT from
  FULL by projection, proved once on denmark and kept as a field-policies
  control. heavy-opus (its L9) says to decode once per (file, policy) without
  projection until field-policies gains that control.
- **Equality by hash.** engine-opus and orchestration-opus accept a PASS
  decided by XXH3-128 digest equality, noting the cache already trusts that
  hash, with BLAKE3 or comparing chunk digests of the decoded text as the
  conservative alternatives. heavy-opus would rather cache compressed text
  than hash streams.
- **pbfhogg's library readers.** Every Opus hunter and codex-engine reject the
  library as a decoder, because the crate links no pbfhogg and a shared
  decoder bug would hide. codex-orchestration and codex-heavy cite
  `ElementReader` for pbfhogg-native observations only.

Payoff: heavy-opus expects about 2x on the heavy checks from the local
removals alone and 3-5x overall. other-opus expects 4-8x on getid-removeid and
derive-changes. engine-opus says that if decode and compare are at least half
the run, they shrink by 3-4x. Every redesign also removes tens of GB of scratch
writes per run, a recorded wall-time hazard on these drives.

Risks:

- One shared decoding feeds many checks.
- Per-blob decode equivalence under a synthetic header needs its own control.
- Header-dependent decoding.
- Specific strictness diagnostics and byte-identical listings must survive.
- The storage budget on germany.
- Hash-equality trust.
- Streaming loses the "OPL pair kept on error" property; re-decode on the
  failure path.
- CPU oversubscription from several osmium pools (`OSMIUM_POOL_THREADS`).
- Lifecycle concurrency (launch groups settle at a barrier; see
  reference/verify.md "Processes").
- An extra FULL decode for files only ever compared under CONTENT.
- renumber's attribution needs paired events even for equal records, and
  degrade needs ordered removal events, so the merge must be exposed, not only
  its verdict (codex-engine, codex-heavy).

Kind: high-conviction rewrite (all eight).

## PERF-065 - Decoding summaries leave some comparisons merging every run

Reported by: orchestration-opus, engine-opus, heavy-opus, other-opus,
orchestration-codex.

Landed: cross-run decoding summaries (ordered, counts, XXH3-128 of the OPL
stream) in the reference cache, keyed by the decode's reference key; strict
comparisons and order scans decide from them (reference/verify.md "Decoding
summaries"). Denmark `sort` on plantasjen, A = 16e70d27, B = the change
uncommitted, PASS-line check wall: A 18.4 / 18.8 / 18.5 s; B warm 0.73 /
0.72 / 0.71 s; B cold (summaries removed) 18.9 / 18.7 s, within noise of A
once the digest moved under the read buffer (per-line hashing cost about
1.6 s). `cat` 22.7 s cold to 0.69 s warm.

Residue:

- Partly tested end to end: the apply, dedupe, extract, getid, read and
  refs fixtures run with summaries live and do hit stored summaries
  (observed by temporary instrumentation, 2026-11-24), but no test asserts
  which decided path ran, and whether the one-hit scan is reached was not
  traced. The encoding and diff fixtures run fresh.
- The one-hit path (pbfhogg output bytes changed, reference summary stored)
  is unmeasured: denmark sort and cat outputs were byte-stable.
- `compare_with_opl` (the derive-changes expected model) and the derive
  model always decode and merge. Repack, degrade and renumber decide warm
  runs from stored readings, but a cold or changed output still decodes
  and merges both sides.
- Renumber's canonical readings are decided only when both are stored:
  with one output changed, both sides are decoded and walked again,
  though the unchanged side's stored reading could stand in for its
  decode (the changed side would still need a full reading). Building
  both readings during a cold walk costs about 4.8 s on denmark
  (reference/performance/verifier.md). No test reaches the hit path: renumber
  is driven only by the diff fixture, which runs fresh.
- A fresh decode of a missing side is still a full file-backed decode; the
  shortcut saves the other side and the merge only.

Kind: orchestration-owned machinery.

## PERF-067 - Reference cache retention is by recency, not recompute cost

Reported by: orchestration-opus, other-opus.

Residue after the per-run digest memo, once-per-run tool probes,
stamp-trusted lookups and real hit timing landed (the publication fsync
stays, with its reason at the code site).

`retain` keeps the most recently used entries that fit the byte budget
(default 20 GiB). Denmark's working set (about 4-5 GB) fits; germany's (about
40-45 GB) never does, and a germany run pushes older denmark entries out. A
cost-only order pins stale entries forever; GreedyDual-Size (priority = an
aging floor plus recompute wall per byte, wall recorded in the manifest) is
the candidate shape. other-opus attributes geocode's 2.5-4.4 minute denmark
spread to LRU loss of its four osmium artifacts; nothing recorded supports
or refutes that.

Blocked on measurement: deciding needs a germany run and a repeated denmark
`verify all` with the per-step hit times now printed. Note that `retain` caps bytes only after a run; no
ranking caps scratch during one.

Kind: local, medium.

## PERF-069 - OSC parsing and check-ids unsort runs remain separate

Reported by: orchestration-opus, orchestration-codex, heavy-codex, other-opus,
other-codex.

Plain apply, apply -l, merge-changes and tags-filter --input-kind osc still
parse the OSC separately. Check-ids still produces its unsort fixtures in
separate runs. The cross-blob case uses explicit cap and force arguments,
and the intra-blob case starts from stripped input and needs distinct
framing. The shared producer results are recorded in
[verifier performance](../reference/performance/verifier.md).

## PERF-070 - degrade and repack still decode and merge on cold runs

Reported by: heavy-opus, heavy-codex, engine-codex. Narrowed 2026-11-22.

Landed: every warm case, drop and unsort included, is decided from the
output's stored summary against a stream predicted from the expected file
(reference/verify.md "Decoding summaries"); warm denmark degrade 1m55s to
1m13-1m17s (reference/performance/verifier.md). Since 2026-11-26 the
warm drop case inflates once, `partition` reads once and reuses grouped
input, the stripped expected file is built once per run, and the layout
predictors walk the claims once (warm denmark degrade 56.4 s to 41 s,
repack 32.5 s to 22.2 s).

Residue:

- A cold whole-stream equality case now decides from the summary scan
  (cold repack 54.8 s to 48.4 s), but cold drop and unsort cases still
  merge, since their judges need the removal or swap listing; cold degrade
  stays at about 71 s, 27.8 s of it osmium `cat`. Streaming that decode
  is PERF-064.
- No test reaches the summary-decided sequence walk end to end: repack and
  degrade are driven only by the encoding fixture, which runs fresh
  because its fake osmium decodes by file name, and its 3-object input
  runs no drop or unsort case. Unit tests hold
  the predicted listing, counts and digest to the merge's.

## PERF-076 - diff and derive-changes prepare each input pair repeatedly

Reported by: other-codex, other-opus, orchestration-codex, engine-codex.

Narrowed 2026-11-21 on the verifier side. Landed elsewhere since filing:
the order checks are folded into the first derive pair, each file is
decoded once per run (the in-run store), warm runs decide all four derive
pairs from summaries with no decode or merge, and the pbfhogg runs of both
checks run in launch groups (derive-changes' two derives together; warm
denmark B A A B 17.5 / 18.3 / 18.3 / 17.3 s in
reference/performance/verifier.md). Warm, derive-changes' cost is its
unindexed derive (about 15 s of the 17 s); the indexed one takes a second
or two.

Residue, verifier side (cold only, unmeasured): a 4-way walk over `new`
and the three roundtrips would save three of the four merge read passes,
and on a host where the store evicts idle decodings under
`DECODING_BUDGET` (germany scale) also repeat decodes, at the price of
four live OPL files at once. It must keep each pair's error, judged-order
and listing behaviour when a cursor steps down. Streaming the text diff's
old/new merge into the listing judge saves only the write and reread of a
change-set-sized listing. The omission exception from cursors touches a
failure path unreachable on the configured datasets. Take none of these
without a cold phase split showing material cost.

On the production side, each stripped diff arm goes through
`StreamingBlocks::new_sequential`, where the merge itself drives both
decoders.

Redesigns:

- Production side (other-codex): a bounded parallel reader deriving kind,
  count, order and key ranges from the actual payloads (`BlockBuffer::project`,
  `KindIds`, the ordered project workers). Compare byte-identical ranges once
  they are validated and counted, and merge divergent ranges by key. Each
  serializer runs from the prepared comparison. Do not trust indexed counts
  for a stripped blob, and do not derive plain from verbose, which would make
  the projection check tautological.
- engine-codex, local: judge verbose diff refs and members as typed sequences,
  format only the checked items, and index unique tag keys once.

Risks: buffering large divergent ranges; duplicates at range boundaries.

Kind: high-conviction rewrite (other-codex).

## PERF-077 - The verifier's own frame reader is serial and reopens per blob

Reported by: engine-opus, engine-codex, heavy-opus, heavy-codex,
orchestration-opus.

Narrowed 2026-11-26: `index_truth` reads through one open file and
inflates on a worker pool folding in file order, and header-only walks use
positional reads with `POSIX_FADV_RANDOM` (verifier.md, "Shared expected
file and the pooled frame reader"). Further narrowed 2026-11-26: frame
audits are memoized per file identity within a run and shared by the
header, indexdata, size, layout and unsort checks; dedupe reads IDs and
group shape in one walk with reused buffers.

Residue: codex-engine's reuse of decode results for byte-equal payloads
across related files (an output and its input share most blobs), judging
each occurrence's framing separately. Unmeasured; warm runs no longer
inflate much, so this pays only cold.

Risks: sharing buffers only where both interpretations see the bytes they need;
keep `check --index` and the header-only paths.

Kind: medium local (engine-opus, heavy-opus); high-conviction rewrite
(engine-codex, its second).

## PERF-079 - merge-changes and the OSC listing path sort pointers into text

Reported by: engine-codex, heavy-codex, heavy-opus, engine-opus.

Mechanism: the OSC path reads each file twice, joins the XML metadata scan to
osmium's OPL by position, and writes an escaped listing. merge-changes then
concatenates the listings (again for replay), externally sorts occurrence
descriptors, seeks back into the text one survivor at a time, sorts the
survivors again by action, and builds each fixture's model twice. The
`OscParser::feed` state machine does a `mem::take` per byte and allocates a
`Vec` per tag, and `Utf8Validator::push` copies every 16 KiB chunk.

Redesigns:

- engine-codex and heavy-codex: one decompressed XML stream feeds the strict
  scan and osmium's body decoder, joined by occurrence. Fold histories by
  signed identity, keeping the first claim, high water mark, first regression
  and winner. Emit the survivors as three already-ordered action streams with
  no second sort. Replay reuses the occurrence storage.
- heavy-opus: hold `simplify-j1` to `simplify` with `listing::same_content`
  before its own model comparison. Launch groups let the per-case runs
  overlap.
- engine-opus: `memchr` in text state and in-place UTF-8 validation.

Risks: occurrence order, delete bodies, absent versus empty attributes, the
mixed-timestamp condition, byte parity for jobs.

Kind: rewrite (codex, ranked below PBF); local (opus). Matters only for large
OSC inputs.

## PERF-080 - Extract decoding reuse still has policy and retention costs

Reported by: other-opus, other-codex, engine-opus, orchestration-codex.

On LocationsOnWays inputs the present-member listing uses a separate decode
under the no-locations policy. Multi-extract retains sequential decodings
until each strategy ends, which can exceed the idle decoding budget. The
simple relation-count diagnostic still names osmium fileinfo even though
the count comes from the OPL comparison. The ordered comparison and
sequential reuse measurements are recorded in
[verifier performance](../reference/performance/verifier.md).

## PERF-081 - Getparents and tags-filter run separate selections

Reported by: other-codex, other-opus.

Getparents makes separate requests. Its node request depends on the
selected way, and each request has its own add-self semantics. Sharing
the scan would need to preserve both dependencies.

Tags-filter scans separately for each expression, including its OSC
reference cases. A shared scan would need to preserve each expression's
membership, dependency closure and pruning.

Kind: medium.

## PERF-082 - field-policies decodes the same small base many times

Reported by: heavy-opus, heavy-codex, orchestration-opus.

Mechanism: `judge_policies` decodes the base under FULL, NO_LOCATIONS and
CONTENT inside every control: 52 decodes plus about 100 other osmium launches,
about 150 processes in all. `between` often resolves to a policy just tested on
the same pair. The data-scale cost is acquiring the slice (two cached `osmium
getid` runs over the whole input).

Redesigns:

- Memoize the base's decodings, saving about 40 launches.
- Evaluate each distinct policy once while still calling `Policy::between` in
  both directions.
- Target the sampled IDs through verified frame ranges (heavy-codex).
- orchestration-opus: the verdict depends on no pbfhogg (verifier executable,
  osmium identity and input digest), so it could be cached under that key.
  field-policies need not gate later work, only later verdicts.

Kind: low value.

## PERF-086 - Extract on an indexed history file has no bench input

Reported by: the overlap-component change to sorted simple extract.

Sorted simple extract and multi-extract now read overlapping-range blobs (a
history file's versions straddling a blob cut) as one component task, so an
indexed history input decodes each blob once instead of classifying and
writing per kind. No host registers an indexed history input: bygg's
`planet` snapshot `history2` has `pbf.raw` only, and `region-extract`
defaults to `--variant indexed`. Registering one (a `cat` of history2, then
`pbfhogg-datasets promote --producer cat --as-snapshot`) would let a matched
pair measure the saving and the component-length distribution, including how
often the 8-blob cap sends a kind back to two passes.

Kind: measurement gap; the change is neutral on normal input.

## PERF-087 - Indexed `.osc.gz` derive has no bench cell

Reported by: the change writing indexed OSC creates straight into the output.

`diff-pair --format osc --output-name diff.osc.gz` selects the gzip
route, where unit workers deflate members and framing is glued between them.
That route was last measured by the retired typed `diff-snapshots` cells.
The change that writes creates into the output before the scan ends still
needs an indexed gzip cell.

Kind: measurement gap.
