//! Verify: getid - pbfhogg getid vs osmium getid, getid --invert vs osmium
//! removeid (with way IDs, plus the complement test, and without them, where
//! a `LocationsOnWays` declaration must be kept), and getid --add-referenced
//! (plain and with --remove-tags) vs osmium getid -r (plain and with -t), the
//! last in [`crate::verify_getid_refs`].
//!
//! Domain: a snapshot with one record per kind and ID, whose indexdata
//! claims are true and non-negative (osmium removeid and getid -r match an
//! ID by its absolute value). A header declaring `HistoricalInformation` is refused;
//! repeated records in a file that does not declare it are outside the
//! domain and not looked for in the input, though every output the check
//! compares must be in strict order, so a repeat either tool wrote is
//! caught.
//!
//! Input order. pbfhogg branches on `Sort.Type_then_ID` among the input
//! header's optional features, and so does this check, as verify extract
//! does. Without it (an unsorted snapshot) every osmium run reads an
//! osmium-sorted copy of the input, shared with verify sort's cached run,
//! while pbfhogg reads the input as it is; each pbfhogg output the check
//! diffs must then not claim
//! `Sort.Type_then_ID` and is compared through an osmium-sorted copy,
//! whose strict order proves it repeats no ID. getid answers by ID alone, so
//! reading a sorted copy changes no reference answer.

use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::counts::{Counts, osmium_counts};
use crate::error::Error;
use crate::harness::{Harness, Verdict, WayLocations};
use crate::pbf_frames::{self, IndexSummary};
use crate::reference::ToolRun;
use crate::verify_input_order::{CLAIM_UNSORTED, InputOrder, compare_outputs};

const KIND_LETTERS: [char; 3] = ['n', 'w', 'r'];

/// The IDs to request, per kind (node, way, relation): the lowest and highest
/// ID the input's indexdata claims for that kind, so both exist in any input
/// whose claims are true, whatever the dataset. Negative IDs are left out:
/// osmium refuses a negative text ID and matches the rest by absolute
/// value, so a negative request has no osmium answer to compare with; a kind
/// with none left requests nothing.
fn endpoint_ids(summary: &IndexSummary) -> [Vec<i64>; 3] {
    let mut ids: [Vec<i64>; 3] = Default::default();
    for (kind, claim) in summary.kinds.iter().enumerate() {
        let Some(claim) = claim else { continue };
        for id in [claim.min_id, claim.max_id] {
            if id >= 0 && !ids[kind].contains(&id) {
                ids[kind].push(id);
            }
        }
    }
    ids
}

fn id_args(ids: &[Vec<i64>; 3]) -> Vec<String> {
    ids.iter()
        .zip(KIND_LETTERS)
        .flat_map(|(ids, letter)| ids.iter().map(move |id| format!("{letter}{id}")))
        .collect()
}

