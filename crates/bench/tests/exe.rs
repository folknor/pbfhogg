//! The instruments against the contracts their scripts rely on: the
//! key=value block each prints, the exit status of a failed or interrupted
//! run, and the profiler's report, including on a run a SIGTERM stopped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

const EXE: &str = env!("CARGO_BIN_EXE_pbfhogg-bench");

struct Ran {
    status: ExitStatus,
    stderr: String,
}

impl Ran {
    /// The key=value lines of stderr, each key once.
    fn keys(&self) -> BTreeMap<String, String> {
        let mut keys = BTreeMap::new();
        for line in self.stderr.lines() {
            if let Some((key, value)) = line.split_once('=')
                && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            {
                assert!(
                    keys.insert(key.to_owned(), value.to_owned()).is_none(),
                    "{key} twice: {}",
                    self.stderr
                );
            }
        }
        keys
    }
}

/// The executable, isolated in `owner`'s scratch, its stdout and stderr in
/// files there, under the promise guard.
#[allow(clippy::disallowed_methods)] // capture files in test scratch
fn spawn<'a>(
    args: &[&std::ffi::OsStr],
    env: &[(&str, &str)],
    owner: &'a pbfhogg_test_scratch::TestScratch,
    name: &str,
) -> (crate::common::process::OwnedChild<'a>, PathBuf) {
    let mut cmd = Command::new(EXE);
    crate::common::process::isolate(&mut cmd, owner);
    cmd.args(args).envs(env.iter().copied());
    let err = owner.path().join(format!("{name}.stderr"));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err).expect("stderr file"));
    crate::guard::apply(&mut cmd);
    let child = crate::common::process::OwnedChild::spawn(&mut cmd, owner).expect("spawn");
    (child, err)
}

#[allow(clippy::disallowed_methods)] // capture files in test scratch
fn finish(mut child: crate::common::process::OwnedChild<'_>, err: &Path) -> Ran {
    let status = child
        .wait(pbfhogg::TEST_CHILD_TIMEOUT)
        .expect("wait for the instrument")
        .status;
    assert!(
        !crate::guard::killed_by_guard(status),
        "the instrument broke the promise guard"
    );
    Ran {
        status,
        stderr: String::from_utf8_lossy(&std::fs::read(err).expect("stderr")).into_owned(),
    }
}

fn run(
    args: &[&std::ffi::OsStr],
    env: &[(&str, &str)],
    owner: &pbfhogg_test_scratch::TestScratch,
    name: &str,
) -> Ran {
    let (child, err) = spawn(args, env, owner, name);
    finish(child, &err)
}

fn os(s: &str) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(s)
}

/// A base PBF and an OSC that modifies node 1 and adds node 10000.
#[allow(clippy::disallowed_methods)] // fixtures in test scratch
fn merge_inputs(scratch: &pbfhogg_test_scratch::TestScratch) -> (PathBuf, PathBuf) {
    let base = scratch.path().join("base.osm.pbf");
    crate::common::write_base_pbf(&base);
    let osc = scratch.path().join("change.osc");
    std::fs::write(
        &osc,
        r#"<osmChange version="0.6"><modify><node id="1" version="2" lat="1.0" lon="2.0"/></modify><create><node id="10000" version="1" lat="3.0" lon="4.0"/></create></osmChange>"#,
    )
    .expect("osc");
    (base, osc)
}

/// Every codec and the buffered backend: the full key set, the element
/// counts, and an output that is a PBF holding the change.
#[test]
fn merge_reports_its_block_and_writes_the_output() {
    let scratch = crate::common::test_scratch();
    let (base, osc) = merge_inputs(&scratch);
    for codec in ["none", "zlib", "zstd:3"] {
        let out = scratch.path().join(format!("out-{codec}.osm.pbf"));
        let ran = run(
            &[
                os("merge"),
                base.as_os_str(),
                osc.as_os_str(),
                os("-o"),
                out.as_os_str(),
                os("--compression"),
                os(codec),
                os("--io-mode"),
                os("buffered"),
            ],
            &[],
            &scratch,
            codec,
        );
        assert!(ran.status.success(), "{codec}: {}", ran.stderr);
        let keys = ran.keys();
        for key in [
            "elapsed_ms",
            "base_nodes",
            "base_ways",
            "base_relations",
            "diff_nodes",
            "diff_ways",
            "diff_relations",
            "blobs_passthrough",
            "blobs_rewritten",
            "bytes_passthrough",
            "bytes_rewritten",
            "diff_heap_bytes",
            "output_mb",
        ] {
            assert!(keys.contains_key(key), "{codec}: no {key}: {}", ran.stderr);
        }
        assert_eq!(keys["diff_nodes"], "2", "{}", ran.stderr);
        assert_eq!(keys["base_ways"], "40", "{}", ran.stderr);
        let mut nodes = 0;
        pbfhogg::ElementReader::from_path(&out)
            .expect("the output")
            .for_each(|el| {
                if matches!(
                    el,
                    pbfhogg::Element::DenseNode(_) | pbfhogg::Element::Node(_)
                ) {
                    nodes += 1;
                }
            })
            .expect("read the output");
        assert_eq!(nodes, 301, "{codec}");
    }
}

