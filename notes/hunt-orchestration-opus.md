# Hunt: verifier orchestration, processes and the reference cache

Scope: `crates/verify/src/` main, all, run, selection, harness, process,
stream, reference, reference_cache, tunables, argv, output, and what the
checks make them do. Read from code only; every count below comes from
reading the check bodies, not from measurement, so treat the numbers as
estimates.

## Headline

The verifier's wall time is not spent where the reference cache looks. The
cache memoizes reference *executions* (osmium sort, cat, extract,
apply-changes, ...), and those are mostly hits on a warm machine. What it
deliberately does not memoize is the work that dominates: `osmium cat -f opl`
decodings of whole PBFs, which `opl_compare::decode` marks `.uncached()`, and
which every comparison redoes from scratch, materializing an OPL text file
several times the PBF's size and reading it back once.

A census of `verify all` on a sorted, indexed denmark (the default) gives
roughly 90 full-dataset-sized OPL decodings, plus many subset-sized ones:

| check | full-size decodes | why so many |
|---|---|---|
| sort | 3 | `same_elements` (2) then `check_sorted(ours)` (1) |
| cat | 12 subset decodes, about 4 full equivalents | `compare_outputs` decodes each side twice (order check, then compare), per type |
| cat --dedupe | about 14 | the input decoded once per merge (6 merges), plus `order(input)` and each derived operand's `order` |
| add-locations-to-ways (all) | 10, the largest OPL (FULL, inline coordinates) | osmium's output decoded twice per engine (CONTENT order, FULL compare), each engine output 3 times (order, vs osmium, parity) |
| repack | 8 | 4 cases, each re-decoding the same reference (the input) |
| degrade | about 14 | 7 compares; the reference (the input) decoded 5 times, `drop-a` 3 times |
| degrade -l | about 16 | as degrade, plus the coordinates compare |
| apply-changes, apply-changes -l | 3 each | CONTENT order check, then the NO_LOCATIONS decode of the same output |
| diff --format osc | 10 | `new` decoded 4 times under CONTENT, `roundtrip-pbfhogg` 3 times |
| renumber, diff | 2 each | |

The unique (file content, policy) pairs among those are perhaps 40; the
unique files about 35; and because many pbfhogg outputs copy input blobs
verbatim, the unique *blob payload bytes* behind all of them are on the order
of ten denmark-equivalents, not ninety. Within a run, the same denmark input
is decoded to OPL more than twenty times. Across runs, every decoding is
redone even when neither osmium nor the file changed.

So the redesign target is clear: make osmium's decoding of PBF bytes a
first-class, content-addressed, shared, persisted artifact, streamed instead
of materialized, and run independent work concurrently. Everything else in
this report is second order.

## 1. Decoding as a content-addressed service (high-conviction rewrite)

### The bottleneck

`opl_compare::decode` is the funnel for every element comparison
(`compare`, `compare_sequence`, `compare_paired`, `compare_with_opl*`,
`order`, `osmium_derive_roundtrip`), and through it `Harness::same_elements`,
`check_sorted`, `check_sorted_counted`, `verify_reencode::compare` and
`counts`. Each call:

1. launches `osmium cat INPUT -f POLICY -o FILE` as an uncached `ToolRun`
   (staged under the cache directory, then hard-linked into the check
   directory),
2. waits for the whole OPL file to be written (several GB for denmark at
   FULL),
3. merges it, line by line, against the other side's file,
4. deletes it.

Nothing about the result survives the call. The structure guarantees the
duplication in the census: a comparison is parameterized by paths, the
decoding is an implementation detail inside it, so two comparisons that
share a side, or a check that orders and then compares the same file, or six
merges against the same input, each pay a full decode. And the decision to
mark it uncached is correct for the cache as designed (an OPL the size of
the input, published only to be evicted), which is why the fix is a
different artifact, not a cache flag.

### The redesign

A run-wide `Decodings` service, owned by the harness services layer (see 2),
with this contract:

- **Key**: osmium identity, the decode format (policy), and the *content* of
  what is decoded. At minimum the file's digest; better, the digest of each
  data blob's payload bytes plus the parsed header feature set (see below).
