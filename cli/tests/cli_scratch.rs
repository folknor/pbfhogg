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

use crate::invoker::CliInvoker;

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
    let dir = crate::common::test_scratch();
    let missing_input = dir.path().join("no-such-input.osm.pbf");
    let missing_other = dir.path().join("no-such-other.osm.pbf");
    let tmp = dir.path().join("no-such-dir");
    let output = dir.path().join("out");
    // An OSC output must carry one of the two accepted suffixes, or the name
    // is refused before the --tmp-dir check this test is about.
    let osc_output = dir.path().join("out.osc");
    let (input, other, tmp_arg, output_arg, osc_arg) = (
        missing_input.to_str().unwrap(),
        missing_other.to_str().unwrap(),
        tmp.to_str().unwrap(),
        output.to_str().unwrap(),
        osc_output.to_str().unwrap(),
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
        vec![
            "diff",
            input,
            other,
            "--format",
            "osc",
            "-o",
            osc_arg,
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

    // A text diff writes no scratch at any -j, so it refuses --tmp-dir
    // outright, also before any input is read.
    let out = CliInvoker::new()
        .args(["diff", input, other, "--tmp-dir", tmp_arg])
        .assert_failure();
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("--tmp-dir is only valid with --format osc"),
        "{stderr}"
    );
    assert!(!stderr.contains("no-such-input"), "{stderr}");
}

#[test]
fn a_tmp_dir_that_is_a_file_or_unwritable_is_refused() {
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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

/// The altw case above can fail before a scratch file exists. An OSC diff
/// cannot: it creates its per-action fragments before it decodes anything,
/// so a last blob that is valid framing around an undecodable payload fails
/// the run with scratch files already on disk.
#[test]
fn a_run_that_fails_after_writing_scratch_leaves_none_behind() {
    let dir = crate::common::test_scratch();
    let work = dir.path().join("work");
    let tmp = dir.path().join("tmp");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&tmp).unwrap();
    let old = work.join("old.osm.pbf");
    let new = work.join("new.osm.pbf");
    crate::common::write_base_pbf(&old);
    crate::common::write_changed_pbf(&new);
    let bytes = std::fs::read(&new).unwrap();
    let last = crate::common::adversarial::locate_blobs(&bytes).len() - 1;
    let corrupt = crate::common::adversarial::mutate_blob_payload(&bytes, last, |payload| {
        *payload = vec![0xff; 16];
    });
    std::fs::write(&new, corrupt).unwrap();
    let output = work.join("out.osc");

    for tmp_args in [vec![], vec!["--tmp-dir", tmp.to_str().unwrap()]] {
        let mut args = vec![
            "diff",
            "--format",
            "osc",
            "-j",
            "2",
            old.to_str().unwrap(),
            new.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ];
        args.extend(tmp_args);
        let out = CliInvoker::new().args(&args).run();
        assert_eq!(out.status.code(), Some(2), "{}", out.stderr_str());
        assert!(names(&tmp).is_empty(), "{:?}", names(&tmp));
        assert_eq!(
            names(&work),
            ["new.osm.pbf", "old.osm.pbf"],
            "nothing may be left beside the output"
        );
    }
}

/// A run whose command line guarantees no scratch makes no run directory, so
/// a current directory it cannot write is not an obstacle (a text diff, at
/// any `-j`).
#[test]
fn a_scratch_free_diff_needs_no_writable_directory() {
    use std::os::unix::fs::PermissionsExt;
    // Root writes anywhere, so the read-only directory proves nothing there.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    crate::common::write_base_pbf(&old);
    crate::common::write_changed_pbf(&new);
    let cwd = dir.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o500)).unwrap();
    for args in [
        vec!["-j", "1"],
        vec!["-j", "1", "-q"],
        vec!["-v", "-j", "1"],
        vec!["-q"],
        vec!["-q", "-j", "2"],
        vec!["-j", "2"],
        vec!["-v"],
    ] {
        let out = CliInvoker::new()
            .current_dir(&cwd)
            .args(
                ["diff"]
                    .into_iter()
                    .chain(args)
                    .chain([old.to_str().unwrap(), new.to_str().unwrap()]),
            )
            .run();
        assert!(
            out.status.code().is_some_and(|c| c <= 1),
            "{}",
            out.stderr_str()
        );
    }
    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn successful_runs_clean_up_after_themselves() {
    let dir = crate::common::test_scratch();
    let tmp = dir.path().join("tmp");
    std::fs::create_dir(&tmp).unwrap();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    crate::common::write_base_pbf(&old);
    crate::common::write_changed_pbf(&new);
    let tmp_arg = tmp.to_str().unwrap();

    let osc = dir.path().join("delta.osc.gz");
    // The inputs differ, so the OSC diff completes with exit 1.
    let out = CliInvoker::new()
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
        .run();
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr_str());
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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

