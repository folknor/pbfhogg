# `degrade --drop-ids N:SEED` - implementation specification

Written against [`reference/technical-implementation-spec.md`](../reference/technical-implementation-spec.md).
Spawned from the v2.4 item in [`notes/degrade.md`](degrade.md) ("v2.4 -
`--drop-ids N:SEED`"). Test bricks additionally satisfy
[`reference/testing.md`](../reference/testing.md).

This document resolves every open design question in the v2.4 sketch to a
concrete, buildable artifact. Two implementers working from it alone produce
the same command surface, the same selection rule, the same on-disk output for
a given input and `N:SEED`, and the same tests.

---

## 1. Goal and consumer

Introduce a deterministic, reproducible set of *dropped elements* into a
degrade output so that surviving ways/relations that referenced the dropped
IDs become **dangling references**. The primary consumer is
`check --refs` (slow-path / dangling-reference benchmarking and error-recovery
validation): the number and identity of resulting dangling references must be
a well-defined, testable function of the input and `N:SEED`.

Non-goals (explicitly out of scope, per the parent notes):

- Element filtering by tag/type/bbox (use `getid`, `tags-filter`, `extract`).
- Any probabilistic / rate-based dropping. `N` is an **exact absolute count**
  (Section 3), not a per-mille or probability. This is a deliberate decision:
  the consumer needs an exact, reproducible dangling count, which a rate cannot
  give.
- Repairing or renumbering after the drop.

---

## 2. Survey of the ground

### 2.1 Current `degrade` architecture (unchanged pieces)

`src/commands/degrade/mod.rs` picks one of two paths up front from the flag
set (`DegradeFlags::needs_decode`):

- **Passthrough** (`--strip-indexdata` and/or `--strip-tagdata` only): raw blob
  frames copied through, only targeted `BlobHeader` fields cleared. Never
  decodes payloads, never changes blob/element counts.
- **Decode path** (`--unsort` / `--unsort-intra` / `--strip-locations`): three
  sequential per-kind phases (`nodes -> ways -> relations`) driven by
  `crate::scan::classify::parallel_classify_phase` over per-kind schedules from
  `build_classify_schedules_split`. Workers decode one input blob, filter to the
  current kind, and either pre-frame full cap blocks (`WorkerOutput.full_framed`)
  or ship trailing/all elements as `Owned*` (`WorkerOutput.tail`); a merge thread
  runs a single central `BlockBuilder` per kind. Requires blob-level indexdata
  (or `--force`).

`DegradeFlags` is a `Copy` struct of `bool`s. `DegradeStats { blobs_written,
elements_written, flags }` is printed by `print_summary`. Counters and markers
are emitted via `crate::debug::emit_counter` / `emit_marker`.

### 2.2 Why `--drop-ids` cannot use the passthrough path

Dropping elements changes per-blob element counts and therefore blob framing:
a passthrough that copies blob bytes verbatim cannot remove elements. `--drop-ids`
therefore **forces the decode path** (it must decode, filter out the dropped
elements, and re-frame). This matches the v2.4 sketch exactly.

### 2.3 The consumer: `check --refs`

`src/commands/check/refs.rs::check_refs` runs a three-phase parallel scan
(nodes -> ways -> relations) building three `IdSet`s and checking references
against them. It reports (via `RefCheckResult`, surfaced by
`check --refs [--check-relations] --json` as machine-readable counts):

- `missing_node_refs`  - unique dropped-node IDs referenced by surviving ways.
- `missing_node_members` - unique dropped-node IDs referenced by surviving
  relations' node members.
- `missing_way_refs` - unique dropped-way IDs referenced by surviving relations'
  way members.
- `missing_relation_members` - unique dropped-relation IDs referenced by
  surviving relations' relation members (deduplicated;
  `missing_relation_member_occurrences` is the pre-dedup occurrence count).

All missing-count fields are **unique** (deduplicated) counts. `check --refs`
membership is against the **output** element set: a reference is dangling iff its
target `(kind, id)` is absent from the output. Because degrade's output is the
input minus the dropped set `D`, the exact dangling counts are a pure function
of `D` and the input's reference structure (Section 7.3). `check_refs` relies on
kind separation (all nodes in node blobs, etc.), which degrade's per-kind decode
preserves; it does not rely on intra-kind ID order for correctness, so it is
correct even when `--drop-ids` composes with `--unsort` (Section 6).

Note for tests: `check --refs` returns **exit code 1** when integrity fails
(`ExitWithCode(1)`), while still printing the JSON report to stdout. Tests must
read stdout from `CliInvoker::run()` (which does not assert on status) rather
than `assert_success()`.

### 2.4 Standing decisions checked

- `decisions/0002-negative-ids-rejected-project-wide.md`: negative IDs are
  rejected project-wide. `--drop-ids` never *creates* IDs; it hashes existing
  IDs. It imposes no new constraint and honors the decision (the hash accepts
  any `i64` the reader already accepted). No ADR change.
- `CORRECTNESS.md` / `DEVIATIONS.md`: `--drop-ids` produces an intentionally
  broken (dangling-ref) file; that is the explicit purpose of `degrade` and is
  already the documented character of the command. No new deviation from osmium
  is introduced (osmium has no analogous producer). **No new ADR is required**;
  this is a feature landing on an existing adversarial-generator command, not a
  new architecture policy.

---

## 3. Semantics of `N` and `SEED`

- **`N`** (`u64`): the exact number of elements removed from the output.
  `elements_written == input_element_count - N` exactly (Section 7.1). `N >= 1`
  is required; `N == 0` is rejected at the CLI (a no-op is not a transformation).
  If `N` exceeds the input's total element count, the run fails with a hard error
  after the count is known (Section 5.3) - dropping more elements than exist is
  ill-defined.
- **`SEED`** (`u64`): perturbs the hash so different seeds select different drop
  sets while the same `(N, SEED)` always selects the identical set. `SEED` may be
  any `u64` including `0`.

Selection is **global across all three kinds**, not per kind. Because nodes vastly
outnumber ways and relations in real data, the overwhelming majority of dropped
elements are nodes - which maximizes dangling references (way->node and
relation->node), matching the consumer's intent. This is a deliberate choice
over per-kind quotas.