/// The per-kind cardinality assertion: on a snapshot (one object per kind
/// and ID) getid must select exactly the requested IDs that exist, and every
/// requested ID exists by the indexdata claim.
fn selection_verdicts(ids: &[Vec<i64>; 3], selected: Counts) -> [(&'static str, usize, u64); 3] {
    [
        ("nodes", ids[0].len(), selected.nodes),
        ("ways", ids[1].len(), selected.ways),
        ("relations", ids[2].len(), selected.relations),
    ]
}

/// Run getid cross-validation: pbfhogg getid vs osmium getid, the pbfhogg
/// getid --invert complement test, then getid --add-referenced against
/// osmium getid -r ([`crate::verify_getid_refs`]).
///
/// The IDs come from the input itself (see [`endpoint_ids`]), read by a
/// header-only walk of its blob framing ([`pbf_frames::walk`]): no element
/// data is read to choose them, so the cost is the blob count even at
/// planet scale. The walk refuses an input that is not fully indexed in a
/// form pbfhogg reads, and a history file (the cardinality assertion needs
/// one object per kind and ID).
#[allow(clippy::too_many_lines)]
pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("getid-removeid")?;

    harness.msg("--- getid: choosing IDs from the input's indexdata ---");
    let summary = { pbf_frames::walk(pbf, pbf_frames::Origin::Other)? };
    if summary.is_history() {
        return Err(Error::Tool(format!(
            "{} declares HistoricalInformation; getid-removeid asserts one object per kind \
             and ID, which a history file does not have",
            pbf.display()
        )));
    }
    // osmium removeid (and getid -r below) match an ID by its absolute
    // value, pbfhogg by its signed one.
    crate::verify_getid_refs::refuse_negative_ids(&summary, pbf)?;
    let ids = endpoint_ids(&summary);
    let requested = id_args(&ids);
    if requested.is_empty() {
        return Err(Error::Tool(format!(
            "{} has no element with a non-negative ID to request; the comparison would be vacuous",
            pbf.display()
        )));
    }
    harness.msg(&format!("  requesting: {}", requested.join(" ")));

    let order = InputOrder::of(harness, pbf, &outdir)?;

    harness.msg("--- getid: pbfhogg vs osmium ---");

    // pbfhogg getid <pbf> -o <out> <ids...>
    let pbfhogg_getid = outdir.join("pbfhogg-getid.osm.pbf");
    let captured = harness.pbfhogg_discard(
        PbfArgv::new("getid")
            .input(pbf)
            .output(&pbfhogg_getid)
            .arg_if(direct_io, "--direct-io")
            .args_from(requested.iter().map(String::as_str)),
    )?;
    harness.require_success(&captured, "pbfhogg getid")?;
    harness.judge_way_locations(
        "pbfhogg getid",
        pbf,
        &pbfhogg_getid,
        &captured,
        WayLocations::Dropped,
    )?;

    let selected = osmium_counts(harness, &pbfhogg_getid)?;
    for (kind, want, got) in selection_verdicts(&ids, selected) {
        let verdict = Verdict::from_pass(u64::try_from(want).ok() == Some(got));
        if verdict.is_pass() {
            harness.msg(&format!(
                "  selected {kind}: {got} of {want} requested PASS"
            ));
        } else {
            harness.msg(&format!(
                "  selected {kind}: {got}, but {want} requested IDs exist FAIL"
            ));
        }
        harness.record(&format!("getid selected {kind}"), verdict);
    }

    // osmium getid <pbf> <ids...> -o <out> --overwrite
    let osmium_getid = outdir.join("osmium-getid.osm.pbf");
    let mut osmium_run = ToolRun::osmium("getid.1")
        .arg("getid")
        .input(&order.reference_input);
    for id in &requested {
        osmium_run = osmium_run.arg(id.as_str());
    }
    let osmium_run = osmium_run
        .arg("-o")
        .output(&osmium_getid)
        .arg("--overwrite");
    let captured = harness.reference(&osmium_run, &outdir.join("osmium-getid.stdout"))?;
    harness.require_success(&captured, "osmium getid")?;

    compare_outputs(
        harness,
        &order,
        "getid",
        "osmium getid",
        &pbfhogg_getid,
        &osmium_getid,
    )?;

    // --- getid --invert: vs osmium removeid, and the complement test ---
    harness.msg("--- getid --invert: pbfhogg vs osmium removeid ---");
    let invert = Invert {
        outdir: &outdir,
        pbf,
        order: &order,
        direct_io,
    };
    let pbfhogg_invert = invert.run(harness, "getid-invert", &ids)?;

    // The complement assertion: getid selects exactly the listed IDs (the
    // element comparison above pins that) and getid --invert drops exactly them, so
    // per element kind the two outputs must add up to the original. A kind
    // whose sum is off means --invert kept or dropped something it shouldn't.
    // The original's counts are its indexdata claims, from the walk above,
    // without another pass; the outputs' counts are osmium fileinfo's.
    let original = Counts {
        nodes: summary.count(0),
        ways: summary.count(1),
        relations: summary.count(2),
    };
    let inverted = osmium_counts(harness, &pbfhogg_invert)?;
    for (kind, orig, sel, inv) in [
        ("nodes", original.nodes, selected.nodes, inverted.nodes),
        ("ways", original.ways, selected.ways, inverted.ways),
        (
            "relations",
            original.relations,
            selected.relations,
            inverted.relations,
        ),
    ] {
        let verdict = complement_verdict(orig, sel, inv);
        if verdict.is_pass() {
            harness.msg(&format!(
                "  complement ({kind}): PASS ({sel} selected + {inv} inverted = {orig} original)"
            ));
        } else {
            harness.msg(&format!(
                "  complement ({kind}): FAIL ({sel} selected + {inv} inverted != {orig} original)"
            ));
        }
        harness.record(&format!("getid --invert complement ({kind})"), verdict);
    }

    // A request without way IDs, where getid --invert must copy every way
    // blob whole and so keep a LocationsOnWays declaration, which the
    // comparison with osmium removeid then reads coordinate by coordinate.
    // An input without ways made the request above that case already.
    let ways_kept = [ids[0].clone(), Vec::new(), ids[2].clone()];
    let kept_unrun = !ids[1].is_empty() && ways_kept.iter().all(Vec::is_empty);
    if kept_unrun {
        harness.msg(
            "  getid --invert without way IDs: not run, the input claims no node or relation \
             to request",
        );
    } else if !ids[1].is_empty() {
        harness.msg("--- getid --invert without way IDs: pbfhogg vs osmium removeid ---");
        invert.run(harness, "getid-invert-no-ways", &ways_kept)?;
    }

    // --- getid --add-referenced, plain and --remove-tags ---
    let mut claim = crate::verify_getid_refs::run(
        harness,
        &crate::verify_getid_refs::RefsInput {
            outdir: &outdir,
            pbf,
            order: &order,
            summary: &summary,
            direct_io,
        },
    )?;
    if kept_unrun {
        claim.insert(0, "no getid --invert request without way IDs".to_owned());
    }
    if !order.sorted {
        claim.insert(0, CLAIM_UNSORTED.to_owned());
    }
    Ok((!claim.is_empty()).then(|| claim.join("; ")))
}

