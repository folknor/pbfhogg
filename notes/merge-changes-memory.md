# merge-changes: remaining ideas (research note)

Transient research note. Nothing here is binding; it may be wrong. Written
2026-10-30, trimmed the same day after the streaming path moved to lexical
batches with spill (`src/commands/merge_changes/stream.rs`), which resolved
the 7-OSC peak-anon regression: planet 7-OSC 30 s / 895 MB to 9.9 s /
286 MB, 1-OSC 24 s / 1.6 MB to 2.7 s / 53 MB on bygg
(reference/performance.md, "Streaming path on lexical batches with spill").

What landed, and how it differs from this note's original R1 (a raw-slab
framer): readers still drive the one OSC driver (`drive_osc`), so framing
and structural checks stay in one machine; they record raw tag content
into `LexBatch`es, and workers replay those into the owned sink, so
attribute parsing, allocation, XML re-emit and gzip moved to the pool.
Output produced for a later input while an earlier one is still being
written goes to a per-input spill file in the run scratch, so the
inflate+tokenize critical path is the max over inputs, not the sum.
`merge_changes` takes a `RunScratch`; the CLI gained `--tmp-dir`.

## Remaining ideas

### R2. Validate-and-forward worker mode

Workers could validate a batch on borrowed attributes and forward the
source bytes instead of re-emitting. Saves CPU (numeric parse/format,
string allocation, escape per element); a wall win only where workers,
not readers, bound the run. Needs a real OSC grammar validator and an
explicit preservation policy (source spelling of coordinates, delete
payloads, attribute order). Preserving source bytes is a policy choice,
not a correctness loss.

### R3. The ledger budget at planet

`merge_changes_credit_wait_ns` summed 15.6 s over the 7-OSC run: readers
park on the 64 MiB budget. Whether a larger `MERGE_CHANGES_INFLIGHT_BYTES`
shortens wall (or whether the pool is simply saturated) is unmeasured;
one cell at a larger budget, read against the average core count, settles
it.

### R4. `--simplify` on the same readers

`--simplify` still parses whole inputs into owned `ChangeStream`s per
pool task (22.8 GB in `3e3ef119`); the reader/batch stage could feed its
dedupe, and `tags-filter --input-kind osc` shares the owned-sink re-emit.

## Lateral findings (still open)

1. **README merge-changes rows.** No stored cell pairs "54 s" with "2 MB"
   (the 54 s matches `b6e964cc` / `f4de3f8d`, both about 950 MB anon; the
   2 MB cells are the 267-273 s serial era), and neither 2 MB row holds
   after the batch change.
2. **`src/write/parallel_gzip.rs` header** claims `flate2::read::GzDecoder`
   handles concatenated members. It does not (only the first member);
   `MultiGzDecoder` does.
3. **`ParallelGzipWriter` window** at 32 workers admits up to 160 seqs x
   2 MiB = about 320 MiB raw payload. Bounded, but not small.
4. **`CORRECTNESS.md`** says malformed member roles become empty in all
   sinks; `src/osc/xml_parse.rs` now errors on a malformed optional role
   in the owned sinks (the compact sink still empties it). Not re-verified.
5. **The OSC driver's structure checks are thin** (codex finding, checked
   against `DriveState`): it tracks one section, one open element kind and
   the root close, but does not enforce the parent hierarchy. A recognised
   element nested inside another replaces the sink's current element, and
   unknown wrappers do not stop recognised descendants from being routed.
   Pre-existing, shared by every OSC consumer; strengthening it belongs in
   `drive_osc`.
