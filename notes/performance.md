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
effort), one per scope, reading code only. Findings are as reported and have not
been verified; every payoff is a source-derived hypothesis, nothing was
measured. "Reported by" names the scopes that raised a finding and is a fact
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

## PERF-002 - Filtered blob bodies are read before the filter runs

Reported by: framing, pipeline, tags.

Mechanism: in `run_pipeline` the reader reads and allocates the whole body and
parses the envelope; `should_skip_blob` runs later on a worker
(`spawn_decode_task`), so pruning saves decompression but not ingestion,
allocation, dispatch, admission or ordered completion. Single-pass tags-filter
(`with_blob_filter`, the `-R` path) and type-filtered cat
(`cat_type_filtered`, `cat_type_passthrough`) reach this.

Redesign: decide skip, copy or decode from the header before materializing the
payload; the file reader already exposes header/read/skip operations
(`blob.rs`). Skip validates framing and advances; copy keeps a source range;
decode obtains a lease.

Payoff: excluded bodies avoid allocation and userspace copies; physical I/O
savings depend on selection density and readahead.

Risks: the pipeline currently parses envelopes of filtered blobs; moving the
filter earlier must keep required refusals (malformed envelope fields,
truncated bodies) and `--distrust-metadata` behaviour, which needs an explicit
decision about what rejected blobs promise. A seekable envelope validator can
check tags and lengths while skipping payload contents. On generic streams
skipping still drains bytes.

Kind: high-conviction redesign (framing); local change (tags).

## PERF-003 - Source ranges are not a first-class output representation

Reported by: output, framing, copy, altw-sparse, altw-external, apply-core,
merge, sort.

Mechanism: range copy exists (`PbfWriter::write_raw_copy`, blob merge
`CopyRun`), but many paths do not reach it: indexed `cat_passthrough` reads
complete frames and hands buffers to the writer; type-filtered cat reads before
checking kind; header-only degrade reads every payload and `reframe_raw` copies
it again; cat reindex `reframe_with` and `framing.rs` header-only reframing
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
extents (`copy_file_range` for buffered, parallel aligned read and write for
direct). Commands hand ranges and replacement segments instead of reading a
frame to call `write_raw_owned`. Target userspace traffic for indexed cat,
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
iteration that decodes dense metadata content equality ignores.

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
compressor copy to remove.

Payoff: removes work on every genuinely rebuilt node.

Risks: interleaved column writes may behave worse than separate loops; prefix
backpatching can cost as much as the copies; size accounting for outliers;
partial metadata semantics.

Kind: high-conviction encoder rewrite.

## PERF-014 - Repack grow and degrade unsort funnel elements through one central encoder

Reported by: copy, decode, encoding.

Mechanism: `worker_split_blob` encodes blobs with at least `cap` elements on
the worker but turns smaller ones into `Vec<OwnedNode/Way/Relation>`;
`run_kind_phase` feeds every owned element to one central `BlockBuilder`. Owned
conversions allocate tag strings, ref vectors, member roles and user strings
(`owned.rs` read helpers; relation output allocates another member vector).
Degrade unsort (`worker_decode_kind`) sends every selected element owned
because one adjacent pair per kind swaps. OSC already has an arena
representation (`osc/compact.rs`), yet `add_osc_way` / `add_osc_relation`
collect vectors again.

Redesign: after actual per-kind counts, plan output blocks as ordered slices of
retained decoded source buffers (one output block may span inputs); each output
partition goes to a worker that encodes and compresses completely; the
ordered writer receives frames; buffers live until their last slice finishes;
admission bounded by retained bytes; dictionaries translated once per source
table. Degrade's swap becomes part of the partition description. Existing
packing policy (flush a grow run before a shrink blob, keep shrink remainders
separate) can be kept. When sparse tails must be evacuated, use a batch arena
with ranges and dictionary IDs rather than individual allocations.

Payoff: removes per-element ownership allocation and a serial encoder.

Risks: ordering across partitions; a large source pinned for a small residual;
duplicated inflation if tasks share sources; SID translation as the next
bottleneck; partitioning must use actual counts, not unverified claims;
metadata or location handling changes when replacing owned conversions.

Kind: high-conviction rewrite.

2026-11-21 verifier hunt (heavy-codex, other-codex): the same funnel is what
`verify degrade` and `verify check-ids` pay for, since both unsort modes build
dataset-sized fixtures through it although the perturbation touches only the
first two ID runs or the first output boundary per kind. Their proposal: a
small coordinator settles the swap and its boundary records while workers
encode the settled blocks. `--unsort-intra` keeps input boundaries away from
the splice, and `--unsort` needs a prefix-count plan for continuous packing to
the cap. They also report that forced repack decodes each unindexed blob in
all three kind phases; one decode dispatching its actual kinds removes that
while still exercising `--force`.

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

## PERF-016 - cat --dedupe decodes both copies of the same source

Reported by: copy, merge.

Mechanism: merging an input with itself makes corresponding blobs overlap
everywhere, so the sweep reads and decodes both copies and re-encodes
survivors.

Redesign: recognize repeated pinned sources before work; decode an aliased
frame once with its multiplicity and precedence; evaluate duplicate identity
once; copy compatible frames rather than recompress. After proving all
competitors lose and the survivor has the required order and uniqueness, copy
the block through.

Payoff: especially compelling for self-dedupe.

Risks: dropping the repeated argument is wrong, because overlap sweeps remove
equal `(kind, ID, version)` records while isolated blobs keep internal
duplicates, so merging a file with itself can change its duplicates; indexdata
has no uniqueness proof; for different files compressed equality needs a byte
comparison, not a fingerprint.

Kind: high potential for the stated workload.

## PERF-017 - Simple extract classifies then rewrites any kind with overlapping ID ranges

Reported by: pipeline, indexdata, spatial.

Sorted simple extract, single region (`extract/fused.rs`) and multi-extract
(`extract/multi_fused.rs`), reads each blob once only for a kind whose
indexdata ID ranges are all present and pairwise disjoint
(`fused::ranges_disjoint`). One overlapping pair, as in history files, or one
unindexed blob sends the whole kind back to classify, freeze, then reread and
decode every blob. Interval components would narrow that: preclassify only
the blobs whose ranges overlap another, install their IDs, and fuse the rest.
Unmeasured; on a history kind most blobs may overlap, so the payoff depends
on how localized the overlap is.

