//! Generated extract protocols: no datasets, network or installed tools.
use crate::{
    checks, extract_config::PolygonInput, fake_pbf, harness, process, reference, reference_cache,
    run,
};
use std::ffi::OsString;
use std::path::Path;

const SORTED: &str = "Sort.Type_then_ID";

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
        reference::Tools { osmium: exe.into() },
        false,
        cache,
    );
    for (sorted, fault, expected) in [
        (true, "", harness::CheckStatus::Passed),
        (false, "", harness::CheckStatus::Passed),
        (true, "content", harness::CheckStatus::Failed),
    ] {
        if sorted {
            fake_pbf::write(&input, &[SORTED], "sorted");
        } else {
            fake_pbf::write(&input, &[], "unsorted");
        }
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
    if args[0] == "cat" && args.contains(&"relation") {
        // osmium's complete_ways relations: the fixture has none.
        std::fs::write(output(), "").unwrap();
        return 0;
    }
    if fake_pbf::is_opl_cat(&args) {
        fake_pbf::opl_cat(&args);
        return 0;
    }
    match args[0] {
        "fileinfo" => print!("{}", fake_pbf::fileinfo(1, 0, 0)),
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
                    if fault == "content" {
                        fake_pbf::rewrite(Path::new(args[1]), &output, "wrong");
                    } else {
                        std::fs::copy(args[1], output).unwrap();
                    }
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
                let bad = args.contains(&"--output")
                    && fault == "content"
                    && !output()
                        .ancestors()
                        .any(|a| a.file_name().is_some_and(|n| n == "sequential"));
                if bad {
                    fake_pbf::rewrite(Path::new(args[1]), output(), "wrong");
                } else {
                    std::fs::copy(args[1], output()).unwrap();
                }
            }
        }
        "sort" => fake_pbf::write(output(), &[SORTED], "sorted"),
        _ => panic!("unexpected extract fixture invocation: {args:?}"),
    }
    0
}
