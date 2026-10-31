# Truncation handling

How pbfhogg behaves when a PBF input ends before its declared
structure says it should.

Drafted 2026-04-26 to settle a stance that had been implicit and
inconsistent across `cat`, `inspect`, `sort`, and the read path
itself. Until this doc landed, behavior on truncated input was
"whatever happened to surface" - some shapes errored, some
succeeded silently, none were guaranteed.

## Stance

A truncation that splits any structural element of the wire format
in two is a hard error. The reader returns `Err`; the command
exits non-zero with the error on stderr. Every reader reports it the
same way, because every reader decodes frames through the one frame
walk (see "What gets pinned"): the error is located at the frame it
broke (the input, the blob's index and the frame's start offset) and
the message names the declared extent that runs past the end of the
input, for example "frame truncated: its BlobHeader declares a
31234-byte Blob at bytes [A, B), past the end of the input". Silent
recovery from partial input is **not** supported.

The single exception is a clean cut at a frame boundary: a file
that ends with the previous frame fully written and either zero
bytes or 1-3 leftover bytes of an incomplete next length prefix.
That shape is the natural signal of "end of input" and is
tolerated.

## Header position and declared size

Every reader rejects an OSMHeader frame after the first frame, before
reading or skipping its payload. The frame walk (`read::frame::Frames`)
applies this policy once per frame for every source. Sequential,
parallel, pipelined, raw passthrough and header-only command paths
therefore inherit the same refusal. A caller that intentionally stops
early need not inspect later frames.

For `BlobReader::new_seekable`, the stream starts at the reader position
when constructed, including a nonzero position containing an embedded PBF.
An OSMHeader at that position counts as leading. Subsequent seeks do not
redefine the start: revisiting that header is allowed, but seeking to a
different OSMHeader is rejected. Byte offsets remain absolute within the
underlying reader; a nonzero construction position leaves the absolute
blob index unknown.

This matches libosmium's `pbf_input_format.hpp`: its initial header parse
expects OSMHeader, then its data loop expects OSMData. Ignoring a second
header would discard its feature requirements and metadata, interpreting
its data under the first header instead. The alternative would be to
accept and reconcile every header, including incompatible feature claims;
that policy is not implemented. Pass separately framed PBF files as
separate command inputs instead of concatenating their bytes.

Low-level blob APIs still permit reading a data frame alone, including by
seeking to it, and retain their existing handling of unknown extension
blobs. An OSMHeader is accepted again after rewinding to the stream start
(offset zero for readers opened from a path).

`WireBlobHeader::parse` checks `datasize` as a signed int32 before narrowing.
Out-of-range varints cannot alias a smaller frame length (for example,
2^32 + 100 becoming 100). Sign-extended negative int32 values retain their
value and reach the existing negative-size guard; the existing 32 MiB
serialized-body cap still applies before any body allocation or skip.

## The five shapes

PBF wire format:

```text
Stream = Frame*
Frame  = [u32 BE length L] [L bytes BlobHeader] [datasize bytes Blob]
```

| # | Shape                                  | Behavior      |
|---|----------------------------------------|---------------|
| 1 | EOF at frame boundary (0-3 leftover)   | Tolerated     |
| 2 | Length prefix points past EOF          | Hard error    |
| 3 | EOF inside BlobHeader bytes            | Hard error    |
| 4 | EOF inside Blob payload                | Hard error    |
| 5 | Decompression of payload fails         | Hard error    |

Shape 1 covers both "file ends exactly at frame boundary" (the
common case for a complete file) and "file ends with a partial
length prefix that didn't get fully written" (a writer that
crashed at the end of the previous frame). The 1-3 byte tail is
not enough to start a new frame, so the reader treats it as EOF.

Shapes 2-5 cover every case where the reader was committed to
producing more data and didn't get it. Returning `Ok` with
fewer-than-declared elements would silently ship corrupt output to
downstream consumers.

## Why hard error on shapes 2-5

The failure mode this stance prevents is **silent data loss**.

