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

## FMT-010 - A replaced pinned input can surface as PlatformUnsupported

Reported by: read-pipeline [P2].

`PinnedInput` (`read/input.rs`) promises `InputReplaced` when its fallback path
reopen meets a different file, including a FIFO. The direct reopen adds
`O_DIRECT` before identity verification, and `open_for_read_verified`
(`direct_io.rs`) classifies an open failure before it runs `verify`. With
procfs reopening unavailable, replace the pinned path with a FIFO. The direct
open fails with `EINVAL` and becomes `PlatformUnsupported` instead of
`InputReplaced`. A replacement on a filesystem that refuses direct I/O behaves
the same way. This changes the typed failure and the CLI exit classification.
The fault is in classifying backend setup failure before establishing which
input it concerned.

## FMT-011 - An aligned direct skip validates against the length captured at open

Reported by: read-pipeline [P2].

`DirectReader::skip_with` (`read/direct_reader.rs`) checks an aligned target
only against the length captured at open. Read a BlobHeader whose payload
extends past the current buffer, then truncate the file inside that payload.
If the declared end is aligned, the skip succeeds against the stale length,
the next read returns zero, and the frame walk can report a clean EOF. This
breaks the unconditional shape-4 refusal and source-equivalence claims in
`reference/truncation-handling.md`. Pinning's documented lack of snapshot
isolation does not establish that a declared payload still exists. The hunter
says either the skip needs a current-extent check or the documentation must
limit the guarantee to immutable inputs.

## FMT-012 - pipeline_decode_tasks counts blobs that are never decompressed

Reported by: read-pipeline [P3].

The dispatcher (`read/pipeline.rs`) increments `pipeline_decode_tasks` for
every blob, unknown extension blobs included, even though the ordinary
transform skips those without decompressing. `pipeline_metrics.rs` defines the
count as one per OSMData blob and says that subtracting filter skips gives the
number of real decompressions. A file holding only extension blobs after its
header yields a positive inferred decompression count with none performed. The
counter and its documented interpretation disagree.

## FMT-013 - Invalid compression levels panic instead of returning an error

Reported by: write-encoding [P2], write-backends.

`Compression::Zlib(10)` can be constructed through the public enum.
`compress_zlib` (`write/framing.rs`) forwards it straight to
`flate2::Compress::new`, and the locked zlib-rs backend asserts when
initialization rejects the level. A synchronous header write or path
constructor can therefore unwind instead of returning its declared `Result`.
The CLI string parser protects CLI users, but library callers bypass it.
`write/compression.rs` acknowledges this explicitly and cites compatibility.
Both hunters say that rationale conflicts with the pre-1.0 premise. The
proposed fix is a checked compression-level type, or validation at the writer
boundary before any output is created.

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

