# Negative IDs: plan (DOC-001, BUG-010 and neighbours)

This is a plan for the user's approval. Nothing has been edited, filed or
committed, and no gate has been run. The tree was read at HEAD 2b4e511c,
clean. This is version 3: it replaces the v2 plan under the user's ruling
below.

## Ruling this plan implements

pbfhogg aims to be the sane alternative to osmium, and to replace it.
- Anything osmium accepts, pbfhogg accepts and answers correctly.
- Where osmium is wrong, pbfhogg is exact. Osmium abs-folds IDs in getid,
  getparents, removeid, tags-filter, extract and the `--keep-member-nodes`
  retention.
- Stricter correctness than osmium is fine. Refusing a file that osmium
  processes is not.

Consequences:
- Negative IDs are handled like any other ID, everywhere.
- A command that mishandles one has a bug.
- The per-site inventory is evidence, not policy.

## Provenance key

Every claim carries one of these tags:

| Tag | Meaning |
|---|---|
| **[S]** | I verified it in the source. |
| **[O]** | Verified in the vendored osmium, libosmium or traccar sources or man pages. |
| **[C:x]** | Established by a codex investigation that read the source. I did not re-read it. |
| **[P]** | My position, not independently verified. |

The codex sessions behind **[C:x]**:

| Tag | Session | What it covered |
|---|---|---|
| C:spar | 01a1ed66-e357-7cb0-93dd-1ff409ae2586 | The original design spar, 3 exchanges. |
| C:inv | 01a1eda9-6333-7e91-85fd-ce06f18f66a4 | Command inventory and title check, 2 exchanges. |
| C:perf | 01a1ed86-58a0-7ea0-a1c3-a6a757fcd5a5 | Positive-input performance. |
| C:re | 01a1ed87-42f4-7760-90a6-5bbb31e358c6 | Re-examination of lightly sparred points. |
| C:altw | 01a1edcb-1d05-79b1-8c9b-47dabc172a91 | Sparse and external negative joins. |
| C:rgp | 01a1edcb-9237-7fe1-8ca8-165f22e9f061 | Renumber, geocode and prevention. |

Every row the ruling changed (ALTW, geocode, renumber) now rests on C:altw
or C:rgp. Those are fresh source reads, not carried over from v2. Line
references are as of 2b4e511c, and the implementer re-verifies each site
before changing it.

## 1. Premise

DOC-001 holds **[S]**. The blanket "rejected project-wide" claim appears in:
- the title and Decision opening of
  `decisions/0002-negative-ids-rejected-project-wide.md`;
- its index line in `decisions/README.md`;
- the heading and summary line of the `DEVIATIONS.md` section.

It is also used as a reason in places where that reason is false:
- `crates/verify/src/checks/check_ids.rs`;
- `crates/verify/src/verify_reencode.rs`;
- `reference/verify.md`;
- the `renumber/mod.rs` `validate_start_ids` doc;
- the `idset.rs` `set` doc;
- `reference/technical-implementation-spec.md`.

The ADR body itself declines blanket rejection: it turns down alternative (a)
and chooses (c).

BUG-010 stands on its own merits, through the silent-wrong-answer rule and
complete-ways' completeness claim **[S][C:inv]**. Under the ruling, its
remedy is support, not refusal.

## 2. The ADR

**Title [P, checked against every row in section 3]:** "ADR-0028: Negative IDs
are handled like any other ID". Filename:
`decisions/0028-negative-ids-are-handled-like-any-other-id.md`. 0028 is the
next free number **[S]**.

**Decision (draft):**
1. Every `i64` element ID, way ref and relation member is a valid identity.
   Each command compares, selects, joins, ranks, renumbers, orders and
   reports a negative ID by the same rules as a non-negative one:
   - canonical `id_order` for order;
   - signed numeric extrema for indexdata ranges;
   - the same metadata-trust contract (ADR-0013);
   - deliberate removals (filters, deletes, `--drop-ids`, dedupe) apply
     regardless of sign.
2. Where pbfhogg's answer differs from osmium's, the difference is that
   pbfhogg is exact. It never abs-folds, and DEVIATIONS.md records each case.
