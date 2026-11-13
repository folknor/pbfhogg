#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use super::*;
use crate::artefact::{CallOutcome, StreamAccount};
use crate::describe;
use crate::sidecar::{Counter, Sealing, Terminal};

/// A sealed call: entry 10, return 50, seal 100, snapshot `{n: 3}`, the sink
/// sealed with no drops.
fn doc() -> CallDoc {
    CallDoc {
        entry_us: 10,
        return_us: 50,
        seal_us: 100,
        outcome: CallOutcome::Ok,
        publications: Vec::new(),
        seal: Seal::Sealed,
        snapshot: Some(BTreeMap::from([("n".to_owned(), 3)])),
        stream: StreamAccount {
            state: SinkState::Sealed,
            drops: 0,
        },
        diagnostics: Vec::new(),
        results: Vec::new(),
        context: crate::json::Json::Object(vec![("p".to_owned(), crate::json::Json::Int(1))]),
    }
}

/// Records of call `a` from wire-like shorthand: `M ts NAME`, `C ts NAME V`,
/// `S ts NAME V`, `T ts sealed|failed DROPS`; `X` tags call `b` instead.
fn records(lines: &[&str]) -> Vec<Record> {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let parts: Vec<&str> = line.split(' ').collect();
            let (kind, call) = match parts[0].strip_prefix('X') {
                Some(kind) => (kind, "b"),
                None => (parts[0], "a"),
            };
            let ts: i64 = parts[1].parse().unwrap();
            match kind {
                "M" => Record::Marker(Marker {
                    marker_idx: i32::try_from(i).unwrap(),
                    timestamp_us: ts,
                    name: parts[2].to_owned(),
                    call: Some(call.to_owned()),
                }),
                "C" => Record::Counter(Counter {
                    timestamp_us: ts,
                    name: parts[2].to_owned(),
                    value: parts[3].parse().unwrap(),
                    call: Some(call.to_owned()),
                }),
                "S" => Record::Sealing(Sealing {
                    timestamp_us: ts,
                    call: call.to_owned(),
                    name: parts[2].to_owned(),
                    value: parts[3].parse().unwrap(),
                }),
                "T" => Record::Terminal(Terminal {
                    timestamp_us: ts,
                    call: call.to_owned(),
                    end: if parts[2] == "sealed" {
                        SinkEnd::Sealed
                    } else {
                        SinkEnd::Failed
                    },
                    drops: parts[3].parse().unwrap(),
                }),
                other => panic!("{other}"),
            }
        })
        .collect()
}

fn lost() -> CaptureLoss {
    CaptureLoss {
        rejected_lines: 1,
        ..CaptureLoss::default()
    }
}

fn judged(doc: &CallDoc, lines: &[&str], loss: &CaptureLoss) -> CallStream {
    judge("a", doc, &records(lines), loss)
}

const GOOD: &[&str] = &[
    "M 10 READ",
    "C 20 n 2",
    "M 30 WRITE",
    "XC 31 n 99",
    "M 60 FLUSH",
    "S 70 n 3",
    "T 80 sealed 0",
];

#[test]
fn a_complete_stream_passes_with_its_phases_and_tail() {
    let stream = judged(&doc(), GOOD, &CaptureLoss::default());
    assert_eq!(stream.status, Status::Pass, "{}", stream.detail);
    assert_eq!(
        stream.phases,
        [("READ".to_owned(), 20), ("WRITE".to_owned(), 20)],
        "the last phase ends at return_us"
    );
    assert_eq!(stream.tail, ["FLUSH"]);
    // A marker stamped before entry fails the stream and is no phase.
    let mut early = GOOD.to_vec();
    early.insert(0, "M 5 EARLY");
    let stream = judged(&doc(), &early, &CaptureLoss::default());
    assert_eq!(stream.status, Status::Fail);
    assert_eq!(stream.phases[0].0, "READ");
}

/// brokkr#62: only a sealing emission may not repeat a name; a marker
/// emitted again and a counter sampled many times are ordinary records.
#[test]
fn repeated_ordinary_names_are_legal() {
    let lines = [
        "M 10 WAIT",
        "C 15 n 1",
        "M 20 WAIT",
        "C 25 n 2",
        "M 30 WAIT",
        "C 35 n 2",
        "S 70 n 3",
        "T 80 sealed 0",
    ];
    let stream = judged(&doc(), &lines, &CaptureLoss::default());
    assert_eq!(stream.status, Status::Pass, "{}", stream.detail);
}

#[test]
fn what_the_records_prove_fails_even_when_records_were_lost() {
    for (from, to, why) in [
        ("S 70 n 3", "S 70 n 4", "but the snapshot holds 3"),
        ("S 70 n 3", "S 70 m 3", "which the snapshot lacks"),
        ("C 20 n 2", "C 20 n 4", "above the snapshot's final 3"),
        ("M 60 FLUSH", "M 5 FLUSH", "outside entry_us..seal_us"),
        ("S 70 n 3", "S 40 n 3", "stamped before return_us"),
        (
            "M 60 FLUSH",
            "M 75 FLUSH",
            "after a sealing record stamped 70",
        ),
        (
            "T 80 sealed 0",
            "T 65 sealed 0",
            "before the sealing record ahead of it",
        ),
        (
            "T 80 sealed 0",
            "T 80 failed 0",
            "the terminal record says failed",
        ),
    ] {
        let lines: Vec<&str> = GOOD
            .iter()
            .map(|l| if *l == from { to } else { l })
            .collect();
        for loss in [CaptureLoss::default(), lost()] {
            let stream = judged(&doc(), &lines, &loss);
            assert_eq!(stream.status, Status::Fail, "{to}: {}", stream.detail);
            assert!(stream.detail.contains(why), "{to}: {}", stream.detail);
        }
    }
}