- **Single-flight, memoized in the run**: the first request decodes, every
  later request for the same key in the run gets the same handle; concurrent
  requests wait on the one in flight.
- **Persisted across runs**: a decoding of given bytes by a given osmium is a
  reference fact, not a pbfhogg verdict, so it belongs in the reference
  cache. A rerun after a commit that did not change a command's output bytes
  then pays only pbfhogg and hashing for that command's checks.
- **Derived facts computed once per decoding**: strict order, per-kind
  counts, first and last key. That is what `order`/`check_sorted` exist to
  produce; they stop being separate osmium runs.
- **Streamed**: osmium writes OPL to a pipe; the service consumes it into its
  stored form (compressed text plus per-chunk summaries) and into any
  consumer waiting on it. No full OPL file is ever written for a comparison.
- **Compact stored form**: zstd-compressed OPL text per chunk, with a chunk
  summary (record count per kind, first and last key, internally strictly
  ascending or not, a 128-bit digest of the chunk's lines). OPL compresses
  very well; a denmark FULL decoding should land around a gigabyte, and the
  summaries are megabytes.

**Blob granularity is where the large win is.** OPL decoding of a PBF is a
concatenation of independent per-blob decodings: dense-node deltas and string
tables are block-local, and an OPL line depends only on its object. The only
file-level inputs are the header's features (required features gate the
read; `LocationsOnWays` governs inline coordinates). So key the stored chunks
by (osmium identity, format, header feature set, blob payload digest), using
the `Blob` bytes, not the `BlobHeader`, so indexdata rewrites do not defeat
it. Then:

- the input's blobs are decoded once per run, for all ~20 checks that decode
  it;
- every pbfhogg output that copies input blobs verbatim (indexed `cat
  --type`, `cat --dedupe` passthrough, `sort` of sorted input, the untouched
  blobs of `apply-changes`, the unremoved blobs of a `degrade --drop-ids`,
  `strip-indexdata` copies) needs only its rewritten blobs decoded;
- a rerun needs only blobs whose bytes changed.

