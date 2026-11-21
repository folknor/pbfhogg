# Hunt: the verifier's decode and compare engine

Scope: `crates/verify/src/` `opl_compare.rs`, `verify_opl.rs`, `verify_diff_opl.rs`,
`verify_diff_detail.rs`, `verify_element_diff.rs`, `verify_change_listing.rs`,
`pbf_frames.rs`, `osc.rs`, `diff_summary.rs`, `verify_reencode.rs`,
`verify_input_order.rs`, `verify_extsort.rs`, `counts.rs`, plus the callers and
the harness, process and cache code they run through. Read at e8377a78. Nothing
was built, run or measured: every number below is an estimate from reading, and
each one says so.

## Verdict

The engine has no notion of "a file I have already decoded". Every question
about a PBF (is it ordered, how many objects, does it equal that other file) is
answered by a fresh `osmium cat` to a multi-gigabyte OPL file on disk, which the
verifier then reads back line by line on one thread, after which the file is
deleted. Two decodings per comparison run one after the other, never together.
The same file is decoded two to seven times inside one check, and the dataset
input is decoded again in check after check. Nothing about a decoding survives
the call that made it, let alone the run.

The right move is a rewrite of the engine around one idea: **one streamed
osmium decoding per distinct file content, reduced on the fly to a tiny summary
plus an on-demand record stream, with comparisons decided from summaries when
they can be and by a concurrent streamed merge when they cannot.** OPL stays as
the wire between osmium and the verifier; it stops being an artifact. On top of
that, the summary is a few hundred bytes and is exactly a completed reference
execution in the cache's own sense, so it can be persisted across runs: on the
development loop, any file whose bytes did not change (the dataset, every
cached osmium reference, every pbfhogg output the change did not touch) is never
decoded again.

Reading PBF through the pbfhogg library is the wrong answer (it makes pbfhogg
judge itself, and the crate forbids it). A verifier-owned full element decoder
is a plausible later tier but not the first move, for reasons in "Considered and
not recommended". The verifier's own frame reader (`pbf_frames`) should stay and
get parallel.

## How the engine spends its time today

### One comparison

`opl_compare::compare` / `compare_sequence` / `compare_paired` all go through
`walk`:

1. `decode(first)`: `osmium cat FIRST -f opl[,...] -o FIRST.opl` through
   `Harness::reference` (uncached), written into the cache's staging directory
   and hard-linked into the check directory. For denmark with metadata this is
   several GB of text (estimate: on the order of 80 to 100 bytes per object).
2. `decode(second)`: the same, only after (1) has finished. The verifier is a
   single process that runs exactly one child at a time
   (`process::Lifecycle::run` settles each launch before returning).
3. `merge`: two `BufReader<File>` with the default 8 KiB buffer,
   `read_until(b'\n')`, `parse_key`, a byte compare of whole lines, all on the
   one thread. The OPL just written is read straight back.
4. Both OPL files are deleted.

So a comparison costs `decode(a) + decode(b) + read(a) + read(b)` in series,
plus writing and then discarding roughly twice the decoded text volume. The
merge time is not even visible: the step list in the PASS line attributes the
`osmium cat` runs, but the verifier's own read and compare is the unlabelled
residue of the check total.

### The same file, decoded again and again

`opl_compare::decode` says so outright: "A file decoded twice in one check gets
a fresh decoding each time." What the callers do with that, as read:

| Check / path | Full-size decodes | Distinct files | Why the extra decodes |
|---|---|---|---|
| `verify_input_order::compare_outputs` (cat x3 types, tags-filter, getid-removeid, getparents, add-locations) | 4 per call | 2 | `check_sorted(theirs)`, `check_sorted(ours)` (each a CONTENT decode), then `same_elements(ours, theirs)` decodes both again |
| `sort::run` | 3 | 2 | `same_elements(ours, theirs)` already refuses an out-of-order `ours` (strict merge); the trailing `check_sorted(ours)` decodes it a third time to restate that as a verdict |
| `cat_dedupe::run` | about 16 | about 10 | `order(input)`, then six `same_elements(out, input)`: the input is decoded seven times |
| `repack::run` | 8 | 5 | the reference (the input itself when standard) is decoded once per case |
| `degrade::run_on` | about 16 | about 8 | the reference is decoded in `observed_coordinates`, `strip_locations`, every drop and unsort case; `drop-a` three times |
| `derive_changes::run` | 10 (12 on the omission path) | 4 | four `same_elements` over four files, two `check_sorted`, `osmium_derive_roundtrip` decodes old and new again |
| `apply_changes::run` | 3 + an `osmium fileinfo -e` | 2 | `check_sorted(pbfhogg_out)`, `decode(osmium_out)` into a file, a full copy into `expected.opl`, then `compare_with_opl_each` decodes `pbfhogg_out` again |
| `diff::run` | 2 | 2 | none, but serial |
| `renumber::run` | 2 | 2 | none, but serial |
| `counts::osmium_counts` | 1 | 1 | a whole decode to count objects |

