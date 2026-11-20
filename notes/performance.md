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

## PERF-017 - Simple extract has a classify then write barrier inside each kind

Reported by: pipeline, indexdata, spatial.

Mechanism: `extract_simple_single_pass` classifies nodes, completes the ID set,
rereads nodes to write, then does the same for ways (another decompression and
block construction) and relations (`simple.rs` node, way and relation phases;
`common.rs` write executor). Multi-extract repeats it. The write schedule is not
restricted to blobs with matches. The dependency is between kinds, not within
one.

Redesign: per kind, one worker task reads, decompresses, validates, selects,
encodes and compresses, returning frames plus compact membership updates;
membership is frozen before the dependent kind; verified raw-copy candidates
verify while the body is present; simple relations need no membership table and
emit directly. `BlockBuffer::project` and `ValidatedBlock::into_block` already
support validate-once.

Payoff: removes a second read and decompression of nearly every visited blob,
tuple allocations and repeated decoding. Simple extract needs one source
traversal on sorted input.

Risks: selection is ID based, so one matching occurrence emits other
occurrences of the same ID; duplicate runs, including across blobs, must be
reconciled at ordered boundaries; dishonest ordering claims and sorted inputs
without homogeneous indexed blocks need care.

Kind: high conviction; the clearest concrete opportunity (pipeline).

Defect hunt 2026-11-17 (extract) found the same two-pass shape and adds that
it falsifies a documented claim: the public API says simple multi-extract
reads the input once (`extract/mod.rs`), but node classification
(`extract/multi.rs`) finishes before a separate node write pass, ways and
relations repeat it, and single-extract simple does the same. Verified raw
copies avoid the second decompression but still reread the frame. The
documentation half is filed as DOC-018 in `notes/bugs-docs.md`.

## PERF-018 - Multi-region extract reruns whole extractions per region

Reported by: spatial.

Mechanism: in `extract_multi_to`, shared multi-region execution exists only
for sorted simple extraction without attribute cleaning; complete-ways, smart
and cleaned simple run the full single-region extraction once per region, so
decompression, classification, reference scanning and dependency resolution
multiply with region count.

Redesign: one engine whose records carry region masks: node masks from one
classification; way masks as the union of referenced node masks; relation masks
from members; parent propagation for complete-ways and smart; smart completion
masks to member ways and nodes then their nodes; emit per region in promised
order. `Membership` (`membership.rs`) already has compressed masks.

Payoff: shared source work becomes nearly independent of region count.

Risks: keep initial spatial membership separate from completion membership or
added elements seed extra relations; parent relations must not inherit
multipolygon completion; overlapping-region memory; parent closure through
cycles.

Kind: high-conviction coherent rewrite.

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

## PERF-022 - Export decodes and formats on one thread

Reported by: spatial, decode.

Mechanism: export calls `ElementReader::for_each`, which reads, decompresses,
builds blocks and runs the callback sequentially; geometry, filtering,
coordinate formatting, JSON escaping, metadata and writing are all serial.
Unemittable relations and unwanted kinds are decoded. Features are built as a
geometry string and a properties string then copied into `FeatureWriter`.
Property serialization allocates a JSON string per key and value and builds
hash sets per feature; metadata is read when disabled; `coord_fmt.rs` uses
general float formatting; `is_area_way` scans refs before separate coordinate
and geometry scans.

Redesign: read block -> worker validates and exports all features into a
reusable chunk -> ordered sink writes (`for_each_fused_block`,
`for_each_fused_buffer` already exist); FeatureCollection chunks carry counts
and the sink handles commas; push type and conservative expression filters into
scheduling via `BlobFilter` and the tags-filter expression logic (export accepts
empty expressions, tags-filter does not); local changes: append escaped text
directly and cache escaped string-table entries per block, skip metadata,
integer decimal emission for e7 coordinates preserving rounding for non-default
framing (`decimicro_*` truncation is not equivalent), fuse area and bbox
summaries with coordinate decode. `text_quote.rs` is not in export's output
path. Switching to `for_each_pipelined` alone leaves the work in one consumer.

Payoff: removes a whole-command serial bottleneck; remaining serial part is
mostly output bandwidth.

Risks: chunks bounded by bytes, feature and property order, prompt stop on
write failure, one very large way holding unbounded text.

Kind: high conviction.

## PERF-023 - Export should consume the ALTW join directly

Reported by: spatial.

Mechanism: export requires inline way locations and currently does no lookup;
producing an ALTW PBF first pays delta encoding, compression, intermediate I/O,
decompression and decoding to cross a command boundary. Extract re-encodes
ways without locations, so extract followed by export loses the prerequisite.

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

Mechanism: sparse ALTW `scan_way_refs` decodes refs into per-way scratch, the
caller copies them into a per-blob `Vec<i64>`, sends it over the result
channel, and one consumer calls `referenced.set()` per reference; under prepass
injection the same consumer detects shared references. tags-filter sends
`(id, Vec<refs>)` and one consumer inserts. Reference checking does one atomic
op per observed ID; dense nearby IDs hit the same words. This is O(total
references) on one core; more decoders only feed it faster. The `Mutex`
receiver is per blob, the insertion loop per reference.

