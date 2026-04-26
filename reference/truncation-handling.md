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
- `read_raw_frame` (`src/read/raw_frame.rs`)
- `HeaderWalker::next_header` (`src/read/header_walker.rs`)
- `PrimitiveBlock::new` (`src/read/block.rs`) for shape 5

A truncation that reaches any of those entry points must surface
as `Err(ErrorKind::Io)` (or a more specific variant) before the
caller sees decoded output.

Commands that consume `BlobReader` / `ElementReader` /
`IndexedReader` inherit the contract automatically: an `Err` from
the read path propagates up through the command's `run()` and out
to the CLI's `main`, which exits non-zero with the error message
on stderr.

## Tests

The contract is pinned by:

- `tests/cli_truncation_sweep.rs::truncation_sweep_no_panic` -
  drives every command through ~50 truncation offsets covering
  every blob's length-prefix midpoint, header midpoint, header
  end, payload midpoint, and payload end. Asserts non-zero exit
  + no panic + bounded stderr.
- `tests/proptests.rs::blob_reader_truncated_fixture_never_panics` -
  property-based variant: arbitrary truncation length must surface
  as Err, never panic.

A regression that returns `Ok` from any of the four contract sites
on shapes 2-5 fails one of the above. A regression that returns
`Err` on shape 1 fails the existing roundtrip tests
(`tests/roundtrip*.rs`) which write and read complete PBFs.

## Implementation status

Not yet aligned. As of 2026-04-26, the truncation-sweep test was
loosened to "no panic + bounded stderr" because `cat`, `inspect`,
and `sort` currently exit 0 on shapes 2-5 in some cases. Aligning
the implementation with this stance requires auditing the four
contract sites for `Ok`/`None` returns on mid-frame EOF and
promoting each to `Err`. After the audit, the sweep test
re-tightens to assert non-zero exit and the deferred Tier A8
follow-up in `notes/testing.md` closes.

`mutate_blob_payload`-based regression tests
(`tests/cli_defensive_input.rs`) exercise shape 5 (decompression
failure on a byte-valid frame whose inner protobuf is corrupt) -
this shape already errors today via the upstream protobuf walker.
The other three shapes (2-4) are the ones the audit needs to
target.
