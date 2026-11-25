//! Verify: tags-filter - pbfhogg tags-filter vs osmium tags-filter, with
//! omitted references (`-R`) and in the default, reference-including mode.
//!
//! Sign domain. osmium's default mode records every matched and referenced
//! way and relation ID, way node ref and member ref by its absolute value,
//! and pbfhogg selects by the signed one (pbfhogg's DEVIATIONS.md,
//! "tags-filter is exact"); `-R` consults no ID set and is exact in both.
//! So the `-R` arms run on any input, and each default arm reads osmium's
//! output for a fold witness before anything of pbfhogg's is judged
//! ([`crate::verify_signs::refuse_folds`]), refusing the comparison as the
//! verifier's error.
//!
//! Selection by ID. pbfhogg's default mode refuses a history file, and an
//! undeclared repeated ID whose records disagree about the expressions,
//! where osmium writes every record of a matched ID. A history header is
//! refused before the first default arm; a pbfhogg run refusing repeats is a
//! verifier error when osmium fileinfo finds a repeated ID in the input,
//! and a FAIL otherwise.

use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::Harness;
use crate::pbf_frames::{self, Framing};
use crate::reference::{Origin, ToolRun};
use crate::verify_input_order::{self, CLAIM_UNSORTED, InputOrder, compare_outputs};
use crate::verify_signs::{self, Witnesses};

/// One comparison: pbfhogg's flags, osmium's flags, the expression and the
/// output label.
struct Case {
    pbfhogg_flag: Option<&'static str>,
    osmium_flag: Option<&'static str>,
    expr: &'static str,
    label: &'static str,
    /// Whether the run selects by ID (no `-R`): the sign and repeat domain
    /// applies.
    by_id: bool,
    /// Whether pbfhogg needs `--force` on unindexed input. Not where its CLI
    /// refuses the flag: under `--invert-match`, or `-R` with expressions
    /// naming every element type, which never read indexdata.
    force_unindexed: bool,
}

const CASES: &[Case] = &[
    Case {
        pbfhogg_flag: Some("--omit-referenced"),
        osmium_flag: Some("-R"),
        expr: "highway=primary",
        label: "highway-R",
        by_id: false,
        force_unindexed: false,
    },
    Case {
        pbfhogg_flag: Some("--omit-referenced"),
        osmium_flag: Some("-R"),
        expr: "amenity=restaurant",
        label: "amenity-R",
        by_id: false,
        force_unindexed: false,
    },
    Case {
        pbfhogg_flag: Some("--omit-referenced"),
        osmium_flag: Some("-R"),
        expr: "w/highway=primary",
        label: "w-highway-R",
        by_id: false,
        force_unindexed: true,
    },
    // Default mode: direct nodes, way node dependencies and the relation
    // closure of matched relations.
    Case {
        pbfhogg_flag: None,
        osmium_flag: None,
        expr: "highway=primary",
        label: "highway",
        by_id: true,
        force_unindexed: true,
    },
    // Nested relation closure: a route master holds routes.
    Case {
        pbfhogg_flag: None,
        osmium_flag: None,
        expr: "r/type=route_master",
        label: "r-route-master",
        by_id: true,
        force_unindexed: true,
    },
    Case {
        pbfhogg_flag: Some("--remove-tags"),
        osmium_flag: Some("-t"),
        expr: "w/highway=primary",
        label: "w-highway-t",
        by_id: true,
        force_unindexed: true,
    },
    // Unprefixed: osmium scans ways under invert only when a way filter
    // exists or a relation referenced a way, so an `n/` expression would
    // make it omit the ways pbfhogg selects.
    Case {
        pbfhogg_flag: Some("--invert-match"),
        osmium_flag: Some("-i"),
        expr: "highway=primary",
        label: "highway-i",
        by_id: true,
        force_unindexed: false,
    },
];

/// What pbfhogg's default mode says when it refuses disagreeing repeats.
const REPEAT_REFUSAL: &str = "repeats an ID whose records disagree";

/// Run tags-filter cross-validation: pbfhogg vs osmium, three expressions
/// with `-R`, then four in default mode (plain, nested relations,
/// `--remove-tags`, `--invert-match`).
///
/// pbfhogg tags-filter takes unsorted input and keeps its order, so on input
/// without the sort claim osmium reads an osmium-sorted copy and each pbfhogg
/// output is compared through one ([`compare_outputs`]).
pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("tags-filter")?;
    let order = InputOrder::of(harness, pbf, &outdir)?;
    let unindexed = pbf_frames::framing(pbf, Origin::Other)? == Framing::Unindexed;

    let mut history_read = false;
    for case in CASES {
        if case.by_id && !history_read {
            refuse_history(harness, pbf, &outdir)?;
            history_read = true;
        }
        run_case(harness, case, pbf, &order, &outdir, unindexed, direct_io)?;
    }

    Ok(if order.sorted {
        None
    } else {
        Some(CLAIM_UNSORTED.to_owned())
    })
}

