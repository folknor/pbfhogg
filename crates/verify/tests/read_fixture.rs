//! Generated tool protocols exercise the read checks without datasets or tools.
use std::ffi::OsString;
use std::path::Path;

use crate::{checks, fake_pbf, harness, process, reference, reference_cache, run};

const SORTED: &str = "Sort.Type_then_ID";
const LOW: &str = "LocationsOnWays";
const CAT_FIXTURE: &str = "FixtureCat";

#[allow(
    clippy::too_many_lines,
    reason = "Keep the protocol success and fault cases together for audit"
)]
pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join("read-fixture");
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    fake_pbf::write(&input, &[SORTED], "sorted");
    let cat_input = dir.join("cat-input.pbf");
    fake_pbf::write(&cat_input, &[SORTED, CAT_FIXTURE], "sorted");
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
    for (name, run, input) in [
        ("cat", checks::cat::run as checks::ReadCheck, &cat_input),
        ("tags-filter", checks::tags_filter::run, &input),
    ] {
        h.run.watch(input).unwrap();
        assert_eq!(
            h.run_check(name, |h| run(h, input, true)).unwrap(),
            harness::CheckStatus::Passed
        );
    }
    // A content change fails both, and so does a version-only change: both
    // compare metadata, every arm by name.
    for fault in ["content", "metadata"] {
        for (name, run, input, arms) in [
            (
                "bad-cat",
                checks::cat::run as checks::ReadCheck,
                &cat_input,
                &[
                    "cat -t node diff",
                    "cat -t way diff",
                    "cat -t relation diff",
                ][..],
            ),
            (
                "bad-tags",
                checks::tags_filter::run,
                &input,
                &TAGS_FILTER_ARMS[..],
            ),
        ] {
            h.run = run::Run::create(&dir, exe, true).unwrap();
            h.run.watch(input).unwrap();
            std::fs::write(dir.join("fault"), fault).unwrap();
            let judged = super::judge(&mut h, name, |h| run(h, input, true));
            assert_eq!(
                judged.status,
                harness::CheckStatus::Failed,
                "{name}: {fault}"
            );
            assert_eq!(judged.error, None, "{name}: {fault}");
            assert_eq!(judged.failed, arms, "{name}: {fault}");
        }
    }
    tags_filter_domain(&mut h, exe, &dir);
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
    let unsorted_cat = dir.join("unsorted-cat.pbf");
    fake_pbf::write(&unsorted_cat, &[CAT_FIXTURE], "unsorted");
    for (name, run, input) in [
        (
            "unsorted-cat",
            checks::cat::run as checks::ReadCheck,
            &unsorted_cat,
        ),
        ("unsorted-tags", checks::tags_filter::run, &unsorted),
    ] {
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(input).unwrap();
        assert_eq!(
            h.run_check(name, |h| run(h, input, true)).unwrap(),
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

/// The tags-filter comparisons a content or version fault fails, in order.
const TAGS_FILTER_ARMS: [&str; 7] = [
    "tags-filter --omit-referenced highway=primary diff",
    "tags-filter --omit-referenced amenity=restaurant diff",
    "tags-filter --omit-referenced w/highway=primary diff",
    "tags-filter highway=primary diff",
    "tags-filter r/type=route_master diff",
    "tags-filter --remove-tags w/highway=primary diff",
    "tags-filter --invert-match highway=primary diff",
];

/// The default-mode domain: a fold witness in osmium's default output (a
/// negative reference or object ID), a history header, and pbfhogg's
/// refusal of disagreeing repeats on input osmium finds repeating are each
/// the verifier's error, never a FAIL; the same refusal on input without a
/// repeat is pbfhogg's failure. Each case gets its own input marker, so no
/// cached reference run or stored reading of an earlier case answers it.
fn tags_filter_domain(h: &mut harness::Harness, exe: &Path, dir: &Path) {
    for (fault, said) in [
        ("negative-ref", "way 1 references node -2"),
        ("negative-id", "holds way -3"),
        ("history", "declares HistoricalInformation"),
        ("repeats", "repeats an ID"),
    ] {
        let input = dir.join(format!("tags-{fault}.pbf"));
        let required: &[&str] = if fault == "history" {
            &["HistoricalInformation"]
        } else {
            &[]
        };
        fake_pbf::write_with(&input, required, &[SORTED], fault);
        std::fs::write(dir.join("fault"), fault).unwrap();
        h.run = run::Run::create(dir, exe, true).unwrap();
        h.run.watch(&input).unwrap();
        match checks::tags_filter::run(h, &input, true) {
            Err(crate::error::Error::Tool(why)) => {
                assert!(why.contains(said), "{fault}: {why}");
            }
            other => panic!("{fault}: expected a refusal, got {other:?}"),
        }
    }
    let input = dir.join("tags-lone-refusal.pbf");
    fake_pbf::write(&input, &[SORTED], "lone-refusal");
    std::fs::write(dir.join("fault"), "lone-refusal").unwrap();
    h.run = run::Run::create(dir, exe, true).unwrap();
    h.run.watch(&input).unwrap();
    let judged = super::judge(h, "lone-refusal", |h| {
        checks::tags_filter::run(h, &input, true)
    });
    assert_eq!(judged.status, harness::CheckStatus::Failed, "{judged:?}");
    std::fs::write(dir.join("fault"), "").unwrap();
}

/// Whether `args` is pbfhogg tags-filter in default mode.
fn pbfhogg_default_tags_filter(args: &[&str]) -> bool {
    args[0] == "tags-filter" && args.contains(&"--output") && !args.contains(&"--omit-referenced")
}

/// osmium fileinfo, whose report finds an ID repeated under the repeats
/// fault, and pbfhogg's default-mode refusal of disagreeing repeats under
/// the repeats and lone-refusal faults: the exit code when `args` is one.
fn repeat_fault(args: &[&str], fault: &str) -> Option<i32> {
    if args[0] == "fileinfo" {
        let report = fake_pbf::fileinfo(3, 2, 1);
        if fault == "repeats" {
            print!(
                "{}",
                report.replace(
                    r#""multiple_versions":false"#,
                    r#""multiple_versions":true"#
                )
            );
        } else {
            print!("{report}");
        }
        return Some(0);
    }
    if matches!(fault, "repeats" | "lone-refusal") && pbfhogg_default_tags_filter(args) {
        eprintln!(
            "tags-filter without omit-referenced selects objects by ID, and the input repeats \
             an ID whose records disagree about the expressions"
        );
        return Some(2);
    }
    None
}

/// Under the sign faults, osmium's default-mode tags-filter outputs also
/// hold a fold witness, after the record opl_cat wrote to `out`.
fn add_fold_witness(args: &[&str], fault: &str, out: &Path) {
    let name = Path::new(args[1])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.starts_with("osmium-") || name.ends_with("-R.osm.pbf") {
        return;
    }
    let witness = match fault {
        "negative-ref" => "w1 T Nn-2\n",
        "negative-id" => "w-3 T N\n",
        _ => return,
    };
    let mut opl = std::fs::read_to_string(out).unwrap();
    opl.push_str(witness);
    std::fs::write(out, opl).unwrap();
}

/// An input declaring LocationsOnWays whose first data blob is indexed
/// (a believed node claim, so sort has no way blob to rewrite): cat and
/// sort keep the declaration, so both outputs declare it and every
/// element comparison reads inline coordinates. One way coordinate
/// altered on pbfhogg's outputs alone must fail the arms that contain it by
/// name and no other; a clean run passes.
fn kept_locations(h: &mut harness::Harness, exe: &Path, dir: &Path) {
    let indexed = dir.join("indexed.pbf");
    fake_pbf::write_indexed(&indexed, &[SORTED, LOW, CAT_FIXTURE], "sorted @w1");
    for fault in ["", "coords"] {
        for (name, run, arms) in [
            (
                "indexed-cat",
                checks::cat::run as checks::ReadCheck,
                &["cat -t way diff"][..],
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

/// Give the cat fixture one record of each kind and select only the kind
/// named in the output's bytes. Keep the selection through a fake sort.
fn cat_decoding(args: &[&str], output: &Path) {
    let features = fake_pbf::features(Path::new(args[1]));
    if !features.iter().any(|feature| feature == CAT_FIXTURE) {
        return;
    }
    let opl = std::fs::read_to_string(output).unwrap();
    let mut lines = opl.lines();
    let node = lines.next().unwrap();
    let stem = node
        .strip_prefix('n')
        .unwrap()
        .strip_suffix(" x1 y1")
        .unwrap();
    let (_, refs) = lines
        .next()
        .and_then(|line| line.rsplit_once(" N"))
        .unwrap_or(("", "n1"));
    let way = format!("w{stem} N{refs}");
    let relation = format!("r{stem} M");
    let kind = features
        .iter()
        .find_map(|feature| feature.strip_prefix("FixtureType."));
    let text = match kind {
        Some("node") => format!("{node}\n"),
        Some("way") => format!("{way}\n"),
        Some("relation") => format!("{relation}\n"),
        None => format!("{node}\n{way}\n{relation}\n"),
        Some(kind) => panic!("unexpected fixture kind {kind}"),
    };
    std::fs::write(output, text).unwrap();
}

fn mark_cat_output(args: &[&str], output: &Path) {
    let mut features = fake_pbf::features(output);
    if features.iter().any(|feature| feature == CAT_FIXTURE) {
        let at = args.iter().position(|&arg| arg == "--type").unwrap();
        features.push(format!("FixtureType.{}", args[at + 1]));
        let features: Vec<_> = features.iter().map(String::as_str).collect();
        let marker = format!("{}{}", fake_pbf::marker(output), fake_pbf::suffixes(output));
        fake_pbf::write(output, &features, &marker);
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
    clippy::cognitive_complexity,
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
        cat_decoding(&args, output());
        add_fold_witness(&args, &fault, output());
        return 0;
    }
    if let Some(code) = repeat_fault(&args, &fault) {
        return code;
    }
    match args[0] {
        "cat" | "tags-filter" => {
            if args.contains(&"--output") {
                assert!(args.contains(&"--direct-io"));
                assert!(args.contains(&"--staging-dir"));
                assert!(!args.contains(&"--tmp-dir"));
            } else if args[0] == "cat" {
                asks_locations(&args);
                if fake_pbf::features(Path::new(args[1]))
                    .iter()
                    .any(|feature| feature == CAT_FIXTURE)
                {
                    assert_eq!(value("-t"), "changeset");
                }
            }
            std::fs::copy(args[1], output()).unwrap();
            if args.contains(&"--output") {
                fault_output(&fault, Path::new(args[1]), output());
                if args[0] == "cat" {
                    mark_cat_output(&args, output());
                }
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
            let input_features = fake_pbf::features(Path::new(args[1]));
            let mut features = vec![SORTED];
            if locations {
                features.push(LOW);
            }
            if input_features.iter().any(|feature| feature == CAT_FIXTURE) {
                assert!(!args.contains(&"multipass"));
                features.extend(
                    input_features
                        .iter()
                        .map(String::as_str)
                        .filter(|feature| feature.starts_with("Fixture")),
                );
            }
            // Sorting keeps every field, the way and its coordinate too.
            let marker = format!("sorted{}", fake_pbf::suffixes(Path::new(args[1])));
            fake_pbf::write(output(), &features, &marker);
        }
        "check" if args.contains(&"--refs") => {
            assert!(!args.contains(&"--direct-io"));
            if fault == "report" {
                println!("Referential integrity: OK\nMissing node refs in ways: 4");
            } else if args.contains(&"--check-relations") {
                // One missing node referenced twice, one missing relation
                // referenced twice: the total counts each ID once.
                println!(
                    "Missing node refs in ways: 1 (2 references)\nMissing relation members: 1 (2 references)\nReferential integrity: FAILED (2 missing references)"
                );
                return 1;
            } else {
                println!(
                    "Missing node refs in ways: 1 (2 references)\nReferential integrity: FAILED (1 missing references)"
                );
                return 1;
            }
        }
        "check-refs" => {
            // osmium counts every referencing occurrence.
            eprintln!(
                "There are 3 nodes, 2 ways, and 1 relations in this file.\nNodes in ways missing: 2"
            );
            if args.contains(&"-r") {
                eprintln!(
                    "Nodes in relations missing: 0\nWays in relations missing: 0\nRelations in relations missing: 2"
                );
            }
            return 1;
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
