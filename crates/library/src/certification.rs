//! Sequential sweep and permanent certification publication.
use crate::{certify, describe, discovery, identity, requirements, run};
use std::io::Write as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

fn inputs(
    config: &pbfhogg_devkit::Config,
    host: &str,
    dataset: &str,
    snapshot: Option<&str>,
    scenario: &describe::Scenario,
) -> Result<Vec<pbfhogg_devkit::ResolvedInput>, String> {
    scenario
        .roles
        .iter()
        .filter_map(|role| match &role.source {
            describe::RoleSource::Dataset { variant } => Some(
                config
                    .resolve_input(host, dataset, snapshot, &role.id, variant)
                    .map_err(|e| e.to_string()),
            ),
            _ => None,
        })
        .collect()
}

/// Include unlaunched declarations too: missing coverage is failed, not unsupported.
fn sweep<'a>(
    doc: &'a describe::Describe,
    mut launch: impl FnMut(&describe::Scenario) -> Result<run::Report, String>,
) -> Vec<certify::Attempt<'a>> {
    let mut stopped = false;
    doc.scenarios
        .iter()
        .map(|scenario| {
            if stopped {
                return certify::Attempt::errored(
                    scenario,
                    "not launched: quiescence unestablished".into(),
                );
            }
            match launch(scenario) {
                Ok(report) => {
                    stopped = report.unestablished;
                    certify::Attempt::of_report(scenario, &report)
                }
                Err(error) => certify::Attempt::errored(scenario, error),
            }
        })
        .collect()
}

/// Reserve through the same permanent id namespace as scenario runs.
/// A create-new file never replaces a previous certification.
fn persist(root: &Path, record: &mut serde_json::Value) -> Result<std::path::PathBuf, String> {
    let reserved = run::allocate(root).map_err(|e| e.to_string())?;
    let id = reserved.file_name().ok_or("missing certification id")?;
    record["run_id"] = id.to_string_lossy().into_owned().into();
    let dir = root.join("library-certifications");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&dir)
        .map_err(|e| e.to_string())?;
    let path = dir.join(id).with_extension("json");
    let name = std::ffi::CString::new(format!("{}.json", id.to_string_lossy()))
        .map_err(|e| e.to_string())?;
    // SAFETY: live directory descriptor and NUL-terminated single-component name.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: openat returned a fresh descriptor, now owned by this File.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let bytes = serde_json::to_vec(record).map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    directory.sync_all().map_err(|e| e.to_string())?;
    std::fs::remove_dir(reserved).map_err(|e| e.to_string())?;
    Ok(path)
}

pub(crate) fn verify(
    config: &pbfhogg_devkit::Config,
    host: &str,
    harness: &Path,
    dataset: &str,
    snapshot: Option<&str>,
) -> Result<(), String> {
    let digest = identity::file_digest(harness)?;
    let doc = discovery::load(harness)?;
    let root = config
        .scratch_root(host)
        .map_err(|e| e.to_string())?
        .join("tools");
    let attempts = sweep(&doc, |scenario| {
        let inputs = inputs(config, host, dataset, snapshot, scenario)?;
        run::verify(harness, scenario, &inputs, &root)
    });
    let mut record = certify::record(&requirements::applied(), &attempts, &digest);
    record["dataset"] = dataset.into();
    record["snapshot"] = snapshot.unwrap_or("base").into();
    record["hostname"] = host.into();
    // An executable changed during the sweep cannot identify what was certified.
    if identity::file_digest(harness).as_ref() != Ok(&digest) {
        record["certified"] = false.into();
        record["failures"] = serde_json::json!(["harness executable changed during certification"]);
    }
    let path = persist(&root, &mut record)?;
    crate::output::write(|out| writeln!(out, "{record}")).map_err(|e| e.to_string())?;
    if record["certified"] == true {
        Ok(())
    } else {
        Err(format!(
            "certification failed; record kept at {}",
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    #[test]
    fn stops_before_next_launch_and_preserves_missing_declarations() {
        let doc = describe::parse(br#"{"protocol":1,"scenarios":[{"id":"one","description":"d","deadline_s":1,"roles":[],"calls":[{"id":"a","label":"a"}],"assertions":[]},{"id":"two","description":"d","deadline_s":1,"roles":[],"calls":[{"id":"a","label":"a"}],"assertions":[]}]}"#).unwrap();
        let mut launches = 0;
        let attempts = sweep(&doc, |_| {
            launches += 1;
            Ok(run::Report {
                failures: vec!["cleanup failed".into()],
                results: vec![],
                unestablished: true,
                run_dir: "kept".into(),
                identity: serde_json::json!({"datasets":[]}),
            })
        });
        assert_eq!(launches, 1);
        assert_eq!(attempts.len(), 2);
        let record = certify::record(&requirements::applied(), &attempts, "digest");
        assert_eq!(record["complete"], false);
        assert_eq!(record["certified"], false);
        assert_eq!(
            record["scenarios"][0]["identity"]["datasets"],
            serde_json::json!([])
        );
        assert_eq!(record["harness"]["xxh128"], "digest");
    }
    #[test]
    fn certifications_are_permanent_and_ids_not_reused() {
        let owner = pbfhogg_test_scratch::scratch();
        let mut a = serde_json::json!({});
        let first = persist(owner.path(), &mut a).unwrap();
        let mut b = serde_json::json!({});
        let second = persist(owner.path(), &mut b).unwrap();
        assert_ne!(first, second);
        assert_ne!(a["run_id"], b["run_id"]);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(first).unwrap()).unwrap(),
            a
        );
    }
    #[test]
    fn failed_scenario_does_not_stop_later_scenarios() {
        let doc = describe::parse(include_bytes!("../fixtures/describe.json")).unwrap();
        let mut launches = 0;
        let attempts = sweep(&doc, |_| {
            launches += 1;
            Err("resolution failed".into())
        });
        assert_eq!(launches, doc.scenarios.len());
        assert!(attempts.iter().all(|a| !a.passed));
    }

    #[test]
    fn refuses_a_linked_certification_directory() {
        let owner = pbfhogg_test_scratch::scratch();
        let elsewhere = owner.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, owner.path().join("library-certifications"))
            .unwrap();
        assert!(persist(owner.path(), &mut serde_json::json!({})).is_err());
        assert_eq!(std::fs::read_dir(elsewhere).unwrap().count(), 0);
    }
}
