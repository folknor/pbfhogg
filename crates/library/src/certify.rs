//! `pbfhogg-library verify --all`: each required capability classified from the
//! results that cover it, the certification verdict, and its record.
//!
//! Coverage is declared in the describe document: an assertion's `covers`,
//! and a call's `stream`, which is the coverage of its `brokkr.stream`
//! result. No other result covers anything. A result counts only as one of
//! the run that reported it: a run whose artefact tree was invalid, or that
//! did not run, reports none, so everything it declares is missing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::artefact::{Outcome, Status};
use super::describe::Scenario;
const STREAM_RESULT: &str = "brokkr.stream";

/// The record's kind and version, so a reader can tell what it holds.
const RECORD_VERSION: u32 = 1;

/// The applied requirement list: capability id to its one-line meaning,
/// taken once, so what is printed, classified, hashed and stored is one
/// value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Requirements(BTreeMap<String, String>);

impl Requirements {
    pub(crate) fn new(list: BTreeMap<String, String>) -> Self {
        Self(list)
    }

    pub(crate) fn list(&self) -> &BTreeMap<String, String> {
        &self.0
    }

    /// `require-v1:` and the XXH3-128 (seed 0, lowercase hex) of the
    /// required ids in byte order, each followed by a newline. Ids only: a
    /// capability whose meaning changes gets a new id, so the meaning text
    /// is description, kept in the record but not part of the identity.
    pub(crate) fn digest(&self) -> String {
        let mut bytes = Vec::new();
        for id in self.0.keys() {
            bytes.extend_from_slice(id.as_bytes());
            bytes.push(b'\n');
        }
        format!("require-v1:{:032x}", xxhash_rust::xxh3::xxh3_128(&bytes))
    }
}

/// What one scenario's run of the sweep came to.
#[derive(Debug, Clone)]
pub(crate) struct Attempt<'a> {
    pub(crate) scenario: &'a Scenario,
    pub(crate) identity: serde_json::Value,
    pub(crate) unestablished: bool,
    pub(crate) passed: bool,
    /// The results the run reported (none when it did not run or its
    /// artefacts were invalid).
    pub(crate) results: Vec<(Option<String>, Outcome)>,
    /// Every failure line, as printed.
    pub(crate) failures: Vec<String>,
    /// The run directory, when it was kept.
    pub(crate) kept: Option<PathBuf>,
}

impl<'a> Attempt<'a> {
    pub(crate) fn of_report(scenario: &'a Scenario, report: &super::run::Report) -> Self {
        Self {
            scenario,
            identity: report.identity.clone(),
            unestablished: report.unestablished,
            passed: report.failures.is_empty() && !report.unestablished,
            results: report.results.clone(),
            failures: report.failures.clone(),
            kept: (!report.failures.is_empty()).then(|| report.run_dir.clone()),
        }
    }

    /// A scenario whose run did not come to a verdict: `error` is its one
    /// failure, and it reported nothing.
    pub(crate) fn errored(scenario: &'a Scenario, error: String) -> Self {
        Self {
            scenario,
            identity: serde_json::Value::Null,
            unestablished: false,
            passed: false,
            results: Vec::new(),
            failures: vec![error],
            kept: None,
        }
    }
}

/// How a required capability came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// Every declared covering result passed, and there is at least one.
    Passed,
    /// A covering result failed, was unevaluated, or is missing.
    Failed,
    /// Nothing declared covers it, as on a commit that predates it.
    Unsupported,
}

impl Class {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Unsupported => "unsupported",
        }
    }
}

/// One declared covering result and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Covering {
    pub(crate) scenario: String,
    pub(crate) call: Option<String>,
    pub(crate) result: String,
    /// `None`: the run reported no such result.
    pub(crate) status: Option<Status>,
}

impl Covering {
    fn status_name(&self) -> &'static str {
        self.status.map_or("missing", Status::name)
    }
}

/// A required capability's classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) id: String,
    pub(crate) meaning: String,
    pub(crate) class: Class,
    pub(crate) covering: Vec<Covering>,
}