/// One `pbfhogg getid --invert` request, compared with `osmium removeid` of
/// the same IDs.
struct Invert<'a> {
    outdir: &'a Path,
    pbf: &'a Path,
    order: &'a InputOrder,
    direct_io: bool,
}

impl Invert<'_> {
    /// Run the request `ids` under the file stem `name`, judge the output's
    /// `LocationsOnWays` declaration and compare its elements with osmium
    /// removeid's (written with the input's inline coordinates when it
    /// declares them, so a kept declaration is compared coordinate by
    /// coordinate). Returns pbfhogg's output.
    fn run(
        &self,
        harness: &mut Harness,
        name: &str,
        ids: &[Vec<i64>; 3],
    ) -> Result<PathBuf, Error> {
        let requested = id_args(ids);
        harness.msg(&format!("  requesting: {}", requested.join(" ")));
        let ours = self.outdir.join(format!("pbfhogg-{name}.osm.pbf"));
        let captured = harness.pbfhogg_discard(
            PbfArgv::new("getid")
                .arg("--invert")
                .input(self.pbf)
                .output(&ours)
                .arg_if(self.direct_io, "--direct-io")
                .args_from(requested.iter().map(String::as_str)),
        )?;
        harness.require_success(&captured, "pbfhogg getid --invert")?;
        let label = if ids[1].is_empty() {
            "pbfhogg getid --invert without way IDs"
        } else {
            "pbfhogg getid --invert"
        };
        harness.judge_way_locations(
            label,
            self.pbf,
            &ours,
            &captured,
            invert_way_locations(ids),
        )?;

        // The declaration, not payload arrays: undeclared arrays are not
        // content pbfhogg promises and its output never declares them, so
        // the comparison reads no coordinates there whatever osmium wrote.
        let declares = crate::pbf_frames::header(self.pbf, crate::pbf_frames::Origin::Other)?
            .optional_feature("LocationsOnWays");
        let theirs = self.outdir.join(format!("osmium-{name}.osm.pbf"));
        let mut run = ToolRun::osmium("removeid.1")
            .arg("removeid")
            .input(&self.order.reference_input);
        for id in &requested {
            run = run.arg(id.as_str());
        }
        if declares {
            run = run.args(&["-f", "pbf,locations_on_ways=true"]);
        }
        let run = run.arg("-o").output(&theirs).arg("--overwrite");
        let captured =
            harness.reference(&run, &self.outdir.join(format!("osmium-{name}.stdout")))?;
        harness.require_success(&captured, "osmium removeid")?;
        if declares {
            crate::checks::sort::require_declared_locations(&theirs, "osmium removeid")?;
        }
        compare_outputs(
            harness,
            self.order,
            label.trim_start_matches("pbfhogg "),
            "osmium removeid",
            &ours,
            &theirs,
        )?;
        Ok(ours)
    }
}

