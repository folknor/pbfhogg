# Pre-release: work items, and what stands between here and 0.6.0

The working document for the 0.6.0 release and everything workable found on
the way to it. It absorbs `notes/bugs.md` and the four research notes of
2026-10-30 (cat --dedupe memory, merge-changes memory, pool-heavy RSS,
inspect --extended wall), plus the residue of the retired library-harness
note; git history keeps them.

This is a transient note. Nothing in it is guaranteed true, and nothing
durable (code comments, `reference/`, `docs/`, `decisions/`, tests) may cite
it, so durable text never cites a WRK identifier either. Every workable item
has a `WRK-NNN` identifier, whether or not anyone means to do it. An
identifier is never reused or renumbered; when an item is done or dropped it
is removed entirely, with no historical record left here. When the file is
empty, delete it.

---

## Status

The library-use gate is certified (`brokkr library --all --dataset denmark`
at `627883d7`: 12/12 scenarios, 17/17 capabilities). io_uring is exercised on
real data and its writer fixed. Multi-region extract admits its descriptors.
The planet regressions found by the RAM pass below are resolved:
`13ec0927` (cat --dedupe), `418739bd` (inspect --extended), `c97b9985`
(merge-changes), `5bfa37ab` (pool-heavy decode buffers) and `b1f522f7`
(apply-changes). The release itself still needs WRK-001 to WRK-005.

---

## Release

### WRK-001: Version bump and CHANGELOG date

CHANGELOG.md's section is headed `## 0.6.0 - 2026-10-30`; set the real date
when it ships. Both `Cargo.toml`s still say 0.5.0, and no tag after `v0.5.0`
exists. The 0.6.0 section was rewritten to net change against v0.5.0, one
line per entry; keep it that terse.

### WRK-002: Re-measure the rows that share the changed decode path

`5bfa37ab` changed the decode path these commands share, and their rows were
not re-measured: `check --ids --full` (stored `f6ba05d4`, 2.47 GB),
`extract --complete` with `-25,34,45,72` (`ce0bdcc0`, 5.27 GB) and
`multi-extract --simple -c`, 5 regions over `-10,35,32,72` (`4babae4d`,
10.3 GB). One stored `--bench 1` cell each at HEAD, one at a time, about 14
minutes of bygg in total. Their peaks are set-dominated, so little movement
is expected.

### WRK-003: Stored cells for the table's unstored rows

Four rows cite `--force` cells that were never stored: `cat --dedupe`,
`apply-changes` (4913, `zstd:1`), both `merge-changes` rows and
`inspect --extended`. A stored HEAD cell each gives the table a UUID to cite.

### WRK-004: The README planet table

The maintainer may move the README table to bygg; the bygg columns below are
a draft of it. Its walls would need best-of-N cells on a quiet host first.
Rows known wrong at HEAD regardless of host: both merge-changes rows (2 MB;
no stored cell ever paired the 7-OSC "54 s" with "2 MB": the 54 s matches
`b6e964cc` / `f4de3f8d`, both about 950 MB anon, and the 2 MB cells are the
267-273 s serial era), `cat --dedupe` (the old serial engine on another
host), `inspect --extended` (no commit or host on the row), and the rows
whose README figure predates the fixes listed in Status.

### WRK-005: Expect declarations in brokkr.toml (brokkr#71)

brokkr `b68f3e3` (installed) fails an answer run whose verdict contradicts a
declaration: `pbf.<variant>.expect` with `check-refs`, `check-ids`,
`check-index` = `clean` or `violations`, and `diff` keyed by OSC seq =
`same` or `differs`; dataset-level `expect.diff-snapshots` keyed by the two
files' xxhash digests in either order. Absent keys store both answers.
`brokkr man datasets` has the syntax. Each declaration must be what the file
actually gives, not an assumption:

- Planet `pbf.indexed`: `check-refs = "clean"` (`0accad98`, exit 0) and
  `check-ids = "clean"` (`4edab6b2`, and `f6ba05d4` for `--full`), both
  post-split cells at `627883d7`. `check-index` never ran on it.
- Denmark, germany, europe and the other regional extracts: every stored
  check cell predates the exit-code split (`854f9465`), so exit 1 there is
  ambiguous. A Geofabrik cut may carry relation members outside the region,
  so `check-refs` may genuinely report violations. One answer run each on
  post-split pbfhogg settles it.
- `diff` per OSC: a primary diffed against itself with its next OSC applied
  should be `differs` (denmark 4705, planet 4913, and so on).
- `expect.diff-snapshots`: planet base against `20260411`
  (`3ce64c66...` + `7facb638...`) is two different planets, `differs`.
- Deliberately broken snapshots: `unsorted` / `test-unsorted`
  (`bc689783...`) should give `check-ids = "violations"`, `test-no-index`
  should give `check-index = "violations"`. Confirm each with one run first;
  brokkr#71 notes `--unsort` breaks ID order only on large enough inputs.

---

## Planet RAM pass against the README table (2026-10-30)

Reference material for WRK-002 to WRK-004, not an item. The blocker
question was whether every command is still RAM-safe at planet on a 28-ish
GB host. Answer: yes. The highest peak is `inspect --tags -j 16` at 17.5 GB,
unchanged from the README.

Setup: host bygg (Ryzen 9 9950X3D2, 16c/32t, 30.5 GiB, 29.1 GiB available),
pbfhogg `627883d7`, one `--bench 1` cell per command after `fstrim`, input
`planet-20260223-with-indexdata.osm.pbf`. Peak anon RSS is the sidecar's
`anon` max. The README table was measured on a Ryzen 9 5900X (12c/24t), so
walls do not compare across the two columns (bygg is the faster host) and
are listed for reference only.

