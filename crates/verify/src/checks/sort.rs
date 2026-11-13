//! sort: `pbfhogg sort` against `osmium sort`.
use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, listing_beside};
use crate::reference::ToolRun;

/// The file name of this check's osmium output. Part of a reference run's
/// declaration, so a later check sorting the same input under this name
/// can share its run once runs are cached.
pub(crate) const OSMIUM_SORTED: &str = "osmium.osm.pbf";

/// `osmium sort INPUT -o OUTPUT` as a reference run, exit 0 required.
pub(crate) fn osmium_sort(
    h: &mut Harness,
    input: &Path,
    output: &Path,
    sink: &Path,
) -> Result<(), Error> {
    h.reference(
        &ToolRun::osmium("sort.1")
            .arg("sort")
            .input(input)
            .arg("-o")
            .output(output),
        sink,
    )?;
    Ok(())
}

pub(crate) fn run(h: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = h.subdir("sort")?;
    h.msg("=== verify sort ===");

    let ours = outdir.join("pbfhogg.osm.pbf");
    let ended = h.pbfhogg(
        PbfArgv::new("sort")
            .input(pbf)
            .output(&ours)
            .arg_if(direct_io, "--direct-io"),
        &outdir.join("pbfhogg.stdout"),
    )?;
    h.require_success(&ended, "pbfhogg sort")?;

    let theirs = outdir.join(OSMIUM_SORTED);
    osmium_sort(h, pbf, &theirs, &outdir.join("osmium.stdout"))?;
    h.print_inspect("osmium", &theirs)?;

    let diff = h.diff_pbfs(&ours, &theirs, &listing_beside(&ours))?;
    h.msg(if diff == crate::harness::Verdict::Pass {
        "  diff: PASS (identical)"
    } else {
        "  diff: FAIL (differences found)"
    });
    h.record("pbfhogg vs osmium diff", diff);

    let order = h.check_sorted("pbfhogg sort", &ours)?;
    h.record("pbfhogg sort output order", order);
    Ok(None)
}
