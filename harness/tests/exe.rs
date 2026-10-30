//! The harness executable against brokkr's library protocol: describe mode,
//! a passing run of every scenario over a generated input, the identity of
//! `repeat-cat`'s plan and contexts, and quiet refusals.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
fn run(mut cmd: Command, env: &[(&str, &str)], scratch: &Path) -> Ran {
    for var in [
        "BROKKR_MARKER_FIFO",
        "BROKKR_HARNESS_ARTEFACT_DIR",
        "HOTPATH_OUTPUT_PATH",
        "HOTPATH_OUTPUT_FORMAT",
        "HOTPATH_METRICS_SERVER_OFF",
        "RAYON_NUM_THREADS",
    ] {
        cmd.env_remove(var);
    }
    cmd.envs(env.iter().copied());
    let out = scratch.join("stdout");
    let err = scratch.join("stderr");
    cmd.stdin(Stdio::null())
        .stdout(std::fs::File::create(&out).expect("stdout file"))
        .stderr(std::fs::File::create(&err).expect("stderr file"));
    crate::guard::apply(&mut cmd);
    let mut child = cmd.spawn().expect("spawn the harness");
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for the harness") {
            break status;
        }
        if start.elapsed() > Duration::from_secs(15) {
            child.kill().ok();
            child.wait().ok();
            panic!("the harness did not finish");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
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
    let ran = run(cmd, &[], scratch.path());
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

/// A marker FIFO whose lines a thread collects until [`Fifo::finish`].
struct Fifo {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    reader: std::thread::JoinHandle<Vec<u8>>,
}

impl Fifo {
    #[allow(clippy::disallowed_methods)] // a FIFO in test scratch
    fn new(dir: &Path) -> Self {
        use std::io::Read as _;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let path = dir.join("fifo");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL");
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        // Opened here, non-blocking, so a child that never opens the FIFO
        // cannot leave the reader waiting.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .expect("open the read end");
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let reader = std::thread::spawn(move || {
            let mut lines = Vec::new();
            let mut buf = [0_u8; 65536];
            loop {
                let done = stopping.load(Ordering::Acquire);
                match file.read(&mut buf) {
                    Ok(n) if n > 0 => lines.extend_from_slice(&buf[..n]),
                    _ if done => return lines,
                    _ => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        });
        Self { path, stop, reader }
    }

    fn finish(self) -> String {
        self.stop.store(true, Ordering::Release);
        String::from_utf8(self.reader.join().expect("the reader")).expect("UTF-8 lines")
    }
}

/// A run directory laid out as brokkr lays it out.
struct RunDir {
    _scratch: tempfile::TempDir,
    root: PathBuf,
}

impl RunDir {
    #[allow(clippy::disallowed_methods)] // fixtures in test scratch
    fn new(input: &Path, request: &str) -> Self {
        let scratch = crate::common::test_scratch();
        let root = scratch.path().canonicalize().expect("canonical scratch");
        for dir in ["cwd", "artefacts", "roles", "private", "capture"] {
            std::fs::create_dir(root.join(dir)).expect("layout");
        }
        let request = request
            .replace("@INPUT@", &input.display().to_string())
            .replace("@ROLES@", &root.join("roles").display().to_string());
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
            (
                "BROKKR_HARNESS_ARTEFACT_DIR",
                artefacts.to_str().expect("UTF-8"),
            ),
            ("BROKKR_MARKER_FIFO", fifo.to_str().expect("UTF-8")),
        ];
        vars.extend_from_slice(env);
        run(cmd, &vars, &self.root.join("capture"))
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

const REPEAT_CAT: &str = r#"{"protocol": 1, "scenario": "repeat-cat", "iteration": ITER,
  "bindings": {"input": "@INPUT@", "out-a": "@ROLES@/out-a", "out-b": "@ROLES@/out-b"}}"#;

/// A sorted, indexed input in its own scratch.
fn input() -> (tempfile::TempDir, PathBuf) {
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
    let run_dir = RunDir::new(input, &REPEAT_CAT.replace("ITER", &iteration.to_string()));
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
    let (keep, input) = input();
    let raw = keep
        .path()
        .canonicalize()
        .expect("canonical")
        .join("raw.osm.pbf");
    crate::common::write_base_pbf_non_indexed(&raw);
    let mut bindings: Vec<String> = datasets
        .iter()
        .map(|role| match *role {
            "raw" => format!("\"raw\": \"{}\"", raw.display()),
            _ => format!("\"{role}\": \"@INPUT@\""),
        })
        .collect();
    for role in roles.iter().chain(seeded) {
        bindings.push(format!("\"{role}\": \"@ROLES@/{role}\""));
    }
    let request = format!(
        "{{\"protocol\": 1, \"scenario\": \"{scenario}\", \"iteration\": 0, \"bindings\": {{{}}}}}",
        bindings.join(", ")
    );
    let run_dir = RunDir::new(&input, &request);
    for role in seeded {
        std::fs::write(
            run_dir.root.join("roles").join(role),
            format!("brokkr-library-seed-v1 {role}\n"),
        )
        .expect("seed");
    }
    let fifo = Fifo::new(&run_dir.root.join("capture"));
    let ran = run_dir.launch(&fifo.path.clone(), &[]);
    let lines = fifo.finish();
    assert!(
        ran.stdout.is_empty() && ran.stderr.is_empty(),
        "stdout {:?} stderr {:?}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    let mut results = run_dir.doc("scenario.json")["assertions"]
        .as_array()
        .expect("results")
        .clone();
    for call in run_dir.entries("artefacts/calls") {
        let doc = run_dir.doc(&format!("calls/{call}"));
        results.extend(
            doc["assertions"]
                .as_array()
                .expect("results")
                .iter()
                .cloned(),
        );
    }
    assert!(
        results.iter().all(|r| r["status"] == "pass"),
        "{scenario}: {results:?}"
    );
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
fn nofile_cat_is_refused_at_soft_1024() {
    let (run, _) = scenario_passes("nofile-cat-refused", false, &["out"], &[]);
    let doc = run.doc("calls/a.json");
    assert_eq!(doc["context"]["rlimit_nofile_soft"], 1024);
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
    assert_eq!(doc["context"]["rlimit_nofile_soft"], 1024);
    assert_eq!(doc["outcome"]["kind"], "invalid_argument");
}

#[test]
fn nofile_altw_external_runs_three_workers() {
    let (run, _) = scenario_passes("nofile-altw-external", true, &["out"], &[]);
    assert_eq!(run.doc("calls/a.json")["context"]["parallelism"], 8);
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

/// A request the harness cannot honour exits 2 without a byte of output,
/// and says why in refusal.json.
#[test]
fn bad_requests_are_refused_quietly() {
    let (_keep, input) = input();
    for request in [
        REPEAT_CAT
            .replace("repeat-cat", "no-such")
            .replace("ITER", "0"),
        REPEAT_CAT
            .replace(", \"out-b\": \"@ROLES@/out-b\"", "")
            .replace("ITER", "0"),
        REPEAT_CAT.replace("\"iteration\": ITER", "\"iteration\": 0, \"iteration\": 0"),
        REPEAT_CAT.replace("ITER", "-0"),
        REPEAT_CAT.replace("ITER", "0.5"),
        format!("{} x", REPEAT_CAT.replace("ITER", "0")),
    ] {
        let run_dir = RunDir::new(&input, &request);
        let missing = run_dir.root.join("no-fifo");
        let ran = run_dir.launch(&missing, &[]);
        assert_eq!(ran.status.code(), Some(2), "{request}");
        assert!(ran.stdout.is_empty() && ran.stderr.is_empty(), "{request}");
        assert_eq!(run_dir.entries("artefacts"), ["refusal.json"], "{request}");
        let reason = run_dir.doc("refusal.json");
        assert_eq!(reason["protocol"], 1);
        assert!(!reason["reason"].as_str().expect("reason").is_empty());
        assert!(run_dir.entries("roles").is_empty());
    }
}
