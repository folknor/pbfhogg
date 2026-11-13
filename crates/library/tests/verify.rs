//! Own-process integration contract: only this test owns adopted workers.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
#[path = "../src/artefact.rs"]
mod artefact;
#[path = "../src/describe.rs"]
mod describe;
#[path = "../src/identity.rs"]
mod identity;
#[path = "../src/json.rs"]
mod json;
#[path = "../src/output.rs"]
mod output;
#[path = "../src/process.rs"]
mod process;
#[path = "../src/publication.rs"]
mod publication;
#[path = "../src/run.rs"]
mod run;
#[path = "../src/seed.rs"]
mod seed;
#[path = "../src/sidecar.rs"]
mod sidecar;
#[path = "../src/stream.rs"]
mod stream;
#[path = "../src/tunables.rs"]
mod tunables;

mod worker;

const EXE: &str = env!("CARGO_BIN_EXE_pbfhogg-library-test-worker");
#[test]
fn scenario_launch_and_evidence() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "scenario launch and evidence",
    );
    process::subreaper().unwrap();
    let owner = pbfhogg_test_scratch::scratch();
    let root = owner.path().join("tools");
    let mut scenario = describe::parse(br#"{"protocol":1,"scenarios":[{"id":"ok","description":"fixture","deadline_s":1,"roles":[{"id":"out-a","kind":"pbf","source":"output","owner":"a"}],"calls":[{"id":"a","label":"fixture","stream":["counters.v1"]}],"assertions":[{"id":"a.outcome","call":"a"},{"id":"whole","call":null}]}]}"#).unwrap().scenarios.remove(0);
    for mode in [
        "ok",
        "seeded",
        "deadline",
        "orphan",
        "flood",
        "stderr",
        "refusal",
        "layout",
        "request",
        "cwd",
        "missing-output",
        "assertion",
        "exit-one",
        "seal",
        "path",
        "timestamp",
        "symlink",
        "stream",
        "unknown-call",
        "fragment",
        "fifo-flood",
        "exit-invalid",
    ] {
        scenario.id = mode.into();
        scenario.roles[0].source = describe::RoleSource::Output {
            owner: "a".into(),
            seeded: mode == "seeded",
        };
        let report = run::verify(std::path::Path::new(EXE), &scenario, &[], &root).unwrap();
        assert!(!report.unestablished, "{mode}: {:?}", report.failures);
        if matches!(mode, "ok" | "seeded") {
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            assert!(!report.run_dir.exists());
            assert!(
                report.identity["plan"]["digest"]
                    .as_str()
                    .unwrap()
                    .starts_with("plan-v1:")
            );
        } else {
            let expected = match mode {
                "deadline" => "deadline",
                "orphan" => "residual children",
                "flood" => "stdout",
                "stderr" => "stderr",
                "refusal" => "fixture refusal",
                "layout" => "unexpected entry",
                "request" => "request.json was changed",
                "cwd" => "not empty",
                "missing-output" => "out-a",
                "assertion" => "exited 0 but",
                "exit-one" => "a.outcome",
                "seal" => "fixture seal",
                "path" => "run-specific path",
                "timestamp" => "process wall",
                "symlink" => "not a regular file",
                "stream" => "snapshot holds",
                "unknown-call" => "does not declare",
                "fragment" => "FIFO reader rejected",
                "fifo-flood" => "FIFO capture overflow",
                "exit-invalid" => "outside protocol",
                _ => unreachable!(),
            };
            assert!(
                report
                    .failures
                    .iter()
                    .any(|failure| failure.contains(expected)),
                "{mode} did not fail for {expected}: {:?}",
                report.failures
            );
            for name in [
                "stdout",
                "stderr",
                "stream.txt",
                "failures.txt",
                "identity.json",
            ] {
                assert!(
                    report.run_dir.join("evidence").join(name).is_file(),
                    "{mode}/{name}"
                );
            }
        }
    }
}
