//! Generated extract protocols: no datasets, network or installed tools.
use crate::{
    checks, extract_config::PolygonInput, harness, process, reference, reference_cache, run,
};
use std::ffi::OsString;
use std::path::Path;

pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join("extract-fixture");
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    let polygon = dir.join("city.geojson");
    std::fs::write(
        &polygon,
        r#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}"#,
    )
    .unwrap();
    let polygons = [PolygonInput {
        name: "city".into(),
        path: polygon.clone(),
    }];
    let bbox = pbfhogg_devkit::Bbox::parse("0,0,1,1").unwrap();
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache =
        reference_cache::ReferenceCache::new(&dir, "extract-fixture".into(), false).unwrap();
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
    for (sorted, fault, expected) in [
        (true, "", harness::CheckStatus::Passed),
        (false, "", harness::CheckStatus::Passed),
        (true, "content", harness::CheckStatus::Failed),
    ] {
        std::fs::write(&input, if sorted { "sorted" } else { "unsorted" }).unwrap();
        std::fs::write(dir.join("fault"), fault).unwrap();
        for multi in [false, true] {
            h.run = run::Run::create(&dir, exe, true).unwrap();
            h.run.watch(&input).unwrap();
            h.run.watch(&polygon).unwrap();
            let status = h
                .run_check("fixture", |h| {
                    if multi {
                        checks::multi_extract::run(h, &input, &bbox, &polygons, 2, true)
                    } else {
                        checks::extract::run(h, &input, Some(&bbox), &polygons, true)
                    }
                })
                .unwrap();
            assert_eq!(
                status, expected,
                "multi={multi}, sorted={sorted}, fault={fault}"
            );
        }
    }
    // Gate a polygon with a known different interpretation before launching.
    h.run = run::Run::create(&dir, exe, true).unwrap();
    std::fs::write(&polygon, r#"{"type":"FeatureCollection","features":[]}"#).unwrap();
    let error = checks::extract::run(&mut h, &input, None, &polygons, false).unwrap_err();
    assert!(error.to_string().contains("cannot be compared with osmium"));
}

pub fn tool(args: &[OsString]) -> i32 {
    let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
    let path = args
        .iter()
        .map(Path::new)
        .find(|p| {
            p.is_absolute()
                && p.ancestors()
                    .any(|a| a.file_name().is_some_and(|n| n == "extract-fixture"))
        })
        .unwrap();
    let root = path
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "extract-fixture"))
        .unwrap();
    let fault = std::fs::read_to_string(root.join("fault")).unwrap_or_default();
    let value = |flag| args[args.iter().position(|a| *a == flag).unwrap() + 1];
    let output = || {
        Path::new(value(if args.contains(&"--output") {
            "--output"
        } else {
            "-o"
        }))
    };
    match args[0] {
        "inspect" => {
            inspect(path);
        }
        "extract" => {
            if args.contains(&"--config") {
                assert!(args.contains(&"--staging-dir"));
                assert!(args.contains(&"--direct-io"));
                assert!(!args.contains(&"--tmp-dir"));
                let doc: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(value("--config")).unwrap()).unwrap();
                for region in doc["extracts"].as_array().unwrap() {
                    let output = Path::new(doc["directory"].as_str().unwrap())
                        .join(region["output"].as_str().unwrap());
                    let text = std::fs::read_to_string(args[1]).unwrap();
                    std::fs::write(output, if fault == "content" { "wrong" } else { &text })
                        .unwrap();
                }
            } else {
                if args.contains(&"--output") {
                    assert!(args.contains(&"--staging-dir"));
                    let sequential = output()
                        .ancestors()
                        .any(|a| a.file_name().is_some_and(|n| n == "sequential"));
                    assert_eq!(args.contains(&"--direct-io"), !sequential);
                } else if value("-s") == "smart" {
                    assert_eq!(value("-S"), "types=multipolygon,boundary");
                }
                let text = std::fs::read_to_string(args[1]).unwrap();
                let bad = args.contains(&"--output")
                    && fault == "content"
                    && !output()
                        .ancestors()
                        .any(|a| a.file_name().is_some_and(|n| n == "sequential"));
                std::fs::write(output(), if bad { "wrong" } else { &text }).unwrap();
            }
        }
        "sort" => {
            std::fs::write(output(), "sorted").unwrap();
        }
        "cat" => {
            std::fs::write(output(), "").unwrap();
        }
        "diff" => {
            if args.contains(&"--format") {
                assert!(args.contains(&"--staging-dir"));
                assert!(args.contains(&"--tmp-dir"));
                let different = args.iter().any(|a| {
                    Path::new(a).is_file() && std::fs::read_to_string(a).is_ok_and(|s| s == "wrong")
                });
                let body = if different {
                    "<modify><node id=\"1\"/></modify>"
                } else {
                    ""
                };
                std::fs::write(
                    output(),
                    format!("<osmChange version=\"0.6\">{body}</osmChange>"),
                )
                .unwrap();
                return i32::from(different);
            }
            assert!(args.contains(&"--suppress-common"));
            if std::fs::read(args[2]).unwrap() != std::fs::read(args[3]).unwrap() {
                println!("*n1");
                return 1;
            }
        }
        _ => panic!("unexpected extract fixture invocation: {args:?}"),
    }
    0
}

fn inspect(path: &Path) {
    let text = std::fs::read_to_string(path).unwrap();
    let features = if text.contains("unsorted") {
        ""
    } else {
        r#""Sort.Type_then_ID""#
    };
    print!(
        r#"{{"header":{{"required_features":[],"optional_features":[{features}]}},"elements":{{"nodes":1,"ways":0,"relations":0}},"ordering":{{"standard":true}},"data":{{"objects_ordered":true}}}}"#
    );
}
