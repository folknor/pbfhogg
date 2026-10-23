//! Scratch, end to end through the binary: `--tmp-dir` is validated before
//! any input is touched, a run leaves no scratch behind (on success or
//! failure), and what crashed runs left is reported and never deleted.
//!
//! The mechanism itself (the run directory, the lock, the scan and its typed
//! values) is proven on the library in `src/scratch.rs`; the backends that
//! write into a run directory are in `host_contract`; the forced failures of
//! each scratch-producing command are the `fault_*` tests.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use crate::common::cli::CliInvoker;
use tempfile::TempDir;

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// The layout of a run directory whose owner is gone: nobody holds its lock.
fn plant_dead_run_dir(parent: &Path) -> PathBuf {
    let dir = parent.join(".pbfhogg-run-999999-0123456789abcdef0123456789abcdef");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("LOCK"), b"").unwrap();
    std::fs::write(dir.join("READY"), b"").unwrap();
    std::fs::write(dir.join("id-W0-000"), vec![7_u8; 2048]).unwrap();
    dir
}

#[test]
fn a_bad_tmp_dir_fails_before_any_input_is_read() {
    let dir = TempDir::new().unwrap();
    let missing_input = dir.path().join("no-such-input.osm.pbf");
    let missing_other = dir.path().join("no-such-other.osm.pbf");
    let tmp = dir.path().join("no-such-dir");
    let output = dir.path().join("out");
    let (input, other, tmp_arg, output_arg) = (
        missing_input.to_str().unwrap(),
        missing_other.to_str().unwrap(),
        tmp.to_str().unwrap(),
        output.to_str().unwrap(),
    );
    let runs: Vec<Vec<&str>> = vec![
        vec![
            "add-locations-to-ways",
            input,
            "-o",
            output_arg,
            "--tmp-dir",
            tmp_arg,
        ],
        vec!["diff", input, other, "--tmp-dir", tmp_arg],
        vec![
            "diff",
            input,
            other,
            "--format",
            "osc",
            "-o",
            output_arg,
            "--tmp-dir",
            tmp_arg,
        ],
        vec![
            "build-geocode-index",
            input,
            "--output-dir",
            output_arg,
            "--tmp-dir",
            tmp_arg,
        ],
    ];
    for args in runs {
        let out = CliInvoker::new().args(&args).assert_failure();
        let stderr = out.stderr_str();
        assert!(stderr.contains("--tmp-dir"), "{args:?}: {stderr}");
        assert!(stderr.contains(tmp_arg), "{args:?}: {stderr}");
        assert!(
            !stderr.contains("no-such-input"),
            "{args:?}: the input must not have been looked at: {stderr}"
        );
        assert!(!tmp.exists(), "{args:?}: --tmp-dir must not be created");
        assert!(!output.exists(), "{args:?}: nothing may be published");
    }
}

#[test]
fn a_tmp_dir_that_is_a_file_or_unwritable_is_refused() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("missing.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    let file = dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let out = CliInvoker::new()
        .args([
            "add-locations-to-ways",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--tmp-dir",
            file.to_str().unwrap(),
        ])
        .assert_failure();
    assert!(
        out.stderr_str().contains("not a directory"),
        "{}",
        out.stderr_str()
    );

    // Root ignores directory permissions, so this half cannot fail there.
    if is_root() {
        return;
    }
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o500)).unwrap();
    let out = CliInvoker::new()
        .args([
            "add-locations-to-ways",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--tmp-dir",
            locked.to_str().unwrap(),
        ])
        .assert_failure();
    let stderr = out.stderr_str();
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    assert!(stderr.contains("--tmp-dir"), "{stderr}");
    assert!(!stderr.contains("missing.osm.pbf"), "{stderr}");
    assert!(names(&locked).is_empty());
}

#[test]
fn a_failed_run_leaves_no_scratch_in_the_tmp_dir_or_beside_the_output() {
    let dir = TempDir::new().unwrap();
    let work = dir.path().join("work");
    let tmp = dir.path().join("tmp");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&tmp).unwrap();
    let input = work.join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let mut bytes = std::fs::read(&input).unwrap();
    bytes.truncate(bytes.len() - 5);
    let truncated = work.join("truncated.osm.pbf");
    std::fs::write(&truncated, bytes).unwrap();
    let output = work.join("out.osm.pbf");

    for tmp_args in [vec![], vec!["--tmp-dir", tmp.to_str().unwrap()]] {
        let mut args = vec![
            "add-locations-to-ways",
            truncated.to_str().unwrap(),
            "--force",
            "-o",
            output.to_str().unwrap(),
        ];
        args.extend(tmp_args);
        CliInvoker::new().args(&args).assert_failure();
        assert!(names(&tmp).is_empty(), "{:?}", names(&tmp));
        let beside: Vec<_> = names(&work);
        assert_eq!(
            beside,
            ["in.osm.pbf", "truncated.osm.pbf"],
            "nothing may be left beside the output"
        );
    }
}

