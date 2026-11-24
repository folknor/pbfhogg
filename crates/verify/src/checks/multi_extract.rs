//! Verify: multi-extract - compare single-pass multi-extract against
//! N sequential single-region extracts.
//!
//! Generates N adjacent longitude strips from the dataset bbox (neighbours
//! share an inclusive edge, so a node on it belongs to both),
//! runs `pbfhogg extract --config <json> --simple` (single-pass), then
//! runs one `pbfhogg extract --simple` per region (the sequential
//! reference). Per region, the multi output must hold the same elements as
//! its sequential extract, record for record as osmium decodes both
//! (`crate::opl_compare`, metadata included, headers left out). The
//! reference is pbfhogg's own sequential extract, so this is
//! single-pass/sequential parity: an extraction bug both paths share passes,
//! and `verify extract` is what holds extraction to osmium.
//!
//! Two configs. The first holds the strips alone. When the dataset
//! configures polygons, a second holds the strips and then every polygon, as
//! `polygon_file` entries naming the file by its absolute path: pbfhogg's
//! single pass takes another route once any region is a polygon (an all-bbox
//! config may copy node blobs raw), so the mixed config re-checks the strips
//! too. Both compare against the same sequential extracts, one per region.
//!
//! Input order. pbfhogg's single pass needs `Sort.Type_then_ID` among the
//! input header's optional features. With it, each multi output must be in
//! strict order. Without it (an unsorted snapshot) pbfhogg declines the
//! single pass and runs every region on its sequential fallback, so neither
//! single-pass route is exercised; every multi and sequential output must
//! then not claim the flag (an extract keeps its input's order claim), and
//! each is compared through an osmium-sorted copy whose strict order proves
//! it repeats no ID.
//!
//! Only the simple strategy is checked: it is the one with a single-pass
//! implementation, while complete-ways and smart under `--config` run the
//! per-region path, which would be compared with itself.
//!
//! Route. The fallback's outputs equal the sequential reference by
//! construction, so parity alone cannot show the single pass ran. Each config
//! run's stderr must carry the route the header predicts: the single-pass
//! banner on sorted input, the sequential-fallback note on unsorted input,
//! and not the other. Which single-pass classifier ran (the all-bbox one or
//! the polygon one of a mixed config) has no diagnostic and is not confirmed.
//!
//! The strips are adjacent, and polygons overlap them only where the dataset's
//! polygons happen to; identical regions with distinct outputs and a
//! deliberately empty region are never constructed (an element can still
//! fall in several regions, and a region can still come out empty). osmium's
//! extract membership is not consulted (osmium only decodes the compared
//! files and, on unsorted input, sorts the comparison copies); `verify
//! extract` holds polygon membership to osmium.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::checks::sort as verify_sort;
use crate::error::Error;
use crate::extract_config::PolygonInput;
use crate::extract_config::{ExtractStrategy, NamedRegion, PbfhoggCommand};
use crate::harness::{Harness, Role, Verdict};
use crate::pbf_frames::{Origin, SORT_TYPE_THEN_ID};
use pbfhogg_devkit::Bbox;

/// The claim a passing multi-extract check's summary line carries.
fn claim(input_sorted: bool, polygons: &[PolygonInput]) -> String {
    let configs = if polygons.is_empty() {
        "for an all-strip config; no dataset polygon is configured, so no mixed strip and polygon \
         config is run"
            .to_owned()
    } else {
        let names: Vec<&str> = polygons.iter().map(|p| p.name.as_str()).collect();
        format!(
            "for an all-strip config and a config of the strips and polygons {}",
            names.join(", ")
        )
    };
    if input_sorted {
        format!(
            "the config runs report the single pass, and per region the simple multi-extract \
             output is in strict order and element-content \
             identical, as osmium decodes both, to a sequential pbfhogg extract, {configs}; \
             osmium's extract membership is not consulted"
        )
    } else {
        format!(
            "on unsorted input, where pbfhogg declines its single pass so no single-pass route is \
             exercised and the config runs report the fallback: per region neither output claims {SORT_TYPE_THEN_ID}, and the multi \
             output, through an osmium-sorted copy, repeats no ID and is element-content \
             identical, as osmium decodes both, to a sequential pbfhogg extract, {configs}; \
             osmium's extract membership is not consulted"
        )
    }
}

/// One region's sequential extract as compared: the output itself on sorted
/// input, its osmium-sorted copy otherwise.
struct Sequential {
    compared: PathBuf,
}