---

## 4. Selection rule (exact, reproducible, order-independent)

### 4.1 Ordering key and total order

For every element in the input define:

```
kind(e) in { KIND_NODE = 0, KIND_WAY = 1, KIND_RELATION = 2 }   // existing constants
key(e) = ( drop_hash(kind(e), id(e), SEED),  kind(e),  id(e) )   // (u64, u8, i64)
```

Order keys lexicographically: primary by hash ascending, then kind ascending,
then id ascending.

The **dropped set `D`** is the `N` elements with the smallest keys under this
order. On a valid input each `(kind, id)` identifies exactly one element, so
`(hash, kind, id)` is a **strict total order with no ties** and `D` is uniquely
determined by the set of `(kind, id)` pairs and `SEED` alone - independent of
blob layout, thread scheduling, decode order, or merge order. Two runs with the
same input and `N:SEED` select the byte-identical `D`.

**Uniqueness precondition (required for the exact-count contract).** `--drop-ids`
requires that every element have a distinct `(kind, id)`. Every sorted / valid
OSM PBF satisfies this by construction, as does every degrade fixture. degrade
does **not** enforce it: detecting duplicates would need an `O(elements)` seen-set,
defeating the bounded-memory pre-pass (Section 4.3). The consequence on malformed
input is precise and must be documented, not hidden:

- The selection key `(hash, kind, id)` and the drop filter (Section 5.2) both key
  on `(kind, id)`. Two elements sharing a `(kind, id)` share an **identical
  complete key** - `(kind, id)` does *not* break that tie (the earlier draft's
  claim that it did was wrong).
- Because `DropSets` stores `(kind, id)` membership, not per-occurrence identity,
  selecting one duplicate removes **every** occurrence of that `(kind, id)`. So on
  input with duplicate `(kind, id)` pairs, more than `N` elements may be dropped
  and `elements_written == input_element_count - N` (Section 7.1) can fail.

The exact-count contract is therefore stated **only for unique-`(kind, id)`
input**, which is the only input the consumer (`check --refs`) is defined against
and the only input the tests build. This keeps the contract clean and testable
(the fixture has unique ids, so the exact count holds exactly) while being honest
that degrade accepts arbitrary bytes and does not police this precondition.

### 4.2 The hash `drop_hash`

Fully specified inline; **no new crate**. A splitmix64-style finalizer over a
mixed word of `kind`, `id`, and `SEED`. Add to the degrade module:

```rust
/// splitmix64 finalizer. Fully specified so the dropped-id selection is
/// reproducible byte-for-byte across builds and hosts. Not a general hash
/// API - private to --drop-ids selection.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministic per-element drop score. `kind` participates so the same id
/// across kinds (node 5 vs way 5) scores independently; the ordering key in
/// Section 4.1 also carries kind+id, so uniqueness never depends on this.
fn drop_hash(kind: u8, id: i64, seed: u64) -> u64 {
    #[allow(clippy::cast_sign_loss)]
    let idw = id as u64; // bit-cast; monotonic mapping is irrelevant, we hash it
    // Single XOR of the FULL 64-bit seed into the splitmix64 finalizer input.
    // The finalizer avalanches it across all output bits, so every seed bit
    // matters. Do NOT fold the seed as `seed.rotate_left(32) ^ seed`: that
    // collapses a 64-bit seed a:b to (a^b):(a^b), making seed 0 and every seed
    // of the form x:x score identically - a 32-bit-effective seed. All 64 bits
    // must survive.
    let w = idw
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ u64::from(kind).wrapping_mul(0xD1B5_4A32_D192_ED03)
        ^ seed;
    mix64(w)
}
```

These exact constants and operations are part of the contract: changing them
changes which IDs are dropped for a given seed. Any two implementations must use
this function verbatim.

**Golden vectors (pin these; they encode the corrected 64-bit seed
incorporation).** A mandatory inline unit test (Section 8.3) asserts:

```
mix64(0x0000000000000000)                         == 0x0000000000000000
mix64(0x0000000000000001)                         == 0x5692161d100b05e5
mix64(0xffffffffffffffff)                         == 0xb4d055fcf2cbbd7b
drop_hash(kind=0, id=1,  seed=0x0)                == 0xe220a8397b1dcdaf
drop_hash(kind=1, id=1,  seed=0x0)                == 0xd28f049168bdd34c
drop_hash(kind=2, id=42, seed=0x0)                == 0x454c00469e5363e2
drop_hash(kind=0, id=1,  seed=0x1)                == 0xe4d971771b652c20
drop_hash(kind=0, id=1,  seed=0x100000000)        == 0x219fc13d6bc5b015
drop_hash(kind=2, id=42, seed=0xdeadbeefcafebabe) == 0xdd1cb91ccef48036
```

`mix64(0) == 0` is the splitmix64 zero fixed point (expected). The
`seed=0x1` and `seed=0x100000000` vectors both differ from `seed=0x0` and from
each other, which is the concrete proof that both a low seed bit and bit 32 reach
the output - i.e. the 64-bit-collapse defect is gone.

### 4.3 Bounded-memory selection algorithm

