//! Verify: getid - pbfhogg getid vs osmium getid, the getid --invert
//! complement test, and getid --add-referenced (plain and with
//! --remove-tags) vs osmium getid -r (plain and with -t), the last in
//! [`crate::verify_getid_refs`].
//!
//! Domain: a snapshot with one record per kind and ID, whose indexdata
//! claims are true. A header declaring `HistoricalInformation` is refused;
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
//! diffs (the `--invert` output is only counted) must then not claim
//! `Sort.Type_then_ID` and is compared through an osmium-sorted copy,
//! whose strict order proves it repeats no ID. getid answers by ID alone, so
//! reading a sorted copy changes no reference answer.

use std::path::Path;

use crate::argv::PbfArgv;
use crate::counts::{Counts, inspect_counts};
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::pbf_frames::{self, IndexSummary};
use crate::reference::ToolRun;
use crate::verify_input_order::{CLAIM_UNSORTED, InputOrder, compare_outputs};

const KIND_LETTERS: [char; 3] = ['n', 'w', 'r'];

/// The IDs to request, per kind (node, way, relation): the lowest and highest
/// ID the input's indexdata claims for that kind, so both exist in any input
/// whose claims are true, whatever the dataset. Negative IDs are left out
/// (pbfhogg getid does not accept them); a kind with none left requests
/// nothing.
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
    let summary = { pbf_frames::walk(pbf)? };
    if summary.is_history() {
        return Err(Error::Tool(format!(
            "{} declares HistoricalInformation; getid-removeid asserts one object per kind \
             and ID, which a history file does not have",
            pbf.display()
        )));
    }
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

    let selected = inspect_counts(harness, &pbfhogg_getid)?;
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

    // --- getid --invert: complement test ---
    harness.msg("--- getid --invert: complement test ---");

    let pbfhogg_invert = outdir.join("pbfhogg-getid-invert.osm.pbf");
    let captured = harness.pbfhogg_discard(
        PbfArgv::new("getid")
            .arg("--invert")
            .input(pbf)
            .output(&pbfhogg_invert)
            .arg_if(direct_io, "--direct-io")
            .args_from(requested.iter().map(String::as_str)),
    )?;
    harness.require_success(&captured, "pbfhogg getid --invert")?;

    // The complement assertion: getid selects exactly the listed IDs (the
    // osmium diff above pins that) and getid --invert drops exactly them, so
    // per element kind the two outputs must add up to the original. A kind
    // whose sum is off means --invert kept or dropped something it shouldn't.
    // The original's counts are its indexdata claims, from the walk above -
    // what pbfhogg inspect reports for it too, without another pass.
    let original = Counts {
        nodes: summary.count(0),
        ways: summary.count(1),
        relations: summary.count(2),
    };
    let inverted = inspect_counts(harness, &pbfhogg_invert)?;
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
    if !order.sorted {
        claim.insert(0, CLAIM_UNSORTED.to_owned());
    }
    Ok((!claim.is_empty()).then(|| claim.join("; ")))
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
