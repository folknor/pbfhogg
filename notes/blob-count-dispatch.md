# Blob-count threshold dispatch for header-walk commands

Implementation spec, revision 2. Written against
[`reference/technical-implementation-spec.md`](../reference/technical-implementation-spec.md);
spawned from [`reference/blob-density.md`](../reference/blob-density.md)
("Measured evidence" + "getid include mode confirms, steeper") and the
open decision in [`notes/getparents.md`](getparents.md) ("Crossover
measured"). Tests added by this spec follow
[`reference/testing.md`](../reference/testing.md). Ratified 2026-07-10
(threshold-dispatch over revert/accept).

Revision 2 (2026-07-11) follows a codex xhigh critique of revision 1
(14 findings; transcript
`~/.codex/sessions/2026/07/11/rollout-2026-07-11T00-41-49-*.jsonl`).
Revision 1's central artifact - a streaming read-and-discard mode on
`HeaderWalker` - was refuted: the measured scan arms decode bodies
inline in one pass, while read-and-discard reads bodies twice on any
file larger than cache, so the measured bounds did not price that
design. Revision 2's high-blob-count arm is the pipelined reader -
the exact code shape the `--commit` baseline cells executed.

## 1. Problem, priced

`HeaderWalker`'s QD=1 probe-pread walk costs ~45-75 us per blob,
linear in blob count, single-threaded, unmitigated by cache. Commands
whose walk substitutes for reading bytes win hugely on low-blob-count
encodings and lose 57-209 % on high-blob-count encodings. Six measured
cells, two commands (all plantasjen; scan cells via `brokkr --commit`
at the named refs, walker cells at HEAD-of-day):

| command | encoding | blobs | scan arm | walker arm | walker verdict |
|---|---|---:|---:|---:|---|
| getparents | planet primary | 50,816 | 44.8 s | **23.5 s** (`11bc44dc`) | -46 % |
| getparents | europe | 522,168 | **26.4 s** | 44.2 s | +68 % |
| getparents | planet 8k | 1,453,433 | **52.8 s** (`2b3e496e` @ `68e1ba0`) | 82.7 s (`425d1f1e`) | +57 % |
| getid include | planet primary | 50,816 | 43.7 s (`24362e36`-era) | **6.1 s** | -86 % |
| getid include | europe | 522,168 | **17.9 s** (`bc96d15d` @ `51c662e`) | 40.2 s (`57ffbf49`) | +125 % |
| getid include | planet 8k | 1,453,433 | **33.2 s** (`c0d89d8f` @ `51c662e`) | 102.6 s (`aa5bc158`) | +209 % |

The scan arms in this table are the pipelined-reader paths deleted at
`783970a` (getparents) and `bb16193` (getid). Revision 2 dispatches to
resurrected versions of exactly those paths, so these numbers price
the actual proposal.

`sort` pass 1 is EXCLUDED from this spec (revision 1 included it).
Its old arm was seek-based body skipping through `FileReader::skip`,
not a pipelined scan - a third mechanism with its own economics
(readahead prefetch), a shallower asymmetry (+21 %/-9 %), and no
measured cell at either extreme of this spec's bracket. It becomes a
named follow-on (section 8) rather than an unpriced passenger.

## 2. Survey

### Current structure

- `src/read/header_walker.rs` - `HeaderWalker::open`, `next_header`,
  `pread_data`, `shared_file`, `file_size`. `FADV_RANDOM`. Untouched
  by this spec except for the estimator addition.
- `src/commands/getparents/mod.rs::getparents` - header walk builds a
  kind-filtered `schedule`, then `parallel_classify_phase` +
  `ReorderBuffer` drain. `process_block(block, bb, output, ids,
  add_self)` holds the per-block matching logic and is ARM-NEUTRAL:
  it takes a `&PrimitiveBlock` regardless of how the block was
  produced.
- `src/commands/getid/mod.rs::filter_by_id(include=true)` - single
  HeaderWalker pass; include mode preads matching bodies only.
  `filter_by_id(include=false)` (removeid) preads every body for raw
  passthrough. The block-level match/emit logic lives inline in the
  walker loop today and must be factored to be arm-neutral (brick 3).
- The pipelined reader (`ElementReader::for_each_block_pipelined`,
  `with_blob_filter(BlobFilter)`) is live, mature, and
  memory-bounded since the decode-admission gate (`a0a2e3b`). getid's
  `--add-referenced` pass 2 uses this exact shape today
  (`getid/mod.rs:547`).

### The old arms being resurrected

- **getparents @ `68e1ba0`**: `ElementReader` + blob-kind filter +
  per-element classify into a `BlockBuilder`, single pass, decode
  parallel in the reader's pool, I/O overlapped. Read 26.1 GB at
  europe (kind filter skips node blobs pre-decompression via
  indexdata).
- **getid include @ `51c662e`**: pipelined raw-frame pass with
  `BlobFilter` type + ID-range prescreen (skip decompression of
  non-intersecting blobs), decode + re-encode of matching blobs only.

Both resurrections are REWRITES against HEAD APIs (the reader surface
changed since April: admission gate, `parse_waymembers` groundwork),
not cherry-picks; git refs above are the reference implementations.

### Failure history and standing decisions

- `c912e4d` (pipelined-to-sequential conversion, 4.7x Denmark
  regression): not re-proposed - the high-count arm IS the pipelined
  reader; decode stays parallel in both arms.
- Revision 1's read-and-discard walker: logged here as a refuted
  design. Do not re-attempt without pricing the double-read against a
  file-larger-than-cache cell first.
- No `decisions/*`, `CORRECTNESS.md`, or `DEVIATIONS.md` entry
  governs walk strategy. This spec creates policy: ADR
  `decisions/0006-blob-count-threshold-dispatch.md` (brick 5),
  recording the constant, estimator contract, per-command scope, the
  excluded alternatives (read-and-discard walker; a body-carrying
  streaming iterator that could fuse removeid's raw passthrough -
  possible future work, out of scope), and the explicit
  non-generalization to other walk consumers pending per-consumer
  byte-fraction classification (section 8).

## 3. Target artifacts

### 3.1 Estimator (`src/read/header_walker.rs`)

```rust
/// Estimate of the OSMData blob count of a PBF, from a bounded probe
/// walk of leading blob headers.
pub(crate) struct BlobCountEstimate {
    /// OSMData blobs. Exact when `exact` is true.
    pub osmdata_blobs: u64,
    /// Walk hit EOF within the sample cap: the count is exact.
    pub exact: bool,
}

/// Probe-walk up to `SAMPLE_CAP` frames from the file head.
/// - EOF within the cap: `osmdata_blobs` = exact count of
///   `BlobKind::OsmData` frames seen (0 for a header-only or empty
///   PBF); `exact = true`.
/// - Cap reached: mean frame length = (end of last sampled frame -
///   start of first OSMDATA frame) / OSMData frames sampled;
///   `osmdata_blobs = remaining_bytes / mean + sampled`; `exact =
///   false`. The leading OsmHeader frame and any unknown-kind frames
///   are excluded from both numerator and denominator.
/// - Zero-byte / headerless / malformed-before-first-frame input:
///   propagate the walker's existing error (all three consumers
///   already fail on such input at the walk; the estimator must not
///   invent a success path).
pub(crate) fn estimate_blob_count(path: &Path) -> Result<BlobCountEstimate>;

const SAMPLE_CAP: usize = 1_000;
```

Sampling bias: the file head over-represents node frames. Node frames
dominate count (89 % europe, 65 % planet primary) and mean frame size
per kind differs by < 15 % on both reference encodings, so the
estimate discriminates the >= 3x gaps this spec dispatches on.
Because that argument is desk-derived, the estimator is VALIDATED,
not trusted: brick 1's gate measures it against the exact counts of
four registered encodings (section 6, E-gates), and every dispatch
site emits `walk_estimated_blobs` plus, on the Pread arm,
`walk_actual_osmdata_blobs` (the walk knows the true count at
schedule end) so every future bench run accumulates estimator
evidence for free.

### 3.2 Dispatch helper (`src/read/header_walker.rs`)

```rust
/// Arm selected for a header-walk-substitutable command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanArm {
    /// HeaderWalker probe preads + parallel_classify_phase.
    Walker,
    /// Pipelined reader with blob filters (single pass, inline decode).
    Pipelined,
}

