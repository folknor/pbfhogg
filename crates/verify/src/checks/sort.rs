//! sort: `pbfhogg sort` against `osmium sort`.
use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, listing_beside};
use crate::reference::ToolRun;

/// The file name of this check's osmium output. Part of a reference run's
/// declaration, so a later run sorting the same input under this name with
/// the same strategy shares the cached run; the cache key carries the argv,
/// so the sort check's multipass run and a simple sorted copy never collide.
pub(crate) const OSMIUM_SORTED: &str = "osmium.osm.pbf";

/// `osmium sort INPUT -o OUTPUT` as a reference run with the default
/// (simple) strategy, exit 0 required. This is the sorted copy other checks
/// feed to pbfhogg: only the simple strategy writes the `Sort.Type_then_ID`
/// header claim pbfhogg branches on (osmium-tool's `run_multi_pass` never
/// sets `sorting`), so copies cannot take multipass. When the input's header
/// declares `LocationsOnWays` the copy is written with
/// `-f pbf,locations_on_ways=true`, so the inline coordinates libosmium
/// decodes survive the sort and stay comparable.
pub(crate) fn osmium_sort(
    h: &mut Harness,
    input: &Path,
    output: &Path,
    sink: &Path,
) -> Result<(), Error> {
    osmium_sort_as(h, input, output, sink, false)
}

/// `osmium_sort`, optionally with `-s multipass`. The simple strategy holds
/// every input object in memory at once (about ten times the PBF size by
/// osmium's man page) and was OOM-killed on germany; multipass holds one
/// object type at a time. Both order by type, ID and version with a stable
/// sort and neither deduplicates, so the elements and their order agree.
fn osmium_sort_as(
    h: &mut Harness,
    input: &Path,
    output: &Path,
    sink: &Path,
    multipass: bool,
) -> Result<(), Error> {
    let locations = crate::pbf_frames::header(input)?.optional_feature("LocationsOnWays");
    let mut run = ToolRun::osmium("sort.1").arg("sort").input(input);
    if multipass {
        run = run.args(&["-s", "multipass"]);
    }
    if locations {
        run = run.args(&["-f", "pbf,locations_on_ways=true"]);
    }
    h.reference(&run.arg("-o").output(output), sink)?;
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
    // sort keeps the declaration unless it rewrites a way blob, which the
    // verifier cannot see; the comparison below reads coordinates only when
    // both files declare them, so a drop must at least be the warned one.
    h.judge_way_locations(
        "pbfhogg sort",
        pbf,
        &ours,
        &ended,
        crate::harness::WayLocations::KeptOrReported,
    )?;

    let theirs = outdir.join(OSMIUM_SORTED);
    // Multipass: this check judges elements and pbfhogg's order, never the
    // reference's header, so the claim multipass omits is not needed here.
    osmium_sort_as(h, pbf, &theirs, &outdir.join("osmium.stdout"), true)?;
    let policy = crate::opl_compare::Policy::between(&ours, &theirs)?;
    let diff = h.same_elements(&ours, &theirs, &listing_beside(&ours), policy)?;
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
