//! CLI-driven tests for `pbfhogg geocode-query`: one JSON object per query
//! over an index `build-geocode-index` wrote, and refusal of a malformed
//! points file before any answer is printed.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use crate::common::{TestNode, TestWay, write_test_pbf_sorted};
use crate::invoker::CliInvoker;

/// An index over one addressed node and one named street through it.
fn built_index(dir: &Path) -> PathBuf {
    let node = |id, lon, tags| TestNode {
        id,
        lat: 550_000_000,
        lon,
        tags,
        meta: None,
    };
    let nodes = vec![
        node(
            1,
            120_000_000,
            vec![("addr:housenumber", "7"), ("addr:street", "Example Road")],
        ),
        node(2, 120_010_000, vec![]),
    ];
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1, 2],
        tags: vec![("highway", "residential"), ("name", "Example Road")],
        meta: None,
    }];
    let input = dir.join("input.osm.pbf");
    write_test_pbf_sorted(&input, &nodes, &ways, &[]);
    let index = dir.join("index");
    let scratch = pbfhogg_test_scratch::scratch();
    let out = CliInvoker::new(&scratch)
        .arg("build-geocode-index")
        .arg(&input)
        .arg("--output-dir")
        .arg(&index)
        .run();
    assert!(out.status.success(), "{}", out.stderr_str());
    index
}

#[test]
fn answers_one_json_object_per_query_in_order() {
    let dir = pbfhogg_test_scratch::scratch();
    let index = built_index(dir.path());
    let points = dir.path().join("points.txt");
    std::fs::write(&points, "55.0 12.0\n10.0 -40.0\n").unwrap();
    let scratch = pbfhogg_test_scratch::scratch();
    let out = CliInvoker::new(&scratch)
        .arg("geocode-query")
        .arg(&index)
        .arg(&points)
        .run();
    assert!(out.status.success(), "{}", out.stderr_str());
    let answers: Vec<serde_json::Value> = out
        .stdout_str()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(answers.len(), 2);

    let near = &answers[0];
    assert_eq!(near["lat"], 55.0);
    assert_eq!(near["lon"], 12.0);
    let address = &near["address"];
    assert_eq!(address["house_number"], "7");
    assert_eq!(address["street"], "Example Road");
    assert!(address["place"].is_null() && address["postcode"].is_null());
    assert_eq!(address["lat_e7"], 550_000_000);
    assert_eq!(address["lon_e7"], 120_000_000);
    assert!(address["distance_m"].as_f64().unwrap() < 0.01);
    assert_eq!(near["street"]["name"], "Example Road");
    assert!(near["interpolation"].is_null());
    assert_eq!(near["admin"], serde_json::json!([]));

    let far = &answers[1];
    for class in ["address", "street", "interpolation"] {
        assert!(far[class].is_null(), "{class}: {far}");
    }
}

/// SIGINT and SIGTERM stop the run at a query: the interrupted exit status,
/// nothing on stdout and nothing left in the current directory.
#[cfg(pbfhogg_test_hooks)]
#[test]
fn an_interrupted_run_prints_no_answer() {
    let dir = pbfhogg_test_scratch::scratch();
    let index = built_index(dir.path());
    let points = dir.path().join("points.txt");
    std::fs::write(&points, "55.0 12.0\n55.0 12.0\n").unwrap();
    for (signal, code) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
        let cwd = pbfhogg_test_scratch::scratch();
        let scratch = pbfhogg_test_scratch::scratch();
        let mut child = CliInvoker::new(&scratch)
            .arg("geocode-query")
            .arg(&index)
            .arg(&points)
            .current_dir(cwd.path())
            .cancel_pause(std::num::NonZeroU64::new(2).unwrap(), false)
            .spawn();
        child
            .wait_announcement(&crate::invoker::Announcement::Paused(2))
            .unwrap();
        child.signal(signal);
        let out = child.wait();
        assert_eq!(out.status.code(), Some(code), "{}", out.stderr_str());
        assert!(out.stdout_str().is_empty(), "{}", out.stdout_str());
        assert_eq!(std::fs::read_dir(cwd.path()).unwrap().count(), 0);
    }
}

#[test]
fn a_malformed_points_file_is_refused_before_any_answer() {
    let dir = pbfhogg_test_scratch::scratch();
    let index = built_index(dir.path());
    for (case, text) in ["55.0 12.0\n55.0\n", "55.0 12.0\n91 0\n", "55.0 12.0\n\n"]
        .iter()
        .enumerate()
    {
        let points = dir.path().join(format!("points-{case}.txt"));
        std::fs::write(&points, text).unwrap();
        let scratch = pbfhogg_test_scratch::scratch();
        let out = CliInvoker::new(&scratch)
            .arg("geocode-query")
            .arg(&index)
            .arg(&points)
            .run();
        assert_eq!(out.status.code(), Some(2), "{text:?}: {}", out.stderr_str());
        assert!(
            out.stdout_str().is_empty(),
            "{text:?}: {}",
            out.stdout_str()
        );
        assert!(
            out.stderr_str().contains("points line 2"),
            "{}",
            out.stderr_str()
        );
    }
}