Across a `verify all` that is on the order of 70 to 80 full-size decodes (my
count from the table plus the extract, multi-extract and field-policies paths),
of which perhaps 30 to 40 are of distinct file contents. The dataset input
alone is decoded in field-policies, cat-dedupe (seven times), repack (four),
degrade, diff, and others.

### Materialized intermediates beyond the decodings

- `apply_changes::write_expected` writes a complete second copy of osmium's
  decoding (`expected.opl`) only so `compare_with_opl_each` can read it back.
- `opl_compare::osmium_derive_roundtrip` writes a complete model file
  (`osmium-derive-model.opl`) the same way.
- `apply_changes::read_model` decodes the OSC to `input-osc.opl` and reads it
  back (small for a daily diff).
- `extract`'s simple arm runs `osmium removeid` and then two more decodes
  (`present_members`) to learn which member IDs a file already decoded in the
  same check contains.

### Hidden costs around every reference run (orchestration's, flagged here)

- `Run::watch` copies a whole file (`private_input_copy`) whenever an input is
  in the run directory with `nlink > 1`. Every cached osmium artifact is placed
  by hard link, so the first time any cached reference PBF is fed to a decode it
  is copied in full first.
- `reference_cache::cache_key` and `identity_text_again` each spawn
  `osmium --version` and `ldd`, and digest the osmium binary and every shared
  library, before and after every cached reference run; the key also digests
  every input in full, and `lookup` re-digests every artifact. Uncached decodes
  skip this, but every `osmium sort`, `extract`, `fileinfo` pays it twice.

## Opportunity 1 (full rewrite, high conviction): decode once, stream, summarize

### The bottleneck

Three structural faults compound: decodings are materialized to disk and read
back; the two sides of every comparison are serialized; and no decoding is
reused, so the same osmium work is paid again for every question asked about
the same bytes.

### Why the current structure causes it

`opl_compare` is built as path-in, path-out functions (`decode` to a file,
`merge` of two files, `order` of one file), each owning the whole life of its
OPL. A caller that wants order and equality has no way to ask for both from one
decoding, so it calls `check_sorted` and `same_elements` and pays twice. The
process model (`Lifecycle::run`, one child, stdout always to a file sink)
forbids a pipe and forbids a second concurrent decoder, so materialization and
serialization are forced.

### The redesign

A run-scoped `Engine` owned by the harness, replacing `opl_compare`'s public
surface, `counts.rs`, `Harness::check_sorted*` and `Harness::same_elements`:

1. **Identity.** A file is named by its content digest (XXH3-128, the same
   digest the cache already trusts for artifact identity), memoized per run by
   its watched stamp. Two files with the same bytes are one decoding.