Kind: low conviction; no history-input measurement yet.

## PERF-019 - Complete-ways and smart extract decode the same payloads repeatedly

Reported by: spatial, framing.

Mechanism: complete-ways classifies every kind then rereads everything for
emission; smart adds another full way sweep for relation-selected ways'
dependencies (`smart.rs` extra-way resolution scans the full schedule to get
refs for `extra_way_ids`). Unsorted extraction `collect_pass1_generic` runs
separate node, way and relation sweeps, each decompressing spatially admitted
data. Extract's `BlobDesc` discards ID ranges so `IdSet::any_in_range` cannot
prune.

Redesign (spatial): read relation topology early (member IDs, needed roles,
type, parent edges, payload); after node classification read ways once
(initial membership, outside-node dependencies, refs and payload for candidate
completion ways and selected ways); resolve direct matches, parent closure and
completion over retained records; emit nodes and retained payloads. Sweeps
become: nodes twice, ways and relations once. Complete-ways and smart need a
node revisit or retained node payload; smart does not need three traversals.
Redesign (framing): decode once to produce node membership, compact way refs
and relation edges, resolve dependencies over those, then reread only data
needed for output. Retain ranges and selection summaries so sparse dependency
sets give sparse emission schedules and empty sets no work.

Payoff: removes repeated decompression and parsing; strongest for small
extracts.

Risks: trading decompression for scratch traffic, less certain for near-world
output; projection size; incomplete projections losing semantics.

Kind: strong architectural opportunity; workload-specific rewrite.

## PERF-020 - Polygon region membership walks segments per point

Reported by: spatial.

Mechanism: `Rings::contains` walks the latitude band's segments with exact
crossing tests for every point inside the envelope; the multi-region grid only
prunes bboxes. Polygon classification falls back to general element iteration
instead of coordinate columns.

Redesign: an adaptive spatial subdivision with proven region masks in
boundary-free leaves and short unresolved lists of segments in boundary leaves;
bbox regions join the same structure; polygon classification consumes
coordinate columns.

Payoff: point location instead of point-against-edge work; strongest for
complex admin boundaries and many regions.

Risks: boundary cells must conservatively include touching edges and
endpoints; planar parity versus feature-union rules; periodic longitude images;
approximate rasters or simplification break the contract. Ranks after removing
duplicate decode.

Kind: strong hypothesis.

## PERF-021 - Multi-extract writes all destinations from one consumer

Reported by: spatial, pipeline, output.

Mechanism: `write_regions` drains every destination serially from one
aggregate result through synchronous writers; one slow destination stalls
others; admission credit covers an input blob's whole fan-out. `MultiLocal`
flushes every region's builder at each input-block boundary, making small
blocks for sparse matches.

Redesign: per-destination ordered frontiers and write queues under one shared
byte budget and one shared output scheduler (not a full pool per region); pack
output across input blocks.

Payoff: removes artificial coupling between unrelated files and poor
compression from tiny blocks.

Risks: aggregate budget fairness.

Kind: high conviction (as part of PERF-011); local change (output).

## PERF-022 - Export's remaining per-feature work

Reported by: spatial, decode.

Export formats on the decode workers and writes on one ordered consumer
(landed 2026-11-24; numbers in `reference/performance/export.md`). What the
original finding proposed and was not done:

- An escaped string-table cache per block: every key and value occurrence is
  still escaped through serde_json on its own.
- Fusing the walks over a way: `is_area_way` walks the refs, `--bbox` walks
  the locations, then formatting walks them again.
- Expression and tag-key blob pruning was refused, not deferred: the summary
  counts untagged elements of every selected kind before any expression, and
  a blob skipped on its key set would drop them (comment at `ExportBlocks`).

Kind: local, low conviction once the consumer's write path is the bound.

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

## PERF-024 - Export scope differs from what the hunt brief assumed

Reported by: spatial.

The spatial hunter found that this checkout's export writes GeoJSON and GeoJSON
sequences for nodes and ways only, requires LocationsOnWays, ignores
relations, and has no GeoParquet writer; ring assembly serves the geocode
builder, not export. Filed because the brief named GeoParquet and relation
geometry as export hot paths; there was nothing there to assess.

## PERF-025 - Reference membership is inserted by one consumer

Reported by: indexdata, altw-sparse, tags, check.

Sparse ALTW pass 0 is done: workers insert word-batched and atomically over a
claim-sized reservation (planet pass 0 36.5 s to 15.3 s,
`reference/performance/add-locations-to-ways.md`). What remains:

- tags-filter pass 1 and the way-dependency walk still send
  `(id, Vec<refs>)` and one consumer calls `insert` per reference. Default
  queries ran pass 1 at 30 cores (planet `w/highway=primary`, `49cf423f`),
  so not consumer-bound there; invert-match ran pass 1 at 7.5 cores with
  workers parked on result send (`185cdb38`, 2026-11-09, before the signed
  set and two-read closure). Needs a fresh invert-match planet cell and a
  consumer-time counter (tags-filter has none) before building; the
  pass-0 shape (reserve from the split walk's node `max_id`, insert after
  the block is classified, spill uncovered IDs) carries over.
- Under `--inject-prepass` the reservation factor is halved across the two
  sets, so a dense extract leaves its top chunks to the serial union (Japan
  spilled 97 M of 323 M refs, 617 ms serial). Raising it is a memory
  envelope decision, not a tuning one.
- `check --refs` `replay_nodes` does one `try_insert_atomic` per node on
  the workers (not a serial consumer); node IDs ascend within a blob, so
  `AtomicWordBatch` applies as in renumber. Unmeasured; check refs planet
  is 20 s with the consumer waiting on workers.

Chunk-partitioned owners were considered for pass 0 and not built:
atomic insertion over a bounded reservation removed the serial union with
no second channel layer; owners stay the answer only if a sparse-ID input
needs occupancy-driven chunks with parallel insertion.

Kind: high conviction.

## PERF-027 - Sparse ALTW collects references even when an all-node store fits

