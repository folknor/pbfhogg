//! Generated OSC protocol exercises the production filter and its failure paths.
use crate::{checks, harness, process, reference, reference_cache, run};
use std::ffi::OsString;
use std::path::Path;

const HEAD: &str = "<osmChange version=\"0.6\">";
const CREATE: &str = "<create><node id=\"1\" version=\"1\" lat=\"1\" lon=\"2\"><tag k=\"amenity\" v=\"restaurant\"/></node></create>";
const DELETE: &str = "<delete><way id=\"2\" version=\"2\"><nd ref=\"1\"/><tag k=\"highway\" v=\"primary\"/></way></delete>";
const NODE: &str = "n1 v1 dV c0 t i0 u Tamenity=restaurant x2 y1";
const WAY: &str = "w2 v2 dD c0 t i0 u Thighway=primary N1";

pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join("change-fixture");
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.osc");
    std::fs::write(&input, format!("{HEAD}{DELETE}{CREATE}</osmChange>")).unwrap();
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, "change-fixture".into(), false).unwrap();
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
    for fault in ["", "", "drop-delete", "serial"] {
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(&input).unwrap();
        std::fs::write(dir.join("fault"), fault).unwrap();
        assert_eq!(
            h.run_check("osc-filter", |h| checks::tags_filter_osc::run(h, &input))
                .unwrap(),
            if fault.is_empty() {
                harness::CheckStatus::Passed
            } else {
                harness::CheckStatus::Failed
            }
        );
    }
}

pub fn tool(args: &[OsString]) -> i32 {
    let root = args
        .iter()
        .find_map(|a| {
            Path::new(a)
                .ancestors()
                .find(|p| p.file_name().is_some_and(|n| n == "change-fixture"))
        })
        .unwrap();
    let fault = std::fs::read_to_string(root.join("fault")).unwrap_or_default();
    let output = args
        .windows(2)
        .find(|a| a[0] == "--output" || a[0] == "-o")
        .map(|a| Path::new(&a[1]))
        .unwrap();
    if args[0] == "cat" {
        let input = args
            .iter()
            .map(Path::new)
            .find(|p| p.extension().is_some_and(|e| e == "osc" || e == "gz"))
            .unwrap();
        let xml = std::fs::read_to_string(input).unwrap();
        let mut records = Vec::new();
        for (needle, line) in [("id=\"1\"", NODE), ("id=\"2\"", WAY)] {
            if let Some(at) = xml.find(needle) {
                records.push((at, line));
            }
        }
        records.sort();
        std::fs::write(
            output,
            records
                .iter()
                .map(|(_, l)| format!("{l}\n"))
                .collect::<String>(),
        )
        .unwrap();
    } else {
        let expressions: Vec<_> = args
            .iter()
            .filter_map(|a| a.to_str())
            .filter(|s| s.contains('=') || *s == "n/amenity")
            .collect();
        let node = expressions
            .iter()
            .any(|s| *s == "amenity=restaurant" || *s == "n/amenity");
        let way =
            expressions.contains(&"highway=primary") || expressions.contains(&"w/highway=primary");
        if args.iter().any(|a| a == "--input-kind") {
            assert!(args.iter().any(|a| a == "--staging-dir"));
            assert!(!args.iter().any(|a| a == "--tmp-dir" || a == "--direct-io"));
            let delete = if fault == "drop-delete" { "" } else { DELETE };
            let create = if node { CREATE } else { "" };
            let suffix = if fault == "serial" && args.iter().any(|a| a == "--jobs") {
                "\n"
            } else {
                ""
            };
            std::fs::write(
                output,
                format!("{HEAD}{create}{delete}</osmChange>{suffix}"),
            )
            .unwrap();
        } else {
            let lines = format!(
                "{}{}",
                if way {
                    format!("{WAY}\n")
                } else {
                    String::new()
                },
                if node {
                    format!("{NODE}\n")
                } else {
                    String::new()
                }
            );
            std::fs::write(output, lines).unwrap();
        }
    }
    0
}
