# Bug findings

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

Defects turned up by the 2026-11 performance hunt (see `notes/performance.md`
for the scope names). Where the 2026-11-17 defect hunt reported the same
mechanism, its account is appended to the entry under a "Defect hunt
2026-11-17" paragraph naming its scopes; its other findings are in
`notes/bugs-format.md`, `notes/bugs-commands.md`, `notes/bugs-geocode.md`,
`notes/bugs-tooling.md` and `notes/bugs-docs.md`. They are as reported, read from source, not reproduced
unless an entry says so, and some hunters marked their own finding as a code
reading or an API hazard rather than a demonstrated wrong result; that label is
kept. "Reported by" is a fact about the reports. Entries are in no order of
importance.

## BUG-006 - Rebuild paths drop inline way locations

Reported by: encoding, sort, spatial.

Extract, tags-filter and the sort sweep (borrowed way output and `read_way`
owned demotion) encode ways without their locations, so in sort whether a way
keeps coordinates depends on overlap routing. Some callers classify output as
re-encoded without enrichments, so the hunters flag it for judgement against
each command's promise. `notes/verification.md` VER-135 describes sort's drop
as required and lacking a real-data witness.

## BUG-007 - add_owned_way silently drops mismatched locations

Reported by: encoding.

`add_owned_way` (`owned.rs`) falls back to a plain way when a nonempty
locations vector disagrees with refs; the direct builder location API refuses
the mismatch. The fallback can hide an internal transformation error.

## BUG-011 - Node-statistics histograms depend on worker scheduling

Reported by: check.

`WorkerAccum` (`inspect/node_stats.rs`) carries a partial 128-coordinate window
across whichever blobs a worker receives, so adjacency, buckets and window count
vary with scheduling; the parity test excludes histogram equality. Fixing needs
a contract decision.

Defect hunt 2026-11-17 (check-inspect, P2): witness - two blobs of 64 nodes,
the first at latitude 0 throughout, the second at 900,000,000 throughout. In
file order they form one 128-value window needing 30 bits; on separate workers
they become two constant windows needing zero bits. Histogram, block count and
compression estimate change with scheduling or `-j`. The comment claiming the
histogram shape is preserved is false. Proposed: order coordinate batches
before forming windows.

## BUG-017 - apply-changes direct reads do not reach the bulk reads

Reported by: apply-core.

`direct_reads` reaches the initial `InputFront`, but the scanner opens a
buffered walker (`scanner.rs`) and the worker pool opens a plain file
(`streaming.rs`), so the heavy reads stay buffered.

## BUG-018 - renumber direct reads do not reach the bulk reads

Reported by: sort.

The initial header read gets the flag, but payload workers use `pin.open()` and
schedule construction opens the ordinary header walker (`renumber/mod.rs` input
setup).

## BUG-019 - apply-changes blob-size percentiles sample passthrough only

Reported by: apply-core.

Samples are inserted only in the two passthrough arms; rewritten and create
frames never contribute, though `stats.rs` labels the result blob sizes.

## BUG-021 - renumber turns explicit zlib:6 into zlib:1

Reported by: sort.

Compression selection (`renumber/mod.rs`) maps explicit `zlib:6` to `zlib:1`;
disclosed in CLI source but an observable trap for any pipeline rewrite.

## BUG-023 - cat's blobs_decoded hides decompression

Reported by: copy.

Unindexed plain cat decompresses ordinary blobs to generate indexdata but counts
them as passthrough; `blobs_decoded` counts only rebuilt unclassifiable blobs.

Defect hunt 2026-11-17 (passthrough-rewrite): the repair path has the same
gap. `repair_passthrough` (`cat/mod.rs`) validates every data blob, but sets
`blobs_decoded` only from blobs rebuilt into single-kind blocks, and
`Outcome::Rewritten` increments neither decoded nor passthrough counts. One
repaired, classifiable blob can report `0 blobs passed through` then
`Repaired 1 input blobs`, with `cat_blobs_decoded = 0`, against
`CatStats::blobs_decoded`'s documented meaning.

## BUG-024 - Export keeps decoding after a write failure

Reported by: spatial.

The export callback records `first_err` and later callbacks return early, but
sequential `for_each` still reads and decompresses the rest of the input, so a
broken pipe can waste a planet traversal.

Defect hunt 2026-11-17 (export) observed the same behaviour and notes it is
already recorded as deferred work.