fn occurrences(haystack: &str, needle: &Path) -> usize {
    haystack.matches(&needle.display().to_string()).count()
}

#[test]
fn extract_config_scans_every_destination_directory_once() {
    let dir = crate::common::test_scratch();
    let (first, second) = (dir.path().join("first"), dir.path().join("second"));
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let input = dir.path().join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let config = dir.path().join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[
                {{"output":"first/a.osm.pbf","bbox":[0.0,0.0,1.0,1.0]}},
                {{"output":"first/b.osm.pbf","bbox":[0.0,0.0,1.0,1.0]}},
                {{"output":"second/c.osm.pbf","bbox":[0.0,0.0,1.0,1.0]}}]}}"#,
            dir.path().display()
        ),
    )
    .unwrap();
    let in_first = first.join(".pbfhogg-tmp-999999-0.a.osm.pbf");
    let in_second = second.join(".pbfhogg-tmp-999999-1.c.osm.pbf");
    std::fs::write(&in_first, vec![3_u8; 64]).unwrap();
    std::fs::write(&in_second, vec![4_u8; 64]).unwrap();

    let out = CliInvoker::new()
        .args([
            "extract",
            input.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--simple",
        ])
        .run();
    let stderr = out.stderr_str();
    assert!(out.status.success(), "{stderr}");
    for name in ["a.osm.pbf", "b.osm.pbf", "c.osm.pbf"] {
        let dest = if name == "c.osm.pbf" { &second } else { &first };
        assert!(dest.join(name).exists(), "{name} published\n{stderr}");
    }
    assert_eq!(occurrences(&stderr, &in_first), 1, "{stderr}");
    assert_eq!(occurrences(&stderr, &in_second), 1, "{stderr}");
    // The scan runs before this run reserves its own outputs: had it run
    // after, this run's own staging would be reported beside the planted
    // entries.
    assert_eq!(stderr.matches(".pbfhogg-tmp-").count(), 2, "{stderr}");
    assert_eq!(std::fs::read(&in_first).unwrap(), vec![3_u8; 64]);
    assert_eq!(std::fs::read(&in_second).unwrap(), vec![4_u8; 64]);
}

/// Where older versions put their scratch: the text diff's `diff-par-*`
/// shards beside the old input, derive's `derive-*` fragments beside the
/// output (`--format osc`).
#[test]
fn diff_reports_what_older_versions_left_where_they_left_it() {
    let dir = crate::common::test_scratch();
    let (inputs, out_dir, tmp) = (
        dir.path().join("inputs"),
        dir.path().join("out"),
        dir.path().join("tmp"),
    );
    for d in [&inputs, &out_dir, &tmp] {
        std::fs::create_dir(d).unwrap();
    }
    let old = inputs.join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    crate::common::write_base_pbf(&old);
    crate::common::write_changed_pbf(&new);
    let shard = inputs.join("diff-par-999999-n-0.txt.tmp");
    let derive = out_dir.join("derive-creates-999999.xml.tmp");
    let unrelated = inputs.join("diff-par-notes.txt");
    std::fs::write(&shard, vec![5_u8; 128]).unwrap();
    std::fs::write(&derive, vec![6_u8; 128]).unwrap();
    std::fs::write(&unrelated, b"mine").unwrap();

    for osc in [false, true] {
        let mut args = vec!["diff", old.to_str().unwrap(), new.to_str().unwrap()];
        let output = out_dir.join(format!("delta-{osc}.osc.gz"));
        if osc {
            // Only the osc form writes scratch, so only it takes --tmp-dir.
            args.extend([
                "--format",
                "osc",
                "-o",
                output.to_str().unwrap(),
                "--tmp-dir",
                tmp.to_str().unwrap(),
            ]);
        }
        let stderr = CliInvoker::new().args(&args).run().stderr_str();
        assert_eq!(occurrences(&stderr, &shard), 1, "osc={osc}: {stderr}");
        // Only the osc form writes beside an output.
        assert_eq!(
            occurrences(&stderr, &derive),
            usize::from(osc),
            "osc={osc}: {stderr}"
        );
        assert!(!stderr.contains("diff-par-notes"), "{stderr}");
    }
    assert_eq!(std::fs::read(&shard).unwrap(), vec![5_u8; 128]);
    assert_eq!(std::fs::read(&derive).unwrap(), vec![6_u8; 128]);
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"mine");
}

