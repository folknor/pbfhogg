//! One scenario launch, evidence retention and independent verdict.
use super::artefact::{self, CallDoc, Outcome, Seal, Status};
use super::describe::{RoleKind, RoleSource, Scenario};
use crate::{identity, process};
use std::collections::BTreeMap;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
const REQUEST_FILE: &str = "request.json";
const CWD_DIR: &str = "cwd";
const ARTEFACTS_DIR: &str = "artefacts";
const ROLES_DIR: &str = "roles";
/// Lifecycle failure remains distinct for certification: no subsequent launch is safe.
pub(crate) struct Report {
    pub(crate) failures: Vec<String>,
    pub(crate) results: Vec<(Option<String>, Outcome)>,
    pub(crate) unestablished: bool,
    pub(crate) run_dir: PathBuf,
    pub(crate) identity: serde_json::Value,
}
fn path_string(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("non-UTF-8 path: {}", path.display()))
}
/// Atomic create-new allocation with no reuse, including after a pass removes its directory.
pub(crate) fn allocate(root: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    // Persist each reservation outside the removable run directory.
    let reservations = root.join(".library-run-ids");
    std::fs::create_dir_all(&reservations)?;
    for sequence in 0_u64.. {
        let id = format!("library-{sequence}");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(reservations.join(&id))
        {
            Ok(_) => {
                let path = root.join(id);
                std::fs::create_dir(&path)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("run id space exhausted"))
}
struct Fifo(PathBuf);
impl Drop for Fifo {
    fn drop(&mut self) {
        let _ignored = std::fs::remove_file(&self.0);
    }
}
fn fifo(root: &Path, run: &Path) -> Result<(Fifo, std::fs::File), String> {
    use std::os::unix::ffi::OsStrExt;
    let name = run.file_name().ok_or("run id missing")?;
    let path = root.join(name).with_extension("fifo");
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: live NUL-terminated pathname; mkfifo creates without replacing.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let guard = Fifo(path);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(&guard.0)
        .map_err(|e| e.to_string())?;
    Ok((guard, file))
}
pub(crate) fn verify(
    harness: &Path,
    scenario: &Scenario,
    inputs: &[pbfhogg_devkit::ResolvedInput],
    root: &Path,
) -> Result<Report, String> {
    let datasets = inputs
        .iter()
        .map(|i| (i.role.clone(), i.path.clone()))
        .collect();
    let prepared = prepare(scenario, &datasets, root, 0).map_err(|e| e.to_string())?;
    let execution = launch(harness, scenario, root, &prepared);
    finish(scenario, inputs, &prepared, &execution)
}
fn finish(
    scenario: &Scenario,
    inputs: &[pbfhogg_devkit::ResolvedInput],
    prepared: &Prepared,
    execution: &Result<process::Completed, process::Failure>,
) -> Result<Report, String> {
    let mut report = Report {
        failures: Vec::new(),
        results: Vec::new(),
        unestablished: false,
        run_dir: prepared.run_dir.clone(),
        identity: serde_json::json!({"datasets": input_identities(inputs)}),
    };
    let captures = match execution {
        Ok(run) => (&run.out, &run.err, &run.markers),
        Err(process::Failure::Unestablished(p) | process::Failure::Refused(p)) => {
            (&p.captures.out, &p.captures.err, &p.captures.markers)
        }
    };
    match execution {
        Err(e) => {
            report.unestablished = matches!(e, process::Failure::Unestablished(_));
            report.failures.push(e.to_string());
        }
        Ok(run) => {
            if run.timed_out {
                report.failures.push("worker deadline exceeded".into());
            }
            if run.residual_children {
                report.failures.push("worker left residual children".into());
            }
            for (name, cap) in [("stdout", &run.out), ("stderr", &run.err)] {
                if !cap.bytes.is_empty() || cap.overflow {
                    report
                        .failures
                        .push(format!("worker {name} is not empty and complete"));
                }
            }
            let layout = layout_failures(
                &prepared.run_dir,
                &prepared.request_bytes,
                prepared.cwd_identity,
                scenario,
            );
            if layout.is_empty() {
                judge_completed(run, scenario, inputs, prepared, &mut report);
            } else {
                report.failures.extend(layout);
            }
        }
    }
    if captures.2.overflow {
        report.failures.push("FIFO capture overflow".into());
    }
    if report.failures.is_empty()
        && let Err(error) = std::fs::remove_dir_all(&prepared.run_dir)
    {
        report.failures.push(format!(
            "remove passed run {}: {error}",
            prepared.run_dir.display()
        ));
    }
    if !report.failures.is_empty()
        && let Err(error) = evidence(&prepared.directory, captures, &report)
    {
        report.failures.push(error);
    }
    Ok(report)
}
fn judge_completed(
    run: &process::Completed,
    scenario: &Scenario,
    inputs: &[pbfhogg_devkit::ResolvedInput],
    prepared: &Prepared,
    report: &mut Report,
) {
    let forbidden: Vec<_> = prepared.forbidden.iter().map(String::as_str).collect();
    match run.status.code() {
        Some(code @ (0 | 1)) => {
            if let Some(judged) = judge_artefacts(
                run.elapsed,
                code,
                &prepared.artefacts,
                scenario,
                &forbidden,
                report,
            ) {
                collect(&judged, run, scenario, prepared, report);
                match identify(&judged, scenario, inputs, &prepared.paths) {
                    Ok(id) => report.identity = id,
                    Err(e) => report.failures.push(e),
                }
            }
        }
        Some(2) => {
            let refusal = if artefact::entry_kind(&prepared.artefacts)
                .is_ok_and(|k| k.is_some_and(|k| k.is_dir()))
            {
                artefact::read_refusal(&prepared.artefacts.join(artefact::REFUSAL_FILE))
            } else {
                Err("artefact directory replaced".into())
            };
            report
                .failures
                .push(format!("worker refused request: {refusal:?}"));
        }
        other => report
            .failures
            .push(format!("worker ended outside protocol: {other:?}")),
    }
}
fn launch(
    harness: &Path,
    scenario: &Scenario,
    root: &Path,
    p: &Prepared,
) -> Result<process::Completed, process::Failure> {
    let (fifo, file) = fifo(root, &p.run_dir).map_err(|reason| {
        process::Failure::Refused(Box::new(process::Problem {
            reason,
            captures: process::Captures::default(),
        }))
    })?;
    let mut cmd = process::command(harness);
    cmd.arg("--test-harness")
        .arg(&p.request)
        .arg("--private-dir")
        .arg(&p.cwd)
        .arg("--artefact-dir")
        .arg(&p.artefacts)
        .current_dir(&p.cwd)
        .env("BROKKR_MARKER_FIFO", &fifo.0);
    process::run(
        cmd,
        std::time::Duration::from_secs(u64::from(scenario.deadline_s)),
        Some(file),
    )
}
fn collect(
    j: &Judged,
    run: &process::Completed,
    scenario: &Scenario,
    p: &Prepared,
    report: &mut Report,
) {
    let (records, loss, census) = crate::sidecar::parse(
        &run.markers.bytes,
        run.markers.overflow,
        scenario.calls.len().saturating_add(1),
    );
    let limit = run
        .elapsed
        .saturating_add(crate::tunables::CLOCK_TOLERANCE)
        .as_micros();
    if records
        .iter()
        .any(|r| u128::try_from(r.timestamp_us()).map_or(true, |ts| ts > limit))
    {
        report
            .failures
            .push("FIFO record lies outside the worker clock interval".into());
    }
    if loss.stream_lost() {
        report.failures.extend(loss.stream_causes());
    }
    if let Some(e) = crate::stream::undeclared_failure(scenario, &census) {
        report.failures.push(e);
    }
    report
        .results
        .extend(j.scenario_results.iter().cloned().map(|r| (None, r)));
    for (id, doc) in &j.calls {
        report
            .results
            .extend(doc.results.iter().cloned().map(|r| (Some(id.clone()), r)));
        report.results.push((
            Some(id.clone()),
            Outcome {
                id: "brokkr.seal".into(),
                status: if doc.seal == Seal::Sealed {
                    Status::Pass
                } else {
                    Status::Fail
                },
                detail: if doc.seal == Seal::Sealed {
                    String::new()
                } else {
                    format!("{:?}", doc.seal)
                },
            },
        ));
        report.failures.extend(crate::publication::failures(
            scenario,
            id,
            doc,
            &p.run_dir.join(ROLES_DIR),
        ));
        let stream = crate::stream::judge(id, doc, &records, &loss);
        if scenario
            .calls
            .iter()
            .any(|c| c.id == *id && !c.stream.is_empty())
        {
            report.results.push((
                Some(id.clone()),
                Outcome {
                    id: "brokkr.stream".into(),
                    status: stream.status,
                    detail: stream.detail,
                },
            ));
        }
    }
    for (call, r) in &report.results {
        if r.status != Status::Pass {
            report.failures.push(format!(
                "{call:?}/{}: {}: {}",
                r.id,
                r.status.name(),
                r.detail
            ));
        }
    }
}
fn identify(
    j: &Judged,
    scenario: &Scenario,
    inputs: &[pbfhogg_devkit::ResolvedInput],
    paths: &BTreeMap<String, PathBuf>,
) -> Result<serde_json::Value, String> {
    let plan = identity::Document::of("plan", &j.plan)?;
    let mut contexts = BTreeMap::new();
    for (id, doc) in &j.calls {
        let d = identity::Document::of("context", &doc.context)?;
        contexts.insert(
            id,
            serde_json::json!({"digest":d.digest,"canonical":d.canonical}),
        );
    }
    let mut generated = BTreeMap::new();
    let mut seeds = BTreeMap::new();
    for role in &scenario.roles {
        if role.source == RoleSource::Generated {
            generated.insert(&role.id, identity::generated(&paths[&role.id], role.kind)?);
        }
        if role.seeded() {
            seeds.insert(&role.id, crate::seed::recipe(role.kind));
        }
    }
    let datasets = input_identities(inputs);
    Ok(
        serde_json::json!({"plan":{"digest":plan.digest,"canonical":plan.canonical},"contexts":contexts,"datasets":datasets,"generated":generated,"seeds":seeds}),
    )
}
fn input_identities(inputs: &[pbfhogg_devkit::ResolvedInput]) -> BTreeMap<&str, serde_json::Value> {
    inputs
        .iter()
        .map(|i| {
            (
                i.role.as_str(),
                serde_json::json!({
                    "expected_digest": i.expected_digest,
                    "observed_digest": i.observed_digest,
                    "variant": i.variant,
                }),
            )
        })
        .collect()
}
fn evidence(
    directory: &std::fs::File,
    caps: (&process::Capture, &process::Capture, &process::Capture),
    report: &Report,
) -> Result<(), String> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    // Both directories stay pinned by descriptor, including on uncertain
    // cleanup. Never follow a worker-supplied replacement or evidence link.
    let path = PathBuf::from(format!("/proc/self/fd/{}/evidence", directory.as_raw_fd()));
    let write = || -> std::io::Result<()> {
        std::fs::create_dir(&path)?;
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&path)?;
        let pinned = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        let identity = serde_json::to_vec_pretty(&report.identity)?;
        let capture = serde_json::to_vec_pretty(&serde_json::json!({
            "stdout_overflow":caps.0.overflow, "stderr_overflow":caps.1.overflow,
            "fifo_overflow":caps.2.overflow, "quiescence_unestablished":report.unestablished,
        }))?;
        let reasons = report.failures.join("\n");
        for (name, bytes) in [
            ("stdout", caps.0.bytes.as_slice()),
            ("stderr", caps.1.bytes.as_slice()),
            ("stream.txt", caps.2.bytes.as_slice()),
            ("failures.txt", reasons.as_bytes()),
            ("identity.json", identity.as_slice()),
            ("capture.json", capture.as_slice()),
        ] {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(pinned.join(name))?;
            file.write_all(bytes)?;
        }
        Ok(())
    };
    write().map_err(|e| {
        format!(
            "failed to retain evidence at {}: {e}; {}",
            report.run_dir.join("evidence").display(),
            report.failures.join("; ")
        )
    })
}
struct Prepared {
    directory: std::fs::File,
    run_dir: PathBuf,
    cwd: PathBuf,
    artefacts: PathBuf,
    request: PathBuf,
    request_bytes: Vec<u8>,
    cwd_identity: (u64, u64),
    /// Spellings no plan or context string may contain.
    forbidden: Vec<String>,
    /// Every role's bound path.
    paths: BTreeMap<String, PathBuf>,
}