Reported by: altw-sparse.

Mechanism: sparse decompresses ways, decodes refs, builds and freezes the
referenced set before nodes, then decompresses ways again for output; the first
traversal exists only to save coordinate storage.

Redesign: when an all-node store fits comfortably, build an immutable
coordinate dictionary from node ingestion (ID-range segments with presence and
compact coordinates, populated by workers without a global owner, small
directory); execution becomes relation deps, node ingest and output, way lookup
and reframe, relation copy; ways decoded once.

Payoff: grows with R; removes a way decompression pass and O(R) accumulation;
costs about 8(N - U) extra bytes.

Risks: node-heavy files favour reference selection; `inject_prepass` still
needs shared-reference data; current sparse checks the geographic domain only
for referenced nodes, so an eager store must store raw pairs and judge on
consumption.

Kind: high conviction in the fitting-memory regime.

## PERF-028 - ALTW routing uses 8N and is non-monotonic under memory pressure

Reported by: altw-sparse.

Mechanism: the router compares an estimated 8N store with 80 % of available
memory minus 3 GiB; unknown memory, or available memory at or below the
allowance, selects sparse. An 8N estimate below the real 8U store (dangling
refs take slots) is now caught by the post-pass-0 recheck, which re-routes
external; an 8N far above 8U (standalone nodes) still routes external with
no correction. Lower memory flips back to the random-access engine. Past
residency each `SparseArrayIndex::get` can be a dependent page fault.

Redesign: regime choice - all-node store fits: actual-node dictionary, skip
collection; only reference-selected fits: sharded collection and compact store;
far over memory: partitioned join with sequential scratch; near the boundary or
skewed: hybrid, resident partitions plus a bounded spill join publishing into
one ordered assembly. Sparse and external converge in lifecycle and assembly,
keeping different physical joins.

Risks: the hybrid is medium-confidence (duplicates, skew, scratch lifetime,
backpressure, keeping resident and spilled results in sync); choosing resident
versus bounded correctly is higher confidence.

Kind: high conviction (routing); medium (hybrid).

## PERF-030 - External ALTW sorts every occurrence by node ID

Reported by: altw-external.

Mechanism: stage 2 reads each request shard, expands packed words into
12-byte `IdRecord`s, comparison-sorts the bucket, decodes node blobs into
`NodeTuple` vectors and merge-walks. Equal-width ID buckets do not bound request
population, so a heavily referenced node can put most references in one bucket
that `prepare_bucket` reserves and sorts at once. Logical scratch traffic is
about 40R + 2P bytes (requests I, 12-byte resolved records, encoded payloads
P); two external permutations are the practical floor for this operating point.

Redesign: decode a partition's nodes into a bounded RAM table (dense for
ordinary ranges, compact rank for sparse; about 438 MB for a 54.7 M-ID range at
8 bytes) and stream requests through it in original way order; no sort, no
whole-bucket vector; populate via the node callback. Preserved order enables
fragment formats: blob identity and count once per fragment, local node IDs as
a column, delta or run encoded destinations, closure flags separate; replies in
fragment order without a 4-byte slot beside every coordinate; framed delta or
bit-packed coordinate blocks. Coordinates with presence and pin fit 8 bytes
(biased latitude plus one in 31 bits, zero for absent). Lower confidence:
dedupe requests by node ID and output group with per-group coordinate
dictionaries.

Payoff: removes the occurrence sort, struct expansion and memory traffic; bounds
memory independent of occurrence count; enables smaller scratch both ways.

Risks: random DRAM gathers replace sorted access; partition sizing by an
explicit budget; domain errors only for referenced coordinates; under
`--inject-prepass` pins need all non-closure occurrence counts first (a counting
pass or extra request read). A radix sort alone captures less.

Kind: coherent join rewrite.

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

## PERF-036 - apply-changes ingest is serial and goes through a hash-indexed overlay

Reported by: osc, apply-core.

Mechanism: apply-changes (`rewrite.rs`) parses one OSC into `CompactDiffOverlay`
then converts it to `SortedDiff`, all serially before parallel PBF work.
Attributes are scanned separately per field; `CompactSink` allocates fresh
staging vectors per element and key and value strings per tag (a key string
even when interning hits); element end allocates another borrowed-pair vector
and copies into the arena; insertion consults the index and version-floor map
and removes from the deleted set; `KindTable::build` extracts, sorts, unzips and
sorts deletes. The command accepts one OSC path, so merge-then-apply goes
through XML, gzip, inflate and parse again.

Redesign (osc): an ingest compiler - stable source positions; workers parse
batches straight into native headers and batch arenas (refs, members, tags as
spans); one pass over each attribute list; partition descriptors by kind and ID
range, integer-key sort, reduce histories in source order with batch-local
summaries; produce an immutable range-partitioned change set (sorted upserts
and tombstones, native metadata with presence bits, coordinate columns,
addressable payloads, shard-local dictionaries, range slicing, node lookup);
apply-changes accepts that frozen set or an ordered list of OSCs. Redesign
(apply-core): keep `SortedDiff`'s sorted access; add a sorted per-kind event
view (ID, action, payload location, winner-selection metadata) so planning
assigns slices once instead of separate upsert, delete and create cursors.

Payoff: parallelizes interpretation, allocation, compaction and conflict
resolution; removes several allocations per element.

Risks: reduction semantics - the winner is last by position, not highest
version; equal versions legal; unversioned changes keep the floor; deletes carry
history; batch summaries must detect cross-batch regressions with the offending
position. Arbitrary OSC needs sorted runs and a final reduction.

Kind: highest priority for ingest (osc).

## PERF-037 - merge-changes --simplify renders every change before choosing winners

Reported by: osc, merge.

Mechanism: `render_batch` parses and formats every change to XML with a
descriptor; `dedupe` builds and sorts another key vector and scans groups
serially for version validation and winner choice; output is split about one
chunk per worker per action, built and optionally compressed, and `all_chunks`
retains everything before draining. Superseded changes are formatted and kept;
winners are copied again.

Redesign: parse and validate into compact records holding what merge-changes
re-emits (create versus modify, delete bodies and optional coordinates, lexical
metadata passthrough); partition by object identity (ordered input epochs per
partition, or radix partitioning descriptors), reducers retain the latest
payload per object, per-input local compaction early; render only survivors;
bounded render and compress chunks streamed to the destination.