A user runs `pbfhogg cat input.pbf -o output.pbf`, the input was
truncated (failed download, partial copy, disk full mid-write),
and the command exits 0 with a smaller output file. The user
believes they have the data; they have a fraction of it. Every
downstream stage of a pipeline carries the loss forward. Backup
verification scripts pass on truncated archives because `pbfhogg
cat /dev/null < archive.pbf` succeeds.

Hard erroring at the read path stops the cascade at its source.
The error message names what was missing, the user knows the
input is bad, and the pipeline halts before propagating partial
data.

## What gets pinned

Every command that reads PBF input adheres to the same contract -
there is no per-command relaxation. The contract sites are:

- `read::frame::Frames` (`src/read/frame.rs`) - the one frame
  walk. It owns the length prefix, the shape-1 EOF policy, both size
  guards (zero-length and `MAX_BLOB_HEADER_SIZE`), the header parse,
  the header-position rule, the declared-datasize guards, shapes 2-4
  and the provenance stamped on every frame error. Every frame
  reader holds one over its own byte source: `BlobReader`
  (streaming, for its iterator and for `next_header_skip_blob`),
  `HeaderWalker` (pread), the raw-frame walks of the passthrough and
  full-scan commands (`Frames::next_raw` over a `FileReader`), and
  the input-front probes (`has_indexdata`, `InputFront`), which read
  headers and skip payloads over a `FileReader`. `cat --dedupe`'s
  pass 1 walks with `HeaderWalker`, as `sort`'s does.

