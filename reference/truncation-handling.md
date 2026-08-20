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
exits non-zero; stderr names the byte offset and the shape of the
break. Silent recovery from partial input is **not** supported.

The single exception is a clean cut at a frame boundary: a file
that ends with the previous frame fully written and either zero
bytes or 1-3 leftover bytes of an incomplete next length prefix.
That shape is the natural signal of "end of input" and is
tolerated.

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

- `BlobReader::next` (`src/read/blob.rs`)
- `BlobReader::skip_blob_body` (`src/read/blob.rs`) - the
  payload-skip path used by the header-only fast scans
- `read_raw_frame` (`src/read/raw_frame.rs`)
- `HeaderWalker::next_header` (`src/read/header_walker.rs`)
- `FileReader::skip` (`src/read/file_reader.rs`) - the
  caller-side payload-skip used by `read_blob_header_only`
  consumers (`has_indexdata`, `diff::process`, `cat::dedupe`,
  `altw::passthrough`)
- `PrimitiveBlock::new` (`src/read/block.rs`) for shape 5
- `zlib_decompress_into` / `zstd_decompress_into`
  (`src/read/decompress.rs`) for shape 5 - the actual
  decompressors, reached by every `decompress_*` entry point
- `validate_groups_strict` (`src/read/block.rs`) for the
  block-layer contract below

A truncation that reaches any of those entry points must surface
as `Err(ErrorKind::Io)` (or a more specific variant) before the
caller sees decoded output. The two skip-path sites
(`skip_blob_body`, `FileReader::skip`) preserve their `BufReader`
seek-relative optimization for in-range targets and only pay the
sentinel-read cost when the skip would otherwise pass EOF
silently.

Commands that consume `BlobReader` / `ElementReader` /
`IndexedReader` inherit the contract automatically: an `Err` from
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
   bounded varints, and the parallel-array invariants the zipping
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
parsing walks field headers and records slice bounds without
decoding any packed array, and the dense length check is a byte
scan. On a matched interleaved A/B over Denmark
(`add-locations-to-ways`, commit `40cbae08`, host `bygg`) the cost
was at or below run-to-run noise: 2201 ms / 2100 ms / 2100 ms for
on / off / on.

The consequence for consumers is that the `Err` arms remaining in
the element iterators are now unreachable rather than silent: a
block that constructed successfully cannot contain an element that
fails to parse. There is deliberately **no lenient mode**. A
reader that wants partial data from a corrupt file gets a hard
error and the byte offset instead.

Two dense-metadata cases are knowingly *not* covered and remain
silent: a `DenseInfo` array shorter than the id array (metadata
disappears from that point on) and a `keys_vals` run without a
trailing delimiter. Both are on the read-path bug list.

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
- The `Blob.raw_size` capacity hint is clamped to the same cap
  before it reaches `Vec::reserve`. It is an unvalidated `int32`,
  so a corrupt blob could otherwise declare 2 GiB of
  pre-decompression allocation from five bytes on the wire.

## Tests

The contract is pinned at two layers:

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

**Command layer** (`tests/cli_truncation_sweep.rs::truncation_sweep_no_panic`)
drives `cat`, `inspect`, `sort`, `getid`, `add-locations-to-ways`,
and `renumber` through ~50 truncation offsets covering every
blob's length-prefix midpoint, header midpoint, header end,
payload midpoint, and payload end. The six commands exercise
distinct read-path shapes - passthrough, header-only fast scan,
indexed-decode, header-walk-with-pread, `altw::passthrough`
through `FileReader::skip`, and full-read + pass-2 reframe - so
between them they touch every contract site listed above. For
shape 2-4 offsets, asserts non-zero exit. For shape 1 offsets,
asserts no-panic + bounded stderr only - command-level outcome on
a partial-input file is per-command policy (sort may legitimately
reject a tail truncation that drops most data blobs even when the
reader's tolerance contract holds; the reader contract is pinned
by the reader-layer tests above).

Other commands' read-path tolerance is implicitly covered by the
shared `BlobReader` / `HeaderWalker` / `FileReader::skip`
primitives - any command that goes through them inherits the
contract.

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
2-4. The list grew from six to eight on 2026-08-20, when the two
decompressors and `validate_groups_strict` were added; the six
framing sites are the ones the end-to-end sweep drives, and the
two new ones are pinned by unit tests instead.

Initial alignment landed in commit `436998b` (covering
`BlobReader::next`, `skip_blob_body`, and `HeaderWalker::next_header`);
the `FileReader::skip` site landed in commit `12699db` after a
post-pass review surfaced the `read_blob_header_only` caller-side
payload-skip path as a silent shape-4 hole. `read_raw_frame` and
`PrimitiveBlock::new` were already aligned by their existing
patterns (`read_exact` end-to-end and Cursor-based protobuf walk
respectively).

`mutate_blob_payload`-based regression tests
(`tests/cli_defensive_input.rs`) exercise shape 5 (decompression
failure on a byte-valid frame whose inner protobuf is corrupt).
