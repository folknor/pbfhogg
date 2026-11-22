//! Generated protocols for the production diff and renumber checks.
use crate::{checks, fake_pbf, harness, process, reference, reference_cache, run};
use std::ffi::OsString;
use std::path::Path;

const SORTED: &str = "Sort.Type_then_ID";

/// The OPL the fake osmium decodes a renumber output to, by fault, under
/// start ID 10: one node and ten relations, the first naming an orphan
/// node, which osmium calls 11 and pbfhogg, by its own mapping, 16.
fn renumber_opl(fault: &str, pbfhogg: bool) -> String {
    let mut lines = vec!["n10 v1".to_owned()];
    for r in 10..20 {
        if pbfhogg && fault == "one-sided" && r == 19 {
            continue;
        }
        let tags = if pbfhogg && fault == "relation" && r == 11 {
            "a=1"
        } else {
            ""
        };
        // A version-only change of one relation: renumber keeps metadata.
        let version = if pbfhogg && fault == "metadata" && r == 12 {
            2
        } else {
            1
        };
        let members = match (r, pbfhogg, fault) {
            (10, false, _) => "n11@",
            (10, true, "orphan-live") => "n10@",
            (10, true, _) => "n16@",
            _ => "",
        };
        lines.push(format!("r{r} v{version} T{tags} M{members}"));
    }
    if pbfhogg && fault == "node" {
        lines[0] = "n10 v1 Ta=1".to_owned();
    }
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// The OPL the fake osmium decodes the text diff's pair to, with every
/// field: OLD (`new` `None`) and NEW as written under fault `new`. n1's
/// name changed, as pbfhogg reports. Under `untouched` n2 also changed, by
/// a shifted tag boundary osmium diff's checksum could not see, which
/// pbfhogg does not report; under `version` n1 also moved to v2, which
/// pbfhogg's header does not report; under `metadata-only` n2 moved to v2
/// alone, and under `coords` w1's inline coordinate alone changed, neither
/// a difference pbfhogg may report.
fn text_opl(new: Option<&str>) -> String {
    let fault = new.unwrap_or("");
    let n1 = if fault == "version" { 2 } else { 1 };
    let n2 = if fault == "metadata-only" { 2 } else { 1 };
    let n2_tags = if fault == "untouched" {
        "Tab=c"
    } else {
        "Ta=bc"
    };
    let x = if fault == "coords" { 3 } else { 1 };
    format!(
        "n1 v{n1} dV c1 t2020-01-01T00:00:00Z i1 ua Tname={} x1 y2\n\
         n2 v{n2} dV c1 t2020-01-01T00:00:00Z i1 ua {n2_tags} x1 y2\n\
         w1 v1 dV c1 t2020-01-01T00:00:00Z i1 ua T Nn1x{x}y2\n",
        if new.is_some() { "y" } else { "x" }
    )
}

/// The OPL the fake osmium decodes the derive check's kind-change pair to:
/// OLD holds n1, n5 and n7, NEW n1 and w7, so osmium's derive, comparing
/// IDs without kinds, deletes n5 but keeps n7. Its roundtrip under fault
/// `kind-change` is NEW plus n7 as it was; `kind-change-extra` also keeps
/// n5, `kind-change-altered` keeps n7 with another coordinate.
fn derive_opl(side: &str) -> String {
    let node = |id: u32, x: u32| format!("n{id} v1 dV c1 t2020-01-01T00:00:00Z i1 ua T x{x} y1\n");
    let way = "w7 v1 dV c1 t2020-01-01T00:00:00Z i1 ua T Nn1\n";
    match side {
        "old" => [node(1, 1), node(5, 1), node(7, 1)].concat(),
        "new" => [node(1, 1), way.into()].concat(),
        "osmium-kind-change" => [node(1, 1), node(7, 1), way.into()].concat(),
        "osmium-kind-change-extra" => [node(1, 1), node(5, 1), node(7, 1), way.into()].concat(),
        "osmium-kind-change-pbfhogg" => [node(1, 1), node(7, 1), way.into()].concat(),
        "osmium-kind-change-altered" => [node(1, 1), node(7, 2), way.into()].concat(),
        _ => panic!("no derive decoding {side}"),
    }
}

pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join("diff-fixture");
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    let osc = dir.join("input.osc");
    std::fs::write(
        &osc,
        "<osmChange version=\"0.6\"><modify><node id=\"1\"/></modify></osmChange>",
    )
    .unwrap();
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, "diff".into(), true).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools { osmium: exe.into() },
        false,
        cache,
    );
    for (check, faults) in [
        (
            "text",
            vec![
                "",
                "detail",
                "summary",
                "raw-listing",
                "plain-listing",
                "exit",
                "untouched",
                "version",
                "metadata-only",
                "coords",
                "route",
                "unsorted",
                "publish",
            ],
        ),
        (
            "osc",
            vec![
                "",
                "roundtrip",
                "roundtrip-metadata",
                "roundtrip-strip",
                "roundtrip-raw-exit",
                "raw-osc",
                "kind-change",
                "kind-change-extra",
                "kind-change-altered",
                "kind-change-pbfhogg",
                "route",
                "unsorted",
                "publish",
            ],
        ),
        (
            "renumber",
            vec![
                "",
                "node",
                "relation",
                "orphan-live",
                "one-sided",
                "metadata",
                "unsorted",
                "publish",
            ],
        ),
    ] {
        std::fs::write(dir.join("check"), check).unwrap();
        for fault in faults {
            std::fs::write(dir.join("fault"), fault).unwrap();
            // An input without the optional sort feature takes the refusal
            // route. (As a required feature it is a header pbfhogg cannot
            // read, which the verifier's header reader refuses.)
            if ["unsorted", "publish"].contains(&fault) {
                fake_pbf::write_with(&input, &[], &[], "unsorted");
            } else if check == "text" {
                // The text pair's decodings key on content alone.
                fake_pbf::write_with(&input, &[], &[SORTED], "text-old");
            } else if fault.starts_with("kind-change") {
                fake_pbf::write_with(&input, &[], &[SORTED], "derive-old");
            } else {
                fake_pbf::write_with(&input, &[], &[SORTED], "sorted");
            }
            h.run = run::Run::create(&dir, exe, true).unwrap();
            h.run.watch(&input).unwrap();
            h.run.watch(&osc).unwrap();
            let judged = super::judge(&mut h, check, |h| match check {
                "text" => checks::diff::run(h, &input, &osc, true),
                "osc" => checks::derive_changes::run(h, &input, &osc, true),
                _ => checks::renumber::run(h, &input, "fixture", Some("10"), true, true),
            });
            assert_judged(check, fault, &judged);
        }
    }
}

