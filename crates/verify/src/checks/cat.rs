//! Verify: cat - pbfhogg cat vs osmium cat for each element type.

use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::Harness;
use crate::reference::ToolRun;
use crate::verify_input_order::{CLAIM_UNSORTED, InputOrder, compare_outputs};

/// Cross-validate `pbfhogg cat` against `osmium cat` for node/way/relation types.
///
/// pbfhogg cat takes unsorted input and keeps its order, so on input without
/// the sort claim osmium reads an osmium-sorted copy and each pbfhogg output
/// is compared through one ([`compare_outputs`]).
pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("cat")?;
    let order = InputOrder::of(harness, pbf, &outdir)?;

    for elem_type in &["node", "way", "relation"] {
        harness.msg(&format!("=== verify cat -t {elem_type} ==="));

        // --- pbfhogg cat ---
        let pbfhogg_out = outdir.join(format!("pbfhogg-{elem_type}.osm.pbf"));

        let captured = harness.pbfhogg_discard(
            PbfArgv::new("cat")
                .input(pbf)
                .arg("--type")
                .arg(*elem_type)
                .output(&pbfhogg_out)
                .arg_if(direct_io, "--direct-io"),
        )?;
        harness.require_success(&captured, "pbfhogg cat")?;

        // --- osmium cat ---
        let osmium_out = outdir.join(format!("osmium-{elem_type}.osm.pbf"));

        let captured = harness.reference(
            &ToolRun::osmium("cat.1")
                .arg("cat")
                .input(&order.reference_input)
                .args(&["-t", elem_type, "-o"])
                .output(&osmium_out)
                .arg("--overwrite"),
            &outdir.join(format!("osmium-{elem_type}.stdout")),
        )?;
        harness.require_success(&captured, "osmium cat")?;

        // --- Element counts ---
        harness.print_inspect("pbfhogg", &pbfhogg_out)?;
        harness.print_inspect("osmium", &osmium_out)?;

        compare_outputs(
            harness,
            &order,
            &format!("cat -t {elem_type}"),
            "osmium cat",
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