#[test]
fn geocode_reports_old_scratch_inside_the_index_and_in_old_staging() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let index = dir.path().join("index");
    let buckets = index.join(".buckets-level17");
    std::fs::create_dir_all(&buckets).unwrap();
    std::fs::write(buckets.join("bucket"), vec![9_u8; 256]).unwrap();
    std::fs::write(index.join("geocode_header.bin"), b"index").unwrap();
    // Siblings of the index: the staging of the version before this one
    // (holding the previous index, so a backup), and this version's own
    // staging directory with scratch inside it, which is one entry.
    let swap = dir.path().join(".geocode-build-7");
    std::fs::create_dir_all(swap.join("old")).unwrap();
    std::fs::write(swap.join("old").join("geocode_header.bin"), b"previous").unwrap();
    let staging = dir.path().join(".pbfhogg-tmp-999999-0.index");
    std::fs::create_dir_all(staging.join("new").join(".buckets-level17")).unwrap();

    // Without --overwrite the build is refused; the scan comes first.
    let stderr = CliInvoker::new()
        .args([
            "build-geocode-index",
            input.to_str().unwrap(),
            "--output-dir",
            index.to_str().unwrap(),
            "--allow-unindexed",
        ])
        .assert_failure()
        .stderr_str();
    assert_eq!(occurrences(&stderr, &buckets), 1, "{stderr}");
    assert!(stderr.contains("deletes the old index"), "{stderr}");
    assert_eq!(occurrences(&stderr, &swap), 1, "{stderr}");
    assert!(stderr.contains("only copy"), "{stderr}");
    assert_eq!(occurrences(&stderr, &staging), 1, "{stderr}");
    assert!(
        !stderr.contains(&staging.join("new").display().to_string()),
        "a staging directory is one entry: {stderr}"
    );
    assert_eq!(
        std::fs::read(buckets.join("bucket")).unwrap(),
        vec![9_u8; 256]
    );
    assert_eq!(
        std::fs::read(swap.join("old").join("geocode_header.bin")).unwrap(),
        b"previous"
    );
}

/// `--overwrite` replaces an index wholesale, so an index holding anything
/// pbfhogg never deletes - another output's backup, a run directory that
/// may be live - is refused before any work, and nothing in it is touched.
#[test]
fn an_index_holding_leftovers_is_not_replaced() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    crate::common::write_synthetic_indexed_input(&input);
    let index = dir.path().join("index");
    std::fs::create_dir(&index).unwrap();
    std::fs::write(index.join("geocode_header.bin"), b"index").unwrap();
    let backup = index.join(".pbfhogg-backup-999999-0.other.osm.pbf");
    std::fs::create_dir(&backup).unwrap();
    std::fs::write(backup.join("other.osm.pbf"), b"only copy").unwrap();
    let run = plant_dead_run_dir(&index);

    let stderr = CliInvoker::new()
        .args([
            "build-geocode-index",
            input.to_str().unwrap(),
            "--output-dir",
            index.to_str().unwrap(),
            "--allow-unindexed",
            "--overwrite",
        ])
        .assert_failure()
        .stderr_str();
    assert!(stderr.contains("cannot be replaced"), "{stderr}");
    assert!(stderr.contains(&backup.display().to_string()), "{stderr}");
    assert!(stderr.contains(&run.display().to_string()), "{stderr}");
    assert_eq!(
        std::fs::read(backup.join("other.osm.pbf")).unwrap(),
        b"only copy"
    );
    assert_eq!(
        std::fs::read(index.join("geocode_header.bin")).unwrap(),
        b"index"
    );
    assert!(run.join("id-W0-000").exists());
    let siblings: Vec<_> = names(dir.path())
        .into_iter()
        .filter(|n| n.starts_with(".pbfhogg-"))
        .collect();
    assert!(siblings.is_empty(), "{siblings:?}");
}

#[test]
fn a_backup_that_cannot_be_removed_is_reported_by_the_cli() {
    // Root ignores directory permissions, so the removal cannot fail there.
    if is_root() {
        return;
    }
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    // The builder refuses an input with nothing to index.
    crate::common::write_synthetic_indexed_input(&input);
    let index = dir.path().join("index");
    let locked = index.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(index.join("geocode_header.bin"), b"index").unwrap();
    std::fs::write(locked.join("f"), b"x").unwrap();
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o500)).unwrap();

    let out = CliInvoker::new()
        .args([
            "build-geocode-index",
            input.to_str().unwrap(),
            "--output-dir",
            index.to_str().unwrap(),
            "--allow-unindexed",
            "--overwrite",
        ])
        .run();
    let stderr = out.stderr_str();
    // Put the permissions back before anything can fail the test.
    let backups: Vec<_> = names(dir.path())
        .into_iter()
        .filter(|n| n.starts_with(".pbfhogg-backup-"))
        .collect();
    for backup in &backups {
        let leftover = dir.path().join(backup).join("index").join("locked");
        std::fs::set_permissions(
            &leftover,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .ok();
    }
    assert!(
        out.status.success(),
        "the replacement itself succeeded: {stderr}"
    );
    assert_eq!(backups.len(), 1, "{backups:?}\n{stderr}");
    assert!(stderr.contains("Warning: "), "{stderr}");
    assert!(stderr.contains("removing the backup"), "{stderr}");
    assert!(stderr.contains(&backups[0]), "{stderr}");
}