Payoff: less resident data even with unique changes, less rendering with many
superseded ones, no serial reduction tail.

Risks: last-by-position, not highest-version; every claim checked in original
order, including discarded payloads (`VersionFloor`); all elements still
validated; input failures take precedence over version-regression failures; the
apply-changes overlay would discard information merge-changes needs.

Kind: high conviction; replace the pipeline.

## PERF-038 - Unsimplified OSC merge replays a lexical transcript

Reported by: osc.

Mechanism: `LexBatch` tokenizes every child, copies start-tag content into
batch text, records a descriptor, and workers replay events into another
representation.

Redesign: immutable decompressed buffers plus whole-element spans and action
context; a light framing pass finds legal boundaries; workers fully parse and
validate.

Risks: framing must understand quoted attributes, comments and XML constructs;
byte-pattern splitting is insufficient.

## PERF-039 - OSC output passes through spill tiers

Reported by: osc, merge, output.

Mechanism: streaming merge always spills members produced for a non-head input
to that input's scratch file and later copies it into the destination, even if
it would fit in memory; promotion occupies the sole coordinator
(`Coordinator::drain`, `advance_head`). Diff derive switches a unit to per-action
spill files when its channel fills; the consumer copies those into three action
fragments; `assemble_osc` copies fragments into the output, so a spilled byte
is written and read twice.

Redesign: merge - a bounded ordered completion window charged for retained
encoded bytes, reserving progress for the head and keeping compression workers
nonblocking; spill only under real pressure. Derive - stream creates into the
staged destination as results arrive; retain or spool modify and delete members
once; append them after creates; reference worker spill extents directly. Gzip
members can be copied without recompression.

Payoff: depends on output volume; large for big change exports.

Risks: a naive shared budget can be exhausted by later inputs; ordering,
cancellation, fragment lifetime; unknown compressed lengths still need ordering
before offsets.

Kind: high conviction about the waste.

## PERF-040 - Gzip inflation is serial inside the OSC reader

Reported by: osc.

Mechanism: `open_osc_reader` inflates gzip synchronously in the reader driving
XML tokenization, so parallel parsing cannot exceed one inflater.

Redesign: decouple inflation from framing; inflate independent files
concurrently in native ingest too. Parallel inflation of one gzip member is a
separate medium-conviction rewrite (DEFLATE bit boundaries, dictionaries,
member and trailer validation).

## PERF-041 - Diff decodes crossing frames per unit and builds full elements to compare

Reported by: decode, osc, merge.

Mechanism: `Frame` shares compressed bytes across units but
`BlockPairSide::decode` decompresses and builds a block per unit, plus a
clipping walk (`clip`, metadata-free) and one positioning walk of the clipped
prefix. After the `blobs_byte_equal` shortcut fails, full elements are built,
dense metadata decoded though equality ignores it, tags and roles resolved per
element. Unindexed `StreamingBlocks` decompresses, merges and emits on one
thread (production side overlaps PERF-076). The prefix replay across merge
rounds is gone: the merge keeps one `ElementCursor` per side across rounds
(2026-11-25); on planet base against `20260411` it measured neutral, so
the per-element construction, not the replay, is what remains.

Redesign: crossing frames share one decoded form with seek checkpoints from
the first scan; validation produces comparison views (IDs, normalized
coordinates, tag boundaries, raw field spans) with metadata lazy and full
elements only for changed output; equality via increasingly expensive proofs
(compressed bytes, decompressed primitive bytes, columns, identical packed
fields with same interpretation, string-table equivalence resolved once,
semantic fallback); decode unindexed blobs in parallel and plan comparisons
over actual ranges.

Payoff: unmeasured. The crossing decode is costed at about one blob per unit
(`DIFF_UNIT_TARGET_BYTES` doc). The node phase runs at about 30 cores on
planet, so the comparison decoder's payoff is per-element CPU.

Risks: any comparison view must stay equal to `content.rs` (digest
`--form content` shares it), including the `Err`-token rule and derive's
visibility; text without `-c` needs versions of equal elements; bounded
shared decoded frames, checkpoint state per delta stream, duplicate IDs and
history versions at boundaries (equal IDs pair in occurrence order across
blobs; a held boundary residual), signed ordering, equal SIDs across tables
prove nothing, dense versus plain and noncanonical varints need semantic
fallback, late dense metadata needs prior deltas.

Kind: medium conviction, wider payoff.

## PERF-042 - Tag predicates are evaluated per occurrence on resolved strings

Reported by: tags, decode.

Mechanism: `element_matches` loops expressions then the element's resolved
tags, after both paths collect tags into a temporary vector; common keys and
values are resolved and compared thousands of times per block. Dense tags are
walked three times (PERF-006); classification advances coordinates and output
selection advances metadata and coordinates for rejected nodes. tags-count
already counts SID tuples (`BlobTally`, 256-shard table) but `count` then
`select` puts every tag into the `(kind, key_sid, value_sid)` histogram when any
expression counts pairs, and repeats key selection per value. Geocode's
fixed-key extraction does not use the numeric representation either.

Redesign: per block, cache for each dictionary entry which key and value
matchers accept it and which expressions apply per kind; occurrences combine
integer masks (expression bit in both key and value masks; key-only needs the
key mask; negation complements); cache each distinct matcher only against
entries it needs; selection produces masks and dependency records without
`Element` wrappers. For counting, apply the predicate before counters: SID-
indexed key arrays with a touched list, pair histogram only for accepted pairs,
counters provisional during validation.

Payoff: removes repeated string work on every tagged element; key-only counting
loses a hash lookup per occurrence.

Risks: expression identity (do not cross-match `amenity=school` and
`shop=bakery`); element-level OR, empty expressions, type restrictions,
inversion; dense boundary finding (tag SIDs are bounded by block
validation, so a tally needs no out-of-range handling of its own); key
and pair rows independent, no double counts, duplicate dictionary strings merged
by text. Medium-confidence: partition-owner counting instead of mutex shards.

Kind: high conviction.

## PERF-044 - tags-filter OSC formats every element before deciding to keep it

