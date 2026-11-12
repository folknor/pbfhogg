//! The scenarios the harness declares, one file per family, and the
//! judgments their calls share. Each scenario is a [`Workload`], from which
//! the runner derives the plan, the declaration and every call, and an
//! [`Observation`] that only watches and judges.

mod cancel;
mod concurrent_cat;
mod error_path;
mod executor;
pub(crate) mod file_assertions;
mod hotpath_cat;
mod nofile;
mod page_cache;
mod repeat_cat;
mod seeded_replace;

use crate::protocol::{AssertionDecl, Outcome, ScenarioDecl};
use crate::runner::{CallRecord, Observation};
use crate::workload::Workload;
use pbfhogg::{Decision, Effect};

/// The scenario-level assertion every scenario carries: every call's
/// diagnostic senders were released when its collection closed, no call
/// diagnostic arrived after that, and no collector, setup's included,
/// dropped an event over its budget.
pub(crate) const DIAGNOSTICS_CLOSED: &str = "call-diagnostics-closed";

/// One scenario: its workload, and a fresh observation of one run.
pub(crate) trait Scenario {
    fn workload(&self) -> Workload;
    fn observe(&self) -> Box<dyn Observation>;
}

/// Every scenario, in describe order. The executable registry test runs each
/// declaration using its dataset, generated and seeded role descriptions.
pub(crate) fn all() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(repeat_cat::RepeatCat),
        Box::new(concurrent_cat::ConcurrentCat),
        Box::new(executor::FixedResourcesExecutor),
        Box::new(cancel::CancelCat::PRE_DECISION),
        Box::new(cancel::CancelCat::POST_DECISION),
        Box::new(nofile::NofileCatRefused),
        Box::new(nofile::NofileMultiExtractRefused),
        Box::new(nofile::NofileAltwExternal),
        Box::new(error_path::ErrorPath),
        Box::new(seeded_replace::SeededReplace),
        Box::new(page_cache::PageCacheKeep),
        Box::new(hotpath_cat::HotpathCat),
    ]
}

/// The scenario named `id`.
pub(crate) fn find(id: &str) -> Option<Box<dyn Scenario>> {
    all().into_iter().find(|s| s.workload().id == id)
}

/// A workload's full declaration, the shared assertions included.
pub(crate) fn declared(workload: &Workload) -> ScenarioDecl {
    let mut decl = workload.decl();
    decl.assertions.push(AssertionDecl {
        id: DIAGNOSTICS_CLOSED,
        call: None,
        covers: &[],
    });
    decl
}

/// The call published exactly once, `decision`, its one member `role` with
/// `effect`.
pub(crate) fn published_once(
    record: &CallRecord,
    decision: Decision,
    role: &str,
    effect: Effect,
) -> bool {
    matches!(
        record.publications.as_slice(),
        [p] if p.ordinal == 1 && p.decision == decision && p.members == [(role, effect)]
    )
}

/// A one-line account of a call for an assertion's detail.
pub(crate) fn account(record: &CallRecord) -> String {
    let rows: Vec<String> = record
        .publications
        .iter()
        .map(|p| format!("{} {:?} {:?}", p.ordinal, p.decision, p.members))
        .collect();
    format!(
        "outcome {}, publications [{}], cancel {:?}, sealed {}",
        record.outcome.to_json(),
        rows.join("; "),
        record.cancel,
        record.sealed()
    )
}

/// `<call>.outcome` for a call that must return ok, cancel nothing and
/// install its one output.
pub(crate) fn installed_ok(id: &'static str, record: &CallRecord, role: &str) -> Outcome {
    let ok = record.outcome.is_ok()
        && record.cancel.is_none()
        && published_once(record, Decision::Authorized, role, Effect::Installed);
    Outcome::judge(id, ok, account(record))
}

/// Whether `path` is absent.
pub(crate) fn absent(path: &std::path::Path) -> std::io::Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}

/// Assertions in declaration shape.
pub(crate) const fn assertion(
    id: &'static str,
    call: Option<&'static str>,
    covers: &'static [&'static str],
) -> AssertionDecl {
    AssertionDecl { id, call, covers }
}

#[cfg(test)]
mod absence_tests {
    #[test]
    fn only_not_found_means_absent() {
        let scratch = pbfhogg_test_scratch::scratch();
        let file = scratch.child("file");
        std::fs::write(&file, b"x").expect("write");
        assert!(!super::absent(&file).expect("present"));
        assert!(super::absent(&scratch.child("missing")).expect("absent"));
        assert!(super::absent(&file.join("child")).is_err());
        #[cfg(unix)]
        {
            let link = scratch.child("link");
            std::os::unix::fs::symlink(scratch.child("missing"), &link).expect("link");
            assert!(!super::absent(&link).expect("dangling symlink is present"));
        }
    }
}
