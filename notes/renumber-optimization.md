# `renumber --index-type external` optimization notes

Low-priority maintenance file. `renumber` is not on the critical path;
the three items below are potential wins preserved for future pick-up,
not active work.

## Current baseline

| Commit    | UUID       | Wall     | Peak Anon    | Temp Disk | Mode       |
|-----------|------------|----------|--------------|-----------|------------|
| `aee7727` | `abd74459` | 204.5 s  | 3.3 GB       | 0         | `--bench 3` |
| `cb99106` | historical | 194 s    | (same shape) | 0         | `--bench 3` |
| `cb99106` | `0b6d13e3` | 213 s    | (same shape) | 0         | `--bench 1` |

All on plantasjen. The **+10 s drift between `cb99106` and `aee7727`**
on `--bench 3` is inside variance but not comfortably so. The 2026-04-19
re-run at `cb99106` came in at **213 s** on `--bench 1` - 19 s above the
194 s `--bench 3` number at the same commit, which is within single-
sample variance on this workload. The single-sample at `cb99106` is
actually slower than the `--bench 3` number at `aee7727` (213 vs 204.5
s), so the direction of the "drift" is not yet established; the `abd74459`
post-drift figure comes from a 3-sample min while `0b6d13e3` is one shot.
Re-run pre-drift on `--bench 3` if renumber becomes the critical path -
the 10 s gap may vanish under matched sample counts.

Architecture summary (see `src/commands/renumber/mod.rs` doc comment):
pass 1 (parallel node rewrite) → build node rank index → stage 2d
(parallel way rewrite with inline ref resolve, builds `way_id_set`) →
R1 (sequential relation ID collect) → R2d (parallel relation rewrite
with member ref resolve).

## Item 1: Varint encode fast path in reframe functions

**Potential win:** plausible, unmeasured. TODO's original estimate was
−2 to −3 s wall. That's ~1-1.5 % of the 204.5 s baseline - inside the
observed `--bench 3` drift, so it needs a paired bench at the same
commit to prove.

**Current code.** `src/commands/renumber/wire_rewrite.rs` calls
`protohoggr::encode_varint(buf: &mut Vec<u8>, value: u64)` at three
hot sites:

- Line 143: new node ID in pass 1 reframe
- Line 297: new ref delta in stage 2d way reframe (hottest - 1 call per
  way ref, ~10 B at planet)
- Line 530: new member ID delta in R2d relation reframe

`encode_varint` is a `Vec::push` loop with no fast-path branch:

```rust
pub fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}
```

**Available primitive.** protohoggr already ships
`encode_varint_to_slice(buf: &mut [u8], value: u64) -> usize` with a
branchless 1-byte and 2-byte fast path. The crate docs claim it
covers ~95 % of zigzag-encoded OSM way-ref deltas:

```rust
#[inline]
pub unsafe fn encode_varint_to_slice(buf: &mut [u8], value: u64) -> usize {
    if value < 0x80 {
        unsafe { *buf.get_unchecked_mut(0) = value as u8 };
        return 1;
    }
    if value < 0x4000 {
        // branchless 2-byte write
        ...
    }
    // general case
}
```

**Swap shape.** Each call site converts like so (per-call resize +
truncate, no prior reservation):

```rust
let pos = refs_scratch.len();
refs_scratch.resize(pos + 10, 0);
let n = unsafe {
    protohoggr::encode_varint_to_slice(&mut refs_scratch[pos..pos + 10], value)
};
refs_scratch.truncate(pos + n);
```

Or, for the ref-encoding loop that runs many times, batch the
reservation once per way and pre-fill with zeros, then track a cursor
manually.

**Regression risks.**

1. **Unsafe surface widens.** The helper is `pub unsafe fn`. Each call
   site must guarantee `buf.len() >= 10`. Miss the `resize(+10, 0)` →
   UB over uninit memory. Miss the `truncate(pos + n)` → silent output
   corruption (trailing zero bytes make a varint look like it continues).
2. **Per-call bookkeeping can cancel the win for 1-byte values.**
   `resize(+10, 0)` + `truncate` is a 10-byte memset + length adjust vs
   the current single `Vec::push`. The branchless fast path saves one
   branch-predicted loop iteration; the memset likely costs less but
   not zero. Batched reservation (per way or per blob) amortises this
   but requires more code restructure.
3. **Readability cost.** Three clean one-liners become three 4-line
   blocks with `unsafe`. Every reader of the reframe path pays this.