`D` is computed in a **pre-pass** (Section 5.2) that visits every element once
and keeps only the smallest keys. The key type derives lexicographic `Ord`:

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DropKey { hash: u64, kind: u8, id: i64 } // field order == Section 4.1 key
```

**Primitive: `parallel_classify_phase`** (not `parallel_classify_accumulate`).
This is the deliberate, load-bearing choice. `parallel_classify_phase` streams a
per-blob result `R` to the consumer and the consumer holds **one** merge state;
`parallel_classify_accumulate` would hold a size-`N` heap **per worker** and fold
them at the end, which is `O(threads * N)`. We want peak memory independent of
thread count, so:

- **Worker state** `S = ()` (no cross-blob accumulation).
- **Per-blob result** `R = BlockDrop { matched: u64, smallest: Vec<DropKey> }`,
  where the worker (a) counts `matched` = number of elements of *this scan's
  kind* in the blob (Section 5.2 mandates the kind filter) and (b) reduces them to
  their `min(N, matched)` smallest `DropKey`s via a local size-`N` max-heap. So
  `smallest.len() <= min(N, blob_element_count)` and is bounded by blob size
  (~8000), never by `N` alone.
- **Merge state** (owned by `select_drop_sets`, consumer thread only): one global
  `heap: BinaryHeap<DropKey>` and one `total: u64`. For each `BlockDrop`:

  ```rust
  total += r.matched;                     // exact element count, every kind once
  for k in r.smallest {
      if (heap.len() as u64) < n { heap.push(k); }
      else if k < *heap.peek().unwrap() { heap.pop(); heap.push(k); }
  }
  ```

The `seq` argument from `parallel_classify_phase`'s `merge(seq, R)` is ignored:
selection depends only on the key set, so worker-completion order is irrelevant.

**One heap across all three kinds.** Selection is global (Section 3), so the same
`heap` and `total` are threaded through all three `parallel_classify_phase` calls
(node, way, relation schedules). After the third scan, if `n > total` the run
errors (Section 5.3); otherwise the heap holds exactly `min(N, total)` keys - the
drop set - drained into `DropSets` partitioned by `kind`.

**`u64` `N` vs `usize` capacity.** `N` stays `u64` throughout selection. The
capacity test is `(heap.len() as u64) < n`, so an astronomically large `N` (up to
`u64::MAX`) never triggers a `usize` conversion and never pre-allocates: the heap
grows only as elements arrive and so holds at most `min(N, total)` entries. `N >
usize::MAX` (possible only on a 32-bit target) is therefore harmless during
selection; it can only fail the later `N > total` check. No eager `usize::try_from`
is required or performed.

**True peak-memory formula (thread-independent).** With `T` decode threads,
per-`DropKey` size 24 bytes (`u64` + `i64` + padded `u8`), channel depth 32
(`parallel_classify_phase`'s result channel), and blob size `B <= ~8000`:

```
peak_selection  ~=  min(N, total) * 24         // one global heap
                  + 32 * min(N, B) * 24         // in-flight per-blob results
                  + T * min(N, B) * 24          // workers' local per-blob heaps
```

The dominant term is `O(min(N, total))` for the global heap; the in-flight and
per-worker terms are bounded by blob size (`min(N, B)`), **not** by `N * T`.
Because `N <= total` after the check, worst case is `O(total)` = `O(elements)` in
the pathological `N == total` case - stated honestly here rather than claimed flat
`O(N)`. There is no accepted-OOM policy to specify: a size-`total` heap of 24-byte
keys is far smaller than the input the same pass already decodes. `D` is identical
and order/thread-independent for any `T` because the global N-smallest of the key
set is a pure function of the key set (every global N-smallest key is among some
blob's local `min(N, matched)` smallest - standard top-K merge correctness).

`D` is partitioned by kind into three membership sets consulted by the emit phase:

```rust
struct DropSets {
    nodes: rustc_hash::FxHashSet<i64>,
    ways: rustc_hash::FxHashSet<i64>,
    relations: rustc_hash::FxHashSet<i64>,
}
```

`rustc-hash` is already a dependency (`Cargo.toml`); `FxHashSet<i64>` is `Sync`
and `.contains(&id)` is a lock-free read, safe to share `&DropSets` across the
emit-phase classify workers. **No dependency is added.**

---

## 5. Implementation

### 5.1 CLI surface (`cli/src/main.rs`)

Add to the `Command::Degrade` variant, adjacent to the other transformation
flags:

```rust
/// Drop exactly N elements, chosen deterministically by hashing each
/// element id with SEED, so that ways/relations referencing them dangle.
/// Forces the decode path. Format: N:SEED (both required, e.g. 5000:42).
#[arg(long = "drop-ids", value_name = "N:SEED")]
drop_ids: Option<String>,
```

Thread `drop_ids` through the `Command::Degrade { .. } => run_degrade(..)`
match arm and into `run_degrade`'s parameter list (add `drop_ids: Option<String>`).

Parsing in `run_degrade` (before building `DegradeFlags`):

```rust
let drop_spec = drop_ids
    .as_deref()
    .map(pbfhogg::degrade::DropSpec::parse)
    .transpose()?;
```

`DropSpec::parse` (in the degrade module, so the format is one authority). It
returns the degrade module's `Result<T>` alias (the `super::Result` it already
imports, = `crate::BoxResult<T>` = `std::result::Result<T, Box<dyn
std::error::Error>>`), so `.ok_or("literal")?`, `format!(...).into()`, and the
`.transpose()?` in `run_degrade` all type-check against the existing signatures:

```rust
#[derive(Clone, Copy, Debug)]
pub struct DropSpec { pub n: u64, pub seed: u64 }

impl DropSpec {
    /// Parse `N:SEED`. The colon and both fields are mandatory.
    pub fn parse(s: &str) -> Result<Self> {
        let (n_str, seed_str) = s.split_once(':').ok_or(
            "--drop-ids expects N:SEED (e.g. 5000:42); the ':' separator is required",
        )?;
        let n: u64 = n_str.trim().parse().map_err(|_| {
            format!("--drop-ids: N must be a non-negative integer, got {n_str:?}")
        })?;
        let seed: u64 = seed_str.trim().parse().map_err(|_| {
            format!("--drop-ids: SEED must be a non-negative integer, got {seed_str:?}")
        })?;
        if n == 0 {
            return Err("--drop-ids: N must be >= 1 (dropping zero elements is a no-op)".into());
        }
        Ok(Self { n, seed })
    }
}
```

Reject rules, in order: missing `:` -> error; non-numeric field -> error;
`N == 0` -> error. (A `split_once(':')` means an input like `5000` errors on the
missing separator, and `5000:` / `:42` error on the empty numeric field.)

### 5.2 Module changes (`src/commands/degrade/mod.rs`)

**`DegradeFlags`** gains one field (stays `Copy`, since `Option<DropSpec>` is
`Copy`):

```rust
pub drop_ids: Option<DropSpec>,
```

Update:

- `any()` - include `|| self.drop_ids.is_some()`.
- `needs_decode()` - include `|| self.drop_ids.is_some()` (drop always decodes).
- `unsort_any()` / `suppress_boundary_flush()` - unchanged (drop does not alter
  the unsort machinery).

**`degrade()`** validation block: after the existing checks, no extra guard is
needed here for `N` bounds (that requires the element count, checked in the
decode path, Section 5.3). Emit the intent counters alongside the existing ones:

```rust
crate::debug::emit_counter("degrade_drop_ids", i64::from(flags.drop_ids.is_some()));
if let Some(spec) = flags.drop_ids {
    crate::debug::emit_counter("degrade_drop_n", spec.n as i64);      // cast guarded
    crate::debug::emit_counter("degrade_drop_seed", spec.seed as i64); // cast guarded
}
```

Counter representation (explicit contract): `emit_counter` takes `i64`, so a
`spec.n` or `spec.seed` value `>= 2^63` wraps to negative in the emitted counter.
These counters are **sidecar-only diagnostics** (they never affect selection or
output), so the wrap is accepted, not defended against; the
`#[allow(clippy::cast_possible_wrap)]` that guards the cast stays. `SEED` and `N`
accept the full `u64` range at the CLI regardless; only the diagnostic counter
value is representation-limited.

