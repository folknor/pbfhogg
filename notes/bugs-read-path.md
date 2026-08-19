# Bug hunt: read path (`src/read/`)

Hunt date 2026-08-12. Single Opus hunter, scope: `src/read/` in full plus
`src/reorder_buffer.rs` as consumed by the pipeline. Findings only, unverified.

## Truncated/corrupt zlib stream = infinite loop (hang), not an error

`src/read/decompress.rs`, `zlib_decompress_into`.

```rust
loop {
    let status = decompress.decompress_vec(input, buf, FlushDecompress::None)?;
    let consumed = ...; input = &input[consumed..];
    if matches!(status, Status::StreamEnd) { break; }
    if buf.len() == buf.capacity() { buf.reserve(...); }
}
```

`Decompress::decompress_vec` returns `Err` only on a genuine `DecompressError`.
When the deflate stream simply ends early (no final block, input exhausted),
zlib-rs returns `Status::BufError`, which flate2 maps to `Ok(Status::BufError)`
(verified in `flate2-1.1.9/src/ffi/zlib_rs.rs` and `mem.rs`). The loop then spins
forever: `input` is empty, `consumed` is 0, `buf` has spare capacity so no
growth, no `StreamEnd`, no error. 100 % CPU, no output, no timeout, no
diagnostic.

Reachable whenever a byte-valid frame carries a truncated-but-not-otherwise-
corrupt deflate payload (partial copy that also mangled a length prefix, a
hand-crafted blob, a writer that wrote `datasize` correctly but a short
`zlib_data`). This is exactly shape 5 in `reference/truncation-handling.md`,
which the doc pins as "hard error". The code hangs instead. Every read path
funnels through this function (`decompress_blob`, `decompress_blob_raw`,
`decompress_wire_blob_into`, `decompress_parsed_blob_into`), so the whole scope
inherits it, including all rayon decode workers, where the hang looks like a
deadlock rather than a spin.

Fix: treat "no progress and input exhausted" as `Err`. Track
`consumed == 0 && bytes_written == 0` and bail with a typed error.

Confidence: high (code plus dependency source read; not executed).

## Decompression bomb: `MAX_BLOB_MESSAGE_SIZE` is enforced after full inflation

`src/read/decompress.rs`.

The doc comment says "Enforces `MAX_BLOB_MESSAGE_SIZE`". It does not. Both
compression branches decompress to completion and only then compare:

- zlib: `zlib_decompress_into` grows `buf` unboundedly, checks
  `buf.len() > MAX` at the end.
- zstd: `zstd::stream::copy_decode(..., &mut *buf)` runs to completion, then
  checks length.

A blob whose declared `datasize` is just under the 32 MiB cap (which is enforced
pre-read) can inflate to tens of GB with zlib, and far more with zstd (ratios
around 10^5 are trivially constructible). The process OOMs or thrashes before the
guard fires. `MAX_BLOB_DATASIZE`'s doc comment carefully explains that it exists
precisely to bound pre-decompression allocation, and then the post-decompression
bound it hands off to is not actually a bound.

Fix is mechanical: cap the zlib output loop at `MAX_BLOB_MESSAGE_SIZE + 1` bytes
and error on overflow; wrap the zstd reader in `.take(MAX_BLOB_MESSAGE_SIZE + 1)`
or use `zstd::Decoder` with a bounded write target. The existing boundary test
(`decompress_helpers_agree_at_message_size_boundary`) only exercises payloads
that fit in RAM, so it passes today and would keep passing.

Confidence: high.

## Element-level decode silently drops malformed elements

`src/read/wire.rs`, `src/read/block.rs`, `src/read/elements.rs`.

The framing layer is rigorously guarded (six documented contract sites, typed
errors, offsets in the message). Everything below the blob boundary does the
opposite:

- `WireMessageIter::next` (`wire.rs`): `self.cursor.read_tag().ok()??` and
  `if self.cursor.skip_field(wire_type).is_err() { return None }` - a wire error
  silently ends the element stream mid-group.
- `BlockElementsIter::step`, `GroupNodeIter`, `GroupWayIter`,
  `GroupRelationIter` (`block.rs`): `if let Ok(x) = WireX::parse(data) { ... }`
  with the `Err` arm falling through to the next element - malformed elements
  are skipped without a trace.
