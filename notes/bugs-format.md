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

## FMT-032 - Block-decode loops outside ElementReader keep scratch peaks

Reported by: spar on the reader scratch retention rule (lateral).

The three `ElementReader` engines and apply-changes, extract simple/smart
and altw stage 4 release a string-table or group list grown past
`MAX_RETAINED_SCRATCH_CAPACITY` after each block
(`crate::retention::release_scratch`). Other loops that reuse the same
pair across blocks keep their largest capacity until the loop ends, so one
outlier block (a legal 32 MiB block of empty strings grows the table list
to about 128 MiB) stays pinned on that worker: `scan/classify.rs` (three
sites), `osc/merge_join.rs`, `commands/altw/external/relation_scan.rs`,
`commands/altw/passthrough.rs`, `commands/extract/common.rs` and
`extract/multi.rs`, `commands/getid/mod.rs`,
`commands/inspect/show_element.rs` and one loop in `commands/cat/mod.rs`.
Not measured; inventory from a grep of the pair names, not re-verified
site by site.

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