Redesign: decoder workers partition bounded reference batches by node-ID chunk;
each chunk has one owner building its disjoint bitmap part; dense updates as
word masks, sparse as offsets; completed chunks move into the frozen directory
(`IdSet::chunk_of` and consuming merge exist); memory bounded by queued bytes,
not a planet-wide set per worker. Direct checked atomic insertion
(`IdSet` atomic setters, the geocode `pass1_5` collector) is a smaller
alternative where a bounded dense allocation fits.

Payoff: removes a serial pass over billions of references and an expanded
intermediate.

Risks: chunk skew, batching overhead, cache coherence on sparse workloads; no
full ID-space bitmap per worker; header maxima are not safe allocation bounds
and dangling refs can exceed the max node ID; ALTW multiplicity (seen and seen
again) and closed-ring terminal exclusion must survive batch compaction.

Kind: high conviction.

## PERF-026 - Sparse ALTW decodes node blobs twice and builds output after the index

Reported by: altw-sparse.

Mechanism: index construction decodes every node blob through
`extract_node_tuples`; output decodes it again when untagged nodes are
filtered, building `PrimitiveBlock`, converting metadata and rebuilding via
`BlockBuilder`; relation dependencies are collected only after the index.

Redesign: collect relation-member dependencies before node ingestion; decode
each node blob once, feeding the join while selecting retained nodes; compress
and submit node output immediately (on sorted input nodes precede ways, so node
output takes its final position during collection); then process ways, then
copy relations. Keep the source string table and raw tag indices; copy plain
nodes as messages; filter dense columns (raw node builder as an intermediate).

Payoff: removes a full node decompression pass on the default filtering path
and overlaps output with index construction.

Risks: dense metadata presence, delta reconstruction after deletion, tag
delimiters, plain-node fields; arbitrary input order needs source-ordinal
assembly or spooled fragments, which the sorted path should not inherit.

Kind: high-conviction rewrite.

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

## PERF-029 - Sparse ALTW per-node tuples and per-reference rank arithmetic

Reported by: altw-sparse, sort.

Mechanism: node collection materializes a 16-byte `NodeTuple` per node and
revisits it for membership and rank; `rank_within` scans up to seven bitmap
words and popcounts; `rank_if_set` tests membership separately; counters call
`referenced.iter().count()` though `IdSet::len()` popcounts words and
`RankedIdSet::total_count()` exists.

Redesign: feed the store from `extract_node_coords` with a fallible sink; on
verified sorted ranges merge the node stream with `RankedIdSet::iter_from` and
`rank(start)` and advance a cursor (missing referenced nodes still consume
ranks); a frozen index with per-word prefixes combining membership with the
target-word popcount; use `len()` / `total_count()`.

Kind: medium-value local changes.

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

## PERF-041 - Diff replays block prefixes and decodes crossing frames per unit

Reported by: decode, osc, merge.

Mechanism: `BlockRun::elements()` builds a fresh iterator and `.skip(n)`;
`element_merge_pair` rebuilds these for every residual; `BlockElementsIter` has
no seek, so dense skipping reconstructs IDs, coordinates, metadata and tag
boundaries, and a large block against many small ones grows superlinearly.
`Frame` shares compressed bytes across units but `BlockPairSide::decode`
decompresses and builds a block per unit, plus a clipping walk. After the
`blobs_byte_equal` shortcut fails, full blocks and elements are built, dense
metadata decoded though equality ignores it, tags and roles resolved per
element. Unindexed `StreamingBlocks` decompresses, merges and emits on one
thread.

Redesign: owned immutable decoded frames with resumable cursors (message
position, packed-stream positions, delta accumulators, tag position); crossing
frames share one decoded form with seek checkpoints from the first scan;
validation produces comparison views (IDs, normalized coordinates, tag
boundaries, raw field spans, checkpoints) with metadata lazy and full elements
only for changed output; equality via increasingly expensive proofs (compressed
bytes, decompressed primitive bytes, columns, identical packed fields with same
interpretation, string-table equivalence resolved once, semantic fallback);
decode unindexed blobs in parallel and plan comparisons over actual ranges.

Payoff: removes repeated decompression and superlinear decode; strongest for
mostly equal snapshots with different block boundaries or encodings.

Risks: bounded shared decoded frames, checkpoint state per delta stream,
duplicate IDs and history versions at boundaries, signed ordering, equal SIDs
across tables prove nothing, dense versus plain and noncanonical varints need
semantic fallback, versions are not content evidence, late dense metadata needs
prior deltas.

Kind: high conviction (cursors); medium conviction, wider payoff (comparison
decoder).

