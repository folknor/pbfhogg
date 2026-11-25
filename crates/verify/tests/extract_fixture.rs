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
    std::fs::write(dir.join("pbfhogg-bbox"), "0,0,1,1").unwrap();
    std::fs::write(dir.join("osmium-bbox"), "0,0,1,1").unwrap();
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
        // Only the version differs: both checks compare metadata.
        (true, "metadata", harness::CheckStatus::Failed),
        // The outputs are right but the config run took the fallback.
        (true, "route", harness::CheckStatus::Failed),
    ] {
        std::fs::write(dir.join("sorted"), if sorted { "yes" } else { "no" }).unwrap();
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
            let judged = super::judge(&mut h, "fixture", |h| {
                if multi {
                    checks::multi_extract::run(h, &input, &bbox, &polygons, 2, true)
                } else {
                    checks::extract::run(h, &input, Some(&bbox), &polygons, true)
                }
            });
            let case = format!("multi={multi}, sorted={sorted}, fault={fault}: {judged:?}");
            // The route fault touches only the config run, which extract never makes.
            let expected = if fault == "route" && !multi {
                harness::CheckStatus::Passed
            } else {
                expected
            };
            assert_eq!(judged.status, expected, "{case}");
            assert_eq!(judged.error, None, "{case}");
            // Each fault alters one field class of every pbfhogg output, so
            // every arm must fail by name: an arm on a policy blind to it
            // shows as a missing name.
            let failed: &[&str] = match (fault, multi) {
                ("" | "route", false) | ("", true) => &[],
                ("route", true) => &ROUTE_ARMS,
                (_, false) => &EXTRACT_ARMS,
                (_, true) => &MULTI_ARMS,
            };
            assert_eq!(judged.failed, failed, "{case}");
        }
    }
    // osmium folds IDs and references onto their magnitude: an input holding
    // a negative ID, or an osmium output holding a negative reference, is a
    // comparison that cannot be made, refused as the verifier's error, never
    // a FAIL. Each case gets its own input marker, so no cached reference run
    // or stored reading of an earlier case answers it.
    std::fs::write(dir.join("sorted"), "yes").unwrap();
    for (fault, said) in [
        ("negative-id", "holds relation ID -7"),
        ("negative-ref", "way 1 references node -2"),
    ] {
        fake_pbf::write(&input, &[SORTED], fault);
        std::fs::write(dir.join("fault"), fault).unwrap();
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(&input).unwrap();
        match checks::extract::run(&mut h, &input, Some(&bbox), &[], true) {
            Err(crate::error::Error::Tool(why)) => {
                assert!(why.contains(said), "{fault}: {why}");
                assert!(why.contains("absolute value"), "{fault}: {why}");
            }
            other => panic!("{fault}: expected a refusal, got {other:?}"),
        }
    }
    std::fs::write(dir.join("fault"), "").unwrap();
    bbox_spellings(&mut h, exe, &dir, &input);
    // Gate a polygon with a known different interpretation before launching.
    h.run = run::Run::create(&dir, exe, true).unwrap();
    std::fs::write(&polygon, r#"{"type":"FeatureCollection","features":[]}"#).unwrap();
    let error = checks::extract::run(&mut h, &input, None, &polygons, false).unwrap_err();
    assert!(error.to_string().contains("cannot be compared with osmium"));
}

