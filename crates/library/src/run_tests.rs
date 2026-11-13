#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
fn scenario() -> Scenario {
    crate::describe::parse(br#"{"protocol":1,"scenarios":[{"id":"s","description":"fixture","deadline_s":1,"roles":[],"calls":[{"id":"a","label":"fixture"}],"assertions":[]}]}"#).unwrap().scenarios.remove(0)
}
#[test]
fn uncertain_cleanup_retains_capture_without_an_artefact_verdict() {
    let owner = pbfhogg_test_scratch::scratch();
    let scenario = scenario();
    let p = prepare(&scenario, &BTreeMap::new(), owner.path(), 0).unwrap();
    // An invalid artefact tree must not be read on this lifecycle failure.
    std::fs::remove_dir(&p.artefacts).unwrap();
    std::os::unix::fs::symlink("/not-readable", &p.artefacts).unwrap();
    let report = finish(
        &scenario,
        &[],
        &p,
        &Err(process::Failure::Unestablished(Box::new(
            process::Problem {
                reason: "fixture enumeration failure".into(),
                captures: process::Captures {
                    out: process::Capture {
                        bytes: b"out".to_vec(),
                        overflow: true,
                    },
                    err: process::Capture {
                        bytes: b"err".to_vec(),
                        overflow: false,
                    },
                    markers: process::Capture {
                        bytes: b"markers".to_vec(),
                        overflow: true,
                    },
                },
            },
        ))),
    )
    .unwrap();
    assert!(report.unestablished);
    assert!(report.results.is_empty());
    assert!(report.identity.get("plan").is_none());
    assert!(report.failures[0].contains("fixture enumeration failure"));
    assert_eq!(
        std::fs::read(report.run_dir.join("evidence/stdout")).unwrap(),
        b"out"
    );
    assert_eq!(
        std::fs::read(report.run_dir.join("evidence/stderr")).unwrap(),
        b"err"
    );
    assert_eq!(
        std::fs::read(report.run_dir.join("evidence/stream.txt")).unwrap(),
        b"markers"
    );
}
#[test]
fn ids_are_never_reused_after_a_run_is_removed() {
    let owner = pbfhogg_test_scratch::scratch();
    let first = allocate(owner.path()).unwrap();
    std::fs::remove_dir(&first).unwrap();
    let second = allocate(owner.path()).unwrap();
    assert_ne!(first, second);
}
#[test]
fn evidence_does_not_follow_a_preinstalled_directory_link() {
    let owner = pbfhogg_test_scratch::scratch();
    let p = prepare(&scenario(), &BTreeMap::new(), owner.path(), 0).unwrap();
    let outside = owner.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, p.run_dir.join("evidence")).unwrap();
    let report = finish(
        &scenario(),
        &[],
        &p,
        &Err(process::Failure::Unestablished(Box::new(
            process::Problem {
                reason: "fixture".into(),
                captures: process::Captures::default(),
            },
        ))),
    )
    .unwrap();
    assert!(
        report.unestablished,
        "evidence failure must not erase the lifecycle class"
    );
    assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
    assert!(
        report
            .failures
            .iter()
            .any(|s| s.contains("retain evidence"))
    );
}

#[test]
fn identities_exclude_run_paths_and_retain_both_input_digests() {
    let owner = pbfhogg_test_scratch::scratch();
    let mut scenario = scenario();
    scenario.roles.push(crate::describe::Role {
        id: "generated".into(),
        kind: RoleKind::File,
        source: RoleSource::Generated,
    });
    scenario.roles.push(crate::describe::Role {
        id: "seeded".into(),
        kind: RoleKind::File,
        source: RoleSource::Output {
            owner: "a".into(),
            seeded: true,
        },
    });
    let inputs = [pbfhogg_devkit::ResolvedInput {
        role: "input".into(),
        variant: "indexed".into(),
        path: owner.path().join("unused.pbf"),
        expected_digest: None,
        observed_digest: "observed".into(),
    }];
    let j = Judged {
        scenario_results: Vec::new(),
        plan: crate::json::parse("plan", br#"{"calls":{"a":{"command":"fixture"}}}"#).unwrap(),
        _scenario_diagnostics: Vec::new(),
        calls: Vec::new(),
    };
    let make = || {
        let p = prepare(&scenario, &BTreeMap::new(), owner.path(), 0).unwrap();
        std::fs::write(&p.paths["generated"], b"same bytes").unwrap();
        identify(&j, &scenario, &inputs, &p.paths).unwrap()
    };
    let first = make();
    assert_eq!(first, make());
    assert!(first["datasets"]["input"]["expected_digest"].is_null());
    assert_eq!(first["datasets"]["input"]["observed_digest"], "observed");
    assert_eq!(first["seeds"]["seeded"], "seed-v1:file");
    let mut pinned = inputs.clone();
    pinned[0].expected_digest = Some("expected".into());
    assert_eq!(
        input_identities(&pinned)["input"]["expected_digest"],
        "expected"
    );
    assert_eq!(
        input_identities(&pinned)["input"]["observed_digest"],
        "observed"
    );
}