3. A command that drops, misplaces or refuses a negative ID, other than
   through the remaining exception below, has a bug.
4. Reports describe negatives in report fields, never on stderr (ADR-0016).
5. The command inventory lives in the executable test registry (section 6),
   which this ADR links. The ADR lists no sites.

**No exceptions.** The user lifted the `renumber --start-id` below 1 refusal
(Q1): pbfhogg accepts a negative start and counts down from it, as osmium does
**[O]**. The ADR has no exceptions.

Why not the earlier titles:
- "Lookups that cannot take them refuse" no longer describes anything,
  because no refusals are left.
- "A negative ID is treated like any other, or refused by name" carries a
  refusal clause that now has no sites.
- "Never dropped silently" is a property implied by the chosen title, not the
  rule itself.

Check against every row: the chosen title is true once the plan lands for
every row in section 3, including row 42 (`--start-id`) under Q1.
Metadata-trust cases are sign-neutral.

## 3. Site table: evidence, not policy

**The count [S][C:inv].** `Command` has 20 top-level commands plus the nested
`inspect tags`: 21 command paths, excluding the generated `help`.
`docs/cli/commands.md` matches. The table splits these into 46 rows of modes
and flags. This table is the plan's working evidence, and it moves into the
test registry, not into the ADR.

Plan codes: **P** means preserve (already exact); **S+** means support (fix a
silent drop or a refusal); **V** means the verdict changes. Rows are
**[C:inv]** unless tagged otherwise.

