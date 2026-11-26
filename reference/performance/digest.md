# Digest

## Dense user_sid cursor for content forms (bygg, 2026-11-26, `1361533e` against `da1f1a55`)

Content forms decode only the dense `user_sid` column instead of full
dense metadata. Germany `canonical-digest --form content`, `--iterations
1` interleaved: `1361533e` 3.1 s `8dd08e6e`, 3.1 s `179cc91a`; `da1f1a55`
3.0 s `699767b8`, 3.0 s `95ed4517`. Within noise; the upper bound the
residue named (4.5 core-s of 80) was small to begin with.

## Content-form node records (bygg, 2026-11-26, against `ec2b2dc0`)

Where the content form's time goes, germany `--form content`, hotpath
builds: wall about 3.4 s at 25 cores; the ordered consumer waited on its
receive 74 % of wall, so the decode workers are the gate. Worker transform
80 core-s: record production (`summarize_block`) 56, decompression 14.7,
block construction about 9; consumer hashing 0.86 s. By block kind, node
blocks 32.6 core-s (about 76 ns per node), way blocks 23.2, relations 0.75.
Dropping dense metadata decode and the user check outright (incorrect, an
upper bound only) took 4.5 core-s off record production.

Landed: a dense node's content record is written as one 18-byte prefix
(kind, id, coordinate token) plus its tags and end byte instead of through
the token loop, and the content forms hash node records once, into the
all-records state, taking the node line from it non-consumingly when the
first way or relation block arrives. Digests identical to `ec2b2dc0` on
germany content (all four lines). Script bench `canonical-digest`,
`--iterations 1`, interleaved after one fstrim:

| Dataset | `ec2b2dc0` | change |
|---|---:|---:|
| germany content | 3427 / 3427 ms (`0d868e95`, `3da6fcc6`) | 3129 / 3117 ms (`656d74d8`, `b47c1f7f`) |
| planet content | 54.0 / 54.0 s (`fe7e5b1b`, `f6200d2d`), 29.0 cores | 48.1 / 48.4 s (`d8a21b45`, `57397de8`), 28.6 cores |

Planet -10.6 % at the same disk read; the pair does not apportion the gain
between the two parts. The full form is untouched (no all-records hasher;
its records go through `write_full_record`).

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