| Command | Peak anon, bygg | README | Wall, bygg | README wall | UUID |
|---|---:|---:|---:|---:|---|
| `inspect --tags -j 16` | 17.5 GB | 17.5 GB | 1m40s | 2m50s | `7855f2a4` |
| `build-geocode-index` | 13.4 GB | ~25 GB | 5m20s | 5m47s | `c261bd3c` |
| `add-locations-to-ways --index-type external` | 10.7 GB | 12.0 GB | 5m21s | 9m52s | `13668dda` |
| `multi-extract --simple -c` (5 regions, `-10,35,32,72`) | 10.3 GB | 9.4 GB | 10m53s | 14m44s | `4babae4d` |
| `extract --smart` (`-25,34,45,72`) | 8.2 GB | 11.17 GB | 2m20s | 4m28s | `d015bbe4` |
| `cat --dedupe` (n4) | 1.46 GB | 1.4 GB | 5m10s | 2h13m | unstored |
| `multi-extract --smart -c` (5 regions, `-10,35,32,72`) | 6.9 GB | 22.9 GB | 8m04s | 13m58s | `d4d8d0dc` |
| `extract --complete` (`-25,34,45,72`) | 5.27 GB | 4.7 GB | 2m02s | 3m42s | `ce0bdcc0` |
| `apply-changes --locations-on-ways` (4913, keep-untagged base) (n2) | 3.9 GB | 3.8 GB | 4m57s | 5m05s | `6e9a63df` |
| `extract --simple` (`-25,34,45,72`) (n7) | 3.09 GB | 3.0 GB | 1m52s | 3m42s | `6b3c88cc` |
| `renumber` | 3.19 GB | 3.3 GB | 2m00s | 3m11s | `ce4acd6a` |
| `apply-changes` (4913, `zstd:1`) (n8) | 2.36 GB | 2.4 GB | 2m05s | 4m29s | unstored |
| `tags-filter w/highway=primary` (two-pass) (n7) | 2.57 GB | 2.6 GB | 1m01s | 1m55s | `b8491cfd` |
| `check --ids --full` | 2.47 GB | 2.22 GB | 34s | 1m10s | `f6ba05d4` |
| `check --refs` | 2.30 GB | 2.17 GB | 35s | 1m0s | `0accad98` |
| `cat --clean version` (n7) | 994 MB | 750 MB | 4m01s | 5m34s | `a8a4384e` |
| `time-filter` (cutoff 2024-01-01) (n7) | 841 MB | 812 MB | 2m27s | 4m24s | `c33e5ddd` |
| `merge-changes` (4914..4920, 7 OSCs) (n6) | 286 MB | 2 MB | 9.9s | 54s | unstored |
| `getparents` (n7) | 288 MB | 506 MB | 13s | 22s | `dd453b3f` |
| `tags-filter -R highway=primary` | 659 MB | 688 MB | 38s | 52s | `55f84dee` |
| `diff --format osc -j 16` (base vs 20260411) | 653 MB | 634 MB | 4m11s | 4m54s | `607cd897` |
| `check --ids` (n7) | 566 MB | 504 MB | 31s | 57s | `4edab6b2` |
| `inspect --nodes -j 16` | 425 MB | 410 MB | 31s | 56.8s | `b1fc3106` |
| `getid --invert` | 58 MB | 102 MB | 56s | 1m31s | `7ee6c776` |
| `inspect --extended` (n5, n7) | 355 MB | 34 MB | 41s | 13m41s | unstored |
| `getid` | ~13 MB (n1) | 27 MB | short (n1) | 6.1s | `67d66e4e` |
| `sort` (sorted input) | 11 MB | 476 MB | 41s | 2m27s | `6bbe8ca4` |
| `cat` (indexdata generation) | 10 MB | ~200 MB | 1m00s | 1m27s | `c7b51564` |
| `cat --type way` | 10 MB | 10 MB | 30s | 45s | `3d4a36a8` |
| `merge-changes` (4913, 1 OSC) (n6) | 53 MB | 2 MB | 2.7s | 44s | unstored |
| `inspect` | <1 MB (n1) | 5 MB | short (n1) | 6.5s | `3ec07278` |
| `tags-filter -i w/highway=primary` (n3) | 7.49 GB | 7.0 GB | 5m16s | 7m57s | `9bf96ab2` |
| `diff -j 16` (text, base vs 20260411) (n3) | 605 MB | 586 MB | 3m04s | 3m48s | `f9fa4f6f` |

- n1: Too short for the 100 ms sampler (three samples); approximate.
- n2: The README row for this one was measured on a 9950X3D2 like bygg, so
  it is the one row whose walls compare directly.
- n3: Measured later the same day on brokkr `a8605c879`, after its fixes for
  brokkr#67 and brokkr#68; every other row ran on the morning's brokkr build.
- n4: A `--force` cell on `42ea567f` plus the borrowed-cursor sweep; the
  RSS-pass cell was `d271f04f`, 7.57 GB and 53m43s.
- n5: A `--force` cell on `13ec0927` plus the pipeline scan; the RSS-pass
  cell was `9b49e8fb`, 26 MB and 13m22s.
- n6: A `--force` cell on `418739bd` plus lexical batches with spill; the
  RSS-pass cells were `f24c29af` (895 MB, 30 s) and `bf6db512` (1.6 MB,
  24 s).
- n7: Peak from a `--force` cell on `c97b9985` plus right-sized, recycled
  decode buffers; walls unchanged per work phase and left as stored. The
  RSS-pass peaks were cat 1.24 GB, time-filter 1.16 GB, getparents 705 MB,
  check `--ids` 646 MB, extract `--simple` 3.52 GB, tags-filter two-pass
  2.84 GB, inspect `--extended` 571 MB.
- n8: A `--force` cell on `5bfa37ab` plus per-block framing; the RSS-pass
  cell was `f2b8beed`, 3.12 GB and 2m07s, and 2.97 GB after the decode
  buffer change.

Measurement artefacts in the RSS pass's percentages, settled: per-worker
peak was the same on both hosts for getparents, check `--ids` and
time-filter (23.5 vs 23.0, 21.5 vs 22.9, 38.6 vs 36.9 MB per worker at 30
vs 22 workers), so those rows grew with bygg's worker count; the extract
rows compare different bboxes (README `-10,35,32,72`, bygg `-25,34,45,72`);
multi-extract is about 4 % in consistent units, not 10 %. RSS fell on
`build-geocode-index` (the README's ~25 GB was probably total RSS; bygg's
total including file-backed mmaps is 18.8 GB), `multi-extract --smart`,
`extract --smart`, external altw, `sort` and `cat`.

---

## Residual memory above the README

### WRK-010: cat --clean at 994 MB against 750 MB

After `5bfa37ab` the peak sits in the relations phase. The 64-task window
holds framed vectors, each worker builds a fresh `BlockBuilder` per blob
(deliberate and measured, see "Facts learned the hard way"), and frames are
copied with `FramedBlobParts::into_vec`. WRK-020 is the structural fix.

### WRK-011: check --ids ways phase (566 MB against the nodes phase's 301 MB)

Each of the three phases spawns fresh workers and pools; the nodes phase's
buffers are freed before the ways phase builds new ones, yet the ways phase
peaks near twice as high. Not investigated.