#[test]
fn order_after_sealing_and_after_the_terminal_is_a_violation() {
    let mut lines = GOOD.to_vec();
    lines.insert(6, "C 75 n 1");
    let stream = judged(&doc(), &lines, &CaptureLoss::default());
    assert!(
        stream.detail.contains("arrived after sealing began"),
        "{}",
        stream.detail
    );
    let mut lines = GOOD.to_vec();
    lines.push("T 90 sealed 0");
    let stream = judged(&doc(), &lines, &CaptureLoss::default());
    assert!(
        stream.detail.contains("a second terminal"),
        "{}",
        stream.detail
    );
    let mut lines = GOOD.to_vec();
    lines.insert(6, "S 72 n 3");
    let stream = judged(&doc(), &lines, &CaptureLoss::default());
    assert!(stream.detail.contains("a second time"), "{}", stream.detail);
}

#[test]
fn a_missing_sealing_emission_fails_only_a_complete_stream() {
    let mut doc = doc();
    doc.snapshot = Some(BTreeMap::from([
        ("n".to_owned(), 3),
        ("zero".to_owned(), 0),
    ]));
    let stream = judged(&doc, GOOD, &CaptureLoss::default());
    assert_eq!(stream.status, Status::Fail);
    assert!(
        stream.detail.contains("never emitted \"zero\""),
        "{}",
        stream.detail
    );
    let stream = judged(&doc, GOOD, &lost());
    assert_eq!(stream.status, Status::Unevaluated, "{}", stream.detail);
    assert!(
        stream.detail.contains("rejected 1 line"),
        "{}",
        stream.detail
    );
}

#[test]
fn a_missing_terminal_must_be_explained() {
    let lines = &GOOD[..6];
    let stream = judged(&doc(), lines, &CaptureLoss::default());
    assert_eq!(stream.status, Status::Fail);
    assert!(
        stream.detail.contains("no terminal record arrived, yet"),
        "{}",
        stream.detail
    );
    let mut dropped = doc();
    dropped.stream.drops = 1;
    let stream = judged(&dropped, lines, &CaptureLoss::default());
    assert_eq!(stream.status, Status::Unevaluated, "{}", stream.detail);
    assert_eq!(judged(&doc(), lines, &lost()).status, Status::Unevaluated);
}

#[test]
fn a_sink_that_dropped_or_failed_is_incomplete() {
    let mut lines = GOOD.to_vec();
    lines[6] = "T 80 sealed 2";
    let mut dropped = doc();
    dropped.stream.drops = 2;
    let stream = judged(&dropped, &lines, &CaptureLoss::default());
    assert_eq!(stream.status, Status::Unevaluated);
    assert!(
        stream.detail.contains("dropped 2 lines"),
        "{}",
        stream.detail
    );
    lines[6] = "T 80 failed 0";
    let mut failed = doc();
    failed.stream.state = SinkState::Failed;
    let stream = judged(&failed, &lines, &CaptureLoss::default());
    assert_eq!(stream.status, Status::Unevaluated);
    assert!(
        stream.detail.contains("the sink failed"),
        "{}",
        stream.detail
    );
}

#[test]
fn a_call_that_did_not_seal_is_unevaluated_unless_its_sink_closed() {
    let mut open = doc();
    open.seal = Seal::Outstanding(BTreeMap::from([("writer".to_owned(), 1)]));
    open.snapshot = None;
    open.stream.state = SinkState::Open;
    let stream = judged(&open, &GOOD[..4], &CaptureLoss::default());
    assert_eq!(stream.status, Status::Unevaluated);
    assert_eq!(stream.detail, "the call did not seal");
    let stream = judged(&open, GOOD, &CaptureLoss::default());
    assert_eq!(stream.status, Status::Fail);
    assert!(
        stream.detail.contains("did not seal, yet"),
        "{}",
        stream.detail
    );
}

#[test]
fn a_saturated_snapshot_value_matches_the_wire() {
    let mut big = doc();
    big.snapshot = Some(BTreeMap::from([("n".to_owned(), u64::MAX)]));
    let lines = ["S 70 n 9223372036854775807", "T 80 sealed 0"];
    assert_eq!(
        judged(&big, &lines, &CaptureLoss::default()).status,
        Status::Pass
    );
}

#[test]
fn an_undeclared_call_or_an_overflowed_census_fails_the_run() {
    let scenario = describe::parse(
        br#"{"protocol": 1, "scenarios": [{"id": "s", "description": "d", "deadline_s": 1,
            "roles": [], "calls": [{"id": "a", "label": "a"}], "assertions": []}]}"#,
    )
    .unwrap()
    .scenarios
    .remove(0);
    let census = |ids: &[&str], overflowed| CallCensus {
        ids: ids.iter().map(|s| (*s).to_owned()).collect(),
        overflowed,
    };
    assert_eq!(undeclared_failure(&scenario, &census(&["a"], false)), None);
    let why = undeclared_failure(&scenario, &census(&["a", "Zed"], false)).unwrap();
    assert!(
        why.contains("\"Zed\", which describe does not declare"),
        "{why}"
    );
    assert!(undeclared_failure(&scenario, &census(&["a"], true)).is_some());
}
