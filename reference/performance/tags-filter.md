# Tags Filter

### OSC hidden node may omit coordinates (bygg, 2026-11-23, `c37ad061` against `0b353eba`)

`c37ad061` adds, per node, a hidden test on the decoded metadata and
routes the location through one `node_coords` helper. `brokkr bench
--iterations 1 tag-select --dataset planet --input-kind osc --input 4920`
(default filter), both arms `--commit` worktrees, order A B B A:
`0b353eba` 1703.6 ms (`b637319d`), `c37ad061` 1696.0 ms (`95b7a1d3`),
`c37ad061` 1770.0 ms (`0919671b`), `0b353eba` 1709.7 ms (`b476849f`).
Medians 1707 against 1733 ms (+1.5 %), pair signs mixed (-0.4 %,
+3.5 %), both arms at 3.1 avg cores in `TAGSFILTEROSC`. Within noise.
The merge-changes cells of the same commit are in merge-changes.md.

### OSC child attribute lists checked to the end (plantasjen, 2026-11-23, `68629f07` + change)

`tag-select --input-kind osc --input 4913` (planet, the bench's default filter),
`--iterations 1`, `--commit 68629f07` baseline interleaved. A generic
one-pass reader (every attribute slotted into an array, then interpreted)
on all three child kinds cost about 4 %, sign-consistent over four pairs:
baseline 2343, 2349, 2383, 2346 ms (`df80555e`, `e8ccb51c`, `9e650f7e`,
`a83f378b`), variant 2473, 2438, 2454, 2459 ms (`e5ad6599`, `50459dfb`,
`aebe0608`, `96b4ef43`). Clean HEAD built in the main target, 2412 and
2359 ms (`4e6c46a7`, `a192c383`), ruled out the binary's build location.
`nd`, the most numerous child, then got its own loop (read `ref`, then
drain the rest of the checked iterator): variant 2359, 2324 ms
(`e0815056`, `48df5aa0`) against baseline 2296 ms (`46037260`), in the
noise. That is the landed form.

### OSC tags-filter streams in input order (bygg, 2026-11-19, against `50a00440`)

The change: OSC mode runs on merge-changes' ordered streaming drain
instead of collecting nine (action, kind) groups and writing them through
one serial gzip encoder. Kept changes stay in input order, each batch is
its own gzip member deflated on the workers, and nothing of the output is
retained.

Planet OSC 4913, `tag-select --input-kind osc --input 4913` (default
`w/highway=primary`), `--iterations 1`, after `fstrim`. Baseline built with
`--commit HEAD` at `50a00440`, variant measured uncommitted. Order A B A B A B.

| arm | UUID | wall (ms) |
|---|---|---|
| baseline | `79f32ae0` | 1723.7 |
| streamed | `1f27e1f3` | 1599.0 |
| baseline | `34d508f8` | 1836.2 |
| streamed | `25f49e08` | 1722.7 |
| baseline | `4f28c8df` | 1730.5 |
| streamed | `5219f691` | 1607.8 |

Streamed median 1607.8 ms against 1730.5 ms, -7 %, faster in every pair.
Peak RSS in the `TAGSFILTEROSC` span (sidecar compare `4f28c8df` against
`5219f691`): 161460 kB to 21304 kB.

### Signed reference inclusion (bygg, 2026-11-18, against `d4e78733`)

The change: the two-pass path inserts every matched ID, way ref and
relation member into its signed `IdSet` with no sign guard, so negative
IDs are selected exactly. On positive input the negative bitmaps are never
allocated.

Germany (`germany-20260224-seq4704-with-indexdata`), `tag-select --filter
r/type=route`, `--iterations 1`, baseline built with `--commit d4e78733`,
variant measured uncommitted. Order A B B A A; the first A ran on a cold
page cache (4935 ms, `57c5de01`) and is excluded.

| arm | UUID | wall (ms) |
|---|---|---|
| baseline | `b20f31ed` | 3692 |
| baseline | `04e98232` | 3654 |
| signed | `6f8b9ac3` | 3715 |
| signed | `dbff80db` | 3715 |

Signed median 3715 ms against 3673 ms, +1.1 %: neutral. Both arms write
the same 27,326,791 elements with identical counters.

### tags-filter checks stored tagdata is canonical (bygg, 2026-11-03, against `ebe69342`)

The change: `TagIndex::deserialize`, which runs once per blob header on a
tag-filtered read, refuses a key not greater than the one before it (one
slice compare per key in the loop that already copies each key), a key
count the bytes cannot hold, and bytes after the last key.

Planet (`planet-20260223-with-indexdata`), the default `w/highway=primary`
expression, which skips most blobs on their tagdata, `--bench 1`, both arms
built in the main tree, no trim of its own. Run order variant, HEAD.

| arm | wall (s) |
|---|---|
| `ebe69342` (`53d5f705`) | 77.7 |
| variant (`--force`, sidecar `24f24eb9`) | 77.2 |

Neutral: the variant is inside a single cell's noise.

## Tags-filter

Two-pass architecture: pass 1 classifies blobs in parallel (parallel
classification + lightweight scanner), closure + way dep scans also
parallelized via `parallel_classify_phase`, pass 2 writes matching
elements.

### Planet axes (commit `16e3694`, plantasjen, `--bench 1`, `w/highway=primary`)

| Axis | Wall | UUID | Notes |
|---|---:|---|---|
| default (transitive two-pass) | **108.2 s** | `7e4301f9` | reproducible against the post-`01c67da` 119.9 s row |
| `-R` (single-pass, keep referenced) | 51.8 s | `cf116a6b` | flat vs `f262f068` baseline |
| `--invert-match` | 461.2 s (7m41s) | `6665605a` | first stored measurement; ~1 % of ways match `highway=primary`, so invert touches ~99 % of ways |
| `--remove-tags` (`-t`) | 111.8 s | `44d96d0a` | first stored measurement; two-pass with tag-stripping in pass 2 |
| `--input-kind osc` (osc 4913, 1-OSC) | 6.2 s | `37f360d2` | OSC parse + filter only; no PBF read |

`--invert-match` is the headline outlier: 4.3× the match path. The
asymmetry is workload-shape: keeping primary highways drops ~99 % of
ways (and most of their referenced nodes) so the writer's pass-2
output is small; inverting keeps ~99 % and the writer becomes the
ceiling.

### Planet `-j N` sweep (default two-pass, `w/highway=primary`, commit `16e3694`)

`-j` only affects the two-pass parallel path; the single-pass `-R`
path ignores it (CLI rejects the combination).

| `-j` | Wall | UUID |
|---:|---:|---|
| 4 | 184.0 s | `46d83578` |
| 8 | 123.3 s | `2a8fe06e` |
| 16 | 112.2 s | `b1d0c53d` |
| 24 | 111.1 s | `cffa644a` |

Saturates at `j16`; `j24` is within single-shot noise. Default
auto-detect on this 24-thread host lands the same workload at
108.2 s (`7e4301f9`), inside the noise band.

### Direct and member ways kept apart (bygg, 2026-11-08, against `0973c352`)

The included-ways set held every direct match a second time; it now holds
only the ways the relation closure pulled in that are not direct matches,
and the way-dependency walk, which only those ways need, is skipped when
there are none (every expression without a matched relation). Planet
(`planet-20260223-reindexed`), `w/highway=primary`, `--bench 1` main-tree
cells in the order A B B A; baseline = `0973c352` stored, variant =
uncommitted, `--force` (dirty sidecar rows):

| Arm | Wall | Peak anon | UUID |
|-----|-----:|----------:|------|
| baseline | 69.05 s | 2.49 GB | `f60408df` |
| variant | 55.93 s | 2.27 GB | `0b19b3c4` |
| variant | 56.67 s | 2.31 GB | `6e9a4f10` |
| baseline | 71.37 s | 2.44 GB | `097ae97a` |

The wall gain is the removed walk (its schedule scan plus about 9.6 s of way
blobs read for nothing); the peak gain is the duplicate way set. Relation
expressions still walk the way blobs, and their peak moves by the size of
their direct-way set only.

### Single pass across the fused engine's transform seam (bygg, 2026-11-09, `2529f73f` vs `c0f67d9e`)

`c0f67d9e` gave the fused engine a transform trait so digest could see each
blob before its decode: the decode-worker closure now hands the transform a
decode closure, and the per-thread string-table and group scratch borrows
end before the transform runs rather than around it. `tags-filter -R` goes
through that engine at both commits (block-only adapter, indexdata parsing
unchanged). Planet (`planet-20260223-reindexed`), `-R w/highway=primary`,
`--bench 1`, both arms as `--commit` worktrees, order A B B A:

| Arm | Wall | UUID |
|-----|-----:|------|
| `2529f73f` | 31.58 s | `4c2ea16c` |
| `c0f67d9e` | 31.54 s | `ff665e30` |
| `c0f67d9e` | 31.52 s | `ab6ceb19` |
| `2529f73f` | 31.60 s | `c44ad99a` |

Medians 31.59 s against 31.53 s (-0.2 %), both pairs the same sign; the one
scan phase runs about 11 cores in both arms. The filter skips 33,287 data
blobs on tagdata, so this screens the seam on the blobs the expression
decodes, not on every fused workload. A regression screen, not a bound;
no re-measure without a change to the decode-worker closure.

### One header walk for every two-pass schedule (bygg, 2026-11-10, `e802ffd4` against `61c19154`)

Pass 1's walk now records every data blob with its kind and the
closure, way-dependency and pass-2 schedules are cut from it. Before,
each later walk was cold on planet: pass 1 reads 28 GB of payload in
between, so each re-read about 500 MB of headers at near zero cores,
4.6 to 4.9 s apiece (sidecars `f60408df`, `0b19b3c4`).

After a trim, `--iterations 1`, both arms `--commit`, A B B A, planet:

| Filter | A, `61c19154` | B, `e802ffd4` |
|---|---|---|
| `w/highway=primary` (pass-2 walk saved) | 55.4, 56.6 s (`7532c026`, `c98653d3`) | 50.0, 50.2 s (`2c63baf3`, `51cfd441`) |
| `r/type=route` (closure and pass-2 walks saved) | 62, 62 s (`04a68224`, `25c97213`) | 56.2, 56.2 s (`2b16eb38`, `2b84bbf0`) |

About 10 % faster on both filters, every cell. Kept.

### OSC tags-filter takes -j (bygg, 2026-11-02)

`tags_filter_osc` takes a `Parallelism`: `Fixed(n)` runs the batch workers
on a pool of `n` built for the call, `Auto` on the host executor as before,
so the default path is the same code. Planet 4913 `w/highway=primary`,
`--bench 1 --force` cells, both arms built in the main tree on `befcab8e`
(baseline: the two changed source files checked out from HEAD). Baseline
2.440, 2.429, 2.435, 2.418 s; variant 2.599, 2.514, 2.411, 2.419 s. The
two slow variant cells were each the first after a rebuild, as was the
baseline's 2.440 s; warm cells agree within 1 %. Variant peak anon 135 MB,
2.6 cores. Output at `-j 1`, `-j 8` and the default is byte-identical
(sha256). brokkr refuses `-j` on this shape, so no `-j` sweep was measured.

## History

### Tags-filter

#### Sequential → two-pass → parallel arc

Two-pass architecture: pass 1 classifies blobs in parallel (parallel
classification + lightweight scanner), closure + way dep scans also
parallelized via `parallel_classify_phase`, pass 2 writes matching
elements.

| Dataset | Sequential (old) | Two-pass (pass 1 only) | Two-pass (all parallel) |
|---------|-----------------|------------------------|------------------------|
| Europe (33.6 GB) | 363s | 158s | **107.5s** (-70%) |

Previously OOM with pipelined reader. Sequential fix (commit `2a8a649`)
brought it to 363s. Parallel classification for pass 1 brought it to
158s. Parallelizing closure + way dep scans brings the total to 107.5s.
Full journey: 366.7s → 107.5s (3.4x total improvement).

---