/// An existing output is refused without -O, replaced with it, and a
/// missing base fails: each a nonzero exit with an Error: line, never a
/// block a script would read as a measurement.
#[test]
#[allow(clippy::disallowed_methods)] // fixtures in test scratch
fn merge_failures_exit_nonzero_without_a_block() {
    let scratch = crate::common::test_scratch();
    let (base, osc) = merge_inputs(&scratch);
    let out = scratch.path().join("out.osm.pbf");
    std::fs::write(&out, b"existing").expect("existing output");
    let args = |extra: &[&'static str]| {
        let mut args = vec![
            os("merge"),
            base.as_os_str(),
            osc.as_os_str(),
            os("-o"),
            out.as_os_str(),
        ];
        args.extend(extra.iter().map(|a| os(a)));
        args
    };
    let refused = run(&args(&[]), &[], &scratch, "refused");
    assert_eq!(refused.status.code(), Some(1), "{}", refused.stderr);
    assert!(refused.stderr.contains("Error: "), "{}", refused.stderr);
    assert!(
        !refused.keys().contains_key("elapsed_ms"),
        "{}",
        refused.stderr
    );
    let replaced = run(&args(&["-O"]), &[], &scratch, "replaced");
    assert!(replaced.status.success(), "{}", replaced.stderr);
    let missing = scratch.path().join("missing.osm.pbf");
    let failed = run(
        &[
            os("merge"),
            missing.as_os_str(),
            osc.as_os_str(),
            os("-o"),
            scratch.path().join("x.osm.pbf").as_os_str(),
        ],
        &[],
        &scratch,
        "missing",
    );
    assert_eq!(failed.status.code(), Some(1), "{}", failed.stderr);
    assert!(
        !failed.keys().contains_key("elapsed_ms"),
        "{}",
        failed.stderr
    );
    let usage = run(&[os("merge")], &[], &scratch, "usage");
    assert_eq!(usage.status.code(), Some(2), "{}", usage.stderr);
}

/// geocode-build makes an index, and geocode-query over it prints the full
/// block in the shape the script checks, the same answers on a second run.
#[test]
fn geocode_query_reports_its_block_over_a_built_index() {
    let scratch = crate::common::test_scratch();
    let index = build_index(&scratch);
    let query = |name: &str| {
        run(
            &[
                os("geocode-query"),
                index.as_os_str(),
                os("--bbox"),
                os("12.4,55.6,12.6,55.8"),
                os("--queries"),
                os("200"),
                os("--rounds"),
                os("2"),
                os("--seed"),
                os("7"),
            ],
            &[],
            &scratch,
            name,
        )
    };
    let first = query("first");
    assert!(first.status.success(), "{}", first.stderr);
    let keys = first.keys();
    for key in [
        "elapsed_ms",
        "open_ms",
        "queries",
        "rounds",
        "us_per_query_best",
        "us_per_query_rounds",
        "admin_answered",
        "admin_matches",
        "addresses",
        "streets",
        "interpolations",
        "result_format",
        "answer_digest",
    ] {
        assert!(keys.contains_key(key), "no {key}: {}", first.stderr);
    }
    assert_eq!(keys["queries"], "200");
    assert_eq!(keys["rounds"], "2");
    assert_eq!(keys["us_per_query_rounds"].split(',').count(), 2);
    assert_eq!(keys["result_format"], "reverse-result-v1");
    assert_eq!(keys["answer_digest"].len(), 16);
    let second = query("second");
    assert_eq!(second.keys()["answer_digest"], keys["answer_digest"]);
}

/// A built index, from the library's minimal geocode input.
fn build_index(scratch: &pbfhogg_test_scratch::TestScratch) -> PathBuf {
    let input = scratch.path().join("geo.osm.pbf");
    crate::common::write_synthetic_indexed_input(&input);
    let index = scratch.path().join("index");
    let ran = run(
        &[
            os("geocode-build"),
            input.as_os_str(),
            os("--output-dir"),
            index.as_os_str(),
            os("--tmp-dir"),
            scratch.path().as_os_str(),
        ],
        &[],
        scratch,
        "build",
    );
    assert!(ran.status.success(), "{}", ran.stderr);
    index
}

