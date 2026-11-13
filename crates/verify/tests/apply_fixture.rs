//! Generated apply-changes protocol, with no datasets or network.
use crate::{checks, harness, process, reference, reference_cache, run};
use std::ffi::OsString;
use std::path::Path;

pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join("apply-fixture");
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    let osc = dir.join("input.osc");
    std::fs::write(
        &osc,
        r#"<osmChange version="0.6"><delete><node id="7"/></delete></osmChange>"#,
    )
    .unwrap();
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, "apply".into(), false).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools {
            osmium: exe.into(),
            osmosis: None,
            osmconvert: Some(exe.into()),
        },
        false,
        cache,
    );
    for (fault, expected) in [
        ("", true),
        ("explained", true),
        ("unexplained", false),
        ("modified", false),
        ("deleted", false),
        ("stale", false),
        ("context-failure", true),
        ("osmium-stale", true),
        ("unsorted", true),
        ("publish", false),
    ] {
        std::fs::write(dir.join("fault"), fault).unwrap();
        std::fs::write(
            &input,
            if fault == "unsorted" || fault == "publish" {
                "unsorted"
            } else if fault == "context-failure" {
                "context-sorted"
            } else {
                "sorted"
            },
        )
        .unwrap();
        for locations in [false, true] {
            h.run = run::Run::create(&dir, exe, true).unwrap();
            h.run.watch(&input).unwrap();
            h.run.watch(&osc).unwrap();
            let status = h
                .run_check("apply", |h| {
                    checks::apply_changes::run(h, &input, &osc, true, locations)
                })
                .unwrap();
            let pass = expected || (fault == "stale" && !locations);
            assert_eq!(
                status,
                if pass {
                    harness::CheckStatus::Passed
                } else {
                    harness::CheckStatus::Failed
                },
                "fault={fault}, locations={locations}"
            );
        }
    }
}

pub fn tool(args: &[OsString]) -> i32 {
    let args: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
    let root = args
        .iter()
        .map(|s| s.strip_prefix("-o=").unwrap_or(s))
        .map(Path::new)
        .flat_map(Path::ancestors)
        .find(|p| p.file_name().is_some_and(|n| n == "apply-fixture"))
        .unwrap();
    let fault = std::fs::read_to_string(root.join("fault")).unwrap();
    let value = |flag| args[args.iter().position(|s| *s == flag).unwrap() + 1];
    match args[0] {
        "inspect" => {
            let sorted =
                std::fs::read_to_string(args.iter().find(|a| a.ends_with(".pbf")).unwrap())
                    .unwrap()
                    != "unsorted";
            let feature = if sorted {
                r#""Sort.Type_then_ID","#
            } else {
                ""
            };
            print!(
                r#"{{"header":{{"required_features":[],"optional_features":[{feature}"LocationsOnWays"]}},"elements":{{"nodes":1,"ways":1,"relations":0}},"ordering":{{"standard":true}},"data":{{"objects_ordered":true}}}}"#
            );
        }
        "apply-changes" => {
            let ours = args.contains(&"--output");
            let output = value(if ours { "--output" } else { "-o" });
            if ours {
                assert!(args.contains(&"--direct-io"));
                assert!(args.contains(&"--staging-dir"));
                assert!(!args.contains(&"--tmp-dir"));
            }
            if std::fs::read_to_string(args[1]).unwrap() == "unsorted" {
                assert!(ours, "references must not run on refusal input");
                if fault == "publish" {
                    std::fs::write(output, "bad").unwrap();
                }
                eprintln!(
                    "Error: apply-changes requires a sorted base PBF (Sort.Type_then_ID). (in {})\n\nSort the input first, decoding every blob rather than believing the header's sort claim and the blobs' indexdata:\n\n  pbfhogg sort --distrust-metadata {} -o sorted.osm.pbf",
                    args[1], args[1]
                );
                return 2;
            }
            std::fs::write(output, "sorted").unwrap();
        }
        "sort" => {
            std::fs::write(value("--output"), "sorted").unwrap();
        }
        "check" => {
            assert!(args.contains(&"--locations"));
            assert!(!args.contains(&"--agreement"));
            if fault == "stale"
                || (fault == "osmium-stale" && args.iter().any(|a| a.ends_with("osmium.osm.pbf")))
            {
                println!("Refs with a node record: 1 verified, 1 mismatched (1 nodes)");
                return 1;
            }
            println!("Refs with a node record: 2 verified, 0 mismatched (0 nodes)");
        }
        "diff" => {
            assert!(args.contains(&"--tmp-dir"));
            assert!(args.contains(&"--staging-dir"));
            let (action, id) = match fault.as_str() {
                "explained" => ("create", 7),
                "unexplained" => ("create", 8),
                "modified" => ("modify", 7),
                "deleted" => ("delete", 7),
                _ => ("", 0),
            };
            let body = if action.is_empty() {
                String::new()
            } else {
                format!("<{action}><node id=\"{id}\"/></{action}>")
            };
            std::fs::write(
                value("--output"),
                format!("<osmChange version=\"0.6\">{body}</osmChange>"),
            )
            .unwrap();
            return i32::from(!action.is_empty());
        }
        _ => {
            // Optional osmconvert context invocation has no subcommand.
            if fault == "context-failure" {
                return 1;
            }
            let output = args.iter().find_map(|a| a.strip_prefix("-o=")).unwrap();
            std::fs::write(output, "sorted").unwrap();
        }
    }
    0
}
