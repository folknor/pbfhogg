//! Verify: cat --dedupe - `pbfhogg cat --dedupe` held to the input itself.
//!
//! osmium merge is not an oracle here: it orders equal (type, ID, version)
//! copies by timestamp and writes the last, where pbfhogg keeps the earliest
//! input's copy and never reads the timestamp, and it refuses a repeated ID
//! and version inside one input. The check therefore works in a domain where
//! the survivor rule cannot matter: every merged input holds only copies of
//! the input's own objects, as osmium writes them, so any two copies of one
//! (type, ID, version) decode to the same OPL record, and the answer is the
//! input itself.
//!
//! On input claiming `Sort.Type_then_ID`, which must also be in strict kind
//! and ID order as osmium decodes it (a repeated ID or a disorder is outside
//! the domain, a verifier error), every run below must be identical to the
//! input as osmium decodes it, metadata always, inline coordinates when both
//! files declare them (`Policy::between`), and must claim the sort order:
//!
//! - `dedupe(A, A)`: every object a duplicate.
//! - `dedupe(A, B)`, B = `osmium tags-filter -R A nwr/amenity`: duplicate
//!   elimination where B overlaps A.
//! - `dedupe(B, C)` and `dedupe(C, B)`, C the same filter with
//!   `--invert-match`: a disjoint union, each operand supplying objects the
//!   other lacks. Both must hold an object, or the case is unexercised (a
//!   verifier error).
//!
//! B and C carry no indexdata (osmium writes none), so every run naming them
//! passes `--force` and takes the unindexed scan. Which planner paths ran
//! (passthrough, overlap runs) is not established: osmium packs a sparse
//! selection into blobs whose ID ranges can span many of A's.
//!
//! Input without the claim is held to dedupe's Unsorted refusal of
//! `dedupe(A, A)` and nothing else runs. `--force` is passed on unindexed
//! input, whose refusal would otherwise be the missing indexdata.

use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict, WayLocations, listing_beside};
use crate::opl_compare::{self, Policy};
use crate::pbf_frames::{self, Framing};
use crate::reference::ToolRun;
use crate::verify_input_order::{self, Refusal};

/// The selection splitting the input into two derived operands.
const SPLIT: &str = "nwr/amenity";

pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("cat-dedupe")?;
    harness.msg("=== verify cat --dedupe ===");
    let header = verify_input_order::input_header(harness, pbf, &outdir)?;
    let unindexed = pbf_frames::framing(pbf)? == Framing::Unindexed;

    if !header.optional_sorted() {
        let argv = PbfArgv::new("cat")
            .arg("--dedupe")
            .input(pbf)
            .input(pbf)
            .output(&outdir.join("refused.osm.pbf"))
            .arg_if(unindexed, "--force")
            .arg_if(direct_io, "--direct-io");
        verify_input_order::expect_unsorted_refusal(
            harness,
            argv,
            &Refusal {
                label: "cat --dedupe",
                operand: pbf,
                answers: &[0],
                sink: None,
            },
        )?;
        return Ok(Some(verify_input_order::refusal_claim(
            "pbfhogg cat --dedupe",
            ", merging the input with itself",
            direct_io,
        )));
    }

    let (ordered, counts) = opl_compare::order(harness, pbf, &outdir.join("input.order.opl"))?;
    if !ordered {
        return Err(Error::tool(
            "the input claims Sort.Type_then_ID but osmium does not decode it in strict kind and \
             ID order (a repeated ID or a disorder), outside the domain this check judges",
        ));
    }
    let locations = header.optional_feature("LocationsOnWays");

    // Every blob of a self-merge overlaps its twin, so every way blob is
    // re-encoded without enrichments.
    let self_ways = if counts[1] > 0 {
        WayLocations::Dropped
    } else {
        WayLocations::Kept
    };
    merge(
        harness,
        &outdir,
        "self",
        pbf,
        &[pbf, pbf],
        unindexed,
        direct_io,
        self_ways,
    )?;

    let selected = derive(harness, &outdir, pbf, "selected", false, locations)?;
    let rest = derive(harness, &outdir, pbf, "rest", true, locations)?;
    let cases: [(&str, [&Path; 2]); 3] = [
        ("input+selected", [pbf, &selected]),
        ("selected+rest", [&selected, &rest]),
        ("rest+selected", [&rest, &selected]),
    ];
    for (label, inputs) in cases {
        merge(
            harness,
            &outdir,
            label,
            pbf,
            &inputs,
            true,
            direct_io,
            WayLocations::KeptOrReported,
        )?;
    }

    Ok(Some(format!(
        "identical to the input as osmium decodes it (metadata included): merged with itself, \
         with its {SPLIT} selection, and as the disjoint union of that selection and its \
         complement in both orders; the survivor rule among differing copies and which planner \
         paths ran are not established"
    )))
}