/// The results `scenario` declares as covering `capability`: each assertion
/// listing it in `covers`, and the `brokkr.stream` of each call listing it
/// in `stream`.
fn declared(scenario: &Scenario, capability: &str) -> Vec<(Option<String>, String)> {
    let assertions = scenario
        .assertions
        .iter()
        .filter(|a| a.covers.iter().any(|c| c == capability))
        .map(|a| (a.call.clone(), a.id.clone()));
    let streams = scenario
        .calls
        .iter()
        .filter(|c| c.stream.iter().any(|s| s == capability))
        .map(|c| (Some(c.id.clone()), STREAM_RESULT.to_owned()));
    assertions.chain(streams).collect()
}

/// What the run reported for `(call, result)`: `None` when nothing, else the
/// first status that is not a pass among every match (a duplicate already
/// fails the run's inventory), else pass.
fn reported(
    results: &[(Option<String>, Outcome)],
    call: Option<&str>,
    result: &str,
) -> Option<Status> {
    let mut found = None;
    for (c, outcome) in results {
        if c.as_deref() == call && outcome.id == result {
            if outcome.status != Status::Pass {
                return Some(outcome.status);
            }
            found = Some(Status::Pass);
        }
    }
    found
}

/// Classify every required capability over the sweep's attempts: failed
/// when any covering result is not a pass or is missing; else passed when
/// one passed; else unsupported.
pub(crate) fn classify(require: &Requirements, attempts: &[Attempt<'_>]) -> Vec<Verdict> {
    require
        .list()
        .iter()
        .map(|(id, meaning)| {
            let mut covering = Vec::new();
            for attempt in attempts {
                for (call, result) in declared(attempt.scenario, id) {
                    let status = reported(&attempt.results, call.as_deref(), &result);
                    covering.push(Covering {
                        scenario: attempt.scenario.id.clone(),
                        call,
                        result,
                        status,
                    });
                }
            }
            let class = if covering.is_empty() {
                Class::Unsupported
            } else if covering.iter().all(|c| c.status == Some(Status::Pass)) {
                Class::Passed
            } else {
                Class::Failed
            };
            Verdict {
                id: id.clone(),
                meaning: meaning.clone(),
                class,
                covering,
            }
        })
        .collect()
}

/// Capability ids some scenario declares coverage for that the requirement
/// list does not name: shown, never judged.
pub(crate) fn not_required(require: &Requirements, attempts: &[Attempt<'_>]) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for attempt in attempts {
        let scenario = attempt.scenario;
        let covered = scenario
            .assertions
            .iter()
            .flat_map(|a| a.covers.iter())
            .chain(scenario.calls.iter().flat_map(|c| c.stream.iter()));
        for id in covered {
            if !require.list().contains_key(id) {
                ids.insert(id.clone());
            }
        }
    }
    ids.into_iter().collect()
}

/// Certified exactly when every scenario passed and every required
/// capability passed.
pub(crate) fn certified(attempts: &[Attempt<'_>], verdicts: &[Verdict]) -> bool {
    attempts.iter().all(|a| a.passed && !a.unestablished)
        && verdicts.iter().all(|v| v.class == Class::Passed)
}

/// Build one record from exactly the requirements and attempts that were judged.
pub(crate) fn record(
    require: &Requirements,
    attempts: &[Attempt<'_>],
    harness_digest: &str,
) -> serde_json::Value {
    let verdicts = classify(require, attempts);
    serde_json::json!({
        "record": "pbfhogg-library-certification",
        "version": RECORD_VERSION,
        "harness": {"xxh128": harness_digest},
        "complete": !attempts.iter().any(|a| a.unestablished),
        "certified": certified(attempts, &verdicts),
        "require": {"digest": require.digest(), "capabilities": require.list()},
        "scenarios": attempts.iter().map(|a| serde_json::json!({
            "id": a.scenario.id, "passed": a.passed,
            "unestablished": a.unestablished, "failures": a.failures,
            "kept": a.kept, "identity": a.identity,
            "results": a.results.iter().map(|(call, o)| serde_json::json!({
                "call": call, "id": o.id, "status": o.status.name(),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "capabilities": verdicts.iter().map(|v| serde_json::json!({
            "id": v.id, "meaning": v.meaning, "class": v.class.name(),
            "covering": v.covering.iter().map(|c| serde_json::json!({
                "scenario": c.scenario, "call": c.call,
                "result": c.result, "status": c.status_name(),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "not_required": not_required(require, attempts),
    })
}

#[cfg(test)]
#[path = "certify_tests.rs"]
mod tests;
