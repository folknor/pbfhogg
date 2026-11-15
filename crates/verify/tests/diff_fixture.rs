//! Generated protocols for the production diff and renumber checks.
use crate::{checks, fake_pbf, harness, process, reference, reference_cache, run};
use std::ffi::OsString;
use std::path::Path;

const SORTED: &str = "Sort.Type_then_ID";

/// The OPL the fake osmium decodes a renumber output to, by fault: ten
/// relations, so one relation difference is within the 10% tolerance.
fn renumber_opl(fault: &str, pbfhogg: bool) -> String {
    let mut lines = vec!["n1 v1".to_owned()];
    for r in 1..=10 {
        let changed = pbfhogg
            && match fault {
                "relation-ok" => r == 1,
                "relation" => r <= 2,
                _ => false,
            };
        if pbfhogg && fault == "one-sided" && r == 10 {
            continue;
        }
        lines.push(format!("r{r} v1 T{}", if changed { "a=1" } else { "" }));
    }
    if pbfhogg && fault == "node" {
        lines[0] = "n1 v1 Ta=1".to_owned();
    }
    lines.iter().map(|l| format!("{l}\n")).collect()
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
        reference::Tools {
            osmium: exe.into(),
            osmosis: None,
            osmconvert: None,
        },
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
                "fetch",
                "unsorted",
                "publish",
            ],
        ),
        (
            "osc",
            vec!["", "roundtrip", "raw-osc", "unsorted", "publish"],
        ),
        (
            "renumber",
            vec![
                "",
                "relation-ok",
                "node",
                "relation",
                "one-sided",
                "unsorted",
                "publish",
            ],
        ),
    ] {
        std::fs::write(dir.join("check"), check).unwrap();
        for fault in faults {
            std::fs::write(dir.join("fault"), fault).unwrap();
            // A required-only sort feature must still take the refusal route.
            if ["unsorted", "publish"].contains(&fault) {
                fake_pbf::write_with(&input, &[SORTED], &[], "unsorted");
            } else {
                fake_pbf::write_with(&input, &[SORTED], &[SORTED], "sorted");
            }
            h.run = run::Run::create(&dir, exe, true).unwrap();
            h.run.watch(&input).unwrap();
            h.run.watch(&osc).unwrap();
            let status = h
                .run_check(check, |h| match check {
                    "text" => checks::diff::run(h, &input, &osc, true),
                    "osc" => checks::derive_changes::run(h, &input, &osc, true),
                    _ => checks::renumber::run(h, &input, "fixture", Some("10"), true, true),
                })
                .unwrap();
            assert_eq!(
                status,
                if fault.is_empty() || ["unsorted", "relation-ok"].contains(&fault) {
                    harness::CheckStatus::Passed
                } else {
                    harness::CheckStatus::Failed
                },
                "{check}: {fault}"
            );
        }
    }
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
        if let Some(rest) = marker.strip_prefix("renumber-") {
            let (side, written_under) = rest.split_once('-').unwrap();
            std::fs::write(value("-o"), renumber_opl(written_under, side == "pbfhogg")).unwrap();
        } else {
            fake_pbf::opl_cat(&args);
        }
        return 0;
    }
    match args[0] {
        "fileinfo" => print!("{}", fake_pbf::fileinfo(1, 0, 10)),
        "getid" => {
            if fault == "fetch" {
                return 1;
            }
            let new = args.iter().any(|a| a.ends_with("new.osm.pbf"));
            std::fs::write(
                value("--output"),
                format!(
                    "n1 v1 dV c1 t2020-01-01T00:00:00Z i1 ua Tname={} x1 y2\n",
                    if new { "y" } else { "x" }
                ),
            )
            .unwrap();
        }
        "diff" if !ours => {
            // Simulate a checksum-blind content change. Touched identities must
            // be fetched from both files even though osmium lists no change.
            eprintln!("Summary: left=0 right=0 same=1 different=0");
        }
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
            if args[0] == "derive-changes" {
                std::fs::write(output, "<osmChange/>").unwrap();
            } else {
                let broken = fault == "roundtrip" && output.ends_with("roundtrip-pbfhogg.osm.pbf");
                // The marker carries everything the fake decode reads,
                // since the reference cache keys the decode on content.
                let marker = match (args[0], ours) {
                    ("renumber", true) => format!("renumber-pbfhogg-{fault}"),
                    ("renumber", false) => format!("renumber-osmium-{fault}"),
                    _ if broken => "wrong".to_owned(),
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
    if refuse(args, fault, output) {
        return 2;
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
    let raw = args.iter().any(|a| a.contains("unindexed.osm.pbf"));
    if fault == "summary" {
        eprintln!("Files are identical (1 common elements)");
    } else {
        eprintln!("1 differences: 0 created, 1 modified, 0 deleted (0 common)");
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