## BUG-026 - Geocode candidate and ranked APIs break equal-area ties differently

Reported by: geocode.

`Candidates::into_result()` keeps the first equal-area admin match; ranked lookup
picks the lower polygon index; an index with equal-area polygons in reverse
order makes them disagree. Code reading, not executed.

Defect hunt 2026-11-17 (geocode-reader, P2): `Candidates::into_result`
(`reader.rs`) explicitly claims to match `query()` semantics. With two
containing same-level polygons of equal stored area presented as `[1, 0]`,
`query()` selects polygon 0 and `candidates().into_result(&reader)` selects
polygon 1. This occurs with valid index data, and equal stored areas also
arise through f32 rounding. `AdminMatch` discards polygon identity, so sharing
the ranking needs that identity kept through collection.

## BUG-027 - Geocode building placement is a vertex mean

Reported by: geocode.

Building address placement averages resolved way coordinates including the
repeated closing vertex, can fall outside a concave building, and averages near
longitude zero across the antimeridian. Flagged to preserve during rewrites
unless placement semantics change.

Defect hunt 2026-11-17 (geocode-builder) observed the antimeridian case
(`builder/pass2.rs` arithmetic longitude mean) and classes it as a surprising
consequence of the documented mean-of-nodes policy, not a contract defect.

## BUG-028 - Geocode query bench can be optimized away and measures warm uniform points

Reported by: geocode.

`crates/bench/src/geocode.rs` measures serial queries over a repeated, warmed,
uniform-in-bbox set, so it says nothing about cold pages, address-heavy load or
CLI throughput, and its optimizer sink consumes only `admin.len()`.

Defect hunt 2026-11-17 (bench): both loops (`crates/bench/src/geocode.rs`)
discard address, street and interpolation results and black-box only the
accumulated `admin.len()`; with fat LTO those computations are unprotected
against elimination. Not established that elimination happens; the hunter
labels it a benchmark-validity concern, not a confirmed timing defect.
Separately, the geocode-query script advertises `--hotpath` and `--alloc`, but
neither the instrument nor `Reader::query` and its search helpers carries
profiling scopes, so those modes measure only the process wrapper.

## BUG-029 - Scan helpers that look complete but are not

Reported by: indexdata.

`blob_filter_check` (`scan/classify.rs`) accepts a `BlobFilter` but ignores its
bbox; `parallel_scan_blobs_raw` does not enforce the schedule's kind claim.
Callers compensate today; the hunter calls these API hazards, not demonstrated
wrong results.

## BUG-031 - check --ids --full duplicate details depend on completion order

Reported by: the codex spar of the check negative-ID work.

The duplicate pass attributes a duplicate to whichever blob reaches the
set second: the atomic setters in the workers, and `settle_overflow` in
the phase's merge, which takes results in completion order
(`check/verify_ids.rs`). Totals are exact, but the order of the stored
violations is not, so `--max-errors` can keep a different finding from run
to run, and even the uncapped list can be ordered differently. Witness:
node blobs `[-1] | [-2] | [-1]`, one per blob. Both schedules report two
violations, `Duplicate -1` and `NonMonotonic -1 after -2`, but in either
order, and `--max-errors 1` keeps whichever came first. Positive IDs above
the allocation and atomic duplicates already behaved this way; since
negatives took the same path they do too. Fix: give duplicates a fixed place
in report order and pick their examples by canonical ID, or settle every
occurrence in one ordered consumer. Neighbour of PERF-048, which plans a
redesign of the same per-blob retention.

## BUG-034 - diff --format osc emits nothing for a visibility-only change

Reported by: the codex spar of the hidden-upsert emission fix.

`diff --format osc` classifies by content alone (`commands/diff/derive.rs`,
`crate::content`), and `visible` is outside content. A created or modified
element that is hidden in the new file is now written with
`visible="false"`, so apply records its deletion; but an object visible in
old and hidden in new with identical content gets no OSC entry at all, and
the applied result keeps it visible. Whether a hidden record deletes thus
depends on an unrelated content change. Fixing it means making visibility an
input to OSC classification (a visible-to-hidden transition is a hidden
modify carrying the new side's version, not a synthesized delete, so a stale
older version cannot resurrect it), without changing what text `diff`
reports. Treating hidden records as absent was weighed and rejected: it
loses the tombstone's version claim.