Reported by: tags, osc.

OSC tags-filter now streams on merge-changes' ordered drain with one gzip
member per batch, so output is neither collected nor deflated serially. What
remains: `ElementBuf` (`merge_changes/emit.rs`) formats an element's
attributes and children as it reads them, before the keep predicate given to
`ByteSink::filtered` decides, so a dropped create or modify pays the
rendering. Redesign: check every element (validation must not be skipped for
a dropped one) but render only survivors. Single-member gzip input remains a
serial inflate.

Kind: unmeasured, moderate conviction.

## PERF-046 - Location checking rereads every way blob per partition

Reported by: check.

Mechanism: `check_locations_inner` discovers node keys, optionally scans ways,
then `run_partition` reads applicable node blobs and every way blob per
partition, so P partitions decode way refs and inline coordinates about P times.

Redesign: decode once, route node records and way-reference observations into
buckets by target node ID, reduce each bucket (the `Store` slot algebra stays),
reduce counters and violations by report-order keys; resident when it fits,
spill when not; partition ownership removes shared atomics.

Risks: scratch records far larger than compressed PBF, bad for one-partition
files; skew; every node record represented; coordinate-domain arithmetic,
duplicate precedence, absent-node agreement and violation order. The report
exposes `partitions` and `blob_decodes`; their old values cannot be kept
honestly under a different algorithm.

Kind: highest scaling payoff (check).

2026-11-21 verifier hunt (heavy-codex): `verify add-locations-to-ways` runs
`check --locations --agreement` on both engine outputs, and apply-changes -l
runs it too, so this cost is paid several times per `verify all`. The
proposal is the same one: project node coordinates and ordered way-reference
observations once, and route compact observations by node ID.

## PERF-047 - Digest builds elements and hashes on one consumer

Reported by: check.

Mechanism: `summarize_block` builds a `PrimitiveBlock`, iterates elements and
writes canonical records into a `Vec<u8>`; plain messages parsed twice, plain
metadata accessed by `check_user` and again, dense tag runs found then read
again, dense metadata reconstructed for content forms that discard it.
`DigestState` hashes every canonical byte on one consumer; content and
content-locations hash the node prefix into both `all` and `nodes`.

Redesign: produce canonical bytes from checked wire projections with
`content.rs` rules authoritative but not `Element`-bound; local: hash the node
prefix once, take `digest128()` non-consumingly, continue that state for all
elements. Medium conviction: assign canonical offsets from length summaries and
have workers compute exact XXH3 stripe contributions and boundary fragments.

Risks: exact record framing and order; combining per-blob digests is invalid;
XXH3 offset, scrambling, short input and final stripe handling.

Kind: high conviction on wasted work; medium on wall-time ceiling.

## PERF-048 - Full ID verification rereads a whole kind after any disorder

Reported by: check.

Mechanism: any order violation makes `verify_ids_full_parallel` reread the
affected kind with a shared atomic set; workers retain full violation vectors
before `merge_kind` applies `max_errors` (`BlobVerifyResult`).

Redesign: the proof pass keeps each blob's observed canonical interval and
internal strictness; only non-strict blobs and overlapping components need
duplicate checking; use observed bounds, not claims or signed comparison. Local:
bound examples at production with exact totals; word-mask membership batches;
request fewer index observations (sortedness and coordinate extents computed
then discarded); narrow inspect getters (timestamp-only enables the full
census).

Kind: medium-priority redesign; local changes.

2026-11-21 verifier hunt (other-codex): `verify check-ids` runs plain and
`--full` on each of four dataset-sized cases, and both execute the same order
proof before `--full` rereads the disordered kinds. Share the ID projection
between the two consumers, keeping validated ID columns for any kind that
needs duplicate replay.

## PERF-049 - Renumber ID sets materialise a 4 MiB bitmap per touched chunk

Reported by: sort.

Narrowed 2026-11-26: the per-node atomic is now one atomic per word run
(`AtomicWordBatch`), pass 1 rewrite worker time -44 % on planet; worker-owned
ranges and parallel rank building were refused (rank builds take
milliseconds, the way-shard merge is gone); see
reference/performance/renumber.md "Word-batched ID set insertion".

Mechanism (residue): the node and way sets reserve up to the file-size bound
(`RENUMBER_PREALLOC_IDS_PER_BYTE`), and a reserved chunk materialises its
whole 4 MiB bit array on its first write. Sparse IDs under the bound can thus
cost up to 8 bitmap bytes per input byte per set (plus one chunk of
rounding), with both sets live in stage 2d; spill-path chunks and directory
and rank storage come on top, and chunks at or above 2^43 are allocated
eagerly. Planet is dense and unaffected.

Redesign: sparse pages for the atomic path (an array-chunk form atomic
writers can fill, or a per-chunk density decision), covering the eager
high-ID path too.

Kind: medium conviction; matters only for sparse-ID inputs.

## PERF-050 - Renumber lookups are scalar and touch separate arrays

Reported by: sort.

Narrowed 2026-11-26: the stage 2d wall was not bound by lookup cost but by
its fixed 6 workers (pass 1 likewise at 4); both stages now run at host
width with a shared way set, planet 126-131 s to 68-80 s
(reference/performance/renumber.md, "Host-sized rewrite stages"). What
remains is per-ref CPU, which matters again now that stage 2d keeps about
28 cores busy; no cell yet attributes lookup time within
`stage2d_reframe_ns` at this width, so the first step is that attribution.

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
residency pass 2 is fault-bound). Baselines to beat before crediting
interleaving: an array chunk's `rank_if_set` runs `binary_search` for
membership and then `partition_point` for rank, where the found index already
is the rank; a bit array's membership load could feed the masked popcount.
Sparse pass 1 is decode-bound and was measured indifferent to rank cost
(reference/performance/add-locations-to-ways.md, rank cursor).

Kind: strong hypothesis.

## PERF-051 - time-filter selects on full decode

Reported by: pipeline, copy.

Mechanism: both paths build a validated `PrimitiveBlock` and select through
`block.elements()`. Element iteration is lazy for plain nodes, ways and
relations (tags, refs and members are not decoded by selection), but dense
iteration advances every coordinate and metadata column and walks
`keys_vals` per node even though selection reads only ID, version,
timestamp and visibility. The history path then walks the block a second
time to encode its winners (the alternative ending no longer re-encodes; it
forks the builder).

