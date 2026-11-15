//! Verify: renumber - `pbfhogg renumber` vs `osmium renumber`.
//!
//! Cross-validates pbfhogg's renumber output against osmium. Unlike the other
//! verify subcommands, a small number of differences is expected and treated
//! as a PASS: pbfhogg's orphan-reference handling in relation members is a
//! documented semantic deviation (see pbfhogg `DEVIATIONS.md`).
//!
//! The comparison is the verifier's merge of osmium's decodings of both
//! outputs (`crate::opl_compare`, metadata included), so both must be in
//! strict order and every object must pair by kind and ID. Fail conditions:
//! - an object only one output holds (so the counts diverge too)
//! - any node or way whose record differs
//! - relation differences above `0.10 * relations` of osmium's output
//!   (sanity threshold)
//!
//! Input whose header lacks `Sort.Type_then_ID` among its optional features
//! is input both tools refuse; pbfhogg's renumber is then held to its
//! documented refusal and nothing is compared (`crate::verify_input_order`).
//!
//! The run directory follows the verifier's shared retention policy.

use std::fs;
use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::Harness;
use crate::opl_compare::{self, Class, Merged, Policy};
use crate::reference::ToolRun;

/// Differing identities per kind (node, way, relation) and class.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Classified {
    one_sided: [u64; 3],
    differs: [u64; 3],
}

impl Classified {
    fn add(&mut self, group: opl_compare::Group) {
        match group.class {
            Class::FirstOnly | Class::SecondOnly => self.one_sided[group.kind] += 1,
            Class::Differs => self.differs[group.kind] += 1,
        }
    }
}

/// Why the comparison fails, or `None` when it passes. osmium's output is
/// the merge's first side.
fn judge(merged: &Merged, classified: &Classified) -> Option<String> {
    let [n, w, r] = classified.differs;
    let threshold = merged.first[2] / 10;
    if merged.first != merged.second {
        Some(format!(
            "element counts diverge (osmium={:?}, pbfhogg={:?})",
            merged.first, merged.second
        ))
    } else if classified.one_sided != [0; 3] {
        Some(format!(
            "objects only one output holds per kind (node, way, relation): {:?}",
            classified.one_sided
        ))
    } else if n > 0 || w > 0 {
        Some(format!("node/way diffs detected (nodes={n}, ways={w})"))
    } else if r > threshold {
        Some(format!(
            "relation diffs {r} exceed sanity threshold {threshold} (10% of {} relations)",
            merged.first[2]
        ))
    } else {
        None
    }
}

