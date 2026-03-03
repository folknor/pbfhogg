# Consolidation Review #5: Dependency-Closure Planner

## Verdict: DO NOT DO

## Per-Command Analysis

### A. `getid --add-referenced`

**Seed criteria:** Explicit ID list (`BTreeSet<i64>` for nodes, ways, relations) parsed from CLI args or file.

**Expansion rules:** Way refs to nodes only. If `--add-referenced` is set and `way_ids` is non-empty, Pass 1 scans way blobs to find ways whose ID is in `ids.way_ids`, then collects all `w.refs()` into `dep_node_ids: BTreeSet<i64>`.

**Passes:**
- Without `--add-referenced`: 1 pass (straight filter).
- With `--add-referenced`: 2 passes. Pass 1 scans ways-only blobs, collects `dep_node_ids`. Pass 2 writes matching elements plus dependent nodes.

**ID set structure:** `BTreeSet<i64>` for all sets. Does NOT use `IdSetDense`. Input is explicit, small ID list from user.

**Closure depth:** Single-level only. No relation expansion. No transitive closure.

**Lines of closure logic:** ~25 lines.

### B. `tags_filter` two-pass

**Seed criteria:** Tag expression matching (OR semantics). `TagMatcher` variants: `KeyOnly`, `KeyPrefix`, `ExactValue`, `MultiValue`, `NotValue`. Each with `TypeFilter`.

**Expansion rules:** Matched ways expand to their node refs. Relations included only if they directly match a tag expression (no member expansion).

**Passes:**
- With `-R` (omit-referenced): 1 pass.
- Without `-R` (default): 2 passes. Pass 1 collects into 4 `IdSetDense` sets. Pass 2 writes from sets.

**ID set structure:** `IdSetDense` for all four sets. Appropriate for planet-scale tag matching.

**Closure depth:** Single-level. No relation member expansion.

**Lines of closure logic:** ~50 lines.

### C. `extract --complete-ways`

**Seed criteria:** Spatial containment (bbox or polygon region test on node coordinates).

**Expansion rules:**
1. Nodes in region -> `bbox_node_ids`
2. Ways with at least one ref in `bbox_node_ids` -> `matched_way_ids`, and ALL refs -> `all_way_node_ids`
3. Relations with at least one matched node or way member -> `matched_relation_ids`

**Passes:** 2 passes (`collect_pass1` + write pass).

**ID set structure:** Four `IdSetDense` sets.

**Closure depth:** Single-level with relation matching but no relation member expansion.

**Lines of closure logic:** ~35 lines.

### D. `extract --smart`

**Seed criteria:** Same as complete-ways (spatial containment).

**Expansion rules:** Same as complete-ways, PLUS:
1. For matched relations with `type=multipolygon` or `type=boundary`: all way members -> `extra_way_ids`, all node members -> `extra_node_ids`.
2. Pass 2 resolves `extra_way_ids` -> their node refs also -> `extra_node_ids`.

**Passes:** 3 passes.
- Pass 1 (`collect_pass1_smart`): Complete-ways logic + extra ID collection from smart relations.
- Pass 2: Scans ways to expand `extra_way_ids` into `extra_node_ids`.
- Pass 3: Writes all matching elements using 6 ID sets.

**ID set structure:** Six `IdSetDense` sets.

**Closure depth:** Two-level. Relations -> ways+nodes, then extra ways -> their nodes.

**Lines of closure logic:** ~55 lines (pass 1) + ~15 lines (pass 2).

## Similarity Comparison

### What is genuinely shared

All three commands follow a pattern:
1. Scan file, apply predicate per element type, collect IDs into sets
2. Matched ways -> collect their node refs into a separate set
3. Write elements whose IDs appear in collected sets

The "matched ways expand to node refs" code is structurally identical:

```rust
// In tags_filter Pass 1:
Element::Way(w) if [predicate] => {
    matched_way_ids.set(w.id());
    for r in w.refs() {
        way_dep_node_ids.set(r);
    }
}

// In extract collect_pass1:
Element::Way(w) if w.refs().any(|r| bbox_node_ids.get(r)) => {
    matched_way_ids.set(w.id());
    for r in w.refs() {
        all_way_node_ids.set(r);
    }
}
```

### What is fundamentally different

1. **ID set types:** getid uses `BTreeSet<i64>` (small explicit lists). tags-filter and extract use `IdSetDense` (planet-scale bitsets). A shared abstraction would need to be generic over the ID set type.

2. **Seed predicates:** Completely different per command (ID membership, tag expression matching, spatial containment). Zero overlap.

3. **Element classification in Pass 1:** Extract processes nodes, ways, AND relations in the same pass, with ways depending on the node set built earlier (works because sorted PBFs have nodes before ways). Different enough that a unified "classify pass" would need to parameterize which element types to process.

4. **Relation handling:** Extract checks relation members against matched sets. Tags-filter matches by own tags. Getid matches by explicit ID. Smart adds two-level expansion. Genuinely different semantics.

5. **Write pass ID checking:** Each command checks different combinations of sets with different stat tracking.

6. **Blob filtering:** Extract uses spatial, tags-filter uses tag-key-aware, getid uses type-based. All different.

## What a DependencyClosurePlan Would Look Like

```rust
struct DependencyClosurePlan {
    node_predicate: Box<dyn Fn(&DenseNode) -> bool>,
    way_predicate: Box<dyn Fn(&Way) -> bool>,
    relation_predicate: Box<dyn Fn(&Relation) -> bool>,
    expand_way_refs: bool,
    expand_smart_relations: bool,
    blob_filter: Option<BlobFilter>,
}
```

### Problems

1. **Dynamic dispatch in hot loops.** Predicates run on every element. Extract's bbox check benefits heavily from inlining. Boxing introduces virtual dispatch in the innermost loop.

2. **"Expand" flags don't capture actual differences.** Extract's way predicate depends on `bbox_node_ids` being built during the same pass (data dependency on earlier elements). Tags-filter's way predicate is independent.

3. **Stat tracking is command-specific.** Planner would need stat callbacks.

4. **Write pass is NOT unified.** Even with a shared Pass 1, each command still needs its own write pass.

5. **getid uses BTreeSet, not IdSetDense.** Forcing IdSetDense for a handful of IDs is wasteful.

## Quantitative Assessment

| Command | Closure logic lines |
|---------|-------------------|
| getid | ~25 |
| tags-filter | ~50 |
| extract complete | ~35 |
| extract smart | ~70 |
| **Total** | **~180** |

The structurally identical part (iterate way refs, insert into set) is ~4 lines per command, so **~16 lines of true duplication**. The rest is command-specific predicate logic, ID set management, and stat tracking.

The commands are already well-factored:
- `IdSetDense` is shared (in `id_set_dense.rs`)
- Parallel write infrastructure is shared in `mod.rs`
- `dense_node_metadata` and `element_metadata` helpers are shared
- `BlockBuilder` and `OwnedBlock` are shared

## Recommendation: DO NOT DO

1. **The actual duplicated code is minimal** (~16 lines of truly identical logic).

2. **An abstraction would be worse than the status quo.** Dynamic dispatch in hot loops, parameterizing over ID set types, command-specific stat callbacks, still can't unify write passes.

3. **The commands are already well-factored.** Shared infrastructure is already extracted. What remains per-command is genuinely command-specific.

4. **Performance risk is real.** +2% to +10% regression from dynamic rule dispatch. At planet scale, even 2% is unwelcome.

5. **Maintenance risk is low without the abstraction.** Closure patterns are stable, haven't changed since initial implementation.

6. **Extract --smart is genuinely different.** 3-pass with transitive closure and 6 ID sets can't be cleanly parameterized alongside 2-pass single-expansion patterns.

**The proposal conflates structural similarity (all do multi-pass ID collection) with implementational duplication (sharing actual code). These are not the same thing.** The multi-pass pattern is a design pattern, not duplicated code.
