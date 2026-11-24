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

/// One data blob as `pbfhogg sort`'s pass 1 sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SortBlob {
    /// Under the header's sort claim, a blob with indexdata: believed from
    /// the claim, never decoded.
    Believed(crate::pbf_frames::BlobClaim),
    /// Any other blob: its `(kind, id)` in stored order, and whether every
    /// group carries at most one element field.
    Scanned(Vec<(usize, i64)>, bool),
}

/// Every data blob of `pbf` as pass 1 sees it: believed from its claim where
/// `sorted` (the optional `Sort.Type_then_ID` feature, what pbfhogg's
/// `HeaderBlock::is_sorted` reads) and indexdata allow, decoded otherwise.
/// So the sorted, fully indexed path stays header-only, as pbfhogg's does.
fn sort_blobs(pbf: &Path, sorted: bool) -> Result<Vec<SortBlob>, Error> {
    let mut walk = crate::pbf_frames::Claims::open(pbf, Origin::Other)?;
    let mut blobs = Vec::new();
    while let Some(blob) = walk.next_blob()? {
        blobs.push(match blob.claim {
            Some(claim) if sorted => SortBlob::Believed(claim),
            _ => {
                let (ids, single) = crate::pbf_frames::blob_ids_and_shape(
                    pbf,
                    Origin::Other,
                    blob.payload_offset,
                    blob.datasize,
                )?;
                SortBlob::Scanned(ids, single)
            }
        });
    }
    Ok(blobs)
}

/// Canonical OSM ID order: 0, then negatives by magnitude, then positives
/// (pbfhogg's `osm_id::osm_id_key`, libosmium's comparator).
fn osm_key(id: i64) -> (u8, u64) {
    match id.cmp(&0) {
        std::cmp::Ordering::Greater => (2, id.unsigned_abs()),
        std::cmp::Ordering::Equal => (0, 0),
        std::cmp::Ordering::Less => (1, id.unsigned_abs()),
    }
}

/// A numeric range's first and last keys as pbfhogg's planner derives them
/// (`blob_osm_first_key`, `blob_osm_last_key`): conservative for a
/// mixed-sign range, which is taken to start at 0. Deliberately not the
/// observed extreme keys, which would change overlap decisions.
fn range_keys(min: i64, max: i64) -> ((u8, u64), (u8, u64)) {
    if min >= 0 {
        (osm_key(min), osm_key(max))
    } else if max <= 0 {
        (osm_key(max), osm_key(min))
    } else {
        (osm_key(0), osm_key(max))
    }
}