/// Measured crossover brackets 51 k-522 k OSMData blobs
/// (reference/blob-density.md). getid's +209 % high-side penalty vs
/// getparents' -46 % low-side win places the constant at the low
/// end. The 51 k-522 k interior is unmeasured; the constant is a
/// policy choice inside a bracket whose endpoints are safe, recorded
/// in decisions/0006 with the counters that would justify moving it.
pub(crate) const PIPELINED_ARM_MIN_BLOBS: u64 = 150_000;

pub(crate) fn choose_scan_arm(est: &BlobCountEstimate) -> ScanArm; // pure, boundary-testable
pub(crate) fn dispatch_scan_arm(path: &Path) -> Result<(ScanArm, BlobCountEstimate)>;
```

### 3.3 getparents (`src/commands/getparents/mod.rs`)

- `pub fn getparents(...)` computes
  `let (arm, est) = dispatch_scan_arm(input)?;`, emits the counters,
  and calls `getparents_with_arm(..., arm)` -
  `pub(crate) fn getparents_with_arm` is the complete-command forcing
  surface for tests and benches of a specific arm.
- `ScanArm::Walker`: today's body, unchanged.
- `ScanArm::Pipelined`: `ElementReader::from_path_with_options(input,
  direct_io)?.with_blob_filter(BlobFilter::new(need_node_blobs,
  need_way_blobs, need_relation_blobs))` then
  `for_each_block_pipelined(|block| ...)` feeding the SAME
  `process_block` + `BlockBuilder` + writer flow the walker arm's
  classify closure uses; output element order identical (both arms
  deliver blocks in file order). One shared `emit_blocks` helper
  keeps the two arms' write paths one implementation.

### 3.4 getid include (`src/commands/getid/mod.rs`)

- `filter_by_id` gains `arm: ScanArm` (invert calls always pass
  `Walker` - see below); `pub fn getid` computes the arm for include
  mode via `dispatch_scan_arm`.
- The per-block include-mode match/emit logic is factored out of the
  walker loop into an arm-neutral helper taking `&PrimitiveBlock`
  (mirrors `getparents::process_block`).
- `ScanArm::Pipelined`: pipelined raw-frame pass with
  `BlobFilter::new(...)` for kind + the existing
  `IdSet::any_in_range` indexdata prescreen (skip decompression of
  non-intersecting blobs), decode + match + re-encode of intersecting
  blobs only - the `51c662e` shape on HEAD's reader.
- **removeid / invert stays `Walker` unconditionally.** With this
  spec's arms, the pipelined path would decode-and-re-encode
  passthrough blobs the walker arm copies raw. A body-carrying
  sequential iterator could fuse invert's raw passthrough and beat
  both arms at high blob count; that is the excluded alternative
  recorded in the ADR, not implemented here.
- **Non-indexed input (`--force`) stays `Walker` in both commands.**
  Without indexdata neither the kind filter nor the range prescreen
  exists, both arms degrade to full decode, and no cell prices the
  difference; dispatch on unmeasured ground is what this spec exists
  to stop. Stated in a comment at the dispatch sites.

## 4. Bricks, ordered

`brokkr check` green at every boundary; commit before any
measurement. Bricks 3a and 3b are SEPARATE landings so the revert
rule (section 6) can act per command.

**Brick 1 - estimator + chooser.**
`estimate_blob_count`, `BlobCountEstimate`, `ScanArm`,
`PIPELINED_ARM_MIN_BLOBS`, `choose_scan_arm`, `dispatch_scan_arm`.
Inline unit tests (in `header_walker.rs` - `pub(crate)` items are
not reachable from `tests/`, per `reference/testing.md` placement):
exact count on a < SAMPLE_CAP fixture including the OsmHeader-frame
exclusion; header-only PBF -> `osmdata_blobs == 0, exact`;
`choose_scan_arm` boundary at 149_999 / 150_000 / exact-vs-estimated.
Estimator-accuracy gate (E-gates) runs after this brick lands:

    brokkr inspect --dataset denmark --variant indexed
    brokkr inspect --dataset planet --variant indexed

plus a tier-1 unit test asserting the estimator's relative error
< 30 % on a synthetic mixed-frame-size fixture. The four registered
real encodings (denmark 1k/64k/320k snapshots + planet 8k) get their
estimator readings recorded during brick 3's bench session via the
new counters - no separate planet-scale run buys only estimator data.
Gate: `brokkr check`.

**Brick 2 - getparents arm split.** Factor the writer drain into the
shared `emit_blocks` helper; add `getparents_with_arm`; implement the
`Pipelined` arm. Tier-1 conformance test in `tests/cli_getparents.rs`
via `CliInvoker` is NOT possible for arm forcing (no CLI knob by
design), so the conformance test is a library-level tier-1 test in
`tests/` calling `pbfhogg::getparents` - which only exercises
auto-dispatch. The arm-equality test therefore lives inline in the
command module (tier 1, `mod tests`): build a multi-kind fixture via
the test-support writer, run `getparents_with_arm` under both arms
(node-ID query, and an `--add-self` variant), assert byte-identical
element sets via the normalized-read helper pattern. Reader-touching?
No reader code changes in this brick (consumers only), so `brokkr
check` + the conformance tests gate it; the full roundtrip suite is
NOT owed here (nothing in reader/writer/BlockBuilder changes -
contract clause checked, not skipped).
Gate: `brokkr check`.

**Brick 3a - getparents dispatch landing.** Wire `dispatch_scan_arm`
+ counters into `pub fn getparents`. Gates:

    brokkr check

then the bench block (section 6, G-gates), committed first. Keep or
revert THIS landing on its bounds.

**Brick 3b - getid dispatch landing.** Factor the include-mode block
logic arm-neutral; implement the `Pipelined` arm; wire dispatch +
counters; invert/non-indexed pinned to `Walker` with comments. Inline
arm-equality test (include mode, mixed-kind ID set, both arms).
Gates:

    brokkr check
    brokkr verify getid --dataset denmark --variant indexed

then the bench block (section 6, I-gates). Keep or revert THIS
landing on its bounds.

**Brick 4 - docs + ADR + baseline bookkeeping.** CHANGELOG entry
(behavior change: getparents and getid include select their scan
strategy by estimated blob count; recovered numbers). ADR
`decisions/0006-blob-count-threshold-dispatch.md`.
`reference/blob-density.md` "resolved by" pointer; TODO's getparents
open decision and getid follow-up line collapse to ADR pointers;
`reference/performance.md` gains the new current numbers,
superseded baselines settle into `reference/performance-history.md`
with the arc narrative. The europe-prefetch TODO item and sort pass 1
are cross-referenced as the named follow-on (section 8), NOT closed.

## 5. Test-forcing surface

- `choose_scan_arm` is pure: boundary unit tests.
- `getparents_with_arm` / `filter_by_id(..., arm)` are the
  complete-command forcing surfaces (internal, no CLI/env exposure).
- Auto-dispatch itself is tested tier-1 by fixture size: a
  > SAMPLE_CAP-blob fixture is impractical, so the dispatch test
  injects the threshold - `choose_scan_arm` takes the constant as a
  parameter internally (`choose_scan_arm_at(est, min_blobs)`), the
  `pub(crate)` wrapper applies the real constant, and the tier-1 test
  drives a real fixture through the full command with a 1-blob
  threshold via a `#[cfg(test)]`-only entry, asserting via the
  counters that the Pipelined arm actually ran.
