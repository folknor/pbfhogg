//! Generated protocols for the production diff and renumber checks.
use crate::{checks, harness, process, reference, reference_cache, run};
use std::ffi::OsString;
use std::path::Path;

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
                "partial",
                "unsorted",
                "publish",
            ],
        ),
    ] {
        std::fs::write(dir.join("check"), check).unwrap();
        for fault in faults {
            std::fs::write(dir.join("fault"), fault).unwrap();
            std::fs::write(
                &input,
                if ["unsorted", "publish"].contains(&fault) {
                    "unsorted"
                } else {
                    "sorted"
                },
            )
            .unwrap();
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
        || args[0] == "inspect"
        || (args[0] == "diff" && !args.contains(&"--output-format"));
    match args[0] {
        "inspect" => {
            let input = args.iter().find(|a| a.ends_with(".pbf")).unwrap();
            let sorted = std::fs::read_to_string(input).unwrap() != "unsorted";
            // A required-only sort feature must still take the refusal route.
            let feature = if sorted { "\"Sort.Type_then_ID\"" } else { "" };
            print!(
                r#"{{"header":{{"required_features":["Sort.Type_then_ID"],"optional_features":[{feature}]}},"elements":{{"nodes":1,"ways":0,"relations":10}},"ordering":{{"standard":true}},"data":{{"objects_ordered":true}}}}"#
            );
        }
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
            std::fs::write(
                output,
                if args[0] == "derive-changes" {
                    "<osmChange/>"
                } else {
                    "sorted"
                },
            )
            .unwrap();
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
    if args.contains(&"-s") {
        assert!(!args.contains(&"--direct-io"));
        let (same, different) = match fault {
            "node" | "relation-ok" => (10, 1),
            "relation" => (9, 2),
            "one-sided" => (9, 1),
            _ => (11, 0),
        };
        if fault == "partial" {
            eprintln!("Summary: left=11 right=11 same=11");
        } else {
            eprintln!("Summary: left=11 right=11 same={same} different={different}");
        }
        if fault == "node" {
            println!("*n1");
        }
        if fault == "relation" {
            println!("*r1\n*r2");
        }
        if ["relation-ok", "one-sided"].contains(&fault) {
            println!("*r1");
        }
        return i32::from(different > 0);
    }
    if check == "osc" {
        if fault == "roundtrip" {
            println!("*n1");
            return 1;
        }
        return 0;
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
