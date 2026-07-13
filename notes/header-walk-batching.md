# Header-walk batching - one primitive, several call sites

Transient plan note. Consolidates a convergence that was living scattered
across the sort/getparents optimization notes and the TODO. Retire this
file when the primitive lands (its context moves into code comments then)
or when the lever is definitively rejected. The durable *evidence* behind
it - the encoder asymmetry, the shape taxonomy, the measured 8k walls -
already lives in `reference/blob-density.md`; this note is only the
unbuilt-fix writeup.

**Disposition up front: deferred, production-negligible, narrower payoff
than the call-site count suggests.** Do not build speculatively. See the
disposition section for the trigger conditions.

## The shared cost

Every one of the call sites below drives a **single-threaded, QD=1,
pread-per-blob header walk** - `HeaderWalker::next_header` in
`src/read/header_walker.rs`, either directly or via
`build_classify_schedules_split` in `src/scan/classify.rs`, which wraps
it. Cost is ~45-70 us per blob and **linear in blob count**, independent
of payload. That is ~3.5 s on the 50 k-blob production planet dump
(invisible) and a fixed ~100 s on the 1.45 M-blob `snapshot.8k`
re-encode (dominant). It is the same physical cost wherever the walk runs
serially; the commands differ only in how large a fraction of their wall
it is.

## Call sites (measured)

| call site | primitive | walk at 8k | payoff of flattening it |
|---|---|---:|---|
| `sort` pass 1 `build_blob_index` | `HeaderWalker` | (europe +21 % on the pass-1 conversion; planet walk 6.7 s of 115 s, writer-bound) | europe-scale unsorted input only |
| `getparents` walker arm | `HeaderWalker` | 64.8 s | lets the walker arm survive past the 150 k dispatch threshold |
| `getid` include walker arm | `HeaderWalker` | ~103 s (walk-dominated) | same - dispatch currently routes to the full scan instead |
| `check --refs` / `check --ids` | `build_classify_schedules_split` | ~101 s (**~2/3 of wall**) | **substantial** - roughly halves the 8k wall |
| `cat --clean` / `repack` / `degrade` / `extract --smart` | `build_classify_schedules_split` | ~97-109 s (**18-29 % of wall**) | marginal - re-encode/write of 1.45 M tiny blobs dominates, not the walk |
| `apply_changes::scanner`, `inspect/scan.rs` | `HeaderWalker` | unmeasured | beneficiaries if the primitive is generalized |

The two selective/verify read-only sites (`getparents`/`getid` walker
arms, `check --refs`/`check --ids`) are where the walk is most of the
wall and a fix is worth most. The four re-encoding sites regress on 8k
mostly from tiny-blob framing/write overhead, which this fix does not
touch. Per-command fractions and UUIDs: `reference/blob-density.md`
"Full shape-3 sweep".

## The proposed primitive

An **io_uring batched-header-probe walker** next to `HeaderWalker`:
submit K header preads in flight, harvest completions, resubmit to keep
the queue full. The call site stays single-threaded; `fadvise(RANDOM)`
is preserved; NVMe queue-depth concurrency is recovered at the primitive
level, collapsing the QD=1 latency term. One generalized primitive
serves every call site above. Independently arrived at as the sort pass-1
io_uring lever (`reference/performance-history.md` "Sort") and the
`notes/getparents.md` walk lever.

## Why it is not trivial, and the alternatives

The walk **cannot be split across threads directly**: the PBF stream has
no top-level blob index, so blob N+1's offset is only known after reading
blob N's header. The sequential dependency is the whole reason the walk
is QD=1. Batching (io_uring) hides the latency without breaking the
dependency; sharding does not work without a way to resync to a blob
boundary blind.

- **Buffered-sequential walk, dispatched on high blob count.** MEASURED
  and rejected for `sort` (the "M3" experiment;
  `reference/performance-history.md` "Sort" do-not-reattempt): europe
  -21 %, but planet **+11 %** because it reads the
  whole file and gives up the walker's IO reduction (20 s BufReader vs
  6.7 s walker on planet). Zero-sum across europe+planet; walker wins the
  tiebreaker because planet is production. A blob-count dispatch could
  pick it only for high-blob-count inputs, but that is a second-choice
  fallback to the io_uring primitive.
- **Worker-sharded seek-to-next-`BlobHeader` scan** (the sort
  sharded-walker mitigation): each worker seeks into its file region and resyncs to
  the next parseable blob header, walks serially, results reassembled in
  offset order. Needs a "sync to next BlobHeader" primitive that does not
  exist in the tree. Days of work; also recovers NVMe concurrency.

## Disposition

Deferred. Production planet is 50 k blobs (walk ~3.5 s); production
`sort` input is already sorted (walk a small fraction of a writer-bound
wall). The lever only pays on **high-blob-count** (Geofabrik-style
packing of a large corpus) or **unsorted-at-scale** input, neither of
which is a configured or real workload today. Even when it does apply the
payoff is narrowed: substantial for the read-only selective/verify
commands (`getid`, `getparents`, `check --refs`, `check --ids`),
marginal for the re-encoders. Build it the day one of those workloads
becomes real - then it is one primitive, not four separate fixes.

## Cross-references

- [`reference/blob-density.md`](../reference/blob-density.md) - the why
  (encoder asymmetry, shape taxonomy, full shape-3 sweep, dispatch
  matrices). Durable; outlives this note.
- [`decisions/0006-blob-count-threshold-dispatch.md`](../decisions/0006-blob-count-threshold-dispatch.md) -
  the shipped alternative for `getid`/`getparents` (dispatch, not batch).
- [`reference/performance-history.md`](../reference/performance-history.md)
  "Sort" - the sharded scan, the io_uring walker, and the rejected M3
  buffered walk with sort-specific numbers.
- [`notes/getparents.md`](getparents.md) - the walk lever in context.
- `src/read/header_walker.rs`,
  `src/scan/classify.rs::build_classify_schedules_split` - the code.
