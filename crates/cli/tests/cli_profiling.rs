//! The CLI's profiling contract. An instrumented build (the `hotpath` or
//! `hotpath-alloc` feature) profiles only when the caller asks for a report
//! file through `HOTPATH_OUTPUT_PATH`, as brokkr does on every call it
//! profiles; unasked, it is as quiet as a shipped build. A shipped build
//! ignores the request. The gate's `cli-hotpath` and `cli-hotpath-alloc`
//! sweeps run this file, and the whole CLI suite, instrumented; every other
//! sweep runs the tests that hold for any build.

use std::path::Path;

use crate::common::{generate_nodes, write_multi_block_test_pbf};
use crate::invoker::{CliInvoker, CliOutput};

/// A real workload with stdout output that reaches instrumented library
/// functions: `inspect tags` over a few blobs.
fn fixture(dir: &Path) -> std::path::PathBuf {
    let input = dir.join("input.osm.pbf");
    let mut nodes = generate_nodes(40, 1);
    for node in &mut nodes {
        node.tags = vec![("amenity", "cafe")];
    }
    write_multi_block_test_pbf(&input, &nodes, &[], &[], 10);
    input
}

fn inspect_tags(input: &Path, report: Option<&Path>) -> CliOutput {
    let scratch = pbfhogg_test_scratch::scratch();
    let mut cli = CliInvoker::new(&scratch)
        .arg("inspect")
        .arg("tags")
        .arg(input)
        .arg("amenity=cafe");
    if let Some(report) = report {
        cli = cli.profile_to(report);
    }
    cli.assert_success()
}

/// A profiling request leaves the command's output byte-identical and,
/// unasked, nothing writes a report.
#[test]
fn a_profiling_request_leaves_the_output_unchanged() {
    let dir = crate::common::test_scratch();
    let input = fixture(dir.path());
    let report = dir.path().join("report.json");
    let plain = inspect_tags(&input, None);
    assert!(!report.exists(), "an unprofiled run wrote a report");
    let profiled = inspect_tags(&input, Some(&report));
    assert_eq!(plain.stdout, profiled.stdout, "profiling changed stdout");
    // Not two empty outputs agreeing.
    plain.assert_stdout_contains("40\t\"amenity\"\t\"cafe\"\n");
    assert_eq!(
        report.exists(),
        cfg!(feature = "hotpath"),
        "a report exists exactly when the build is instrumented"
    );
}

/// A profiled launch of an instrumented CLI writes a JSON report naming
/// library functions with calls counted and, under `hotpath-alloc`, bytes
/// counted: the CLI executable's counting allocator is installed, not just
/// the guard.
#[cfg(feature = "hotpath")]
#[test]
fn a_profiled_launch_reports_library_functions() {
    let dir = crate::common::test_scratch();
    let input = fixture(dir.path());
    let report = dir.path().join("report.json");
    inspect_tags(&input, Some(&report));
    let text = std::fs::read(&report).expect("the profile report");
    let doc: serde_json::Value = serde_json::from_slice(&text).expect("a JSON report");
    let section = if cfg!(feature = "hotpath-alloc") {
        "functions_alloc"
    } else {
        "functions_timing"
    };
    let data = doc[section]["data"]
        .as_array()
        .unwrap_or_else(|| panic!("no {section}.data in {doc}"));
    let library: Vec<&serde_json::Value> = data
        .iter()
        .filter(|entry| {
            entry["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("pbfhogg::"))
        })
        .collect();
    assert!(!library.is_empty(), "no library function in {doc}");
    assert!(
        library
            .iter()
            .any(|entry| entry["calls"].as_u64().is_some_and(|calls| calls > 0)),
        "no library call counted in {doc}"
    );
    if cfg!(feature = "hotpath-alloc") {
        // Byte totals are formatted for humans ("831.4 KB"); a missing
        // counting allocator shows as "0 B" on every function.
        assert_eq!(doc[section]["profiling_mode"], "alloc-bytes", "{doc}");
        assert!(
            library.iter().any(|entry| entry["total"]
                .as_str()
                .is_some_and(|total| !total.starts_with("0 "))),
            "no library allocation counted in {doc}"
        );
    }
}