/// An exponent edge libosmium would truncate before scaling reaches osmium
/// as the plain decimal pbfhogg reads (1e-7, column 1), and an edge no
/// coordinate parser takes is refused before any run.
fn bbox_spellings(h: &mut harness::Harness, exe: &Path, dir: &Path, input: &Path) {
    fake_pbf::write(input, &[SORTED], "exponent-bbox");
    let typed = "0.000000001e2,0,1,1";
    std::fs::write(dir.join("pbfhogg-bbox"), typed).unwrap();
    std::fs::write(dir.join("osmium-bbox"), "0.0000001,0,1,1").unwrap();
    h.run = run::Run::create(dir, exe, true).unwrap();
    h.run.watch(input).unwrap();
    let spelled = pbfhogg_devkit::Bbox::parse(typed).unwrap();
    let judged = super::judge(h, "fixture", |h| {
        checks::extract::run(h, input, Some(&spelled), &[], true)
    });
    assert_eq!(judged.status, harness::CheckStatus::Passed, "{judged:?}");
    let wide = pbfhogg_devkit::Bbox::parse("1e10,0,1,1").unwrap();
    h.run = run::Run::create(dir, exe, true).unwrap();
    match checks::extract::run(h, input, Some(&wide), &[], true) {
        Err(crate::error::Error::Tool(why)) => assert!(why.contains("ten integer digits"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    // A box pbfhogg refuses is the verifier's misuse, refused before any run
    // (the fake pbfhogg would accept it), never a pbfhogg failure.
    for bad in ["1,1,0,0", "0,0,0,1", "0,0,1,91"] {
        let bbox = pbfhogg_devkit::Bbox::parse(bad).unwrap();
        h.run = run::Run::create(dir, exe, true).unwrap();
        match checks::extract::run(h, input, Some(&bbox), &[], true) {
            Err(crate::error::Error::Tool(why)) => {
                assert!(why.contains("not a box pbfhogg extract takes"), "{why}");
            }
            other => panic!("expected a refusal of {bad}, got {other:?}"),
        }
    }
}

/// The extract comparisons a content or version fault fails, in order.
const EXTRACT_ARMS: [&str; 8] = [
    "pbfhogg extract --simple element content differs from osmium on 1 shared IDs",
    "pbfhogg extract --complete element content differs from osmium on 1 shared IDs",
    "pbfhogg extract --smart element content differs from osmium on 1 shared IDs",
    "pbfhogg extract --simple element content differs from osmium's complete_ways on 1 IDs",
    "pbfhogg extract --simple on polygon city element content differs from osmium on 1 shared IDs",
    "pbfhogg extract --complete on polygon city element content differs from osmium on 1 shared IDs",
    "pbfhogg extract --smart on polygon city element content differs from osmium on 1 shared IDs",
    "pbfhogg extract --simple on polygon city element content differs from osmium's complete_ways on 1 IDs",
];

/// The multi-extract comparisons a content or version fault fails, in order.
const MULTI_ARMS: [&str; 15] = [
    "multi-extract simple strip-0 element content differs from the sequential extract",
    "multi-extract simple strip-1 element content differs from the sequential extract",
    "multi-extract simple mixed strip-0 element content differs from the sequential extract",
    "multi-extract simple mixed strip-1 element content differs from the sequential extract",
    "multi-extract simple mixed polygon-city element content differs from the sequential extract",
    "multi-extract complete strip-0 element content differs from the sequential extract",
    "multi-extract complete strip-1 element content differs from the sequential extract",
    "multi-extract complete mixed strip-0 element content differs from the sequential extract",
    "multi-extract complete mixed strip-1 element content differs from the sequential extract",
    "multi-extract complete mixed polygon-city element content differs from the sequential extract",
    "multi-extract smart strip-0 element content differs from the sequential extract",
    "multi-extract smart strip-1 element content differs from the sequential extract",
    "multi-extract smart mixed strip-0 element content differs from the sequential extract",
    "multi-extract smart mixed strip-1 element content differs from the sequential extract",
    "multi-extract smart mixed polygon-city element content differs from the sequential extract",
];

/// The multi-extract verdicts the route fault fails, in order.
const ROUTE_ARMS: [&str; 6] = [
    "multi-extract simple took route sequential fallback where the input header predicts single-pass",
    "multi-extract simple mixed took route sequential fallback where the input header predicts single-pass",
    "multi-extract complete took route sequential fallback where the input header predicts single-pass",
    "multi-extract complete mixed took route sequential fallback where the input header predicts single-pass",
    "multi-extract smart took route sequential fallback where the input header predicts single-pass",
    "multi-extract smart mixed took route sequential fallback where the input header predicts single-pass",
];

/// pbfhogg gets the typed bbox, osmium its plain restatement;
/// multi-extract's sequential strips are its own boxes.
fn assert_bbox_spelling(args: &[&str], root: &Path, output: &Path) {
    let expected = |name| std::fs::read_to_string(root.join(name)).unwrap();
    let strip = output
        .ancestors()
        .any(|a| a.file_name().is_some_and(|n| n == "sequential"));
    let typed = args.iter().find_map(|a| a.strip_prefix("-b="));
    if let (false, Some(typed)) = (strip, typed) {
        assert_eq!(typed, expected("pbfhogg-bbox"));
    } else if let Some(at) = args.iter().position(|a| *a == "-b") {
        assert_eq!(args[at + 1], expected("osmium-bbox"));
    }
}

#[allow(clippy::too_many_lines)]
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
        // Under the negative-ref fault, osmium's extract outputs also hold
        // a way referencing a negative node, after the record opl_cat wrote.
        let osmium_side = Path::new(args[1])
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("osmium-"));
        if fault == "negative-ref" && osmium_side {
            let out = output();
            let mut opl = std::fs::read_to_string(out).unwrap();
            opl.push_str("w1 T Nn-2\n");
            std::fs::write(out, opl).unwrap();
        }
        return 0;
    }
    match args[0] {
        "fileinfo" if fault == "negative-id" => print!(
            "{}",
            fake_pbf::fileinfo(1, 0, 2).replace(r#""relations":1}"#, r#""relations":-7}"#)
        ),
        "fileinfo" => print!("{}", fake_pbf::fileinfo(1, 0, 0)),
        "extract" => {
            if args.contains(&"--config") {
                assert!(args.contains(&"--staging-dir"));
                assert!(args.contains(&"--direct-io"));
                assert!(!args.contains(&"--tmp-dir"));
                let doc: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(value("--config")).unwrap()).unwrap();
                // The route line pbfhogg prints: the one the input's sort
                // claim selects, or under the route fault the fallback on a
                // sorted input, as a probe that declines it would.
                let sorted = std::fs::read_to_string(root.join("sorted")).unwrap() == "yes";
                if sorted && fault != "route" {
                    let strategy = if args.contains(&"--simple") {
                        "simple"
                    } else if args.contains(&"--smart") {
                        "smart"
                    } else {
                        "complete_ways"
                    };
                    eprintln!("[multi-extract] single-pass: 2 regions, {strategy} strategy");
                } else {
                    eprintln!(
                        "Note: Multi-extract single-pass probe declined the input; using one \
                         full pass per region"
                    );
                }
                for region in doc["extracts"].as_array().unwrap() {
                    let output = Path::new(doc["directory"].as_str().unwrap())
                        .join(region["output"].as_str().unwrap());
                    if fault == "content" {
                        fake_pbf::rewrite(Path::new(args[1]), &output, "wrong");
                    } else if fault == "metadata" {
                        fake_pbf::alter(Path::new(args[1]), &output, "@v2");
                    } else {
                        std::fs::copy(args[1], output).unwrap();
                    }
                }
            } else {
                assert_bbox_spelling(&args, root, output());
                if args.contains(&"--output") {
                    assert!(args.contains(&"--staging-dir"));
                    let sequential = output()
                        .ancestors()
                        .any(|a| a.file_name().is_some_and(|n| n == "sequential"));
                    assert_eq!(args.contains(&"--direct-io"), !sequential);
                } else if value("-s") == "smart" {
                    assert_eq!(value("-S"), "types=multipolygon,boundary");
                }
                // The faults alter pbfhogg's outputs but not the sequential
                // ones multi-extract compares its own against.
                let ours = args.contains(&"--output")
                    && !output()
                        .ancestors()
                        .any(|a| a.file_name().is_some_and(|n| n == "sequential"));
                if ours && fault == "content" {
                    fake_pbf::rewrite(Path::new(args[1]), output(), "wrong");
                } else if ours && fault == "metadata" {
                    fake_pbf::alter(Path::new(args[1]), output(), "@v2");
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