/// Cross-validate single-pass multi-extract against sequential extracts.
#[allow(clippy::too_many_lines)]
pub fn run(
    harness: &mut Harness,
    pbf: &Path,
    bbox: &Bbox,
    polygons: &[PolygonInput],
    regions: usize,
    direct_io: bool,
) -> Result<Option<String>, Error> {
    let strips = crate::extract_config::strips(bbox, regions)?;
    let all = crate::extract_config::multi_extract_regions(&strips, polygons);
    let outdir = harness.subdir("multi-extract")?;

    harness.msg(&format!(
        "=== verify multi-extract ({regions} strips, {} polygons) ===",
        polygons.len()
    ));

    // --- Input order: the header claim the single pass needs ---
    let input_sorted =
        crate::verify_input_order::input_header(harness, pbf, &outdir)?.optional_sorted();
    if !input_sorted {
        harness.msg(&format!(
            "  input header does not declare {SORT_TYPE_THEN_ID} as an optional feature: \
             pbfhogg runs every region sequentially, and outputs are compared through \
             osmium-sorted copies"
        ));
    }
    let claim = claim(input_sorted, polygons);

    // --- Sequential single-region extracts, one per region ---
    // These are the reference each region is diffed against, so they run
    // buffered: under `direct_io` an O_DIRECT fault shared by both extract
    // paths would otherwise sit on both sides of the diff and pass. The
    // single-region extract's own O_DIRECT path is held to osmium by
    // `verify extract`.
    let seq_dir = outdir.join("sequential");
    std::fs::create_dir_all(&seq_dir)?;
    harness.msg("  running sequential extracts...");
    let mut sequential: HashMap<String, Sequential> = HashMap::new();
    for named in &all {
        let seq_out = seq_dir.join(named.output_name());
        let command = PbfhoggCommand::Extract {
            strategy: ExtractStrategy::Simple,
        };
        let seq_args = command.extract_args_for(pbf, &named.region, &seq_out)?;
        let captured = harness.pbfhogg_discard(seq_args)?;
        harness.require_success(&captured, &format!("pbfhogg extract {}", named.stem))?;
        harness.judge_way_locations(
            &format!("sequential extract {}", named.stem),
            pbf,
            &seq_out,
            &captured,
            crate::harness::WayLocations::Dropped,
        )?;
        let compared = if input_sorted {
            seq_out
        } else {
            let label = format!("sequential extract {}", named.stem);
            record_no_sort_claim(harness, &label, &seq_out)?;
            let copy = sorted_copy(harness, &seq_out)?;
            // Its own decode, not folded into the region comparison below:
            // that comes only after the multi-extract run, whose failure
            // would otherwise lose this verdict.
            let (unique, _) = harness.check_sorted_counted(&label, (&copy, Origin::Other))?;
            harness.record(&format!("{label} output repeats an ID"), unique);
            copy
        };
        sequential.insert(named.stem.clone(), Sequential { compared });
    }

    // --- Each config: single pass, then every region against its sequential ---
    let mut passes: Vec<(&str, &[NamedRegion])> = vec![("multi", &all[..strips.len()])];
    if !polygons.is_empty() {
        passes.push(("mixed", &all[..]));
    }
    for (pass, pass_regions) in passes {
        let label_prefix = if pass == "multi" {
            "multi-extract".to_owned()
        } else {
            "multi-extract mixed".to_owned()
        };
        let multi_dir = outdir.join(pass);
        std::fs::create_dir_all(&multi_dir)?;
        let config = crate::extract_config::multi_extract_config(&multi_dir, pass_regions)?;
        let config_path = if pass == "multi" {
            crate::extract_config::multi_extract_config_path(&outdir)
        } else {
            outdir.join(format!("{pass}-extract-config.json"))
        };
        std::fs::write(&config_path, config.json.as_bytes())?;

        harness.msg(&format!("  running single-pass {label_prefix}..."));
        let mut multi_args = PbfArgv::new("extract")
            .input(pbf)
            .extract_config(&config_path, config);
        if let Some(flag) = ExtractStrategy::Simple.flag() {
            multi_args = multi_args.arg(flag);
        }
        // The config declares every output, so the argv supplies private staging.
        let captured = harness.pbfhogg_discard(multi_args.arg_if(direct_io, "--direct-io"))?;
        harness.require_success(&captured, &format!("pbfhogg {label_prefix}"))?;

        // Fallback outputs equal the sequential reference by construction, so
        // parity alone cannot show the single pass ran: the route is judged
        // from the run's own diagnostics.
        let route = Route::from_stderr(&captured.stderr_text());
        let expected = if input_sorted {
            Route::SinglePass
        } else {
            Route::Fallback
        };
        harness.msg(&format!(
            "  {label_prefix}: route {} (expected {}) {}",
            route.name(),
            expected.name(),
            if route == expected { "PASS" } else { "FAIL" }
        ));
        harness.record(
            &format!(
                "{label_prefix} took route {} where the input header predicts {}",
                route.name(),
                expected.name()
            ),
            Verdict::from_pass(route == expected),
        );

        // Counts alone let a region with substituted IDs or changed content
        // pass, so each region is diffed against its sequential extract; the
        // counts are printed as context. The diff refuses a side out of
        // strict order, so the multi output's order, a verdict of its own,
        // is judged from the diff's own decoding.
        harness.msg(&format!("  comparing {label_prefix} regions..."));
        for named in pass_regions {
            let multi_pbf = multi_dir.join(named.output_name());
            let label = format!("{label_prefix} {}", named.stem);
            // One invocation writes every region, so its one warning covers
            // them all: each region is held to dropping the declaration.
            harness.judge_way_locations(
                &label,
                pbf,
                &multi_pbf,
                &captured,
                crate::harness::WayLocations::Dropped,
            )?;
            let (compared, order) = if input_sorted {
                (multi_pbf.clone(), format!("{label} output order"))
            } else {
                record_no_sort_claim(harness, &label, &multi_pbf)?;
                (
                    sorted_copy(harness, &multi_pbf)?,
                    format!("{label} output repeats an ID"),
                )
            };
            let seq = sequential
                .get(&named.stem)
                .ok_or_else(|| Error::Tool(format!("no sequential extract for {}", named.stem)))?;
            // Both sides are pbfhogg's outputs on sorted input, osmium's
            // sorted copies of them otherwise.
            let origin = if input_sorted {
                Origin::Pbfhogg
            } else {
                Origin::Other
            };
            let policy =
                crate::opl_compare::Policy::between((&compared, origin), (&seq.compared, origin))?;
            let (same, [counts, _]) = harness.same_elements_judged(
                (
                    (&compared, origin),
                    Role::Judged {
                        label: &label,
                        record: &order,
                    },
                ),
                ((&seq.compared, origin), Role::Required),
                &crate::harness::listing_beside(&compared),
                policy,
            )?;
            harness.record(
                &format!("{label} element content differs from the sequential extract"),
                same,
            );
            let outcome = if same.is_pass() { "PASS" } else { "FAIL" };
            harness.msg(&format!(
                "  {label}: same elements as sequential {outcome} ({} nodes, {} ways, {} \
                 relations)",
                counts.nodes, counts.ways, counts.relations
            ));
        }
    }

    Ok(Some(claim))
}

