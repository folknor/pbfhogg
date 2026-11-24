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

Extract and tags-filter encode ways without their locations and classify the
output as re-encoded without enrichments (drop and warn), so the hunters flag it
for judgement against each command's promise. The sort and `cat --dedupe` sweep
now keeps them when every input declares `LocationsOnWays`; time-filter drops
them deliberately (they are not as of the cutoff).

## BUG-007 - add_owned_way silently drops mismatched locations

Reported by: encoding.

`add_owned_way` (`owned.rs`) falls back to a plain way when a nonempty
locations vector disagrees with refs; the direct builder location API refuses
the mismatch. The fallback can hide an internal transformation error.

## BUG-019 - apply-changes blob-size percentiles sample passthrough only

Reported by: apply-core.

Samples are inserted only in the two passthrough arms; rewritten and create
frames never contribute, though `stats.rs` labels the result blob sizes.

## BUG-024 - Export keeps decoding after a write failure

Reported by: spatial.

The export callback records `first_err` and later callbacks return early, but
sequential `for_each` still reads and decompresses the rest of the input, so a
broken pipe can waste a planet traversal.

Defect hunt 2026-11-17 (export) observed the same behaviour and notes it is
already recorded as deferred work.

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