### WRK-012: Allocator hook coverage by ownership pattern

The CLI's `cross_thread_alloc_pool` hook (`cli/src/host.rs`: two malloc
arenas, `M_MMAP_THRESHOLD` pinned at 32 MiB since `5bfa37ab`) is requested
per command. Per the hook's stated condition the call should follow whether
a command frees buffers on another thread than it allocates them (for
example writer-pool framing), not which commands showed a regression.
getparents, extract, tags-filter and inspect do not call it. apply-changes
is settled as not requested (WRK-013). Settle the rest per command.

### WRK-013: Why the allocator hook costs 20x on apply-changes

With the hook, planet apply-changes took 2606 s instead of 125 s, almost all
kernel time (23.3 cores kernel, 3.3 user), for 112 MB less peak. Recorded as
a measured exclusion in the hook's doc in `src/host.rs`. Which of the two
settings causes it, and the mechanism, are unknown. The same settings serve
cat, time-filter and the dedupe path, so the answer matters beyond
apply-changes.

---

## Memory and throughput: structural work

### WRK-020: Encode-side recycling

Encode into reusable worker storage, compress each block as it completes,
frame into output storage with a bounded return path from the writer.
`BlockBuilder::take_owned` takes `encode_buf`,
`FrameScratch::take_with_capacity` allocates a replacement per handoff and
`into_vec` copies the frame, so keeping a builder per worker alone recycles
nothing. Makes library throughput independent of the host's allocator
policy: embedded callers with `NoProcessActions` currently pay the mmap
churn the CLI hook avoids. `notes/todo.md` "Stale BlockBuilder::new
reserves nothing comments" rides with it.

### WRK-021: Separate offset-table workspace

`parse_and_inline_with_scratch` appends the inline string and group offset
tables to the payload buffer, which can grow it geometrically and relocate
the payload. Keeping the tables in a separate workspace the parsed view
borrows means the payload never relocates and a buffer's size is the
payload's.

### WRK-022: Byte-budgeted fused task engine

One execution model for decode, transform, encode and frame under one byte
budget, replacing the unordered classify pool, `run_ordered`, the writer's
separate rayon framing dispatch and apply-changes' private
scanner/worker/drain pipeline. Admit tasks in sequence order against the
budget; reserve a completion envelope before a task runs (piecemeal credit
acquisition deadlocks: every worker holds input credit while waiting on
output credit and the earliest task starves); release credit when storage
becomes reusable (ordering reservation at ordered delivery, output lease at
I/O completion). Makes transient memory a chosen constant independent of
worker count. Moving storage from workers to tasks does not lower peak by
itself at saturation; the budget value is the memory control, and fusion
shrinks the envelope so the same budget sustains more concurrency. Risks:
the admission protocol's liveness around the earliest task, oversized and
hostile blobs, cancellation and error paths that release leases, and a byte
ledger does not bound pages glibc retains. It is also the generic shape of
WRK-024 and WRK-025.

### WRK-023: Writer permits released at framing, not at ordered delivery

`PIPELINE_DISPATCH_PERMITS` are released when framing finishes, and
`ReorderBuffer::with_capacity(64)` in `src/write/pipeline.rs`
`deliver_in_order` is an initial allocation, not an admission limit, so
completed frames can pile up behind a slow earlier task: "64 permits plus a
64-slot channel" is not an end-to-end bound. The read pipeline already
carries permits to delivery. Every pipelined writer user is affected; the
dedupe path no longer frames through the writer.

### WRK-024: apply-changes drain admission bound

The drain's `BTreeMap` reorder has no bound (415 MB high water at planet);
completed blobs pile up behind a slow earlier one. A real bound needs
ordered admission before dispatch, with permits held through delivery, per
the comment in `drain.rs`. No counter measures the 256-slot drain channel's
occupancy; add one with the bound. Out of release scope because the
measured peak was per-worker storage, not the drain.

### WRK-025: apply-changes overlay reclaims superseded records

`CompactDiffOverlay` holds about 575 MB at planet and never reclaims records
superseded by later changes for the same ID (`notes/todo.md` has the older
entry). Compact superseded records as they are replaced, or collapse at
load, so the overlay holds only the final state per ID.

### WRK-026: Projection scanners for scan commands

check `--ids` needs IDs plus boundary and violation summaries; getparents
needs refs and members matched against the requested ID set. A borrowed
scanner skips offset tables and the general element interface, keeping
every structural and corruption check the command performs today. Shrinks
the envelope WRK-022 rations and cuts CPU. WRK-041 is the same scanner for
inspect; build one.

### WRK-027: Membership track for set-dominated peaks

The rows dominated by ID sets (extract, multi-extract, tags-filter two-pass,
check `--ids --full`, the apply-changes overlay): fewer coexisting sets,
release each set after its last dependent phase, a shared region-membership
representation for overlapping multi-extract regions, partitioned or
spillable dependency state where a hard ceiling matters. "Swap `IdSet` for a
compressed bitmap" is too shallow, since `IdSet` already has sparse and
dense chunks. Bolder for check `--ids --full`: on input proven strictly
sorted, strict monotonicity already rules out duplicates, so the dense sets
can go, with a fallback for unsorted input.

### WRK-028: Parallel compression for multi-extract

multi-extract's write phases run at 2-3 cores: five synchronous region
writers compress serially on the consumer thread, and workers spend about
17,000 thread-seconds blocked on result send behind them (about 600 s of
write phases at planet). Compression belongs inside the tasks, with
per-region ordering under one shared budget; five separately async writers
would each get their own buffering allowance, which is the wrong shape.

### WRK-029: Collapse byte-equal twin frames in dedupe

For entries with equal kind, range and frame length across inputs, verify
payload byte equality and collapse to one representative (and recognise the
same file passed twice by identity); the representative plans as a lone
entry and is copied through. Makes a self-merge a copy and keeps way
enrichments a rewrite drops. Deliberately not done in `13ec0927`: it is
benchmark-shaped, since two different snapshots share no frames. Semantics:
internal duplicates inside a passed-through twin survive, as they already do
in any lone blob.

### WRK-030: Hard memory bound for giant dense overlap components

A dense overlap component still pins the decoded blocks of its live overlap;
sparse tails are demoted to owned elements past `SWEEP_PINNED_BYTES`, dense
ones cannot be. A hard bound needs bounded sorted runs plus an external
merge, or an osmium-style per-source stream merge with globally
byte-bounded prefetch inside oversized components. Only `sort` of
adversarially unsorted input reaches this.

