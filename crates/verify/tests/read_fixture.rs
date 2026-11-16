//! Generated tool protocols exercise the read checks without datasets or tools.
use std::ffi::OsString;
use std::path::Path;

use crate::{checks, fake_pbf, harness, process, reference, reference_cache, run};

const SORTED: &str = "Sort.Type_then_ID";
const LOW: &str = "LocationsOnWays";

#[allow(
    clippy::too_many_lines,
    reason = "Keep the protocol success and fault cases together for audit"
)]
pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join("read-fixture");
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    fake_pbf::write(&input, &[SORTED], "sorted");
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, "read-fixture".into(), false).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools { osmium: exe.into() },
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
    // A content change fails both, and so does a version-only change: both
    // compare metadata, every arm by name.
    for fault in ["content", "metadata"] {
        for (name, run, arms) in [
            (
                "bad-cat",
                checks::cat::run as checks::ReadCheck,
                [
                    "cat -t node diff",
                    "cat -t way diff",
                    "cat -t relation diff",
                ],
            ),
            (
                "bad-tags",
                checks::tags_filter::run,
                [
                    "tags-filter --omit-referenced highway=primary diff",
                    "tags-filter --omit-referenced amenity=restaurant diff",
                    "tags-filter --omit-referenced w/highway=primary diff",
                ],
            ),
        ] {
            h.run = run::Run::create(&dir, exe, true).unwrap();
            h.run.watch(&input).unwrap();
            std::fs::write(dir.join("fault"), fault).unwrap();
            let judged = super::judge(&mut h, name, |h| run(h, &input, true));
            assert_eq!(
                judged.status,
                harness::CheckStatus::Failed,
                "{name}: {fault}"
            );
            assert_eq!(judged.error, None, "{name}: {fault}");
            assert_eq!(judged.failed, arms, "{name}: {fault}");
        }
    }
    kept_locations(&mut h, exe, &dir);
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
    // One field class altered on the external engine's output alone: its
    // comparison with osmium and the engine parity must each fail, so the
    // parity reads metadata and inline coordinates, and the per-engine
    // comparison of two outputs declaring LocationsOnWays reads them too.
    for fault in ["parity-metadata", "parity-coords"] {
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(&input).unwrap();
        std::fs::write(dir.join("fault"), fault).unwrap();
        let judged = super::judge(&mut h, fault, |h| {
            checks::add_locations::run(h, &input, checks::add_locations::AltwMode::All, true)
        });
        assert_eq!(judged.status, harness::CheckStatus::Failed, "{fault}");
        assert_eq!(judged.error, None, "{fault}");
        assert_eq!(
            judged.failed,
            [
                "add-locations-to-ways --index-type external diff",
                "sparse vs external osmium-decoded records"
            ],
            "{fault}"
        );
    }
    std::fs::write(dir.join("fault"), "").unwrap();
    // A separate generated input routes external to refusal only.
    h.run = run::Run::create(&dir, exe, true).unwrap();
    let unsorted = dir.join("unsorted.pbf");
    fake_pbf::write(&unsorted, &[], "unsorted");
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

/// An input declaring LocationsOnWays whose first data blob is indexed
/// (a believed node claim, so sort has no way blob to rewrite): cat and
/// sort keep the declaration, so both outputs declare it and every
/// element comparison reads inline coordinates. One way coordinate
/// altered on pbfhogg's outputs alone must fail each element arm by
/// name and no other; a clean run passes.
fn kept_locations(h: &mut harness::Harness, exe: &Path, dir: &Path) {
    let indexed = dir.join("indexed.pbf");
    fake_pbf::write_indexed(&indexed, &[SORTED, LOW], "sorted @w1");
    for fault in ["", "coords"] {
        for (name, run, arms) in [
            (
                "indexed-cat",
                checks::cat::run as checks::ReadCheck,
                &[
                    "cat -t node diff",
                    "cat -t way diff",
                    "cat -t relation diff",
                ][..],
            ),
            (
                "indexed-sort",
                checks::sort::run,
                &["pbfhogg vs osmium elements"][..],
            ),
        ] {
            h.run = run::Run::create(dir, exe, true).unwrap();
            h.run.watch(&indexed).unwrap();
            std::fs::write(dir.join("fault"), fault).unwrap();
            let judged = super::judge(h, name, |h| run(h, &indexed, true));
            assert_eq!(judged.error, None, "{name} {fault}: {judged:?}");
            if fault.is_empty() {
                assert_eq!(judged.status, harness::CheckStatus::Passed, "{judged:?}");
                assert!(judged.failed.is_empty(), "{name}: {judged:?}");
            } else {
                assert_eq!(judged.status, harness::CheckStatus::Failed, "{judged:?}");
                assert_eq!(judged.failed, arms, "{name}: {judged:?}");
            }
        }
    }
}