/// What `pbfhogg sort` must do with a `LocationsOnWays` declaration, by the
/// planner it shares with `cat --dedupe` (`blob_merge::plan`), predicted
/// from `blobs` in file order. A rewritten way blob is re-encoded without
/// coordinates (docs/cli/commands.md, "Output header": none when a way blob
/// is rewritten), and a way entry is rewritten when it is in an overlap
/// component (entries ordered by kind and first key, a component growing
/// while the next entry starts at or below its widest last key), when a
/// scanned blob is internally out of order (an element below its
/// predecessor in canonical order), or when its scanned block is one no
/// single index describes (a group with two element fields, or two kinds),
/// which is restructured into one rewritten entry per kind. A scanned empty
/// block is omitted; a believed claim is an entry whatever its count.
///
/// The domain: believed claims are true, as nothing decodes them. A scanned
/// blob the verifier's decoder refuses (a repeated or absent scalar ID, a
/// dense ID delta that wraps, compression other than raw or zlib) is a
/// verifier error, though pbfhogg accepts it. Each `(kind, id)` occurs once:
/// pbfhogg also rewrites a scanned blob whose versions of a repeated ID
/// descend, which this model does not read, and [`run`] refuses an input
/// holding a repeated ID as a verifier error before this verdict is made.
/// Within the domain the verdict is exact, so there is no
/// keep-or-drop middle.
pub(crate) fn sort_way_locations(
    blobs: impl IntoIterator<Item = SortBlob>,
) -> crate::harness::WayLocations {
    use crate::harness::WayLocations;
    // Way entries only: components never cross kinds.
    let mut ways: Vec<((u8, u64), (u8, u64))> = Vec::new();
    let mut forced = false;
    for blob in blobs {
        match blob {
            SortBlob::Believed(c) => {
                if c.kind == 1 {
                    ways.push(range_keys(c.min_id, c.max_id));
                }
            }
            SortBlob::Scanned(ids, single) => {
                let Some(&(kind, _)) = ids.first() else {
                    continue;
                };
                if !single || ids.iter().any(|&(k, _)| k != kind) {
                    forced |= ids.iter().any(|&(k, _)| k == 1);
                    continue;
                }
                if kind != 1 {
                    continue;
                }
                if ids.windows(2).any(|w| osm_key(w[1].1) < osm_key(w[0].1)) {
                    forced = true;
                    continue;
                }
                let min = ids.iter().map(|&(_, id)| id).min().unwrap_or(0);
                let max = ids.iter().map(|&(_, id)| id).max().unwrap_or(0);
                ways.push(range_keys(min, max));
            }
        }
    }
    if forced {
        return WayLocations::Dropped;
    }
    ways.sort_by_key(|&(first, _)| first);
    let mut reach = None;
    for (first, last) in ways {
        if reach.is_some_and(|r| first <= r) {
            return WayLocations::Dropped;
        }
        reach = Some(reach.map_or(last, |r: (u8, u64)| r.max(last)));
    }
    WayLocations::Kept
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
    // sort keeps the declaration unless it rewrites a way blob, which the
    // planner predicted here decides exactly. The comparison below reads
    // coordinates only when both files declare them, so a drop must be the
    // warned one and a keep must not be lost.
    // An input without the declaration needs no prediction (the judge only
    // holds the output to not declaring it), so it pays no decode.
    let expect = if header.optional_feature("LocationsOnWays") {
        sort_way_locations(sort_blobs(pbf, header.optional_sorted())?)
    } else {
        crate::harness::WayLocations::Kept
    };
    h.judge_way_locations("pbfhogg sort", pbf, &ours, &ended, expect)?;

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

#[cfg(test)]
mod tests {
    use super::{SortBlob, sort_way_locations};
    use crate::harness::WayLocations::{self, Dropped, Kept};
    use crate::pbf_frames::BlobClaim;

    fn claim(kind: usize, min_id: i64, max_id: i64) -> SortBlob {
        SortBlob::Believed(BlobClaim {
            kind,
            min_id,
            max_id,
            count: 1,
        })
    }

    fn scanned(kind: usize, ids: &[i64]) -> SortBlob {
        SortBlob::Scanned(ids.iter().map(|&id| (kind, id)).collect(), true)
    }

    fn verdict<const N: usize>(blobs: [SortBlob; N]) -> WayLocations {
        sort_way_locations(blobs)
    }

    #[test]
    fn disjoint_ways_must_be_kept() {
        let claims = [
            claim(0, 1, 9),
            claim(1, 1, 5),
            claim(1, 6, 9),
            claim(2, 1, 3),
        ];
        assert_eq!(verdict(claims), Kept);
        // Node and relation ranges never decide the way blobs.
        let other = [claim(0, 1, 9), claim(0, 2, 3), claim(1, 1, 5)];
        assert_eq!(verdict(other), Kept);
        // Scanned in-order blobs, equal neighbours included, are copied.
        assert_eq!(
            verdict([scanned(1, &[1, 2, 2, 5]), scanned(1, &[6, 9])]),
            Kept
        );
        // Canonical order: 0, then negatives by magnitude, then positives.
        assert_eq!(verdict([scanned(1, &[0, -1, -7, 3])]), Kept);
        // A believed empty claim is an entry, and no scanned empty block is.
        let empty = SortBlob::Believed(BlobClaim {
            kind: 1,
            min_id: 0,
            max_id: 0,
            count: 0,
        });
        assert_eq!(
            verdict([empty, SortBlob::Scanned(Vec::new(), true), claim(1, 1, 5)]),
            Kept
        );
    }

    #[test]
    fn overlapping_way_ranges_require_the_drop() {
        assert_eq!(
            verdict([claim(1, 6, 9), claim(1, 1, 5), claim(1, 5, 7)]),
            Dropped
        );
        // Overlap through the widest reach, not only the next by start.
        assert_eq!(
            verdict([claim(1, 1, 100), claim(1, 2, 3), claim(1, 4, 5)]),
            Dropped
        );
        // Negative ranges ordered canonically: -1..-5 reaches past -3's start.
        assert_eq!(
            verdict([scanned(1, &[-1, -5]), scanned(1, &[-3, -4])]),
            Dropped
        );
        // A mixed-sign range is taken to start at 0 and so overlaps 0.
        assert_eq!(verdict([claim(1, -2, 4), claim(1, 0, 0)]), Dropped);
    }

    #[test]
    fn a_scanned_blob_the_planner_cannot_copy_requires_the_drop() {
        // Internally out of order.
        assert_eq!(verdict([scanned(1, &[5, 3])]), Dropped);
        assert_eq!(verdict([scanned(1, &[-2, -1])]), Dropped);
        // Restructured: two kinds, or a group with two element fields.
        let mixed = SortBlob::Scanned(vec![(0, 1), (1, 1)], true);
        assert_eq!(verdict([mixed]), Dropped);
        assert_eq!(
            verdict([SortBlob::Scanned(vec![(1, 1), (1, 2)], false)]),
            Dropped
        );
        // A restructured blob without ways, or a disordered node blob, does
        // not touch the way blobs.
        let nodes = SortBlob::Scanned(vec![(0, 1), (2, 1)], true);
        assert_eq!(verdict([nodes, scanned(0, &[9, 1]), claim(1, 1, 5)]), Kept);
    }
}
