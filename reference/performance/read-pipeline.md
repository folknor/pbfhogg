# Read Pipeline

### Decode buffers sized from the blob and recycled (bygg, 2026-10-30)

The pread decode workers asked the per-worker `DecompressPool` for
`4 x compressed size` (about 7.2 MB for a 1.8 MB planet blob), Vec
doubling on the inline offset tables grew other buffers past the pool's
4 MiB retention cap, and so every planet buffer was dropped on return
(drops = scheduled blobs in every cell): one multi-MB allocate-and-free
per blob per worker. mallinfo2 sampled mid-run in getparents at
`c97b9985` showed 175-250 MB in use against 280-420 MB free inside the
arenas, all resident (THP `always` on bygg). Now the decode helpers size
the buffer from the declared `raw_size` with exact growth (tables
included), callers pass no guess, and the pool retains buffers up to
16 MiB under a 256 MiB idle budget per pool: zero pool drops at planet.
The apply-changes workers recycle theirs through a per-worker pool too.

Once decode buffers stop being freed, nothing raises glibc's sliding mmap
threshold early, and time-filter's in-worker build and frame buffers went
through mmap/munmap per blob: main phase 139.9 s to 169.1 s, kernel time
1.8 to 5.7 cores. The CLI's allocator hook then pinned
`M_MMAP_THRESHOLD` at 32 MiB beside its two-arena cap, which recovered
it. Commands without the hook (getparents, extract, tags-filter,
apply-changes) showed no work-phase regression. The churn was costly only
under the arena cap: with glibc's default arenas time-filter runs the same
wall with or without the pin, and the CLI now sets neither (see "The CLI
no longer caps malloc arenas").

Planet (`planet-20260223-with-indexdata`), `--bench 1`, peak anon RSS.
Variant cells are unstored `--force` cells on `c97b9985` plus the change.
Wall compares the work phases (sidecar phase split): the variant cells ran
with the schedule header walk out of page cache (about 6 s per walk,
code unchanged), which the stored baselines had cached.

| command | baseline | peak before | peak after | work phases |
|---|---|---:|---:|---|
| `getparents` | `dd453b3f` | 705 MB | 288 MB | decode 13.41 s to 13.29 s |
| `check --ids` | `4edab6b2` | 646 MB | 566 MB | -1 % / -2 % |
| `time-filter` | `4f093a45` (`c97b9985`, same day) | 1,177 MB | 841 MB | 139.9 s to 139.0 s |
| `cat --clean version` | `a8a4384e` | 1,242 MB | 994 MB | nodes -4 %, ways -2 % |
| `apply-changes` (4913, `zstd:1`) | `f2b8beed` | 3,123 MB | 2,965 MB | streaming 120.0 s to 120.4 s |
| `extract --simple` (`-25,34,45,72`) | `6b3c88cc` | 3,522 MB | 3,094 MB | equal or faster per phase |
| `tags-filter w/highway=primary` | `b8491cfd` | 2,845 MB | 2,568 MB | equal per phase |
| `inspect --extended` | unstored 571 MB cell | 571 MB | 355 MB | 41.4 s to 42.2 s |

What is left in the larger peaks is not decode storage: apply-changes'
overlay (575 MB by `merge_diff_heap_bytes` as it was then, an
allocated-capacity estimate, not resident memory; its parts are counted
in "Apply-changes retained lookup storage") and
drain reorder (415 MB high water; since bounded by a
sequence window that does not bind on this input, see "Apply-changes
sequence window"); the
persistent ID sets of extract, tags-filter and check `--ids --full`. The
`pwrite` pool's queue is unquantified: no counter samples its occupancy,
and on the default splice route it queues rewritten output frames, which
average about 146 KB at `f2b8beed` (`627883d77`, bygg;
`writer_payload_raw_bytes` over `writer_payload_raw_items`), not the
1.8 MB input blobs.

### Worker-kept decode buffers follow the pool limit (bygg, 2026-11-03, against `d4c97d42`)

Every plain decode buffer a worker keeps across blobs outside a pool (the
raw scan behind `check --locations`, altw sparse and sorted
`extract --simple`; altw external stages 1, 2 and 4; altw passthrough's
way path; geocode pass 1.5; apply-changes' refresh-scan buffer) now takes
the decode pool's per-buffer limit once per blob through
`release_decode_outlier`, counted in
`pipeline_decompress_pool_oversize_drops`. The sequential take loops
(altw external stage 4's non-way blobs, the unsorted extract sweeps) keep
handing their buffer to the block unpooled and now reuse their parse
scratch.

On planet the limit never fires: zero oversize drops in both variant cells
below, as for the pooled paths. Variant cells are unstored `--force` cells
on `d4c97d42` plus the change, `--bench 1`.

| command | baseline | wall before | wall after | notes |
|---|---|---:|---:|---|
| `check --locations` (`altw-keep-untagged`) | `c17025ee` (`7feb4370`; same-commit cells 203.9-208.1 s) | 206.7 s | 201.8 s | every partition phase -1 % to -8 % |
| `add-locations-to-ways --index-type external` | `a12a2f7d` (`8ccb3a2e`; same-commit cells 317.0-353.5 s) | 317.0 s | 322.1 s | stage 4 127.1 s to 122.6 s, `s4_nonway_assemble_ms` 623,354 to 607,361; stage 1 32.4 s to 36.2 s |

The altw stage 1 delta is run history, not the change: the variant
followed a `check --locations` cell over a different 147 GB file, and the
untouched phases before and beside it moved with it (blob metadata scan
5.2 s to 7.9 s, relation scan 9.9 s to 11.6 s, disk read in the segment
after pass A starts 11,373 MB to 13,517 MB). Stage 1's code change is one capacity compare per blob.

### Worker read buffers follow a retention limit (bygg, 2026-11-04, against `22a5f24a`)

Every buffer that keeps a compressed blob payload or a whole frame across
blobs is now `crate::read::read_buf::ReadBuf`: exact growth (the old
`resize` doubled, so a buffer kept up to nearly twice its largest frame)
and a release past `READ_BUF_MAX_RETAINED_CAPACITY` once per blob, counted
in `pipeline_read_buf_releases`. The direct reads' aligned scratch follows
the same line. Two lines were measured on `check --locations` over
`planet-20260223-altw-keep-untagged` (way frames median 4.5 MB, max
9.6 MB), `--bench 1`, run order HEAD, HEAD, 4 MiB, 16 MiB, 4 MiB, HEAD
(HEAD cells stored, variant cells `--force`):

| arm | walls (s) | max phase anon (GB) | releases |
|---|---|---|---|
| `22a5f24a` (`d030ae1a`, `b91bc0b6`, `9cb69c5a`) | 197.4, 203.2, 204.2 | 17.65, 18.14, 17.96 | - |
| 4 MiB, the scratch limit (sidecars `eea69fb4`, `28ac76b5`) | 214.4, 222.9 | 17.98, 18.10 | 90,972 |
| 16 MiB, landed (sidecar `48fea30c`) | 204.4 | 17.99 | 0 |

The 4 MiB line freed and regrew a way-frame buffer on most way reads (452 GB
of released capacity over the six partitions); partitions 2 to 5 ran 2 to
4 % slower in both of its cells, partition 1 17 and 39 %, and the run peak
did not fall. The 16 MiB line runs phase for phase as HEAD and never fires
on planet. apply-changes keeps its own 4 MiB line on the base-read buffer
(see "apply-changes' worker scratch follows the release rule" below).

`add-locations-to-ways --index-type external` on the indexed planet
(largest frame 3.7 MB, zero releases), variant then HEAD: 325.5 s
(`7a756d41`) against 356.4 s (`c0b11c81`). Stage 2 159.2 against 158.9 s,
stage 4 123.8 against 121.7 s; the wall gap is the HEAD cell's stage 1 pass
A at 67.9 s against 33.9 s here and 32 to 36 s in earlier records, run
history rather than the change.

### Parallel-array iterator ends are checked in release (bygg, 2026-11-03, against `ebe69342`)

The change: the end arm of every lockstep iterator over an element's
parallel arrays (`TagIter`, `RawTagIter`, `WayNodeLocationsIter`,
`RelMemberIter`, `MemberIdIter`, `DenseNodeIter`) checks in release that
all its arrays ended together, through one inlined branch onto a cold,
never-inlined panic with a static message; it was a `debug_assert!`. The
same tree also carries the other invariant-check dispositions of that day
(all per block, blob, chunk or run).

Planet (`planet-20260223-with-indexdata`), the default `w/highway=primary`
expression, `--bench 1`. Its way passes skip no blob on tagdata
(`tagsfilter_single_pass_tagidx_skipped_blobs=0`), so every way's tags are
walked through `TagIter`, twice. The A arm is the stored main-tree cell of
the previous entry, whose `crates/lib/src/read/elements.rs` and `dense.rs` are
identical to the variant's base `9c4d6c81`; the variant is `9c4d6c81`
plus the change, built in the main tree. No trim of its own.

| arm | wall (s) |
|---|---|
| `ebe69342` (`53d5f705`) | 77.7 |
| variant (`--force`, sidecar `32433dfa`) | 74.1 |

No cost: the variant is faster, by an amount within the run-to-run spread
these cells show, so no gain is claimed either.

### Element parse and re-encode refusals checked in release (bygg, 2026-11-08, `5a9d40e4` vs `85c1428c`)

The pair brackets exactly three changes (`5a9d40e4` itself touches only
altw external): `e28fdf69` (`take_message` refuses a second data-bearing
`Info` or `DenseInfo` at element parse; the `info()` accessors panic on a
broken invariant instead of defaulting), `9c85d37e` (the block iterators
and the per-node dense `keys_vals` walk go through `validated`, an inlined
branch onto a cold panic, instead of skipping or reading less) and
`85c1428c` (the re-encoding paths in `crates/lib/src/owned.rs` return a `Result` per
element for the user lookup and per member for the role, through
`relation_members_into`).

Planet (`planet-20260223-reindexed`), both arms `--commit` worktree builds
(the same build path, so brokkr#105's worktree offset cancels), `--bench 1`,
arms alternated A B B A, kernel 7.2.14. `brokkr read` runs the element parse
and iterator checks but never touches `info()` or a re-encode; `repack`
re-encodes every element with its metadata (10.45G nodes, 1.17G ways,
14.1M relations through `relation_members_into`, 98.4 GB written), so it
runs every `85c1428c` site. A way-only `tags-filter` was rejected as the
re-encode cell: it keeps no relation and never reaches the role loop.

| cell | `5a9d40e4` (s) | `85c1428c` (s) | delta |
|---|---|---|---|
| read sequential, round 1 | 512.3 (`a5bbad97`) | 508.4 (`a3bdd79a`) | -0.8 % |
| read sequential, round 2 | 512.9 (`a139aae7`) | 508.3 (`314f89ac`) | -0.9 % |
| read parallel, round 1 | 28.6 (`6ce94499`) | 28.7 (`65019195`) | +0.2 % |
| read parallel, round 2 | 30.2 (`16bf1999`) | 30.2 (`b8df6cad`) | 0.0 % |
| repack, round 1 | 260.5 (`1000e419`) | 261.8 (`65d789b4`) | +0.5 % |
| repack, round 2 | 263.5 (`2d9e0431`) | 264.6 (`00e308e4`) | +0.4 % |

No measurable cost on any cell: every delta is under 1 %, inside the noise
floor, and both rounds agree. This is a screen for a regression, not a
proof of a bound: the pair can say the checks cost nothing visible at
planet, not that their cost is under any figure. The verdict is for the
pair; the checked functions are unchanged since, but the code around them
is not, so it is not re-claimed for later trees without a cell there.

### The partial-index guard, not measured by `brokkr read` (bygg, 2026-11-08, at `71c0a4d3`)

`84f50b31` armed every command-input frame walk against a partially
indexed file: an armed walk parses the 42-byte indexdata, deserializes it
and observes it once per completed data frame, including on readers whose
caller asked not to keep it. The proposed screen, `brokkr read --dataset
planet` at `84f50b31^` against `84f50b31`, was not run because it cannot
see the guard: `bench-read` opens every mode through the public library
constructors (`ElementReader::open_with` and `BlobReader::open_with` at
HEAD, `from_path_with_eviction` at `84f50b31`), which never arm the walk,
so both arms run with no guard. `par_map_reduce` has no armed caller.

On the armed command paths the added work is fixed per frame: a stack
copy and a scalar `BlobIndex::deserialize`, with no allocation, element
walk, payload read or seek. These figures are a source estimate, not a
measurement. At 100 ns a frame, a planet walk of 50,816 frames costs about
5 ms and a europe walk of 522,168 frames about 52 ms. At 1 us a frame the
same walks cost about 51 ms and 522 ms. Either way the cost sits far below
the noise floor of a minutes-long planet run. Inputs packed more densely
than europe, and short header-only walks, have a different denominator,
so this is not a claim of insignificance everywhere.

### HeaderWalker / next_header_skip_blob regression check (commit `436998b`, re-measured 2026-04-26 at `16e3694`)

The `436998b` (2026-04-26) read-path truncation alignment added small
per-blob branches in `HeaderWalker::next_header` (payload-extent
check, probe-pread arm cleanup) and rewired
`BlobReader::skip_blob_body` from `seek_relative(n)` to
`seek_relative(n-1) + read_exact(1)`. First-order analysis predicted
no extra syscalls in steady state. Verified empirically against the
four heaviest users of the touched primitives:

| Command | Pre-`436998b` | Post-`436998b` (`16e3694`) | Δ |
|---|---:|---:|---:|
| getid (include) | 6.1 s (`24362e36`) | 6.8 s (`41413398`) | +0.7 s, +11 % (within `--bench 1` noise at sub-10 s walls) |
| check --refs | 72.5 s (`862547e4`) | **53.8 s** (`7d9f5dfd`) | **−18.7 s, −25.8 %** |
| add-locations-to-ways external | 603.7 s (`aa0dc719`, post-A1) | **546.0 s** (`7fd04130`) | **−57.7 s, −9.6 %** |
| build-geocode-index | 432.9 s (`b4b25c05`) | 424.8 s (`2b412af4`) | −8.1 s, −1.9 % (within noise) |

No regression from the truncation-alignment commit. check-refs and
ALTW external both moved materially faster than noise; the wins are
attributable to commits landed between the prior baselines and
`16e3694` (the ALTW row carries A1's effect against the older
pre-A1 row in the audit comment).

### Kind claims checked at decode (bygg, 2026-10-27, commit `7ecb8a43`)

Every block now records the element kinds it holds in its construction
validation pass, the classify pools compare them with a scheduled blob's
claimed kind under `KindClaims::Refuse` (adopted by `check --refs` and
`check --ids`), and the pipelined reader carries the setting unused.
Matched `--bench 1` cells against the parent `e8ae74f3`, alternating parent
then new, three pairs each, bygg, untrimmed:

| Cell | New `7ecb8a43` | Parent `e8ae74f3` | UUIDs (parent, new) per pair |
|---|---:|---:|---|
| check-refs planet | 41.9, 41.5, 41.5 s | 41.8, 41.2, 41.3 s | `9a095693` `efbdc9ab`, `584ca62b` `fcf76ae8`, `8bfaed50` `7cb61453` |
| check-ids planet | 38.1, 38.1, 38.2 s | 37.8, 37.9, 38.0 s | `3d84b122` `418b6270`, `13be238e` `81d53a8b`, `7e1ec65b` `519d0c43` |
| tags-filter -R planet (pipelined, `w/highway=primary`) | 30.5, 30.5, 30.4 s | 30.4, 30.4, 30.4 s | `70a58d33` `0e6b810d`, `d6694f01` `b7ae4e70`, `794d7302` `cb88fee3` |

Medians move by 0.5 % (check-refs), 0.5 % (check-ids) and 0.3 %
(tags-filter), inside the noise band. The checks' sign is consistent across
pairs; the phase splits (`584ca62b` vs `fcf76ae8`, `13be238e` vs `81d53a8b`)
put the difference in the way phase, about 0.2 s of 11 to 14 s at the same
cores, with the schedule walk and the node phase identical.

The adoption by every other per-kind phase (`431d3130`) and the raw
walkers returning the kinds they read (`43e20723`) were measured where they
add work per element, the raw-walker adopters; elsewhere the check is one
compare per decoded block. Interleaved `--bench 1` cells against
`6bb0ad09`, two pairs each, bygg, untrimmed:

| Cell | New `43e20723` | Parent `6bb0ad09` | UUIDs (new, parent, parent, new) |
|---|---:|---:|---|
| extract simple planet, Europe bbox | 119.1, 120.6 s | 118.6, 122.1 s | `7341cc2c`, `0f7da82d`, `0111601a`, `9a2799ee` |
| build-geocode-index planet | 316.7, 315.2 s | 321.1, 321.2 s | `02bd5b17`, `c6f71f3e`, `838241cf`, `471766ed` |

Both inside the noise band (extract medians 119.9 against 120.4 s, geocode
316.0 against 321.2 s).

### Packed varint count (bygg, 2026-10-31)

Block construction counts every packed varint array (the dense id, lat, lon
and metadata arrays, way refs and coordinates, relation members) to check
parallel-array lengths. `e61e0ad7` made the count refuse a tenth byte above
1 by carrying a position within the varint through a per-byte loop, which
replaced a plain terminal-byte count. `brokkr read --dataset planet
--bench 1`, its parent `2ba3e170` then `e61e0ad7`, one pair, untrimmed:

| Mode | `2ba3e170` | `e61e0ad7` | Delta | UUIDs |
|---|---:|---:|---:|---|
| sequential | 524.9 s | 622.7 s | +18.6 % | `37e81b55` `9d17af61` |
| parallel | 28.7 s | 31.5 s | +9.7 % | `c7fd866a` `9dd0794c` |
| pipelined | 197.4 s | 196.1 s | -0.7 % | `2bac0419` `bb0de759` |
| blobreader | 562.2 s | 646.0 s | +14.9 % | `be0344af` `05ca943b` |

`brokkr sidecar --compare 37e81b55 9d17af61` shows the same single core and
the same bytes read: pure decode CPU. `cat --clean --dataset europe` on the
same pair is flat (158.7 s `c6513257` against 156.3 s `a822749f`): its
encode and compression hide the count.

The count now works eight bytes at a time (a mask and a popcount per word,
only the continuation run length carried between words; arrays holding a
run of nine continuation bytes take the exact per-byte scan), with the same
refusal. HEAD `14909999` as a stored `--commit` cell against the change
uncommitted on top of it (`--force`, not stored), alternating, two pairs:

| Mode | `14909999` | change | Delta (pair 1, pair 2) |
|---|---:|---:|---:|
| sequential | 597.6, 597.2 s | 528.9, 526.4 s | -11.5 %, -11.9 % |
| parallel | 31.1, 30.7 s | 28.8, 28.9 s | -7.4 %, -6.1 % |
| pipelined | 193.2, 192.6 s | 190.0, 190.7 s | -1.7 %, -1.0 % |
| blobreader | 635.2, 637.8 s | 567.2, 567.0 s | -10.7 %, -11.1 % |

`14909999` UUIDs: `fe3c0729` `302e9434` `4ecbe852` `a6d47169` (pair 1),
`f6a3283c` `acb37618` `f7c4555d` `6c041b12` (pair 2). The signs agree across
pairs, and the change brings sequential and blobreader back to the level of
`2ba3e170`. Pipelined read is bound elsewhere and does not move.

The whole read-path range, `ce6f20d9^` (`c0f67d9e`) against `307982d7`
(the one frame decoder) and HEAD `82be10a4`, alternating cells later the
same day:

| Mode | `c0f67d9e` | `307982d7` | `82be10a4` |
|---|---:|---:|---:|
| sequential | 524.4, 522.5 s | 604.7, 602.7 s | 534.9, 535.2 s |
| parallel | 28.6, 29.1 s | 31.4, 31.4 s | 29.1, 29.5 s |
| pipelined | 196.0, 195.6 s | 195.3, 195.8 s | 191.8, 191.1 s |
| blobreader | 562.4, 560.8 s | 642.7, 641.9 s | 574.9, 577.6 s |

The range's +15 % is the count above and nothing else: HEAD sits within
about 2.5 % of the range's start on every mode (sequential and blobreader
+2.2 to +2.6 %, both cells; pipelined -2 %), inside the noise floor. UUIDs
per run in mode order: `fa1a43ed` `cf9ee896` `aa12912c` `6104a00d` and
`c2a44d9a` `50eb615e` `4d6151f6` `dc6ccb4e` (`c0f67d9e`); `725c022a`
`a9baa55a` `0adb24b2` `02d7558c` and `6e9f2082` `1f946c48` `b6f51bdd`
`ddbaec65` (`307982d7`); `d52a1091` `455504be` `e0f24d30` `c432d7cc` and
`0a3ca1d6` `cbb3177c` `4738d079` `c70c136d` (`82be10a4`).

## Coordinate scaling (bygg, 2026-10-28)

`71fd420b` below is the measurement build of the commit that landed these
changes: the same non-test code, with this record and one more unit test
added when the commit was amended, so the hash in `results.db` is not on
`main`.

### Columnar granularity-100 shortcut

The columnar dense-node decode (`DenseNodeColumns::decode_append`, used by
`extract --strategy smart` and multi-extract node classification) once had a
granularity-100 shortcut, `cumulative + offset / 100`, that skipped the
per-node multiply and divide. It was added on an estimate (about 8 cycles x
4B nodes, about 10 s at planet), never measured, and it wrapped at a
different point from the element accessors, so on a block whose true
nanodegree value overflows `i64` it classified a node at a different
coordinate from every other reader. Removing it was measured as matched,
interleaved `--bench 1` cells on planet, `64e478f8` (with) against
`71fd420b` (without; the same build also carries the exact node-scanner
scaling below, which these commands do not run):

| Command | With (s) | Without (s) | Median delta |
|---|---|---|---:|
| `extract --strategy smart --bbox=-25.0,34.0,45.0,72.0` | 139.3 `fcd813b1`, 140.3 `61ae1b01`, 141.2 `f73d8063` | 138.3 `2275347f`, 140.9 `c5c4c31f`, 140.9 `069114e2` | +0.4 % |
| `multi-extract --regions 5 --strategy smart --bbox=-10.0,35.0,32.0,72.0` | 481.5 `c207a1e9`, 487.5 `a0e311f1`, 486.7 `7a65cd87` | 482.9 `71901454`, 487.8 `167d427e`, 487.4 `9137d2c6` | +0.1 % |

Node classification is the phase the shortcut lived in, and it did not
move: `PASS1_NODE_CLASSIFY` 21.76 s against 21.62 s (`61ae1b01` against
`c5c4c31f`), about 19 s on both sides in multi-extract. The phase reads
about 56 GB and runs about 25 cores; it is bound by decompression and I/O,
not by the scaling arithmetic. The shortcut was deleted.

### Exact node-scanner scaling

The node scanner (`scan/node.rs`, used by add-locations-to-ways and the
apply-changes `--locations-on-ways` extractors) scales with checked `i64`
arithmetic and falls back to exact `i128` only when an intermediate
overflows, so the hot path keeps its `i64` multiply and divide. Matched,
interleaved `--bench 1` cells on planet, `64e478f8` against `71fd420b`:

| Command | Before (s) | After (s) | Median delta |
|---|---|---|---:|
| `add-locations-to-ways --index-type external` | 735.6 `a8fe2f08`, 707.8 `bec9aaf8`, 675.1 `ba12609f` | 669.5 `20585f4b`, 691.2 `73adb2d8`, 698.5 `9874e28c` | -2.3 % |
| `apply-changes --locations-on-ways`, `altw-keep-untagged` base, OSC 4913 | 335.6 `c5ff6ab4`, 307.3 `f95e13b2`, 307.5 `558d3e1e` | 311.5 `029a1e71`, 305.5 `b40e1b3c`, 310.8 `49d568d7` | +1.1 % |

Both inside noise, with mixed signs across pairs. The altw pair that
overlapped a manual disk cleanup (`974d616a` / `10f71ab4`) was discarded and
rerun as `ba12609f` / `9874e28c`. The altw node stage reads about 199 GB at
under 3 cores, so it is I/O bound. No fstrim preceded these cells.

### Scanner granularity read and the join-time domain check

The node scanner reads `PrimitiveBlock.granularity` as any `int32`, like
every reader, instead of bounding it to `1..=1e9` (a per-block change), and
apply-changes `--locations-on-ways` refuses a kept base node outside the
geographic domain (one branch on the kept-node path of the extraction sink,
not per scanned node). Matched, interleaved `--bench 1` cells on bygg,
2026-10-28, after a fresh `fstrim`: baseline is a clean `38f6ee8c`; the
variant is the same tree with the change as uncommitted edits, run with
`--force`, so its cells are not in results.db and its numbers come from
the run output and `brokkr sidecar dirty`.

| Arm | Wall (s), in run order | Median | `merge_streaming_coord_extract_ns` summed over workers (s), median |
|---|---|---:|---:|
| Baseline `38f6ee8c` | 305.6 `f1973516`, 301.4 `bc1e7026`, 302.4 `8995dd68` | 302.4 | 235.7 |
| Variant (forced) | 295.6, 297.9, 299.8 | 297.9 | 237.9 |

Wall -1.5 % (variant faster in all three pairs), extraction time +0.9 %:
both inside noise. add-locations-to-ways was not re-measured: its per-node
code is untouched, and the granularity read runs once per block.

## Cost of appending the offset tables to the payload (bygg, 2026-11-01)

What block construction pays to append the string-table and group
offset tables to the decompressed payload (`WireBlock::inline_tables`),
the question being whether keeping the tables in separate per-block
storage would pay. Planet `planet-20260223-with-indexdata`, `--hotpath`
cells on `d7d4f967` plus a `hotpath::measure` attribute on
`inline_tables` (unstored `--force` cells):

| Command, buffer source | Calls | Avg | P50 | P95 | P99 | Total | Decode it rides on |
|---|---:|---:|---:|---:|---:|---:|---|
| cat `--clean`, pooled | 50,816 | 10.7 us | 7.7 us | 26.2 us | 44.5 us | 0.55 core-s | `decompress_blob_raw` 339 core-s, wall 154.6 s |
| diff-snapshots base to `20260411`, fresh buffer per block | 107,701 | 35.2 us | 9.6 us | 167.8 us | 272.4 us | 3.79 core-s | block-pair `decode` 1,113 core-s, wall 111.2 s |

The unpooled path's tail is consistent with the payload moving on the
growth (a buffer arrives exactly full), though the timer covers the
reservation and the serialization together. Even there the append is
0.3 % of decode work; spread over the run's cores it is about 0.1 s of
wall, a throughput share rather than a bound. Separate storage would not
remove the table bytes, only move them; what it would remove is a moving
realloc's transient old payload per active decoder and any allocator
retention of it, neither measured. It was not built. time-filter's
snapshot path builds a block per blob on the ordered pool, so it
appends like cat `--clean`; it was not measured separately.

## What the read pipeline's decode scratch holds (bygg, 2026-11-03, on `26db96fe`)

The pipelined reader's decode workers each keep a string-table and a
group-range scratch list for the whole read. Planet
`planet-20260223-with-indexdata`, `inspect --extended --bench 1`, one
unstored `--force` cell on `26db96fe` plus the
`pipeline_scratch_pool_capacity_peak_bytes` counter (30 decode workers,
25.2 average cores):

| Counter or measure | Value |
|---|---:|
| `pipeline_scratch_pool_capacity_peak_bytes` (one read's workers summed) | 33,088,768 B (31.6 MiB) |
| `pipeline_scratch_st_capacity_peak_bytes` (one worker) | 2,097,152 B |
| `pipeline_scratch_gr_capacity_peak_bytes` (one worker) | 4,096 B |
| Peak anon | 283,696 kB (277 MiB) |
| Wall | 27.09 s |

The pool sum is allocated capacity, 11 % of the read's peak anon at most
(an upper bound on what is resident). This run's scan is a projection,
which reads the tables from scratch, so building the tables straight into
the payload tail would remove none of it; the reasoning for not building
that is at the thread-locals in `crates/lib/src/read/pipeline.rs`. The wall is in
line with the stored 26.80 s cell `5b8e6c29` (`179f1b7e`, not a matched
pair); the counters record only when a list grows, a few dozen times per
worker at most, instead of on every blob.

## Header walk read-ahead on cold dense files (bygg, 2026-11-08, baseline `bb414882`)

`PreadSource` (`crates/lib/src/read/header_walker.rs`) keeps its read window across
payload skips and, on a refill, reads a 1 MiB window instead of the 4 KiB
probe when two gates pass: density (mean of the last 8 completed frames
below 128 KiB, so the planet dump's ~1.8 MB frames never qualify) and
residency (a 4 KiB `preadv2(RWF_NOWAIT)` probe first; a hit is the window
at no I/O, and only 4 dense refills missing in a row authorise a window).
The walk is a dependency chain, so read-ahead is the only lever: in-flight
exact probes cannot overlap it. Variant cells are `bb414882` plus the
uncommitted change (bench store marks them dirty); baseline cells are
`--commit bb414882`. Every `vs-osmium` cell starts with its input evicted.

Density gate alone (first variant), alternating pairs:

| Row | Baseline | Variant |
|---|---|---|
| denmark inspect | 644 (`e7be7f03`), 638 (`e009a69f`) ms | 227 (`c108e83b`), 190 (`24f225ca`) ms |
| denmark inspect tags --type way | 693 (`9a53c4ee`), 691 (`92a2efbd`) ms | 263 (`b5f2b35b`), 240 (`c4cf854d`) ms |
| denmark getid | 650 (`2fb09f94`), 649 (`6e3c7fae`) ms | 198 (`6e0f1abd`), 199 (`f274ac24`) ms |
| planet inspect | 6.1 (`8d48e999`), 6.8 (`eb1cb944`) s | 6.8 (`c0c93714`), 6.1 (`6b01c0bb`) s |
| europe inspect | 45.6 s (`1f4345fc`) | 14.4 s (`1520f7b2`) |

Planet is unchanged: both arms read 654-659 MB and the second cell of each
pair was the faster one, an order effect. But density alone regressed a
header-warm walk: europe `check --refs` after a cold europe inspect had left
the headers cached and the bodies not, baseline `3dee9165` 8.3 s with
`SCHEDULE_SCAN_LOOP` 270 ms and no disk read, variant 27.6 s with the walk at
19.8 s reading 15.9 GB - windows fired at refills whose headers were
resident and read the bodies between them, and on a file larger than RAM
(35 GB against 29.8 GB) every walk of a multi-walk command would repeat it.
That is what the residency gate is for.

Final shape (both gates):

| Row | Cells | Median |
|---|---|---|
| denmark inspect | 224, 208, 217 ms | 217 ms (baseline 638-644 ms) |
| denmark inspect tags --type way | 266, 259, 264 ms | 264 ms (baseline 691-693 ms; osmium 395 ms, `7e13294a`) |
| denmark getid | 217, 216, 194 ms | 216 ms (baseline 649-650 ms; osmium 619 ms, `ac1e2007`) |
| europe inspect | 14.4 s (`445a6c9a`) | baseline 45.5-45.6 s (`1f4345fc`, `cb4aad64`) |
| planet inspect | 6.8 s (`8fdf1ee1`) | inside the baseline's 6.1-6.8 s |
| europe check --refs, headers cached | 8.24 s, walk 288 ms, no disk read | baseline 8.28 s (`3dee9165`), walk 270 ms |

The header-warm check-refs cell followed the same predecessor as its
baseline (a cold baseline europe inspect, `cb4aad64`); its counters were
464,510 cache hits, no misses, no windows. A warm denmark inspect is still
5 ms. Not measured: a header-warm walk under memory pressure with scattered
evicted headers (the unit tests pin that isolated misses read no window,
not its wall), and the multi-walk commands on a cold europe.

## Header walk prefetch on cold dense files (bygg, 2026-11-09, baseline `bd2408d5`)

The windows above are still one synchronous read each: under the walk's
`fadvise(RANDOM)` Linux fills a 1 MiB window in readahead-sized steps, one
after another, and caps every `WILLNEED` call at the larger of the device's
optimal I/O size and the readahead window (128 KiB each on bygg's
nvme0n1). A cold denmark `cat --dedupe` (`dedupe-pair --with one-relation
--cache cold`, baseline `e4484a26`) read 306 MB in 292 windows during its
177 ms pass 1, about 1.7 GB/s, and its copy then read the other ~150 MB cold;
the old engine's SEQUENTIAL `BufReader` pass 1 read the whole file at about
3.5 GB/s and copied from cache. So the cold gap was the walk's queue depth,
not a header-only walk.

Change: a window read at or past the end of the last prefetch episode
advises `WILLNEED` over the 32 MiB after it, in 128 KiB calls; crossing an
episode's end restarts the miss streak, so only fresh misses in unadvised
bytes open the next. Variant = `bd2408d5` plus the uncommitted change
(stored dirty), baseline = `--commit bd2408d5`, alternated, after
`sudo fstrim -av`. Walls are bench iterations; child walls in parentheses.

| Row | Baseline | Variant |
|---|---|---|
| denmark dedupe-pair cold | 554 (`19906e02`), 538 (`45668a5a`), 534 (`66f7bbbc`) ms | 370 (`23f1b16e`), 380 (`0b904335`), 391 (`f9a5570b`) ms |
| europe inspect cold (`vs-osmium`) | 14.2 s (`59ab7bd5`) | 9.7 s (`81c02c14`) |
| planet inspect cold (`vs-osmium`) | 6.8 s (`69bb00b9`) | 6.8 s (`7766bd50`) |

Denmark phase split, `45668a5a` against `0b904335`: pass 1 223 to 126 ms,
copy flush 272 to 210 ms (warm flush 152 ms, `abfcf184`), child 509 to
351 ms; the variant's walk read 12 windows, opened 12 episodes advising
403 MB, and hit cache on 6,203 refills. The old engine's cold cells were
327-389 ms (`4f376168` and the callout above), so cold passthrough is back
at the old engine's wall. Planet never qualifies for windows
(`header_walk_prefetch_episodes` 0). Not measured: the header-warm europe
`check --refs` cell above (by construction a walk with cached headers has no
misses and opens no episode; the unit test
`a_cached_header_tail_ends_the_prefetch_within_one_episode` pins the mixed
case), cold sort on a dense file, and memory pressure.

## Zero-free FileReader append (bygg, 2026-11-09, `3286a9c5` vs uncommitted variant)

Every `Frames` built over a `FileReader` now appends through
`FileReader::append_to`: buffered bytes copied out of the `BufReader`, a
remainder of at least its capacity read by a bounded `read_to_end` on the
`File` (native `read_buf`, no zero-fill), O_DIRECT copied from its aligned
buffer. Baseline cells stored at `3286a9c5`; variant cells `--force`
(sidecar ids). Buffered, untrimmed, A B B A per command.

| Cell | Arm | Wall | utime | read syscalls |
|---|---|---:|---:|---:|
| planet plain cat | base (`dc7ccb05`) | 42.2 s | 2.96 s | 351,017 |
| planet plain cat | zero-free (`2605125d`) | 41.4 s | 0.45 s | 152,452 |
| planet plain cat | zero-free (`2179f0ea`) | 57.7 s | - | - |
| planet plain cat | base (`907f94f4`) | 59.0 s | - | - |
| europe getid full-scan arm | base (`5eb33820`) | 8.96 s | - | - |
| europe getid full-scan arm | zero-free (`66b28f09`) | 9.02 s | - | - |
| europe getid full-scan arm | zero-free (`a752d494`) | 8.95 s | 0.30 s | 134,052 |
| europe getid full-scan arm | base (`b983f104`) | 8.97 s | 0.44 s | 130,853 |

The mechanism shows (planet: about 3.9 fewer read syscalls and 50 us less
user CPU per blob; europe getid, 458k blobs under the 256 KiB buffer: a
third less user CPU, syscalls unchanged), the walls do not: both commands
are I/O bound (planet 0.4 cores with walls climbing per back-to-back 92 GB
write, europe getid 0.2 cores at about 3.9 GB/s from disk). The
warm-cache many-small-blob walk where the zeroing could reach the wall
cannot be built on this 30 GB host (europe and planet 8k exceed RAM).
Landed as a CPU reduction with no wall cost. Not to be re-measured
without a change to the append or a host that can hold such a walk warm.

## Scheduled-read advice: RANDOM against plain (bygg, 2026-11-09, at `3d980bee`)

The header walker's description is advised `FADV_RANDOM`, and the pools a
walk hands it to read under that advice; the other pools open plain
descriptions. Neither choice had been measured. Variants were uncommitted
(`--force`, not stored); `read_ahead_kb` is 128 on bygg's nvme. Europe
`add-locations-to-ways --index-type external --bench 1`, consecutive cells:

| Arm | Wall | Stage 1 pass A | Relation scan | Stage 2 | `s2_node_pread_ns` sum | Stage 4 | `s4_pread_ms` sum |
|---|---:|---:|---:|---:|---:|---:|---:|
| HEAD, all plain (`36c31139`) | 112.3 s | 12.14 s | 4.55 s | 47.90 s | 22.3 s | 44.90 s | 64.6 s |
| stage 4 RANDOM (dirty `1f9fc4cb`) | 111.0 s | 12.16 s | 4.60 s | 46.93 s | 22.0 s | 44.63 s | 65.2 s |
| stages 1, 2, 4 and the relation scan RANDOM (dirty `9c63619a`) | 118.3 s | 12.30 s | 4.69 s | 53.05 s | 68.1 s | 45.66 s | n/a |

Stage 2 is the cost: each worker preads the contiguous run of node blobs
for its id bucket in file order (464,702 preads, 21.3 GB, identical in all
arms), a sequential stream that readahead pipelines and RANDOM turns into
synchronous demand reads. Stage 4 dispatches blobs one at a time across 30
workers, so no reader has a run and the advice does not matter there.

The classify side, extract `--simple -b=-25,34,45,72` on europe with the
three classify pools on a fresh RANDOM description against their usual
fresh plain one (WILLNEED unchanged; both arms dirty, order V B V B):
node classify 5.90 / 5.82 / 5.89 / 5.82 s, way classify 3.31 / 3.28 /
3.29 / 3.28 s, walls 51.7 / 51.1 / 51.2 s and B's second cell not run.
About 1 % against RANDOM, consistently, which is below noise.

Ruling: advice follows how a pool's readers walk the file. Readers of
contiguous runs keep a plain description (altw external stage 2,
measured). Per-blob dispatched pools are indifferent (stage 4, extract
simple's classify pools), so the walker handoff stays: it keeps the
scheduled file's identity for a path source at no measured cost.
Unmeasured: apply-changes' pool, renumber's pools, extract's
`pread_execute` (its consumer reads passthrough frames in schedule order)
and the relation scan in isolation; planet, where stage 4 is
readiness-bound (eb94af17: readiness waits sum 1622 s against pread
552 s). Not to be re-measured without a code reason.

## History

#### Read throughput (commit `d387301`)

Count all elements, best of 3 runs.

| Dataset | Size | sequential | parallel | pipelined | blobreader | mmap |
|---------|------|-----------|----------|-----------|------------|------|
| Malta | 9 MB | 49 ms | 9 ms | 24 ms | 50 ms | 52 ms |
| Denmark | 487 MB | 2.86s | 463 ms | 1.46s | 2.93s | 2.93s |
| Norway | 1.4 GB | 8.4s | 1.33s | 4.9s | 8.9s | 8.8s |
| Japan | 2.4 GB | 14.5s | 2.1s | 8.0s | 15.2s | 15.2s |
| Germany | 4.7 GB | 26.9s | 4.2s | 13.0s | 27.8s | 27.6s |

North America (18.8 GB, 2.58B elements, commit `a6ebbfe`):
parallel 22s, pipelined 57s, sequential 130s.

#### `seek_raw` BufReader-discard fix (2026-04-18, commit `aa3147c`)

`BlobReader::seek_raw(SeekFrom::Current(_))` was the hot-path "skip past
just-read blob body." Stdlib `Seek::seek` on `BufReader<File>` always
discards the buffer, so every blob-body skip forced a buffer refill
(~10× file-size read amplification at the default 256 KB buffer).
Fixed via a public `BlobReaderSource` trait: default `skip_relative`
falls through to `Seek::seek` (correct, slow), `BufReader<R>` impl
overrides to call `BufReader::seek_relative` (preserves buffer when
in-range). Internal hot-path methods (`next_header_skip_blob`,
`next_header_with_data_offset`) route through a new `skip_blob_body`
helper using the trait. Public `seek_raw(SeekFrom)` API unchanged for
absolute-seek case. Bound widening on `BlobReader::new_seekable` and
`IndexedReader::new` is the public-API impact (one-line workaround for
downstream library users with non-standard reader types).

Per-caller wall deltas (`--bench 1` single-shot, plantasjen, 2026-04-18):

| Caller / Command | Dataset | Pre-fix `ca6711e` | Post-fix `aa3147c` | Δ |
|---|---|---:|---:|---:|
| extract --smart | Europe | 211.2 s | 195.2 s | **−16.0 s, −7.6 %** |
| add-locations-to-ways --index-type external | Europe | 286.3 s | 270.7 s | **−15.6 s, −5.5 %** |
| add-locations-to-ways --index-type external | Planet | (skipped) | 700.6 s | within noise |
| tags-filter | Europe | 91.7 s | 93.1 s | within noise (+1.5 %) |
| renumber | Planet | 218.6 s | 206.7 s | **−11.9 s, −5.4 %** |

#### Pipelined-reader decode-admission bound (`a0a2e3b`, 2026-07-10, plantasjen)

The 3-stage pipelined reader's stage-2 dispatcher used to `rayon::spawn`
per blob without backpressure, and the reorder buffer admitted far-ahead
blocks unboundedly - so decoded-block memory grew with file size, not
with the nominal `read_ahead`/`decode_ahead` caps (elivagar measured a
reorder high-water of 660 and a 21.5 GB RSS peak on a 19 GB North
America run; the same unfiltered-full-scan shape was live in pbfhogg's
own `time-filter` and `altw` sparse residual paths). Fix: an
`AdmissionGate` with release-after-deliver permits - a permit rides each
decoded item through the channel and reorder slot, so
`admitted - delivered <= decode_ahead` is a hard cap on live decoded
blocks. Landed `a0a2e3b`; validated same day, all gates kept:

| Gate | V0 (`86a03f2`) | V1 (`a0a2e3b`) | Bound | Result |
|------|---------------:|---------------:|-------|--------|
| read japan pipelined | 7.78 s | 7.48 s | <= x1.03 | PASS (−3.9%) |
| read europe pipelined | 110.2 s | 95.9 s | <= x1.05 | PASS (−13%) |
| getid europe `--add-referenced` | ~73 s | 74.3 s | <= x1.03 | PASS (+1.8%) |
| tags-filter europe `-R` | 19.59 s | 19.8 s | <= x1.03 | PASS (+1.1%) |
| memory: reorder filled high-water | unbounded (660 observed) | 32 | <= 64 | PASS |
| backpressure engaged | - | `decode_admit_blocked=10216` | > 0 | PASS |
| denmark smokes | fast | fast | no blowup | PASS |

Provenance: V1 rows are in `results.db` at `a0a2e3b` (japan read
`ba5e1f0a`, europe read `7f35648f`, getid `cc846f0c`, tags-filter
`4241316a`); V0 *read* baselines were captured post-hoc via
`brokkr read --commit 86a03f2` (japan `3285d2af`, europe `e3c62000`).
The V0 getid/tags-filter numbers were never stored as DB rows - they
survive only in the validation-session log and orphaned sidecar
artifacts, so treat those two baselines as approximate.

Counter-semantics change that rides along: `pipeline_reorder_high_water`
now counts FILLED reorder slots (the memory diagnostic); the old
window-length meaning (the completion-skew diagnostic) moved to
`pipeline_reorder_window_high_water`. Cross-run comparisons against
pre-`a0a2e3b` UUIDs (e.g. the 660) must use the window column. New
counters `pipeline_decode_admit_wait_ns` / `pipeline_decode_admit_blocked`
capture the read backpressure that `raw_send_wait_ns` structurally never
saw (stage 2 never let the raw channel fill).

Deliberately deferred alternative: exporting the
`scan::classify::parallel_classify_phase` machinery as `pub` API (so
external consumers could adopt the bounded-by-construction pread-worker
shape) was weighed and postponed - the classify surface is still moving
in-tree, and `pub` is a hard-to-walk-back contract. Revisit only if
elivagar proves classify is its long-term primary read path.

---

### Env-gated read-path batch (2026-07-12 overnight, commit `a65cecc`, plantasjen)

Five read-path follow-ups from the 2026-07-11 architecture reports
(codex xhigh + Fable, verbatim in git history) were landed behind default-off
`PBFHOGG_*` env gates and adjudicated in one overnight A/B run - one binary,
one commit, gate-off IS the baseline, same-day A/B by construction. Host
plantasjen (23 GB RAM this run; the reports' "26-30 GB" was swap-inclusive).
Pre-registered noise floor +/-3%. The working read-out lived in a plan
note deleted on landing; this is the durable record.

| Item | Gate | Verdict | Key numbers |
|---|---|---|---|
| 1 fadvise DONTNEED watermark | `PBFHOGG_FADVISE_BATCH_BYTES` | REVERT | planet-8k all modes +0.75% to +2.68%, inside noise, mildly slower |
| 2 byte-aware buffer knobs (x4) | `READ_AHEAD/DECODE_AHEAD/BLOCK_QUEUE/CMD_BATCH_BYTES` | REVERT | 8k pipelined +3.13%, CMD_BATCH getid-8k +3.49% regressed; no win anywhere |
| 3 command-transform fusion | `PBFHOGG_FUSE_TRANSFORM` | **KEEP** | getid-8k -7.68%, getparents-8k -6.51%, tags-filter-R 8k -6.97%; getid-primary GETID_PASS2 RSS 1.18 GB -> 596 MB (-50%) |
| 4 ordered batched pipeline | `PBFHOGG_BATCHED_PIPELINE` | REVERT | getparents-8k -6.67% (redundant with fusion); combination getid-8k +9.30% regression |
| 5 europe prefetch WILLNEED | `PBFHOGG_PREFETCH_WILLNEED` | **KEEP** | check-refs europe -6.16%, tags-filter europe -5.66% |

**Failed experiments, recorded so they are not blindly re-attempted:**

- **Ordered batched-pipeline rebuild (item 4)** - a byte-bounded ordered
  batch engine with long-lived workers, both architecture reports'
  "high-conviction" rewrite. Measured: no isolated win. Its one
  floor-clearing cell (getparents-8k FullScan -6.67%, base `357c360f`
  62.0 s -> `53a9e76a` 58.8 s bench-3) is redundant - fusion delivers the
  same win on the same cell. Reads neutral (pipelined 8k +2.10%, primary
  +1.59%, europe -1.40%), getid-8k isolation +0.05%, tags-filter-R 8k
  -0.65%. The both-gates combination (BATCHED+FUSE getid-8k, `f1d76362`)
  REGRESSED +9.30% vs baseline (197.9 -> 216.3 s) - and +18.4% vs
  fusion-alone (182.7 s, `895184ee`), the figure that actually kills the
  candidate since fusion-alone is the shipped end state - with deeper
  reorder skew (`pipeline_batched_reorder_high_water` 32 vs 14,
  `pipeline_batches` 43564 vs 30977). Diagnosis: at 1.45 M blobs the per-blob channel/task/
  permit/reorder seams cost LESS than the reports estimated, and the batch
  engine's own per-batch coordination gave the difference back. Do not
  re-propose a batch-rebuild of `run_pipeline` on "per-blob seam overhead
  x blob count" reasoning without first measuring that the seams are the
  binding cost - here they were not.
- **fadvise DONTNEED watermark batching (item 1)** - watermark-batched
  eviction to replace the per-blob cumulative-prefix `posix_fadvise`. The
  1.45 M advisory syscalls on the 8k encoding are real but carry no
  measurable wall cost; every planet-8k mode came out inside +/-3% and
  slightly slower gated. Mispriced (user confidence was already zero).
- **byte-aware buffer knobs (item 2)** - making `read_ahead`/`decode_ahead`/
  `BLOCK_QUEUE`/command-batch admission byte-primary instead of count-primary.
  No configuration improved anything; two knobs regressed just past the
  floor. Count-vs-byte admission was not the binding constraint at either
  encoding.

**Kept (promoted to default; current numbers in `reference/performance/README.md`):**

- **Command-transform fusion (item 3)** - moving getid pass-2, getparents
  FullScan, tags-filter `-R`, and altw decode-all transforms INTO the decode
  workers, deleting the 64-block (~90 MB) materialization + second rayon
  dispatch. All three surviving signal cells cleared +3% (getid-8k
  `a57807df` 197.9 -> `895184ee` 182.7 s; getparents-8k `357c360f` 63.0 ->
  `f461f307` 58.9 s; tags-filter-R 8k `0aa45689` 45.9 -> `896b8ffc` 42.7 s,
  all bench-3). Executing no-regression controls also improved: getid-primary
  -12.88% with GETID_PASS2 peak RSS halved (1.18 GB -> 596 MB) and pass-2
  wall -17% at higher core occupancy; tags-filter-R primary -6.25%. The altw
  europe-raw signal pair OOM-killed on the 23 GB host (anon decode-all +
  re-encode), so altw fusion is correctness-proven but performance-unmeasured
  - a follow-up night on a bigger host owes it. Resolved as State 3 of the
  fusion/batching four-state matrix (keep fusion, revert batching), since the
  two share the `run_pipeline` seam.
- **Europe WILLNEED prefetch (item 5)** - `POSIX_FADV_WILLNEED` over the
  scan schedule's `(data_offset, data_size)` ranges, reclaiming the
  page-warming the 2026-04-20 HeaderWalker swap to `POSIX_FADV_RANDOM` gave
  up. The TODO estimate ("~14 s") was unbacked; the first real measurement
  is ~6% on both europe consumers (check-refs `a64f56dd` 56.8 -> `7d114432`
  53.3 s; tags-filter `9b47383c` 61.8 -> `c8681230` 58.3 s, bench-3).
  Europe-only: planet is larger than RAM so prefetched pages evict before
  reuse.