Mechanically: for a file, find its blobs with no stored chunk; write one or
more synthetic PBFs (the file's own header frame verbatim, then the missing
blobs' frames verbatim, in order) with the verifier's existing frame reader;
run `osmium cat` on each, streamed; split the OPL stream back into per-blob
chunks by element count. The counts come from the verifier's own blob decode
(`pbf_frames::blob_ids`), and each chunk's first and last key must match that
blob's first and last ID, or the split is refused and the file is decoded
whole (a disagreement between the verifier's decoder and osmium is itself
worth an error). Several synthetic PBFs can be decoded concurrently, which
also parallelizes a single large decode.

**The merge becomes chunk-aware**: when both sides are positioned at the
start of the same stored chunk and the previous keys allow it, the whole
chunk is `same` without reading a line; only differing chunks are
decompressed and walked line by line. A passthrough-heavy output compares
against its input in time proportional to its rewritten blobs. Callers that
need every pair (`compare_paired` in renumber) or every line
(`apply_changes::write_expected`, the derive model) still read text, from the
stored chunks rather than a fresh decode.

### Why it is plausibly high-payoff

The census says decodes are the bulk of the work, and the arithmetic of the
current design (about 90 full decodes, each writing and re-reading several GB
of text) fits the reported 12 to 20 minutes on denmark far better than any
other cost in this scope. In-run memoization alone should remove more than
half of them; blob granularity should take the remainder toward the unique
rewritten bytes; persistence takes an unchanged rerun to roughly the cost of
the pbfhogg runs plus hashing. Streaming removes the scratch volume that,
per the comment on `ELIGIBLE_DATASETS` in `main.rs`, already filled scratch
on germany.

### Risks

- **Coupling**: one memoized decoding now feeds many checks, so a bug in the
  service affects all of them at once. Every stored chunk carries its digest,
  and verification on read is cheap next to a decode.
- **Per-blob equivalence is an assumption about libosmium**: that a blob
  decodes the same in a synthetic file carrying the same header frame. It
  holds by how the PBF format is defined, but it should be held by a control
  in the spirit of field-policies: whole-file decode against blob-assembled
  decode on a real slice, byte for byte.
- **Header-dependent decoding**: if any header field besides the feature
  lists turns out to affect decoding, the key must carry it. Keying on the
  full header frame's digest is the conservative fallback, at the cost of
  cross-file reuse.
- **Strictness diagnostics**: today a disordered side ends `compare` in
  `Err(Failed)` naming the file and line. The chunk summaries give the same
  information (which chunk, which key); the message has to stay as specific.
- **Failure listings**: unchanged in content, since differing chunks are read
  as text exactly as now.
- **Storage budget on germany**: compressed decodings of germany are still
  large. The service needs its own byte budget and eviction, and a re-decode
  on a miss is always correct.
- **Hash-equality shortcut**: skipping a chunk because both sides point at
  the same stored chunk relies on the 128-bit payload digest. Inputs are not
  adversarial, but if that is unacceptable, compare the stored chunk digests
  of both sides' decoded text instead, which is no weaker than comparing the
  lines.

### Afterwards, if this works

The same blob-keyed memo fits the verifier's other whole-file passes:
`pbf_frames::index_truth` (single-threaded, one open and seek per blob,
called on the input by repack, degrade and degrade -l and on every repack
and degrade output) and `dedupe_plan::entries`.

## 2. Concurrent execution (high-conviction rewrite)

### The bottleneck

`process::Lifecycle` runs exactly one child at a time, by construction:
`settle` kills and reaps *every* child of the subreaper until ECHILD, so a
second concurrent launch would be killed by the first one's settlement. On
top of that, `Harness` is a single `&mut` threaded through closures, and
`all::run` calls checks one after another. The consequence is that every
pbfhogg run, every osmium reference run, and every decode, including the two
sides of every comparison, run strictly serially, on a machine where an
`osmium cat` uses a small fraction of the cores and the comparison merge
waits on both sides in turn.

### The redesign

**Per-launch containment with a shim.** Keep the verifier as the top-level
subreaper and backstop, but launch each child under a tiny shim: the
verifier's own executable re-executed with a hidden mode. The shim sets
`PR_SET_CHILD_SUBREAPER` and `PR_SET_PDEATHSIG`, spawns the real program,
waits for the leader, then kills and reaps everything it adopted, and reports
through its exit status or a pipe whether a descendant outlived the leader.
That keeps the current per-launch rule (a surviving descendant is an
environment outcome) while letting any number of launches run at once.
Cancellation kills the shims; anything a killed shim strands reparents to the
verifier, whose existing settle sweeps it. Unprivileged PID or user
namespaces would be neater but are restricted on current Ubuntu kernels, and
cgroups need delegation, so neither is a safe default.

**A services layer and per-check contexts.** Split `Harness` into:

- shared services, thread-safe: the launcher, the reference cache (with
  single-flight per key), the decoding service (1), the shared-artifact store
  (3), the input watch list;
- a per-check context: failures, steps, detail buffer, check directory.

**A resource-aware scheduler.** Checks run on a small worker pool. Launches
take tokens by class: pbfhogg runs (themselves multi-threaded, so one or two
at a time), osmium reference runs (with a memory estimate scaled from the
input size; osmium sort with the simple strategy was OOM-killed on germany),
and decodes (several at a time, cheap in memory once streamed). Inside a
check, the obvious overlaps become two-line changes once launches can return
handles: the pbfhogg run and its osmium reference are independent; the two
decodes of a comparison are independent.

**Ordered reporting.** Result lines are buffered and emitted in suite order,
each as soon as it and every earlier check have finished, so stdout is
exactly today's sequence. An ERROR or an interrupt in check k cancels
in-flight work and suppresses the lines after k, as today. field-policies
stays first in the output and gates every later *verdict* (its error still
stops the suite), but it need not gate any later *work*.

### Why it is plausibly high-payoff

Even without 1, splitting each comparison's two decodes and overlapping
pbfhogg with osmium roughly halves the decode-bound checks. With 1 in place,
the remaining work is a set of independent blob batches and pbfhogg runs,
which parallelize almost perfectly up to the token limits.

### Risks

- The shim is now the piece that guarantees nothing outlives a launch. Its
  cancellation, adoption and backstop behaviour must be re-proved with the
  same rigour as `process.rs` today (and its lifecycle integration test).
- Contention changes what each launch measures. The per-check timing in the
  result line becomes noisy and partly meaningless for shared work; report
  shared work as such rather than charging it to the first requester.
- Memory on germany: concurrency must be bounded by the estimate, never by
  core count.
- The reference cache's staging name is `.staging-RUNID-ID`, and the same id
  (`opl-compare.1`, `sort.1`, `fileinfo-e.1`) is issued from several checks.
  Concurrent identical runs would collide; single-flight on the key prevents
  that, and staging names need a per-launch suffix regardless.

## 3. A run-level store for shared constructions (structural, medium-large)

Several pbfhogg and osmium constructions are rebuilt by each check that
needs them, though argv and inputs are identical:

- `pbfhogg apply-changes INPUT OSC` building "new": run by the apply-changes
  check (when `--direct-io` is off its argv is the same), by diff, and by
  diff --format osc. Three runs of the same command on denmark.
- `degrade --strip-indexdata` of the input: repack, check-ids, diff and diff
  --format osc each make their own. Of "new": diff and diff --format osc.
- `osmium fileinfo -e` of the input (cached, but each call pays the key
  costs in 4), `pbfhogg check --index` and `index_truth` of the input in
  repack and degrade.

The redesign: a run-level store keyed by (executable digest, argv with
inputs replaced by their digests), single-flight, outputs under
`RUN/shared/KEY/` and watched like any input. Checks ask it for fixtures
instead of launching them. The key rule makes it automatically correct where
argv differs: the deliberately buffered fixture builds under `--direct-io`
get their own entries, the judged `--direct-io` run its own.

Risk: shared fixtures make one check's fixture failure surface in several
checks. That is already true of the inputs; the store should record which
check first built an entry so a failure names it.

These are pbfhogg outputs, so they must not persist across runs (that would
be caching pbfhogg behaviour); in-run only.

## 4. The reference cache's hot path (local, medium)

Each cached `Harness::reference` call pays costs that do not scale with the
work it saves:

- **Tool identity per call, twice on a miss.** `cache_key` runs `osmium
  --version` and `ldd osmium`, then digests the osmium binary and every
  shared library it links; `identity_text_again` repeats it all after a miss.
  Compute it once per run, and recheck only the stamps of the tool and its
  libraries before publishing.
- **Input digests per call.** `key_text` digests every input of every run in
  full: the denmark input is hashed once per cached reference that reads it,
  dozens of times a run. The run already stamps every input
  (`Run::watch`); memoize the digest by stamp.
- **Artifact re-hash on every hit.** `lookup` re-digests every artifact of a
  hit, although the manifest records a `Stamp` (dev, ino, size, mtime) that
  `lookup` never consults. Trusting a matching stamp for files the cache made
  read-only would remove a full read of every hit artifact; a mismatching
  stamp falls back to the digest. That is a deliberate weakening of
  tamper-evidence inside the cache directory, and should be decided as one.
- **fsync of every published artifact.** `publish` calls `sync_all` on each
  artifact and directory, forcing writeback of multi-GB osmium outputs on
  every miss. A torn file after a crash is caught by the digest on the next
  lookup, so the fsync buys only the avoidance of one re-run after a power
  loss. If stamps are trusted (above), keep a cheap variant: verify by digest
  only entries published before the current boot.
- **A full copy of every cache-linked artifact that is later read as an
  input.** `Run::watch` sees a run-local path with more than one link, which
  every placed reference artifact is (hard-linked from the cache entry), and
  calls `private_input_copy`, copying the whole file to break the link,
  because a later link or eviction would move the shared inode's ctime. Every
  osmium output that is then decoded (which is nearly all of them, through
  `opl_compare::decode`, whose input is watched) is therefore copied in full
  first: osmium sort, cat, extract, apply-changes, add-locations-to-ways,
  renumber, derive roundtrips. This is probably several GB of writes per run
  that nobody asked for. Better: stamp such files without ctime (the cache
  itself identifies artifacts by dev, ino, size and mtime), or use a reflink
  where the filesystem offers one, or have placement copy instead of link
  for artifacts known to be re-read as inputs.

None of these changes what is compared; they change what the cache trusts
and how often it re-proves it.

## 5. Dead and redundant work inside checks (local, small to medium)

Things the decoding service would remove anyway, but which are removable
today:

- **`check_sorted` after a strict comparison of the same file is dead.** The
  strict merge (`opl_compare::compare`) returns `Err(Failed)` when either
  side is not in strict order, ending the check, so a `check_sorted` that
  runs after it on one of its sides can only pass. This applies to
  `sort::run` (`check_sorted(ours)` after `same_elements`) and to
  diff --format osc (`check_sorted(new)` and `check_sorted(rt_pbfhogg)` after
  the four strict roundtrip comparisons). That is three full decodes whose
  verdict is fixed in advance; the counts they print could come from the
  merge's `Merged::first`/`second`.
- **`check_sorted` before a strict comparison is diagnostics only.** In
  `compare_outputs` and `apply_changes`, the order checks give a recorded
  FAIL with a clearer message where the merge would raise an error. A merge
  that reports each side's order instead of erroring (and the caller choosing
  record or error) gives the same verdicts and messages from one decode per
  side.