/// A SIGTERM cancels the run instead of killing it: the instrument stops at
/// a round boundary, exits 143 with an Error: line and no measurement, and,
/// in an instrumented build asked for a report, writes that report on the
/// way out. This is the path brokkr's marker stop takes on a profiled call.
///
/// The signal is sent once the first marker reaches the FIFO: the
/// instrument blocks the signals before it does any work, so by then a
/// SIGTERM reaches its waiter, not the default action.
#[test]
#[allow(clippy::disallowed_methods)] // a FIFO in test scratch
fn a_sigterm_cancels_the_run_and_keeps_the_profile() {
    use std::os::fd::AsFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let scratch = crate::common::test_scratch();
    let index = build_index(&scratch);
    let fifo = crate::common::make_fifo(scratch.path(), "markers.fifo");
    // Held open until the child has exited, so its writes never meet a
    // closed FIFO.
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .expect("the FIFO's read end");
    let report = scratch.path().join("report.json");
    let report_text = report.display().to_string();
    let fifo_text = fifo.display().to_string();
    let env = [
        ("BROKKR_MARKER_FIFO", fifo_text.as_str()),
        ("HOTPATH_OUTPUT_PATH", report_text.as_str()),
        ("HOTPATH_OUTPUT_FORMAT", "json"),
        ("HOTPATH_METRICS_SERVER_OFF", "true"),
    ];
    // Enough rounds that the signal, not completion, ends the run, and few
    // enough that a run the signal failed to stop still ends inside the
    // test's bound.
    let (child, err) = spawn(
        &[
            os("geocode-query"),
            index.as_os_str(),
            os("--bbox"),
            os("12.4,55.6,12.6,55.8"),
            os("--queries"),
            os("100000"),
            os("--rounds"),
            os("200"),
        ],
        &env,
        &scratch,
        "signalled",
    );
    let readiness = child
        .wait_readable(reader.as_fd(), Instant::now() + pbfhogg::TEST_CHILD_TIMEOUT)
        .expect("wait for the first marker");
    assert!(
        matches!(readiness, crate::common::process::Readiness::Readable),
        "no marker before the child ended or the deadline"
    );
    child.signal(libc::SIGTERM).expect("SIGTERM");
    let ran = finish(child, &err);
    drop(reader);
    assert_eq!(ran.status.code(), Some(143), "{}", ran.stderr);
    assert!(ran.stderr.contains("Error: interrupted"), "{}", ran.stderr);
    assert!(!ran.keys().contains_key("elapsed_ms"), "{}", ran.stderr);
    if cfg!(feature = "hotpath") {
        let text = std::fs::read(&report).expect("the profile report");
        let doc: serde_json::Value = serde_json::from_slice(&text).expect("a JSON report");
        assert!(doc.is_object(), "{doc}");
    }
}

/// A profiled merge of an instrumented build reports library functions,
/// with calls counted and, under `hotpath-alloc`, bytes counted: the
/// counting allocator is installed, not only the guard.
#[cfg(feature = "hotpath")]
#[test]
fn a_profiled_merge_reports_library_functions() {
    let scratch = crate::common::test_scratch();
    let (base, osc) = merge_inputs(&scratch);
    let report = scratch.path().join("report.json");
    let report_text = report.display().to_string();
    let out = scratch.path().join("out.osm.pbf");
    let ran = run(
        &[
            os("merge"),
            base.as_os_str(),
            osc.as_os_str(),
            os("-o"),
            out.as_os_str(),
        ],
        &[
            ("HOTPATH_OUTPUT_PATH", report_text.as_str()),
            ("HOTPATH_OUTPUT_FORMAT", "json"),
            ("HOTPATH_METRICS_SERVER_OFF", "true"),
        ],
        &scratch,
        "profiled",
    );
    assert!(ran.status.success(), "{}", ran.stderr);
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
                .is_some_and(|n| n.starts_with("pbfhogg::"))
        })
        .collect();
    assert!(
        library
            .iter()
            .any(|entry| entry["calls"].as_u64().is_some_and(|calls| calls > 0)),
        "no library call counted in {doc}"
    );
    if cfg!(feature = "hotpath-alloc") {
        assert!(
            library.iter().any(|entry| entry["total"]
                .as_str()
                .is_some_and(|total| !total.starts_with("0 "))),
            "no library allocation counted in {doc}"
        );
    }
}
