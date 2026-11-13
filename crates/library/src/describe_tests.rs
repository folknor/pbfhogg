#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

/// The protocol document's own example.
const EXAMPLE: &str = r#"{
  "protocol": 1,
  "scenarios": [
    {
      "id": "repeat-cat",
      "description": "cat the input twice in a row",
      "deadline_s": 600,
      "roles": [
        {"id": "input", "kind": "pbf", "source": "dataset", "variant": "indexed"},
        {"id": "out-a", "kind": "pbf", "source": "output", "owner": "a"},
        {"id": "out-b", "kind": "pbf", "source": "output", "owner": "b"}
      ],
      "calls": [
        {"id": "a", "label": "cat input -> out-a", "stream": ["counters.v1"]},
        {"id": "b", "label": "cat input -> out-b"}
      ],
      "assertions": [
        {"id": "a.outcome", "call": "a"},
        {"id": "b.outcome", "call": "b"},
        {"id": "outputs-equal", "call": null, "covers": ["repeat.equal-outputs.v1"]}
      ]
    }
  ]
}"#;

fn refused(text: &str) -> String {
    match parse(text.as_bytes()) {
        Ok(doc) => panic!("accepted: {doc:?}"),
        Err(e) => e,
    }
}

/// The example with `from` replaced by `to`, which must occur in it.
fn edited(from: &str, to: &str) -> String {
    assert!(EXAMPLE.contains(from), "{from}");
    EXAMPLE.replacen(from, to, 1)
}

#[test]
fn the_documented_example_parses_into_its_scenario() {
    let doc = parse(EXAMPLE.as_bytes()).unwrap();
    let [scenario] = doc.scenarios.as_slice() else {
        panic!("one scenario");
    };
    assert_eq!(scenario.id, "repeat-cat");
    assert_eq!(scenario.deadline_s, 600);
    assert_eq!(scenario.calls.len(), 2);
    assert_eq!(
        scenario.roles[0].source,
        RoleSource::Dataset {
            variant: "indexed".into()
        }
    );
    assert_eq!(
        scenario.roles[2].source,
        RoleSource::Output {
            owner: "b".into(),
            seeded: false
        }
    );
    assert_eq!(scenario.assertions[2].call, None);
    assert_eq!(scenario.assertions[2].covers, ["repeat.equal-outputs.v1"]);
    assert!(scenario.assertions[0].covers.is_empty());
    assert_eq!(scenario.calls[0].stream, ["counters.v1"]);
    assert!(scenario.calls[1].stream.is_empty());
}

