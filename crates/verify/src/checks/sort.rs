//! sort: `pbfhogg sort` against `osmium sort`.
use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, listing_beside};
use crate::pbf_frames::Origin;
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
    (input, origin): (&Path, Origin),
    output: &Path,
    sink: &Path,
) -> Result<(), Error> {
    osmium_sort_as(h, (input, origin), output, sink, false)
}

/// `osmium_sort`, optionally with `-s multipass`. The simple strategy holds
/// every input object in memory at once (about ten times the PBF size by
/// osmium's man page) and was OOM-killed on germany; multipass holds one
/// object type at a time. Both order by type, ID and version with a stable
/// sort and neither deduplicates, so the elements and their order agree.
fn osmium_sort_as(
    h: &mut Harness,
    (input, origin): (&Path, Origin),
    output: &Path,
    sink: &Path,
    multipass: bool,
) -> Result<(), Error> {
    // Gated on the declaration, never on payload arrays: undeclared arrays
    // are not content pbfhogg promises, and its output never declares them,
    // so Policy::between reads no coordinates on such an input whatever
    // option the copy got (reference/verify.md, "Element equality").
    let locations = crate::pbf_frames::header(input, origin)?.optional_feature("LocationsOnWays");
    let mut run = ToolRun::osmium("sort.1").arg("sort").input(input, origin);
    if multipass {
        run = run.args(&["-s", "multipass"]);
    }
    if locations {
        run = run.args(&["-f", "pbf,locations_on_ways=true"]);
    }
    h.reference(&run.arg("-o").output(output), sink)?;
    if locations {
        require_declared_locations(output, "osmium sort")?;
    }
    Ok(())
}

/// Whether `input` declares `LocationsOnWays`; if it does, `run` (a
/// reference writing PBF) is asked for the inline coordinates, and the
/// caller holds its output to [`require_declared_locations`]. Gated on the
/// declaration, never on payload arrays, as in [`osmium_sort_as`].
pub(crate) fn ask_declared_locations(run: ToolRun, input: &Path) -> Result<(ToolRun, bool), Error> {
    let declares =
        crate::pbf_frames::header(input, Origin::Other)?.optional_feature("LocationsOnWays");
    Ok(if declares {
        (run.args(&["-f", "pbf,locations_on_ways=true"]), true)
    } else {
        (run, false)
    })
}

/// Require `output`, written by `tool` with `-f pbf,locations_on_ways=true`,
/// to declare `LocationsOnWays`. The element comparison reads inline
/// coordinates only when both files declare them ([`Policy::between`]), so a
/// reference or sorted copy that lost the declaration would silently turn
/// a coordinate comparison off; that is a verifier error, not a pbfhogg
/// verdict. Checked after the run, so a cached reference is held too.
///
/// [`Policy::between`]: crate::opl_compare::Policy::between
pub(crate) fn require_declared_locations(output: &Path, tool: &str) -> Result<(), Error> {
    if crate::pbf_frames::header(output, Origin::Other)?.optional_feature("LocationsOnWays") {
        return Ok(());
    }
    Err(Error::Tool(format!(
        "{tool} was asked for -f pbf,locations_on_ways=true, but {} does not declare \
         LocationsOnWays, so its inline coordinates cannot be compared",
        output.display()
    )))
}

pub(crate) fn run(h: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = h.subdir("sort")?;
    h.msg("=== verify sort ===");

    // The domain is a snapshot: one object per kind and ID. pbfhogg sort
    // keeps every record of a repeated ID (docs/cli/commands.md), but the
    // strict comparison keys on kind and ID alone, and the reference cannot
    // judge the order of records of one ID and version (osmium breaks that
    // tie by timestamp, pbfhogg keeps input order). So the input is held to
    // the domain before anything pbfhogg did is judged: a declared history
    // file up front, and an undeclared repeat by the reference sort below.
    let header = crate::pbf_frames::header(pbf, Origin::Other)?;
    if header.required.iter().any(|f| f == "HistoricalInformation") {
        return Err(Error::Tool(format!(
            "{} declares HistoricalInformation; the sort check compares one object per \
             kind and ID, which a history file does not have",
            pbf.display()
        )));
    }
    let theirs = outdir.join(OSMIUM_SORTED);
    // Multipass: this check judges elements and pbfhogg's order, never the
    // reference's header, so the claim multipass omits is not needed here.
    // Run first, so its order scan settles the domain before pbfhogg runs.
    osmium_sort_as(
        h,
        (pbf, Origin::Other),
        &theirs,
        &outdir.join("osmium.stdout"),
        true,
    )?;
    // osmium sorts without deduplicating, so its output out of strict order
    // means the input holds an object more than once (or osmium misordered
    // it): a verifier error either way, never a pbfhogg verdict. The policy
    // is a decoding cache hint for the comparison below; the scan reads
    // keys only.
    let hint = crate::opl_compare::Policy::between((pbf, Origin::Other), (&theirs, Origin::Other))?;
    let (ordered, _) = crate::opl_compare::order_as(h, (&theirs, Origin::Other), hint)?;
    if !ordered {
        return Err(Error::Tool(format!(
            "osmium sort's output of {} is not in strict kind and ID order: the input \
             holds an object more than once, outside the sort check's domain of one \
             object per kind and ID",
            pbf.display()
        )));
    }

    let ours = outdir.join("pbfhogg.osm.pbf");
    let ended = h.pbfhogg(
        PbfArgv::new("sort")
            .input(pbf)
            .output(&ours)
            .arg_if(direct_io, "--direct-io"),
        &outdir.join("pbfhogg.stdout"),
    )?;
    h.require_success(&ended, "pbfhogg sort")?;
    // sort keeps a declared LocationsOnWays whatever it does to the way
    // blobs: copied ones keep their arrays as bytes and rewritten ones are
    // re-encoded with them (docs/cli/commands.md, "Output header"). The
    // comparison below reads coordinates only when both files declare them,
    // so the kept declaration is what puts every coordinate under test. An
    // input without it is held to an output that does not declare it.
    h.judge_way_locations(
        "pbfhogg sort",
        pbf,
        &ours,
        &ended,
        crate::harness::WayLocations::Kept,
    )?;

    let policy =
        crate::opl_compare::Policy::between((&ours, Origin::Pbfhogg), (&theirs, Origin::Other))?;
    // pbfhogg's order is judged from the comparison's own decoding: a
    // separate order decode after the strict comparison could only pass.
    let (diff, _) = h.same_elements_judged(
        (
            (&ours, Origin::Pbfhogg),
            crate::harness::Role::Judged {
                label: "pbfhogg sort",
                record: "pbfhogg sort output order",
            },
        ),
        ((&theirs, Origin::Other), crate::harness::Role::Required),
        &listing_beside(&ours),
        policy,
    )?;
    h.msg(if diff == crate::harness::Verdict::Pass {
        "  diff: PASS (identical)"
    } else {
        "  diff: FAIL (differences found)"
    });
    h.record("pbfhogg vs osmium elements", diff);
    Ok(None)
}
