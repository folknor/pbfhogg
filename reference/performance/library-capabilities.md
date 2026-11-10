# Library Capabilities

## Library capability accounting

### Call guards on per-call methods (bygg, 2026-10-29)

The invocation's capability accounting adds success-path work on per-call
public methods: a call guard (one compare-and-swap to admit, one
`fetch_sub` to retire, on a padded word) per `BlobReader::next` and per
`PbfWriter` write method, plus one census-mutex lock at mint and retire per
block a block API hands its caller. Gated by `brokkr write` (public
`BlobReader` iteration and decode, rebuilt into blocks written through
`PbfWriter::write_primitive_block`), denmark `--variant indexed --bench 1
--compressions none`, cells interleaved. Baseline `--commit 1f2919e4`
(stored); variant the capability tree on top of `1f2919e4`, measured
uncommitted with `--force` (forced cells are not stored, so they carry no
UUID).

| Writer | Baseline pair 1 | Variant pair 1 | Baseline pair 2 | Variant pair 2 | Medians |
|---|---|---|---|---|---|
| sync | 6702 ms (`95ba54f2`) | 6499 ms | 6670 ms (`40463eef`) | 6480 ms | 6686 vs 6490 ms |
| pipelined | 5829 ms (`f621aa90`) | 5846 ms | 5792 ms (`3292831a`) | 5713 ms | 5810 vs 5780 ms |

No cost: the sync arm is faster in both pairs, the pipelined arm within
noise with mixed signs. No brokkr command issues blocks in bulk (`brokkr
read --mode pipelined` runs `for_each_pipelined`, which lends elements and
issues nothing; `build-geocode-index` iterates relation blocks only, through
an internal route), so the census lock per issued block has no bulk
measurement; it is one uncontended mutex per block, beside a decode.

### Fallible admission (bygg, 2026-10-30)

Sealing makes admission fallible: the guard's compare-and-swap now also
requires the Open state (a mask and a branch on the word it already
loaded), and `BlobReader::next` checks its walk's failure latch before it
admits. The call sink's sealing path keeps no in-flight count: the word
orders every emission before the seal, so no brokkr CLI command gains
per-emission work. Same gate as above: `brokkr write`, denmark `--variant
indexed --bench 1 --compressions none`, cells interleaved. Baseline
`--commit 62e18608` (stored); variant the sealing tree on top of it,
measured uncommitted with `--force` (not stored, no UUID).

| Writer | Baseline pair 1 | Variant pair 1 | Baseline pair 2 | Variant pair 2 | Medians |
|---|---|---|---|---|---|
| sync | 6415 ms (`66a370df`) | 6468 ms | 6412 ms (`5294bccd`) | 6416 ms | 6414 vs 6442 ms |
| pipelined | 5724 ms (`e0935539`) | 5753 ms | 5737 ms (`3c12b82f`) | 5679 ms | 5731 vs 5716 ms |

No cost: both arms within half a percent, the pipelined arm with mixed
signs.

### Framing placement counter (bygg, 2026-10-30)

Each pipelined framing job now clones the writer's `Executor` (one `Arc`
increment and decrement) and, on success, checks whether it runs on a
worker of that executor (one thread-local read) and adds one to
`writer_payload_framed_on_executor_items`. The synchronous writer is
untouched, so its arm is the control. Same gate: `brokkr write`, denmark
`--variant indexed --bench 1 --compressions none`, cells interleaved.
Baseline `--commit e36fa38f` (stored); variant the counter tree on top of
it, measured uncommitted with `--force` (not stored, no UUID).

| Writer | Baseline pair 1 | Variant pair 1 | Baseline pair 2 | Variant pair 2 | Medians |
|---|---|---|---|---|---|
| sync | 6390 ms (`d8e3ff65`) | 6498 ms | 6403 ms (`828f53ee`) | 6486 ms | 6397 vs 6492 ms |
| pipelined | 5687 ms (`0f8f6011`) | 5749 ms | 5694 ms (`bdea0d40`) | 5808 ms | 5691 vs 5779 ms |

Below noise: the pipelined arm is 1.5 % slower in both pairs, and so is
the unchanged sync arm, so the shift is the build or the drive between
the worktree and the working tree, not the counter.

### Input eviction counter (bygg, 2026-10-30)

Every input eviction (`POSIX_FADV_DONTNEED` behind the read head, once per
blob under `PageCachePolicy::Evict`, which the CLI uses) now mints a nested
call on the invocation and adds one to `fadvise_input_evictions`, beside the
syscall it counts. Under `Keep` the branch is never armed. Gate: `brokkr
tags-filter -R`, denmark `--variant indexed --bench 1`, whose single-pass
path reads through the evicting `ElementReader` (the default two-pass path
reads by pread walkers and evicts at most its header reads, so it would not
exercise the counter). Baseline `--commit 8b91fae9` (stored); variant the
counter tree on top of it, measured uncommitted with `--force` (not stored,
no UUID). Cells interleaved.

| Baseline pair 1 | Variant pair 1 | Baseline pair 2 | Variant pair 2 | Medians |
|---|---|---|---|---|
| 200.8 ms (`15fbaa3b`) | 200.8 ms | 200.9 ms (`224a6659`) | 200.9 ms | 200.8 vs 200.8 ms |

No cost. The run is short, so this bounds the counter to well under the
2-3 % noise floor of a 200 ms wall, not to its absolute per-blob cost.