/// Allocate the run directory and lay it out: `cwd/`, `artefacts/`,
/// `roles/` with each role in its initial state (a seeded output role as
/// its seed), and the request binding every role.
fn prepare(
    scenario: &Scenario,
    datasets: &BTreeMap<String, PathBuf>,
    library_root: &Path,
    iteration: usize,
) -> Result<Prepared, Box<dyn std::error::Error>> {
    let lexical = allocate(library_root)?;
    let run_dir = std::fs::canonicalize(&lexical)?;
    let (cwd, artefacts, roles) = (
        run_dir.join(CWD_DIR),
        run_dir.join(ARTEFACTS_DIR),
        run_dir.join(ROLES_DIR),
    );
    for dir in [&cwd, &artefacts, &roles] {
        std::fs::create_dir(dir)?;
    }

    let mut bindings: BTreeMap<String, String> = BTreeMap::new();
    let mut paths: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut forbidden: Vec<String> = Vec::new();
    for spelling in [&lexical, &run_dir] {
        push_spelling(&mut forbidden, spelling)?;
    }
    for role in &scenario.roles {
        let path = match &role.source {
            RoleSource::Dataset { .. } => {
                let given = datasets
                    .get(&role.id)
                    .ok_or_else(|| format!("dataset role {} was not bound", role.id))?;
                push_spelling(&mut forbidden, given)?;
                let canonical = std::fs::canonicalize(given).map_err(|e| {
                    format!("dataset file {} for role {}: {e}", given.display(), role.id)
                })?;
                push_spelling(&mut forbidden, &canonical)?;
                canonical
            }
            RoleSource::Generated | RoleSource::Output { .. } | RoleSource::Scratch { .. } => {
                let path = roles.join(&role.id);
                let starts_empty_dir = role.kind == RoleKind::Directory
                    && !matches!(role.source, RoleSource::Output { .. });
                if starts_empty_dir {
                    std::fs::create_dir(&path)?;
                }
                if role.seeded() {
                    super::seed::write(&path, role.kind, &role.id)?;
                }
                path
            }
        };
        bindings.insert(role.id.clone(), path_string(&path)?);
        paths.insert(role.id.clone(), path);
    }
    let request_bytes = request_document(&scenario.id, iteration, &bindings)?;
    let request = run_dir.join(REQUEST_FILE);
    std::fs::write(&request, &request_bytes)?;
    let cwd_identity = identity(&cwd)?;

    Ok(Prepared {
        directory: std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&run_dir)?,
        run_dir,
        cwd,
        artefacts,
        request,
        request_bytes,
        cwd_identity,
        forbidden,
        paths,
    })
}