/// Refuse a `HistoricalInformation` input before the first default arm:
/// pbfhogg's default mode refuses it by contract.
fn refuse_history(harness: &mut Harness, pbf: &Path, outdir: &Path) -> Result<(), Error> {
    let header = verify_input_order::input_header(harness, pbf, outdir)?;
    if header.required.iter().any(|f| f == "HistoricalInformation") {
        return Err(Error::Tool(format!(
            "{} declares HistoricalInformation; tags-filter without omit-referenced selects by \
             ID and refuses a history file, so its default mode cannot be compared on it",
            pbf.display()
        )));
    }
    Ok(())
}

/// One case: osmium's run (and on a default arm its fold witnesses), then
/// pbfhogg's, its way-location judgment and the comparison.
fn run_case(
    harness: &mut Harness,
    case: &Case,
    pbf: &Path,
    order: &InputOrder,
    outdir: &Path,
    unindexed: bool,
    direct_io: bool,
) -> Result<(), Error> {
    let flags = case.pbfhogg_flag.map_or(String::new(), |f| format!("{f} "));
    let label = format!("tags-filter {flags}{}", case.expr);
    let osmium_label = match case.osmium_flag {
        Some(flag) => format!("osmium tags-filter {flag}"),
        None => "osmium tags-filter".to_owned(),
    };
    harness.msg(&format!("--- {label} ---"));

    // osmium first on a default arm: a fold witness in its output refuses
    // the comparison before a pbfhogg verdict could be recorded.
    let osmium_out = outdir.join(format!("osmium-{}.osm.pbf", case.label));
    let mut osmium = ToolRun::osmium("tags-filter.1")
        .arg("tags-filter")
        .input(&order.reference_input, Origin::Other)
        .arg(case.expr);
    if let Some(flag) = case.osmium_flag {
        osmium = osmium.arg(flag);
    }
    let (osmium, declares) = crate::checks::sort::ask_declared_locations(osmium, pbf)?;
    let osmium = osmium.arg("-o").output(&osmium_out).arg("--overwrite");
    let captured = harness.reference(
        &osmium,
        &outdir.join(format!("osmium-{}.stdout", case.label)),
    )?;
    harness.require_success(&captured, &osmium_label)?;
    if declares {
        crate::checks::sort::require_declared_locations(&osmium_out, &osmium_label)?;
    }
    if case.by_id {
        verify_signs::refuse_folds(
            harness,
            &osmium_out,
            verify_input_order::reference_policy(&osmium_out),
            &osmium_label,
            "osmium tags-filter",
            Witnesses::IdsAndReferences,
        )?;
    }

    let pbfhogg_out = outdir.join(format!("pbfhogg-{}.osm.pbf", case.label));
    let mut argv = PbfArgv::new("tags-filter").input(pbf);
    if let Some(flag) = case.pbfhogg_flag {
        argv = argv.arg(flag);
    }
    let captured = harness.pbfhogg_discard(
        argv.arg(case.expr)
            .output(&pbfhogg_out)
            .arg_if(case.force_unindexed && unindexed, "--force")
            .arg_if(direct_io, "--direct-io"),
    )?;
    if case.by_id && !captured.status.success() && captured.stderr_text().contains(REPEAT_REFUSAL) {
        refuse_repeats(harness, order, outdir)?;
    }
    harness.require_success(&captured, &format!("pbfhogg {label}"))?;
    harness.judge_way_locations(
        &format!("pbfhogg {label}"),
        pbf,
        &pbfhogg_out,
        &captured,
        crate::harness::WayLocations::Kept,
    )?;

    compare_outputs(
        harness,
        order,
        &label,
        &osmium_label,
        &pbfhogg_out,
        &osmium_out,
    )
}

/// pbfhogg refused repeated IDs whose records disagree: a verifier error
/// when osmium finds an ID repeated in the input it read (an osmium-sorted
/// copy keeps every repeat), so the refusal is the contract; else nothing,
/// and the refusal stands as pbfhogg's failure.
fn refuse_repeats(harness: &mut Harness, order: &InputOrder, outdir: &Path) -> Result<(), Error> {
    let info = crate::verify_fileinfo::fileinfo(
        harness,
        (order.reference_input.as_path(), Origin::Other),
        &outdir.join("input.fileinfo.json"),
    )?;
    if info.multiple_versions == Some(true) {
        return Err(Error::Tool(format!(
            "{} repeats an ID, and pbfhogg tags-filter without omit-referenced refuses repeats \
             whose records disagree, where osmium writes each; its default mode cannot be \
             compared on it",
            order.reference_input.display()
        )));
    }
    Ok(())
}
