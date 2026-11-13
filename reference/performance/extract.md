# Extract

## Extract

Plantasjen. Best of 3 runs (or single-sample where noted), indexed PBFs.

| Dataset | Size | simple | complete | smart | Commit |
|---------|------|--------|----------|-------|--------|
| Denmark | 487 MB | 2259 ms | 2399 ms | 2693 ms | `aacbe80` |
| Japan | 2.4 GB | **3.8s** | **3.7s** | **4.7s** | `cadc3e6` |
| Europe | 32.4 GB | **96.3s** | **164.9s** | **181.4s** | `cadc3e6` |
| Planet † | 87.7 GB | - | - | **279s** | `cadc3e6` |

† Planet smart extract: single-sample `--bench 1`, Europe bbox, UUID
`2d028196`. Peak anon RSS 11.17 GB on 32 GB host (27.9 GB avail at run
start, 16.7 GB headroom to the ~25 GB "ship as-is" threshold). Peak
anon is dominated by PASS3 write work (bbox-sized), not by PASS1
scanning the input file. The mechanism identified during the
2026-04-10/11 investigation was a cold-arena-page residency cascade
triggered by post-PASS1 header scans touching pages glibc had reserved
but not populated; fixed by plumbing the PASS1 schedule forward into
PASS2 and PASS3 via `Pass1Result::pass3_blob_schedule` and
`pread_write_pass_with_schedule`.

Denmark bbox `12.4,55.6,12.7,55.8`, Japan bbox `139.5,35.5,140.0,36.0`,
Europe and Planet bbox `-25.0,34.0,45.0,72.0` (full-continent). Europe's
is its configured dataset bbox; planet's dataset bbox in `brokkr.toml` is a
Berlin box chosen for `brokkr run --with pbfhogg pbfhogg-verify --` cost, so planet extract cells that
want the continent box pass `--bbox` explicitly.

Simple extract uses a 3-phase barrier pipeline with parallel classification
and raw frame passthrough. Each phase (nodes, ways, relations) classifies
blobs in parallel then writes matching raw frames via pread workers - no
decode+re-encode.

Complete-ways and smart pass 1 (`collect_pass1_generic`) uses three-phase
parallel pread classification (nodes → ways → relations) via a reusable
`parallel_classify_phase` helper. Smart pass 2 (way dependency resolution)
also uses `parallel_classify_phase`, replacing the old sequential BlobReader
scan. Workers pread + decompress + classify in parallel, sending compact
results back to the consumer. Write passes use pread-from-workers with full
PrimitiveBlock lifecycle per worker.

### Parent relations, integer bbox edges and plain nodes (bygg, 2026-11-03, against `4a3ba718`)

The change that brought extract's membership to osmium's: complete-ways and
smart close the relation set over parent relations (member-to-parent edges
collected in the relation classify phase, sorted, walked from the matched
set), every bbox membership test compares integers against one quantized box
(simple's node classify included, where the f64 product comparison ran per
node), and the sorted columnar classifier also walks plain Node messages.
Planet, Europe box `-25.0,34.0,45.0,72.0`, `--bench 1` main-tree cells,
interleaved, after `fstrim`; baseline = `4a3ba718` stored, variant = the same
tree with the change, `--force`:

| Strategy | baseline | variant | baseline peak | variant peak |
|----------|----------|---------|---------------|--------------|
| smart | 132.99 s (`d6c01405`), 132.56 s (`a49fcc12`) | 133.15 s (`dea99869`), 132.42 s (`39c9957b`) | 8.14 / 8.11 GB | 8.10 / 8.13 GB |
| complete | 118.90 s (`f7cb91a2`) | 119.05 s (`ccaa5096`) | 4.92 GB | 4.93 GB |
| simple | 108.37 s (`01fcae89`) | 108.15 s (`0a37fc61`) | - | - |

All within noise. The parent closure (`PASS1_RELATION_PARENTS`) is 209 ms
over 1 042 815 edges and adds 78 408 relations; the relation classify phase
that collects the edges stays at 0.94 s. Variant UUIDs are dirty sidecar
rows (`brokkr sidecar <uuid>`), not `results` rows.

### Polygon membership on osmium's banded integer test (bygg, 2026-11-03, against `c4ed2ad8`)

The polygon predicate moved from an f64 ray cast over every vertex per node
to osmium's integer test over the segments of the node's latitude band.
brokkr runs extract and multi-extract with bbox regions only, so these are
direct runs of the two release binaries, interleaved, wall clock, not stored
cells; bbox extracts do not reach the changed code. Boundaries exported by
`osmium export` from the same input:

| Input, region (vertices), strategy | `c4ed2ad8` | variant |
|------------------------------------|------------|---------|
| denmark-20260220, Aarhus Kommune (9694), simple, 4 pairs | 1.26 s median (1.26 to 1.28) | 0.35 s median (0.35 to 0.36) |
| germany-20260224, Berlin (6017), complete, 2 pairs | 5.70 s warm, 11.10 s cold first run | 3.28 s, 3.31 s |

Prepared segments and bands are built once per region; the per-node cost is
the band's segments instead of every vertex, so the gain grows with the
polygon's vertex count.

### ID sets that hold only what the writer does not already have (bygg, 2026-11-08, against `0973c352`)

complete and smart keep the node refs of matched ways only where they are
outside the region set (the writer includes the union and tests the region
set first, so a ref in both bought nothing); smart's relation-driven extras
skip matched ways and already-included nodes, and its pass-1 relation
classify sends member IDs per blob instead of accumulating an `IdSet` per
worker. Planet (`planet-20260223-reindexed`), Europe box
`-25,34,45,72`, `--bench 1` main-tree cells; baseline = `0973c352` stored,
variant = the uncommitted change, `--force` (dirty sidecar rows):

| Strategy | baseline wall | variant wall | baseline peak anon | variant peak anon |
|----------|---------------|--------------|--------------------|-------------------|
| complete | 116.98 s (`80089cbc`) | 116.14 s (`8c487309`) | 4.44 GB | 2.76 GB |
| smart | 129.21 s (`05bbc210`) | 128.73 s (`15c17254`) | 8.06 GB | 2.87 GB |
| simple, node set dropped before the way write | 106.81 s (`38954d07`) | 107.51 s (`6d5424f1`) | 2.57 GB | 2.60 GB |

complete's way classify fell from 16.1 s to 12.5 s (it sets far fewer IDs).
smart's 8 GB was the relation classify phase: 32 workers each promoting
dense chunks for dispersed member IDs; per-blob send removes it, and the
phase falls from 0.85 s to 0.36 s. The simple row is a refusal: moving the
relation classify ahead of the way write so the node set could be dropped
there freed live memory but moved no peak, since the freed chunks stay in
the arenas and the way write's worker-side buffers do not reuse them; it was
not landed (the reason sits at the site in `simple.rs`).

## History

### Extract

#### PASS1 schedule reuse (commits `d4ea760`, `0b085b1`, 2026-04-10/11)

The parallel_classify_regression investigation discovered that every
header scan running *after* PASS1's parallel allocator work was
redundant - `collect_pass1_generic` already scans the whole file once.
By plumbing `full_way_schedule` and `pass3_blob_schedule` out of
`collect_pass1_generic` via `Pass1Result` and consuming them via
`mem::take` in PASS2/PASS3, smart extract now does ONE file scan instead
of THREE. Europe impact at commit `cadc3e6` vs pre-investigation
`fc17b51`:

| Strategy | Pre-investigation | Post | Δ |
|---|---|---|---|
| smart | 208.2s (`fc17b51`) | **181.4s** | **−13%** (−29% vs mid-investigation `5ca2df9` peak of 254s) |
| complete | 198.0s (`fc17b51`) | **164.9s** | **−17%** |
| simple | 113.1s (`fc17b51`) | **96.3s** | **−15%** |

Complete benefits because `extract_complete_ways` PASS2 now also consumes
`pass3_blob_schedule` via `pread_write_pass_with_schedule`. Simple benefits
from shared instrumentation and scan-path improvements in the same commit
range.

#### Europe simple phase breakdown (commit `b95e5ab`)

- Node classify: 13s, Node write: 11s
- Way classify: 6s, Way write: 40s
- Rel classify: 13s, Rel write: 2s

#### Per-worker accumulation drops single-extract alloc (Japan)

Single-extract Japan alloc, `parallel_classify_phase` per-worker
accumulation (baseline `ec43a8b` -> final `201a4cf`, plantasjen): total
alloc 6.4 GB -> **2.0 GB (-69%)**; `parallel_classify_phase` fell from
5.0 GB (48.8% of churn) out of the top 10. Workers accumulate `Vec<i64>`
across all blobs and send once at scope exit - no per-blob allocation
through the channel.

#### Historical (sorted pass1 optimization, commit `37b7c19`)

Impact on simple: Denmark -14% (2625→2259ms), Japan -8%
(12,619→11,643ms). Single-pass classification on sorted input
eliminates the second file read. Superseded by the parallel classify +
raw frame passthrough architecture.

---