- Fixture-parity set shared by both commands' inline tests: empty
  query result, single-data-blob file, mixed indexed/non-indexed
  blobs (non-indexed blob present -> Walker forced), and the
  malformed-input cases already covered by `fault_*`/defensive
  fixtures (`decisions/0004`) which both arms inherit unchanged.

## 6. Bench gates and keep/revert bounds

Discipline: commit the landing, then run gates. Order: ALL HEAD cells
first, worktree cells after, and one final HEAD build
(`brokkr getid --dataset denmark --variant indexed --bench` costs
seconds and restores HEAD artifacts) - per the AGENTS.md
`CARGO_TARGET_DIR` thrash rule. Multi-run rule: denmark/europe cells
use default `--bench` (best-of-3, per `reference/performance.md`
reading rules); planet-scale cells use `--bench 1` (hours-budget
exception, matching every existing planet baseline in section 1 -
those were also single-run, so the comparison is like-for-like).

G-gates (brick 3a, getparents):

    brokkr getparents --dataset planet --variant indexed --bench 1
    brokkr getparents --dataset planet --variant indexed --snapshot 8k --bench 1
    brokkr getparents --dataset europe --variant indexed --bench

I-gates (brick 3b, getid):

    brokkr getid --dataset planet --variant indexed --bench 1
    brokkr getid --dataset planet --variant indexed --snapshot 8k --bench 1
    brokkr getid --dataset europe --variant indexed --bench