/// What `getid --invert` must do with a `LocationsOnWays` declaration
/// (docs/cli/commands.md, "Output header"): on an indexed input it copies
/// every way blob whole exactly when no way ID is requested, and otherwise
/// re-encodes its ways without coordinates. The check's input is fully
/// indexed ([`pbf_frames::walk`] refuses anything else).
fn invert_way_locations(ids: &[Vec<i64>; 3]) -> WayLocations {
    if ids[1].is_empty() {
        WayLocations::Kept
    } else {
        WayLocations::Dropped
    }
}

/// `selected + inverted == original`, without overflow on absurd counts.
fn complement_verdict(original: u64, selected: u64, inverted: u64) -> Verdict {
    Verdict::from_pass(selected.checked_add(inverted) == Some(original))
}

#[cfg(test)]
mod tests {
    use super::{complement_verdict, endpoint_ids, id_args};
    use crate::harness::Verdict;
    use crate::pbf_frames::{IndexSummary, KindClaim};

    #[test]
    fn endpoints_are_deduplicated_and_skip_negatives_and_empty_kinds() {
        let claim = |min_id, max_id| {
            Some(KindClaim {
                min_id,
                max_id,
                count: 1,
            })
        };
        let summary = IndexSummary {
            kinds: [claim(1, 13_000_000_000), claim(-5, 7), None],
            required_features: Vec::new(),
        };
        let ids = endpoint_ids(&summary);
        assert_eq!(ids[0], vec![1, 13_000_000_000]);
        assert_eq!(ids[1], vec![7]);
        assert!(ids[2].is_empty());
        assert_eq!(id_args(&ids), vec!["n1", "n13000000000", "w7"]);

        let single = IndexSummary {
            kinds: [None, None, claim(42, 42)],
            required_features: Vec::new(),
        };
        assert_eq!(id_args(&endpoint_ids(&single)), vec!["r42"]);
        let negative = IndexSummary {
            kinds: [claim(-9, -1), None, None],
            required_features: Vec::new(),
        };
        assert!(id_args(&endpoint_ids(&negative)).is_empty());
    }

    #[test]
    fn invert_keeps_way_locations_exactly_without_way_ids() {
        use crate::harness::WayLocations;
        let without = [vec![1, 9], Vec::new(), vec![4]];
        assert_eq!(super::invert_way_locations(&without), WayLocations::Kept);
        let with = [Vec::new(), vec![7], Vec::new()];
        assert_eq!(super::invert_way_locations(&with), WayLocations::Dropped);
    }

    #[test]
    fn complement_holds_when_parts_sum_to_whole() {
        assert_eq!(complement_verdict(100, 3, 97), Verdict::Pass);
        assert_eq!(complement_verdict(0, 0, 0), Verdict::Pass);
    }

    #[test]
    fn complement_fails_on_leak_or_loss() {
        // --invert kept a listed element (leak) or dropped an extra one (loss).
        assert_eq!(complement_verdict(100, 3, 98), Verdict::Fail);
        assert_eq!(complement_verdict(100, 3, 96), Verdict::Fail);
        assert_eq!(complement_verdict(5, u64::MAX, 1), Verdict::Fail);
    }
}