/// Format an integer with comma thousand separators.
fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Format a byte count as a short human-readable string.
fn fmt_size(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    #[allow(clippy::cast_precision_loss)]
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Cross-validate `pbfhogg renumber` against `osmium renumber`.
///
/// `direct_io` reaches the renumber run only.
#[allow(clippy::too_many_lines)]
pub fn run(
    harness: &mut Harness,
    pbf: &Path,
    dataset: &str,
    start_id: Option<&str>,
    verbose: bool,
    direct_io: bool,
) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("renumber")?;

    harness.msg("--- renumber cross-validation ---");
    harness.msg(&format!("  dataset: {dataset}"));
    harness.msg(&format!("  input: {}", pbf.display()));
    if let Some(sid) = start_id {
        harness.msg(&format!("  start-id: {sid}"));
    }

    // --- Output paths ---------------------------------------------------
    let osmium_out = outdir.join(format!("osmium-renumber-{dataset}.osm.pbf"));
    let pbfhogg_out = outdir.join(format!("pbfhogg-renumber-{dataset}.osm.pbf"));
    let diff_log = outdir.join(format!("verify-renumber-{dataset}-diff.txt"));

    let mut pbfhogg_args = PbfArgv::new("renumber")
        .input(pbf)
        .output(&pbfhogg_out)
        .arg_if(direct_io, "--direct-io");
    if let Some(sid) = start_id {
        pbfhogg_args = pbfhogg_args.arg("--start-id").arg(sid);
    }

    // Input without the sorted claim is input renumber refuses by contract,
    // and osmium renumber requires sorted input: pbfhogg's run is held to
    // that refusal and osmium does not run (`verify_input_order`).
    if !crate::verify_input_order::input_header(harness, pbf, &outdir)?.optional_sorted() {
        let claim = Some(crate::verify_input_order::refusal_claim(
            "pbfhogg renumber",
            "",
            direct_io,
        ));
        crate::verify_input_order::expect_unsorted_refusal(
            harness,
            pbfhogg_args,
            &crate::verify_input_order::Refusal {
                label: "renumber",
                operand: pbf,
                answers: &[0],
                sink: None,
            },
        )?;
        return Ok(claim);
    }

    // --- osmium renumber ------------------------------------------------
    let mut osmium_run = ToolRun::osmium("renumber.1")
        .arg("renumber")
        .input(pbf)
        .arg("-o")
        .output(&osmium_out)
        .arg("--overwrite");
    if let Some(sid) = start_id {
        osmium_run = osmium_run.args(&["--start-id", sid]);
    }
    let captured = harness.reference(&osmium_run, &outdir.join("osmium-renumber.stdout"))?;
    harness.require_success(&captured, "osmium renumber")?;

    let osmium_size = fs::metadata(&osmium_out).map(|m| m.len()).unwrap_or(0);
    harness.msg(&format!(
        "  osmium renumber: {} output",
        fmt_size(osmium_size),
    ));

    // --- pbfhogg renumber -----------------------------------------------
    let captured = harness.pbfhogg_discard(pbfhogg_args)?;
    harness.require_success(&captured, "pbfhogg renumber")?;

    let pbfhogg_size = fs::metadata(&pbfhogg_out).map(|m| m.len()).unwrap_or(0);
    harness.msg(&format!(
        "  pbfhogg renumber: {} output",
        fmt_size(pbfhogg_size),
    ));

    // --- The element comparison, classified by kind ---------------------
    let policy = Policy::between(&osmium_out, &pbfhogg_out)?;
    let mut classified = Classified::default();
    let merged =
        opl_compare::compare(harness, &osmium_out, &pbfhogg_out, &diff_log, policy, |g| {
            classified.add(g);
        })?;

    let total: u64 = merged.first.iter().sum();
    harness.msg(&format!(
        "  element counts: osmium={} pbfhogg={}",
        fmt_num(total),
        fmt_num(merged.second.iter().sum()),
    ));
    harness.msg(&format!(
        "  same: {}, different: {} (nodes {}, ways {}, relations {}), one-sided: {:?}",
        fmt_num(merged.same),
        fmt_num(merged.differs),
        classified.differs[0],
        classified.differs[1],
        classified.differs[2],
        classified.one_sided,
    ));

    if verbose && !merged.identical() {
        harness.msg(&format!("  diff log: {}", diff_log.display()));
        let (lines, _) = crate::stream::preview(
            &diff_log,
            crate::tunables::PREVIEW_LINES,
            crate::tunables::PREVIEW_LINE_BYTES,
        )?;
        for line in lines {
            harness.msg(&format!("    {line}"));
        }
    }

    match judge(&merged, &classified) {
        Some(reason) => {
            harness.msg(&format!("  result: FAIL ({reason})"));
            harness.msg(&format!("  diff log preserved: {}", diff_log.display()));
            Err(Error::Failed(format!("renumber: {reason}")))
        }
        None => {
            if merged.differs == 0 {
                harness.msg("  result: PASS (identical)");
            } else {
                harness.msg("  result: PASS (within expected delta)");
                harness.msg(&format!(
                    "    -> {} relation-member diff(s) attributed to orphan-reference handling",
                    merged.differs,
                ));
                harness.msg("    -> documented deviation (see pbfhogg DEVIATIONS.md)");
            }
            Ok(Some("node and way records identical as osmium decodes them; relation differences within 10% tolerance; orphan handling attribution not independently proved".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn merged(first: [u64; 3], second: [u64; 3]) -> Merged {
        Merged {
            first,
            second,
            ..Merged::default()
        }
    }

    #[test]
    fn the_verdict_needs_paired_objects_and_bounded_relation_differences() {
        let ok = Classified {
            one_sided: [0; 3],
            differs: [0, 0, 1],
        };
        assert_eq!(judge(&merged([5, 3, 10], [5, 3, 10]), &ok), None);
        assert!(judge(&merged([5, 3, 10], [5, 3, 9]), &ok).is_some());
        let one_sided = Classified {
            one_sided: [1, 0, 0],
            differs: [0; 3],
        };
        assert!(judge(&merged([5, 3, 10], [5, 3, 10]), &one_sided).is_some());
        let way = Classified {
            one_sided: [0; 3],
            differs: [0, 1, 0],
        };
        assert!(judge(&merged([5, 3, 10], [5, 3, 10]), &way).is_some());
        let many = Classified {
            one_sided: [0; 3],
            differs: [0, 0, 2],
        };
        assert!(judge(&merged([5, 3, 10], [5, 3, 10]), &many).is_some());
    }

    #[test]
    fn fmt_num_thousands() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(123), "123");
        assert_eq!(fmt_num(1234), "1,234");
        assert_eq!(fmt_num(59_152_282), "59,152,282");
    }

    #[test]
    fn fmt_size_units() {
        assert_eq!(fmt_size(500), "500 B");
        // Decimal units, matching the sizes the other tools print.
        assert_eq!(fmt_size(2_500), "2 KB");
        assert_eq!(fmt_size(5_500_000), "5 MB");
        assert_eq!(fmt_size(3_000_000_000), "3.0 GB");
        assert_eq!(fmt_size(3 * 1024 * 1024 * 1024), "3.2 GB");
    }
}