2. **One decoding per identity.** `osmium cat FILE -f opl,locations_on_ways=true -o -`
   (today's FULL format, exactly) with stdout a pipe, never a file. Always FULL,
   because NO_LOCATIONS and CONTENT are exact projections of the FULL line:
   NO_LOCATIONS drops the `x...y...` suffix of each `N` token, CONTENT also
   drops the `v`, `d`, `c`, `t`, `i`, `u` fields. OPL escapes every separator, so
   both projections are plain field-level byte edits.
3. **In-process reduction while it streams.** A reader per side splits lines,
   parses the key, and folds a `Summary`:
   - object counts per kind;
   - strict-order verdict and the first violation (record number, previous and
     offending key), never an early error, so order and equality come from one
     pass;
   - a stream digest per policy (FULL, NO_LOCATIONS, CONTENT): XXH3-128 over the
     ordered sequence of (kind, id, projected line);
   - the header facts the policies depend on (`LocationsOnWays`).
   The per-record work (newline split, key parse, three projections, hashing) is
   chunk-parallel: split the pipe data at newline boundaries into a few MB
   chunks, hash records on worker threads, combine the per-record hashes in order
   on one thread. This matters: the verifier's per-line work today is in the
   same range as osmium's formatting (estimate), and it is single-threaded.
4. **Comparisons decided from summaries when possible.** For `compare` (strict)
   and `compare_sequence`: if both summaries are strictly ordered (strict mode)
   and the policy digests are equal, the result is `identical` with
   `same = count` and `first/second` from the counts, no merge, no listing lines.
   This is the PASS path of most checks (cat, sort, cat-dedupe, repack,
   derive-changes, add-locations, the extract strategies that must agree).
5. **A concurrent streamed merge when not.** When digests differ, or a caller
   needs records (`compare_paired` for renumber, the diff listing, models), both
   sides are decoded concurrently into the merge. The merge stays byte for byte
   on the projected lines, writes the same `-`/`+` listing of projected lines (so
   `DiffGroups`, `verify_reencode::Listing`, `verify_opl::scan_objects` and every
   other listing reader keep working unchanged), and hands the same `Group` and
   `Pair` callbacks. Strict-order failures become the same failure, named by file
   and record number instead of by a deleted OPL path.
6. **Record adapters instead of files.** The places that write an OPL only to
   read it back become stream adapters over decodings: apply-changes' expected
   stream (osmium's decoding with the model's touched records substituted,
   merged live against pbfhogg's decoding: three concurrent streams, no
   `expected.opl`), the osmium derive omission model (old and new decodings in,
   expected records out, merged against `rt_osmium`), kind filters.

Process side: `Lifecycle` needs a launch group, several leaders started together
with piped stdout, every stdout, stderr, pidfd and the signalfd multiplexed, and
the group settled together (today's `settle` kills every child of the process,
so a second concurrent leader would be killed as an adopted descendant).
Children must be spawned only from the main thread: `PR_SET_PDEATHSIG`, which
`prepare_child` arms, fires on the death of the forking thread, not the process.
Worker threads only hash, never fork. Two concurrent libosmium pools
oversubscribe the cores; `OSMIUM_POOL_THREADS` in the controlled environment can
split them.

### Why it is plausibly high payoff

- Decodes per `verify all` drop from roughly 70 to 80 to the number of distinct
  file contents, roughly 30 to 40 (estimate), before any cross-run reuse.
- Every remaining comparison overlaps its two sides: wall is about the slower
  decoder rather than the sum of both plus two read-backs.
- No OPL is written or read back on the hot path: today that is several GB
  written and several GB read per decoding, on the same scratch drive the
  pbfhogg commands use, with the write-back competing with whatever runs next.
- `check_sorted`, `osmium_counts`, `order` and the informational
  `print_fileinfo` become free fields of a summary the comparison computed
  anyway.
- Most PASS comparisons need no merge at all.

A rough envelope (unmeasured): if decode and compare are half or more of a
12 to 20 minute `verify all`, this alone plausibly takes that half down by a
factor of three to four.

### Risks

- **Hash equality on the summary path.** A PASS decided by equal 128-bit
  digests is not literally a byte compare. A miss needs an XXH3-128 collision
  between a pbfhogg output and its reference; the cache already rests artifact
  identity on the same hash, so this adds no new trust assumption. If that is
  not acceptable, BLAKE3 over the same stream costs a few seconds per denmark
  file and is cryptographic. The streamed merge stays byte exact.
- **Projection fidelity.** Comparisons between two projected decodings only need
  the projection to be a correct field drop. The consumers that mix projected
  lines with lines osmium wrote under another format (apply-changes compares
  against `osmium getid -f opl` and the OSC's OPL; both NO_LOCATIONS) need the
  projection to be byte identical to osmium's own `opl` and
  `opl,add_metadata=false` output. That is how libosmium's OPL writer is
  structured (metadata fields and the per-ref location are separate suffixes),
  but it must be proved once on denmark by diffing projections against osmium's
  own decodings, and could stay as a self-check in field-policies.
- **FULL costs more text than CONTENT** for a file only ever compared under
  CONTENT (derive-changes' roundtrips). The dedupe wins more than this loses in
  every check I read; measure it.
- **Lifecycle concurrency** is the delicate part: multiplexed waits, group
  settlement, signal handling with several live children, a stderr-heavy child
  that must not deadlock a blocked stdout read (poll every fd, or drain stderr
  on its own thread).
- **Diagnostics wording** changes where an error named an OPL path that no
  longer exists. The information is the same or better (file plus record
  number plus keys).

## Opportunity 2 (rewrite, high conviction, crosses into orchestration): summaries as cached reference results

### The bottleneck

Across runs, every decoding is repeated even when osmium and the bytes are
identical: the dataset input, every cached osmium reference output (which the
cache already hands back without rerunning osmium, only to have the engine
decode it again), and every pbfhogg output that did not change.

### Why

`decode` marks its run `uncached` because its output is as large as its input.
That reason disappears once the product of a decoding is a summary.

### The redesign

Store each `Summary` in the reference cache as a reference result keyed by the
osmium identity the cache already computes, the engine's summary format version,
and the decoded file's identity. Use the digest of the data-blob payload
sequence plus the header facts osmium's decoding depends on (required features,
`LocationsOnWays`) rather than the whole-file digest, so a header that only
differs in `writingprogram` or a replication timestamp still hits.

This stays inside the cache's stated rule ("completed reference executions only;
no pbfhogg verdict is cached"): the summary is osmium's reading of exact bytes;
the comparison of two summaries still runs every time.

### Payoff

On the development loop, after a change to one command, most files a
`verify all` touches have the same bytes as last time. Their decodings vanish,
and their PASS comparisons are summary compares. The engine's share of the run
approaches the decodes of the outputs that actually changed.

### Risks

- An osmium decode that would now fail on bytes it decoded last time cannot
  happen if osmium's identity is in the key (the cache already includes the
  binary and library digests).
- Mismatches still need records, so the failure path still decodes; that is the
  path where the time is wanted.
- The payload-sequence key must include every header fact osmium consults; if in
  doubt, include the whole `HeaderBlock` minus `writingprogram` and the
  replication fields.

## Opportunity 3 (medium, local): a parallel, memoized own frame reader

`pbf_frames::index_truth`, `blob_ids`, `blob_ids_and_shape` and
`dedupe_plan::entries` inflate every data blob on one thread through `flate2`'s
default backend (miniz_oxide), opening the file again for every blob.
`index_truth` runs on the input in `read_input` for repack, degrade and
degrade -l, and on every repack and degrade output in `judge_index`: around a
dozen whole-file inflates per `verify all` (estimate: several seconds each for
denmark).

- Read frames once, inflate and scan blobs on a worker pool, fold in order (the
  same IO, parallel decode, reorder shape as pbfhogg's `read/pipeline.rs`, but
  written here, not linked).
- Memoize `IndexTruth` and the per-blob ID lists by file identity; the input's
  answer is identical in repack and degrade.
- Use a faster inflater that is not pbfhogg's (system zlib, which is what osmium
  uses, or zlib-ng through flate2) so the independent decode stays independent
  of pbfhogg's `zlib-rs`.
