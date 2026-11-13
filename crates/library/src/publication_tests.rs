#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::{artefact, describe};

const DESCRIBE: &str = r#"{"protocol": 1, "scenarios": [
  {"id": "set", "description": "one output set", "deadline_s": 60,
   "roles": [
     {"id": "fresh", "kind": "pbf", "source": "output", "owner": "a"},
     {"id": "old", "kind": "file", "source": "output", "owner": "a", "seeded": true},
     {"id": "tree", "kind": "directory", "source": "output", "owner": "a", "seeded": true}
   ],
   "calls": [{"id": "a", "label": "publish a set"}],
   "assertions": []}
]}"#;

fn scenario() -> Scenario {
    describe::parse(DESCRIBE.as_bytes())
        .unwrap()
        .scenarios
        .remove(0)
}

/// A call document for `a` with `publications`.
fn doc(publications: &str) -> CallDoc {
    let text = format!(
        r#"{{"protocol": 1, "call": "a", "entry_us": 1, "return_us": 2, "seal_us": 3,
 "outcome": {{"status": "ok"}}, "publications": {publications}, "cancel": null,
 "seal": {{"state": "sealed"}}, "snapshot": {{}}, "stream": {{"state": "sealed", "drops": 0}},
 "context": {{"p": 1}}, "diagnostics": [], "assertions": []}}"#
    );
    artefact::parse_call(text.as_bytes(), &scenario(), "a", &[]).unwrap()
}

/// A roles directory with each role in its initial state.
fn roles() -> pbfhogg_test_scratch::TestScratch {
    let scratch = pbfhogg_test_scratch::scratch();
    let dir = scratch.path();
    for role in &scenario().roles {
        if role.seeded() {
            seed::write(&dir.join(&role.id), role.kind, &role.id).unwrap();
        }
    }
    scratch
}

#[test]
fn untouched_roles_agree_with_no_publication() {
    let scratch = roles();
    let dir = scratch.path();
    assert_eq!(
        failures(&scenario(), "a", &doc("[]"), dir),
        Vec::<String>::new()
    );
}

#[test]
fn an_output_set_replays_member_by_member() {
    let scratch = roles();
    let dir = scratch.path();
    // fresh installed; old moved aside and put back; tree replaced.
    std::fs::write(dir.join("fresh"), b"pbf").unwrap();
    std::fs::remove_dir_all(dir.join("tree")).unwrap();
    std::fs::create_dir(dir.join("tree")).unwrap();
    std::fs::write(dir.join("tree/index"), b"x").unwrap();
    let published = doc(r#"[{"ordinal": 1, "decision": "authorized", "members": [
             {"role": "fresh", "effect": "installed"},
             {"role": "old", "effect": "restored"},
             {"role": "tree", "effect": "installed"}]}]"#);
    assert_eq!(
        failures(&scenario(), "a", &published, dir),
        Vec::<String>::new()
    );

    // The same disk read against a set that reports old displaced.
    let displaced = doc(r#"[{"ordinal": 1, "decision": "authorized", "members": [
             {"role": "fresh", "effect": "installed"},
             {"role": "old", "effect": "displaced"},
             {"role": "tree", "effect": "installed"}]}]"#);
    let found = failures(&scenario(), "a", &displaced, dir);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("output old holds absent, but it is its seed"),
        "{found:?}"
    );
}

#[test]
fn later_publications_continue_from_the_state_earlier_ones_left() {
    let scratch = roles();
    let dir = scratch.path();
    // Installed, then a second attempt that displaced it: absent.
    std::fs::remove_file(dir.join("old")).unwrap();
    let sequence = doc(
        r#"[{"ordinal": 1, "decision": "authorized", "members": [{"role": "old", "effect": "installed"}]},
            {"ordinal": 2, "decision": "authorized", "members": [{"role": "old", "effect": "displaced"}]},
            {"ordinal": 3, "decision": "refused_error", "members": [{"role": "old", "effect": "none"}]}]"#,
    );
    assert_eq!(
        failures(&scenario(), "a", &sequence, dir),
        Vec::<String>::new()
    );

    // A restore of a destination already displaced has nothing to restore.
    let restored = doc(
        r#"[{"ordinal": 1, "decision": "authorized", "members": [{"role": "old", "effect": "displaced"}]},
            {"ordinal": 2, "decision": "authorized", "members": [{"role": "old", "effect": "restored"}]}]"#,
    );
    let found = failures(&scenario(), "a", &restored, dir);
    assert!(
        found[0].contains("publication 2 reports restored old, which had no destination"),
        "{found:?}"
    );
}

#[test]
fn a_destination_of_the_wrong_kind_is_reported() {
    let scratch = roles();
    let dir = scratch.path();
    std::fs::create_dir(dir.join("fresh")).unwrap();
    let found = failures(&scenario(), "a", &doc("[]"), dir);
    assert!(
        found[0].contains("output fresh:") && found[0].contains("is not a pbf"),
        "{found:?}"
    );
}

#[test]
fn an_install_cannot_leave_the_seed_or_no_destination() {
    let scratch = roles();
    let installed = doc(
        r#"[{"ordinal":1,"decision":"authorized","members":[{"role":"old","effect":"installed"}]}]"#,
    );
    let found = failures(&scenario(), "a", &installed, scratch.path());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("a seed is never published"), "{found:?}");
    std::fs::remove_file(scratch.path().join("old")).unwrap();
    let found = failures(&scenario(), "a", &installed, scratch.path());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("but it is absent"), "{found:?}");
}

#[test]
fn an_unreported_install_is_a_failure() {
    let scratch = roles();
    std::fs::write(scratch.path().join("fresh"), b"unreported").unwrap();
    let found = failures(&scenario(), "a", &doc("[]"), scratch.path());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("holds absent, but it is present"),
        "{found:?}"
    );
}