/// Which route a config run reports having taken, from its stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// The single-pass banner alone.
    SinglePass,
    /// The sequential-fallback note alone.
    Fallback,
    /// Neither line, or both: no route is established. Requiring positive
    /// evidence makes a reworded diagnostic fail rather than pass. A path
    /// holding a newline could counterfeit a line, but only add one, so the
    /// genuine line of the route actually taken still vetoes a wrong verdict.
    Unknown,
}

impl Route {
    fn from_stderr(stderr: &str) -> Self {
        let single = stderr
            .lines()
            .any(|l| l.starts_with("[multi-extract] single-pass: "));
        let fallback = stderr
            .lines()
            .any(|l| l.starts_with("Note: Multi-extract single-pass probe declined the input"));
        match (single, fallback) {
            (true, false) => Self::SinglePass,
            (false, true) => Self::Fallback,
            _ => Self::Unknown,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::SinglePass => "single-pass",
            Self::Fallback => "sequential fallback",
            Self::Unknown => "unknown",
        }
    }
}

/// On unsorted input, `output` must not claim the sort flag.
fn record_no_sort_claim(harness: &mut Harness, label: &str, output: &Path) -> Result<(), Error> {
    let claims = harness.has_sort_flag(output, Origin::Pbfhogg)?;
    harness.msg(&format!(
        "  {label}: header {} {SORT_TYPE_THEN_ID} on unsorted input {}",
        if claims { "claims" } else { "does not claim" },
        if claims { "FAIL" } else { "PASS" }
    ));
    harness.record(
        &format!("{label} output claims {SORT_TYPE_THEN_ID} on unsorted input"),
        Verdict::from_pass(!claims),
    );
    Ok(())
}

/// An osmium-sorted copy of `output`, beside it.
fn sorted_copy(harness: &mut Harness, output: &Path) -> Result<PathBuf, Error> {
    let stem = output
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".osm.pbf"))
        .ok_or_else(|| Error::Tool(format!("unexpected output name {}", output.display())))?;
    let copy = output.with_file_name(format!("{stem}.sorted.osm.pbf"));
    verify_sort::osmium_sort(
        harness,
        (output, Origin::Pbfhogg),
        &copy,
        &output.with_file_name(format!("{stem}.sorted.stdout")),
    )?;
    Ok(copy)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_claim_names_the_configs_and_what_unsorted_input_leaves_out() {
        let sorted = claim(true, &[]);
        assert!(sorted.contains("no mixed strip and polygon config is run"));
        assert!(sorted.starts_with("the config runs report the single pass"));
        let polygon = PolygonInput {
            name: "city".into(),
            path: PathBuf::from("/d/city.geojson"),
        };
        let mixed = claim(false, &[polygon]);
        assert!(mixed.contains("strips and polygons city"));
        assert!(mixed.contains("no single-pass route is exercised"));
    }

    #[test]
    fn the_route_needs_its_own_line_and_not_the_other() {
        let single = "[multi-extract] single-pass: 5 regions, simple strategy\n";
        let fallback = "Note: Multi-extract single-pass probe declined the input; using one \
                        full pass per region\n[1/5] Extracting to a.osm.pbf\n";
        assert_eq!(Route::from_stderr(single), Route::SinglePass);
        assert_eq!(Route::from_stderr(fallback), Route::Fallback);
        assert_eq!(Route::from_stderr(""), Route::Unknown);
        assert_eq!(
            Route::from_stderr(&format!("{single}{fallback}")),
            Route::Unknown
        );
        assert_eq!(
            Route::from_stderr("Note: Region grid declined\n"),
            Route::Unknown
        );
    }
}