| gate | keep bound (hard) | reference |
|---|---|---|
| getparents planet primary | <= 25.0 s | 23.5 s walker win preserved |
| getparents europe | <= 29.0 s | 26.4 s scan arm + 10 % |
| getparents planet 8k | <= 58.0 s | 52.8 s scan arm + 10 % |
| getid planet primary | <= 6.7 s | 6.1 s walker win preserved |
| getid europe | <= 19.7 s | 17.9 s scan arm + 10 % |
| getid planet 8k | <= 36.5 s | 33.2 s scan arm + 10 % |

Bounds are single-valued and hard: at or under = keep, over = revert
that command's landing (no secondary tolerance band - revision 1's
"+15 % on top of the bound" ambiguity is removed). Wall is the
verdict axis; additionally each 8k cell's sidecar is read
(`brokkr sidecar <UUID> --human`) to confirm the schedule/walk phase
is GONE from the Pipelined-arm profile (the mechanism check, not just
the total). Disk-read bytes from the sidecar are recorded in
performance.md alongside wall but carry no bound - the arms
legitimately differ in read volume per encoding.

The dispatch-correctness check rides the same runs: the emitted
`walk_estimated_blobs` counter must select Pipelined at 8k/europe and
Walker at planet primary. A wrong arm choice is a revert regardless
of wall.