### WRK-031: Budget for oversized frames in the overlap sweep

The per-run chunk bound is `window x depth x max(OVERLAP_RUN_CHUNK_BYTES,
largest frame)`. A frame near the 32 MiB format limit (`--compression none`
on a huge block) travels alone and is not otherwise budgeted.

### WRK-032: Dedupe pass 2 buffered under --direct-io --io-uring

Pass 2 (sort and `cat --dedupe`) now reads `O_DIRECT` under `--direct-io`
unless the output splices and the plan copies some frame through
(`blob_merge::open_sources`). That case - `--direct-io --io-uring` with any
passthrough - still reads the sweep buffered. Closing it needs a second,
direct descriptor per argument for the sweep beside the buffered one the
splice uses, which changes the descriptor grant (`CAT_EXTRA_FDS`, the
`FdPool` doc, the open-count tests in `tests/fault_cat_input.rs` and
`tests/fd_pool.rs`); or a per-source choice (direct only for inputs with
no copied frame). Belongs with the deferred `--direct-io` reach redesign in
`notes/todo.md`.

### WRK-033: A sweep test at planet blob density

No test runs the overlap sweep at planet blob density (about 228k elements
per blob); nothing guards sweep memory. Resource tests should assert bounded
admission and progress, never a host's RSS.

### WRK-034: merge-changes validate-and-forward worker mode

Workers could validate a batch on borrowed attributes and forward the source
bytes instead of re-emitting. Saves numeric parse/format, string allocation
and escaping per element; a wall win only where workers, not readers, bound
the run. The driver now enforces the element hierarchy on the reader thread,
so what remains is attribute validation on borrowed bytes (what the sinks
check today) and an explicit preservation policy: source
spelling of coordinates, delete payloads (today stripped by
`write_delete_xml`), attribute order. Preserving source bytes is a policy
choice, not a correctness loss.

### WRK-035: merge-changes in-flight budget at planet

`merge_changes_credit_wait_ns` summed 15.6 s over the planet 7-OSC run:
readers park on the 64 MiB `MERGE_CHANGES_INFLIGHT_BYTES` budget. Whether a
larger budget shortens wall, or the pool is simply saturated, is unmeasured;
one cell at a larger budget, read against average cores, settles it.

### WRK-036: merge-changes --simplify and tags-filter OSC on the batch readers

`--simplify` still parses whole inputs into owned `ChangeStream`s per pool
task (22.8 GB in `3e3ef119`). The reader and batch stage of `c97b9985`
could feed its dedupe, and `tags-filter --input-kind osc` shares the
owned-sink re-emit. `diff --format osc` already uses `ParallelGzipWriter`
and could converge on the same ordered output stage.

### WRK-037: ParallelGzipWriter window size

At 32 workers the window admits up to 160 sequences x 2 MiB, about 320 MiB
of raw payload. Bounded, but not small; matters if anything reuses its
sizing.

### WRK-040: inspect fused validating projection scanner

After `418739bd` planet `inspect --extended` is worker bound (the dispatcher
waits on admission for 96 % of wall), so per-blob CPU sets wall. The worker
still constructs a `PrimitiveBlock` (offset tables appended to the payload),
runs `validate_groups_strict`, then iterates `Element`s with per-node info
structs and `keys_vals` rescans. One fused pass over the decompressed bytes:
read block parameters first (protobuf field order is free); tight
min/max/first/last loops over the id and coordinate columns; dense metadata
in one lockstep loop (every slot judged by value whatever its version, user
names through a per-block table of empty string indices, per CORRECTNESS.md
"Metadata coverage in inspect --extended"); one `keys_vals` delimiter
walk for tagged-node counts; plain Info parsed once; refs and members
checked for structure only; the stringtable UTF-8 check in a borrowed pass.
Validation is fused, not dropped: inspect keeps refusing every file it
refuses today. Traps: wrapping delta and scaling semantics (scaling raw
extrema is not equivalent under negative scales), repeated packed segments,
mixed-group iteration order, all-or-none dense info. Risk: a second wire
walker beside block.rs, so share varint, packed iteration and group framing.

### WRK-041: One scanner for every decoding inspect mode

Plain inspect's decode fallback, `--distrust-metadata`, `--locations`,
`--blocks` and `--extended` already share `418739bd`'s engine; with WRK-040
they become projections over one scanner. Afterwards the same scanner
serves WRK-026.

### WRK-042: Per-blob summaries written by the producer

A versioned per-blob extension carrying measured counts, ID boundaries,
timestamp range, bbox and metadata coverage would let a repeated inspect
skip decompression. Stored summaries are claims, so this is a second
product ("a description from claims") beside the measurement; partial
coverage could be filled by scanning only the blobs without one. Needs a
design document first.

---

## Correctness

### WRK-105: OSC sinks disagree on children of a deleted object

Children of an object in `<delete>` (tags, nds, members) are routed by the
driver to every sink. `CompactSink` (apply-changes) and `FilterSink`
(tags-filter) drop them without parsing; `OwnedSink` (merge-changes) parses
and stores them, and the writer strips them again (`write_delete_xml`). So a
malformed attribute on a deleted object's child (a bad role, a tag without
`k`) refuses in merge-changes and passes in the other two: one input, two
answers, with no parity test covering it. Pick one rule (likely: the owned
sink skips delete children too, which also saves work) and add the case to
`tests/osc_parser_parity.rs`.

Correction (checked 2026-10-31): the split is per callback, not per sink.
`FilterSink` skips a deleted object's `tag` before parsing, but parses `nd`
(`ref`) and `member` (`ref`, `role`) before its `current` check, so those
refuse in tags-filter too; `CompactSink` skips `tag` and `member` but parses
`nd` first. CORRECTNESS.md states the overlay's member exception and must
change with whatever rule lands here.

### WRK-052: Same-way fallback for unresolved refs in apply-changes -l

On a base without its untagged nodes, an OSC-modified way that keeps a ref
from its old version could take that ref's coordinate from the old version's
inline value, which sits in the blob the worker is already rewriting. That
would resolve many of the refs that make `-l` fail on such a base (about
2.8M on a europe daily diff), but not refs a way newly adds, and it carries
forward any coordinate an earlier run left stale. It needs its own
resolution and accounting (the global `loc_missing` would still count those
IDs). Not decided whether it is wanted at all, given the docs require
`--keep-untagged-nodes`. Background: the stale inline coordinates defect is
fixed (`81f3fbe4`), with `check --locations` (`669a31e6`) as its oracle.

