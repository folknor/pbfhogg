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

A regression at any of the six contract sites surfaces as a
reader-layer test failure (the sweep is the end-to-end backstop
for the six commands listed above specifically).

## Implementation status

Aligned. All six contract sites listed above hard-error on shapes
2-4. Initial alignment landed in commit `436998b` (covering
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