Because `drop_ids` sets `needs_decode()`, `degrade()` dispatches to
`degrade_decode_path` (the passthrough branch is never taken when `drop_ids` is
`Some`).

**`degrade_decode_path()`** - **run the selection pre-pass, including the
`N > total` check, BEFORE opening the output writer.** Today the writer is opened
(`writer_from_header_bytes`, which writes the header) immediately after cloning
the header and before the schedules are built. Reorder so a selection failure
leaves **no** output file:

1. Read/clone the header and (unless `--strip-locations`) warn about
   `LocationsOnWays` loss - unchanged.
2. Build the three per-kind schedules (`build_classify_schedules_split`) - moved
   up, so the pre-pass can reuse them and `shared_file` (no second header walk).
3. If `flags.drop_ids.is_some()`, run `select_drop_sets` (below). It returns the
   `DropSets` **or** the `N > total` error. Because this happens before
   `writer_from_header_bytes`, an `N > total` error - or any decode/selection
   error in the pre-pass - aborts with no output file created, not a header-only
   partial. (Without `--drop-ids`, no pre-pass runs and ordering is unchanged.)
4. Only now compute `preserve_sorted` / `header_bytes` and open the writer.
5. Thread `drop_sets.as_ref()` into the three emit phases.

```rust
let (node_schedule, way_schedule, rel_schedule, shared_file) =
    crate::scan::classify::build_classify_schedules_split(input)?;

let drop_sets = if let Some(spec) = flags.drop_ids {
    crate::debug::emit_marker("DEGRADE_DROP_SELECT_START");
    let sets = select_drop_sets(
        &shared_file, &node_schedule, &way_schedule, &rel_schedule, spec,
    )?; // returns Err on N > total: writer not yet opened, so no partial output
    crate::debug::emit_marker("DEGRADE_DROP_SELECT_END");
    Some(sets)
} else {
    None
};

// writer opened AFTER selection succeeds:
let preserve_sorted = !flags.unsort_any() && header.is_sorted();
let header_bytes = build_output_header(&header, preserve_sorted, overrides, |hb| hb)?;
let mut writer =
    writer_from_header_bytes(output, compression, &header_bytes, direct_io, io_uring)?;
```

Thread `drop_sets.as_ref()` (an `Option<&DropSets>`) into each
`run_kind_phase(..)` call and on into `worker_decode_kind(..)`. The emit phases
consult only the set matching their kind.

`select_drop_sets` runs the three `parallel_classify_phase` scans and merge state
specified in Section 4.3 (one global `heap` + `total` across all three kinds). The
classify closure of each scan **must match only its phase's `Element` variants** -
the node scan tests `Element::Node | Element::DenseNode` and ignores ways and
relations, the way scan tests only `Element::Way`, the relation scan only
`Element::Relation`. This is mandatory, not incidental: `build_classify_schedules_split`
replicates every **unindexed** blob (the `--force` path) into all three schedules,
so a scan that hashed/counted every element in the blob would hash and count each
mixed-blob element three times, corrupting both `total` and `D`. With the kind
filter, a mixed unindexed blob contributes its nodes to the node scan only, its
ways to the way scan only, and its relations to the relation scan only - so `total`
counts each element exactly once and `D` is correct on both indexed (homogeneous
blobs) and `--force` (mixed blobs) inputs. Each scan reads only the id of a matched
element (no metadata decode is required for the key). After all three scans:

```rust
if spec.n > total {
    return Err(format!(
        "--drop-ids: cannot drop {} elements, input has only {}",
        spec.n, total,
    ).into());
}
```

then drain the global heap into `DropSets` partitioned by `kind`.

Rationale for reusing the per-kind classify scans rather than a single all-blobs
scan: it needs no new schedule shape, inherits degrade's existing indexdata
precondition and `--force` handling, and keeps the code path uniform with the emit
phases. Determinism is unaffected (selection depends only on the key set).

