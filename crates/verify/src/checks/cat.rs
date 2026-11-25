//! Verify: cat - pbfhogg cat vs osmium cat for each element type.

use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, WayLocations};
use crate::reference::{Origin, ToolRun};
use crate::verify_input_order::{CLAIM_UNSORTED, InputOrder, compare_outputs};

/// Cross-validate `pbfhogg cat` against `osmium cat` for node/way/relation types.
///
/// pbfhogg cat takes unsorted input and keeps its order, so on input without
/// the sort claim osmium reads an osmium-sorted copy and each pbfhogg output
/// is compared through one ([`compare_outputs`]).
pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("cat")?;
    let order = InputOrder::of(harness, pbf, &outdir)?;
    // cat --type copies indexed way blobs whole, and re-encodes them when the
    // first data blob carries no indexdata (or there is none).
    // The reference keeps the input's inline coordinates when it declares
    // them (osmium writes them only when asked), so a kept declaration is
    // compared coordinate by coordinate rather than switching the comparison
    // to one without them. Lat/lon arrays without the declaration are not
    // content pbfhogg promises (copied blobs keep them as bytes), and its
    // output never declares them, so the comparison reads no coordinates
    // there whatever option the reference got.
    let declares = crate::pbf_frames::header(pbf, crate::pbf_frames::Origin::Other)?
        .optional_feature("LocationsOnWays");
    let locations =
        if crate::pbf_frames::first_data_blob_indexed(pbf, crate::pbf_frames::Origin::Other)? {
            WayLocations::Kept
        } else {
            WayLocations::Dropped
        };

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
        harness.judge_way_locations(
            &format!("pbfhogg cat -t {elem_type}"),
            &[pbf],
            &pbfhogg_out,
            &captured,
            locations,
        )?;

        // --- osmium cat ---
        let osmium_out = outdir.join(format!("osmium-{elem_type}.osm.pbf"));

        let mut run = ToolRun::osmium("cat.1")
            .arg("cat")
            .input(&order.reference_input, Origin::Other)
            .args(&["-t", elem_type]);
        if declares {
            run = run.args(&["-f", "pbf,locations_on_ways=true"]);
        }
        let captured = harness.reference(
            &run.arg("-o").output(&osmium_out).arg("--overwrite"),
            &outdir.join(format!("osmium-{elem_type}.stdout")),
        )?;
        harness.require_success(&captured, "osmium cat")?;
        if declares {
            crate::checks::sort::require_declared_locations(&osmium_out, "osmium cat")?;
        }

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