| # | Path | Today | Plan | Osmium |
|---|---|---|---|---|
| 1-3 | inspect default, `--indexed`/`--nodes`/`--get`, `--show` | Exact. `--show` takes signed queries **[S]**. | P. `has_negative_ids` report field. | fileinfo; no lookup. |
| 4-5 | inspect `--id-ranges`, `--extended` | Numeric extrema and monotonicity. | P. Label as numeric; the canonical `objects_ordered` is BUG-012. | `fileinfo -e`, canonical. |
| 6 | inspect tags | Exact. | P | tags-count |
| 7 | check (bare) | Refs phase refuses negatives. | S+ via row 10. | check-refs supports. |
| 8 | check `--ids` | Canonical, exact. | P, plus field. | CheckOrder **[O]** |
| 9 | check `--ids --full` | Negatives are `IdOutOfRange`; exit 1 **[S]**. | V. Lazy `HashSet<i64>` per kind in the serial merge. Overturns the pbfhogg#50 ruling. | Accepts ordered. |
| 10 | check `--refs`, `--check-relations` | Refuses compared negatives. | S+. Signed existence via a serial side merge; signed missing tallies; signed deferred lookup. These are not rank or slot users **[C:inv]**. | Separate sign sets **[O]** |
| 11 | check `--locations`, `--agreement` | Refuses negative refs. Unreferenced negative records get no slot, so a coordinate conflict passes clean. | S+. Negative-key ordinals in the same slot store. Cost: 8 + 8.25 bytes per distinct negative key, nothing when there are none **[C:inv]**. | No equivalent. |
| 12 | check `--index` | Signed, exact. | P | none |
| 13-15 | getid positional, `-i`, `-I` | Text refuses negatives **[S]**; `-I` drops them silently **[S]**. | S+. Signed queries from every source; `-I` negatives appended, then sorted and deduped once. | Refuses text; abs-folds `-I` **[O]**. pbfhogg is a superset and exact. |
| 16 | getid include and invert prefilters | Clamped at 0. | S+. Numeric predicate on signed extrema, `min_id <= q <= max_id`; signed kind presence; `Ways::BlobsCopied` only if `indexed && !W` **[C:re]**. | n/a |
| 17 | getid `-r` | Drops negative refs, members and seeds **[S]**. | S+. Signed `EdgeStore` children and `WaySpans`. | Abs-folds **[O]** |
| 18 | getid `--invert` | Shares the `-I` drop. | S+. Exact signed removal. | removeid abs-folds: `n7` also removes `n-7` **[O][C:re]** |
| 19 | getparents | Shares the query rules. | S+. Signed kind pruning and `ParentProbe` **[C:re]**. | Man page disclaims; source abs-folds **[O]** |
| 20 | tags-filter PBF default | Drops negative matches and dependencies **[S]**. | S+, both passes. | Abs-folds |
| 21-22 | tags-filter `-R`, OSC | Exact. | P | Direct; OSC unsupported. |
| 23-26 | extract simple, complete-ways, smart, `--config` | Drop through `IdSet`/`Membership` **[S]**. | S+. Multi-extract maps the `i64` bits as `u64` into overflow chunk keys `2^38..2^39-1`, carrying region masks **[C:re]**. | Abs-folds **[O]** |
| 27 | ALTW sparse | Negative way refs filtered (`altw/mod.rs` around 986) **[S]**. | S+. Signed join (section 4.4). | Separate negative index; joins **[O]** |
| 28 | ALTW external | `check_range` refuses a min < 0; stage 1 skips negative refs **[S]**. | S+. Two bucket families (section 4.5). | Joins |
| 29 | ALTW auto, `--force` | No sign rule. | P. Post-pass-0 exact-size recheck (section 4.4). | n/a |
| 30 | ALTW retention | Negative members not kept (`altw/mod.rs` around 112) **[S]**. | S+. Signed node and way member sets. | `--keep-member-nodes` abs-folds and unsupported **[O][C:altw]** |
| 31 | ALTW `--inject-prepass` | Negative member ways lose their bit (line 113) **[S]**. | S+. Signed membership plus signed shared-node pins. | n/a |
| 32 | ALTW `--ignore-missing-nodes` | A present `n-1` becomes `(0,0)`. | S+. Only true misses are filled, with the settled `(0,0)` placeholder. | Writes undefined raw location **[C:altw]**; see the doc gap in section 10. |
| 33 | build-geocode-index | pass1 drops negative admin ways **[S]**; pass1_5 skips negative refs. | S+ (section 4.7). | No counterpart. traccar is partial: signed member ways, but a `Dummy` negative location store, so a negative node ref fails **[O][C:rgp]**. |
| 34 | geocode-query | No ID operand. | P | n/a |
| 35-37 | diff text and OSC, digest | Canonical and exact **[S]**. | P | Canonical |
| 38-39 | apply-changes, `--locations-on-ways` | Signed and exact. | P | The location join abs-folds `±1` **[C:inv]**; pbfhogg is exact. |
| 40 | merge-changes, `--simplify` | Signed keys. | P | Canonical |
| 41 | renumber | Refuses negative sources and refs **[S]**. | S+. Signed remap (section 4.6). | Signed remap via `id_order` **[O]** |
| 42 | renumber `--start-id` | Refuses below 1 **[S]**. | S+. Negative start accepted, counting down (Q1). | Counts down from a negative start **[O]** |
| 43-46 | degrade, repack, cat (all modes), sort, time-filter, export | Sign-neutral, exact. | P | sort and export support negatives **[O]** |

## 4. Code design

### 4.1 Signed sets as the default type [landed]

`IdSet` and `RankedIdSet` are signed (`crates/lib/src/idset.rs`): two private
bitmaps over a checked `NonNegId` (non-negatives keyed by id, negatives by
`!id`), every order-dependent method named numeric or canonical
(`any_in_numeric_range`, `iter_numeric_from`, `iter_canonical`,
`numeric_rank`, `numeric_rank_if_set`, `canonical_rank_if_set`), and the
atomic-or-spill API `try_insert_atomic` / `try_insert_atomic_if_new` (a
negative or uncovered ID returns "not recorded"; the caller spills it to
`insert`). Every command that does not yet select negatives drops them with
an explicit `id >= 0` guard at its insertion site; each later commit deletes
its command's guards.

What remains of 4.1:
- A total `canonical_rank(id)` for absent IDs, needed by the orphan
  bijection, lands with commit 10.
- `check --locations` partitions its keys by a binary search of
  `numeric_rank` over `0..=i64::MAX`; commit 6 widens it to `i64::MIN..` with
  an overflow-safe midpoint once negatives are keyed.

### 4.2 Membership users

