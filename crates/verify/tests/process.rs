//! The lifecycle's signal mask and subreaper belong to a fresh process per case.
#![allow(clippy::unwrap_used)]
// Mount the worker just as the library verifier does, so the test-module
// audit includes its source graph. Its main is invoked only as a binary.
mod worker;
use worker::{
    argv, checks, counts, diff_summary, error, extract_config, harness, inspect_report, osc,
    output, pbf_frames, process, reference, reference_cache, run, stream, tunables,
    verify_change_listing, verify_diff_detail, verify_diff_opl, verify_element_diff,
    verify_extract_simple, verify_extsort, verify_fileinfo, verify_getid_refs, verify_input_order,
    verify_opl, verify_polygon, verify_reencode,
};

const EXE: &str = env!("CARGO_BIN_EXE_pbfhogg-verify-test-worker");

#[test]
fn verifier_lifecycle() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "verifier process lifecycle",
    );
    let scratch = pbfhogg_test_scratch::scratch();
    for role in [
        "cache",
        "orphan",
        "chain",
        "interrupt",
        "overflow",
        "armed",
        "wrong-parent",
        "parent-death",
    ] {
        let dir = scratch.cell();
        run_worker(role, &dir);
    }
}

fn run_worker(role: &str, dir: &std::path::Path) {
    let mut cmd = std::process::Command::new(EXE);
    cmd.env_clear()
        .arg("driver")
        .arg(role)
        .arg(dir)
        .current_dir(dir);
    let output = cmd.output().unwrap();
    assert!(output.status.success(), "{role}: {output:?}");
}

#[test]
fn verifier_shared_readers() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "referenced-object verifier behaviour",
    );
    let scratch = pbfhogg_test_scratch::scratch();
    run_worker("refs", scratch.path());
}

#[test]
fn verifier_read_checks() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "read verifier behaviour",
    );
    let scratch = pbfhogg_test_scratch::scratch();
    run_worker("read", scratch.path());
}

#[test]
fn verifier_extract_checks() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "extract verifier behaviour",
    );
    let scratch = pbfhogg_test_scratch::scratch();
    run_worker("extract", scratch.path());
}

#[test]
fn verifier_encoding_checks() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "encoding verifier behaviour",
    );
    let scratch = pbfhogg_test_scratch::scratch();
    run_worker("encoding", scratch.path());
}

#[test]
fn verifier_apply_changes() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "apply verifier",
    );
    let scratch = pbfhogg_test_scratch::scratch();
    run_worker("apply", scratch.path());
}

#[test]
fn verifier_provisioning() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "verifier provisioning",
    );
    let scratch = pbfhogg_test_scratch::scratch();
    run_worker("provision", scratch.path());
}

#[test]
fn verifier_change_checks() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "OSC verifier",
    );
    let scratch = pbfhogg_test_scratch::scratch();
    run_worker("change", scratch.path());
}

#[test]
fn verifier_diff_family() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "diff verifier behaviour",
    );
    let scratch = pbfhogg_test_scratch::scratch();
    run_worker("diff-family", scratch.path());
}

const VERIFIER: &str = env!("CARGO_BIN_EXE_pbfhogg-verify");

fn boundary_command(dir: &std::path::Path, mode: &str) -> std::process::Command {
    std::fs::create_dir_all(dir).unwrap();
    std::os::unix::fs::symlink(EXE, dir.join("osmium")).unwrap();
    std::fs::write(dir.join("input.pbf"), mode).unwrap();
    std::fs::write(
        dir.join("brokkr.toml"),
        "[host.fixture]\nscratch = 'scratch'\n",
    )
    .unwrap();
    let mut cmd = std::process::Command::new(VERIFIER);
    cmd.env_clear()
        .env("PATH", dir)
        .current_dir(dir)
        .arg("--host")
        .arg("fixture")
        .arg("--pbfhogg")
        .arg(EXE)
        .arg("sort")
        .arg("--input")
        .arg(dir.join("input.pbf"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd
}

#[test]
fn verifier_command_exit_boundaries() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "verifier command boundary",
    );
    let scratch = pbfhogg_test_scratch::scratch();
    let fail = scratch.cell().join("boundary-fixture");
    let output = boundary_command(&fail, "fail").output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("sort: FAIL"),
        "{output:?}"
    );
    let bad = scratch.cell().join("boundary-fixture");
    let mut cmd = boundary_command(&bad, "fail");
    std::fs::remove_file(bad.join("input.pbf")).unwrap();
    let output = cmd.output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stop = scratch.cell().join("boundary-fixture");
    let mut cmd = boundary_command(&stop, "interrupt");
    use std::os::unix::ffi::OsStrExt;
    let ready = stop.join("input.ready");
    let path = std::ffi::CString::new(ready.as_os_str().as_bytes()).unwrap();
    // SAFETY: path is NUL terminated; this private FIFO is owned by the test.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let child = cmd.spawn().unwrap();
    // wait-deliberate: FIFO open wakes on the child's writer; HangBound contains a regression.
    let _connection = std::fs::File::open(ready).unwrap();
    // SAFETY: child is live and owned here; SIGTERM is the signal brokkr kill forwards.
    assert_eq!(
        unsafe { libc::kill(i32::try_from(child.id()).unwrap(), libc::SIGTERM) },
        0
    );
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("sort: INTERRUPTED"),
        "{output:?}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("all done"));
}
