# `repack` - command design

New subcommand: re-encode a PBF with a configurable element-count cap
per blob. Motivated by `reference/blob-density.md` - we need a way to
produce same-corpus-different-encoding pairs so benchmarks can control
for blob-count effects independent of byte size.

Drafted 2026-04-24 as scaffolding before implementation. Will drift.

## Purpose

Read a PBF sequentially, emit a bit-identical-semantically PBF with a
different blob-size target. Primary consumer: the measurement matrix in
`reference/blob-density.md`. Secondary consumer: anyone reproducing
pbfhogg benchmarks on their own hardware who wants to control blob
density independent of byte size.

Non-goals:

- No element filtering (that's `getid` / `tags-filter` / `extract`).
- No format conversion (XML/OPL out are separate tools).
- No sort-order manipulation (that's `sort` / `degrade --unsort`).

## API

```
pbfhogg repack <input> -o <output> [--elements-per-blob N]
                                    [--compression C]
                                    [--direct-io]
                                    [--io-uring]
```

- `--elements-per-blob N` (default: 8000, matching PBF interop spec).
  Caller may pass 1 000, 64 000, 256 000, etc. No upper bound enforced
  beyond what `BlockBuilder` can materialise in memory (blob > a few
  MB compressed risks exceeding protobuf 32 MB message cap).
- `--compression C`: passthrough to `PbfWriter`. Useful for A/B against
  zstd vs zlib at a fixed blob size.
- `--direct-io` / `--io-uring`: standard pbfhogg write-path flags, free
  because it's all existing `PbfWriter` plumbing.

## Implementation sketch

Stream-read via `ElementReader::into_blocks_pipelined`, re-emit via
`BlockBuilder` + `PbfWriter`. Element-by-element in PBF sort order.

Pseudocode:

```rust
let reader = ElementReader::open(input, direct_io)?;
let mut writer = writer_from_header(output, compression, reader.header(),
                                     true, overrides, |hb| hb, direct_io, io_uring)?;
let mut bb = BlockBuilder::with_element_cap(elements_per_blob);

for_each_element(reader, |element| {
    match element {
        Element::Node(n)       => { ensure_node_capacity(&mut bb, &mut writer)?; bb.add_node(...); }
        Element::DenseNode(dn) => { /* same */ }
        Element::Way(w)        => { ensure_way_capacity(&mut bb, &mut writer)?; bb.add_way(...); }
        Element::Relation(r)   => { /* same */ }
    }
})?;
writer.flush()?;
```

### `BlockBuilder` element-cap plumbing

Today `BlockBuilder` has a hardcoded cap (check
`src/write/block_builder.rs` - likely 8000). Two options:

1. Add `BlockBuilder::with_element_cap(n)` constructor (new API).
2. Add a setter on existing `BlockBuilder::new()` instances.

Option 1 is cleaner; existing call sites stay identical. If (1) turns
out to require cascading param passing through a lot of callers,
option 2 is the fallback.

### Metadata preservation

Critical: every bit of per-element metadata must round-trip.

- tags (keys + values)
- OsmMetadata (version, timestamp, changeset, uid, user, visible)
- Way refs (delta-encoded node IDs)
- Relation members (id + type + role)
- LocationsOnWays if present (check header features)
- DenseNode packing (preserve dense encoding where input has it)

Writer handles most of this via `BlockBuilder::add_*`; the round-trip
via `Element::*` should preserve whatever the reader surfaces.

## Correctness criteria

**Semantic equivalence:** for every element in the input, the output
contains an element with identical ID, tags, metadata, and (for ways)
refs, (for relations) members with matching role and type.

**Ordering preserved:** if the input is `Sort.Type_then_ID`, the
output is too.

**Features header preserved:** `OsmSchema-V0.6`,
`DenseNodes`, `HistoricalInformation`, `Sort.Type_then_ID`,
`LocationsOnWays` - whatever the input declared. Writer-added metadata
(writingprogram etc.) can diverge.

**Indexdata regenerated:** the output is a fresh framing, so its
indexdata is newly computed for the new blob layout.

## Tests

1. **Denmark round-trip**: `repack --elements-per-blob 8000` then
   `repack --elements-per-blob 64000`, verify element count + sample
   IDs match original.
2. **Element-count cap respected**: any blob in output has no more
   than `N` elements (inspect via `brokkr inspect` or equivalent).
3. **Blob count prediction**: output blob count ≈ total_elements / N.
4. **Metadata preservation**: for a tagged corpus (e.g. denmark
   restaurants), verify tag multiset is identical across
   round-trip.
5. **Osmium cross-validation**: if osmium exposes
   `cat --output-format=pbf --set-block-size-elements=N` or similar,
   diff outputs. (Need to check osmium docs.)
6. **LocationsOnWays round-trip**: if input has `LocationsOnWays`
   feature, output must too, and coordinate values preserved.

## Scope for v1

Minimum viable:

- `--elements-per-blob N`
- `--compression C`
- DenseNodes preserved (no conversion between dense/non-dense
  encoding)
- LocationsOnWays preserved

Deferred:

- `--blob-size-bytes N` (target compressed size instead of element
  count - harder to target precisely)
- `--densify` / `--undensify` to convert between DenseNodes and
  plain Node encoding
- `--normalize-compression zlib:6` to force a canonical re-encode

## Open questions

- Does the current `ElementReader::into_blocks_pipelined` path
  yield elements in a form that round-trips cleanly through
  `BlockBuilder::add_*`? The `altw` passthrough and `sort`
  overlap-rewrite paths already do similar work - review those
  for patterns to reuse.
- How to time this at planet scale? A full repack is at least as
  expensive as `cat --type none`; a planet repack to 8k/blob
  (producing ~6-7 M blobs) could take many minutes. Bench the
  target-size produce step once and record it in `brokkr.toml` so
  we don't redo it casually.
- Is there value in emitting progress feedback (like `apply-changes`
  does) for long planet repacks? Likely yes for user experience;
  low priority for v1.

## Cross-references

- [`reference/blob-density.md`](../reference/blob-density.md) - the
  insight that motivates this command.
- [`notes/degrade.md`](degrade.md) - companion command for
  adversarial testing; shares the "take a PBF, emit a derived PBF"
  pipeline but with different semantics.
- [`src/write/block_builder.rs`](../src/write/block_builder.rs) -
  BlockBuilder; cap plumbing lives here.
- [`src/write/writer.rs`](../src/write/writer.rs) - PbfWriter.
- [`src/commands/cat/mod.rs`](../src/commands/cat/mod.rs) - closest
  existing "stream-read, stream-write" command; reference for
  structure.
