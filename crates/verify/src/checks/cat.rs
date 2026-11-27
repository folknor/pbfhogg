//! Verify cat's per-kind outputs against osmium's decoding.

use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Role, Verdict, WayLocations};
use crate::opl_compare::{self, Basis, Policy};
use crate::reference::{Origin, ToolRun};
use crate::verify_input_order::{
    CLAIM_UNSORTED, InputOrder, compare_outputs, compare_outputs_after_order,
};
use crate::verify_reencode::KINDS;

mod views;

use views::{Kind, Views};

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
    let declares =
        crate::pbf_frames::header(pbf, Origin::Other)?.optional_feature("LocationsOnWays");
    let locations = if crate::pbf_frames::first_data_blob_indexed(pbf, Origin::Other)? {
        WayLocations::Kept
    } else {
        WayLocations::Dropped
    };

    let mut expected = None;
    for (kind, elem_type) in KINDS.into_iter().enumerate() {
        harness.msg(&format!("=== verify cat -t {elem_type} ==="));

        // --- pbfhogg cat ---
        let pbfhogg_out = outdir.join(format!("pbfhogg-{elem_type}.osm.pbf"));

        let captured = harness.pbfhogg_discard(
            PbfArgv::new("cat")
                .input(pbf)
                .arg("--type")
                .arg(elem_type)
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

        let expected = match &mut expected {
            Some(expected) => expected,
            slot @ None => slot.insert(Expected::prepare(harness, &order, &outdir, declares)?),
        };
        expected.compare(harness, &order, &outdir, &pbfhogg_out, kind)?;
    }

    Ok(if order.sorted {
        None
    } else {
        Some(CLAIM_UNSORTED.to_owned())
    })
}

struct Expected {
    header: PathBuf,
    declares: bool,
    views: Option<Views>,
}

impl Expected {
    fn prepare(
        h: &mut Harness,
        order: &InputOrder,
        outdir: &Path,
        declares: bool,
    ) -> Result<Self, Error> {
        let policy = if declares {
            Policy::FULL
        } else {
            Policy::NO_LOCATIONS
        };
        let views = match Views::prepare(h, &order.reference_input, outdir, policy) {
            Ok(views) => Some(views),
            // Leave input-error attribution to the original per-kind runs.
            Err(Error::Tool(_) | Error::Failed(_)) => None,
            Err(e) => return Err(e),
        };
        // Single-input osmium cat copies its Reader header to the Writer
        // before consuming any objects. The type switches only the Reader
        // mask. Libosmium's PBF reader emits no changesets, so this gives
        // each projected arm the real header without writing object blobs.
        let header = if (order.sorted || declares)
            && views
                .as_ref()
                .is_some_and(|views| views.observed.kinds.iter().any(Kind::can_project))
        {
            reference_cat(h, &order.reference_input, outdir, "changeset", declares)?
        } else {
            order.reference_input.clone()
        };
        Ok(Self {
            header,
            declares,
            views,
        })
    }

    fn compare(
        &mut self,
        h: &mut Harness,
        order: &InputOrder,
        outdir: &Path,
        ours: &Path,
        kind: usize,
    ) -> Result<(), Error> {
        let elem_type = KINDS[kind];
        let label = format!("cat -t {elem_type}");
        let Some(views) = self
            .views
            .as_mut()
            .filter(|views| views.observed.kinds[kind].can_project())
        else {
            // Keep the old roundtrip for normalization and the old order
            // diagnostic for a disordered reference (including repeats).
            let theirs =
                reference_cat(h, &order.reference_input, outdir, elem_type, self.declares)?;
            return compare_outputs(h, order, &label, "osmium cat", ours, &theirs);
        };
        let header = &self.header;
        let [nodes, ways, relations] = views.observed.kinds[kind].summary.counts;
        h.msg(&format!(
            "  osmium cat: {nodes} nodes, {ways} ways, {relations} relations"
        ));
        h.msg("  osmium cat: in strict order (osmium OPL) PASS");
        compare_outputs_after_order(h, order, &label, ours, header, |h, a, listing| {
            let policy = Policy::between(a.0, (header, Origin::Other))?;
            let view = &views.observed.kinds[kind];
            let (summary, no_locations) = match &view.without_locations {
                Some(summary) if !policy.locations => (summary, true),
                _ => (&view.summary, false),
            };
            let basis = views.key.as_ref().map(|key| Basis {
                source: order.reference_input.clone(),
                key: key.clone(),
                counts: summary.counts,
                digest: summary.digest.clone(),
            });
            let source_policy = views.policy;
            let walked = opl_compare::compare_judged_with_opl(
                h,
                a.0,
                listing,
                policy,
                basis.as_ref().map(|basis| (basis, source_policy)),
                |h| views.path(h, kind, no_locations),
            )?;
            let (merged, _) = h.judge_walk([(a.0.0, a.1), (header, Role::Required)], walked)?;
            if !merged.identical() {
                h.replay_listing(listing)?;
            }
            Ok(Verdict::from_pass(merged.identical()))
        })
    }
}

fn reference_cat(
    h: &mut Harness,
    input: &Path,
    outdir: &Path,
    elem_type: &str,
    declares: bool,
) -> Result<PathBuf, Error> {
    let output = outdir.join(format!("osmium-{elem_type}.osm.pbf"));
    let mut run = ToolRun::osmium("cat.1")
        .arg("cat")
        .input(input, Origin::Other)
        .args(&["-t", elem_type]);
    if declares {
        run = run.args(&["-f", "pbf,locations_on_ways=true"]);
    }
    let captured = h.reference(
        &run.arg("-o").output(&output).arg("--overwrite"),
        &outdir.join(format!("osmium-{elem_type}.stdout")),
    )?;
    h.require_success(&captured, "osmium cat")?;
    if declares {
        crate::checks::sort::require_declared_locations(&output, "osmium cat")?;
    }
    Ok(output)
}
