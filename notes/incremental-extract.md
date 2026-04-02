# Incremental extract update

## Problem

`extract --apply-changes base_extract.pbf + planet.osc.gz + region →
updated_extract.pbf` — update a regional extract using the daily diff
without re-reading the full planet PBF.

Current workflow:
```
pbfhogg apply-changes planet.pbf diff.osc.gz -o planet-updated.pbf  # 762s
pbfhogg extract planet-updated.pbf --bbox ... -o denmark.pbf         # ~100s
```

Total: ~862s. The extract reads the entire 87 GB planet PBF to produce
a 483 MB Denmark extract. 99.5% of the data read is discarded.

## What an incremental extract would do

```
pbfhogg extract --apply-changes denmark.pbf --diff diff.osc.gz --bbox ... -o denmark-updated.pbf
```

1. Parse the OSC diff
2. Determine which changes affect the extract region
3. Apply only those changes to the existing extract
4. Write the updated extract

Input: the existing 483 MB extract + a ~15 MB daily diff.
Expected time: seconds, not minutes.

## Challenges

### 1. Spatial filtering of the diff

The OSC contains changes from the entire planet. For a Denmark extract,
we only care about changes within Denmark's bbox. But:

- **Node changes**: easy — check if the node's coordinates are in the bbox.
  But a node could have *moved* — its old position was in the bbox but
  its new position is outside (or vice versa). Need both old and new
  coordinates.
- **Way changes**: a way is in the extract if any of its node refs are
  in the bbox. But the OSC contains way modifications with new ref lists
  — we need to check if the *new* refs resolve to nodes inside the bbox.
  This requires a node coordinate lookup (from the existing extract or
  a separate index).
- **Relation changes**: transitive closure — a relation is in the extract
  if any member (or member of member) is in the extract. Determining this
  for a modified relation requires the full member graph.

### 2. Missing context in the OSC

The OSC contains only the *changed* elements. But extract decisions
depend on context:

- A way modification in the OSC changes the way's tags but not its refs.
  To know if the way is in the extract, we need its refs — which are in
  the existing extract PBF (if the way was already there) or in the
  planet PBF (if it's new to the region).
- A relation modification requires knowing all its members. The OSC has
  the new member list, but the member elements themselves may or may not
  be in the OSC.

### 3. Strategy differences

- **Simple extract**: include elements whose coordinates/refs are in the
  bbox. Straightforward spatial filter.
- **Complete extract**: include all node refs of included ways, even if
  outside the bbox. A way change might add new refs that are outside the
  bbox — those nodes need to be fetched from somewhere.
- **Smart extract**: additionally include relations whose members are in
  the region, and all their transitive dependencies.

Complete and smart strategies need context beyond what's in the diff +
existing extract. They may require access to the planet PBF for newly
referenced elements.

## Approach 1: Diff-filter + merge on extract

```
1. Parse OSC
2. Filter to elements that are (or were) in the extract region
3. Apply filtered changes to the existing extract PBF
```

**Step 2 details:** for each element in the OSC:
- Nodes: check if old coords (from existing extract) or new coords
  (from OSC) are in bbox
- Ways: check if way ID exists in existing extract. If yes, it's
  relevant. If no, check if any new refs are nodes in the extract.
- Relations: check if relation ID exists in existing extract.

**Step 3:** this is just `apply-changes` on the small extract PBF
with the filtered OSC. Already implemented and fast.

**Limitation:** this only handles the simple strategy correctly. A
new way with refs to nodes outside the bbox would be included in
complete strategy but those nodes aren't in the extract to fetch.

### Implementation

```rust
fn incremental_extract_simple(
    extract_path: &Path,   // existing 483 MB Denmark extract
    osc_path: &Path,       // 15 MB daily diff
    bbox: Bbox,
    output_path: &Path,
) -> Result<()> {
    // 1. Parse OSC into CompactDiffOverlay
    let diff = parse_osc_file(osc_path)?;

    // 2. Build spatial filter: which OSC elements affect this bbox?
    // Scan existing extract for all element IDs (to detect deletions
    // and modifications of existing elements)
    let existing_ids = scan_extract_ids(extract_path)?;

    // 3. Filter diff entries to region-relevant changes
    let filtered = filter_diff_to_region(&diff, &existing_ids, bbox)?;

    // 4. Apply filtered diff to extract
    apply_changes(extract_path, &filtered, output_path)?;
}
```

### Missing element problem

For complete/smart: when a new way references nodes outside the bbox,
those nodes must be fetched from the planet PBF. This requires either:
- Access to the planet PBF (defeats the purpose of incremental extract)
- A planet-scale node coordinate index (maintained separately)
- Accepting that complete/smart incremental extract may miss some
  peripheral nodes

## Approach 2: OSC → regional OSC + merge

Pre-filter the planet OSC to produce a regional OSC, then apply:

```
pbfhogg osc-filter diff.osc.gz --bbox ... -o denmark-diff.osc.gz
pbfhogg apply-changes denmark.pbf denmark-diff.osc.gz -o denmark-updated.pbf
```

This is approach 1 as a two-command pipeline. The `osc-filter` command
is new but simple — parse OSC, keep entries whose coordinates/refs
overlap the bbox, write filtered OSC.

**Advantage:** separates the spatial filtering from the merge. The
`osc-filter` output is inspectable and debuggable.

**Disadvantage:** needs to determine spatial relevance from the OSC
alone (no existing extract context). For nodes, coordinates are in
the OSC. For ways, ref changes need node coordinate lookup.

## Approach 3: Osmium-style incremental extract

osmium-extract supports `--with-history` for incremental updates via
update files. The approach:

1. Extract with `-S simple` produces a region extract
2. Apply OSC to the region extract using `osmium apply-changes`
3. Re-extract from the updated region file

This works because `apply-changes` on the small extract is fast, and
the re-extract filters out elements that moved outside the region.

pbfhogg already has both `apply-changes` and `extract`. The composition
is:

```
pbfhogg apply-changes denmark.pbf diff.osc.gz -o denmark-dirty.pbf
pbfhogg extract denmark-dirty.pbf --bbox ... -o denmark-updated.pbf
```

**Problem:** `apply-changes` on the region extract would fail for
new elements that aren't in the base — OSC entries with new IDs have
no corresponding base element.

**Fix:** use `apply-changes --allow-missing` to insert new elements
that don't exist in the base. Then re-extract to filter to the bbox.

This is the simplest approach and requires minimal new code — just
the `--allow-missing` flag for apply-changes.

## Approach 4: Maintained planet-scale ID → region mapping

Maintain a persistent mapping from element ID → which regions it
belongs to. When processing a daily diff:

1. For each changed element, look up affected regions
2. Route the change to those regions' extract pipelines
3. For elements that moved between regions, update the mapping

**Advantage:** amortized O(1) per change, handles all strategies.
**Disadvantage:** the mapping is large (~11.6B entries at planet
scale) and must be maintained across updates.

This is essentially what Geofabrik does for their extract service.

## Recommendation

**Approach 3 (apply + re-extract on region)** for simplicity:
- Minimal new code (just `--allow-missing` for apply-changes)
- Uses existing, validated commands
- Works for simple strategy immediately
- The re-extract step handles all edge cases (elements that moved
  out of the region, deleted elements, etc.)
- Denmark: `apply-changes` on 483 MB takes ~5s + `extract` on the
  result takes ~5s = ~10s total vs 862s for full pipeline.

**Approach 2 (osc-filter)** is complementary — useful for producing
regional diffs for downstream consumers who want the OSC, not just
the updated extract.

Complete/smart strategies need approach 4 or planet access.

## Prerequisites

- `apply-changes --allow-missing` (or silent skip of missing base
  elements) — currently apply-changes expects every modified element
  to exist in the base
- Verify that the re-extract produces identical results to a fresh
  extract from the updated planet (within the simple strategy)