Redesign: a selection projection riding on the validator (dense nodes visit
last in validation order, so selection must keep iteration order and raise
its errors only after validation succeeds), handing the encoder the winner
positions directly. Copying unchanged fields instead of re-encoding them is
PERF-007, not this entry.

Kind: hypothesis, unsized. The path is CPU-bound on bygg's planet `history2`
(about 27 cores), but the hotpath cell does not split selection from
encoding inside `select_blob`; validation stays either way.

## PERF-052 - getid with references scans whole kinds

Reported by: copy.

Mechanism: `getid_with_refs` scans all relation blobs for closure (building
edges for unrequested relations), may scan relations again for members,
`collect_way_nodes` scans every way blob for one way, and output filters by
kind only, decompressing every blob of each selected kind. Basic getid's walker
and streaming paths decompress candidates serially. `SelectProbe` and
`ParentProbe` return a Boolean and the command rebuilds the block on a hit.

Redesign: a searchable blob directory from one header walk; frontier traversal
of relations over intersecting blobs, then candidate way blobs; output only
blobs intersecting the final sets; probes record selected spans or dense
positions during validation; broad queries degrade to coalesced scans. Minimal
decode: getid, own IDs then selected content (plus refs and typed members for
dependencies); getparents, refs and member IDs until decided; dense selection,
IDs then the delta prefixes for selected positions. Getparents `--add-self`
should select node blobs by ID range. A reverse index from referenced IDs to
blobs would transform repeated small getparents queries (conditional on reuse).

Risks: validator still checks unrelated arrays and UTF-8; avoiding element
construction must keep refusals; directory reuse needs binding to the exact
file.

Kind: high conviction.

## PERF-053 - Geocode admin lookup reconstructs polygons per query

Reported by: geocode.

Already landed (format 9, ADR-0029): the reader reads only the query's own
admin cell and trusts its interior flag, about 130 to 26 us per query on the
germany box; the dedupe question dissolved (one list repeats no polygon).

Residue, query: a containment test on a boundary (unflagged) entry still
allocates and rebuilds all of the polygon's rings from mmap coordinates (plus
an unwrapped copy at the antimeridian) and scans every vertex. A prepared
per-cell edge index (the edges of each polygon that cross the cell, evaluated
with the existing crossing expression) would make that cost proportional to
the cell's edges, a format change from 9. It needs more than the cell's
edges: the query's ray also crosses edges outside the cell, so each cell
entry would carry a base parity per ring and per longitude retry.

Residue, build: the flood fill (`pass3.rs` `admin_cells_for_polygon`) runs
full containment for seeds and visited cells, one task per polygon; preparing
rings once, refining boundary cells and keeping uniform subtrees would cut it.

Risks: holes, even-odd self-intersections, implicit closing edges, antimeridian
retries, simplification, stored f32 area, polygon-index ties; any change must
keep the format 9 cover rules at `AdminEntry`.

Kind: highest-conviction query rewrite.

## PERF-054 - Geocode build converges on one merge owner

Reported by: geocode, pipeline.

Mechanism: pass 2 parallelizes node processing and way resolution but one
consumer interns strings, encodes records, assigns offsets, writes geometry,
collects interpolation metadata and inserts admin geometry; worker hashing and a
cloned pool reduce but keep this. Stage A materializes chunk-wide assignments,
repartitions, writes both bucket trees and waits at chunk boundaries; stage B
sorts and encodes in parallel but returns whole buckets to one writer; top-byte
cell partitioning gives large regions disproportionate ownership; geometry is
written in source order, reopened and traversed again for cell assignment.

Redesign: workers emit independently writable fragments with logical identity
from source position; local or hash-partitioned interning resolved by parallel
remapping (compare bytes on collision); workers write geometry and generate
spatial records while coordinates are live; prefix sums for contiguous IDs and
offsets; partitions sized by record volume as worker-private runs, each owning
sort, encode and output (pipeline: replace serial offset assignment with
parallel chunks plus relocation; if first-occurrence string layout must stay
byte-identical, record earliest occurrence and assign from that order).

Risks: physical order changes can change equal-distance winners; admin
duplicate-way replacement, interpolation identities, string-offset equality
used in interpolation; relocation or intermediate storage traffic (pipeline:
medium conviction on net payoff).

Kind: strongest build rewrite (geocode).

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

Reported by: geocode.

- Fuse dense validation, decode and address extraction, classifying address keys
  once per string-table entry (pass 2 uses ordinary elements; a projection alone
  still repeats array walks; the ALTW tuple scanner lacks tags and has different
  refusals).
- Persist interpolation total and per-segment prefix lengths at build time
  (an improving candidate triggers a full section scan, then a prefix walk),
  computed in the same accumulation order to keep rounding; stop indexing
  unresolved sections.
- CLI queries run serially, build `serde_json::Value` trees and keep the whole
  report; `Reader` is `Send + Sync`, so ordered parallel batches with direct
  serialization (optionally spatially grouped), preserving order, bytes and
  no-output-on-interrupt.

## PERF-059 - Read-side local changes

Reported by: framing, pipeline.

- `decompress_blob_data_into` calls `WireBlob::parse_slice`, copying the
  input into `Bytes`; `decompress_blob_raw` already parses from `&[u8]`.
  Reached by check-index, cat indexing, reindexing and repair, getid and some
  merge work.
- `DirectPositional::read_exact_at` copies out of aligned scratch;
  `read_aligned_in` returns the slice (used by blob merge `FrameBuf::read`).
- `evict_consumed_prefix` advises `0..offset` after every blob; batch by a
  page-aligned watermark respecting cache policy and lease lifetimes.
- `ReadBuf::read_exact_at` resizes before reading, zeroing bytes `pread`
  overwrites.
- Raw blobs are copied into a `Vec` because block construction appends lookup
  tables to mutable payload; immutable input plus separate metadata would allow
  leases (uncompressed workloads only).
- Remove the read dispatcher's per-blob rayon submission hop.
- `DecodeTransform::apply(&self, ..)` has no worker-local state, so tags-filter
  builds builders per blob.
