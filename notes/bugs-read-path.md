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

## Shape-4 detection is three mechanisms with three failure messages

Deliberately left alone when the frame decoder was collapsed into
`src/read/frame.rs` (2026-08-20), and re-filed here so the decision stays
actionable rather than merely made. `frame.rs` unified the length prefix, the
end-of-file policy and both size guards; it could not unify shape 4 ("EOF inside
the Blob payload"), because each site detects it with the only mechanism its I/O
shape offers:

- `BlobReader::next` (`blob.rs`) compares the count returned by
  `Take::read_to_end` against the declared `datasize`.
- `BlobReader::skip_blob_body` (`blob.rs`) and `FileReader::skip`
  (`file_reader.rs`) seek past the payload and then read one sentinel byte to
  find out whether the seek landed inside the file.
- `HeaderWalker::next_header` (`header_walker.rs`) compares
  `data_offset + data_size` against the `file_size` captured at `open()`.

Streaming has no file size, and pread has no stream position, so unifying them
needs a seekable-source abstraction that reports "bytes available from here"
and degrades to the read-and-count arm when the source cannot answer. That is a
real refactor, not a guard to add, which is why it was not folded into the
frame collapse.

Nothing is known to be wrong at any of the three today - every one of them hard
errors, which is the contract. The cost is drift risk and three different
messages for one shape, and the concrete symptom is that
`reference/truncation-handling.md` has to enumerate the sites individually
where shapes 1-3 now need only `read::frame`.

Confidence: high that the divergence is real, high that it is currently benign.

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

1. The dense `keys_vals` / `denseinfo` length checks close the last known
   silent-truncation paths inside a block, finishing what strict block
   validation started.
2. The shape-4 unification is the remaining half of the frame-decoder collapse.
   Nothing is broken there today, so it is a refactor to schedule rather than a
   bug to fix - do it when the seekable-source abstraction is wanted for its own
   sake.
