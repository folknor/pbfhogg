I found seven defects by inspection. I changed no files and ran no builds or tests.

1. **[P1] `all` stops at `degrade -l` whenever a locations base is configured.**  
   The suite runs ordinary degrade, then locations degrade ([all.rs:94](/home/folk/Programs/pbfhogg/crates/verify/src/all.rs:94)). Both call `run_on`, which unconditionally requests `checks/degrade` ([degrade.rs:144](/home/folk/Programs/pbfhogg/crates/verify/src/checks/degrade.rs:144)). The second request fails because that directory already exists. The suite prints `ERROR`, exits 2, and never reaches its remaining checks.

   This breaks the binding contract that both entries execute when their prerequisites exist. Directory identity needs to distinguish the two checks.

2. **[P2] Apply-changes verification reports documented precedence differences as defects.**  
   [apply_changes.rs:390](/home/folk/Programs/pbfhogg/crates/verify/src/checks/apply_changes.rs:390) tolerates only osmium-only objects finally deleted by the OSC. Every differing shared record and every pbfhogg-only object fails.

   Consider a base containing `n1` at version 10 and an OSC containing one modification of `n1` at version 9 with different coordinates. This is accepted under pbfhogg's documented rule: the version floor covers earlier changes, not the base. pbfhogg applies the modification; osmium retains version 10. Verify incorrectly fails the correct result. Equal-version changes with differing timestamps cause the same problem.

   The broken claim is parity accounting for deliberate deviations. The delete-only exception in `reference/verify.md` is itself wrong against [DEVIATIONS.md:188](/home/folk/Programs/pbfhogg/DEVIATIONS.md:188). The same assumption also affects the osmium roundtrips in [derive_changes.rs:138](/home/folk/Programs/pbfhogg/crates/verify/src/checks/derive_changes.rs:138). These cases need an independent precedence model, rather than unconditional equality with osmium.

3. **[P2] Extract compares different regions for accepted bbox spellings.**  
   [extract.rs:248](/home/folk/Programs/pbfhogg/crates/verify/src/checks/extract.rs:248) passes the original bbox text directly to osmium. However, [DEVIATIONS.md:392](/home/folk/Programs/pbfhogg/DEVIATIONS.md:392) explicitly records different exponent quantization: `0.000000001e2` becomes coordinate column 1 in pbfhogg and column 0 in libosmium.

   With `--bbox=0.000000001e2,-1,1,1`, a node at longitude zero is correctly excluded by pbfhogg and included by osmium. Verify reports a membership failure.

   This breaks the claim that extract compares membership in the same region. The verifier must translate the intended integer edges into an equivalent reference bbox, or refuse the comparison domain explicitly.

4. **[P2] Sort verification rejects correctly preserved repeated IDs.**  
   Sort promises to keep every element ([blob_merge.rs:1](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:1)). An indexed history input containing `n1 v1` and `n1 v2` can therefore be correctly sorted by both tools while retaining both records.

   Nevertheless, [sort.rs:232](/home/folk/Programs/pbfhogg/crates/verify/src/checks/sort.rs:232) uses the shared comparator, whose ordering key contains only kind and ID. Its strict advancement rejects the second `n1` ([opl_compare.rs:307](/home/folk/Programs/pbfhogg/crates/verify/src/opl_compare.rs:307)). The separate output-order check rejects it too.

   This breaks sort parity by imposing uniqueness that sorting does not promise. The verifier documentation's strict-uniqueness requirement is wrong for this command; ordering and uniqueness need separate judgments.

5. **[P2] A settled reference environment failure can poison later checks.**  
   Reference staging names contain RUNID and the reusable `ToolRun.id`, without a launch identifier ([reference_cache.rs:422](/home/folk/Programs/pbfhogg/crates/verify/src/reference_cache.rs:422)). If a reference descendant outlives its leader, lifecycle settlement correctly returns `Environment`. But `result?` then leaves staging behind ([reference_cache.rs:467](/home/folk/Programs/pbfhogg/crates/verify/src/reference_cache.rs:467)).

   The suite continues as promised. Its next use of that run ID, such as `opl-compare.1`, fails `create_dir` with `AlreadyExists`, turning a recoverable environment outcome into a fatal verifier error.

   This breaks the explicit promise to continue past environment outcomes. Launch-unique staging would avoid coupling later checks to earlier failures.

6. **[P2] Geocode can manufacture missing locations in its reference model.**  
   [geocode.rs:747](/home/folk/Programs/pbfhogg/crates/verify/src/checks/geocode.rs:747) filters the original input, then immediately feeds the resulting order to osmium add-locations-to-ways. It never sorts that reference input.

   For a valid indexed file with a named highway way before its node blobs, pbfhogg's separate builder passes can resolve those nodes. Osmium's location handler encounters the way first, however, and `--ignore-missing-nodes` leaves its coordinates absent. The verifier then rejects the model as containing an unlocated street way ([geocode.rs:530](/home/folk/Programs/pbfhogg/crates/verify/src/checks/geocode.rs:530)), although the source contains every required node.

   This breaks the geocode check's claim to model the input's geometry. The missing locations arise in reference construction, not in the input or pbfhogg output.

7. **[P2] Unsupported comparison domains are mislabeled as pbfhogg failures.**  
   The contract explicitly assigns unsupported comparison domains to `ERROR`, exit 2 ([verify.md:295](/home/folk/Programs/pbfhogg/reference/verify.md:295)). But the independent blob decoder constructs `Error::Failed` for all decoding refusals ([pbf_frames.rs:503](/home/folk/Programs/pbfhogg/crates/verify/src/pbf_frames.rs:503)).

   For example, sorting a claimless indexed `LocationsOnWays` input with zstd data can succeed correctly, then fail the verifier's planner prediction because its decoder supports only raw and zlib. That becomes `FAIL`, exit 1, attributing the verifier's limitation to pbfhogg. Repeated scalar IDs and wrapping dense deltas encounter the same classification problem despite being explicitly documented comparison-domain exclusions.

One additional coverage limitation: [sort.rs:205](/home/folk/Programs/pbfhogg/crates/verify/src/checks/sort.rs:205) never supplies `--force`. Consequently, selecting an unindexed raw input exercises the CLI's indexdata refusal rather than sort's supported fallback.
