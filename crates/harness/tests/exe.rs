//! The harness executable against brokkr's library protocol: describe mode,
//! a passing run of every scenario over a generated input, the identity of
//! `repeat-cat`'s plan and contexts, and quiet refusals.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

const EXE: &str = env!("CARGO_BIN_EXE_pbfhogg-harness");

/// What a child left behind.
struct Ran {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run `cmd` under the promise guard with nothing inherited of the
/// protocol's or the profiler's variables but what `env` sets.
#[allow(clippy::disallowed_methods)] // capture files in test scratch
fn run(
    mut cmd: Command,
    env: &[(&str, &std::ffi::OsStr)],
    scratch: &Path,
    owner: &pbfhogg_test_scratch::TestScratch,
) -> Ran {
    let cwd = cmd.get_current_dir().map(Path::to_path_buf);
    crate::common::process::isolate(&mut cmd, owner);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.envs(env.iter().copied());
    let out = scratch.join("stdout");
    let err = scratch.join("stderr");
    cmd.stdin(Stdio::null())
        .stdout(std::fs::File::create(&out).expect("stdout file"))
        .stderr(std::fs::File::create(&err).expect("stderr file"));
    crate::guard::apply(&mut cmd);
    let mut child =
        crate::common::process::OwnedChild::spawn(&mut cmd, owner).expect("spawn the harness");
    let status = child
        .wait(pbfhogg::TEST_CHILD_TIMEOUT)
        .expect("wait for the harness")
        .status;
    assert!(
        !crate::guard::killed_by_guard(status),
        "the harness broke the promise guard"
    );
    Ran {
        status,
        stdout: std::fs::read(&out).expect("stdout"),
        stderr: std::fs::read(&err).expect("stderr"),
    }
}

#[test]
fn describe_prints_one_valid_document_and_nothing_else() {
    let scratch = crate::common::test_scratch();
    let mut cmd = Command::new(EXE);
    cmd.arg("--describe-harness").current_dir(scratch.path());
    let ran = run(cmd, &[], scratch.path(), &scratch);
    assert!(ran.status.success(), "{}", ran.status);
    assert!(
        ran.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    let doc: Value = serde_json::from_slice(&ran.stdout).expect("one JSON document");
    assert_eq!(doc["protocol"], 1);
    let scenarios = doc["scenarios"].as_array().expect("scenarios");
    assert!(scenarios.iter().any(|s| s["id"] == "repeat-cat"));
    // It wrote nothing but its two capture files.
    let entries = std::fs::read_dir(scratch.path()).expect("list").count();
    assert_eq!(entries, 2);
}

#[path = "../../lib/tests/common/fifo_capture.rs"]
mod fifo_capture;
use fifo_capture::FifoCapture as Fifo;

/// A run directory laid out as brokkr lays it out.
struct RunDir {
    _scratch: pbfhogg_test_scratch::TestScratch,
    root: PathBuf,
}

impl RunDir {
    #[allow(clippy::disallowed_methods)] // fixtures in test scratch
    fn new(request: impl FnOnce(&Path) -> Vec<u8>) -> Self {
        let scratch = crate::common::test_scratch();
        let root = scratch.path().canonicalize().expect("canonical scratch");
        for dir in ["cwd", "artefacts", "roles", "private", "capture"] {
            std::fs::create_dir(root.join(dir)).expect("layout");
        }
        let request = request(&root.join("roles"));
        std::fs::write(root.join("request.json"), request).expect("request");
        Self {
            _scratch: scratch,
            root,
        }
    }

    fn launch(&self, fifo: &Path, env: &[(&str, &str)]) -> Ran {
        let mut cmd = Command::new(EXE);
        cmd.arg("--test-harness")
            .arg(self.root.join("request.json"))
            .arg("--private-dir")
            .arg(self.root.join("private"))
            .current_dir(self.root.join("cwd"));
        let artefacts = self.root.join("artefacts");
        let mut vars = vec![
            ("BROKKR_HARNESS_ARTEFACT_DIR", artefacts.as_os_str()),
            ("BROKKR_MARKER_FIFO", fifo.as_os_str()),
        ];
        vars.extend(
            env.iter()
                .map(|(key, value)| (*key, std::ffi::OsStr::new(value))),
        );
        run(cmd, &vars, &self.root.join("capture"), &self._scratch)
    }

    fn doc(&self, relative: &str) -> Value {
        let text = std::fs::read(self.root.join("artefacts").join(relative)).expect(relative);
        serde_json::from_slice(&text).expect("a JSON document")
    }

    fn entries(&self, relative: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.root.join(relative))
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn request(scenario: &str, iteration: u32, bindings: &serde_json::Map<String, Value>) -> Value {
    serde_json::json!({
        "protocol": 1, "scenario": scenario, "iteration": iteration, "bindings": bindings
    })
}

fn repeat_request(input: &Path, roles: &Path, iteration: u32) -> Value {
    request(
        "repeat-cat",
        iteration,
        &serde_json::Map::from_iter([
            ("input".into(), serde_json::json!(input)),
            ("out-a".into(), serde_json::json!(roles.join("out-a"))),
            ("out-b".into(), serde_json::json!(roles.join("out-b"))),
        ]),
    )
}

/// A sorted, indexed input in its own scratch.
fn input() -> (pbfhogg_test_scratch::TestScratch, PathBuf) {
    let scratch = crate::common::test_scratch();
    let path = scratch
        .path()
        .canonicalize()
        .expect("canonical")
        .join("input.osm.pbf");
    crate::common::write_base_pbf(&path);
    (scratch, path)
}

/// One passing `repeat-cat` launch; returns its run directory and FIFO
/// lines.
fn repeat_cat(input: &Path, iteration: u32, rayon: &str) -> (RunDir, String) {
    let run_dir = RunDir::new(|roles| {
        serde_json::to_vec(&repeat_request(input, roles, iteration)).expect("request JSON")
    });
    let fifo = Fifo::new(&run_dir.root.join("capture"));
    let ran = run_dir.launch(&fifo.path.clone(), &[("RAYON_NUM_THREADS", rayon)]);
    let lines = fifo.finish();
    assert!(
        ran.stdout.is_empty() && ran.stderr.is_empty(),
        "stdout {:?} stderr {:?}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(ran.status.code(), Some(0), "{}", ran.status);
    (run_dir, lines)
}

#[test]
fn repeat_cat_runs_to_the_protocol() {
    let (_keep, input) = input();
    let (run, lines) = repeat_cat(&input, 0, "2");
    assert_eq!(run.entries("artefacts"), ["calls", "scenario.json"]);
    assert_eq!(run.entries("artefacts/calls"), ["a.json", "b.json"]);
    assert!(run.entries("cwd").is_empty());
    assert_eq!(run.entries("roles"), ["out-a", "out-b"]);
    let a = std::fs::read(run.root.join("roles/out-a")).expect("out-a");
    assert_eq!(
        a,
        std::fs::read(run.root.join("roles/out-b")).expect("out-b")
    );
    for (call, role) in [("a", "out-a"), ("b", "out-b")] {
        let doc = run.doc(&format!("calls/{call}.json"));
        assert_eq!(doc["call"], call);
        assert_eq!(doc["outcome"]["status"], "ok");
        assert_eq!(doc["seal"]["state"], "sealed", "{doc}");
        assert_eq!(doc["stream"]["state"], "sealed", "{doc}");
        assert!(doc["cancel"].is_null());
        // Every counter of the snapshot has its sealing emission, carrying
        // its value, and the terminal record agrees with the account.
        let snapshot = doc["snapshot"]
            .as_object()
            .expect("a sealed call's snapshot");
        assert!(!snapshot.is_empty());
        let drops = doc["stream"]["drops"].as_u64().expect("drops");
        if drops == 0 {
            for (name, value) in snapshot {
                assert!(
                    lines.contains(&format!(" %{call} @@{name}={value}\n")),
                    "no sealing emission for {call} {name}"
                );
            }
            assert!(
                lines.contains(&format!(" %{call} !sealed drops=0\n")),
                "no terminal record for {call}"
            );
        }
        let (entry, ret, seal) = (&doc["entry_us"], &doc["return_us"], &doc["seal_us"]);
        let (entry, ret, seal) = (entry.as_u64(), ret.as_u64(), seal.as_u64());
        assert!(entry <= ret && ret <= seal && entry.is_some());
        assert_eq!(doc["publications"][0]["decision"], "authorized");
        assert_eq!(doc["publications"][0]["members"][0]["role"], role);
        assert_eq!(doc["publications"][0]["members"][0]["effect"], "installed");
        assert_eq!(doc["assertions"][0]["status"], "pass", "{doc}");
        assert!(
            lines.contains(&format!(" %{call} ")),
            "no tagged line for {call}"
        );
    }
    let scenario = run.doc("scenario.json");
    let results = scenario["assertions"].as_array().expect("results");
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r["status"] == "pass"), "{scenario}");
}

/// Two processes of one workload agree on the plan and every context,
/// whatever their iteration; a different ambient pool size is a different
/// context.
#[test]
fn plan_and_contexts_identify_the_workload() {
    let (_keep, input) = input();
    let identity = |run: &RunDir| {
        (
            run.doc("scenario.json")["plan"].clone(),
            run.doc("calls/a.json")["context"].clone(),
            run.doc("calls/b.json")["context"].clone(),
        )
    };
    let first = identity(&repeat_cat(&input, 0, "2").0);
    let second = identity(&repeat_cat(&input, 1, "2").0);
    assert_eq!(first, second);
    let narrower = identity(&repeat_cat(&input, 0, "1").0);
    assert_eq!(first.0, narrower.0);
    assert_eq!(narrower.1["executor_threads"], 1);
    assert_ne!(first.1, narrower.1);
}

/// One launch of `scenario`, its dataset role `input` (when `dataset`)
/// bound to a generated input, every other role under `roles/` (the `seeded` ones
/// starting as their seed, as brokkr lays them out). Asserts a quiet exit
/// 0 and every result passing; returns the run directory and FIFO lines.
#[allow(clippy::disallowed_methods)] // seeds in test scratch
fn scenario_passes(
    scenario: &str,
    dataset: bool,
    roles: &[&str],
    seeded: &[&str],
) -> (RunDir, String) {
    let datasets: &[&str] = if dataset { &["input"] } else { &[] };
    scenario_passes_on(scenario, datasets, roles, seeded)
}

/// [`scenario_passes`] with every role in `datasets` bound: `raw` to the
/// generated input's blocks without indexdata, any other to the input.
fn scenario_passes_on(
    scenario: &str,
    datasets: &[&str],
    roles: &[&str],
    seeded: &[&str],
) -> (RunDir, String) {
    scenario_passes_with(scenario, datasets, roles, seeded, &[])
}

/// [`scenario_passes_on`] with `env` added to the launch.
#[allow(clippy::disallowed_methods)] // seeds in test scratch
fn scenario_passes_with(
    scenario: &str,
    datasets: &[&str],
    roles: &[&str],
    seeded: &[&str],
    env: &[(&str, &str)],
) -> (RunDir, String) {
    let keep = crate::common::test_scratch();
    let mut bindings = serde_json::Map::new();
    for role in datasets {
        let path = keep.path().join(format!("{role}.osm.pbf"));
        if *role == "raw" {
            crate::common::write_base_pbf_non_indexed(&path);
        } else {
            crate::common::write_base_pbf(&path);
        }
        bindings.insert((*role).into(), serde_json::json!(path));
    }
    let run_dir = RunDir::new(|dir| {
        for role in roles.iter().chain(seeded) {
            bindings.insert((*role).into(), serde_json::json!(dir.join(role)));
        }
        serde_json::to_vec(&request(scenario, 0, &bindings)).expect("request JSON")
    });
    for role in seeded {
        std::fs::write(
            run_dir.root.join("roles").join(role),
            format!("brokkr-library-seed-v1 {role}\n"),
        )
        .expect("seed");
    }
    let fifo = Fifo::new(&run_dir.root.join("capture"));
    let ran = run_dir.launch(&fifo.path.clone(), env);
    let lines = fifo.finish();
    assert!(
        ran.stdout.is_empty() && ran.stderr.is_empty(),
        "stdout {:?} stderr {:?}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    let declaration = describe_scenario(scenario);
    let calls: Vec<_> = run_dir
        .entries("artefacts/calls")
        .into_iter()
        .map(|file| {
            let doc = run_dir.doc(&format!("calls/{file}"));
            (file, doc)
        })
        .collect();
    validate_inventory(&declaration, &run_dir.doc("scenario.json"), &calls)
        .unwrap_or_else(|error| panic!("{scenario}: {error}"));
    assert_eq!(ran.status.code(), Some(0), "{}", ran.status);
    (run_dir, lines)
}

#[test]
fn concurrent_cat_tags_each_calls_counters() {
    let (run, lines) = scenario_passes(
        "concurrent-cat",
        false,
        &["input-small", "input-large", "out-a", "out-b"],
        &[],
    );
    // The FIFO's tags carry each call's own workload: one block and three.
    for (call, blocks) in [("a", 1), ("b", 3)] {
        assert!(
            lines.contains(&format!(" %{call} @cat_blobs_passthrough={blocks}\n")),
            "call {call}: {lines}"
        );
        let doc = run.doc(&format!("calls/{call}.json"));
        assert_eq!(doc["context"]["fd_pool_budget"], 18);
        // The sealed snapshot holds the call's own raw payloads.
        assert_eq!(doc["snapshot"]["writer_payload_raw_items"], blocks);
    }
}

#[test]
fn page_cache_keep_counts_no_input_eviction() {
    let (run, _) = scenario_passes("page-cache-keep", false, &["input"], &[]);
    for (call, policy, evictions) in [("keep", "keep", 0), ("evict", "evict", 4)] {
        let doc = run.doc(&format!("calls/{call}.json"));
        assert_eq!(doc["context"]["page_cache"], policy);
        assert_eq!(doc["snapshot"]["fadvise_input_evictions"], evictions);
    }
}

#[test]
fn fixed_resources_executor_frames_on_the_callers_pool_only() {
    let (run, _) = scenario_passes_on(
        "fixed-resources-executor",
        &["input", "raw"],
        &["out", "framed"],
        &[],
    );
    for call in ["a", "b"] {
        let doc = run.doc(&format!("calls/{call}.json"));
        assert_eq!(doc["context"]["executor"], "pool");
        assert_eq!(doc["context"]["executor_threads"], 4);
    }
    // Call b framed through the writer, every payload on the caller's pool.
    let snapshot = &run.doc("calls/b.json")["snapshot"];
    let framed = snapshot["writer_payload_framed_items"]
        .as_u64()
        .expect("framed");
    assert!(framed > 0, "{snapshot}");
    assert_eq!(snapshot["writer_payload_framed_on_executor_items"], framed);
}

#[test]
fn cancel_pre_decision_refuses_the_output() {
    let (run, _) = scenario_passes("cancel-pre-decision", true, &["out"], &[]);
    let doc = run.doc("calls/a.json");
    assert_eq!(doc["cancel"]["result"], "before_decision");
    assert_eq!(doc["outcome"]["status"], "cancelled");
    assert!(run.entries("roles").is_empty());
}

#[test]
fn cancel_post_decision_lets_the_publication_stand() {
    let (run, _) = scenario_passes("cancel-post-decision", true, &["out"], &[]);
    let doc = run.doc("calls/a.json");
    assert_eq!(doc["cancel"]["result"], "after_decision");
    assert_eq!(doc["outcome"]["status"], "ok");
}

#[test]
fn nofile_cat_is_refused_with_a_1024_grant() {
    let (run, _) = scenario_passes("nofile-cat-refused", false, &["out"], &[]);
    let doc = run.doc("calls/a.json");
    assert_eq!(doc["context"]["process_actions"], "fd_pool");
    assert_eq!(doc["context"]["fd_pool_budget"], 1024);
    assert_eq!(doc["outcome"]["kind"], "invalid_argument");
}

#[test]
fn nofile_multi_extract_is_refused_with_few_descriptors_free() {
    let (run, _) = scenario_passes(
        "nofile-multi-extract-refused",
        false,
        &["out-a", "out-b"],
        &[],
    );
    let doc = run.doc("calls/a.json");
    assert_eq!(doc["context"]["process_actions"], "fd_pool");
    assert_eq!(doc["context"]["fd_pool_budget"], 6);
    assert_eq!(doc["outcome"]["kind"], "invalid_argument");
}

#[test]
fn nofile_altw_external_runs_three_workers() {
    let (run, _) = scenario_passes("nofile-altw-external", true, &["out"], &[]);
    let context = &run.doc("calls/a.json")["context"];
    assert_eq!(context["parallelism"], 8);
    assert_eq!(context["process_actions"], "fd_pool");
    assert_eq!(context["fd_pool_budget"], 896);
    assert_eq!(context["executor"], "pool");
    assert_eq!(context["executor_threads"], 8);
}

#[test]
fn error_path_reports_the_missing_header() {
    let (run, _) = scenario_passes("error-path", false, &["bad-input", "out"], &[]);
    assert_eq!(run.doc("calls/a.json")["outcome"]["kind"], "missing_header");
    assert_eq!(run.entries("roles"), ["bad-input"]);
}

#[test]
fn seeded_replace_replaces_one_seed_and_keeps_the_other() {
    let (run, _) = scenario_passes("seeded-replace", true, &[], &["out-replace", "out-refuse"]);
    let kept = std::fs::read(run.root.join("roles/out-refuse")).expect("out-refuse");
    assert_eq!(kept, b"brokkr-library-seed-v1 out-refuse\n");
    assert_eq!(
        run.doc("calls/refuse.json")["publications"],
        serde_json::json!([])
    );
}

#[test]
fn isolation_cat_runs_one_call() {
    let (run, _) = scenario_passes("isolation-cat", true, &["out"], &[]);
    assert_eq!(run.entries("artefacts/calls"), ["a.json"]);
}

/// The profiler variables brokkr sets on a profiled launch, writing the
/// report to `report`.
fn profiling_env(report: &Path) -> [(&'static str, String); 3] {
    [
        ("HOTPATH_OUTPUT_PATH", report.display().to_string()),
        ("HOTPATH_OUTPUT_FORMAT", "json".to_owned()),
        ("HOTPATH_METRICS_SERVER_OFF", "true".to_owned()),
    ]
}

/// Describe mode never profiles, even when asked: one document, nothing on
/// stderr, no report. Every build runs it; the instrumented sweeps make it
/// mean something.
#[test]
fn describe_ignores_a_profiling_request() {
    let scratch = crate::common::test_scratch();
    let report = scratch.path().join("report.json");
    let vars = profiling_env(&report);
    let env: Vec<(&str, &std::ffi::OsStr)> = vars
        .iter()
        .map(|(key, value)| (*key, std::ffi::OsStr::new(value)))
        .collect();
    let mut cmd = Command::new(EXE);
    cmd.arg("--describe-harness").current_dir(scratch.path());
    let ran = run(cmd, &env, scratch.path(), &scratch);
    assert!(ran.status.success(), "{}", ran.status);
    assert!(
        ran.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    let _: Value = serde_json::from_slice(&ran.stdout).expect("one JSON document");
    assert!(!report.exists(), "describe mode wrote a profile");
}

/// A profiled launch of an instrumented build: the run passes quietly and
/// the report names library functions the scenario reached, with calls
/// counted and, under `hotpath-alloc`, bytes counted - so the counting
/// allocator is installed, not just the guard. Unprofiled quiet runs of the
/// same build are every other scenario test in this file.
#[cfg(feature = "hotpath")]
#[test]
fn a_profiled_launch_reports_library_functions() {
    let keep = crate::common::test_scratch();
    let report = keep.path().join("report.json");
    let vars = profiling_env(&report);
    let env: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (_run, _) = scenario_passes_with("isolation-cat", &["input"], &["out"], &[], &env);
    let text = std::fs::read(&report).expect("the profile report");
    let doc: Value = serde_json::from_slice(&text).expect("a JSON report");
    let section = if cfg!(feature = "hotpath-alloc") {
        "functions_alloc"
    } else {
        "functions_timing"
    };
    let data = doc[section]["data"]
        .as_array()
        .unwrap_or_else(|| panic!("no {section}.data in {doc}"));
    let library: Vec<&Value> = data
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
        // The report formats byte totals for humans ("831.4 KB"); a
        // missing counting allocator shows as "0 B" on every function.
        assert_eq!(doc[section]["profiling_mode"], "alloc-bytes", "{doc}");
        assert!(
            library.iter().any(|entry| entry["total"]
                .as_str()
                .is_some_and(|total| !total.starts_with("0 "))),
            "no library allocation counted in {doc}"
        );
    }
}

/// A request the harness cannot honour exits 2 without a byte of output,
/// and says why in refusal.json.
#[test]
fn bad_requests_are_refused_quietly() {
    let (_keep, input) = input();
    // The same input and live FIFO must support a valid launch first.
    let _control = repeat_cat(&input, 0, "2");
    // Each case changes one property of the passing request. In particular,
    // range checks have separate lower and upper probes, and each required
    // field and field type is exercised without another invalid field.
    for (case, expected) in [
        ("unknown scenario", "no scenario"),
        ("missing binding", "bindings name"),
        ("extra binding", "bindings name"),
        ("duplicate key", "duplicate key"),
        ("duplicate binding", "duplicate key"),
        ("negative zero", "not a plain integer"),
        ("leading zero", "not a plain integer"),
        ("fraction", "a number with a fraction or exponent"),
        ("exponent", "a number with a fraction or exponent"),
        ("trailing bytes", "trailing bytes"),
        ("protocol version", "request.protocol:"),
        ("negative iteration", "request.iteration: out of range"),
        ("large iteration", "request.iteration: out of range"),
        ("missing protocol", "lacks a required key"),
        ("missing scenario", "lacks a required key"),
        ("missing iteration", "lacks a required key"),
        ("missing bindings", "lacks a required key"),
        (
            "protocol type",
            "request.protocol: unknown key or wrong type",
        ),
        (
            "scenario type",
            "request.scenario: unknown key or wrong type",
        ),
        (
            "iteration type",
            "request.iteration: unknown key or wrong type",
        ),
        (
            "bindings type",
            "request.bindings: unknown key or wrong type",
        ),
        ("unknown field", "request.extra: unknown key or wrong type"),
        ("binding type", "request.bindings.out-b: not a string"),
        ("relative binding", "request.bindings.out-b: not absolute"),
        ("non-object", "the request is not an object"),
    ] {
        let run_dir = RunDir::new(|roles| {
            let mut value = repeat_request(&input, roles, 0);
            match case {
                "unknown scenario" => value["scenario"] = serde_json::json!("no-such"),
                "missing binding" => {
                    value["bindings"]
                        .as_object_mut()
                        .expect("bindings")
                        .remove("out-b");
                }
                "extra binding" => {
                    value["bindings"]["extra"] = serde_json::json!(roles.join("extra"));
                }
                "protocol version" => value["protocol"] = serde_json::json!(2),
                "negative iteration" => value["iteration"] = serde_json::json!(-1),
                "large iteration" => value["iteration"] = serde_json::json!(u64::MAX),
                "missing protocol" | "missing scenario" | "missing iteration"
                | "missing bindings" => {
                    let key = case.strip_prefix("missing ").expect("missing field case");
                    value.as_object_mut().expect("request").remove(key);
                }
                "protocol type" | "scenario type" | "iteration type" | "bindings type" => {
                    let key = case.strip_suffix(" type").expect("field type case");
                    value[key] = serde_json::Value::Null;
                }
                "unknown field" => value["extra"] = serde_json::json!(0),
                "binding type" => value["bindings"]["out-b"] = serde_json::json!(0),
                "relative binding" => value["bindings"]["out-b"] = serde_json::json!("out-b"),
                "non-object" => value = serde_json::json!([]),
                _ => {}
            }
            let text = serde_json::to_string(&value).expect("request JSON");
            // Text edits preserve spellings that serde_json would normalize.
            let text = match case {
                "duplicate key" => {
                    text.replacen("\"iteration\":0", "\"iteration\":0,\"iteration\":0", 1)
                }
                "duplicate binding" => {
                    let binding =
                        serde_json::to_string(&value["bindings"]["out-b"]).expect("path JSON");
                    text.replacen(
                        "\"bindings\":{",
                        &format!("\"bindings\":{{\"out-b\":{binding},"),
                        1,
                    )
                }
                "negative zero" => text.replacen("\"iteration\":0", "\"iteration\":-0", 1),
                "leading zero" => text.replacen("\"iteration\":0", "\"iteration\":00", 1),
                "fraction" => text.replacen("\"iteration\":0", "\"iteration\":0.5", 1),
                "exponent" => text.replacen("\"iteration\":0", "\"iteration\":0e0", 1),
                "trailing bytes" => format!("{text} x"),
                _ => text,
            };
            text.into_bytes()
        });
        assert_request_refused(&run_dir, case, expected);
        assert!(run_dir.entries("roles").is_empty());
    }
}

/// Assert refusal before setup or any call starts, with the specific reason
/// so a later validation failure cannot mask a removed earlier check.
fn assert_request_refused(run_dir: &RunDir, case: &str, expected: &str) {
    let fifo = Fifo::new(&run_dir.root.join("capture"));
    let ran = run_dir.launch(&fifo.path.clone(), &[]);
    let lines = fifo.finish();
    assert_eq!(ran.status.code(), Some(2), "case {case}");
    assert!(
        ran.stdout.is_empty() && ran.stderr.is_empty(),
        "case {case}"
    );
    assert!(lines.is_empty(), "case {case} emitted markers: {lines}");
    assert_eq!(
        run_dir.entries("artefacts"),
        ["refusal.json"],
        "case {case}"
    );
    let reason = run_dir.doc("refusal.json");
    assert_eq!(reason["protocol"], 1);
    assert!(
        reason["reason"]
            .as_str()
            .expect("reason")
            .contains(expected),
        "case {case}: {reason}"
    );
    assert!(run_dir.entries("cwd").is_empty(), "case {case}");
    assert!(run_dir.entries("private").is_empty(), "case {case}");
}

#[test]
fn bad_binding_states_are_refused_before_setup() {
    let (_keep, input) = input();
    for (case, expected) in [
        ("missing input", "input input:"),
        ("directory input", "input input is not a regular file"),
        ("existing output", "role out-a is not in its initial state"),
        ("missing parent", "role out-a:"),
        ("same outputs", "name one path"),
        ("aliased parents", "name one path"),
        (
            "missing seed",
            "role out-replace is not in its initial state",
        ),
        ("wrong seed", "role out-replace is not in its initial state"),
        (
            "symlink seed",
            "role out-replace is not in its initial state",
        ),
        ("input output alias", "name one path"),
    ] {
        let run_dir = RunDir::new(|roles| {
            let mut value = repeat_request(&input, roles, 0);
            match case {
                "missing input" => {
                    value["bindings"]["input"] = serde_json::json!(roles.join("missing"));
                }
                "directory input" => value["bindings"]["input"] = serde_json::json!(roles),
                "existing output" => {
                    std::fs::write(roles.join("out-a"), b"untouched").expect("existing output");
                }
                "missing parent" => {
                    value["bindings"]["out-a"] = serde_json::json!(roles.join("missing/out"));
                }
                "same outputs" => value["bindings"]["out-b"] = value["bindings"]["out-a"].clone(),
                "aliased parents" => {
                    std::fs::create_dir(roles.join("sub")).expect("subdirectory");
                    value["bindings"]["out-b"] = serde_json::json!(roles.join("sub/../out-a"));
                }
                "missing seed" | "wrong seed" | "symlink seed" | "input output alias" => {
                    value["scenario"] = serde_json::json!("seeded-replace");
                    value["bindings"]
                        .as_object_mut()
                        .expect("bindings")
                        .remove("out-a");
                    value["bindings"]
                        .as_object_mut()
                        .expect("bindings")
                        .remove("out-b");
                    for role in ["out-replace", "out-refuse"] {
                        value["bindings"][role] = serde_json::json!(roles.join(role));
                        if role == "out-refuse" || case != "missing seed" {
                            let seed = if role == "out-refuse" || case != "wrong seed" {
                                format!("brokkr-library-seed-v1 {role}\n")
                            } else {
                                "wrong seed\n".to_owned()
                            };
                            if role == "out-replace" && case == "symlink seed" {
                                let target = roles.join("seed-target");
                                std::fs::write(&target, seed).expect("seed target");
                                std::os::unix::fs::symlink(target, roles.join(role))
                                    .expect("seed link");
                            } else {
                                std::fs::write(roles.join(role), seed).expect("seed");
                            }
                        }
                    }
                    if case == "input output alias" {
                        value["bindings"]["input"] = value["bindings"]["out-replace"].clone();
                    }
                }
                _ => unreachable!("listed binding case"),
            }
            serde_json::to_vec(&value).expect("request JSON")
        });
        assert_request_refused(&run_dir, case, expected);
        if case == "existing output" {
            assert_eq!(
                std::fs::read(run_dir.root.join("roles/out-a")).expect("output"),
                b"untouched"
            );
        }
    }
}

/// Read the executable's declaration, without coupling tests to scenario internals.
fn describe_scenario(id: &str) -> Value {
    let scratch = crate::common::test_scratch();
    let mut cmd = Command::new(EXE);
    cmd.arg("--describe-harness").current_dir(scratch.path());
    let ran = run(cmd, &[], scratch.path(), &scratch);
    assert!(ran.status.success());
    assert!(ran.stderr.is_empty());
    let doc: Value = serde_json::from_slice(&ran.stdout).expect("describe JSON");
    doc["scenarios"]
        .as_array()
        .expect("scenarios")
        .iter()
        .find(|s| s["id"] == id)
        .expect("declared scenario")
        .clone()
}

/// Every registered scenario is an executable obligation. Role declarations
/// supply its basic case; specialized tests below add stronger observations.
#[test]
fn every_declared_scenario_executes() {
    let scratch = crate::common::test_scratch();
    let mut cmd = Command::new(EXE);
    cmd.arg("--describe-harness").current_dir(scratch.path());
    let ran = run(cmd, &[], scratch.path(), &scratch);
    assert!(ran.status.success());
    assert!(ran.stderr.is_empty());
    let doc: Value = serde_json::from_slice(&ran.stdout).expect("describe JSON");
    let mut executed = std::collections::BTreeSet::new();
    for declaration in doc["scenarios"].as_array().expect("scenarios") {
        let id = declaration["id"].as_str().expect("scenario id");
        assert!(executed.insert(id), "duplicate scenario {id}");
        let mut datasets = Vec::new();
        let mut roles = Vec::new();
        let mut seeded = Vec::new();
        for role in declaration["roles"].as_array().expect("roles") {
            let name = role["id"].as_str().expect("role id");
            match role["source"].as_str().expect("role source") {
                "dataset" => {
                    let variant = role["variant"].as_str().expect("dataset variant");
                    assert!(
                        matches!((name, variant), ("raw", "raw") | (_, "indexed")),
                        "add a fixture for dataset {name} variant {variant}"
                    );
                    datasets.push(name);
                }
                "output" if role["seeded"] == true => seeded.push(name),
                "output" | "generated" => roles.push(name),
                source => panic!("unsupported role source {source}"),
            }
        }
        scenario_passes_on(id, &datasets, &roles, &seeded);
    }
    assert!(!executed.is_empty(), "empty scenario registry");
}

/// Local executable-test coverage check. The reusable consumer validator
/// belongs in brokkr; compare declarations here until it is available there.
fn validate_inventory(
    decl: &Value,
    scenario: &Value,
    calls: &[(String, Value)],
) -> Result<(), String> {
    use std::collections::BTreeSet;
    let strings = |values: &Value| -> Result<Vec<String>, String> {
        values
            .as_array()
            .ok_or_else(|| "missing inventory".to_owned())?
            .iter()
            .map(|v| {
                v["id"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "missing id".to_owned())
            })
            .collect()
    };
    let declared_calls = strings(&decl["calls"])?;
    let mut expected_files: Vec<_> = declared_calls
        .iter()
        .map(|id| format!("{id}.json"))
        .collect();
    expected_files.sort();
    let mut files: Vec<_> = calls.iter().map(|(file, _)| file.clone()).collect();
    files.sort();
    if files != expected_files {
        return Err("call inventory differs".into());
    }
    let declarations = decl["assertions"]
        .as_array()
        .ok_or("missing assertion declarations")?;
    for (call, doc) in std::iter::once((None, scenario))
        .chain(calls.iter().map(|(_, doc)| (doc["call"].as_str(), doc)))
    {
        if doc["protocol"] != 1 || (std::ptr::eq(doc, scenario) && doc["scenario"] != decl["id"]) {
            return Err("document identity differs".into());
        }
        if let Some(id) = call {
            if !declared_calls.iter().any(|c| c == id)
                || !calls
                    .iter()
                    .any(|(file, value)| file == &format!("{id}.json") && std::ptr::eq(value, doc))
            {
                return Err("call identity differs".into());
            }
        } else if !std::ptr::eq(doc, scenario) {
            return Err("missing call identity".into());
        }
        let expected: BTreeSet<_> = declarations
            .iter()
            .filter(|a| a["call"].as_str() == call)
            .map(|a| a["id"].as_str().ok_or("missing declared assertion id"))
            .collect::<Result<_, _>>()?;
        let actual = strings(&doc["assertions"])?;
        let unique: BTreeSet<_> = actual.iter().map(String::as_str).collect();
        if unique.len() != actual.len() || unique != expected {
            return Err(format!("assertion inventory differs for {call:?}"));
        }
        if !doc["assertions"]
            .as_array()
            .ok_or("missing assertions")?
            .iter()
            .all(|a| a["status"] == "pass")
        {
            return Err("assertion did not pass".into());
        }
    }
    Ok(())
}

#[test]
fn inventory_rejects_missing_duplicate_misplaced_and_undeclared_results() {
    let decl = serde_json::json!({"id":"s", "calls":[{"id":"a"}],
        "assertions":[{"id":"outer","call":null},{"id":"inner","call":"a"}]});
    let scenario = serde_json::json!({"protocol":1,"scenario":"s",
        "assertions":[{"id":"outer","status":"pass"}]});
    let call = serde_json::json!({"protocol":1,"scenario":"s","call":"a",
        "assertions":[{"id":"inner","status":"pass"}]});
    let calls = vec![("a.json".into(), call.clone())];
    assert!(validate_inventory(&decl, &scenario, &calls).is_ok());
    assert!(validate_inventory(&decl, &scenario, &[]).is_err());
    for assertions in [
        serde_json::json!([]),
        serde_json::json!([{"id":"inner","status":"pass"},{"id":"inner","status":"pass"}]),
        serde_json::json!([{"id":"outer","status":"pass"}]),
        serde_json::json!([{"id":"other","status":"pass"}]),
    ] {
        let mut bad = call.clone();
        bad["assertions"] = assertions;
        assert!(validate_inventory(&decl, &scenario, &[("a.json".into(), bad)]).is_err());
    }
    let mut missing = scenario.clone();
    missing["assertions"] = serde_json::json!([]);
    assert!(validate_inventory(&decl, &missing, &calls).is_err());
    let duplicates = vec![("a.json".into(), call.clone()), ("a.json".into(), call)];
    assert!(validate_inventory(&decl, &scenario, &duplicates).is_err());
}

#[test]
fn request_paths_round_trip_json_sensitive_characters() {
    let scratch = crate::common::test_scratch();
    let input = scratch.path().join("input-\"-\\-\n.osm.pbf");
    crate::common::write_base_pbf(&input);
    let (run, _) = repeat_cat(&input, 0, "2");
    let bytes = std::fs::read(run.root.join("request.json")).expect("request");
    let doc: Value = serde_json::from_slice(&bytes).expect("escaped request");
    assert_eq!(doc["bindings"]["input"], serde_json::json!(input));
}