- Header-only walks (`Claims`, `framing`, `header`) go through a `BufReader`
  with a `seek` per frame, which discards the buffer and leaves the kernel's
  sequential readahead pulling payloads the walk skips. `pread` of the frame
  header with `POSIX_FADV_RANDOM` (what pbfhogg's `read/header_walker.rs` does)
  keeps it header-only.
- Within the engine's visit to a file, the frame reader can run beside the
  osmium decoding of the same bytes, sharing the page cache.

## Opportunity 4 (medium, local, independent of the rewrite): delete the redundant decodes now

If the rewrite is not started at once, these are each a few lines and remove
whole decodings:

- Make the strict merge return each side's order verdict and counts instead of
  erroring, and record those. Then `sort::run` drops its trailing
  `check_sorted(ours)`, `compare_outputs` drops both `check_sorted` calls,
  `apply_changes` drops `check_sorted(pbfhogg_out)`, `derive_changes` drops
  `check_sorted(rt_pbfhogg)` (it is already a side of a strict merge), and
  `extract` drops its two `check_sorted_counted` calls. The claims are recorded
  under the same names.
- Run the two `decode` calls of `walk` concurrently into files (a launch group
  with file sinks) even before pipes: half the wall of every comparison.
- Raise every OPL `BufReader` to 1 MiB.

All of these disappear into Opportunity 1 if it lands.

## Opportunity 5 (medium): identity shortcuts

- **Byte identity.** Two files with the same content digest decode identically.
  `degrade`'s `drop-a` against `drop-a-again`, `add-locations`' engine pairs,
  `diff`'s and `derive-changes`' `new.osm.pbf` (same pbfhogg apply-changes of
  the same inputs) need no decode at all once the memo is keyed by content.
- **Payload identity.** When a file's data-payload sequence equals another
  file's (the verifier already proves this with `same_data_payloads` for
  `degrade --strip-indexdata` and per blob with `same_payload` for cat-dedupe's
  passthroughs) and the header facts osmium consults agree, osmium's decoding of
  the data is identical by determinism; only the new header needs osmium's
  acceptance, which `osmium fileinfo` without `-e` gives cheaply. That replaces a
  full decode with a header read for strip-indexdata outputs. Check owners
  decide; the engine can offer it as "decoded as" another identity.