### WRK-053: Ordered pool internal panic lingers behind parked waits

Residual of the altw stage 4 worker-panic fix (`64e478f8`). Stage 4 workers
and their setup hold an `AbortOnDrop` guard, so a panic in caller-supplied
worker code aborts the coord-payload router at once. A panic inside the
ordered pool's own machinery (admission, the result send) is outside those
guards: the gate closes, but a worker parked in `wait_ready` for a way blob
slot stays until stage 3 publishes it or observes cancellation. That would
be a pool bug, not caller code; the Panics section of `src/ordered_pool.rs`
states the general behaviour. The pool-level alternative (a task panic as an
ordered failure that runs `abandon`) was rejected because `abandon` would
still wait on earlier tasks parked on the same router. Not decided whether
anything beyond the documentation is wanted.

---

## Documentation and comment corrections

### WRK-063: Resident bytes in the writer reorder buffer

`writer_reorder_high_water` (seq span) and `writer_reorder_filled_high_water`
(occupied slots) are documented and recorded, but no counter gives the bytes
held there, so memory attribution from them stays inferred. A byte peak needs
per-chunk accounting across `Framed` / `Raw` / `RawChunks` (allocated
capacity, not length), `CopyRange` (no bytes) and error slots, added on push
and subtracted on pop in `deliver_in_order`; it is per blob on every
pipelined writer, so it needs its own matched A/B.

### WRK-106: pipeline.md calls the permit pool the writer's memory bound

`reference/pipeline.md` (frame scratch section) says the writer channel "is
deliberately not the memory bound - the permit pool is". Permits are released
when framing ends (WRK-023), so they do not bound the reorder buffer either;
fix the sentence with WRK-023's outcome.

---

## Tooling

### WRK-070: bz pbfhogg#18, sidecar --compare and repeated phase names

Waiting on brokkr's source: whether `brokkr sidecar --compare` matches a
repeated phase name to its first occurrence only. No stored run repeats a
marker name, so existing data cannot settle it. Matters for multi-input
cat `--clean` per-kind rows.

### WRK-071: inspect rows stored under other command names

`brokkr results --command inspect` misses the denmark `fileinfo --extended`
row `e4f4508c` (`23862d1`), stored under command `fileinfo`. The hotpath
rows stored as `inspect` are tags-count runs, so no profile of the
full-decode path exists.

### WRK-072: Allowlist gaps met during measurement

`sudo fstrim -av` (AGENTS.md asks for it before a matched suite) and
`taskset` (to run bygg at a README-like worker count) are not allowlisted,
so implementers skipped both. The maintainer's call.

### WRK-073: Digest v2 cost

brokkr#61 landed `brokkr digest` (brokkr `cb83a5f`). Its single-sample
denmark indexed cells put digest at 503 ms on `d2d8836a` against 701 ms on
`4157567d`, about 39 % slower, after the digest `v2` change (`16154b0d`).
The ticket suggests a `--form content` run to separate drift from code.
The recorded "no cost" claim for v2 needs re-checking against that.

### WRK-074: Guard the input-eviction capability proof

The `host.page-cache-keep.v1` capability's proof rests on
`fadvise_input_evictions` counting every input eviction, which in turn
rests on the variant split `Advice::EvictInput` versus `Advice::DontNeed`
and an audit that `FileReader::evict_consumed_prefix` is the only input
eviction. Nothing enforces that: a new eviction path through `DontNeed`
would silently void the capability. A textlint or a type split would guard
it.

---

## The measurement campaign

Matched A/B cells planned at the end of the hygiene drain to bisect what the
drain changed. Today's regression fixes did not need them, but they remain
the way to find any regression the RAM pass could not see. "A vs B" means
matched, interleaved `--bench 1` cells with `--commit`; when a pair moves,
bisect inside it. Prefer stored cells where they already answer the
question.

### WRK-080: Read path range

`brokkr read --dataset planet`, `ce6f20d9^` vs `307982d7`, the cheapest
broad check. Covers the `BlockBuilder` cold refusal (`ce6f20d9`), the raw
scanners and `packed_varint_count` (`e61e0ad7`), the single `run_pipeline`
engine (`dd312160`) and the one frame decoder (`307982d7`: `BlobReader`'s
prefix through one `fill`, and `HeaderWalker` copying each header into its
buffer, which would show on warm-cache dense inputs).

### WRK-081: packed_varint_count, ruled measure first

`brokkr read --dataset planet`, `e61e0ad7^` vs `e61e0ad7`, plus one
decode-bound command (`cat --clean` on europe), required whatever WRK-080
shows. The per-byte position counter probably stops the loop vectorizing
(estimated 2-5 % on decode-bound planet commands); BUG-191's width check was
rejected (`bc38ad55`) for exactly that cost. Inside noise: keep. Above: keep
the exact refusal but restore the vectorizable terminal-byte count, with a
masked eight-bytes-at-a-time check for a run of nine continuation bytes and
the per-byte scan only for an array that has one. The maintainer may
instead revert to the plain count (a malformed tenth byte slips through);
the measurement decides. `packed_uint32_count` tracked position before.

### WRK-082: The validated-invariant trio

`brokkr read --dataset planet` plus one tag-heavy re-encoding command
(`tags-filter` or `inspect --tags`), `5a9d40e4` vs `85c1428c`. Covers
`e28fdf69` (`take_message` slot check per element parse, elements parsed
twice), `9c85d37e` (a cold panic call per element and two per dense tag
varint, through a generic `#[inline]` `#[track_caller]` helper) and
`85c1428c` (a `Result` per kept element on every re-encoding command, a push
loop instead of `extend` for roles). Bisect only above 3 %.

### WRK-083: The partial-index guard

`brokkr read --dataset planet`, `84f50b31^` vs `84f50b31`. An armed frame now
parses indexdata (a 42-byte copy and a deserialize) on reads that skipped it
(the pipelined reader without a blob filter, `par_map_reduce`) and compares
once per data frame everywhere.

### WRK-084: Ordered pools