- The read pipeline reads clocks and updates shared atomics around every send,
  receive and gate acquisition, so its wait totals include fast-path overhead
  (`pipeline.rs` completion, `counters.rs` `Stopwatch`).
- The shared mutex-protected `DecompressPool` serves fused paths that drop the
  buffer locally.

## PERF-060 - Write-side local changes

Reported by: output, encoding, apply-locations, apply-core.

- io_uring `submit_buffer` submits after every write; queue several before
  submitting.
- `parallel_gzip.rs` `compress_one` builds a new `GzEncoder` and output per
  member; reuse deflate state preserving member boundaries, header, CRC and
  trailer.
- Buffered output has no writeback initiated behind the completed frontier;
  overlapping it with compression could shorten the tail, keeping final fsync
  and the publication protocol.
- Per-tag `FxHashSet` insertion for key presence could be a SID-indexed bitset
  or generation array.
- Omit the dense tag stream for entirely tagless groups (the reader accepts an
  empty stream).
- Remove temporary ref and member vectors with iterator or column inputs.
- Bulk varint output only after fixing representation.
- Submit rewritten frame groups through `write_raw_chunks` instead of once per
  frame (the backend still expands chunks into writes).
- Carry refresh hit positions into coordinate encoding; for default framing a
  changed coordinate alters its delta and the next, so intervening packed bytes
  can be kept.
- `AscendingTally` where nonnegative claims permit; drop unused tagdata
  ownership in descriptors.

## PERF-061 - Copy-shaped local changes

Reported by: copy, osc, merge.

- Plain cat's unindexed scan decompresses serially; reindex and repair already
  use the frame pool.
- Degrade's drop selection builds blocks to hash IDs then decodes again; use ID
  projections and bulk selection instead of one global heap.
- OSC interner stores each string in its arena and in a `HashMap<String, u32>`;
  the builder's arena `HashTable` is the slot-index pattern.
- `osc/write.rs` creates temporary numeric strings, start tags and coordinate
  clones per element.
- Reuse parser staging and append helpers (`append_str_attr`,
  `parse_member_into`) in `CompactSink`.
- Stream simplified OSC chunks immediately; text diff `TextUnitSink` copies
  formatted chunk slices (transfer ownership); reuse relation-member scratch in
  the sweep.
- The serial diff planner reads both payload streams on one thread and may be
  the next limit once comparison work shrinks (not claimed dominant today).
- tags-count derives its standalone kind filter only for a single kind.

## PERF-062 - Ring assembly allocates a graph per relation

Reported by: spatial.

Mechanism: `geo::assemble_rings` (node-ID biconnected blocks, `geo/rings.rs`)
builds a node graph and owned rings for every relation. The old backtracking
search over coordinate copies is gone, and its pathological worst cases with
it; what remains is per-relation allocation.

Payoff: small; not a current export throughput item.

## PERF-063 - Geocode Pass 1.5 overflow lists are unbounded per worker

Reported by: the negative-ID geocode change.

Mechanism: `record_referenced_node` (`geocode_index/builder/pass1_5.rs`)
spills every node ref the size-bounded pre-allocation does not cover
(above it, or negative) onto a per-worker `Vec<i64>`. Compaction sorts and
dedups only once a list has doubled, keeps its capacity, and workers can
hold the same IDs; every list lives until the scan joins. A file whose
relevant refs mostly spill (an all-negative file, or IDs far above 64 per
input byte) holds about 8 bytes per spilled ref per worker at peak, and
distinct dangling spills stay in the final set. Sign-neutral: large
positive IDs had the same shape before negatives joined it; ordinary
positive input spills nothing new.

Redesign: bounded batches folded into one shared accumulator, or a
scratch-backed sort and dedupe where the final set also needs a bound.

Payoff: bounded memory on exotic ID layouts; nothing on planet-shaped input.

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

## PERF-069 - Checks rebuild the same fixtures independently

Reported by: orchestration-opus, orchestration-codex, heavy-codex, other-opus,
other-codex.

Mechanism: identical argv on identical inputs is run by several checks:

- `pbfhogg apply-changes INPUT OSC` (buffered) is run by apply-changes (when
  `--direct-io` is off), diff and diff --format osc.
- `degrade --strip-indexdata INPUT` is run by repack, check-ids, diff and
  derive-changes. The same strip of `new` is run by diff and derive-changes.
- check-ids' cross-blob `--unsort` at cap 8000 matches degrade's default cap,
  but the argv differs (`--elements-per-blob 8000 --force`). The intra-blob
  input is stripped and needs distinct framing.
- extract's buffered polygon simple run is identical to multi-extract's
  sequential polygon baseline.
- `reencode::read_input` on the input (fileinfo, header, framing,
  `index_truth`, `check --index`) is run by both repack and degrade.
- The OSC is parsed separately by plain apply, apply -l, merge-changes and
  tags-filter --input-kind osc.

Redesign: a run-level, single-flight memo keyed by (executable digest,
canonical argv with inputs by content digest, I/O mode). It returns the
artifact plus its recorded exit and stderr, kept in-run only because these are
pbfhogg outputs. Each check still judges its own claims and reports its own
failure if a shared prerequisite fails, and the store records which check
built an entry. codex-orchestration counts five full-file producer launches
saved in the buffered default case. Under `--direct-io`, the judged direct
run keeps its own entry.

Risks: stderr that a check judges must be the recorded stderr; never alias an
artifact a check mutates (check-ids `append_empty_claim` mutates only its
copy); suite order should put the consumers of a shared artifact next to each
other.

Kind: structural, medium-large; high confidence (codex-orchestration).

## PERF-070 - degrade and repack still decode and merge on cold runs

Reported by: heavy-opus, heavy-codex, engine-codex. Narrowed 2026-11-22.

Landed: every warm case, drop and unsort included, is decided from the
output's stored summary against a stream predicted from the expected file
(reference/verify.md "Decoding summaries"); warm denmark degrade 1m55s to
1m13-1m17s (reference/performance/verifier.md).

Residue:

- A cold run, or an output whose bytes changed, still decodes and merges
  every case. A cheaper cold path (hash-scan the output decoding, merge
  only on mismatch) is unmeasured.
