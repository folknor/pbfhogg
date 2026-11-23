# Read, write and infrastructure defects

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

Defects from the 2026-11-17 defect hunt in the PBF read path, the write path,
direct and io_uring I/O, and the library's concurrency, filesystem and output
infrastructure. Every finding was made by one codex hunter per scope reading
source at `63d4ec1d`. None was built, run or reproduced. "Reported by" names
the scope or scopes, and the bracketed priority is the hunter's own label.
Findings that match an existing mechanism were added to `notes/bugs.md`
instead of being filed here. Entries are in no order of importance.

## FMT-005 - BlobReader::seek does not resume a failed reader

Reported by: read-framing.

After a malformed frame produces `Err`, a successful `BlobReader::seek`
(`read/blob.rs`) to a valid frame still leaves `next()` and
`next_header_skip_blob()` returning `None`: the source moved but the sticky
failure remains. `seek_raw` and `blob_from_offset` do resume. This contradicts
the internal statement in the same file that iteration stops "until a seek
resumes it". The public docs should either distinguish the behaviours, or the
public seek operations should share recovery semantics.

## FMT-006 - raw_blob_payload promises refusals it does not make

Reported by: read-framing.

Its documentation (`read/decompress.rs`) promises the same refusals as
`decompress_blob_raw`. It discards `raw_size` and accepts envelopes the
decompressor rejects for size mismatch. Diff's documented byte-equality
shortcut explains why. The hunter says the helper's promise is what is wrong.

## FMT-009 - par_map_reduce's sub-2-GiB working-set guarantee is false

Reported by: read-pipeline [P2].

`par_map_reduce` (`read/reader.rs`) promises a working set well under 2 GiB
even with pathological 32 MiB blobs. Its admission weight counts only the
compressed Blob body. A legal block near 32 MiB holding millions of empty
string-table entries expands to about 128 MiB of entry scratch plus another
128 MiB of appended offset tables (`read/wire.rs`), on top of the payload.
Several workers on such blocks exceed the bound while staying well inside the
compressed-byte budget. The sizes are derived from the representation, not
measured. Keeping the guarantee would need admission that accounts for decoded
storage, tables and worker scratch.

## FMT-029 - The frame conformance matrix has no O_DIRECT FileReader source

Reported by: implementer of the pinned direct-open and aligned-skip fixes.

`reference/truncation-handling.md` says the full matrix, every truncation shape
through every source, is `read/frame_conformance_tests.rs`. Its `Source` enum
drives buffered `FileReader`, `BlobReader` and both header walkers, but no
frame walk over `FileReader::Direct`, so `DirectReader::skip` (both its
aligned and unaligned branches) is outside the matrix the document cites. Only
the reader's own unit tests cover it. A direct source, skipping where the
platform refuses O_DIRECT, would make the claim true.

## FMT-012 - pipeline_decode_tasks counts blobs that are never decompressed

Reported by: read-pipeline [P3].

The dispatcher (`read/pipeline.rs`) increments `pipeline_decode_tasks` for
every blob, unknown extension blobs included, even though the ordinary
transform skips those without decompressing. `pipeline_metrics.rs` defines the
count as one per OSMData blob and says that subtracting filter skips gives the
number of real decompressions. A file holding only extension blobs after its
header yields a positive inferred decompression count with none performed. The
counter and its documented interpretation disagree.

## FMT-030 - zstd context allocation failure panics instead of erroring

Reported by: spar partner during the checked-compression-level work.

`zstd::bulk::Compressor::new` returns `io::Result`, but builds its context
with `CCtx::default()` first, which calls `create()` and panics if zstd
returns null. An allocation failure on a framing worker's first zstd blob
therefore unwinds rather than returning the writer's `Result`. Levels can no
longer cause a failure there (`ZstdLevel` is checked against the linked
library's range). A fix would build the context with `CCtx::try_create()`
and set the level through `zstd_safe` directly in `write/framing.rs`.
Allocation-failure class; low priority.

## FMT-018 - PbfWriter::write_header skips the cancellation checkpoint

Reported by: write-backends [P2].

Every other public write entry in `write/writer.rs` checks cancellation, but
`write_header` frames and writes directly. A synchronous writer created with
`PbfWriter::new_in` accepts and writes a header after its invocation is
cancelled, and returns success. This contradicts the per-write cancellation
contract documented next to it. Publication still prevents a cancelled staged
output from being published. The defect is in the write method's behaviour.

## FMT-024 - path_guard same_file stringifies the original I/O error

Reported by: filesystem-output [P2].

The `named` closure in `path_guard.rs` formats the original error into a
string and builds a replacement `io::Error`. A canonicalization failure with
`EACCES` keeps `PermissionDenied` but loses the original error object and its
errno, and later conversion into `pbfhogg::Error` cannot recover either. This
breaks the error-provenance contract in `reference/pipeline.md`: typed errors
keep their causes, and intermediate layers do not stringify them. The original
error should be kept in a typed path wrapper. `path_guard::dir_holds`, the
containment preflight `build-geocode-index` shares with the library, names
its errors the same way and should move with it.

## FMT-025 - ParallelGzipWriter writes nothing for an empty stream

Reported by: write-encoding (flagged as a surprise).

The empty-stream test in `write/parallel_gzip.rs` deliberately requires zero
output bytes, with not even an empty gzip member. The current OSC caller
always writes document framing, so the hunter found no CLI trigger.

## FMT-026 - decompress_blob_data_into copies the whole serialized Blob

Reported by: read-framing (optimization observation).

`decompress_blob_data_into` (`read/decompress.rs`) copies the entire
serialized Blob through `parse_slice`, even though it already receives
borrowed bytes and caller-owned output storage. The shared borrowed parser
`BlobEnvelope::parse` (`read/blob_wire.rs`) now exists, and
`decompress_blob_raw` already decodes from it without a copy.