`56a50e0d` added windows where there were none (`default_window`, 8 per
worker, floor 32, never tuned), claim workers instead of a dispatcher,
`WILLNEED` advice moved from the dispatcher to the consumer (one
`posix_fadvise` per consumed item on the serial writer thread, further
ahead than before; it can enter reclaim at planet scale), failures in
schedule order, passthrough prefixes copied between decode results. Risks
named: `OrderedGate::advance` takes the mutex and `notify_all`s on every
consumed item (a wake syscall even with no waiters, about 1 us per blob; a
thundering herd when the window is full, stage 4's steady state); stage 1
workers parking behind a slow head for no memory gain; with
`--keep-untagged-nodes`, stage 4 copying the node prefix only after the
first way result. Pair `60e1b691` against `c2409c45` (the shape that ships;
`56a50e0d` itself carries the window-deep channel `c2409c45` removed) and
bisect inside if it moves. Phase split first, in this order: altw external
on planet at zlib:6 (stage 1 and stage 4 walls, peak anon, `s4_*`);
`extract --strategy smart` on planet with the Europe bbox; `multi-extract
--regions 5` on Japan and Europe (its region writer gained `WILLNEED` here,
measured a win on europe only, ruled kept); `repack` on europe or planet;
tags-filter two-pass and getparents on planet; time-filter on planet.

### WRK-085: extract simple on the unordered driver

`8928de18` moved extract simple's two pools onto the unordered driver and
gave them `WILLNEED` (ruled kept, to be measured): `extract --simple` on
planet and europe, `8928de18^` vs `8928de18`.

### WRK-086: cat --dedupe on Denmark and Japan across the engine move

`79d2e923^` vs `79d2e923`, then HEAD. Pass 1 moved to `HeaderWalker` preads,
passthrough to coalesced `copy_file_range`, overlap runs to dedicated sweep
threads; `13ec0927` has since replaced the sweep itself.

### WRK-087: sort on planet across the engine move

`sort --dataset planet`, `79d2e923^` vs `79d2e923`. Expected unchanged: no
runs, no threads, one coalesced copy.

### WRK-088: sort on the Denmark unsorted snapshot

`79d2e923^` vs `79d2e923` on snapshot `unsorted` (or `test-unsorted`, the
same file). Sort's recorded trade from the pass-1 move: a planet win, a
dense-europe pass-1 regression.

### WRK-089: sort --force on unindexed input

`sort --force` on an unindexed europe or planet input (`pbf.raw`),
`060c2f74^` vs `060c2f74`: the `SORT_INDEX_BUILD` wall and peak RSS. Every
unindexed blob now pays the combined ID-and-tag scan in serial pass 1, and
its serialized key set is held to pass 2 (up to GBs at 8k-encoding blob
counts). `plan` adds four per-frame `Vec<bool>`s and about 60 B per blob.

### WRK-090: diff byte-equal skip on planet

Planet `brokkr diff` (base against the apply-changes output), `2c12a955^` vs
`2c12a955`: should show the restored byte-equal skip on the default path
(japan `diff -c` went 50.0 s to 6.8 s with it, before the July default flip
lost it). The same cell for `diff -q`, which takes the skip too
(`f0fe01fc`), and for the text drivers' optional writer (`dfe45a35^` vs
`dfe45a35`, one predictable branch per write site, at the default `-j` and
`-j 1`).

### WRK-091: diff-snapshots across the block-pair engine

Planet `diff-snapshots --from base --to 20260411`, text and osc,
`2c12a955^` vs `2c12a955`, expected unchanged. Recorded sequential planet
figures, which cannot be re-measured: text 2134 s (`a9d430f2`), osc
2225.6 s, peak anon about 28-41 MB per phase; header walk about 15 s cold.

### WRK-092: diff -j 1 on europe, with its decision

A `-j 1` europe cell for wall and peak anon. `-j 1` now does a whole-file
header walk of both inputs first, then preads per blob from the walker's
descriptor re-advised `SEQUENTIAL`; it lost page-cache eviction,
`--direct-io` and `MergeBuffers` recycling. Verbose diff now shards, so its
output goes through scratch and a copy. If clearly worse, give one-shard
runs a streaming read back, without restoring a second engine.

### WRK-093: Unindexed sequential diff

`brokkr diff --dataset denmark --variant raw` (europe if it shows), with and
without `--type way`, `8881415e^` vs `8881415e` (per-element order check,
per-block kind classification, excluded kinds dropped unmerged; expected
faster under `--type`, slightly slower per element otherwise). Then
`94743e65^` vs `94743e65` separately (borrowed elements instead of owned
conversion, expected faster; the two sides' decoded-block peaks can now
coincide) and `126cbce2` (one `DecompressPool` per merge). No measurement of
the unindexed path exists at all. Check that brokkr's `diff` takes
`--variant raw`.

### WRK-094: Plain cat across the one frame decoder

Plain `brokkr cat --dataset planet`, `307982d7^` vs `307982d7`. The raw-frame
walks still zero-fill (`FileReader` implements only `read`, so std's default
`read_buf` zero-initialises) and an unsized `Take::read_to_end` starts at
8 KiB and doubles, about 4 read calls per 1.8 MB payload plus an extra copy
through the BufReader; estimated 25 us per blob, probably hidden if
I/O-bound. If it costs: implement `read_buf` on `FileReader` (which would
also remove a memset of every payload on every `BlobReader` path) or go back
to `resize` plus `read_exact`. The same cell covers cat's fd admission
(`56f8d9a9`) and the reframe's `MAX_BLOB_DATASIZE` compare (`f9625b94`).

### WRK-095: cat --clean version across the single ordered pass

`cat --clean version` on planet, `a05b3d25` vs `8da26b23` (one ordered pass
per input instead of three per-kind phases, window 64, not tuned): wall,
peak anon, stall counters, and whether workers stall behind a slow head
blob. Then repeat at HEAD, which has the per-kind rows back
(approximately). Baseline `f2315551`: 333.8 s, 750 MB peak anon.

### WRK-096: Writer cluster

One A/B of `ae646053^` vs `52690f1e` covers the per-block dispatch
(`ae646053`), the shared pipeline spawn (`3ab1b4ce`), the checked frame
header encoder (`f9625b94`), the one-meaning writer counters (`62046948`),
the shared ordered-delivery loop (`7abba0df`) and the OSC gzip writer's
`ReorderBuffer` (`52690f1e`). A block-encoding command on europe or planet
(repack or cat `--clean`) plus planet `diff --format osc` covers it. Bisect
only if it moves.

### WRK-097: altw raw scanners read fallibly

`e61e0ad7^` vs `e61e0ad7`, altw external on planet (three `read_varint`s in
lockstep instead of zipped `PackedSint64Iter`s, whose iterator swallowed
errors on purpose). Forced sparse at planet thrashes for hours
(`notes/todo.md`, "altw sparse path"); use europe or north-america for
sparse. This pair cannot separate the scanners from `packed_varint_count`.