Defect hunt 2026-11-17 (diff) found the same quadratic prefix replay. The
repeated-ID boundary defect it also named is fixed without cursors: the pair
merge holds a boundary ID whose peer block ran out as a residual. A persistent
cursor must keep that rule (equal IDs pair in occurrence order across blobs,
and a held side's claim is checked against what lies behind its repeats).

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

## PERF-043 - tags-filter closure rescans relations to a fixed point

Reported by: tags, framing, decode, indexdata.

Mechanism: `collect_relation_member_closure` rescans every relation blob each
iteration and expands every included relation again; workers retain member
vectors and one thread inserts IDs. `collect_way_node_dependencies` scans the
whole way schedule though it needs relation-selected ways; matched ways decode
refs in classification and again in output. `CatalogEntry` keeps kind but drops
`min_id` / `max_id`, so `IdSet::any_in_range` cannot prune (getid does). Pass
2 sets `want_nodes = true` even when node inclusion sets are empty. Substring and
wildcard keys disable key-based blob filtering though the stored key set could
answer conservatively.

Redesign: dependency order - relations read once into adjacency and packed
typed member lists, worklist closure (as `getid::close_relations`), ways after
closure combining direct match and inclusion with one ref collection, nodes
after all dependencies, output assembled by source ordinals from bounded buffers
or compressed scratch; route requested way IDs to blobs instead of a planet
way-to-node graph; partitioned reference insertion (PERF-025); local: keep ID
ranges, skip empty node pass, wider tag-index use.

Payoff: closure becomes one graph build and one traversal; concentrated in
relation-heavy queries.

Risks: spooling cost may exceed saved decode; cycles, missing refs, duplicate
IDs and versions, source order, provenance (direct matches, way nodes, closure
objects affect tag removal and stats differently); stored spans must not point
into pooled decompression buffers.

Kind: high conviction (fixed-point removal).

Defect hunt 2026-11-17 (id-filter-selection) independently observed the
per-nesting-level rescan and repeated collection of already reached members,
pointing to `getid` as the existing explicit worklist; it labels this an
unmeasured optimization, not a correctness finding.

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

## PERF-045 - Checks re-decode payload per check

Reported by: check.

Mechanism: `run_check` runs locations, IDs, index verification and references
as separate library calls; bare `check` runs IDs and references separately, so
node and way blobs are decoded twice, and `--index` adds a scan. Inspect report
and node statistics run separately (`run_inspect`). Unindexed
`unindexed_phase` discards a whole way or relation verdict on any miss and
rereads the blob.

Redesign: an audit execution reading and decompressing each blob once and
computing ID order, membership, references and index observations from the same
fields, sharing membership between reference and duplicate checks, separate
results and policies; deferred misses as compact unresolved requests (a hit is
final since membership grows) with bounded spill. Combined inspect modes share
payload work while keeping claim descriptions distinct from measurements.
Per-mode projection table in the check report.

Risks: policy differences (index checking's narrower acceptance, inspect's
partial coverage, kind-claim stops, type filters); refusal scope must not widen;
error precedence and capped-detail order.

Kind: highest conviction in the check scope.

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

## PERF-049 - Renumber map construction does atomic per-node insertion

Reported by: sort.

Mechanism: every node does an atomic `fetch_or`; consecutive IDs hit the same
word; ways go to workers building general `IdSet`s merged serially; rank
construction scans bitmaps serially; the map preallocates node-ID-wide from a
bounded maximum claim.

Redesign: workers own contiguous schedule regions, accumulate whole words
locally, return owned pages with local rank summaries and first and last IDs;
joining moves pages, combines boundary fragments, checks strict order and
prefix-sums; sparse pages for sparse high-ID input.

Risks: range ownership must follow observed IDs, not unverified claims; gain
beside decompression and compression needs measurement.

Kind: high conviction.

## PERF-050 - Renumber lookups are scalar and touch separate arrays

Reported by: sort.

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

Kind: strong hypothesis.

## PERF-051 - History time-filter selects and rebuilds on one thread

Reported by: pipeline, copy.

Mechanism: `time_filter_history` decodes in parallel then groups, selects,
clones and re-encodes on the calling thread; every version before the cutoff is
cloned into `OwnedElement`, replacing the last candidate.

Redesign: workers validate local key and version order, reduce interior
`(kind, id)` groups, encode their survivors, and return first and last group
summaries (ordering, counts, last eligible candidate); an ordered stitcher
reconciles boundary groups, including groups spanning many blobs, and places
winners before the next partition's frames; selection reads identity, version,
timestamp and visibility, full content only for winners. Snapshot time-filter
already filters on workers; its gain is projection and encoded selection.

Risks: groups across arbitrary blobs, strict version checks, missing
timestamps, deletion accounting, malformed superseded versions still validated
(user and role indices) without copying.

Kind: high-conviction full rewrite.

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

## PERF-058 - Zstd decompression builds a streaming decoder per blob

Reported by: decode.

Mechanism: `decompress.rs` creates a streaming decoder per blob and uses
`Read::take(..).read_to_end(..)` despite holding the payload in memory.

Redesign: a worker-owned bulk context decoding into a bounded destination.

Risks: keep output limits, raw-size checks, concatenated-frame behaviour and
corruption detection; an oversized recycled `Vec` is not a valid bound.

Kind: local change. (Decode ranks a different whole-buffer DEFLATE backend,
the decompression-pool mutex, offset-table layout and fused inflate-plus-parse
below this; none is recommended first.)

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
