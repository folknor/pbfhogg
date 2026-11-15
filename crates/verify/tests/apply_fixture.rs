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

/// The OPL the fake osmium decodes each output to, by fault: the element
/// differences the apply check classifies.
fn opl(fault: &str, pbfhogg: bool) -> &'static str {
    match (fault, pbfhogg) {
        ("explained", false) => "n1 v1\nn7 v1\n",
        ("unexplained", false) => "n1 v1\nn8 v1\n",
        ("modified", true) => "n1 v1\nn7 v1 Ta=1\n",
        ("modified", false) => "n1 v1\nn7 v1 Ta=2\n",
        ("deleted", true) => "n1 v1\nn7 v1\n",
        // Both tools keep the node the change deletes: the outputs agree,
        // and only the final-delete rule can fail it.
        ("both-retain", _) => "n1 v1\nn7 v1\n",
        _ => "n1 v1\n",
    }
}

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
    // A small osmosis install tree, since the reference cache hashes the
    // launcher's install root and JAVA_HOME as osmosis's identity.
    let osmosis = dir.join("osmosis-tool");
    std::fs::create_dir_all(osmosis.join("bin")).unwrap();
    std::fs::create_dir(osmosis.join("jdk")).unwrap();
    std::os::unix::fs::symlink(exe, osmosis.join("bin/osmosis")).unwrap();
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, "apply".into(), false).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools {
            osmium: exe.into(),
            osmosis: Some(reference::OsmosisTools {
                osmosis: osmosis.join("bin/osmosis"),
                java_home: osmosis.join("jdk"),
            }),
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
        ("both-retain", false),
        ("stale", false),
        ("context-failure", true),
        ("osmium-stale", true),
        ("unsorted", true),
        ("publish", false),
        ("silent-drop", false),
    ] {
        std::fs::write(dir.join("fault"), fault).unwrap();
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
            // osmosis context runs only without -l and when the check gets
            // past the refusal; a failed run leaves no output behind.
            if !locations && fault.is_empty() {
                assert!(context_output(&h).exists());
            }
            if fault == "context-failure" {
                assert!(!context_output(&h).exists(), "locations={locations}");
            }
        }
    }
}

fn context_output(h: &harness::Harness) -> std::path::PathBuf {
    h.run
        .dir
        .join("checks")
        .join("apply-changes")
        .join("osmosis.osm.pbf")
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
    let value = |flag| args[args.iter().position(|s| *s == flag).unwrap() + 1];
    if fake_pbf::is_opl_cat(&args) {
        // Keyed on content alone, as the reference cache keys the decode:
        // the marker names the side and the fault that wrote it.
        let marker = fake_pbf::marker(Path::new(args[1]));
        let (side, written_under) = marker.split_once('-').unwrap();
        std::fs::write(value("-o"), opl(written_under, side == "pbfhogg")).unwrap();
        return 0;
    }
    match args[0] {
        "fileinfo" => print!("{}", fake_pbf::fileinfo(1, 1, 0)),
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
        "--read-xml-change" => {
            // The optional osmosis context run: its output is the last
            // `file=` operand, after `--write-pbf`. A failure leaves a
            // partial output the check must remove.
            let output = args.last().and_then(|a| a.strip_prefix("file=")).unwrap();
            std::fs::write(output, "sorted").unwrap();
            if fault == "context-failure" {
                return 1;
            }
        }
        other => panic!("unexpected fake tool invocation {other:?}"),
    }
    0
}