## Opportunity 6 (low): OSC and small readers

`osc::OscParser::feed` runs a per-byte state machine with `mem::take` per byte,
allocates a fresh `Vec` for every tag, and `Utf8Validator::push` copies every
16 KiB chunk into a new vector to validate it. A `memchr` for `<` in text state
and in-place validation carrying only the trailing partial code point would make
it several times faster. Daily denmark diffs are small, so this only matters for
large merge-changes inputs.

## Considered and not recommended

### Reading PBF through the pbfhogg library

The crate comment is explicit ("Links no pbfhogg: no verdict may rest on code
compiled into the verifier"), and the reason holds: a decoder that ignores a
field ignores it on pbfhogg's output and on osmium's reference alike, so a
pbfhogg writer bug in that field passes. pbfhogg's readers are useful here only
as architecture to copy (`read/pipeline.rs`, `read/header_walker.rs`), which
Opportunity 3 does.

### A verifier-owned full element decoder as the comparison source

Fast (no text formatting, parallel per blob, direct hashing), but the claims are
phrased over osmium's reading, and two things are lost:

- **osmium's acceptance of every pbfhogg output.** Today every compared pbfhogg
  output is fully decoded by osmium, so an output osmium cannot read fails.
  Dropping that is a weakening. Keeping it means an osmium pass per pbfhogg
  output anyway, which is most of what Opportunity 1 already pays.
- **osmium's normalization.** OPL renders absent metadata, version 0 and
  strings cut at NUL the way libosmium does. An own decoder either reproduces
  that exactly (brittle) or compares stricter, which changes verdicts.

The only sound form is a two-tier one: an own decoder that refuses anything it
does not fully read (so its equality implies osmium's), used as a PASS fast path
for osmium-written references and the input, with osmium still decoding each
pbfhogg output for acceptance, and the osmium path deciding every mismatch. After
Opportunities 1 and 2 the remaining cost this could remove is small. Afterwards,
if this works and profiles still show osmium decoding of references dominating
cold runs, revisit it.

### A cheaper osmium output than OPL

OPL is the only full-fidelity, escaped, line-delimited osmium rendering. It is
the right wire; the waste was never the format, only materializing it and
repeating it.

## The representation the checks and orchestration should want

```text
Engine (run-scoped, owned by Harness)
  identity(path)                    -> FileId     content digest, stamp-memoized
  summary(path)                     -> &Summary   decode once, or cached
  records(path, policy)             -> Records    a pull stream of (kind, id, projected line)
  compare(a, b, policy, mode, sink) -> Compared   summary fast path, else concurrent streamed merge

Summary   { counts: [u64; 3], order: Strict | Violation { record, prev, at },
            digest: { full, no_locations, content }, locations_declared }
Compared  { merged: Merged, order: [OrderVerdict; 2], decided_by: Summaries | Merge,
            listing: Option<PathBuf> }
Records   adapters: .project(policy), .kinds(mask), .substitute(model), merged with others
```

`mode` is today's strict / sequence / paired; `sink` is today's `each` and
`paired` callbacks plus the listing path. A caller that passes a `paired`
callback always gets the merge.

## What the checks and orchestration would need to change

Checks:

- Replace each `check_sorted` + `same_elements` pair (and `compare_outputs`'
  four calls) with one `compare`, recording its two order verdicts and the
  element verdict under today's names; claims and PASS lines are unchanged.
- Replace `opl_compare::decode` plus hand-written readers of the resulting file
  (`apply_changes::write_expected` and `read_model`, `osmium_derive_roundtrip`,
  `counts::osmium_counts`) with `records` and `summary`.
- `extract`'s simple arm can answer member presence from the key set of a file
  it already decoded instead of `osmium removeid` plus two decodes (check scope
  decides).
- Consumers of listing text (`DiffGroups`, `verify_reencode::Listing`,
  `scan_objects`, `judge_drop`) need no change: the listing is the same text.

Orchestration:

- `Lifecycle`: launch groups with piped stdout, multiplexed waits, group
  settlement, children forked only from the main thread.
- Reference cache: summaries as a small artifact kind, keyed by osmium
  identity, engine format version and payload-sequence identity.
- Compute each file's digest once per run and share it between the cache key,
  the engine identity and `Run::watch`.
- Optional: start decoding a reference as soon as it is placed, while the
  pbfhogg command it will be compared with is still running.
- Report decode and merge as their own steps in the PASS line timing, so the
  engine's cost is visible where today it is the unlabelled remainder.

## Lateral findings

- **Inconsistent strictness in `apply_changes::opl_lines`.** It reads OPL with
  `BufRead::split(b'\n')`, which yields a final line that has no newline. Every
  other OPL reader in the engine (`opl_compare::Stream`, `verify_opl`,
  `verify_diff_opl::Lines`) refuses that as truncation. A truncated
  `base-touched.opl` would be read as complete. Low likelihood (osmium exited
  0), but it breaks the engine's stated rule.
- **Merge time is invisible.** `Harness::step` times launches only, so the
  verifier's own OPL read and compare never appears in the PASS line; anyone
  reading the timings undercounts the engine.
- **Full-file copies in `Run::watch`.** See "Hidden costs" above: every cached
  reference PBF decoded later is copied once because the cache places it by hard
  link. A reflink, or an identity stamp that does not depend on the link
  count's effect on ctime for digest-verified artifacts, would avoid it.
- **Per-reference identity probes.** `cache_key` and `identity_text_again`
  spawn two probe processes and digest osmium and its libraries before and after
  every cached reference run. Once per run, revalidated by stamp, would do.
- **Header walks are not header-only.** `pbf_frames` walks with a `BufReader`
  plus `seek` per frame; readahead pulls the payloads in. pbfhogg's own walker
  fixes this with `pread` and `FADV_RANDOM`.
- **`index_truth` reopens the file for every blob** (`read_block` calls
  `File::open` each time). Harmless at denmark's blob count, wasteful at
  germany's.
- **`pbf_frames` refuses zstd blobs** (`blob_content`), so `index_truth` and the
  layout predictions cannot run on a zstd output; that is documented as a
  verifier error, but it is a coverage gap if a check ever re-encodes with zstd.
- **`decode` deletes stale outputs first**, then writes into the cache staging
  directory and links back. On a decode failure the first side's multi-GB OPL is
  left in the run directory by design; with two big decodes per comparison a
  failing `verify all` can leave a lot of scratch behind.

## How to measure the case before building it

The PASS lines of a recent denmark `verify all` already carry
`osmium cat <total> (<n> runs)` per check. Summing those, and the residue of each
check total over its listed steps (which is mostly the merge), gives the
engine's share of the run without any new instrumentation. If that share is
well under half, Opportunity 2 (cross-run summaries) is the one to build first,
since it removes decodes rather than speeding them up; if it is over half,
Opportunity 1 first, as it is also Opportunity 2's foundation.