- A warm drop case inflates its output twice with the verifier's frame
  decoder: once for the survivor candidate, once in `index_truth`.
- `partition` reads the input decoding three times (once per kind) and
  rewrites it whole even when the input is already grouped, and repack and
  degrade each build the same stripped expected file in their own check.
- Each audited output still gets `index_truth`, `check --index`, a claims
  walk and layout work, and the layout predictors walk the input claims
  once per kind per case (the frame-reader half is PERF-077).
- No test reaches the summary-decided sequence walk end to end: repack and
  degrade are driven only by the encoding fixture, which runs fresh
  because its fake osmium decodes by file name, and its 3-object input
  runs no drop or unsort case. Unit tests hold
  the predicted listing, counts and digest to the merge's.

## PERF-074 - sort runs osmium multipass even on already ordered input

Reported by: heavy-codex, orchestration-codex.

Mechanism: the check always runs osmium's multipass sort, which opens the input
once per kind, even when independent decoding proves the input is strictly
ordered, in which case a stable sort cannot permute anything. The multipass
artifact also differs from the simple-sort artifact other checks request (the
simple path writes a sort declaration).

Redesign: on proven order, use the independently read input sequence with the
same projection. On disorder, use an independent stable sort that emits
comparison records directly. Keep type, signed-ID and version ordering,
duplicate rejection and the way-location prediction. Provide a declared PBF
only to the consumers that need one.

Risks: the header alone is insufficient evidence; equivalence to osmium's PBF
write/read normalization (visibility, coordinates) must be established first.

Kind: high-payoff oracle rewrite (heavy-codex).

## PERF-075 - cat and the reencode partition commission extra osmium passes

Reported by: orchestration-codex, engine-codex, heavy-codex.

Mechanism: `cat` asks osmium for three kind-filtered PBFs and decodes each
again. On nonstandard order, `verify_reencode`'s partition path runs three
filtered `osmium cat` passes plus a fourth to join them.

Redesign: one independent base scan supplies the per-kind views and the stable
kind partition. Keep real osmium output where a check observes its header,
roundtrip behaviour or blob packing. A way-only result still needs an order
judgment without its referenced nodes.

Kind: smaller win.

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

Mechanism:

- `pbf_frames::index_truth` walks claims serially, then for each blob calls
  `read_block`, which opens the file, seeks, allocates and inflates with
  `flate2`'s default miniz backend on one thread (about 7,700 opens on
  denmark).
- `blob_ids_and_shape` walks the groups again.
- It runs on the input in degrade and repack and on every repack and degrade
  output, about a dozen whole-file inflates per run (estimate: several seconds
  each).
- Header-only walks use a `BufReader` plus `seek`, so readahead pulls in
  payloads the walk skips.
- Dedupe passthrough allocates buffers per payload pair.

Redesign: read the frames once and inflate on a worker pool, folding in file
order so the first violation stays stable. Copy the shape of pbfhogg's
`read/pipeline.rs` without linking it. Return count, kind, extrema and shape in
one pass and memoize per file digest. Use `pread` plus `POSIX_FADV_RANDOM` for
header walks, as `read/header_walker.rs` does. Use an inflater that is not
pbfhogg's (system zlib or zlib-ng) to keep the decode independent. Fuse with
the claims walks that judge the same output (`output_blobs`, `LayoutCheck`,
`judge_header`). codex-engine adds reusing decode results for byte-equal
payloads across related files while judging each occurrence's framing
separately.

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

## PERF-080 - extract and multi-extract re-ask osmium questions they already answered

Reported by: other-opus, other-codex, engine-opus, orchestration-codex.

Mechanism: per region, extract runs three strategies on each side, with
order-check decodes before each diff. `verify_extract_simple::adjudicate`
decodes complete-ways relations twice, runs `osmium removeid` on osmium's
simple output and diffs it against the original to find which members are
present, then compares pbfhogg simple against complete-ways separately.
multi-extract runs `check_sorted_counted(multi)` before `same_elements`, and
decodes the sequential outputs again for the mixed pass.

Redesign: one ordered comparison of pbfhogg simple, osmium simple and osmium
complete-ways, collecting member IDs as the records pass (the check refuses
an input with a negative ID and an osmium output with a negative ref or
member, so removeid's absolute-value matching never matters). Use
osmium's multi-region config once per strategy where there are several regions
(other-codex). On the production side, share the classifier work across
strategies (see PERF-019). Fuse multi-extract's order check into its merge and
decode each sequential output once.

Kind: rewrite plus local.

## PERF-081 - Selection checks run one tool invocation per request

Reported by: other-codex, other-opus.

Mechanism:

- getparents runs eight selections per tool.
- getid --add-referenced runs its plain and `--remove-tags` arms separately.
- getid --invert takes counts from a fifth full decode (`osmium_counts`).
- check-refs runs two modes, and osmium's `-r` report already holds the
  ways-only count (cached).
- inspect-tags runs one report per kind.
- tags-filter runs one scan per expression.
- The tags-filter OSC case runs four reference cases.
- check-ids reads four dataset-sized cases with osmium and runs plain and
  `--full` reports, which both execute the same order proof.

Redesign: share the decoded relation pass across getparents' W, R, N and M
requests and resolve the first ref of the selected W way first. Share closure
in getid's arms. Take the invert counts from the comparison pass. Do the
check-refs node/way work once. Count inspect-tags in one pass. Route
tags-filter hits per expression in one scan with per-output pruning (a single
OR would lose the way-only check). For check-ids, share the ID projection
between plain and full, and reuse a summary for the stripped input after
proving payload identity.

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

## PERF-083 - Verifier-wide small costs

Reported by: engine-opus, heavy-opus, orchestration-opus, orchestration-codex.

The PASS line's steps now time the verifier's own OPL reading
(`opl merge`, `opl order scan`, `opl derive model`) apart from the decodes.
What remains is the measurement those labels exist for: one denmark `all`,
read for the split between `osmium cat` and the merge, before any redesign of
the comparison path is argued from cost. No baseline
with the 8 KiB OPL readers exists to set against it; the readers are now
1 MiB.

Kind: local.