/// Hold one fault's outcome to what it must be.
fn assert_judged(check: &str, fault: &str, judged: &super::Judged) {
    let passes = [
        "",
        "unsorted",
        "roundtrip-metadata",
        "kind-change",
        "metadata-only",
        "coords",
    ];
    assert_eq!(
        judged.status,
        if passes.contains(&fault) {
            harness::CheckStatus::Passed
        } else {
            harness::CheckStatus::Failed
        },
        "{check}: {fault}: {judged:?}"
    );
    // One field class altered: the text diff reads versions from the merged
    // listing, renumber's attribution compares metadata.
    match (check, fault) {
        ("text", "version") => {
            assert_eq!(
                judged.failed,
                ["indexed verbose listing vs osmium content"],
                "{judged:?}"
            );
            assert_eq!(judged.error, None);
        }
        // The note swapped between the pairs, outputs unchanged: each run
        // fails on its own note and nothing else does.
        ("text", "route") => {
            assert_eq!(
                judged.failed,
                [
                    "indexed verbose fallback note present where its inputs predict it absent",
                    "indexed plain fallback note present where its inputs predict it absent",
                    "unindexed verbose fallback note absent where its inputs predict it present",
                    "unindexed plain fallback note absent where its inputs predict it present",
                ],
                "{judged:?}"
            );
            assert_eq!(judged.error, None);
        }
        ("renumber", "metadata") => {
            assert!(judged.failed.is_empty(), "{judged:?}");
            assert!(
                judged.error.as_deref().is_some_and(|e| e
                    .contains("relation 12 differs: a field other than its node or member refs")),
                "{judged:?}"
            );
        }
        // derive-changes: a broken roundtrip fails both roundtrip arms and
        // nothing else, a raw OSC only the index comparison, a published
        // refusal each side once.
        // A strip or unindexed derive failing in the same run as a broken
        // roundtrip: the unindexed side runs early in launch groups, but its
        // failure is raised only after the roundtrip findings are recorded.
        ("osc", "roundtrip-strip" | "roundtrip-raw-exit") => {
            assert_unindexed_failure_after_roundtrips(fault, judged);
        }
        ("osc", _) => {
            assert_eq!(judged.error, None, "{judged:?}");
            match fault {
                // pbfhogg's roundtrip equal to osmium's attributed one is
                // still held to new.
                "roundtrip" | "kind-change-pbfhogg" => assert_eq!(
                    judged.failed,
                    [
                        "diff pbfhogg roundtrip vs new",
                        "diff pbfhogg roundtrip vs osmium roundtrip"
                    ],
                    "{judged:?}"
                ),
                // osmium keeping more than its same-ID omission explains,
                // or keeping it changed, fails both arms judging osmium's
                // roundtrip.
                "kind-change-extra" | "kind-change-altered" => assert_eq!(
                    judged.failed,
                    [
                        "diff osmium roundtrip vs new",
                        "diff pbfhogg roundtrip vs osmium roundtrip"
                    ],
                    "{judged:?}"
                ),
                "route" => assert_eq!(
                    judged.failed,
                    [
                        "indexed derive fallback note present where its inputs predict it absent",
                        "unindexed derive fallback note absent where its inputs predict it present",
                    ],
                    "{judged:?}"
                ),
                "raw-osc" => assert_eq!(
                    judged.failed,
                    ["unindexed OSC vs indexed OSC"],
                    "{judged:?}"
                ),
                "publish" => assert!(
                    matches!(judged.failed.as_slice(), [old, new]
                        if old.starts_with("pbfhogg diff --format osc (input as OLD) published ")
                            && new.starts_with("pbfhogg diff --format osc (input as NEW) published ")
                            && old.ends_with(" on input it refuses")
                            && new.ends_with(" on input it refuses")),
                    "{judged:?}"
                ),
                _ => assert!(judged.failed.is_empty(), "{judged:?}"),
            }
        }
        _ => {}
    }
}