Rows 10, 13-20, 23-26, 30, 31 and 33 use the signed set. The specifics:
- prefilter predicates are as in row 16;
- the `EdgeStore` byte encoding already works for signed children
  **[C:spar]**;
- multi-extract paging is as in row 26 **[C:re]**.

### 4.3 check

- `--ids --full`: as in row 9.
- `--refs`: as in row 10.
- `--locations`: signed slot ordinals, as in row 11.
- The `has_negative_ids` field on `VerifyIdsReport` and on inspect's
  `data` **[C:re]**.

### 4.4 ALTW sparse join [C:altw]

- **Store.** A second ranked set, keyed by `!id`, maps negatives
  (`-1 -> 0`, `i64::MIN -> i64::MAX`; no signed negation). Both sets share
  one coordinate mmap of `8 * (neg_distinct + nonneg_distinct)` slots,
  computed with checked arithmetic. Writer and reader each get pre-sliced
  regions, so each side ranks from zero.
- **Pass 0.** The worker's existing sign check splits refs into per-blob
  vectors. Non-negatives take the unchanged serial `set` loop. Negatives drain
  separately.
- **Prepass and pins.** Preserve ref multiplicity until the shared-node
  decision. A trailing ring closure is excluded from the shared count and
  takes the first position's pin. A missing ref never gets a pin.
- **Population.** Parallel workers use signed rank routing and atomic stores.
  Conflicting duplicate node records need a deterministic winner, shared with
  external.
- **Auto routing.** The header estimate stays as a heuristic. After pass 0,
  recheck the exact slot bytes before allocating, and charge the negative
  rank directory against the budget. This also fixes an underestimate that
  exists today, because absent referenced IDs get slots too.
- **Positive input.** When pass 0 found no negatives, a non-negative-only
  implementation is selected outside the inner loop.

### 4.5 ALTW external join: two bucket families [C:altw]

1. **Metadata.**
   - Keep numeric signed extrema for validation.
   - Join-key intervals are per family. A negative-only blob has
     `[!max_id, !min_id]`.
   - Decode the boundary blobs, those with `min_id < 0 <= max_id`. Use
     `ClaimWalk` to check canonical order, count and extrema, and feed the
     actual first and last IDs into the watermark.
   - A blob `[0, -1]` belongs to both families.
   - `[-1]` followed by `[-2, 1]` is valid, while `[-1]` followed by
     `[0, -2, 1]` is a descent. They have the same extrema, so only a decode
     can tell them apart.
2. **Stage 1.** Emit records keyed by `(family, key)`. A wide record path
   carries keys beyond the compact codec's limit of about `2^38`. Open shards
   are charged to the descriptor and scratch plans.
3. **Stage 2.** Partition per family. Merge only the tuples of that family,
   and never cross families inside a mixed blob. With prepass on, count over
   the whole `(family, key)` run. Spill runs that are too large.
4. **Stage 4.**
   - `AscendingTally` stays for non-negative claims.
   - Signed claims are validated canonically.
   - The retention skip asks whether either signed portion of the range can
     hold a retained node.
   - `check_range`'s refusal is deleted.

### 4.6 renumber signed remap [C:rgp, O]

- **Assignment.** Live IDs are assigned by canonical rank within each kind:
  `0, -1, -2, ...`, then `1, 2, ...`. With the default start, `0, -2, -9, 2, 7`
  becomes `1..5`. This matches osmium's numbering on sorted unique input. A
  numeric rank would misdirect references.
- **Pipeline.** The parallel pipeline is kept. Freeze the signed node map,
  then the way map and the full relation map. Each member resolves through
  the map for its type. Unknown member types stay opaque.
- **Wire arithmetic.** Deltas wrap modulo `2^64`, so `i64::MIN -> i64::MAX`
  round-trips. Allocation of new IDs stays checked.
- **Orphans: replace the shift.** The current `start + count + old_id`
  collides for negative orphans. With start 1 and three live nodes, orphan -1
  becomes live node 3. It also fails near `i64::MAX` today.
  - Use a canonical ordinal over all of `i64`: `K(0) = 0`,
    `K(neg) = |id|`, `K(pos) = 2^63 + id`.
  - Live IDs occupy `[S, S + C)`.
  - An absent `x` gets `r = K(x) - rank(x)`, then maps to ordinal `r` if
    `r < S`, else to `r + C`.
  - The result is a collision-free orphan bijection that needs only the
    immutable rank index.
  - This changes the documented positive orphan names, so DEVIATIONS and the
    renumber verifier change. The verifier checks live-interval membership
    plus a per-kind orphan bijection, since osmium names orphans in encounter
    order.
