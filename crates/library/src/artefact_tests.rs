#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

const DESCRIBE: &str = r#"{"protocol": 1, "scenarios": [
  {"id": "pair", "description": "two calls", "deadline_s": 60,
   "roles": [
     {"id": "out-a", "kind": "pbf", "source": "output", "owner": "a"},
     {"id": "side-a", "kind": "file", "source": "output", "owner": "a", "seeded": true},
     {"id": "out-b", "kind": "pbf", "source": "output", "owner": "b"},
     {"id": "tmp-a", "kind": "directory", "source": "scratch", "owner": "a"}
   ],
   "calls": [{"id": "a", "label": "first"}, {"id": "b", "label": "second"}],
   "assertions": [{"id": "a.outcome", "call": "a"}, {"id": "b.outcome", "call": "b"},
                  {"id": "whole", "call": null}]}
]}"#;

fn scenario() -> Scenario {
    describe::parse(DESCRIBE.as_bytes())
        .unwrap()
        .scenarios
        .remove(0)
}

/// A valid call document for `a`, every top-level field named.
const CALL: &str = r#"{"protocol": 1, "call": "a", "entry_us": 10, "return_us": 20, "seal_us": 30,
 "outcome": {"status": "ok"},
 "publications": [{"ordinal": 1, "decision": "authorized",
                   "members": [{"role": "out-a", "effect": "installed"}, {"role": "side-a", "effect": "restored"}]},
                  {"ordinal": 2, "decision": "refused_cancelled",
                   "members": [{"role": "out-a", "effect": "none"}]}],
 "cancel": {"result": "after_decision", "ordinal": 1},
 "seal": {"state": "sealed"},
 "snapshot": {"writer_blocks": 3, "pipeline wait_ns": 18446744073709551615},
 "stream": {"state": "sealed", "drops": 0},
 "context": {"parallelism": 4, "executor": "ambient"},
 "diagnostics": [{"at_us": 25, "kind": "note", "key": "sort.distrust_metadata", "message": "m", "truncated": false}],
 "assertions": [{"id": "a.outcome", "status": "pass", "detail": "line one\nline two"}]}"#;

fn call(text: &str) -> Result<CallDoc, String> {
    parse_call(text.as_bytes(), &scenario(), "a", &["/run/dir"])
}

fn edited(from: &str, to: &str) -> String {
    assert!(CALL.contains(from), "{from}");
    CALL.replacen(from, to, 1)
}

#[test]
fn a_valid_call_document_parses() {
    let doc = call(CALL).unwrap();
    assert_eq!((doc.entry_us, doc.return_us, doc.seal_us), (10, 20, 30));
    assert_eq!(doc.seal, Seal::Sealed);
    assert_eq!(doc.outcome, CallOutcome::Ok);
    assert_eq!(doc.diagnostics[0].key, "sort.distrust_metadata");
    assert_eq!(doc.diagnostics[0].at_us, 25);
    assert_eq!(doc.results[0].detail, "line one\nline two");
    assert_eq!(
        doc.publications[0].members,
        [
            ("out-a".to_owned(), Effect::Installed),
            ("side-a".to_owned(), Effect::Restored)
        ]
    );
    assert_eq!(doc.publications[1].decision, Decision::RefusedCancelled);
}