- `PrimitiveGroup::dense_nodes` / `BlockElementsIter::step`: a
  `WireDenseNodes::parse` error yields `DenseNodeIter::empty(...)` - an entire
  dense group of up to 8000 nodes vanishes into an empty iterator.
- `DenseNodeIter::next`: stops as soon as any of the id/lat/lon packed arrays
  runs out, so mismatched array lengths truncate the node run.
- `elements.rs`: `info_data.and_then(|d| WireInfo::parse(d).ok())
  .unwrap_or_default()` - corrupt metadata becomes default metadata (version 0,
  timestamp 0), indistinguishable from genuinely absent.

Net effect: `pbfhogg cat corrupt.pbf -o out.pbf` exits 0 with silently fewer
elements. That is precisely the failure mode `reference/truncation-handling.md`
was written to prevent ("Returning `Ok` with fewer-than-declared elements would
silently ship corrupt output to downstream consumers") - the stance was
implemented at the frame layer and never carried into the block layer. The doc's
claim that the contract is complete is false for anything inside a blob.

This is a design problem, not a line bug. `Iterator` returning `Element` rather
than `Result<Element>` is what forces every one of these `.ok()`s. Pre-1.0, the
hunter would change the iterators to `Result<Element>`, or (cheaper, less
invasive on consumers) have `PrimitiveBlock` validate the group structure
eagerly at construction - it already walks every group to inline the ranges, so
a strict pass costs almost nothing and turns all of the above into one hard
error at `finish()`.

Confidence: high on the behavior; the fix shape is a judgement call.

## Panic on malformed dense block via the columnar path

`src/read/columnar.rs`, `decode_append`.

`decode_append` decodes ids, lats, lons in three independent loops, each pushing
to its own `Vec`. Nothing ties their lengths together. `len()` returns
`ids.len()`. Then:

```rust
for i in 0..n { let lat = lats[i]; let lon = lons[i]; ... }
```

A dense group with more ids than lats (packed `lat` field truncated, or absent
entirely while `id` is present) gives `lats.len() < ids.len()` and the loop
panics with index-out-of-bounds. Same in `collect_matching_ids_multi_bbox` and
`collect_matching_ids_multi_bbox_grid`.

Reachable from user input via `extract --strategy smart`
(`commands/extract/smart.rs`) and `multi-extract`
(`commands/extract/multi.rs`), which are the only callers. Note the
element-iteration path (`DenseNodeIter`) handles the same malformed input
gracefully (it stops), so the two decoders disagree on the same bytes, one
silently truncating and one panicking. Fix: after the three loops, require all
three lengths equal and return an error otherwise (this function should be
fallible).

Confidence: high.

## `header_len == 0` frames are accepted - infinite garbage reads as clean success

All four frame decoders accept a 4-byte length prefix of `0`:
`read_blob_header` (`blob.rs`), `read_raw_frame` and `read_blob_header_only`
(`raw_frame.rs`), `HeaderWalker::next_header` (`header_walker.rs`). A
zero-length `BlobHeader` parses to `BlobKind::Unknown("")` with `datasize == 0`,
forming a legal 4-byte frame that every consumer then skips as "unknown blob
type".

Consequence: a file consisting of a valid OSMHeader followed by N zero bytes is
read as N/4 empty unknown blobs, all silently skipped, exit 0, empty output.
Same for any run of zeros appended to or substituted into a real file.
`ElementReader::for_each` and the pipeline both explicitly document "skip
non-OsmData without decode", so nothing anywhere notices.

A `BlobHeader` with no `type` field is not a valid PBF frame. Reject
`header_len == 0`, and reject `BlobKind::Unknown("")` (empty type string) at the
same place.

Confidence: high on the mechanism; severity depends on how much silent success
on garbage input matters.

## Four hand-rolled copies of the same frame decoder

`BlobReader::read_blob_header`, `read_raw_frame`, `read_blob_header_only`,
`HeaderWalker::next_header` each independently implement: read 4-byte BE prefix,
EOF tolerance policy, `MAX_BLOB_HEADER_SIZE` guard, read header bytes, parse,
`MAX_BLOB_DATASIZE` guard, payload handling. Three of them share
`parse_blob_header_with_index` for the last two steps; `BlobReader` deliberately
does not and carries an inline twin of the datasize check (its own comment says
so). The truncation doc has to enumerate "six contract sites" and the test suite
has to drive six different commands to cover them - that enumeration is a
symptom.

Each copy differs in ways that matter and are not obviously intentional:

- EOF tolerance: `read_blob_header` distinguishes 0 vs 1-3 trailing bytes with a
  two-part `read_exact`; `read_raw_frame` treats any `UnexpectedEof` on the
  prefix as clean EOF (so a 2-byte tail is EOF, matching by luck);
  `HeaderWalker` uses `probe_len < 4`.
- Shape-4 detection: `BlobReader` compares `read_to_end` count; `skip_blob_body`
  and `FileReader::skip` use the seek-then-sentinel-byte trick; `HeaderWalker`
  compares against `file_size`. Three mechanisms, three failure messages, three
  chances to drift.
- `header_len == 0` (finding above) is wrong in all four, identically.

Pre-1.0, collapse this to one `FrameDecoder` that owns prefix, EOF policy, both
size guards, and header parse, parameterised only by "materialise payload / skip
payload / record offsets". The four call sites become thin. That deletes the
six-contract-site table from the doc and makes the `header_len == 0` finding a
one-line fix instead of a four-line fix.

Confidence: high that this is the right shape; it is a real refactor.

## Divergent Blob-envelope semantics between the two parsers

`WireBlob::parse` (`blob_wire.rs`) is last-field-wins: a blob carrying both
`raw` (field 1) and `zlib_data` (field 3) decodes as zlib. `decompress_blob_raw`
(`decompress.rs`) returns immediately on field 1, so the same bytes decode as
raw. Two read paths, two different decoded contents for one adversarial blob.
Neither rejects the multi-field case.

Also, neither errors when `raw_size` disagrees with the actual decompressed
length - it is used only as a capacity hint, so a lying `raw_size` is silently
ignored (osmium cross-checks it). Low practical impact today; it is a latent
divergence in a place where "two paths must agree" is a stated goal (see the
`decompress_helpers_agree_at_message_size_boundary` test, written for exactly
this class of bug at a neighbouring boundary).

Confidence: high on the divergence, low on current exploitability.

## `reference/truncation-handling.md` names a contract site that isn't one

The doc lists "`PrimitiveBlock::new` (`src/read/block.rs`) for shape 5" and says
it "was already aligned by [its] existing pattern (Cursor-based protobuf walk)".
`PrimitiveBlock::new` takes already-decompressed `Bytes`; it never touches a
compressed payload and cannot detect a decompression failure. Shape 5 actually
lives in `decompress.rs`, which is not mentioned anywhere in the doc - and, per
the first two findings, is where the stance is violated. A durable-folder
document that must be true is currently pointing at the wrong file for the one
shape that is broken.

## `PrimitiveBlock::new` copies the whole buffer on a zero-copy path

```rust
pub fn new(buffer: Bytes) -> Result<PrimitiveBlock> { Self::from_vec(buffer.to_vec()) }
```

This is the public constructor, and it pays a full ~1.4 MB memcpy per block for
the privilege of an owned `Vec` that `parse_and_inline` then extends. Internal
callers were all migrated away (`to_primitiveblock`,
`decode_blob_to_primitiveblock` both route through `decompress_into` +
`from_vec`, with comments explaining why), so what remains is a public API whose
only implementation is the slow path the internals were rewritten to avoid. Any
library consumer holding `Bytes` gets the copy. Since the `Bytes` must be
extended in place, the honest signature is `from_vec(Vec<u8>)`; pre-1.0, make
that the public constructor and delete `new`.

Confidence: high.

## `BlobReader::from_path` issues `POSIX_FADV_DONTNEED` over `0..offset` per blob

`blob.rs`, in `Iterator::next`. Two problems, both design rather than bug:

- Global side effect from a library constructor. `from_path` unconditionally
  sets `evict_fd`, so any consumer of the public `BlobReader::from_path` /
  `ElementReader::from_path` API silently evicts the page cache for that file -
  for the whole system, not just this process. A second pass over the same file
  (two-pass commands, a test suite, a user's `osmium` running alongside) is
  guaranteed cold. There is no opt-out on the public surface; the only escape is
  `from_path_direct`, which is a different I/O mode entirely.
- Quadratic-ish syscall shape. The advised range is `0..current_offset`,
  re-issued per blob. On planet that is ~2.5 M `posix_fadvise` calls each
  nominally covering a growing prefix. Kernel side is cheap per already-evicted
  page, but it is still 2.5 M syscalls to express "drop what I just read", which
  `offset_prev..offset` would express in the same number of calls with a bounded
  range.

Should be an explicit builder option (`.evict_page_cache(bool)`), defaulted
however the CLI wants, and the range should be the just-consumed window.

Confidence: high on the API smell; the perf cost was not measured.

## Minor / lower confidence

- `DenseNodeIter` tag-run boundary. In `dense.rs`, `tag_end` subtracts 1 whenever
  `tag_start < kv_pos`, on the assumption that the run ended at a `0` delimiter.
  When `keys_vals` ends without a trailing delimiter (last node, malformed
  writer), the loop exits via `cursor.is_empty()` and the unconditional `-1`
  chops the last byte of the final value varint, silently corrupting or dropping
  that tag. Only reachable on non-conforming input. Confidence: medium-high.
- `DenseNodeInfoIter` short-array truncation. If any of the six parallel info
  arrays is shorter than the id array, `next()` returns `None` and every
  subsequent dense node in the group gets `info == None` - metadata silently
  disappears from that point on rather than erroring. Same class as the
  element-drop finding.
- `WireBlobHeader::parse` silently ignores wrong-sized `indexdata`. A length that
  is neither 26 nor 42 is dropped with no signal, so a blob with a corrupt-length
  index reads as "no index" and the filter conservatively passes it through. Safe
  direction, but invisible; a warning or error would be better than nothing.
- `AdmissionGate` single-acquirer invariant is load-bearing and unenforced.
  `release()` uses `notify_one()` and the doc comment says a second acquirer
  requires `notify_all()`. Both `run_pipeline` and `run_pipeline_fused` honour it
  today (one dispatcher each), but nothing in the type prevents a future second
  caller, and the failure mode is a silent hang under load. A `debug_assert` on
  acquirer thread id, or making the gate own its single acquire handle, would pin
  it.
- `ReorderBuffer::push` asserts on duplicate/stale seq, and the pool-build
  failure path sends `seq = 0`. In `run_pipeline`, if the rayon pool fails to
  build after any item has been delivered, `push(0)` panics with "stale sequence
  number" inside `drain_decoded` on the caller's thread. Only reachable on
  `ThreadPoolBuilder::build` failure, which is near-impossible in practice - but
  the error path chose a magic seq that the reorder buffer's contract forbids.
  Use a dedicated out-of-band error channel or the current high-water seq.
- `PipelinedBlocks::next` maps `RecvError` to `None`. Correct today because the
  background thread always sends the error before dropping `tx`, but the coupling
  is implicit: if the pipeline thread ever panics, the consumer sees a clean
  end-of-iteration instead of an error. `Drop` then joins and discards the panic
  (`drop(h.join())`). A panicking pipeline currently reads as "file ended".
  Confidence: high on the mechanism, low on frequency.

## Hunter's suggested order

1. The zlib hang and the decompression bomb are the only two that turn a hostile
   or merely damaged file into a hang or an OOM. Both are localized to
   `zlib_decompress_into` and the zstd branches; both are ~20 lines.
2. The columnar `decode_append` panic is reachable from two shipped commands.
3. The element-drop finding deserves an actual design decision rather than a
   patch: the crate has a well-argued, well-tested stance on silent data loss
   that stops at the blob boundary, and the block layer contradicts it in a dozen
   places. Deciding it (strict validation at `PrimitiveBlock::finish` is the
   hunter's recommendation - cheapest, one error site, no consumer churn) is more
   valuable than any individual `.ok()` fix.
4. The frame-decoder collapse makes the `header_len == 0` finding and future
   framing bugs one-line fixes instead of four.