- **Cost on indexed positive input.** No extra scan and no hash table. There
  is one rank lookup with a cached negative-count offset, and the
  canonical-compare fast path for two non-negatives. Extra work is confined
  to orphans.

### 4.7 build-geocode-index [C:rgp]

- **pass1.** `needed_admin_ways` becomes a signed set.
- **pass1_5.** Keep the positive atomic bitmap. A signed collector inserts
  atomically when an ID is covered, and spills otherwise. Merge after join,
  then freeze the signed rank. The skip and its warning are deleted. The
  numeric upper-bound prune stays.
- **pass2.** Signed rank into the same slots. Absent refs keep the existing
  missing treatment. The way store already zigzags signed way IDs, and admin
  geometry uses signed keys.
- There is no concrete blocker: **support**.

## 5. Performance on positive-only input

The rule from the ruling: a regression means improving the implementation,
never refusing negatives. The AGENTS.md speed rules govern the
implementation.

**Per hot path [C:perf, C:altw, C:rgp]:**

| Path | What changes on positive input | How it stays free |
|---|---|---|
| ALTW sparse pass 0 | The existing worker sign check now splits into two vectors. | The serial non-negative `set` loop is unchanged. The no-negatives implementation is chosen outside the loop. |
| ALTW sparse population and lookup | A negative region in the store. | The non-negative-only variant is selected when there are no negatives. |
| ALTW external stage 1 | `(family, key)` emit. | The compact positive record path is kept. A family branch per ref is the only addition. |
| ALTW external stages 2 and 4 | Per-family partition. | Canonical decode only for blobs straddling zero; there are none on positive input. |
| renumber | Canonical compare and cached offset. | Fast path for two non-negatives. |
| geocode pass1_5 | Collector spill branch. | Positive atomic path unchanged. |
| getid, getparents, tags-filter, extract | One sign decision per probe. | `min_id >= 0` skips the side store, which is cold. |
| check `--refs`, `--locations`, `--ids --full` | Side merge or side Vec. | Empty, so no allocation. |

**Prior evidence [C:perf].**
- `notes/altw.md`: an unconditional extra bitmap probe in the sparse serial
  union cost 6.61 against 4.30 ns/ref (Japan).
- The generic insert path costs about 1.1 ns/ref.
- `reference/performance/add-locations-to-ways.md`: per-node canonical walks
  cost material CPU in external stages 2 and 4. This is why canonical decode
  is restricted to the boundary blobs.

**Risks.**
- The signed type losing inlining in the tightest loops.
- Bulk operations losing their whole-chunk paths: union, consuming merge,
  word iteration, rank builds.
- An accidental validation scan in front of a bulk copy.

**Measurement plan.**
- Discipline:
  - same-day `--iterations 1` cells in A B B A order, with the baseline built
    via `--commit 2b4e511c`;
  - trim first and balance predecessors;
  - read the phase split before calling a regression;
  - a regression is a median more than 3 % slower with both pairs agreeing.
- Cells:
  - `embed-locations`, Japan, sparse, `--inject-prepass no --stop
    ALTW_PASS0_END`, compared on ns/ref (`altw_pass0_union_ms /
    altw_pass0_refs_total`);
  - Japan sparse, full run, with prepass off and on as separate pairs (store
    bytes, peak anon, pass 1 and 2 CPU);
  - Germany external, full run (`s1a_id_emit_ms` per record, shard bytes,
    stage 2 and 4 CPU);
  - a planet pair of pairs for sparse ALTW;
  - `renumber`, Germany;
  - `region-extract`, Japan multi simple 5 regions, and Germany single smart;
  - `tag-select`, Germany, `r/type=route`;
  - `parent-select` and `id-select --add-referenced yes`, Germany;
  - `geocode-build`, Germany, `--stop GEOCODE_PASS1_5_END`;
  - `id-order --full yes`, Denmark `--snapshot unsorted`.