#[test]
fn readings_must_be_ordered_and_in_range() {
    let e = call(&edited(r#""return_us": 20"#, r#""return_us": 5"#)).unwrap_err();
    assert!(e.contains("not in order"), "{e}");
    let e = call(&edited(
        r#""seal_us": 30"#,
        r#""seal_us": 9223372036854775808"#,
    ))
    .unwrap_err();
    assert!(e.contains("seal_us"), "{e}");
}

#[test]
fn the_call_field_names_its_own_file() {
    let e = parse_call(CALL.as_bytes(), &scenario(), "b", &[]).unwrap_err();
    assert!(e.contains(r#""a", not "b""#), "{e}");
}

#[test]
fn a_seal_decides_its_snapshot() {
    let outstanding = edited(
        r#""seal": {"state": "sealed"}"#,
        r#""seal": {"state": "outstanding", "held": {"session": 1, "writer": 2}}"#,
    );
    let e = call(&outstanding).unwrap_err();
    assert!(e.contains("must be null unless the call sealed"), "{e}");
    let still_sealed = outstanding.replacen(
        r#""snapshot": {"writer_blocks": 3, "pipeline wait_ns": 18446744073709551615}"#,
        r#""snapshot": null"#,
        1,
    );
    let e = call(&still_sealed).unwrap_err();
    assert!(e.contains("open exactly when the call did not seal"), "{e}");
    let ok = still_sealed.replacen(
        r#""state": "sealed", "drops""#,
        r#""state": "open", "drops""#,
        1,
    );
    let doc = call(&ok).unwrap();
    assert_eq!(
        doc.seal,
        Seal::Outstanding(BTreeMap::from([
            ("session".to_owned(), 1),
            ("writer".to_owned(), 2)
        ]))
    );
    for (held, why) in [
        ("{}", "holds at least one value"),
        (r#"{"writer": 0}"#, "0 is outside 1 to"),
        (r#"{"Writer": 1}"#, "not an identifier"),
    ] {
        let e = call(&ok.replacen(r#"{"session": 1, "writer": 2}"#, held, 1)).unwrap_err();
        assert!(e.contains(why), "{held}: {e}");
    }
    assert_eq!(doc.snapshot, None);
    assert_eq!(doc.stream.state, SinkState::Open);
    let e = call(&edited(
        r#""state": "sealed", "drops""#,
        r#""state": "open", "drops""#,
    ))
    .unwrap_err();
    assert!(e.contains("but it is open and the seal is sealed"), "{e}");
    let sealed = call(CALL).unwrap();
    assert_eq!(
        sealed.snapshot.unwrap()["pipeline wait_ns"],
        u64::MAX,
        "the snapshot keeps its values"
    );
    let e = call(&edited(
        r#""seal": {"state": "sealed"}"#,
        r#""seal": {"state": "sealed", "held": {"writer": 1}}"#,
    ))
    .unwrap_err();
    assert!(e.contains("not allowed on a sealed seal"), "{e}");
    let refused = edited(
        r#""seal": {"state": "sealed"}"#,
        r#""seal": {"state": "refused", "reason": "busy", "held": {"writer": 1}}"#,
    );
    assert!(
        call(&refused)
            .unwrap_err()
            .contains("not allowed on a refused seal")
    );
    let e = call(&edited(r#""writer_blocks": 3"#, r#""a=b": 3"#)).unwrap_err();
    assert!(e.contains("not a counter name"), "{e}");
}

#[test]
fn publications_and_cancel_must_agree() {
    let e = call(&edited(r#""ordinal": 2"#, r#""ordinal": 3"#)).unwrap_err();
    assert!(e.contains("without gaps"), "{e}");
    let e = call(&edited(
        r#""members": [{"role": "out-a", "effect": "none"}]"#,
        r#""members": [{"role": "out-a", "effect": "displaced"}]"#,
    ))
    .unwrap_err();
    assert!(e.contains("leaves every member none"), "{e}");
    let e = call(&edited(
        r#""result": "after_decision", "ordinal": 1"#,
        r#""result": "after_decision", "ordinal": 2"#,
    ))
    .unwrap_err();
    assert!(e.contains("not an authorized publication"), "{e}");
    let e = call(&edited(
        r#""decision": "refused_cancelled""#,
        r#""decision": "authorized""#,
    ))
    .unwrap_err();
    assert!(e.contains("authorized after the cancel"), "{e}");
    let pending = edited(
        r#""result": "after_decision", "ordinal": 1"#,
        r#""result": "no_publication_pending""#,
    );
    assert!(call(&pending).is_ok());
    let e = call(&edited(
        r#""result": "after_decision", "ordinal": 1"#,
        r#""result": "no_publication_pending", "ordinal": 1"#,
    ))
    .unwrap_err();
    assert!(e.contains("unknown key \"ordinal\""), "{e}");
}

#[test]
fn a_member_is_an_output_role_of_its_call_once_per_publication() {
    let member = r#"[{"role": "out-a", "effect": "none"}]"#;
    for (members, why) in [
        ("[]", "at least one member"),
        (
            r#"[{"role": "out-b", "effect": "none"}]"#,
            "\"out-b\" is not an output role of call \"a\"",
        ),
        (
            r#"[{"role": "tmp-a", "effect": "none"}]"#,
            "not an output role",
        ),
        (
            r#"[{"role": "out-a", "effect": "none"}, {"role": "out-a", "effect": "none"}]"#,
            "a member twice",
        ),
        (r#"[{"role": "out-a", "effect": "gone"}]"#, "unknown effect"),
        (
            r#"[{"role": "out-a", "effect": "none", "installed": true}]"#,
            "unknown key \"installed\"",
        ),
    ] {
        let e = call(&edited(member, members)).unwrap_err();
        assert!(e.contains(why), "{members}: {e}");
    }
}

#[test]
fn context_and_plan_stay_in_the_plain_value_domain() {
    let e = call(&edited(r#""parallelism": 4"#, r#""parallelism": 4.0"#)).unwrap_err();
    assert!(e.contains("expected an integer"), "{e}");
    let e = call(&edited(
        r#""executor": "ambient""#,
        r#""executor": "/run/dir/x""#,
    ))
    .unwrap_err();
    assert!(e.contains("run-specific path"), "{e}");
    let e = call(&edited(r#""executor": "ambient""#, r#""/run/dir": 1"#)).unwrap_err();
    assert!(e.contains("key contains the run-specific path"), "{e}");
    let e = call(&edited(r#""parallelism": 4, "executor": "ambient""#, "")).unwrap_err();
    assert!(e.contains("an empty object"), "{e}");

    let scenario = scenario();
    let doc = |plan: &str| {
        format!(
            r#"{{"protocol": 1, "scenario": "pair", "plan": {plan}, "assertions": [], "diagnostics": [{{"at_us": 0, "kind": "warn", "key": "setup", "message": "", "truncated": false}}]}}"#
        )
    };
    let ok = doc(r#"{"calls": {"a": {"cmd": "cat"}, "b": {"cmd": "cat"}}, "order": [1, -2]}"#);
    let parsed = parse_scenario(ok.as_bytes(), &scenario, &[]).unwrap();
    assert_eq!(parsed.diagnostics[0].kind, "warn");
    for (plan, why) in [
        (
            r#"{"calls": {"a": {"cmd": "cat"}}}"#,
            "call \"b\" is missing",
        ),
        (
            r#"{"calls": {"a": {"cmd": "cat"}, "b": {}}}"#,
            "an empty object",
        ),
        (
            r#"{"calls": {"a": {"x": 1}, "b": {"x": 1}, "c": {"x": 1}}}"#,
            "not a declared call",
        ),
        (
            r#"{"calls": {"a": {"x": 1}, "b": {"x": -0}}}"#,
            "expected an integer",
        ),
        (
            r#"{"calls": {"a": {"x": 1}, "b": {"x": 1, "x": 2}}}"#,
            "duplicate key",
        ),
        (
            r#"{"calls": {"a": {"x": "a\u0000"}, "b": {"x": 1}}}"#,
            "NUL",
        ),
        (r#"{}"#, "missing key \"calls\""),
    ] {
        let e = parse_scenario(doc(plan).as_bytes(), &scenario, &[]).unwrap_err();
        assert!(e.contains(why), "{plan}: {e}");
    }
}

#[test]
fn diagnostics_carry_kind_key_and_a_bounded_message() {
    let e = call(&edited(r#""kind": "note""#, r#""kind": "Note""#)).unwrap_err();
    assert!(e.contains("not an identifier"), "{e}");
    let long = "m".repeat(4001);
    let e = call(&edited(
        r#""message": "m""#,
        &format!(r#""message": "{long}""#),
    ))
    .unwrap_err();
    assert!(e.contains("0 to 4000 bytes"), "{e}");
    let e = call(&edited(r#", "truncated": false"#, "")).unwrap_err();
    assert!(e.contains("missing key \"truncated\""), "{e}");
    let e = call(&edited(r#""at_us": 25, "#, "")).unwrap_err();
    assert!(e.contains("missing key \"at_us\""), "{e}");
    // Setup's diagnostics are never a call's: one before entry_us fails.
    let e = call(&edited(r#""at_us": 25"#, r#""at_us": 9"#)).unwrap_err();
    assert!(e.contains("9 is before the call's entry_us 10"), "{e}");
    // After the seal is the call's own still.
    assert!(call(&edited(r#""at_us": 25"#, r#""at_us": 99"#)).is_ok());
}

#[test]
fn results_are_closed_and_unique() {
    let e = call(&edited(r#""status": "pass""#, r#""status": "skip""#)).unwrap_err();
    assert!(e.contains("unknown status"), "{e}");
    let twice = edited(
        r#"[{"id": "a.outcome", "status": "pass", "detail": "line one\nline two"}]"#,
        r#"[{"id": "a.outcome", "status": "pass", "detail": ""}, {"id": "a.outcome", "status": "fail", "detail": ""}]"#,
    );
    assert!(call(&twice).unwrap_err().contains("reported twice"));
    let e = call(&edited(
        r#""stream": {"state": "sealed""#,
        r#""stream": {"state": "draining""#,
    ))
    .unwrap_err();
    assert!(e.contains("unknown stream state"), "{e}");
}

#[test]
fn the_inventory_must_be_reported_exactly_where_declared() {
    let scenario = scenario();
    let result = |id: &str| Outcome {
        id: id.to_owned(),
        status: Status::Pass,
        detail: String::new(),
    };
    let ok = artefact_ok(
        &scenario,
        &[result("whole")],
        &[("a", &["a.outcome"]), ("b", &["b.outcome"])],
    );
    assert!(ok.is_empty(), "{ok:?}");
    let failures = artefact_ok(
        &scenario,
        &[],
        &[("a", &["a.outcome", "b.outcome", "other"]), ("b", &[])],
    );
    assert!(
        failures
            .iter()
            .any(|f| f.contains("scenario.json has no result for \"whole\""))
    );
    assert!(
        failures
            .iter()
            .any(|f| f.contains("which belongs to calls/b.json"))
    );
    assert!(
        failures
            .iter()
            .any(|f| f.contains("\"other\", which describe does not declare"))
    );
    assert!(
        failures
            .iter()
            .any(|f| f.contains("calls/b.json has no result for \"b.outcome\""))
    );

    fn artefact_ok(scenario: &Scenario, top: &[Outcome], calls: &[(&str, &[&str])]) -> Vec<String> {
        let calls: Vec<(String, Vec<Outcome>)> = calls
            .iter()
            .map(|(c, ids)| {
                (
                    (*c).to_owned(),
                    ids.iter()
                        .map(|id| Outcome {
                            id: (*id).to_owned(),
                            status: Status::Pass,
                            detail: String::new(),
                        })
                        .collect(),
                )
            })
            .collect();
        inventory_failures(scenario, top, &calls)
    }
}

#[test]
fn the_tree_admits_exactly_its_documents_and_never_follows_a_link() {
    let scratch = pbfhogg_test_scratch::scratch();
    let root = scratch.path();
    let scenario = scenario();
    let dir = root.join("artefacts");
    std::fs::create_dir_all(dir.join(CALLS_DIR)).unwrap();
    std::fs::write(dir.join(SCENARIO_FILE), b"{}").unwrap();
    std::fs::write(dir.join("calls/a.json"), b"{}").unwrap();
    let e = read_tree(&dir, &scenario).err().unwrap();
    assert!(e.contains("calls/b.json is missing"), "{e}");
    std::os::unix::fs::symlink(root.join("elsewhere"), dir.join("calls/b.json")).unwrap();
    let e = read_tree(&dir, &scenario).err().unwrap();
    assert!(e.contains("calls/b.json is not a regular file"), "{e}");
    std::fs::remove_file(dir.join("calls/b.json")).unwrap();
    std::fs::write(dir.join("calls/b.json"), b"{}").unwrap();
    assert!(read_tree(&dir, &scenario).is_ok());
    std::fs::write(dir.join("extra"), b"").unwrap();
    let e = read_tree(&dir, &scenario).err().unwrap();
    assert!(e.contains("unexpected entry \"extra\""), "{e}");
}

#[test]
fn a_regular_read_refuses_links_specials_and_oversize() {
    let scratch = pbfhogg_test_scratch::scratch();
    let root = scratch.path();
    let file = root.join("f");
    std::fs::write(&file, b"12345").unwrap();
    assert_eq!(read_regular(&file, 5).unwrap(), b"12345");
    assert!(
        read_regular(&file, 4)
            .unwrap_err()
            .contains("over the 4-byte limit")
    );
    let link = root.join("l");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    assert!(
        read_regular(&link, 5).is_err(),
        "a symlink is never followed"
    );
    assert!(
        read_regular(root, 5)
            .unwrap_err()
            .contains("not a regular file")
    );
    // A FIFO with no writer would block an ordinary open; this one returns.
    let fifo = root.join("p");
    let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    assert!(
        read_regular(&fifo, 5)
            .unwrap_err()
            .contains("not a regular file")
    );
}

#[test]
fn a_refusal_names_its_reason() {
    assert_eq!(
        parse_refusal(br#"{"protocol": 1, "reason": "role out-a is bound twice"}"#).unwrap(),
        "role out-a is bound twice"
    );
    assert!(parse_refusal(br#"{"protocol": 1, "reason": "two\nlines"}"#).is_err());
}

#[test]
fn tree_roots_and_call_directories_cannot_be_links() {
    let scratch = pbfhogg_test_scratch::scratch();
    let dir = scratch.path().join("artefacts");
    let calls = scratch.path().join("real-calls");
    std::fs::create_dir(&dir).unwrap();
    std::fs::create_dir(&calls).unwrap();
    std::fs::write(dir.join(SCENARIO_FILE), b"{}").unwrap();
    std::os::unix::fs::symlink(&calls, dir.join(CALLS_DIR)).unwrap();
    assert!(
        read_tree(&dir, &scenario())
            .err()
            .unwrap()
            .contains("calls is not a directory")
    );
    let alias = scratch.path().join("alias");
    std::os::unix::fs::symlink(&dir, &alias).unwrap();
    assert!(
        read_tree(&alias, &scenario())
            .err()
            .unwrap()
            .contains("not a directory")
    );
}

#[test]
fn tree_applies_the_document_limit_to_scenario_and_calls() {
    let scratch = pbfhogg_test_scratch::scratch();
    let dir = scratch.path();
    std::fs::create_dir(dir.join(CALLS_DIR)).unwrap();
    for name in [SCENARIO_FILE, "calls/a.json", "calls/b.json"] {
        std::fs::write(dir.join(name), b"{}").unwrap();
    }
    for name in [SCENARIO_FILE, "calls/a.json"] {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join(name))
            .unwrap();
        file.set_len(FILE_LIMIT as u64 + 1).unwrap();
        let error = read_tree(dir, &scenario()).err().unwrap();
        assert!(error.contains("over the 16777216-byte limit"), "{error}");
        assert!(error.contains(name), "{error}");
        file.set_len(0).unwrap();
    }
}

#[test]
fn refusal_read_is_bounded_and_does_not_follow_a_link() {
    let scratch = pbfhogg_test_scratch::scratch();
    let refusal = scratch.path().join(REFUSAL_FILE);
    std::fs::write(&refusal, br#"{"protocol":1,"reason":"no input"}"#).unwrap();
    assert_eq!(read_refusal(&refusal).unwrap(), "no input");
    let alias = scratch.path().join("alias");
    std::os::unix::fs::symlink(&refusal, &alias).unwrap();
    assert!(read_refusal(&alias).is_err());
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&refusal)
        .unwrap();
    file.set_len(REFUSAL_LIMIT as u64 + 1).unwrap();
    assert!(
        read_refusal(&refusal)
            .unwrap_err()
            .contains("over the 65536-byte limit")
    );
}