/// The roundtrip findings recorded, then the held unindexed-side error.
fn assert_unindexed_failure_after_roundtrips(fault: &str, judged: &super::Judged) {
    assert_eq!(
        judged.failed,
        [
            "diff pbfhogg roundtrip vs new",
            "diff pbfhogg roundtrip vs osmium roundtrip"
        ],
        "{judged:?}"
    );
    let program = if fault == "roundtrip-strip" {
        "pbfhogg degrade --strip-indexdata failed"
    } else {
        "pbfhogg diff --format osc (unindexed inputs) failed"
    };
    assert!(
        judged.error.as_deref().is_some_and(|e| e.contains(program)),
        "{judged:?}"
    );
}

/// The faults under which pbfhogg's own roundtrip is wrong.
const BROKEN_ROUNDTRIP: [&str; 3] = ["roundtrip", "roundtrip-strip", "roundtrip-raw-exit"];

/// The strip of new refused under the `roundtrip-strip` fault.
fn strip_refused(subcommand: &str, fault: &str, output: &str) -> bool {
    let refused = subcommand == "degrade"
        && fault == "roundtrip-strip"
        && output.ends_with("new-unindexed.osm.pbf");
    if refused {
        eprintln!("strip refused");
    }
    refused
}

pub fn tool(args: &[OsString]) -> i32 {
    let args: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
    let root = args
        .iter()
        .map(Path::new)
        .flat_map(Path::ancestors)
        .find(|p| p.file_name().is_some_and(|n| n == "diff-fixture"))
        .unwrap();
    let fault = std::fs::read_to_string(root.join("fault")).unwrap();
    let check = std::fs::read_to_string(root.join("check")).unwrap();
    let value = |flag| args[args.iter().position(|s| *s == flag).unwrap() + 1];
    let ours = args.contains(&"--staging-dir")
        || (args[0] == "diff" && !args.contains(&"--output-format"));
    if fake_pbf::is_opl_cat(&args) {
        // Keyed on content, as the reference cache keys the decode.
        let marker = fake_pbf::marker(Path::new(args[1]));
        // Every decoding is projected by the requested options, so a check
        // passing the comparison the wrong field policy decodes differently.
        let opl = if marker == "text-old" {
            Some(text_opl(None))
        } else if let Some(written_under) = marker.strip_prefix("text-new-") {
            Some(text_opl(Some(written_under)))
        } else if let Some(side) = marker.strip_prefix("derive-") {
            Some(derive_opl(side))
        } else if let Some(rest) = marker.strip_prefix("renumber-") {
            let (side, written_under) = rest.split_once('-').unwrap();
            Some(renumber_opl(written_under, side == "pbfhogg"))
        } else {
            None
        };
        if let Some(opl) = opl {
            std::fs::write(value("-o"), fake_pbf::project(&args, &opl)).unwrap();
        } else {
            fake_pbf::opl_cat(&args);
        }
        return 0;
    }
    match args[0] {
        "fileinfo" => print!("{}", fake_pbf::fileinfo(1, 0, 10)),
        "diff" => return diff_tool(&args, &fault, &check),
        "renumber" | "apply-changes" | "sort" | "degrade" | "derive-changes" => {
            let output = value(if args.contains(&"--output") {
                "--output"
            } else {
                "-o"
            });
            if args[0] == "renumber" && ours {
                assert!(args.contains(&"--direct-io"));
                assert!(args.contains(&"--start-id"));
                assert!(!args.contains(&"--tmp-dir"));
                if refuse(&args, &fault, Some(output)) {
                    return 2;
                }
            }
            if ["apply-changes", "degrade", "sort"].contains(&args[0]) && ours {
                assert!(
                    !args.contains(&"--direct-io"),
                    "comparators and builders run buffered"
                );
                assert!(!args.contains(&"--tmp-dir"));
            }
            if strip_refused(args[0], &fault, output) {
                return 2;
            }
            if args[0] == "derive-changes" {
                std::fs::write(output, "<osmChange/>").unwrap();
            } else {
                let roundtrip = output.ends_with("roundtrip-pbfhogg.osm.pbf");
                let broken = roundtrip && BROKEN_ROUNDTRIP.contains(&fault.as_str());
                // An OSC carries no metadata the roundtrip could keep, so
                // a version-only change of the roundtrip must pass.
                let versioned = fault == "roundtrip-metadata" && roundtrip;
                // The marker carries everything the fake decode reads,
                // since the reference cache keys the decode on content.
                let marker = match (args[0], ours) {
                    ("renumber", true) => format!("renumber-pbfhogg-{fault}"),
                    ("renumber", false) => format!("renumber-osmium-{fault}"),
                    ("apply-changes", _) if check == "text" => format!("text-new-{fault}"),
                    ("apply-changes", false)
                        if fault.starts_with("kind-change")
                            && output.ends_with("roundtrip-osmium.osm.pbf") =>
                    {
                        format!("derive-osmium-{fault}")
                    }
                    // pbfhogg's own roundtrip keeping n7 as osmium's does:
                    // identical to osmium's, still not new.
                    ("apply-changes", _)
                        if fault == "kind-change-pbfhogg"
                            && output.ends_with("roundtrip-pbfhogg.osm.pbf") =>
                    {
                        "derive-osmium-kind-change".to_owned()
                    }
                    ("apply-changes", _) if fault.starts_with("kind-change") => {
                        "derive-new".to_owned()
                    }
                    _ if broken => "wrong".to_owned(),
                    _ if versioned => "sorted @v2".to_owned(),
                    _ => "sorted".to_owned(),
                };
                fake_pbf::write(Path::new(output), &[SORTED], &marker);
            }
        }
        _ => panic!("unexpected protocol {args:?}"),
    }
    0
}