- About 13 regional cells plus the planet pair: over the threshold of about
  ten, so Q2 asks for go-ahead.
- Record every cell, with its verdict, under `reference/performance/`.

## 6. Prevention: getting future commands right without anyone remembering [C:rgp]

1. **The type (primary).**
   - Signed `IdSet`/`RankedIdSet` is the only public set.
   - The unsigned rank and atomic backend takes `NonNegId`, and only the
     signed type calls it.
   - It catches every future membership user. It misses code that sorts,
     filters or casts IDs outside the set API.
2. **The test registry (where the inventory lives).**
   - A CLI matrix test derives command paths from `pbfhogg_cli::cli_command()`
     and fails if any path lacks an entry. An entry is a mixed-sign case or a
     reasoned "no ID input" mark.
   - Each entry names its ID-sensitive modes, so the 46 rows become test
     entries.
   - Cases run on a shared mixed-sign fixture graph with edges both ways
     across zero: same-blob and cross-blob, indexed and unindexed.
   - Each case compares against a **relabelled positive twin** of the same
     graph. The twin uses an order-preserving per-kind bijection, because an
     arithmetic shift can change canonical order or overflow.
   - Selected edges are also asserted independently, since a differential
     test passes when both arms share a bug.
   - The graph and its twin exist: `crates/lib/tests/common/mixed_sign.rs`
     (`mixed_sign_graph`, `relabelled_twin`, `Relabel`, `blob_ids`), with
     its structure pinned in `crates/lib/tests/fixture_helpers.rs`.
   - This catches a new command that was never thought about, and a
     regression in an old one. It misses fast paths a small fixture never
     reaches, so the fixture must reach each strategy.
3. **A source-policy rule (backstop).**
   - Flag sign filters (`id >= 0`, `id < 0`, reversed forms), `as u64` casts
     and `.abs()` used as an identity on recognisable OSM ID expressions in
     `crates/lib/src`.
   - Narrow, reasoned exceptions: canonical ordering, wire encoding, checked
     conversions, family routing.
   - It needs its own fixtures. It is token based, with no type information,
     so it is a review backstop and not proof.

**Where the inventory lives.**
- **Tests:** the registry above. It cannot drift, because a missing or stale
  entry fails.
- **ADR-0028:** the rule only, with a link to the registry.
- **DEVIATIONS.md:** only the observable differences from osmium.
- **docs/cli/commands.md:** observable behaviour per command, where it is
  user-relevant (renumber orphans, report fields).

This answers the drift critique.

## 7. Durable documents

**ADR procedure [S].** `decisions/README.md` says a superseded ADR stays in
place with `Status: Superseded by NNNN`, and the new one says
`Supersedes NNNN`. The precedents are 0005 to 0021 and 0015 to 0022.

**ADR-0028.**
- The rule as in section 2, with a link to the registry.
- Alternatives considered:
  - wording-only;
  - refuse at the silent drops;
  - refuse everywhere with renumber as the remedy;
  - per-operand refusal at unsigned indexes, the earlier plan;
  - osmium's abs-fold.

  Each records the reason it lost. The parity ruling is the deciding reason
  for the three refusal shapes.

**ADR-0002.** Status `Superseded by 0028`; the body is untouched.

**decisions/README.md.** Index entries for both.

**DEVIATIONS.md.**
- Replace the section with "Negative IDs: pbfhogg is exact where osmium
  abs-folds", listing:
  - getid, its `--invert` (removeid) mode and getparents, which match the
    exact signed ID, and accept negative text queries where osmium refuses
    them;
  - tags-filter and extract;
  - ALTW `--keep-member-nodes`-style retention;
  - the apply-changes location join;
  - `check --ids --full` (no difference left);
  - renumber orphan naming (by bijection, not encounter order).

  A negative `--start-id` now matches osmium, so it is not a deviation; the
  existing start-ID paragraph is rewritten to say so.