- **Byte-identical sides.** degrade's `drop-a` against `drop-a-again`
  decodes two files that, when the run is deterministic, are byte-identical;
  equal bytes imply equal decodings, so a byte comparison settles the
  repeatability claim without osmium and the decode is needed only when the
  bytes differ. A content-keyed memo makes this automatic everywhere.
- **field-policies depends on no pbfhogg.** Its outcome is a function of the
  verifier executable, osmium's identity and the input's digest, and it runs
  roughly 150 osmium processes on its slice. Its verdict could be cached
  under that key, as the reference cache caches tool runs; this is not a
  pbfhogg verdict. Within a run, its controls also re-decode the same base
  file under the same policy four times per control (`judge_policies`), which
  a memo collapses.
- **`index_truth` is single-threaded and per-blob open/seek/read.** On the
  input and on every repack and degrade output, it decompresses every blob
  in one thread. Blob-parallel decompression is straightforward, and a
  per-blob memo shares the input's blobs with every passthrough output.

## What the check side would have to change

- Stop treating decodings as files. Callers of `opl_compare::decode` that
  read the OPL themselves (`apply_changes::read_model` and `judge_elements`,
  `field_policies::literal_decode`) take a decoding handle and iterate it;
  `write_expected` streams its expected stream into the merge instead of
  writing `expected.opl`.