Revert leaves brick 1+2 artifacts in place only if brick 3b survives;
if BOTH landings revert, bricks 1-2 revert too (an estimator with no
consumers is dead weight, and this spec's premise returns to
notes/getparents.md as an open decision with the revision-2 numbers
appended).

## 7. What this spec does NOT protect (stated, accepted)

- Query shapes beyond brokkr's preset ID sets (dense million-ID
  queries shift the Pipelined arm's decode volume). The counters
  record arm + estimate on every run, so a future workload complaint
  is diagnosable from its sidecar. Custom ID-set distribution
  benching is TODO's existing "Custom ID set distributions" item.
- The 51 k-522 k interior crossover. Endpoints are safe by
  measurement; the constant inside is policy (ADR).
- Non-indexed high-blob-count inputs (pinned to Walker; unmeasured).

## 8. Stopping rule and follow-ons

- `sort` pass 1: named follow-on, own mini-spec after this lands -
  its old arm is seek-skip streaming (a third mechanism), its
  asymmetry is 5x shallower, and its gate needs a fresh
  `--commit 1f97fae^ --bench` europe baseline that does not exist in
  results.db today.
- Other `HeaderWalker`/schedule consumers (check, extract,
  tags-filter, cat --type, geocode, renumber, repack, degrade,
  apply-changes scanner, diff, time-filter, altw, multi-extract,
  tags-count): explicitly out of scope. The ADR records that
  extending dispatch to any of them requires the same two-arm
  measurement this spec's consumers got - classification by
  downstream byte fraction alone is not a verdict (revision 1's
  blanket "they decode most bytes" claim was wrong for the selective
  ones). Their shared high-blob-count lever remains io_uring-batched
  header probes - separate item.
- `inspect` index-only: the walk is the command; no second arm.
- removeid body-carrying streaming fusion: excluded alternative,
  recorded in the ADR.
- No user-facing knob; no env vars; internal forcing surfaces only.

## 9. Standing references

- Contract: `reference/technical-implementation-spec.md`
- Spawned from: `reference/blob-density.md`, `notes/getparents.md`
- Tests contract: `reference/testing.md`
- Measurement record: `reference/performance.md`,
  `reference/performance-history.md`, `.brokkr/results.db`
  (section-1 UUIDs; all cells plantasjen)
- Review record: codex xhigh critique 2026-07-11 (14 findings), this
  revision's section 2 "Failure history" logs the refuted design
- ADR added: `decisions/0006-blob-count-threshold-dispatch.md`