- Delete the residual-divergence, migration-path and "renumber first" text.
- Fix the derive-changes paragraph (section 9).
- If verified, record the `--ignore-missing-nodes` fill-value difference in
  the add-locations-to-ways section (section 10).
- Update the renumber orphan section for the new mapping.
- Fix the two inbound anchors.

**reference/.**
- `verify.md`: check-ids, the renumber verifier contract, and the ALTW
  verify path. That path currently runs `check --locations --agreement`,
  which refuses negatives; after row 11 it is signed **[C:altw]**.
- `technical-implementation-spec.md`: replace the example.
- `testing.md`: describe the negative-ID registry and the fixture graph, and
  link 0028.
- `indexdata-trust.md`: reword it.

**docs/cli/commands.md.**
- check: the `--ids --full`, `--refs` and `--locations` rows, plus the report
  fields.
- getid and getparents: negative text queries.
- renumber: negative inputs and orphan naming.
- ALTW and geocode-build.
- inspect: the numeric monotonic wording.

**Doc comments and messages.**
- `idset.rs`, rewritten with the type.
- `renumber/mod.rs`, `wire_rewrite.rs` and `relations.rs`.
- `extract/membership.rs`.
- `getid/mod.rs`.
- `blob_meta/mod.rs` `BlobIndex::deserialize`.
- `altw/external/blob_meta.rs`: the "use sparse" error disappears.
- The geocode warning.
- `cli_negative_id_invariants.rs`, which becomes part of the registry or is
  replaced by it.
- `renumber_external.rs`.
- In `crates/verify`, the `check_ids.rs` preflight is removed and a
  mixed-sign fixture added. `verify_reencode.rs` keeps its refusal with its
  true reason, which is the degrade layout oracle. Its sign handling is a
  harness question, not a pbfhogg one.

Durable text never cites a finding ID.

## 8. CHANGELOG

- **Negative IDs are now handled like any other ID.** getid (including
  `--invert`), getparents, tags-filter and extract select them exactly,
  where before they dropped them silently.
- getid and getparents accept negative query IDs.
- add-locations-to-ways joins negative node refs in both index types, and
  retains and marks negative relation members. The external index no longer
  refuses node blobs below zero.
- build-geocode-index uses negative admin ways and node refs.
- renumber accepts negative IDs, and a `--start-id` below 1, counting down
  from it as osmium does.
  - **Behaviour change:** orphan references are named by a new
    collision-free mapping.
- check:
  - `--ids --full` no longer fails ordered files holding negatives.
    **Breaking:** the JSON `IdOutOfRange` variant is removed.
  - `--refs` and `--locations` check negatives, where before they refused
    them.
  - `--locations` now reports coordinate conflicts on negative nodes.
- New `has_negative_ids` report fields in check and inspect.

The old 0.3.x entry stays as history. Whether its anchor is checked by
`scripts/check_doc_links.py` is still to be established.

## 9. Re-examined points (unchanged from v2)

- **derive-changes, "Osmium's own gap" [C:re, O].**
  - The DEVIATIONS claim is wrong: signed equality is correct.
  - The real osmium bug is a missing type comparison: old `[n7 v1]` against
    new `[w7 v1]` loses the node deletion.
  - This is not a negative-ID matter. Drop it or rewrite it in the docs
    commit.
- **`negative_id_fallback` test names [C:re].** They are stale: the tests
  check input pinning on mixed-sign fixtures. Rename them.
- **getparents and removeid against osmium [O][C:re].** Osmium refuses text
  negatives and abs-folds the rest, while pbfhogg matches exactly. Both go in
  DEVIATIONS.

## 10. Separate items

These are not bundled, but follow-ups should cite ADR-0028:
- BUG-012: inspect canonical order.
- CMD-020: tags-filter history versions.
- Metadata-trust gaps (sign-neutral).

**Input differences from osmium surfaced while designing [C:altw][C:rgp].**
User ruling: these are not new bugs and are not filed.
- The indexdata and sorted-input requirements (renumber; external ALTW) are
  deliberate design, documented in the "Proceeding without indexdata" table
  of `docs/cli/commands.md`, with `cat` as the named remedy. "Replace osmium"
  does not mean "accept every input without a preparation step".
