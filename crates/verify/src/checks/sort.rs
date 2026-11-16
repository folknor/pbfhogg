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
    if locations {
        require_declared_locations(output, "osmium sort")?;
    }
    Ok(())
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
    if crate::pbf_frames::header(output)?.optional_feature("LocationsOnWays") {
        return Ok(());
    }
    Err(Error::Failed(format!(
        "{tool} was asked for -f pbf,locations_on_ways=true, but {} does not declare \
         LocationsOnWays, so its inline coordinates cannot be compared",
        output.display()
    )))
}

/// Every data blob's indexdata claim, in file order, by a header-only walk.
fn input_claims(pbf: &Path) -> Result<Vec<Option<crate::pbf_frames::BlobClaim>>, Error> {
    let mut walk = crate::pbf_frames::Claims::open(pbf)?;
    let mut claims = Vec::new();
    while let Some(blob) = walk.next_blob()? {
        claims.push(blob.claim);
    }
    Ok(claims)
}

/// What `pbfhogg sort` must do with a `LocationsOnWays` declaration, read
/// from the input's per-blob indexdata claims (`claims`, in file order;
/// `None` for a blob without indexdata) and its header sort claim, on the
/// verifier's domain of true claims and one record per kind and ID.
///
/// sort rewrites every way entry of an overlap run, and a rewritten way blob
/// is re-encoded without coordinates (docs/cli/commands.md, "Output header":
/// none when a way blob is rewritten). Two non-empty, non-negative way ranges
/// that overlap cannot be written in order by copying both blobs whole, since
/// each holds an ID on either side of the other's first ID, so the drop is
/// required. With no overlap and the sort claim, pbfhogg believes each blob
/// in order and copies every way blob whole, so the declaration must be
/// kept. Otherwise the framing does not decide it: without the sort claim
/// pbfhogg rewrites an internally unsorted blob, which only a decode shows; a
/// blob without indexdata may hold ways (a mixed block is restructured); and
/// a negative ID or an empty claim falls outside the overlap argument.
pub(crate) fn sort_way_locations(
    claims: impl IntoIterator<Item = Option<crate::pbf_frames::BlobClaim>>,
    sorted: bool,
) -> crate::harness::WayLocations {
    use crate::harness::WayLocations;
    let mut ways = Vec::new();
    let mut decided = true;
    for claim in claims {
        match claim {
            None => decided = false,
            Some(c) if c.kind != 1 => {}
            Some(c) if c.count == 0 || c.min_id < 0 => decided = false,
            Some(c) => ways.push((c.min_id, c.max_id)),
        }
    }
    ways.sort_unstable();
    if ways.windows(2).any(|w| w[1].0 <= w[0].1) {
        return WayLocations::Dropped;
    }
    if decided && sorted {
        WayLocations::Kept
    } else {
        WayLocations::KeptOrReported
    }
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
    // sort keeps the declaration unless it rewrites a way blob; where the
    // input's claims decide that, the verdict is exact. The comparison below
    // reads coordinates only when both files declare them, so a drop must at
    // least be the warned one.
    let expect = sort_way_locations(
        input_claims(pbf)?,
        crate::pbf_frames::header(pbf)?.optional_sorted(),
    );
    h.judge_way_locations("pbfhogg sort", pbf, &ours, &ended, expect)?;

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
    h.record("pbfhogg vs osmium elements", diff);

    let order = h.check_sorted("pbfhogg sort", &ours)?;
    h.record("pbfhogg sort output order", order);
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::sort_way_locations;
    use crate::harness::WayLocations;
    use crate::pbf_frames::BlobClaim;

    fn claim(kind: usize, min_id: i64, max_id: i64) -> Option<BlobClaim> {
        Some(BlobClaim {
            kind,
            min_id,
            max_id,
            count: 1,
        })
    }

    #[test]
    fn disjoint_ways_on_sorted_input_must_be_kept() {
        let claims = [
            claim(0, 1, 9),
            claim(1, 1, 5),
            claim(1, 6, 9),
            claim(2, 1, 3),
        ];
        assert_eq!(sort_way_locations(claims, true), WayLocations::Kept);
        // Without the sort claim intra-blob order is not visible.
        assert_eq!(
            sort_way_locations(claims, false),
            WayLocations::KeptOrReported
        );
        // Node and relation ranges never decide the way blobs.
        let other = [claim(0, 1, 9), claim(0, 2, 3), claim(1, 1, 5)];
        assert_eq!(sort_way_locations(other, true), WayLocations::Kept);
    }

    #[test]
    fn overlapping_way_ranges_require_the_drop() {
        for sorted in [true, false] {
            let claims = [claim(1, 6, 9), claim(1, 1, 5), claim(1, 5, 7)];
            assert_eq!(sort_way_locations(claims, sorted), WayLocations::Dropped);
            // Overlap with a blob not next by start, beside an unindexed blob.
            let wide = [claim(1, 1, 100), claim(1, 2, 3), claim(1, 4, 5), None];
            assert_eq!(sort_way_locations(wide, sorted), WayLocations::Dropped);
        }
    }

    #[test]
    fn undecidable_claims_leave_the_choice_to_the_run() {
        let unindexed = [claim(1, 1, 5), None];
        assert_eq!(
            sort_way_locations(unindexed, true),
            WayLocations::KeptOrReported
        );
        let negative = [claim(1, -5, -1), claim(1, 1, 5)];
        assert_eq!(
            sort_way_locations(negative, true),
            WayLocations::KeptOrReported
        );
        let empty = [Some(BlobClaim {
            kind: 1,
            min_id: 0,
            max_id: 0,
            count: 0,
        })];
        assert_eq!(
            sort_way_locations(empty, true),
            WayLocations::KeptOrReported
        );
    }
}