### WRK-098: Shared versus separate input descriptors

About eight commands hand the `HeaderWalker`'s `FADV_RANDOM` descriptor to
their pread workers (classify, tags-filter, extract smart and multi,
getparents, geocode pass 1.5, diff's shard walk, altw sparse passthrough),
while altw external and apply-changes keep separate descriptors on an
argument (`a3f1a1bc`) never measured. The split may track access pattern
(selective readers with `FADV_RANDOM` plus `WILLNEED` against stage 4
reading every blob), so measure both shapes: for example altw external
stage 4 at europe scale with a variant handing the walker's descriptor to
the workers. Renumber is not on the shared-walker side, and `InputFront` is
a probe closed before any pass.

### WRK-099: apply-changes -l node boundary on the drain channel

`-l` on the planet `altw-keep-untagged` base, `ff0b0a37^` vs `ff0b0a37`: the
node boundary rides the shared 256-slot drain channel and publishes from one
readiness check. Probably neutral. Ride along: the OSC arena's `str::get`
instead of `from_utf8` per key, role and user (`20d10610`, argued cheaper).

### WRK-100: apply-changes -j 1 cost

What `-j 1` (accepted since `9744a9fc`) costs in wall and memory; nothing is
known.

### WRK-101: build-geocode-index codec commits

Planet, `d510bbd7` vs `067be2d8`, covering `64141296` and `067be2d8`; planet
baseline 347 s.

### WRK-102: digest walk folded into the decode pass

`c0f67d9e` (ADR-0009) touched the shared fused engine: `getid` or
`tags-filter -R` on planet, `c0f67d9e^` vs `c0f67d9e`. Expect noise.

### WRK-103: The Mutex Receiver dispatch question

Read `classify_unordered_desc_lock_wait_ns` against
`classify_unordered_desc_recv_wait_ns` on a planet tags-filter or extract
run (`reference/performance-history.md` has the history). Stored cells may
already answer it.

### WRK-104: Counter name sets survive the refactors

Observability, not performance: compare `brokkr sidecar <uuid> --counters`
name sets from an altw external Denmark run at HEAD and at `9dcf53a7^`, and
an apply-changes and a renumber run for `13dd02aa`.

### Campaign reference: how to measure

- Baselines first from `brokkr results`: a recent stored cell on the same
  host is the baseline; run only the variant. Variants run uncommitted with
  `--force` and are not stored, so record their numbers in
  `reference/performance.md`.
- Interleave matched cells when both arms must run: `--bench N` is best-of-N
  within one cell and cannot cancel drift between cells.
- Noise floor: ignore differences below 2-3 %. Planet single-sample walls
  carry a 5-8 % environmental band on the bench host, and a cold schedule
  header walk adds about 6 s per walk (`notes/todo.md`).
- Read the phase split before calling any delta: `brokkr sidecar <uuid>
  --human`, and `--compare` for a pair (check its host-difference annotation
  first). `brokkr sidecar <uuid> --stat anon` gives peak anon cheaply.
- One cell at a time, never in parallel. Old commits build through
  `--commit <ref>`; the first build per commit is a cold rebuild.
- Numbers written into markdown carry commit hash and hostname.

### Campaign reference: reading rows across these commits

- Rows from `56a50e0d` up to `c2409c45` (the ordered pools' result channel
  as deep as the window) do not compare on send-side counters (`s4_send_ms`,
  `WAIT_S4_SEND`, `WAIT_P2_SEND`, `altw_pass2_send_ms`) or on consumer-bound
  memory.
- `6592af1f` (protohoggr 0.5.0) changed output bytes: `BlockBuilder` output
  shrinks for dense uid / user_sid deltas of magnitude `2^30` or more.
- `CAT_NODES_*` / `CAT_WAYS_*` / `CAT_RELATIONS_*` are absent from
  `8da26b23` until `168e581a` and since then mean write-side kind
  transitions, so per-kind rows against `f2315551` are approximate;
  `SORT_OVERLAP_PARALLEL_*`, `DIGEST_ENVELOPE_*` and `writer_write_ns` are
  gone for good.
- Hotpath names moved: getparents, time-filter, repack, degrade and
  multi-extract's writer report under `parallel_classify_phase_ordered`;
  command pipelines under `run_pipeline` instead of `run_pipeline_fused`;
  cat `--clean`'s walk under
  `build_classify_schedule_blob_filtered_accepting_partial`.
- Every ordered pool records `ordered_pool_*_wait_ns`, every unordered
  classify pool `classify_unordered_*_wait_ns`: one process-wide running
  total emitted after every pool run. `classify_unordered_desc_lock_wait_ns`
  is the shared-receiver mutex's own cost only while
  `classify_unordered_desc_recv_wait_ns` stays near zero.
- `5bfa37ab` changed decode-buffer sizing and pool retention everywhere:
  peak anon of any pool command compares across it only with that in mind.
- Exit codes: `854f9465` split them (1 an answer, 2 a failure; before, both
  were 1); `50a9fc06` made `diff --format osc` exit 1 on differences (it
  always exited 0 before). brokkr `39d0988` stores exit 1 as an answer on
  builds with the split; read the stderr of a pre-split `--commit` cell that
  exits 1.

### Campaign reference: inputs

- The planet `altw-keep-untagged` variant for `apply-changes -l`, passed by
  brokkr's `--locations-on-ways` (the plain `pbf.altw` base is refused: OSC
  4913 references about 4.2M nodes it dropped as untagged).
- A Denmark unsorted snapshot (`unsorted` and `test-unsorted` are the same
  file) and an unindexed europe or planet input (`pbf.raw`).

### Campaign reference: unmeasured changes with no cell of their own

Each was argued neutral or below noise. Listed so a bisect landing near one
knows what it changed.

- Write side: `frame_in_scratch` (`1db71d8a`, `f9625b94`); the permit taken
  through `acquire_permit` before spawn (`1db71d8a`); `fadvise::advise` on
  every `posix_fadvise` (`d510bbd7`); the copy loops made two (`bd2de78d`).
- Read side: `BlobReader::next` as `next_frame` plus `into_blob`, eviction
  before the envelope parse (`8881415e`); `OrderWatermark`'s mixed-sign
  `min_id` (`8881415e`); one `statx` per direct open (`ea17fd86`); the
  scratch borrows in the fused engine (`c0f67d9e`); the OSC arena reads
  (`20d10610`); `BlobReader::leading_header` judging the first frame before
  its body on every `ElementReader` (`418739bd`).
