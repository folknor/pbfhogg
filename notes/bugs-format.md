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

## FMT-031 - par_map_reduce worker scratch keeps its peak for the whole read

Reported by: spar on the par_map_reduce memory promise (lateral).

`run_par_worker` (`read/reader.rs`) never releases `st_scratch` or
`gr_scratch`. One block of empty groups followed by one of empty strings
leaves a worker holding about 128 MiB in each list until the read ends, so an
outlier block raises that worker's footprint for the rest of the file. The
`commands`-gated `release_scratch` helper is not reachable from the core
reader; a fix would move it into shared code. It does not lower the
concurrent peak of a wave of such blocks.

## FMT-012 - pipeline_decode_tasks counts blobs that are never decompressed

Reported by: read-pipeline [P3].

The dispatcher (`read/pipeline.rs`) increments `pipeline_decode_tasks` for
every blob, unknown extension blobs included, even though the ordinary
transform skips those without decompressing. `pipeline_metrics.rs` defines the
count as one per OSMData blob and says that subtracting filter skips gives the
number of real decompressions. A file holding only extension blobs after its
header yields a positive inferred decompression count with none performed. The
counter and its documented interpretation disagree.

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