#[test]
fn an_assertion_covers_unique_capability_ids() {
    let from = r#""covers": ["repeat.equal-outputs.v1"]"#;
    for good in ["[]", r#"["a.v1", "b.v2"]"#] {
        let doc = edited(from, &format!(r#""covers": {good}"#));
        assert!(parse(doc.as_bytes()).is_ok(), "{good}");
    }
    for bad in [r#"["equal"]"#, r#"["equal.v0"]"#, r#""equal.v1""#] {
        let e = refused(&edited(from, &format!(r#""covers": {bad}"#)));
        assert!(
            e.starts_with("describe.scenarios[0].assertions[2].covers"),
            "{bad}: {e}"
        );
    }
    let e = refused(&edited(from, r#""covers": ["a.v1", "a.v1"]"#));
    assert!(e.contains("listed twice"), "{e}");
}

#[test]
fn a_call_stream_lists_unique_capability_ids() {
    for good in ["[]", r#"["a.v1", "cancel.pre-decision.v12"]"#] {
        let doc = edited(
            r#""stream": ["counters.v1"]"#,
            &format!(r#""stream": {good}"#),
        );
        assert!(parse(doc.as_bytes()).is_ok(), "{good}");
    }
    for bad in [
        r#"["counters"]"#,
        r#"["counters.v0"]"#,
        r#"["counters.v01"]"#,
        r#"["counters.vx"]"#,
        r#"[".v1"]"#,
        r#"["Counters.v1"]"#,
        r#""counters.v1""#,
    ] {
        let e = refused(&edited(
            r#""stream": ["counters.v1"]"#,
            &format!(r#""stream": {bad}"#),
        ));
        assert!(
            e.starts_with("describe.scenarios[0].calls[0].stream"),
            "{bad}: {e}"
        );
    }
    let e = refused(&edited(
        r#""stream": ["counters.v1"]"#,
        r#""stream": ["counters.v1", "counters.v1"]"#,
    ));
    assert!(e.contains("listed twice"), "{e}");
}

#[test]
fn only_an_output_role_starts_seeded() {
    let seeded = edited(
        r#""source": "output", "owner": "b"}"#,
        r#""source": "output", "owner": "b", "seeded": true}"#,
    );
    let doc = parse(seeded.as_bytes()).unwrap();
    assert!(doc.scenarios[0].roles[2].seeded());
    assert!(!doc.scenarios[0].roles[1].seeded());
    let e = refused(&edited(
        r#""variant": "indexed"}"#,
        r#""variant": "indexed", "seeded": true}"#,
    ));
    assert!(e.contains("only an output role starts seeded"), "{e}");
    let e = refused(&edited(
        r#""source": "output", "owner": "b"}"#,
        r#""source": "output", "owner": "b", "seeded": 1}"#,
    ));
    assert!(e.contains("roles[2].seeded"), "{e}");
}

#[test]
fn another_protocol_is_refused_by_name() {
    let e = refused(&edited("\"protocol\": 1", "\"protocol\": 2"));
    assert!(
        e.contains("protocol 2") && e.contains("protocol 1 only"),
        "{e}"
    );
}

#[test]
fn unknown_missing_and_duplicate_keys_are_refused_with_their_path() {
    let e = refused(&edited(
        "\"deadline_s\": 600,",
        "\"deadline_s\": 600, \"warmup\": true,",
    ));
    assert_eq!(e, "describe.scenarios[0]: unknown key \"warmup\"");
    let e = refused(&edited("\"deadline_s\": 600,", ""));
    assert_eq!(e, "describe.scenarios[0]: missing key \"deadline_s\"");
    let e = refused(&edited(
        "\"deadline_s\": 600,",
        "\"deadline_s\": 600, \"deadline_s\": 5,",
    ));
    assert_eq!(e, "describe.scenarios[0]: duplicate key \"deadline_s\"");
}

#[test]
fn retired_fields_are_refused_even_when_false_or_null() {
    for key in ["cli", "counterpart", "isolation"] {
        for value in ["false", "null", "{}"] {
            let text = edited(
                "\"deadline_s\": 600,",
                &format!("\"deadline_s\": 600, {key:?}: {value},"),
            );
            assert!(refused(&text).contains("unknown key"));
            let text = edited(
                "\"label\": \"cat input -> out-b\"",
                &format!("\"label\": \"cat input -> out-b\", {key:?}: {value}"),
            );
            assert!(refused(&text).contains("unknown key"));
        }
    }
    let text = edited(
        "\"deadline_s\": 600,",
        "\"deadline_s\": 600, \"hotpath\": true,",
    );
    assert!(refused(&text).contains("exactly one call"));
}

#[test]
fn deadlines_are_whole_seconds_in_range() {
    for bad in ["0", "86401", "600.0", "6e2", "\"600\""] {
        let e = refused(&edited(
            "\"deadline_s\": 600",
            &format!("\"deadline_s\": {bad}"),
        ));
        assert!(
            e.starts_with("describe.scenarios[0].deadline_s: "),
            "{bad}: {e}"
        );
    }
    assert!(parse(edited("\"deadline_s\": 600", "\"deadline_s\": 86400").as_bytes()).is_ok());
}

#[test]
fn identifiers_follow_the_grammar_and_setup_is_reserved() {
    for bad in ["Repeat", "-x", "", "a b", &"a".repeat(65)] {
        let e = refused(&edited(
            "\"id\": \"repeat-cat\"",
            &format!("\"id\": {bad:?}"),
        ));
        assert!(e.contains("not an identifier"), "{bad}: {e}");
    }
    let e = refused(&edited(
        "{\"id\": \"a\", \"label\"",
        "{\"id\": \"setup\", \"label\"",
    ));
    assert!(e.contains("never a call id"), "{e}");
}

#[test]
fn ids_are_unique_within_their_list() {
    let e = refused(&edited(
        "{\"id\": \"b\", \"label\"",
        "{\"id\": \"a\", \"label\"",
    ));
    assert!(e.contains("call id \"a\" is declared twice"), "{e}");
    let e = refused(&edited("{\"id\": \"out-b\"", "{\"id\": \"out-a\""));
    assert!(e.contains("role id \"out-a\" is declared twice"), "{e}");
    let e = refused(&edited("{\"id\": \"b.outcome\"", "{\"id\": \"a.outcome\""));
    assert!(
        e.contains("assertion id \"a.outcome\" is declared twice"),
        "{e}"
    );
}

#[test]
fn a_scenario_needs_a_call_and_a_document_needs_a_scenario() {
    let e = refused(r#"{"protocol": 1, "scenarios": []}"#);
    assert!(e.contains("declares no scenario"), "{e}");
    let e = refused(
        r#"{"protocol": 1, "scenarios": [{"id": "s", "description": "d", "deadline_s": 1,
            "roles": [], "calls": [], "assertions": []}]}"#,
    );
    assert!(e.contains("declares no call"), "{e}");
}

#[test]
fn writable_roles_have_exactly_one_owner_call() {
    let e = refused(&edited("\"owner\": \"a\"", "\"owner\": \"setup\""));
    assert!(e.contains("not a call of this scenario"), "{e}");
    let e = refused(&edited(", \"owner\": \"a\"", ""));
    assert!(e.contains("missing key \"owner\""), "{e}");
    let e = refused(&edited("\"owner\": \"a\"", "\"owner\": [\"a\", \"b\"]"));
    assert!(e.contains("expected a string, found an array"), "{e}");
}

#[test]
fn each_source_admits_its_kinds_and_keys_only() {
    let e = refused(&edited(
        "\"kind\": \"pbf\", \"source\": \"dataset\"",
        "\"kind\": \"file\", \"source\": \"dataset\"",
    ));
    assert!(
        e.contains("a role of source dataset cannot be of kind file"),
        "{e}"
    );
    let e = refused(&edited(
        "\"variant\": \"indexed\"",
        "\"variant\": \"indexed\", \"owner\": \"a\"",
    ));
    assert!(e.contains("not allowed on a role of source dataset"), "{e}");
    let e = refused(&edited(
        "\"kind\": \"pbf\", \"source\": \"output\", \"owner\": \"a\"",
        "\"kind\": \"pbf\", \"source\": \"generated\", \"owner\": \"a\"",
    ));
    assert!(e.contains("setup owns it"), "{e}");
    let e = refused(&edited(
        "\"kind\": \"pbf\", \"source\": \"output\", \"owner\": \"a\"",
        "\"kind\": \"file\", \"source\": \"scratch\", \"owner\": \"a\"",
    ));
    assert!(
        e.contains("a role of source scratch cannot be of kind file"),
        "{e}"
    );
    let e = refused(&edited(
        "\"source\": \"output\", \"owner\": \"a\"",
        "\"source\": \"output\", \"owner\": \"a\", \"variant\": \"raw\"",
    ));
    assert!(e.contains("not allowed on a role of source output"), "{e}");
    let e = refused(&edited("\"source\": \"dataset\"", "\"source\": \"remote\""));
    assert!(e.contains("unknown role source"), "{e}");
}

#[test]
fn assertions_name_their_call_or_null_and_avoid_the_brokkr_namespace() {
    let e = refused(&edited(
        "{\"id\": \"a.outcome\", \"call\": \"a\"}",
        "{\"id\": \"a.outcome\"}",
    ));
    assert!(e.contains("missing key \"call\""), "{e}");
    let e = refused(&edited("\"call\": \"a\"", "\"call\": \"c\""));
    assert!(e.contains("not a call of this scenario"), "{e}");
    let e = refused(&edited(
        "{\"id\": \"a.outcome\"",
        "{\"id\": \"brokkr.stream\"",
    ));
    assert!(
        e.contains("reserved for the results the driver adds"),
        "{e}"
    );
}

#[test]
fn text_is_one_short_line() {
    // An escaped newline: valid JSON, but not one line of text.
    let e = refused(&edited("twice in a row", "twice\\nin a row"));
    assert!(e.contains("without control characters"), "{e}");
    let long = "x".repeat(201);
    let e = refused(&edited("cat the input twice in a row", &long));
    assert!(e.contains("1 to 200 bytes"), "{e}");
}

#[test]
fn an_oversized_document_is_refused_before_parsing() {
    let mut big = EXAMPLE.to_owned();
    big.push_str(&" ".repeat(DESCRIBE_LIMIT));
    let e = refused(&big);
    assert!(e.contains("over the"), "{e}");
}

#[test]
fn hotpath_is_a_boolean_and_requires_exactly_one_call() {
    let single = r#"{"protocol":1,"scenarios":[{"id":"s","description":"single call","deadline_s":1,
        "hotpath":true,"roles":[],"calls":[{"id":"a","label":"call"}],"assertions":[]}]}"#;
    assert!(parse(single.as_bytes()).unwrap().scenarios[0].hotpath);
    assert!(
        !parse(single.replace("true", "false").as_bytes())
            .unwrap()
            .scenarios[0]
            .hotpath
    );
    assert!(refused(&single.replace("true", "null")).contains("hotpath"));
    assert!(refused(&single.replace("true", "1")).contains("hotpath"));
}