Shape 4 is detected by whatever the source can do, and reported the
same way whatever it was: a stream counts the bytes its bounded read
delivered against the declared `datasize`; a seekable stream skips
the payload with `BlobReaderSource::skip_relative` (which keeps a
`BufReader`'s buffer) and reads one sentinel byte, because a seek
past the end succeeds and only a read can tell; a `FileReader` skips
through `FileReader::skip` (the same for buffered input,
`DirectReader::skip` for O_DIRECT, which validates its target
itself); and the pread walker compares the payload extent against
the file size it captured at `open()`.

The sources agree on every frame-layer outcome and report it in the
same words at the same location. The full matrix, every truncation
shape and size guard through every source, is
`src/read/frame_conformance_tests.rs`, one expectation per case,
with the error text compared across sources; a change to it is a
behaviour change at every source. Two things are outside the frame
layer and so differ by reader:

- Envelope parsing. Only `BlobReader`'s iterator parses the Blob
  message, because a parsed `Blob` is what it returns; raw
  passthrough and header walks never look inside a payload. A
  payload that is not a Blob message is therefore refused by the
  iterator (located at its frame) and passed by the other readers;
  it surfaces at decode, or not at all on a passthrough that copies
  it verbatim.
- Shape 5 is invisible to every frame reader; it surfaces when the
  payload is decompressed.

The other contract sites are:

- `zlib_decompress_into` / `zstd_decompress_into`
  (`src/read/decompress.rs`) for shape 5 - the actual
  decompressors, reached by every `decompress_*` entry point
- `WireBlob::parse` (`src/read/blob_wire.rs`) and
  `decompress_blob_raw` (`src/read/decompress.rs`) - the two Blob
  envelope parsers. Both reject a blob carrying more than one of
  `raw` / `zlib_data` / `zstd_data`, and both cross-check a
  declared `Blob.raw_size` against the payload they produced.
- `validate_groups_strict` (`src/read/block.rs`) for the
  block-layer contract below. Every `PrimitiveBlock` constructor -
  `from_vec`, `from_vec_with_scratch`,
  `from_vec_pooled_with_scratch` - funnels through `finish` and so
  through this check. Note that no `PrimitiveBlock` constructor is
  a shape-5 site: they take already-decompressed bytes and cannot
  observe a decompression failure.

A truncation that reaches any of those entry points must surface
as `Err(ErrorKind::Io)` (or a more specific variant) before the
caller sees decoded output. The two stream skips (the `FrameSkip`
implementations for a `BlobReaderSource` and for `FileReader`) keep
the `BufReader` seek-relative optimization: they seek `n - 1` bytes and then read one
sentinel byte. That sentinel read is paid on every nonzero skip, not
only when the skip would pass EOF, because a seek past EOF succeeds
and only the read can tell. A zero-byte skip returns without
reading. `FileReader::skip` over an O_DIRECT file does not use a
sentinel: `DirectReader::skip` validates the target against EOF
itself.

Commands that consume `BlobReader` / `ElementReader` inherit the contract
automatically: an `Err` from
the read path propagates up through the command's `run()` and out
to the CLI's `main`, which exits non-zero with the error message
on stderr.

## Inside the blob: the block-layer contract

The stance above was originally implemented at the frame layer
only. Everything below the blob boundary did the opposite, because
`Iterator::next` cannot return an error: `WireMessageIter::next`
ended the element stream on a wire error, the per-kind group
iterators skipped any element whose `parse` failed, a malformed
`DenseNodes` message became an empty iterator, mismatched dense
array lengths truncated the node run, and a corrupt `Info`
sub-message became default metadata. A corrupt blob therefore
produced fewer elements and exit code 0 - the exact failure this
document exists to prevent, one layer in.

Since 2026-08-20 the block layer is strict too. Every
`PrimitiveBlock` constructor funnels through `finish`, which runs
`validate_groups_strict` before the block is handed out. Per
group, in order:

1. the group's own top-level field framing,
2. every `Node` / `Way` / `Relation` sub-message parses,
3. every element's optional `Info` sub-message parses,
4. every packed array on those elements holds only complete,
   bounded varints (see "Which varints count as complete" below),
   and the parallel-array invariants the zipping
   iterators depend on hold: `keys` against `vals` on all three
   kinds, `lat` against `lon` against `refs` on ways, and
   `roles_sid` against `memids` against `types` on relations,
5. the `DenseNodes` sub-message parses, and its packed `id`,
   `lat` and `lon` arrays are well-formed and of equal length.

Step 4 matters as much as the rest: parsing an element only
records the bounds of its packed fields, so a `vals` array one
entry short, a `types` array that drops the last relation member,
or a `lon` array that turns a closed way into an open one all
parse cleanly and then truncate inside an infallible iterator that
zips them. Those are silent geometry and tag changes, not just
missing elements.

Any failure is `Err` from the constructor, so it propagates like
any other read error. The order matters: the framing walk is what
makes the element iterators used in steps 2-4 unable to terminate
early, so running them first would silently examine a prefix.

This is a structural pass, not a second decode - sub-message
parsing walks field headers and records slice bounds, and the
packed-array checks count varints with a byte scan rather than
decoding their values. The one exception is the dense `keys_vals`
walk described below, which decodes that array's varints to find
each node's tag-run terminator. On a matched interleaved A/B over Denmark
(`add-locations-to-ways`, commit `40cbae08`, host `bygg`) the cost
was at or below run-to-run noise: 2201 ms / 2100 ms / 2100 ms for
on / off / on.

The consequence for consumers is that the `Err` arms remaining in
the element iterators are now unreachable rather than silent: a
block that constructed successfully cannot contain an element that
fails to parse. There is deliberately **no lenient mode**. A
reader that wants partial data from a corrupt file gets a hard
error and the byte offset instead.

This holds for consumers that report on fields the failing check
does not touch. The full-decode modes of `inspect`'s scanning report
refuse a block with a malformed way `refs` array even though no
statistic reads it: after a read failure the report has not
established complete aggregates, and the blocks read so far must
not be presented as whole-file totals. The acceptance set is the
chosen boundary, not an artifact of `PrimitiveBlock`
construction, so a projection scanner that replaces construction
in those modes keeps refusing exactly what the block reader
refuses. Lookups and specialised scanners (`inspect --show`,
`inspect --nodes`, `src/scan/node.rs`) are not bound by this
paragraph; their own docs state what they check.

The two dense-node arrays that array-length equality could not
express are covered as well. `keys_vals` is a single flat array of
zero-terminated per-node tag runs, so it is walked once per block
and must yield exactly as many runs as the `id` array has entries;
this is the only check in the pass that is not a byte scan. Every
`denseinfo` array must be either absent or exactly as long as the
`id` array, and a `denseinfo` carrying only some of its five
metadata arrays is rejected, because `DenseNodeInfoIter` zips them
and stops at the shortest - which blanks metadata from that point
on while reporting success.

Two encodings stay legal and are not truncation: an absent
`keys_vals`, meaning no node in the group has tags; and a final
tag run that ends at the end of the array without its `0`
terminator, which some writers emit.

### Which varints count as complete

Every packed iterator decodes with `protohoggr`'s
`Cursor::read_varint` and turns its error into an end of
iteration, so a length check is only as good as the count it
compares. The count therefore accepts exactly the arrays repeated
`read_varint` consumes in full: no array may end mid-varint, and
the tenth byte of a varint must be `0` or `1` (anything more is
payload past bit 63 or an eleventh byte, which the decoder
refuses). Overlong encodings within ten bytes, and the ten-byte
sign-extended negatives of `int32` fields, stay legal because the
decoder accepts them. The count used to look only at terminal
bytes, so a ten-byte varint ending in `0x02` counted as one value
and decoded as none: a validated dense group then yielded one node
fewer than its id count, and the columnar decoder, which decodes
the three arrays independently, could pair an id with another
node's coordinates. A unit test checks the count against the
decoder itself.

The refusal costs nothing on normal input. The tenth-byte rule
needs a position within the varint, and tracking one per byte
made the count serial: about a sixth of single-threaded planet
decode. So the count works eight bytes at a time, counting
terminal bytes with a mask and a popcount and carrying only the
length of the continuation run between words. Without a run of
nine continuation bytes no varint reaches a tenth byte, so the
array is valid exactly when it does not end mid-varint. Only an
array that holds such a run, which no PBF writer emits, is
judged byte by byte. `performance.md` holds the measurements.
`packed_uint32_count`, the variant for tag string indices, scans
per byte, because its `u32` bound needs the position anyway.

This is a decodability guarantee, not a value-range one, and on
purpose. A 32-bit field whose varint does not fit its type is
decoded by narrowing, as protobuf specifies (a cast for `int32`
and `uint32`, truncation to 32 bits before the zigzag step for
`sint32`): `DenseInfo` `version`, `uid` and `user_sid`, relation
`roles_sid` and `types`, and the scalar `Info` fields all read
that way, so a version of `2^32` reads as 0. Only a malformed
writer produces such a value, and the read path is built for speed
on well-formed data: the width check that was tried and rejected
added a per-byte state machine to the per-node metadata arrays of
every dense block. What holds for a narrowed value: string indices
resolve through checked lookups (a negative one is an error), an
out-of-range member type is represented as `MemberType::Unknown`,
and the dense deltas accumulate with wrapping arithmetic. One
exception: in a debug build, `BlockBuilder`'s raw-passthrough
assertion (`debug_assert_raw_strings`) checks role indices as raw
`u64`, so a role of `2^32 + n` that the reader resolves as string
`n` trips it; release builds pass the bytes through. Renumber's raw
relation rewriter dispatches on the same truncated member type, so
the two paths agree.

The non-canonical form real files do carry is one this rule reads
correctly: protohoggr before 0.5.0 converted the zigzagged `i32`
with `as u64`, which sign-extends a zigzag value with bit 31 set (a
delta `v >= 2^30` or `v < -2^30`) to ten bytes, and `BlockBuilder`
writes the dense `uid` and `user_sid` deltas through it. Since
protohoggr 0.5.0 those deltas are written as canonical five-byte
varints, but files written earlier keep the ten-byte form, and
truncation reads both back exactly.

Two string-index bounds are the exception and stay: `keys` / `vals`
must fit `u32`, and dense `keys_vals` entries must fit a
nonnegative `int32`. Neither guards memory safety (tag lookups are
checked). The first keeps the element decoder in agreement with
the raw scanners that read the same indices and refuse oversized
ones (`blob_meta`'s tag scan, way classification in `scan/way.rs`);
the second keeps every index representable as the nonnegative
`i32` that the dense raw-tag iterator narrows it to.

## Which wire forms of a packed field count as present

The checks above are only worth as much as the parser's notion of
"this array is empty". Protobuf lets a packed repeated field be
written either length-delimited (one packed segment, what every
OSM writer emits) or varint (individually tagged, unpacked
values), and lets it appear more than once, with concatenation
semantics.

Until 2026-08-20 `src/read/wire.rs` matched only the
length-delimited form on every packed field of `Node`, `Way`,
`Relation`, `DenseNodes` and `DenseInfo`. An unpacked field fell
through to `skip_field` and left the slot empty, which meant an
unpacked `DenseNodes.keys_vals` decoded as "no node in this block
has tags" - and sailed straight through the emptiness early-return
in the validator above. A spec-legal encoding therefore produced
exactly the silent data loss this document exists to prevent.
libosmium accepts both forms for every one of these fields, so
such a file is not hypothetical corruption; it is a file the
reference implementation reads.

Current behavior, in `take_packed`:

- **Length-delimited**: accepted, zero-copy, unchanged.
- **Varint (unpacked)**: accepted. A single unpacked value is
  byte-identical to a one-element packed segment, so the field's
  own bytes are captured as that segment with no copy and every
  downstream iterator is unaffected.
- **Two or more occurrences that both carry data**: rejected, with
  an error naming the message and field. Empty occurrences are
  ignored in either position, since an empty segment contributes
  nothing to a concatenation.

The rejection is a deliberate strictness delta, of the same kind
as the multi-payload-blob rejection below. Spec-correct handling
is to concatenate every occurrence in order, which cannot be
expressed as the single borrowed slice the read path is built on -
and the measured cost of strict validation depends on that slice
staying borrowed. The alternative is what this parser used to do
and what libosmium still does: keep the last occurrence and drop
the rest, silently. Between rejecting a shape no known writer
produces and silently dropping element data, we reject. Multiple
unpacked values of the same field hit the same rule, which is why
unpacked support is exact for a one-element array and an error
beyond it, where libosmium keeps only the last value.

### The independent scanners

Several components walk these same messages without going through
`ElementReader`, and each carried its own matcher. As of 2026-08-20
they answer the same way `wire.rs` does, by two different routes.

**Routed through `wire.rs`** - the scanner no longer has a message
parser of its own, so the policy cannot drift again:

- `src/scan/way.rs` parses each Way via `WireWay::parse`. It had
  matched `WIRE_LEN` only on `refs`, `keys` and `vals`; an unpacked
  `refs` invoked no callback at all and the caller concluded the way
  referenced no nodes - a silently short node index. It decodes
  `refs` with the fallible `read_varint` and propagates its error,
  since no block construction validated the array first.
- `src/blob_meta/scan.rs` parses every element message (`Node`,
  `DenseNodes`, `Way`, `Relation`) via the `wire.rs` parsers. Its
  predecessor read an unpacked `id` array as empty, so the blob
  silently got no indexdata.
- `src/scan/node.rs` already routed through `WireDenseNodes::parse`
  and never had the defect; its own top-level matcher touches only
  submessages and scalar fields, neither of which can be packed.
  Because it never constructs a `PrimitiveBlock`, it makes the
  block layer's dense coordinate checks itself, during the decode
  it already performs: `id`, `lat` and `lon` are read in lockstep
  with the fallible `read_varint`, and the group is refused unless
  all three arrays end together. It used to zip three infallible
  packed iterators, which stopped at the shortest array or the
  first bad varint, so a malformed group scanned to fewer nodes and
  the altw node index silently missed them. It does not validate
  `keys_vals` or `denseinfo`, which it never reads.

**Handled locally, because routing is impossible:**

- `src/commands/renumber/wire_rewrite.rs` is a byte-level splicer.
  It needs a field's *byte range* inside the enclosing message so
  every other field can be copied verbatim; `wire.rs` yields the
  *value* as a borrowed slice and deliberately does not expose
  ranges. It therefore rejects the unpacked form of `DenseNodes.id`,
  `Way.refs`, `Relation.memids` and `Relation.types` with an error
  naming the field. Previously an unpacked `memids` left the memids
  range unset, so the relation took the "zero-member" branch and its
  member IDs were copied through **un-renumbered** into output that
  claims to be renumbered.
- `src/commands/altw/reframe.rs` accepts both forms for `Way.refs`
  and normalizes the unpacked spelling to the packed one on output;
  it re-emits that field itself, so it can. Matching `WIRE_LEN` only
  meant an unpacked `refs` fell to the verbatim copy arm, no
  coordinates were appended, and the way came out of
  `add-locations-to-ways` with no locations at all. It also strips
  both forms of the stale `lat` / `lon` / pins fields, so an unpacked
  stale coordinate can no longer survive next to the fresh one. It
  decodes `refs` with the fallible `read_varint` too: an array that
  ends mid-varint or holds a varint wider than 64 bits used to be
  copied through verbatim with locations for only its prefix. The
  normalization is byte-exact for every packed input, including an
  empty packed segment: `add-locations-to-ways` output is compared
  byte-wise against osmium in `brokkr verify`, so re-emitting the
  packed form has to be a no-op.

Where a component rejects rather than accepts, the observable
contract still holds: a legal encoding is never silently
mis-handled.

### Repeated occurrences in the two rewriters

The unpacked form is only half the policy. Protobuf also permits a
packed repeated field to occur several times, with concatenation
semantics: two length-delimited segments of `Way.refs` are one ref
array. `take_packed` refuses that shape because the read path is
built on a single borrowed slice; the two rewriters have to refuse
it for a sharper reason, and both now do, per field, with an error
naming the message and field:

- `renumber`'s splice replaces one contiguous byte range and copies
  everything else verbatim, so keeping only the last occurrence left
  the earlier segment's IDs **un-renumbered** inside output that
  claims otherwise - for `DenseNodes.id`, `Way.refs` and
  `Relation.memids` alike. `Relation.types` is not spliced but drives
  the per-member node/way/relation dispatch, so keeping one segment
  either desynchronizes it from `memids` (caught by the count
  cross-check, with a misleading message) or types every member
  against the wrong ID space.
- `add-locations-to-ways` builds one ref list per way, so a second
  data-bearing occurrence would append coordinates for only part of
  the way. Because each unpacked value is its own occurrence, this is
  also where its unpacked support ends: exact for a one-element
  array, an error beyond it, matching `take_packed` exactly.

Empty occurrences are ignored in either position in both rewriters,
as in `take_packed` - a zero-length segment carries no elements and
cannot participate in a concatenation.

## Decompression bounds (shape 5)

Shape 5 is enforced during decompression, not after it:

- A deflate stream that ends before its final block is a typed
  error. `flate2` reports it as `Ok(Status::BufError)` rather than
  `Err`, so the inflate loop terminates on "no input consumed and
  no output produced" instead of spinning.
- The `MAX_BLOB_MESSAGE_SIZE` cap is applied *during* inflation
  for both zlib and zstd. A blob whose declared `datasize` passes
  the 32 MiB pre-read guard can still inflate to orders of
  magnitude more, so a post-hoc length check fires only after the
  process has allocated the bomb. Inflation stops one byte past
  the cap and reports `MessageTooBig`; the true decompressed size
  is unknown by design.
- Both Blob parsers validate `raw_size` as a signed int32 before narrowing,
  once per envelope. Out-of-range varints cannot alias a small honest size
  and bypass the decompressed-length comparison. Sign-extended negative
  int32 values parse intact and fail that comparison.
- The `Blob.raw_size` capacity hint is clamped to the same cap
  before it reaches `Vec::reserve`. It is a producer-declared `int32`,
  so a corrupt blob could otherwise declare 2 GiB of
  pre-decompression allocation from five bytes on the wire.
- Once decompression has run, the same `Blob.raw_size` is
  cross-checked against the payload actually produced. A hint used
  only for sizing means a lying value is invisible; a value that
  disagrees with the result is corruption. An absent `raw_size`
  stays legal - our own writer omits it on uncompressed blobs, as
  libosmium does. We are stricter than libosmium in one direction:
  it sizes its output buffer to `raw_size` and calls `uncompress`,
  so an undersized value fails with `Z_BUF_ERROR` but an oversized
  one succeeds and silently yields a payload zero-padded to the
  declared length.
- A `Blob` carrying more than one of `raw` / `zlib_data` /
  `zstd_data` is rejected by both parsers. They are three
  encodings of one payload, not three payloads. The two parsers
  previously disagreed about such a blob - `WireBlob::parse` was
  last-field-wins, `decompress_blob_raw` returned on the first
  `raw` it saw - so one adversarial blob decoded to two different
  blocks depending on which read path reached it. Rejecting
  removes the ambiguity rather than enshrining a winner.

  This is a deliberate compatibility cost, not a free win. Repeating
  a non-repeated protobuf field is legal wire encoding with
  last-one-wins semantics, so a file that read successfully before
  may now be rejected. No known writer produces the shape -
  libosmium's PBF output builder emits exactly one payload field per
  blob - but its reader tolerates it, with a third policy again
  (returns on the first `raw`, otherwise keeps the last compressed
  field). Three readers, three answers, is the argument for
  rejecting rather than for copying the reference implementation.
  The error message names the offending fields so a user can act on
  a file they did not write.

## Tests

The contract is pinned at several layers:

**Frame conformance** (`src/read/frame_conformance_tests.rs`): every
truncation shape, size guard and boundary case driven through every
frame source (`BlobReader`'s read and skip paths, the raw-frame walk,
the front probes' header-then-skip walk and `HeaderWalker`), each
case held to one outcome, error kind, file, blob index and byte offset
for all of them, and the error text compared across sources. The
envelope difference has its own test beside it.

**Reader layer** (`tests/read_paths.rs`): unit tests on
`BlobReader::next` directly, independent of any command's policy
on partial input.

- `trailing_partial_length_prefix_returns_ok_none` - 0-3 leftover
  bytes after every complete blob is `Ok(None)`. Pins shape 1
  tolerance.
- `trailing_partial_length_prefix_4_bytes_is_committed_frame` -
  4 trailing bytes (a complete length prefix declaring header
  bytes that don't follow) is shape 2, must hard-error.
- `truncated_header_size`, `truncated_header_data` in
  `tests/corrupt_input.rs` - shapes 1 and 3 at the BlobReader
  level.
- `tests/proptests.rs::blob_reader_truncated_fixture_never_panics` -
  property-based: arbitrary truncation length is finite Ok/Err,
  never panics.

**Command layer** (`cli/tests/cli_truncation_sweep.rs::truncation_sweep_no_panic`)
drives `cat`, `inspect`, `sort`, `getid`, `add-locations-to-ways`,
and `renumber` through truncation offsets inside every blob's
length prefix, header and payload, plus uniformly spaced offsets
across the file. The commands read the input by different routes, so
between them they reach several of the contract sites listed above;
the sweep does not map each command to a site. For
shape 2-4 offsets, asserts non-zero exit. For shape 1 offsets,
asserts no-panic + bounded stderr only - command-level outcome on
a partial-input file is per-command policy (sort may legitimately
reject a tail truncation that drops most data blobs even when the
reader's tolerance contract holds; the reader contract is pinned
by the reader-layer tests above).

Other commands' read-path tolerance is implicitly covered by the
shared frame walk - any command that reads frames goes through it
and inherits the contract.

**Block and decompression layer** (unit tests beside the code):

- `src/read/block.rs::strict_validation_tests` - hand-encoded
  blocks pinning each check, each paired with a well-formed
  counterpart so the test cannot pass against a constructor that
  rejects everything. The mid-varint fixtures additionally keep
  every array count equal, so only the truncation check can be
  what rejects them; an earlier fixture did not, and passed with
  the truncation check disabled because the length-equality check
  fired instead.
- `src/read/decompress.rs::tests::truncated_zlib_stream_errors_instead_of_hanging` -
  runs the decompression on a worker thread behind a 5 s watchdog,
  so the hang regression surfaces as a bounded failure rather than
  as a hung suite.
- `src/read/decompress.rs::tests::decompression_bomb_is_bounded_during_inflation`
  and `raw_size_capacity_hint_is_clamped_to_the_cap` - pin the
  during-inflation bound and the capacity-hint clamp separately.
- `src/read/frame.rs::tests` - the size guards and the shape-1
  EOF policy at their boundaries: a 0-3 byte tail is `Ok(None)`, a
  complete prefix never is, and a prefix of `0xFFFFFFFF` is
  refused by the size guard rather than by the allocator; and the
  walk protocol (a header, then its payload; a failure ends the
  walk until it is repositioned). The pread source decides how short
  a tail is from the captured file size, so it has its own test,
  `src/read/header_walker.rs::tests::the_walk_tolerates_a_short_tail_but_not_a_complete_prefix`.
- `tests/merge_pbf.rs::merge_rejects_a_corrupt_frame_prefix_without_allocating` -
  both guards end to end through `cat --dedupe`, whose pass 1 once
  had its own frame loop with neither guard and no test of either
  (it now walks with `HeaderWalker`, and so the shared frame walk).
  Each arm
  asserts its own guard's message, not a shared substring: with a
  bare "header" check the zero-length arm passed with the
  zero-length guard disabled, because an empty `BlobHeader` is then
  caught one layer down by the no-type-field rejection, whose
  message also says "header".
- `src/read/decompress.rs::tests::a_blob_with_two_payload_fields_is_rejected_by_both_parsers`
  and `a_lying_raw_size_is_rejected` - the Blob-envelope checks,
  driven through both parsers and in both field orders.
- `tests/corrupt_input.rs::zero_length_blob_header_is_rejected`
  and `trailing_zero_run_after_valid_blobs_is_rejected` - a
  `header_len` of 0 is a committed frame that must error, and a
  zero run appended to a real file must not read as extra empty
  blobs.
- `src/read/columnar.rs::tests::short_coordinate_columns_do_not_panic_any_pass` -
  the classification passes run before any assertion about
  `len()`, so a regression reproduces the index-out-of-bounds
  panic rather than tripping a cheaper structural assert first.

A regression at any of the contract sites surfaces as a
reader-layer test failure (the sweep is the end-to-end backstop
for the six commands listed above specifically).

## Implementation status

Aligned. All contract sites listed above hard-error on shapes
2-4, and every frame source reports them identically.

Initial alignment landed in commit `436998b` (covering
`BlobReader::next`, its payload skip, and `HeaderWalker::next_header`);
the caller-side `FileReader::skip` that followed the front probe's
header-only read landed in commit `12699db` after a post-pass review
surfaced it as a silent shape-4 hole. The raw-frame read was already
aligned by its end-to-end `read_exact` pattern.

On 2026-10-26 the three frame decoders that remained (`BlobReader`'s
own, the streaming helper behind the raw-frame and front-probe reads,
and `HeaderWalker`'s) became one state machine over a byte source,
`read::frame::Frames`. Until then they shared the guards but each had
its own I/O, EOF spelling, truncation messages and provenance, and
they disagreed about what a user saw: `BlobReader` located a
truncation at the first missing byte on its read path and at the last
declared byte on its skip path, and its header-level refusals four
bytes past the frame; the raw-frame reads named no blob; the
front probe's caller-side skip named nothing at all; and only
`BlobReader` called its truncations "(shape N)". All of them now
locate at the frame and word a truncation by the declared extent.

The list was reworked on 2026-08-20 around `read::frame`. Before
that, the frame envelope was hand-rolled at five sites and this
table had to enumerate each of them; the guards then had to be
added five times, and the `cat --dedupe` copy - which this
document never listed - had received neither the zero-length
rejection nor the `MAX_BLOB_HEADER_SIZE` cap, so four bytes of
corrupt input drove an unbounded allocation there. The same pass
corrected a longstanding error in this list: it named the
`PrimitiveBlock` constructor as the shape-5 site, which it never
was. That constructor receives already-decompressed bytes; shape 5
lives in `decompress.rs`, which the list did not mention until
the decompressors were added on 2026-08-20.

`mutate_blob_payload`-based regression tests
(`cli/tests/cli_defensive_input.rs`) exercise shape 5 (decompression
failure on a byte-valid frame whose inner protobuf is corrupt).