/// `osmium tags-filter -R` of `pbf` by [`SPLIT`], inverted when `invert`,
/// held to carry the sort claim (osmium-sorted when it does not), in strict
/// order and holding at least one object.
fn derive(
    harness: &mut Harness,
    outdir: &Path,
    pbf: &Path,
    name: &str,
    invert: bool,
    locations: bool,
) -> Result<PathBuf, Error> {
    harness.msg(&format!("--- deriving {name} ---"));
    let filtered = outdir.join(format!("{name}-filtered.osm.pbf"));
    let mut run = ToolRun::osmium("cat-dedupe-split.1")
        .arg("tags-filter")
        .input(pbf)
        .args(&[SPLIT, "-R"]);
    if invert {
        run = run.arg("--invert-match");
    }
    if locations {
        run = run.args(&["-f", "pbf,locations_on_ways=true"]);
    }
    let captured = harness.reference(
        &run.arg("-o").output(&filtered).arg("--overwrite"),
        &outdir.join(format!("{name}-filtered.stdout")),
    )?;
    harness.require_success(&captured, "osmium tags-filter")?;
    if locations {
        crate::checks::sort::require_declared_locations(&filtered, "osmium tags-filter")?;
    }
    let operand = if pbf_frames::header(&filtered)?.optional_sorted() {
        filtered
    } else {
        let dir = outdir.join(format!("{name}-sorted"));
        std::fs::create_dir_all(&dir)?;
        let sorted = dir.join(crate::checks::sort::OSMIUM_SORTED);
        crate::checks::sort::osmium_sort(harness, &filtered, &sorted, &dir.join("osmium.sink"))?;
        sorted
    };
    let (ordered, counts) =
        opl_compare::order(harness, &operand, &operand.with_extension("order.opl"))?;
    harness.msg(&format!(
        "  {name}: {} nodes, {} ways, {} relations",
        counts[0], counts[1], counts[2]
    ));
    if !ordered {
        return Err(Error::tool(format!(
            "the derived operand {} is not in strict order, so it cannot stand for a sorted \
             subset of the input",
            operand.display()
        )));
    }
    if counts == [0; 3] {
        return Err(Error::tool(format!(
            "the derived operand {name} holds no object, so the disjoint union is unexercised"
        )));
    }
    Ok(operand)
}

/// One `pbfhogg cat --dedupe` of `inputs`, judged against `pbf`.
#[allow(clippy::too_many_arguments)]
fn merge(
    harness: &mut Harness,
    outdir: &Path,
    label: &str,
    pbf: &Path,
    inputs: &[&Path],
    force: bool,
    direct_io: bool,
    expect: WayLocations,
) -> Result<(), Error> {
    harness.msg(&format!("--- cat --dedupe {label} ---"));
    let out = outdir.join(format!("pbfhogg-{label}.osm.pbf"));
    let mut argv = PbfArgv::new("cat").arg("--dedupe");
    for input in inputs {
        argv = argv.input(input);
    }
    let argv = argv
        .output(&out)
        .arg_if(force, "--force")
        .arg_if(direct_io, "--direct-io");
    let captured = harness.pbfhogg_discard(argv)?;
    let name = format!("pbfhogg cat --dedupe {label}");
    harness.require_success(&captured, &name)?;
    harness.judge_way_locations(&name, pbf, &out, &captured, expect)?;
    let claims = harness.has_sort_flag(&out)?;
    harness.msg(&format!(
        "  {name}: header {} Sort.Type_then_ID {}",
        if claims { "claims" } else { "does not claim" },
        if claims { "PASS" } else { "FAIL" }
    ));
    harness.record(
        &format!("{name} output claims Sort.Type_then_ID"),
        Verdict::from_pass(claims),
    );
    let policy = Policy::between(&out, pbf)?;
    let diff = harness.same_elements(&out, pbf, &listing_beside(&out), policy)?;
    harness.msg(&format!(
        "  {name}: {}",
        if diff.is_pass() {
            "PASS (identical to the input)"
        } else {
            "FAIL (differs from the input)"
        }
    ));
    harness.record(&format!("{name} vs input elements"), diff);
    Ok(())
}
