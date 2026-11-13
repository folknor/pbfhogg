//! Verify: tags-filter - pbfhogg tags-filter vs osmium tags-filter.

use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::Harness;
use crate::reference::ToolRun;
use crate::verify_input_order::{CLAIM_UNSORTED, InputOrder, compare_outputs};

/// Expression and output label pairs for the three test cases.
const EXPRESSIONS: &[(&str, &str)] = &[
    ("highway=primary", "highway-R"),
    ("amenity=restaurant", "amenity-R"),
    ("w/highway=primary", "w-highway-R"),
];

/// Run tags-filter cross-validation: pbfhogg vs osmium, 3 expressions with `-R`.
///
/// pbfhogg tags-filter takes unsorted input and keeps its order, so on input
/// without the sort claim osmium reads an osmium-sorted copy and each pbfhogg
/// output is compared through one ([`compare_outputs`]).
pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("tags-filter")?;
    let order = InputOrder::of(harness, pbf, &outdir)?;

    for (expr, label) in EXPRESSIONS {
        harness.msg(&format!("--- tags-filter {expr} -R ---"));

        // pbfhogg: tags-filter <pbf> --omit-referenced <expr> -o <out>
        let pbfhogg_out = outdir.join(format!("pbfhogg-{label}.osm.pbf"));
        let captured = harness.pbfhogg_discard(
            PbfArgv::new("tags-filter")
                .input(pbf)
                .arg("--omit-referenced")
                .arg(*expr)
                .output(&pbfhogg_out)
                .arg_if(direct_io, "--direct-io"),
        )?;
        harness.require_success(&captured, "pbfhogg tags-filter")?;

        // osmium: tags-filter <pbf> <expr> -R -o <out> --overwrite
        let osmium_out = outdir.join(format!("osmium-{label}.osm.pbf"));
        let captured = harness.reference(
            &ToolRun::osmium("tags-filter.1")
                .arg("tags-filter")
                .input(&order.reference_input)
                .args(&[expr, "-R", "-o"])
                .output(&osmium_out)
                .arg("--overwrite"),
            &outdir.join(format!("osmium-{label}.stdout")),
        )?;
        harness.require_success(&captured, "osmium tags-filter")?;

        // Print inspect output for both outputs.
        harness.print_inspect("pbfhogg", &pbfhogg_out)?;
        harness.print_inspect("osmium", &osmium_out)?;

        compare_outputs(
            harness,
            &order,
            &format!("tags-filter --omit-referenced {expr}"),
            "osmium tags-filter -R",
            &pbfhogg_out,
            &osmium_out,
        )?;
    }

    Ok(if order.sorted {
        None
    } else {
        Some(CLAIM_UNSORTED.to_owned())
    })
}