#[test]
fn successful_runs_clean_up_after_themselves() {
    let dir = TempDir::new().unwrap();
    let tmp = dir.path().join("tmp");
    std::fs::create_dir(&tmp).unwrap();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    crate::common::write_base_pbf(&old);
    crate::common::write_changed_pbf(&new);
    let tmp_arg = tmp.to_str().unwrap();

    // Text diff has no output file: the scratch goes where --tmp-dir says.
    let out = CliInvoker::new()
        .args([
            "diff",
            "-j",
            "2",
            old.to_str().unwrap(),
            new.to_str().unwrap(),
            "--tmp-dir",
            tmp_arg,
        ])
        .run();
    assert!(
        out.status.code().is_some_and(|c| c <= 1),
        "{}",
        out.stderr_str()
    );
    assert!(names(&tmp).is_empty(), "{:?}", names(&tmp));

    let osc = dir.path().join("delta.osc.gz");
    CliInvoker::new()
        .args([
            "diff",
            "--format",
            "osc",
            "-j",
            "2",
            old.to_str().unwrap(),
            new.to_str().unwrap(),
            "-o",
            osc.to_str().unwrap(),
            "--tmp-dir",
            tmp_arg,
        ])
        .assert_success();
    assert!(osc.exists());
    assert!(names(&tmp).is_empty(), "{:?}", names(&tmp));

    let located = dir.path().join("located.osm.pbf");
    CliInvoker::new()
        .args([
            "add-locations-to-ways",
            old.to_str().unwrap(),
            "-o",
            located.to_str().unwrap(),
            "--tmp-dir",
            tmp_arg,
        ])
        .assert_success();
    assert!(names(&tmp).is_empty(), "{:?}", names(&tmp));
    assert!(
        !names(dir.path()).iter().any(|n| n.starts_with(".pbfhogg-")),
        "{:?}",
        names(dir.path())
    );
}

#[test]
fn a_tmp_dir_inside_a_replaced_index_is_refused() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let index = dir.path().join("index");
    CliInvoker::new()
        .args([
            "build-geocode-index",
            input.to_str().unwrap(),
            "--output-dir",
            index.to_str().unwrap(),
            "--allow-unindexed",
        ])
        .run();
    if !index.exists() {
        // The fixture has no addresses; whatever the builder made of it, an
        // existing directory is what this test needs.
        std::fs::create_dir(&index).unwrap();
    }
    let out = CliInvoker::new()
        .args([
            "build-geocode-index",
            input.to_str().unwrap(),
            "--output-dir",
            index.to_str().unwrap(),
            "--overwrite",
            "--tmp-dir",
            index.to_str().unwrap(),
        ])
        .assert_failure();
    assert!(
        out.stderr_str().contains("inside the index output")
            || out.stderr_str().contains("not a geocode index"),
        "{}",
        out.stderr_str()
    );
    assert!(
        !names(&index).iter().any(|n| n.starts_with(".pbfhogg-run-")),
        "scratch must not be left inside the index: {:?}",
        names(&index)
    );
}

#[test]
fn leftovers_of_crashed_runs_are_reported_and_never_removed() {
    let dir = TempDir::new().unwrap();
    let work = dir.path().join("work");
    let tmp = dir.path().join("tmp");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&tmp).unwrap();
    let input = work.join("in.osm.pbf");
    crate::common::write_base_pbf(&input);

    let dead_run = plant_dead_run_dir(&tmp);
    let legacy = work.join(".pbfhogg-external-join-999999");
    std::fs::create_dir(&legacy).unwrap();
    std::fs::write(legacy.join("id-W0-000"), vec![1_u8; 512]).unwrap();
    let staging = work.join(".pbfhogg-tmp-999999-0.out.osm.pbf");
    std::fs::write(&staging, vec![2_u8; 100]).unwrap();
    let backup = work.join(".pbfhogg-backup-999999-0.out.osm.pbf");
    std::fs::write(&backup, b"the only copy of the previous output").unwrap();
    let unrelated = work.join("diff-par-notes.txt");
    std::fs::write(&unrelated, b"mine").unwrap();

    let output = work.join("located.osm.pbf");
    let out = CliInvoker::new()
        .args([
            "add-locations-to-ways",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--tmp-dir",
            tmp.to_str().unwrap(),
        ])
        .assert_success();
    let stderr = out.stderr_str();
    for found in [&dead_run, &legacy, &staging, &backup] {
        assert!(
            stderr.contains(&found.display().to_string()),
            "{found:?} not reported:\n{stderr}"
        );
    }
    assert!(!stderr.contains("diff-par-notes"), "{stderr}");
    assert!(
        stderr.contains("only copy"),
        "the backup needs its own wording:\n{stderr}"
    );
    assert!(stderr.contains("never deletes"), "{stderr}");
    assert!(stderr.contains("Warning: "), "{stderr}");

    // Reported, never reaped.
    assert!(dead_run.join("id-W0-000").exists());
    assert!(legacy.join("id-W0-000").exists());
    assert_eq!(std::fs::read(&staging).unwrap(), vec![2_u8; 100]);
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        b"the only copy of the previous output"
    );
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"mine");
    assert!(output.exists());
}

#[test]
fn a_live_run_directory_is_not_reported() {
    // A second process's run directory holds its lock; here the lock is held
    // by this test's own descriptor, which is what a live run looks like.
    let dir = TempDir::new().unwrap();
    let live = plant_dead_run_dir(dir.path());
    let lock = std::fs::File::open(live.join("LOCK")).unwrap();
    // SAFETY: valid descriptor, standard flock call.
    assert_eq!(
        unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&lock),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        },
        0
    );
    let input = dir.path().join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let out = CliInvoker::new()
        .args([
            "sort",
            input.to_str().unwrap(),
            "-o",
            dir.path().join("sorted.osm.pbf").to_str().unwrap(),
        ])
        .assert_success();
    assert!(
        !out.stderr_str().contains(".pbfhogg-run-"),
        "{}",
        out.stderr_str()
    );
    drop(lock);
}