fn refuse(args: &[&str], fault: &str, output: Option<&str>) -> bool {
    let operand = args.iter().find(|a| a.ends_with("input.pbf"));
    if ["unsorted", "publish"].contains(&fault)
        && let Some(input) = operand
    {
        if fault == "publish" {
            if let Some(output) = output {
                std::fs::write(output, "bad").unwrap();
            } else {
                println!("unexpected listing");
            }
        }
        eprintln!(
            "Error: unsorted\nSort the input first, decoding every blob rather than believing the header's sort claim and the blobs' indexdata:\n\n  pbfhogg sort --distrust-metadata {input} -o sorted.osm.pbf"
        );
        return true;
    }
    false
}

fn diff_tool(args: &[&str], fault: &str, check: &str) -> i32 {
    let osc = args.contains(&"--format");
    let output = args
        .iter()
        .position(|a| *a == "--output")
        .map(|i| args[i + 1]);
    if osc {
        assert!(args.contains(&"--tmp-dir"));
        assert!(args.contains(&"--staging-dir"));
        assert_eq!(args.contains(&"--direct-io"), check == "osc");
    } else {
        assert!(!args.contains(&"--staging-dir"));
        assert!(!args.contains(&"--tmp-dir"));
    }
    // Every judged run fixes the worker count, so the fallback note means
    // the same on every host.
    let jobs = args
        .iter()
        .position(|a| *a == "--jobs")
        .map(|i| args[i + 1]);
    assert_eq!(jobs, Some("2"), "{args:?}");
    if refuse(args, fault, output) {
        return 2;
    }
    // pbfhogg's note on an unindexed pair at more than one worker; the route
    // fault prints it on the indexed pair instead, outputs unchanged.
    let raw = args.iter().any(|a| a.contains("unindexed.osm.pbf"));
    // A failed strip must keep the unindexed derive from launching at all.
    assert!(!(raw && fault == "roundtrip-strip"), "{args:?}");
    if osc && raw && fault == "roundtrip-raw-exit" {
        eprintln!("raw derive failed");
        return 2;
    }
    if raw != (fault == "route") {
        eprintln!("Note: Using sequential comparison: missing indexdata");
    }
    if osc {
        std::fs::write(
            output.unwrap(),
            if fault == "raw-osc" && output.unwrap().contains("unindexed") {
                "<osmChange version=\"0.6\"/>"
            } else {
                "<osmChange/>"
            },
        )
        .unwrap();
        return 1;
    }
    assert!(args.contains(&"--direct-io"));
    let verbose = args.contains(&"--verbose");
    if fault == "summary" {
        eprintln!("Files are identical (1 common elements)");
    } else {
        // n2 and w1 are common, as osmium's content gives them.
        eprintln!("1 differences: 0 created, 1 modified, 0 deleted (2 common)");
    }
    println!("*n1 v1");
    if verbose {
        println!(
            "  ~\"name\": \"x\" -> \"{}\"",
            if fault == "detail" { "z" } else { "y" }
        );
        if fault == "raw-listing" && raw {
            println!("extra");
        }
    } else if fault == "plain-listing" {
        println!("extra");
    }
    i32::from(fault != "exit")
}