- Ask for order and counts from the decoding, not from `check_sorted`, and
  drop the dead calls listed in 5.
- Request fixtures (the "new" PBF, stripped copies, the input's fileinfo and
  index truth) from the shared store rather than launching them privately.
- Issue independent launches as handles and join later (pbfhogg run beside
  its osmium reference; both sides of a comparison), so the scheduler has
  something to overlap.
- Accept that a check's directory no longer holds every file it judged:
  shared fixtures and decodings live in run-level stores, and the kept run
  directory on failure must still contain them as evidence.
- Accept a changed meaning of the per-step timings in the result line.

## Suggested sequencing

1. The local fixes in 4 and 5: no architecture change, and they shrink the
   denominator for measuring the rest.
2. The in-run, file-granular decoding memo with derived facts (the first
   half of 1), plus the shared-construction store (3).
3. Streamed decodes and the shim-based concurrent launcher (2), which the
   streamed decodes need in order to run both sides at once.
4. Blob granularity and cross-run persistence of decodings, the step with
   the largest payoff and the most new machinery.

## Other things noticed

- `opl_compare::decode` deletes a pre-existing `.stdout` sink and OPL before
  each decode to get "a fresh decoding each time", and `Harness::fileinfo`
  does the same for its report. Both exist only because one check reads the
  same file twice, which a memo makes moot.
- The cache's retention (`ReferenceCache::retain`) keeps the most recently
  used entries that fit the byte budget, regardless of what each costs to
  recompute. On germany the budget will be exceeded every run, and a cheap,
  large `osmium cat -t` output can displace an expensive `osmium sort`.
  Retention by recompute cost per byte (the manifest could record the run's
  wall time) would keep the expensive entries.
- `Run::revalidate` stats every watched input after every reference run and
  every check; the watch list grows with every decoded output, so the cost
  is quadratic in launches. Negligible at today's scale, but it will not stay
  so once decodes are fine-grained.
