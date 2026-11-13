//! Generated tool protocols exercise the read checks without datasets or tools.
use std::ffi::OsString;
use std::path::Path;

use crate::{checks, harness, process, reference, reference_cache, run};

#[allow(
    clippy::too_many_lines,
    reason = "Keep the protocol success and fault cases together for audit"
)]
pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join("read-fixture");
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    std::fs::write(&input, "sorted").unwrap();
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, "read-fixture".into(), false).unwrap();
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
    h.run.watch(&input).unwrap();
    for (name, run) in [
        ("cat", checks::cat::run as checks::ReadCheck),
        ("tags-filter", checks::tags_filter::run),
    ] {
        assert_eq!(
            h.run_check(name, |h| run(h, &input, true)).unwrap(),
            harness::CheckStatus::Passed
        );
    }
    for (name, run) in [
        ("bad-cat", checks::cat::run as checks::ReadCheck),
        ("bad-tags", checks::tags_filter::run),
    ] {
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(&input).unwrap();
        std::fs::write(dir.join("fault"), "content").unwrap();
        assert_eq!(
            h.run_check(name, |h| run(h, &input, true)).unwrap(),
            harness::CheckStatus::Failed
        );
    }
    std::fs::write(dir.join("fault"), "").unwrap();
    assert_eq!(
        h.run_check("refs", |h| checks::check_refs::run(h, &input))
            .unwrap(),
        harness::CheckStatus::Passed
    );
    assert_eq!(
        h.run_check("locations", |h| checks::add_locations::run(
            h,
            &input,
            checks::add_locations::AltwMode::All,
            true
        ))
        .unwrap(),
        harness::CheckStatus::Passed
    );
    // An invalid pbfhogg report and a failed engine must become FAIL.
    h.run = run::Run::create(&dir, exe, true).unwrap();
    h.run.watch(&input).unwrap();
    std::fs::write(dir.join("fault"), "report").unwrap();
    assert_eq!(
        h.run_check("bad-refs", |h| checks::check_refs::run(h, &input))
            .unwrap(),
        harness::CheckStatus::Failed
    );
    h.run = run::Run::create(&dir, exe, true).unwrap();
    h.run.watch(&input).unwrap();
    std::fs::write(dir.join("fault"), "engine").unwrap();
    assert_eq!(
        h.run_check("bad-engine", |h| checks::add_locations::run(
            h,
            &input,
            checks::add_locations::AltwMode::All,
            false
        ))
        .unwrap(),
        harness::CheckStatus::Failed
    );
    std::fs::write(dir.join("fault"), "").unwrap();
    // A separate generated input routes external to refusal only.
    h.run = run::Run::create(&dir, exe, true).unwrap();
    let unsorted = dir.join("unsorted.pbf");
    std::fs::write(&unsorted, "unsorted").unwrap();
    h.run.watch(&unsorted).unwrap();
    assert_eq!(
        h.run_check("external-refusal", |h| checks::add_locations::run(
            h,
            &unsorted,
            checks::add_locations::AltwMode::External,
            true
        ))
        .unwrap(),
        harness::CheckStatus::Passed
    );
    h.run = run::Run::create(&dir, exe, true).unwrap();
    h.run.watch(&unsorted).unwrap();
    std::fs::write(dir.join("fault"), "publish").unwrap();
    assert_eq!(
        h.run_check("bad-refusal", |h| checks::add_locations::run(
            h,
            &unsorted,
            checks::add_locations::AltwMode::External,
            false
        ))
        .unwrap(),
        harness::CheckStatus::Failed
    );
    std::fs::write(dir.join("fault"), "").unwrap();
    for (name, run) in [
        ("unsorted-cat", checks::cat::run as checks::ReadCheck),
        ("unsorted-tags", checks::tags_filter::run),
    ] {
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(&unsorted).unwrap();
        assert_eq!(
            h.run_check(name, |h| run(h, &unsorted, true)).unwrap(),
            harness::CheckStatus::Passed
        );
    }
    h.run = run::Run::create(&dir, exe, true).unwrap();
    h.run.watch(&unsorted).unwrap();
    assert_eq!(
        h.run_check("unsorted-engines", |h| checks::add_locations::run(
            h,
            &unsorted,
            checks::add_locations::AltwMode::All,
            false
        ))
        .unwrap(),
        harness::CheckStatus::Passed
    );
}

