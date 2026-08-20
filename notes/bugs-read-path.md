# Bug hunt: read path (`src/read/`)

Hunt date 2026-08-12. Single Opus hunter, scope: `src/read/` in full plus
`src/reorder_buffer.rs` as consumed by the pipeline. Findings only, unverified.

## Dense `keys_vals` and `denseinfo` array lengths are still unvalidated

`src/read/block.rs`, `validate_groups_strict`.

Strict block validation now length-checks every packed array on regular
elements and the dense `id`/`lat`/`lon` triple, but two dense arrays are
excluded because array-length equality does not express their invariant:

- `DenseNodes.keys_vals` is a single flat array of zero-terminated per-node
  runs, so establishing that it describes exactly `n` nodes needs a stateful
  walk counting delimiters, not a varint count. A short or over-long
  `keys_vals` therefore still mis-assigns tags, and the `DenseNodeIter`
  tag-run boundary bug below is the same array seen from the other side.
- `DenseNodes.denseinfo` carries six parallel arrays that must each match the
  node count. Today a short one silently drops metadata from that point on
  (see the `DenseNodeInfoIter` entry under Minor).

Both are genuine silent-truncation paths of the same class as the resolved
element-drop finding, and both are excluded deliberately rather than
overlooked. Validating them means walking `keys_vals` once per block, which is
the only check in this pass that is not a byte scan - worth measuring before
adopting.

Confidence: high on the gap; the cost of closing it was not measured.

## `cat --dedupe` reads a 4-byte length prefix with no size guard

`src/commands/cat/dedupe.rs`, the frame loop.

It reads the 4-byte big-endian prefix and immediately does
`header_buf.resize(header_len, 0)` with no `MAX_BLOB_HEADER_SIZE` check - the
guard that `BlobReader`, `read_raw_frame`, `read_blob_header_only` and
`HeaderWalker::next_header` all carry. Four bytes of corrupt input therefore
request up to a 4 GiB allocation before anything inspects them. This is a fifth
copy of the frame decoder that the "four hand-rolled copies" finding below does
not count, and it is the one copy missing a guard the other four share, which
is the argument for that refactor in miniature.

The zero-length case is already covered here, but only indirectly: an empty
header slice parses to an empty type and is rejected by the shared
`WireBlobHeader::parse` funnel rather than by a local check.

Confidence: high.

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
- The `header_len == 0` rejection had to be added to all four independently,
  and to `cat --dedupe`'s fifth copy it was never added at all (see above) -
  a worked example of the cost of the duplication rather than a hypothetical.

Pre-1.0, collapse this to one `FrameDecoder` that owns prefix, EOF policy, both
size guards, and header parse, parameterised only by "materialise payload / skip
payload / record offsets". The four call sites become thin. That deletes the
six-contract-site table from the doc and makes the next framing guard a
one-line fix instead of a five-line one.

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

## Suggested order

1. The `cat --dedupe` missing size guard is the one remaining place where four
   bytes of corrupt input drive an unbounded allocation. It is a two-line fix.
2. The dense `keys_vals` / `denseinfo` length checks close the last known
   silent-truncation paths inside a block, finishing what strict block
   validation started.
3. The frame-decoder collapse makes future framing guards a one-line fix
   instead of a five-line one, and would have prevented the `cat --dedupe` gap
   outright.
