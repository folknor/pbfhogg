//! Generated apply-changes protocol, with no datasets or network.
use crate::{checks, fake_pbf, harness, process, reference, reference_cache, run};
use std::ffi::OsString;
use std::path::Path;

const SORTED: &str = "Sort.Type_then_ID";
const LOW: &str = "LocationsOnWays";
/// The CLI's rendering of the library's `header.way_metadata_loss`.
const LOSS_WARNING: &str = "Warning: the input declares LocationsOnWays, which the output does \
                            not declare: this command does not keep it on every way it writes, \
                            or cannot vouch for it in the output";

const T1: &str = "2020-01-01T00:00:00Z";
const T2: &str = "2020-01-02T00:00:00Z";

/// A node record as osmium's OPL writer spells it with metadata.
fn node(id: i64, version: i64, timestamp: &str, tags: &str) -> String {
    format!("n{id} v{version} dV c1 t{timestamp} i1 u T{tags} x1 y1\n")
}

/// One fault's inputs and tool outputs: the OSC, osmium's OPL decoding of
/// it, the base records osmium getid finds for its identities, and the OPL
/// each tool's output decodes to.
struct Case {
    osc: &'static str,
    osc_opl: String,
    base: String,
    pbfhogg: String,
    osmium: String,
}

/// The default change: a claimless delete of `n7` over a base holding `n1`
/// and `n7` v1. pbfhogg deletes it; plain osmium keeps the base, whose
/// version beats the claimless delete.
fn case(fault: &str) -> Case {
    let n1 = node(1, 1, T1, "");
    let n7 = node(7, 1, T1, "");
    let delete = Case {
        osc: r#"<osmChange version="0.6"><delete><node id="7"/></delete></osmChange>"#,
        osc_opl: "n7 v0 dD c0 t i0 u T x y\n".to_owned(),
        base: format!("{n1}{n7}"),
        pbfhogg: n1.clone(),
        osmium: format!("{n1}{n7}"),
    };
    let modify = |version: u8| Case {
        osc: if version == 0 {
            r#"<osmChange version="0.6"><modify><node id="7" version="0" timestamp="2020-01-01T00:00:00Z" lat="1" lon="1"/></modify></osmChange>"#
        } else {
            r#"<osmChange version="0.6"><modify><node id="7" version="1" timestamp="2020-01-01T00:00:00Z" lat="1" lon="1"/></modify></osmChange>"#
        },
        osc_opl: node(7, version.into(), T1, ""),
        // The base's equal version carries a later timestamp.
        base: format!("{n1}{}", node(7, 1, T2, "")),
        pbfhogg: format!("{n1}{}", node(7, version.into(), T1, "")),
        osmium: format!("{n1}{}", node(7, 1, T2, "")),
    };
    match fault {
        "unexplained" => Case {
            osmium: format!("{n1}{n7}{}", node(8, 1, T1, "")),
            ..delete
        },
        "modified" => Case {
            pbfhogg: node(1, 1, T1, "a=1"),
            osmium: format!("{}{n7}", node(1, 1, T1, "a=2")),
            ..delete
        },
        "deleted" => Case {
            pbfhogg: format!("{n1}{n7}"),
            ..delete
        },
        "osmium-off-model" => Case {
            osmium: n1.clone(),
            ..delete
        },
        "metadata" => Case {
            pbfhogg: node(1, 2, T1, ""),
            osmium: format!("{}{n7}", node(1, 1, T1, "")),
            ..delete
        },
        "coords" => Case {
            pbfhogg: format!("{n1}w1 v1 dV c1 t{T1} i1 u T Nn1x2y1\n"),
            osmium: format!("{n1}{n7}w1 v1 dV c1 t{T1} i1 u T Nn1x1y1\n"),
            ..delete
        },
        "precedence" => modify(1),
        "precedence-wrong" => Case {
            pbfhogg: format!("{n1}{}", node(7, 1, T2, "")),
            ..modify(1)
        },
        "regression" | "regression-accepted" => modify(0),
        _ => delete,
    }
}

pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join("apply-fixture");
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    let osc = dir.join("input.osc");
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, "apply".into(), false).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools { osmium: exe.into() },
        false,
        cache,
    );
    for (fault, expected) in [
        ("", true),
        ("unexplained", false),
        ("modified", false),
        ("deleted", false),
        ("osmium-off-model", false),
        ("stale", false),
        ("osmium-stale", true),
        ("unsorted", true),
        ("publish", false),
        ("silent-drop", false),
        ("metadata", false),
        ("coords", true),
        ("precedence", true),
        ("precedence-wrong", false),
        ("regression", true),
        ("regression-accepted", false),
    ] {
        std::fs::write(dir.join("fault"), fault).unwrap();
        std::fs::write(&osc, case(fault).osc).unwrap();
        if fault == "unsorted" || fault == "publish" {
            fake_pbf::write(&input, &[LOW], "unsorted");
        } else {
            // The fault rides in the input's content, so no reference run is
            // served from the cache entry of another fault.
            fake_pbf::write(&input, &[SORTED, LOW], &format!("sorted {fault}"));
        }
        for locations in [false, true] {
            h.run = run::Run::create(&dir, exe, true).unwrap();
            h.run.watch(&input).unwrap();
            h.run.watch(&osc).unwrap();
            let judged = super::judge(&mut h, "apply", |h| {
                checks::apply_changes::run(h, &input, &osc, true, locations)
            });
            // Under -l osmium keeps or drops a base that does not lose by
            // its cursor, so the model leaves it undecided there.
            let pass = expected
                || (fault == "stale" && !locations)
                || (fault == "osmium-off-model" && locations);
            assert_apply(fault, locations, &judged);
            assert_eq!(
                judged.status,
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

/// Hold one apply fault to the comparison it fails by name, or to the
/// element judgment's error: an element difference, the metadata fault
/// among them, must be the unexplained-entries error in both modes, never a
/// pass the coordinate or declaration arms could mask.
fn assert_apply(fault: &str, locations: bool, judged: &super::Judged) {
    let case = format!("fault={fault}, locations={locations}: {judged:?}");
    let unexplained = match fault {
        "unexplained" | "modified" | "deleted" | "metadata" | "precedence-wrong" => Some(1),
        _ => None,
    };
    assert_eq!(
        judged.error,
        unexplained.map(|n| format!(
            "pbfhogg apply-changes: {n} unexplained difference entries (see element diff above)"
        )),
        "{case}"
    );
    match fault {
        "stale" if locations => assert_eq!(
            judged.failed,
            ["pbfhogg apply-changes inline locations"],
            "{case}"
        ),
        "silent-drop" => assert_eq!(
            judged.failed,
            ["pbfhogg apply-changes output declares LocationsOnWays as its command documents"],
            "{case}"
        ),
        "osmium-off-model" if !locations => assert_eq!(
            judged.failed,
            ["osmium apply-changes departs from the verifier's precedence model on 1 identities"],
            "{case}"
        ),
        "regression-accepted" => assert!(
            judged
                .failed
                .iter()
                .any(|f| f == "pbfhogg apply-changes refuses the version regression")
                && judged
                    .failed
                    .iter()
                    .any(|f| f.starts_with("pbfhogg apply-changes published ")),
            "{case}"
        ),
        // The published path is per run: hold the one entry by its frame.
        "publish" => assert!(
            matches!(judged.failed.as_slice(), [one]
                if one.starts_with("pbfhogg apply-changes published ")
                    && one.ends_with("/pbfhogg.osm.pbf on input it refuses")),
            "{case}"
        ),
        _ => assert!(judged.failed.is_empty(), "{case}"),
    }
}

pub fn tool(args: &[OsString]) -> i32 {
    let args: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
    let root = args
        .iter()
        .map(|s| s.strip_prefix("file=").unwrap_or(s))
        .map(Path::new)
        .flat_map(Path::ancestors)
        .find(|p| p.file_name().is_some_and(|n| n == "apply-fixture"))
        .unwrap();
    let fault = std::fs::read_to_string(root.join("fault")).unwrap();
    let case = case(&fault);
    let value = |flag| args[args.iter().position(|s| *s == flag).unwrap() + 1];
    if fake_pbf::is_opl_cat(&args) && args[1].ends_with(".osc") {
        std::fs::write(value("-o"), fake_pbf::project(&args, &case.osc_opl)).unwrap();
        return 0;
    }
    if fake_pbf::is_opl_cat(&args) {
        // Keyed on content alone, as the reference cache keys the decode:
        // the marker names the side and the fault that wrote it.
        let marker = fake_pbf::marker(Path::new(args[1]));
        let (side, written_under) = marker.split_once('-').unwrap();
        let written = self::case(written_under);
        let opl = if side == "pbfhogg" {
            written.pbfhogg
        } else {
            written.osmium
        };
        std::fs::write(value("-o"), fake_pbf::project(&args, &opl)).unwrap();
        return 0;
    }
    match args[0] {
        "fileinfo" => print!("{}", fake_pbf::fileinfo(1, 1, 0)),
        "getid" => {
            assert!(args.contains(&"-H") && args.contains(&"-I"));
            std::fs::write(value("-o"), fake_pbf::project(&args, &case.base)).unwrap();
        }
        "apply-changes" => {
            let ours = args.contains(&"--output");
            let output = value(if ours { "--output" } else { "-o" });
            if ours {
                assert!(args.contains(&"--direct-io"));
                assert!(args.contains(&"--staging-dir"));
                assert!(!args.contains(&"--tmp-dir"));
            }
            if fake_pbf::marker(Path::new(args[1])) == "unsorted" {
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
            if fault == "regression" {
                assert!(ours, "references must not run on a predicted refusal");
                eprintln!(
                    "Error: apply-changes base merge: node 7 ...; pass --allow-version-regression \
                     to permit lower-version changes"
                );
                return 2;
            }
            // pbfhogg keeps the declaration under -l and otherwise drops it
            // with the loss warning; "silent-drop" loses it without one.
            let locations = args.contains(&"--locations-on-ways");
            let features: &[&str] = if ours && (!locations || fault == "silent-drop") {
                if fault != "silent-drop" {
                    eprintln!("{LOSS_WARNING}");
                }
                &[SORTED]
            } else {
                &[SORTED, LOW]
            };
            fake_pbf::write(
                Path::new(output),
                features,
                &format!("{}-{fault}", if ours { "pbfhogg" } else { "osmium" }),
            );
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
        other => panic!("unexpected fake tool invocation {other:?}"),
    }
    0
}