#[allow(
    clippy::too_many_lines,
    reason = "One explicit arm per emulated tool invocation"
)]
pub fn tool(args: &[OsString]) -> i32 {
    let args: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
    let path = args
        .iter()
        .map(Path::new)
        .find(|p| {
            p.is_absolute()
                && p.ancestors()
                    .any(|a| a.file_name().is_some_and(|n| n == "read-fixture"))
        })
        .unwrap();
    let root = path
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "read-fixture"))
        .unwrap();
    let fault = std::fs::read_to_string(root.join("fault")).unwrap_or_default();
    let value = |flag| args[args.iter().position(|s| *s == flag).unwrap() + 1];
    let output = || {
        Path::new(value(if args.contains(&"--output") {
            "--output"
        } else {
            "-o"
        }))
    };
    match args[0] {
        "inspect" => {
            let sorted = std::fs::read_to_string(path).unwrap() != "unsorted";
            let features = if sorted { r#""Sort.Type_then_ID""# } else { "" };
            print!(
                r#"{{"header":{{"required_features":[],"optional_features":[{features}]}},"elements":{{"nodes":3,"ways":2,"relations":1}},"ordering":{{"standard":true}},"data":{{"objects_ordered":true}}}}"#
            );
        }
        "cat" | "tags-filter" => {
            if args.contains(&"--output") {
                assert!(args.contains(&"--direct-io"));
                assert!(args.contains(&"--staging-dir"));
                assert!(!args.contains(&"--tmp-dir"));
            }
            std::fs::copy(args[1], output()).unwrap();
            if args.contains(&"--output") && fault == "content" {
                std::fs::write(output(), "wrong content").unwrap();
            }
        }
        "sort" => {
            std::fs::write(output(), "sorted").unwrap();
        }
        "diff" => {
            assert!(args.contains(&"--suppress-common"));
            if std::fs::read(args[2]).unwrap() != std::fs::read(args[3]).unwrap() {
                println!("*n1");
                return 1;
            }
        }
        "check" if args.contains(&"--refs") => {
            assert!(!args.contains(&"--direct-io"));
            if fault == "report" {
                println!("Referential integrity: OK\nMissing node refs in ways: 4");
            } else if args.contains(&"--check-relations") {
                println!(
                    "Missing relation members: 1 (2 references)\nReferential integrity: FAILED (1 missing references)"
                );
                return 1;
            } else {
                println!("Referential integrity: OK");
            }
        }
        "check-refs" => {
            eprintln!(
                "There are 3 nodes, 2 ways, and 1 relations in this file.\nNodes in ways missing: 0"
            );
            if args.contains(&"-r") {
                eprintln!(
                    "Nodes in relations missing: 0\nWays in relations missing: 0\nRelations in relations missing: 2"
                );
                return 1;
            }
        }
        "add-locations-to-ways" => {
            if args.contains(&"--index-type") {
                assert!(args.contains(&"--staging-dir"));
                assert!(args.contains(&"--tmp-dir"));
                let input = args[1];
                if std::fs::read_to_string(input).unwrap() == "unsorted"
                    && value("--index-type") == "external"
                {
                    assert_eq!(value("--index-type"), "external");
                    if fault == "publish" {
                        std::fs::write(output(), "partial").unwrap();
                    }
                    eprintln!("pbfhogg sort --distrust-metadata {input} -o sorted.osm.pbf");
                    return 2;
                }
                if fault == "engine" && value("--index-type") == "sparse" {
                    return 1;
                }
            } else {
                assert!(args.contains(&"--keep-member-nodes"));
            }
            let unsorted = args.contains(&"--index-type")
                && std::fs::read_to_string(args[1]).unwrap() == "unsorted";
            std::fs::write(output(), if unsorted { "unsorted" } else { "sorted" }).unwrap();
        }
        "check" => {
            assert!(args.contains(&"--locations"));
            assert!(args.contains(&"--agreement"));
        }
        "digest" => {
            assert_eq!(value("--form"), "full");
            let hash = "0123456789abcdef0123456789abcdef";
            println!("xxh128 v2 {hash}\nnode 3 {hash}\nway 2 {hash}\nrelation 1 {hash}");
        }
        _ => panic!("unexpected read fixture invocation: {args:?}"),
    }
    0
}