/// Assert that an osmium `cat` or `sort` writing PBF asks to keep inline
/// locations exactly when its input declares them, and answer whether it does.
fn asks_locations(args: &[&str]) -> bool {
    let locations = fake_pbf::features(Path::new(args[1]))
        .iter()
        .any(|f| f == LOW);
    assert_eq!(args.contains(&"pbf,locations_on_ways=true"), locations);
    locations
}

/// Alter pbfhogg's `output` of `input` as `fault` asks: its content, its
/// version alone, or its way's inline coordinate alone.
fn fault_output(fault: &str, input: &Path, output: &Path) {
    match fault {
        "content" => fake_pbf::rewrite(input, output, "wrong content"),
        "metadata" => fake_pbf::alter(input, output, "@v2"),
        "coords" => fake_pbf::alter(input, output, "@w2"),
        _ => {}
    }
}

/// Write an add-locations-to-ways `output` of `input`, by pbfhogg's
/// `engine` or (`None`) by osmium. Every output carries a way with an inline
/// coordinate; the parity faults alter one field class of the external
/// engine's output alone.
fn write_located(fault: &str, input: &Path, output: &Path, engine: Option<&str>) {
    if engine.is_some() && fake_pbf::marker(input) == "unsorted" {
        fake_pbf::write(output, &[LOW], "unsorted @w1");
        return;
    }
    let external = engine == Some("external");
    let marker = match fault {
        "parity-metadata" if external => "sorted @w1 @v2",
        "parity-coords" if external => "sorted @w1 @w2",
        _ => "sorted @w1",
    };
    fake_pbf::write(output, &[SORTED, LOW], marker);
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
    if fake_pbf::is_opl_cat(&args) {
        fake_pbf::opl_cat(&args);
        return 0;
    }
    match args[0] {
        "fileinfo" => print!("{}", fake_pbf::fileinfo(3, 2, 1)),
        "cat" | "tags-filter" => {
            if args.contains(&"--output") {
                assert!(args.contains(&"--direct-io"));
                assert!(args.contains(&"--staging-dir"));
                assert!(!args.contains(&"--tmp-dir"));
            } else if args[0] == "cat" {
                asks_locations(&args);
            }
            std::fs::copy(args[1], output()).unwrap();
            if args.contains(&"--output") {
                fault_output(&fault, Path::new(args[1]), output());
            }
        }
        "sort" if args.contains(&"--output") => {
            // pbfhogg sort: the input's believed node claim leaves no way
            // blob to rewrite, so it keeps the features and every field.
            let input = Path::new(args[1]);
            let features = fake_pbf::features(input);
            let refs: Vec<&str> = features.iter().map(String::as_str).collect();
            let marker = format!("sorted{}", fake_pbf::suffixes(input));
            fake_pbf::write(output(), &refs, &marker);
            fault_output(&fault, input, output());
        }
        "sort" => {
            let locations = asks_locations(&args);
            let features: &[&str] = if locations { &[SORTED, LOW] } else { &[SORTED] };
            // Sorting keeps every field, the way and its coordinate too.
            let marker = format!("sorted{}", fake_pbf::suffixes(Path::new(args[1])));
            fake_pbf::write(output(), features, &marker);
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
                if fake_pbf::marker(Path::new(input)) == "unsorted"
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
            let engine = args
                .contains(&"--index-type")
                .then(|| value("--index-type"));
            write_located(&fault, Path::new(args[1]), output(), engine);
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