- Both ALTW reframers refusing non-default way framing is already filed as
  CMD-014 in `notes/bugs-commands.md`.
- The public library `external_join` publishing without the
  missing-location gate is already filed as BUG-008 in `notes/bugs.md`.
- The `--ignore-missing-nodes` fill value is settled policy: failure by
  default, `(0, 0)` as an explicit opt-in placeholder (`CORRECTNESS.md`
  "Null Island ambiguity in coordinate indexes"; `DEVIATIONS.md`
  "add-locations-to-ways: missing nodes are an error" and the apply-changes
  section). Not a question.

**Potential doc gap (unverified).** C:altw reports that osmium's
add-locations-to-ways writes libosmium's undefined location, not `(0, 0)`,
under `--ignore-missing-nodes`. The `DEVIATIONS.md` add-locations-to-ways
section says pbfhogg's behaviour is "The same", while the apply-changes section
already records the same fill-value difference explicitly ("pbfhogg's own
placeholder, not osmium's output"). If the osmium claim holds in
`research/libosmium` and `research/osmium-tool`, the add-locations-to-ways
section should record the fill-value difference the same way. Verify first;
this is a wording fix that can ride in the docs commit, not a decision.

## 11. User decisions

All questions are settled (2026-11-18):

- **Q1. `renumber --start-id` below 1: lifted.** Accept a negative start and
  count down from it, as osmium does **[O]**: normalise 0 to 1 as osmium
  does, use dense delta -1, and check the last assigned ID (C:rgp design).
- **Q2. Measurement budget: approved.** About 13 regional cells plus a
  sparse-ALTW planet pair of pairs (section 5).
- **Q3, Q4. Withdrawn;** settled by the user (section 10).
- **Q5. The JSON `IdOutOfRange` variant is removed** (pre-1.0; breaking
  CHANGELOG entry).
- **Q6. Renumber orphan naming changes, positive orphans included:
  accepted.** The current shift collides near `i64::MAX` today.

## 12. Landable commits

Each commit carries its tests, registry entries, docs bullets and CHANGELOG
lines. It passes `brokkr check` and the relevant Denmark verifies.

2. **The negative-ID test registry, wired to `cli_command()`.** Rows
   already exact are pinned, and every row a later commit fixes carries a
   marked expected failure, which that commit flips.
3. getid and getparents.
4. tags-filter.
5. extract, including multi-extract paging.
6. check: `--ids --full`, `--refs`, `--locations`, and the report fields
   (with inspect's).
7. ALTW sparse signed join, signed retention, prepass and pins, and the auto
   recheck.
8. ALTW external two-family join, with `check_range` removed.
9. build-geocode-index.
10. renumber signed remap and the new orphan mapping, plus the negative
    `--start-id` (Q1).
11. The source-policy rule.
12. ADR-0028, ADR-0002 marked superseded, the README index, the DEVIATIONS
    rewrite (including derive-changes), reference/, docs/, the remaining
    comments, verify messages and test renames.

Run the measurement cells after commits 3 to 10, before commit 12. A
regression is fixed in the implementation before the policy is recorded.

## 13. Lateral findings

- `check --locations` passes clean on conflicting unreferenced negative
  records **[C:inv]**.
- The renumber orphan shift collides near `i64::MAX` on positive input
  **[C:rgp]**. This is a bug today.
- The sparse auto estimate `8 * node_count` underestimates the store, because
  absent referenced IDs also get slots **[C:altw]**.
- The ALTW external error recommends sparse, which also cannot join
  negatives **[C:re]**.
- The possible add-locations-to-ways fill-value doc gap in section 10.
- traccar fails on negative node refs: it uses a `Dummy` negative store
  **[O][C:rgp]**.
- The DEVIATIONS derive-changes paragraph names the wrong osmium bug.
- Stale "untrackable" text in verify and `reference/verify.md` **[S]**.
- Stale `negative_id_fallback` test names.
- DEVIATIONS uses "project-wide" in three incompatible senses **[S]**.
- `merge-changes --simplify` orders action groups numerically **[C:inv]**.
- The check-inspect hunter cited a nonexistent ADR filename.
- "22 commands" against the 21 command paths the tree has.
