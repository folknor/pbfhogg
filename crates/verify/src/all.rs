//! all: every check in suite order, each reported on its own line.
//!
//! A failing check does not stop the suite; an interrupt or an unsettled
//! launch or verifier error does. Absent optional inputs produce explicit skips.
use std::path::Path;
use std::time::Instant;

use crate::checks;
use crate::error::Error;
use crate::harness::{CheckStatus, Harness};
use crate::output;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub(crate) passed: u32,
    pub(crate) failed: u32,
    pub(crate) environment: u32,
    pub(crate) skipped: u32,
}

impl Counts {
    fn skip(&mut self, name: &str, why: &str) {
        self.skipped += 1;
        output::result(&format!("{name}: SKIPPED ({why})"));
    }

    fn add(&mut self, status: CheckStatus) {
        match status {
            CheckStatus::Passed => self.passed += 1,
            CheckStatus::Failed => self.failed += 1,
            CheckStatus::Environment => self.environment += 1,
        }
    }
}

pub(crate) fn run(
    h: &mut Harness,
    pbf: &Path,
    direct_io: bool,
    bbox: Option<&pbfhogg_devkit::Bbox>,
    polygons: &[crate::extract_config::PolygonInput],
    osc: Option<&Path>,
    locations_base: Result<&Path, &str>,
) -> Result<Counts, Error> {
    let start = Instant::now();
    let mut counts = Counts::default();
    counts.add(h.run_check("sort", |h| checks::sort::run(h, pbf, direct_io))?);
    counts.add(h.run_check("cat", |h| checks::cat::run(h, pbf, direct_io))?);
    if bbox.is_some() || !polygons.is_empty() {
        counts.add(h.run_check("extract", |h| {
            checks::extract::run(h, pbf, bbox, polygons, direct_io)
        })?);
    } else {
        counts.skip("extract", "no bbox or dataset polygon configured");
    }
    if let Some(bbox) = bbox {
        counts.add(h.run_check("multi-extract", |h| {
            checks::multi_extract::run(
                h,
                pbf,
                bbox,
                polygons,
                crate::tunables::MULTI_EXTRACT_REGIONS,
                direct_io,
            )
        })?);
    } else {
        counts.skip("multi-extract", "no bbox configured");
    }
    counts.add(h.run_check("tags-filter", |h| {
        checks::tags_filter::run(h, pbf, direct_io)
    })?);
    counts.add(h.run_check("getid-removeid", |h| {
        checks::getid_removeid::run(h, pbf, direct_io)
    })?);
    counts.add(h.run_check("getparents", |h| checks::getparents::run(h, pbf, direct_io))?);
    counts.add(h.run_check("add-locations-to-ways", |h| {
        checks::add_locations::run(h, pbf, crate::AltwMode::All, direct_io)
    })?);
    counts.add(h.run_check("check-refs", |h| checks::check_refs::run(h, pbf))?);
    counts.add(h.run_check("check-ids", |h| checks::check_ids::run(h, pbf, direct_io))?);
    counts.add(h.run_check("repack", |h| checks::repack::run(h, pbf, direct_io))?);
    counts.add(h.run_check("degrade", |h| checks::degrade::run(h, pbf, direct_io))?);
    if let Some(osc) = osc {
        counts.add(h.run_check("apply-changes", |h| {
            checks::apply_changes::run(h, pbf, osc, direct_io, false)
        })?);
        match locations_base {
            Ok(base) => counts.add(h.run_check("apply-changes -l", |h| {
                checks::apply_changes::run(h, base, osc, direct_io, true)
            })?),
            Err(why) => counts.skip("apply-changes -l", why),
        }
    } else {
        counts.skip("apply-changes", "no OSC configured");
        counts.skip("apply-changes -l", "no OSC configured");
    }
    if let Some(osc) = osc {
        counts.add(h.run_check("merge-changes", |h| {
            checks::merge_changes::run(h, &[osc.to_path_buf()])
        })?);
        counts.add(h.run_check("tags-filter --input-kind osc", |h| {
            checks::tags_filter_osc::run(h, osc)
        })?);
    } else {
        counts.skip("merge-changes", "no OSC configured");
        counts.skip("tags-filter --input-kind osc", "no OSC configured");
    }
    if let Some(osc) = osc {
        counts.add(h.run_check("diff --format osc", |h| {
            checks::derive_changes::run(h, pbf, osc, direct_io)
        })?);
    } else {
        counts.skip("diff --format osc", "no OSC configured");
    }
    counts.add(h.run_check("renumber", |h| {
        checks::renumber::run(h, pbf, "input", None, true, direct_io)
    })?);
    if let Some(osc) = osc {
        counts.add(h.run_check("diff", |h| checks::diff::run(h, pbf, osc, direct_io))?);
    } else {
        counts.skip("diff", "no OSC configured");
    }
    output::result(&banner(&counts, start.elapsed()));
    Ok(counts)
}

fn banner(counts: &Counts, took: std::time::Duration) -> String {
    format!(
        "all done in {}: {} passed, {} failed, {} environment, {} skipped out of {}",
        output::duration(took),
        counts.passed,
        counts.failed,
        counts.environment,
        counts.skipped,
        counts.passed + counts.failed + counts.environment + counts.skipped
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn complete_suite_accounts_for_skips() {
        let counts = super::Counts {
            passed: 11,
            skipped: 8,
            ..Default::default()
        };
        let line = super::banner(&counts, std::time::Duration::ZERO);
        assert!(
            line.ends_with("11 passed, 0 failed, 0 environment, 8 skipped out of 19"),
            "{line}"
        );
    }
}
