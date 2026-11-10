# Digest

## digest v2 cost (bygg, 2026-11-08, against `110f0520`)

What `digest --form full` (v2, the default) costs over `--form content`
(v1), and why. No earlier measurement claimed v2 was free: when v2 landed
(`16154b0d`) its success-path cost was recorded as unmeasured. The first
cells (2026-10-30) already held a same-commit form pair, content 500.8 ms
(`a18ba1f1`) against full 701.0 ms (`ee543355`) on denmark at `4157567d`,
with content equal to v1 at `d2d8836a` (500.7 ms, `b0da638b`): the
difference was the form, never drift. Those rows step in 100 ms; the
cells below do not.

Base `110f0520`, stored, interleaved `--bench 1`:

| Dataset | content | full | full over content |
|---|---:|---:|---:|
| denmark | 438.9 / 425.3 / 436.5 ms (`f06a04b1`, `5a4e73b1`, `32165455`) | 568.7 / 565.3 / 565.9 ms (`3d580b85`, `791dda31`, `51531a64`) | +30 % |
| germany | 3447 / 3444 ms (`780005c0`, `fd27696f`) | 5248 / 5176 ms (`7b930651`, `bed503ef`) | +51 % |
| planet (`planet-20260223-reindexed`) | 59.8 s (`9e7877b3`), 25.0 cores | 106.8 s (`d8fffd30`), 14.9 cores | +79 % |

Planet core-seconds were 1494 against 1592, only 7 % more work for 79 %
more wall. A planet blob holds about 200k elements, so one block's
full-form records run to about 14 MB, and every decode worker grew such a
buffer from empty per block and freed it on the consumer. A hotpath run
put the per-block serialization at 39 ms average, 1987 s of wall in it
against about 1470 core-s of CPU in the whole run: the workers were
off-CPU about a quarter of their time.

Two working-tree variants (`--force`, not stored; planet full, same host):

| Variant | planet full | note |
|---|---:|---|
| exact parallel XXH3: a 4-worker pool computes each 1024-byte XXH3 block's lane sums, the consumer folds them in order; digests bit-identical to base on denmark (both forms) and germany | 110.3 s, 15.8 cores | the hash pool sat idle; the decode side was the gate |
| the same plus recycled record buffers | 66.2 s | |
| recycled record buffers only, serial hash as base | 61.7 s; again 62.7 s | consumer about 89 % outside its receive wait, dispatcher admission wait 99 % |

So the cost was the per-block allocate-grow-free of blob-scale record
buffers, not the serial hash, and the parallel hash did not pay once that
was gone. What landed is the recycling alone: the ordered consumer hands
each hashed buffer back, cleared, for a later block (at most
`DEFAULT_DECODE_AHEAD` kept). The parallel hash was not kept.

Recycled buffers against base, working tree (`--force`, not stored), digests
identical to base on denmark content and germany full:

| Dataset | content | full |
|---|---:|---:|
| denmark | 380.2 ms (base ~436 ms) | 460.1 ms (base ~566 ms) |
| germany | 3340 ms (base ~3446 ms) | 3881 ms (base ~5212 ms) |
| planet | 53.1 s (base 59.8 s) | 62.7 s, 25.1 cores, peak RSS 1.42 GB (base 106.8 s, 14.9 cores, 1.62 GB) |

Full over content after the change: denmark +21 %, germany +16 %, planet
+18 %, about the extra bytes the full form serializes and hashes.