fn push_spelling(out: &mut Vec<String>, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let spelled = path_string(path)?;
    if !out.contains(&spelled) {
        out.push(spelled);
    }
    Ok(())
}

/// The request: `protocol`, `scenario`, `iteration` and `bindings`.
fn request_document(
    scenario: &str,
    iteration: usize,
    bindings: &BTreeMap<String, String>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let doc = serde_json::json!({
        "protocol": super::describe::PROTOCOL,
        "scenario": scenario,
        "iteration": iteration,
        "bindings": bindings,
    });
    let mut bytes =
        serde_json::to_vec_pretty(&doc).map_err(|e| format!("cannot write the request: {e}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn identity(path: &Path) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::symlink_metadata(path)?;
    Ok((meta.dev(), meta.ino()))
}

/// The exact run layout after quiescence: unchanged request, original empty
/// cwd, artefacts and roles directories, and only declared roles of their kind.
fn layout_failures(
    run_dir: &Path,
    request: &[u8],
    cwd_identity: (u64, u64),
    scenario: &Scenario,
) -> Vec<String> {
    let mut failures = Vec::new();
    let mut fail = |why: String| failures.push(format!("run directory: {why}"));
    // Nothing is traversed through a symlink: a directory that is not one
    // is a failure, never listed.
    match artefact::entry_kind(run_dir) {
        Ok(Some(kind)) if kind.is_dir() => {}
        Ok(_) => {
            fail(format!("{} is no longer a directory", run_dir.display()));
            return failures;
        }
        Err(e) => {
            fail(e);
            return failures;
        }
    }
    let listed = match artefact::entries(run_dir) {
        Ok(listed) => listed,
        Err(e) => {
            fail(e);
            return failures;
        }
    };
    let is_dir = |name: &str| listed.iter().any(|(n, kind)| n == name && kind.is_dir());
    for (name, kind) in &listed {
        let fits = match name.as_str() {
            REQUEST_FILE => kind.is_file(),
            CWD_DIR | ARTEFACTS_DIR | ROLES_DIR => kind.is_dir(),
            other => {
                fail(format!("an unexpected entry {other:?}"));
                continue;
            }
        };
        if !fits {
            fail(format!("{name} has the wrong type"));
        }
    }
    for name in [REQUEST_FILE, CWD_DIR, ARTEFACTS_DIR, ROLES_DIR] {
        if !listed.iter().any(|(n, _)| n == name) {
            fail(format!("{name} is missing"));
        }
    }
    match artefact::read_regular(&run_dir.join(REQUEST_FILE), request.len()) {
        Ok(bytes) if bytes == request => {}
        Ok(_) | Err(_) => fail(format!("{REQUEST_FILE} was changed")),
    }
    let cwd = run_dir.join(CWD_DIR);
    let same_cwd = is_dir(CWD_DIR) && identity(&cwd).is_ok_and(|id| id == cwd_identity);
    if same_cwd {
        match artefact::entries(&cwd) {
            Ok(inside) if inside.is_empty() => {}
            Ok(_) => fail(format!(
                "{CWD_DIR}/ is not empty: the harness left something in its working directory"
            )),
            Err(e) => fail(e),
        }
    } else {
        fail(format!("{CWD_DIR}/ is not the directory brokkr made"));
    }
    if !is_dir(ROLES_DIR) {
        return failures;
    }
    let inside = match artefact::entries(&run_dir.join(ROLES_DIR)) {
        Ok(inside) => inside,
        Err(e) => {
            fail(e);
            return failures;
        }
    };
    for (name, kind) in inside {
        let role = scenario
            .roles
            .iter()
            .find(|r| r.id == name && !matches!(r.source, RoleSource::Dataset { .. }));
        let Some(role) = role else {
            fail(format!("{ROLES_DIR}/{name:?} is not a bound role"));
            continue;
        };
        let (fits, what) = match role.kind {
            RoleKind::Pbf | RoleKind::File => (kind.is_file(), "regular file"),
            RoleKind::Directory => (kind.is_dir(), "directory"),
        };
        if !fits {
            fail(format!("{ROLES_DIR}/{name} is not a {what}"));
        }
    }
    failures
}

struct Judged {
    scenario_results: Vec<Outcome>,
    plan: super::json::Json,
    /// Setup's and the harness's own diagnostics, from `scenario.json`.
    _scenario_diagnostics: Vec<artefact::Diagnostic>,
    calls: Vec<(String, CallDoc)>,
}

fn judge_artefacts(
    elapsed: std::time::Duration,
    code: i32,
    artefacts: &Path,
    scenario: &Scenario,
    forbidden: &[&str],
    report: &mut Report,
) -> Option<Judged> {
    let invalid = |why: String| format!("the artefacts are invalid: {why}");
    let tree = match artefact::read_tree(artefacts, scenario) {
        Ok(tree) => tree,
        Err(e) => {
            report.failures.push(invalid(e));
            return None;
        }
    };
    let scenario_doc = match artefact::parse_scenario(&tree.scenario, scenario, forbidden) {
        Ok(doc) => doc,
        Err(e) => {
            report.failures.push(invalid(e));
            return None;
        }
    };
    let mut calls: Vec<(String, CallDoc)> = Vec::new();
    for (id, bytes) in &tree.calls {
        match artefact::parse_call(bytes, scenario, id, forbidden) {
            Ok(doc) => calls.push((id.clone(), doc)),
            Err(e) => {
                report.failures.push(invalid(e));
                return None;
            }
        }
    }

    let artefact::ScenarioDoc {
        results: scenario_results,
        plan,
        diagnostics: scenario_diagnostics,
    } = scenario_doc;
    let call_results: Vec<(String, Vec<Outcome>)> = calls
        .iter()
        .map(|(id, doc)| (id.clone(), doc.results.clone()))
        .collect();
    let inventory = artefact::inventory_failures(scenario, &scenario_results, &call_results);
    let complete = inventory.is_empty();
    report.failures.extend(inventory.iter().cloned());

    // Each reading shares the worker clock and must fit the observed wall.
    let limit_us = u64::try_from(elapsed.as_micros())
        .unwrap_or(u64::MAX)
        .saturating_add(
            u64::try_from(crate::tunables::CLOCK_TOLERANCE.as_micros()).unwrap_or(u64::MAX),
        );
    for (id, doc) in &calls {
        let readings = [
            ("entry_us", doc.entry_us),
            ("return_us", doc.return_us),
            ("seal_us", doc.seal_us),
        ]
        .into_iter()
        .chain(
            doc.diagnostics
                .iter()
                .map(|d| ("a diagnostic's at_us", d.at_us)),
        );
        for (what, end) in readings {
            if end > limit_us {
                report.failures.push(format!(
                    "call {id}: {what} is {end} us, beyond the process wall plus clock tolerance ({limit_us} us)"
                ));
            }
        }
    }

    let harness_nonpass = scenario_results
        .iter()
        .chain(calls.iter().flat_map(|(_, doc)| doc.results.iter()))
        .any(|r| r.status != Status::Pass);
    if complete {
        if code == 0 && harness_nonpass {
            report
                .failures
                .push("the harness exited 0 but reported a result that did not pass".to_owned());
        }
        if code == 1 && !harness_nonpass {
            report
                .failures
                .push("the harness exited 1 but every result it reported passed".to_owned());
        }
    }
    Some(Judged {
        scenario_results,
        plan,
        _scenario_diagnostics: scenario_diagnostics,
        calls,
    })
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