- Counters: one struct per altw stage (`9dcf53a7`); `record_max` on the two
  production `fetch_max` sites; apply-changes' and renumber's sets
  (`13dd02aa`); the unordered pools' gated stall helpers (`003fe717`); read
  metrics emitted after the scope joins, plus the fsync funnel (`8384fa29`).
- Pools: the accumulate pool's stop flag (`95eea09b`); the unordered
  classify pools on one driver (`00ed5044`); the altw way reframes' shared
  block walk (`285df56d`).
- Commands: `apply-changes --force` reframing untouched unindexed blobs in
  the worker (`f703b02b`); degrade's sort-preservation guard (`9cce24b3`);
  the `expect_used` restructurings (`11becae7`); `check --ids`' extra
  type-order comparison (`4c254750`); input pinning (`926d5183` onwards).

---

## Handoff

What a session picking this up needs that is not written elsewhere:

- **brokkr state.** brokkr#65 landed (`39d0988`, exit 1 stored as an answer
  on split builds) and #71 landed (`b68f3e3`, declared answers; WRK-005).
  #64, #67, #68, #69 and #70 are answered. Stored short-run rows from before
  brokkr `a8605c879` carry up to ~100 ms of wall error.
- **io_uring on bygg** needs memlock raised for the session (the deferred
  io_uring item in `notes/todo.md`); the writer tests skip otherwise.
- **Filing in brokkr's bz from here:** the Bash hook rejects a command that
  names brokkr together with a redirect, a pipe or backticks, so pass the
  body with `-b '...'` in single quotes and no backticks.
- **Never `brokkr verify` above germany** (AGENTS.md). `brokkr verify` has no
  `inspect` or `merge-changes` subcommand.
- **Do not `git stash` with a dirty `.brokkr/results.db`:** a stash dropped a
  stored row on 2026-10-30.

## What the maintainer has made clear

- **Performance on normal input is what matters; corrupt input is not.** A
  check that only refuses malformed input and costs per-byte or per-element
  work on a hot path is the wrong trade (BUG-191's width check was reverted
  on that ground). Such findings end as comments plus a reference-doc
  correction.
- **A silent wrong answer is worse than a refusal.** An empty user or role,
  a default metadata block, fewer nodes: refuse instead. For a broken
  internal invariant, a release-mode panic over a `debug_assert!`.
- **Follow protobuf's specified decoding where it costs nothing**, with
  osmium as the parity reference. Deviations are deliberate and recorded in
  DEVIATIONS.md.
- **Commands fail loudly and point at `cat`.** Every command refuses a
  partially indexed input except `cat` (every mode), `check --index` (a
  violation) and `inspect`'s descriptions. `cat` is the recovery command.
  `notes/todo.md` holds the wider "`cat` as the universal cleaner" and
  "refusals name both remedies" projects.
- **Inputs are regular files in every mode**; FIFOs, sockets and devices are
  refused at pinning.
- **Indexdata is believed (ADR-0013)**, including the header's
  `Sort.Type_then_ID` claim; `check --index --ids` is the admission test.
- **Observability is a contract.** Counter names, units and the `WAIT_*`
  spans survive refactors. `_ms` counters truncate per sample on purpose;
  keep the unit and recording point when moving timing code.
- **`reference/performance-history.md` is a record of numbers, not of
  machinery.**
- **Do not remove ignored `direct_io` parameters.** They are deliberate
  plumbing for the deferred CLI-surface redesign.
- **One engine over two.** A slow special case gets a better path inside the
  one engine, not a second engine back.
- **The record informs, it does not rule.** Memory moves rather than
  vanishing, and several "obvious" wins measured as losses; but code,
  toolchain and models have moved, so an old failure may have been its
  execution, not its premise.
- **Commits stay granular** so `--commit` bisects land on one decision.
- **protohoggr is the maintainer's crate.** A defect there is fixed there.
- **BUG-009** is the maintainer's own project.
- **571 MB for `inspect --extended` at planet is fine** (2026-10-30): modest
  memory at planet scale is not a reason to hold back a large wall win.

## Facts learned the hard way

- `BlockBuilder` holds `Rc<str>` and is not `Send`; the ordered pool builds
  per-worker local state on the worker's own thread for this. A fresh
  `BlockBuilder` per blob in cat `--clean`, time-filter and getparents is
  deliberate and measured.
- **glibc realloc** (`research/glibc/malloc/malloc.c`): an mmapped chunk
  grows by `mremap`; an arena chunk grows in place or moves. Freeing an
  mmapped chunk raises the process-wide `mmap_threshold` to its size (32 MiB
  cap) and `trim_threshold` to twice that, which is why buffers that stop
  being freed can push later allocations onto mmap (time-filter paid 29 s of
  kernel time for it before `5bfa37ab` pinned the threshold). Read the source
  rather than reasoning from memory.
- **Per-blob decode buffers at planet** (`5bfa37ab`): the old 4x-compressed
  reservation plus the table append put every buffer over the pool's cap,
  and glibc kept the freed memory resident. Raising the cap alone made it
  worse; right-sizing had to come with it.
- **`par_fold_blobs` budget rule** (`read/reader.rs`): a budget below a full
  partial batch plus one maximum blob blocks forever. Compile-time asserted
  for production constants.
- **The claim-counter pools** (altw external stages 2 and 3, geocode pass
  1.5) lock a first-error mutex on every claim.
- **apply-changes `-l` barrier**: the node boundary is a
  `DrainMessage::NodeBoundary` on the drain channel; the 25 ms
  `DRAIN_RECV_TIMEOUT` only wakes the drain to notice failure or
  cancellation. Fault-plan hooks cover every barrier ordering
  (`tests/fault_apply_changes.rs`).
- **Premises drift.** Research notes were wrong on several load-bearing
  points when checked against code (the pool-RSS cause, the pwrite frame
  size, apply-changes' real holder); read the code before building on any
  claim here.
- **Sparring found real bugs** each time codex was asked to look past the
  proposal. `review` refuses stdin over 20,000 bytes, and a session idle for
  about an hour is refused as cache-cold.
- Shell here: `mv` is not allowlisted (`cp` then `rm -f`), and `rm` prompts
  without `-f`. There is no `/usr/bin/time`; for the peak RSS of a run too
  short for the sidecar's sampler, the user runs
  `TIMEFMT='%M kB max RSS'; time <binary> ...` with `!`. Debug builds do not
  tell you about release residency. A subagent working alongside reports
  your own uncommitted edits as someone else's.