**Action if prioritised.**

1. Baseline: run `brokkr renumber --dataset planet --bench 3` at
   current HEAD. Pin the number.
2. Land the swap with per-call resize+truncate first (simplest).
3. Rerun `--bench 3`. If the delta is below `--bench 3` noise (~5 s at
   this workload), revert. If the delta is real (≥ 5 s or consistent
   across 3+ samples), decide whether to also do batched reservation
   for another small slice.
4. Include the measurement in the commit message.

Not today - skip unless renumber wall moves into the critical-path
tier (e.g. a 2× degradation from current baseline).

## Item 2: Skip `way_id_set` if way rank derivable from schedule

**Potential win: considered and shelved.** Not a potential optimisation
at planet scale. Recording here as a load-bearing pin against
re-proposal.

**The shape the TODO originally suggested.** Sorted input means new
way ID = `start_way_id + global_position`, so if we know each way's
global position from the schedule's prefix sums we don't need a full
`IdSet`. Would save ~160 MB peak anon at planet.

**Why it doesn't fly.** The `way_id_set` is not needed for the **write
side** of stage 2d - `stage2.rs:48-58` already computes `base_way_ids`
from schedule prefix sums and workers increment within each blob
(`base + local_position`). The IdSet is built for **R2d's reverse
lookup**: given an old way ID X referenced by a relation member, find
X's new ID.

Deriving that from the schedule alone requires **dense IDs within
each blob** - `count == max_id - min_id + 1`. Real PBFs have gaps
from past deletions, so that condition fails for most blobs at
planet scale. A fallback IdSet would still be needed for non-dense
blobs, which is most of them.

The 160 MB peak anon is already cheap for what it buys (O(1) rank
lookup via a compact bitmap). Alternatives considered:

- Sorted `Vec<i64>` of all way IDs: ~7.2 GB at planet (900 M × 8 B) -
  much heavier than the IdSet.
- Per-blob sorted way-ID list: similar size plus indirection cost.
- Binary search on schedule to find containing blob + decode to find
  rank within blob: restores the full-decode cost we eliminated.

**Status: do not re-propose.** The IdSet is the right structure. The
160 MB cost is not significant at planet scale and the alternatives
are strictly worse.

## Item 3: Finer `reframe_ms` breakdown into parse / lookup / encode / frame

**Potential value: measurement, not optimisation.** Would partition
the stage 2d reframe cost across its sub-operations so future work
knows which sub-step dominates. Not itself a speedup.

**Current instrumentation.** `StageCounters::reframe_ms` in
`renumber/mod.rs:109` measures total reframe wall per blob. No
per-sub-step breakdown. The reframe work in `reframe_ways_with_new_ids`
(wire_rewrite.rs:174) interleaves four sub-operations per way:

1. **Parse** - wire-format cursor walk (outer, group, way, refs
   cursors reading tags and varints)
2. **Lookup** - `node_id_set.resolve()`, `node_id_set.get()`,
   `way_id_set.set()` IdSet queries
3. **Encode** - `protohoggr::encode_varint`, `encode_bytes_field`,
   `encode_tag` writes
4. **Frame** - splicing replacement ranges into the output buffer

**Why it's non-trivial.** These are tightly interleaved per way in
one inline loop - there are no natural sub-functions. Two usable
instrumentation shapes:

1. **Per-way `Instant::now()` + `fetch_add` at 4 points.** Overhead
   per way is probably higher than the work being measured (each
   sub-op is nanoseconds; a timing sample is ~20 ns plus atomic
   contention across 6 stage-2d workers). Results would be dominated
   by measurement noise.
2. **Restructure to batch.** Lift each sub-op into its own function or
   pass and time per-function via `#[cfg_attr(feature = "hotpath",
   hotpath::measure)]`. Requires real refactoring of the reframe inner
   loop - probably a net loss on hot-path ergonomics if the result is
   no optimisation follow-up.
3. **Sampled timing** - take an `Instant::now()` every N ways (say
   N=1000) and apportion the interval across 4 sub-ops. Cheap but
   imprecise.

**Action if prioritised.** Only worth building when there's a
specific optimisation candidate whose wall-share needs measurement
before design. Landing the instrumentation cold (no follow-up
optimisation ready) adds maintenance with no ROI.

**Status: preserved here; no standalone work.** Revisit when
renumber has an active optimisation to prioritise between items.