**`worker_decode_kind()`** - add `drop: Option<&FxHashSet<i64>>` (the set for
this worker's `kind`, or `None` when `--drop-ids` is off). Fold the drop test
into the existing match predicate so a dropped element is treated exactly like a
non-matching element - it is neither counted nor emitted:

```rust
let is_match = matches!(/* existing kind match */)
    && drop.map_or(true, |d| !d.contains(&element_id));
```

where `element_id` is the id of the current element (available from the same
`Element` variant already matched). This must gate **both** the `total` count at
the top of the function (change those three `.filter(...).count()` calls to also
exclude dropped ids) **and** the per-element emit loop, so `full_count` / tail
splitting stays consistent with the number of surviving elements. Everything
downstream (worker pre-framing, tail shipping, the merge thread's central
`BlockBuilder`, the unsort swap) then operates purely on survivors.

Consequences that fall out for free:

- Survivors are emitted in their original stream order, so **sortedness is
  preserved**: `preserve_sorted = !flags.unsort_any() && header.is_sorted()`
  is still correct with drop (dropping a subsequence of a monotone sequence
  leaves it monotone). The output header's `Sort.Type_then_ID` claim remains
  valid after dropping - confirmed. (When `--unsort`/`--unsort-intra` also set,
  the flag is cleared as today; drop does not change that.)
- The unsort swap fires exactly once per kind **that retains more than `hold_at`
  survivors**: its `seen`/`hold_at` state machine counts only the elements fed to
  the central builder, which are now survivors. A kind reduced to `<= hold_at`
  survivors does not swap (Section 6); size drop tests to keep every eligible kind
  above `hold_at`.

**`DegradeStats`** gains `pub dropped: u64` (0 when `--drop-ids` is off, else
`N`). `degrade_decode_path` sets it. `print_summary` appends, when `dropped > 0`,
`" (dropped N elements)"`; and `--drop-ids` is listed in the `applied` vector
(as `--drop-ids`). After the phases, emit:

```rust
crate::debug::emit_counter("degrade_dropped_elements", stats.dropped as i64); // guarded cast
```

### 5.3 Bounds and error surface (exact messages)

- `--drop-ids 0:SEED` (zero N, colon present) -> CLI parse error
  `--drop-ids: N must be >= 1 (dropping zero elements is a no-op)`.
- `--drop-ids 0` (bare, no colon) -> the **missing-separator** error, not the
  `N must be >= 1` error. `split_once(':')` runs first, so any colon-less input -
  including a bare `0` - fails on the separator before `N` is ever parsed. The
  earlier draft claimed bare `0` produced the `N must be >= 1` message; it does
  not, and the tests (Section 8.1 #5 uses `0:1`, #6 uses `10`) already match the
  real ordering.
- Missing colon -> `--drop-ids expects N:SEED (e.g. 5000:42); the ':' separator is required`.
- Non-numeric field -> `--drop-ids: N must be ...` / `SEED must be ...`.
- `N > total_elements` (known only after the pre-pass) -> runtime error
  `--drop-ids: cannot drop {N} elements, input has only {total}`.
- Missing indexdata without `--force` -> the existing `require_indexdata` error
  (drop rides the decode path, so this precondition already applies).

---

## 6. Composition with other flags

`--drop-ids` composes with every decode-path flag; there are **no new rejected
combinations** beyond the existing `--unsort` vs `--unsort-intra` exclusivity.

| Combination | Behavior |
|---|---|
| `--drop-ids` alone | Decode path; survivors emitted sorted (if input sorted), N elements dropped, `LocationsOnWays` lost as on every decode-path command. |
| `--drop-ids --strip-locations` | Ways re-encoded without inline coords; N dropped; header `LocationsOnWays` cleared. |
| `--drop-ids --strip-indexdata` / `--strip-tagdata` | Ride along as `indexdata=None` / `tagdata=None` on the framing call (same as today's decode path); N dropped. |
| `--drop-ids --unsort` | Output unsorted (cross-blob overlap) *and* N dropped. `check_refs` remains correct (kind-separated set membership; intra-kind order irrelevant). |
| `--drop-ids --unsort-intra` | Output has intra-blob inversion *and* N dropped. |

The drop filter runs strictly *before* the unsort swap sees elements, so the two
perturbations are independent: the drop removes exactly N, then the swap operates
purely on survivors. But the swap is **conditional on survivor count**, and that
condition is what the composition table promises. `UnsortKindState` fires only
when a kind's survivor count exceeds `hold_at` (`hold_at = block_cap` for
`--unsort`, `hold_at = 1` for `--unsort-intra`); a kind with `<= hold_at`
survivors holds an element that is re-injected unswapped at end-of-phase and no
overlap/inversion appears. Two corollaries the tests must respect:

- With `N == total` the output is header-only and no swap can fire.
- A `--drop-ids --unsort` (or `--unsort-intra`) test must size `N` and the fixture
  so that **every eligible kind still has more than `hold_at` survivors**, and then
  assert the actual cross-blob-overlap / intra-blob-inversion **shape** (via
  `count_adjacent_overlaps` / `count_intra_blob_inversions`, Section 8.2 #9), not
  merely that the header sort flag was cleared.

No combination is contradictory, so none is rejected.

`--drop-ids` with **only** passthrough-eligible strips (`--strip-indexdata` /
`--strip-tagdata`) still forces the decode path (Section 2.2); this is not an
error, just the required path.

---

## 7. Correctness / testable contracts

### 7.1 Exact count

`output_element_count == input_element_count - N`. Directly testable by counting
elements in input and output.

### 7.2 Determinism

Same input + same `N:SEED` => byte-identical output (deterministic selection ×
deterministic ordered re-encode). Different `SEED` (same `N`) selects a **different
dropped set** and hence a different output: `drop_hash` is a deterministic function
whose output depends on all 64 seed bits (Section 4.2), so on a fixed input the
outcome for any two seeds is decidable, not probabilistic. The tests treat this as
a decidable fact - they compare the two `D` sets directly (Section 8.1 #4) rather
than asserting a probability.

### 7.3 Dangling references are a pure function of the output and the input

`check --refs` reports a reference as missing iff its target `(kind, id)` is
**absent from the output**, regardless of *why* it is absent. The general,
hash-independent definition is therefore:

> A reference dangles iff it is **referenced by a surviving source AND its target
> is absent from the output.**

For a **referentially complete** input (every referenced target exists in the
input - true of every degrade fixture, and asserted as a test precondition), the
only targets absent from the output are exactly the dropped ones, so the counts
reduce to, matching `check_refs`' unique-count semantics:

- `missing_node_refs` = `|{ r absent from output : some surviving way references r }|`.
- `missing_node_members` = `|{ r absent from output : some surviving relation has node member r }|`.
- `missing_way_refs` = `|{ r absent from output : some surviving relation has way member r }|`.
- `missing_relation_members` = `|{ r absent from output : some surviving relation has relation member r }|`.

A test computes these expectations **without the hash** directly from the output:
take the surviving sources and their refs/members from `read_normalized(output)`,
take the set of present target ids per kind from `read_normalized(output)`, and
count refs whose target is not present. (Equivalently, on a referentially complete
input, derive `D_kind = input_ids(kind) - output_ids(kind)` from `read_normalized`
and intersect with the surviving-source reference structure.) It asserts equality
against `check --refs --check-relations --json`. This validates the consumer
contract, not the exact hash constants, while still pinning that the drop count is
exactly `N`. Building the expectation from *output presence* (not from `D`) makes
the test correct even if the input were not referentially complete.

---

## 8. Tests (per `reference/testing.md`)

CLI tests live in `tests/cli_degrade.rs` (existing file), using `CliInvoker` and
the stable allowlist reader/writer helpers - no imports from
`pbfhogg::commands::degrade`. Follow the file's existing conventions
(`write_degrade_fixture`, `read_normalized`, `read_header`, `assert_sorted_file`,
`assert_non_indexed`). The hash/selection unit tests (Section 8.3) instead live
**inline in the degrade module** because they exercise private functions.

**Real helpers (do not invent).** `read_normalized(path) -> NormalizedPbf` exists
and is imported in this file. `NormalizedPbf { nodes, ways, relations }` carries
everything the dangling test needs: `NormalizedWay.refs: Vec<i64>` and
`NormalizedRelation.members: Vec<NormalizedMember { member_type, ref_id, role }>`.
So per-kind IDs *and* the reference structure both come from `read_normalized` -
no nonexistent normalized/refs helper is needed, and (equivalently)
`write_degrade_fixture`'s returned `(Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>)`
is the same ground truth. `blob_elements(path)` (already in the file) gives per-blob
ordered ids for the unsort-shape assertions.

**Fixture reference structure** (from `write_degrade_fixture` = 60 nodes ids 1..=60,
12 ways ids 1..=12, 6 relations ids 1..=6, 20 elems/blob): every way references
nodes `{1, 2, 3}`; every relation has way members `{1, 2}`; there are **no** node
members and **no** relation members. So on this fixture only `missing_node_refs`
(dropping any of nodes 1/2/3 while a way survives) and `missing_way_refs` (dropping
way 1 or 2 while a relation survives) can be non-zero; `missing_node_members` and
`missing_relation_members` are structurally always 0. The input is referentially
complete (all refs point at existing elements), satisfying Section 7.3.

### 8.1 Tier 1 (root of `tests/cli_degrade.rs`, runs in `brokkr check`)

Small, fast fixtures (`write_degrade_fixture`, above). Use a small `N` (e.g.
`N = 10`) so drops land within the fixture, and a fixed seed.

1. **`degrade_drop_ids_removes_exactly_n`**
   Run `degrade --drop-ids 10:1`. Assert
   `output_total_elements == input_total_elements - 10` (sum nodes+ways+relations
   from `read_normalized`). Assert output header still declares
   `Sort.Type_then_ID` (`read_header(&output).is_sorted()`), and
   `assert_sorted_file(&output)` (dropping preserves monotone order).

2. **`degrade_drop_ids_dangling_refs_match_check_refs`**
   Run `degrade --drop-ids 10:16` (pinned, verified below). Read
   `read_normalized(input)` and `read_normalized(output)`. Compute the four
   expected unique missing counts hash-independently from the **output** per
   Section 7.3: for each surviving way's ref (from `output.ways`), the ref is
   missing iff its target id is not among `output.nodes` ids; dedup to a unique
   count -> expected `missing_node_refs`. Likewise surviving relations' way
   members vs `output.ways` ids -> `missing_way_refs`; node members ->
   `missing_node_members`; relation members -> `missing_relation_members`. Run
   `check --refs --check-relations --json <output>` via `CliInvoker::run()` (NOT
   `assert_success` - integrity FAILED exits 1), parse the `refs` object, and
   assert each of the four `missing_*` fields equals its computed expectation.
   **`total_missing` is NOT a JSON field** (only a Rust method), so assert
   "dangles were produced" by summing the four `missing_*` fields and asserting
   the sum `> 0`.

   **Pinned `N:SEED` = `10:16`** is verified to drop referenced elements on this
   fixture: with the Section 4.2 hash, the 10 smallest keys are nodes
   `{2, 3, 8, 16, 20, 24, 28, 36, 40}` and way `{2}`. Nodes 2 and 3 are referenced
   by every surviving way (`missing_node_refs = 2`) and way 2 is a member of every
   surviving relation (`missing_way_refs = 1`); `missing_node_members` and
   `missing_relation_members` stay 0; the four-field sum is 3. The test still
   computes expectations from the output rather than hardcoding these numbers, so
   it survives a fixture tweak - but it should `assert!` that the four-field sum is
   `> 0` to guard against a silently vacuous run.

3. **`degrade_drop_ids_reproducible_same_seed`**
   Run `degrade --drop-ids 10:7` twice to two outputs. Assert the two output
   files are **byte-identical** (read both with `std::fs::read`, compare).

4. **`degrade_drop_ids_different_seed_differs`**
   Run `--drop-ids 10:7` and `--drop-ids 10:8`. Assert the dropped ID sets
   differ: compare `D` derived by set-differencing input vs each output; assert
   the two `D`s are not equal. (Both still drop exactly 10.)

5. **`degrade_drop_ids_rejects_zero`**
   `degrade --drop-ids 0:1` -> `assert_failure().assert_stderr_contains("N must be >= 1")`.

6. **`degrade_drop_ids_rejects_missing_seed`**
   `degrade --drop-ids 10` -> `assert_failure().assert_stderr_contains("N:SEED")`.

7. **`degrade_drop_ids_rejects_n_over_total`**
   `degrade --drop-ids 1000000:1` on the small fixture ->
   `assert_failure().assert_stderr_contains("input has only")`.

### 8.2 Tier 2 (`mod tier2` in `tests/cli_degrade.rs`)

8. **`degrade_drop_ids_and_strip_locations_compose`**
   `--drop-ids 10:16 --strip-locations` on `write_degrade_fixture`. Assert count
   == input - 10 (`read_normalized`), `LocationsOnWays` cleared
   (`!read_header(&output).has_locations_on_ways()`), still sorted
   (`read_header(&output).is_sorted()`).

9. **`degrade_drop_ids_and_unsort_compose`**
   `--drop-ids 10:16 --unsort --block-cap 10` on `write_unsort_fixture` (60 nodes /
   24 ways / 24 rels). Dropping 10 globally leaves every kind with well over
   `block_cap = 10` survivors, so the `--unsort` swap fires for each kind (the
   survivor > `hold_at` condition of Section 6). Assert: count == input - 10;
   header not sorted; and the **actual cross-blob overlap shape** - exactly one
   `count_adjacent_overlaps` per kind and zero `count_intra_blob_inversions` per
   kind (the same shape `assert_unsort_cross_blob_shape` checks, minus its
   multiset-preservation assertion, which no longer holds once 10 elements are
   dropped). Then run `check --refs --check-relations --json` and assert the four
   `missing_*` fields match the Section 7.3 expectation computed from the output -
   proving drop composes with unsort and the consumer contract still holds on an
   unsorted-but-kind-separated file. Do not assert only that the header flag was
   cleared.

10. **`degrade_drop_ids_and_strip_indexdata_compose`**
    `--drop-ids 10:16 --strip-indexdata` on `write_degrade_fixture`. Assert count
    == input - 10 and `assert_non_indexed(&output)`.

### 8.3 Mandatory selection/hash unit tests (inline in the degrade module)

The end-to-end tests (8.1 #1-#4) would pass for many wrong algorithms: any
deterministic drop of `N` elements gives the same count, reproducibility, and
different-seed behavior. The exact hash and the exact N-smallest selection must be
pinned directly. Add these as **required** inline tests in
`src/commands/degrade/mod.rs` (`#[cfg(test)] mod tests`), where the private
`mix64`, `drop_hash`, `DropKey`, and the top-K helper are visible. They run under
`brokkr check` with the library build.

1. **`drop_hash_golden_vectors`** - assert every vector in Section 4.2 verbatim
   (both `mix64` and `drop_hash`, including the `seed=0x1` and `seed=0x100000000`
   cases that prove a low bit and bit 32 both reach the output - the 64-bit-seed
   guard). Any constant or seed-incorporation change breaks this.

2. **`drop_selection_matches_full_sort`** - build a synthetic set of `DropKey`s,
   select the `N` smallest via the size-`N` max-heap top-K helper, and assert it
   equals sorting all keys ascending and truncating to `N`. Cover `N < len`,
   `N == len`, `N > len`.

3. **`drop_selection_permutation_invariant`** - the same key multiset in several
   shuffled orders selects the identical `N`-smallest set.

4. **`drop_selection_partition_invariant`** - split the keys into several chunks
   (simulating per-blob results across workers), reduce each chunk to its own
   `min(N, chunk_len)` smallest, merge the chunk results through one global
   size-`N` heap, and assert the result equals the single-pass top-K. This pins
   the Section 4.3 worker/merge decomposition as order- and partition-independent.

5. **`drop_key_orders_by_hash_then_kind_then_id`** - construct `DropKey`s with an
   **identical `hash`** but differing `(kind, id)` and assert the derived `Ord`
   breaks the tie by `kind` then `id`, and that top-K selection over them is
   deterministic regardless of insertion order. This is the cross-kind
   hash-collision tiebreak (no need to search for a real hash collision; the
   contract is that `(kind, id)` resolves any collision, which this asserts
   directly).

---

## 9. Gates (exact commands)

This feature and every test run against the built `pbfhogg` binary directly (the
`tests/cli_degrade.rs` `CliInvoker` harness shells out to it). **No brokkr change
is required or permitted** - brokkr's `degrade` schema has no `--drop-ids` field
and is not extended here; there is no brokkr companion brick. `brokkr check` /
`brokkr check --profile full` are used only as the sanctioned clippy+test
*runners* (they invoke `pbfhogg degrade` through the compiled test binary, not
through any brokkr `degrade` subcommand), so they need no brokkr modification.

- **Correctness + CLI surface + wiring + tier-1 tests + clippy**: run
  `brokkr check`
  Green is the bar. It builds the library (running the Section 8.3 inline
  selection/hash unit tests) and the tier-1 CLI tests (Section 8.1). Clippy
  contract: `Cargo.toml` denies several lints - guard every `u64 as i64` counter
  cast with `#[allow(clippy::cast_possible_wrap)]` as the existing degrade counters
  do, and keep new functions under the `too_many_lines` / `cognitive_complexity`
  limits by factoring the pre-pass and the `DropSets` partition into helpers.

- **Tier-2 composition tests** (Section 8.2 lives in `mod tier2`, which
  `brokkr check`'s default tier-1 profile skips): run
  `brokkr check --profile full`
  This existing profile runs the `tier2` module; it is the runnable gate for the
  composition tests and needs no brokkr change. (These fixtures are tiny and run
  well under the fixed 20 s per-test watchdog.)

- **Cross-command consumer contract, ad hoc** (optional manual check that
  `check --refs` sees dangles on a larger real file): using the **built binary
  only** - no brokkr, no snapshot machinery - on any local indexed PBF:
  `pbfhogg degrade <in.osm.pbf> -o <out.osm.pbf> --drop-ids 100000:1`
  then
  `pbfhogg check <out.osm.pbf> --refs --check-relations --json`
  and confirm the four `missing_*` counts are non-zero and identical across a
  second degrade run with the same seed. This is exactly what the tier-1/tier-2
  tests already assert on fixtures; the manual run only scales it up. Because it
  is the built binary, note the CLI subcommand `pbfhogg check` is distinct from the
  `brokkr check` runner above despite the name collision.

- **Performance: NOT gated.** `--drop-ids` is a one-time adversarial-input
  generator. It adds a second full decode pass (the selection pre-pass) on top of
  the emit decode, so a `--drop-ids` run costs roughly 2x the decode of the same
  degrade without it. This is **accepted, not measured** - there is no benchmark
  gate, no baseline in `reference/performance.md`, and no keep/revert threshold for
  this feature. The extra cost is confined to runs that pass `--drop-ids` (the
  pre-pass is gated behind `flags.drop_ids.is_some()`); the existing `--unsort` /
  `--strip-locations` / passthrough paths are byte-for-byte unchanged and off this
  code, and their neutrality is confirmed by the unchanged `brokkr check` degrade
  tests, not by a throughput number.

---

## 10. Stopping rule

In scope: the `--drop-ids N:SEED` flag, its parsing/validation, the
selection pre-pass, the drop filter in the decode emit path, stats/counters/
markers, and the tests above. Out of scope and explicitly not built here:
per-kind drop quotas, rate/probability dropping, dropping on the passthrough
path (structurally impossible), post-drop renumbering/repair, and the deferred
`degrade` items v2.2/v2.3/v2.5/v2.6/v2.7 in `notes/degrade.md`. The teardown is
additive: no existing degrade transformation changes behavior, and the
passthrough path is untouched.

---

## 11. Review reconciliation (folded and rejected)

Consolidates the two reviews (R1 = opus, R2 = codex), each validated against the
code before folding.

**Folded (all severities):**

- *Classify-primitive fork* (R1 Major 1 / R2 Critical): resolved to
  `parallel_classify_phase` with one global size-`N` heap on the consumer, worker
  state `S = ()`, per-blob `R = BlockDrop { matched, smallest }`. Peak memory made
  honest and thread-independent (Section 4.3): `O(min(N, total))` global heap plus
  blob-bounded in-flight/per-worker terms, worst case `O(elements)` at `N == total`
  (stated, not hidden). The `accumulate` alternative was rejected as `O(threads*N)`.
- *32-bit seed collapse* (R2 High): `drop_hash` now XORs the full 64-bit seed once
  into the finalizer input; the `rotate_left(32) ^ seed` fold is gone. New golden
  vectors pinned (Section 4.2), including `seed=0x1` and `seed=0x100000000` as the
  64-bit-retention proof.
- *Duplicate-`(kind,id)` exact-N break* (R2 High): chose require-and-document-unique
  `(kind,id)` (Section 4.1). Removed the false "`(kind,id)` breaks a duplicate tie"
  claim; documented that duplicates would drop all occurrences of a selected key, so
  the exact-count contract holds only for unique-`(kind,id)` input (every valid
  PBF/fixture), which degrade does not enforce (no `O(elements)` seen-set).
- *`--force` triple-count* (R1 Major 3 / R2 High): each selection scan's classify
  must match only its phase's `Element` variants; stated as mandatory (Section 5.2),
  with the reason (unindexed blobs replicated into all three schedules) and why it
  keeps `total`/`D` correct on both indexed and `--force` inputs.
- *Nonexistent-helper remedy* (R1 Major 2, partially): validated that
  `read_normalized` **does** exist and exposes `NormalizedWay.refs` and
  `NormalizedRelation.members`, so the tests derive per-kind IDs and the reference
  structure from it (Section 8 intro, test #2); no invented helper. The valid
  sub-points were folded: `total_missing` is not a JSON field, so tests sum the four
  `missing_*` fields (R1 Minor 5); a known-good `N:SEED = 10:16` is pinned and
  verified to drop referenced elements on the fixture (R1 Minor 4).
- *Dangling-count definition* (R2 Medium): generalized to "referenced by a surviving
  source AND absent from output", tested hash-independently from output presence
  (Section 7.3).
- *Mandatory selection/hash tests* (R2 High): golden vectors, N-smallest-vs-full-sort,
  permutation invariance, partition invariance, and cross-kind collision tiebreak are
  now required inline unit tests (Section 8.3), not optional.
- *Unsort survivor>hold_at conditional* (R2 Medium): stated in Sections 6 and 5.2;
  test #9 must size `N` so every eligible kind stays above `hold_at` and assert the
  real overlap/inversion shape, not just a cleared header flag.
- *Parse error ordering* (R2 Low): bare `0` yields the missing-separator error (colon
  check runs first), not `N must be >= 1`; corrected in Section 5.3.
- *Counter `i64` cast* (R1 Nit 7 / R2 Low): documented as sidecar-only, values `>=
  2^63` wrap, accepted (Section 5.2).
- *Selection before writer open* (R2 Low): `degrade_decode_path` now completes the
  pre-pass and the `N > total` check before `writer_from_header_bytes`, so a
  selection failure leaves no partial output (Section 5.2).
- *"Overwhelming probability" imprecision* (R1 Nit 8): reworded to decidable
  language (Section 7.2, test #4).
- *`DropSpec::parse` error type* (R1 Nit 9): stated as the module's `super::Result`
  = `crate::BoxResult` alias (Section 5.1).
- *Tier-2 gate* (R2 Medium): `brokkr check --profile full` named as the runnable
  gate for `mod tier2` (Section 9), no brokkr change.

**Rejected / adjusted:**

- *R1 Major 2 as literally stated* ("`tests/cli_degrade.rs` has no `read_normalized`"):
  rejected - `read_normalized` is imported and used throughout that file and returns
  refs/members. The remedy of routing exclusively through `blob_elements` + fixture
  ground truth is unnecessary; `read_normalized` is the cleaner, sufficient source
  (fixture ground truth remains an equivalent cross-check). Only the finding's valid
  kernel (sum four fields, pin a seed) was folded.
- *R2 High brokkr companion brick* ("add a brokkr `degrade --drop-ids` schema/forwarding
  brick, or use supported commands"): the companion-brick option is rejected - this
  feature and all its tests run against the built `pbfhogg` binary via `CliInvoker`
  and never touch brokkr's `degrade` schema. Folded the valid half: the
  `brokkr degrade --dataset denmark --drop-ids ...` gates (which could not run) are
  removed and replaced with exact built-binary commands (Section 9). No brokkr
  dependency is introduced.
- *R2 Medium/perf "measurable bound + baseline + threshold"*: rejected for this
  feature - a one-time adversarial generator is explicitly not performance-gated
  (Section 9). The `~2x` decode cost is stated as an accepted characterization, not a
  benchmarked contract.
