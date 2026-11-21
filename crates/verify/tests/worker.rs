//! Rust-only lifecycle adversary. No role depends on the host environment.
#![allow(dead_code, clippy::unwrap_used)]
#[path = "../src/error.rs"]
pub(crate) mod error;
#[path = "../src/process.rs"]
pub(crate) mod process;
#[path = "../src/tunables.rs"]
pub(crate) mod tunables;

#[path = "../src/output.rs"]
pub(crate) mod output;
#[path = "../src/reference.rs"]
pub(crate) mod reference;
#[path = "../src/reference_cache.rs"]
pub(crate) mod reference_cache;
#[path = "../src/run.rs"]
pub(crate) mod run;

#[path = "../src/argv.rs"]
pub(crate) mod argv;
#[path = "../src/checks/mod.rs"]
pub(crate) mod checks;
#[path = "dedupe_fixture.rs"]
mod dedupe_fixture;
#[path = "extract_fixture.rs"]
mod extract_fixture;
#[path = "fake_pbf.rs"]
pub(crate) mod fake_pbf;
#[path = "getid_fixture.rs"]
mod getid_fixture;
#[path = "../src/harness.rs"]
pub(crate) mod harness;
#[path = "../src/opl_compare.rs"]
pub(crate) mod opl_compare;
#[path = "../src/osc.rs"]
pub(crate) mod osc;
#[path = "../src/pbf_frames.rs"]
pub(crate) mod pbf_frames;
#[path = "read_fixture.rs"]
mod read_fixture;
#[path = "refs_fixture.rs"]
mod refs_fixture;
#[path = "../src/stream.rs"]
pub(crate) mod stream;
#[path = "../src/verify_getid_refs.rs"]
pub(crate) mod verify_getid_refs;
#[path = "../src/verify_input_order.rs"]
pub(crate) mod verify_input_order;
#[path = "../src/verify_opl.rs"]
pub(crate) mod verify_opl;

#[path = "../src/verify_fileinfo.rs"]
pub(crate) mod verify_fileinfo;

use std::io::Write;
use std::path::Path;

#[path = "apply_fixture.rs"]
pub(crate) mod apply_fixture;
#[path = "encoding_fixture.rs"]
mod encoding_fixture;

#[path = "diff_fixture.rs"]
mod diff_fixture;

/// How one check run under [`judge`] ended: its status, the comparisons it
/// recorded as failed (in order, duplicates kept) and the error its closure
/// returned, if any.
#[derive(Debug)]
pub(crate) struct Judged {
    pub status: harness::CheckStatus,
    pub failed: Vec<String>,
    pub error: Option<String>,
}

/// Run `check` as `name` and report which comparisons failed, so a fault
/// aimed at one arm of a check is held to failing that arm and no other: an
/// arm on the wrong field policy shows as a missing name, which a final
/// status alone cannot show while another arm fails.
pub(crate) fn judge(
    h: &mut harness::Harness,
    name: &str,
    check: impl FnOnce(&mut harness::Harness) -> Result<Option<String>, error::Error>,
) -> Judged {
    let mut failed = Vec::new();
    let mut error = None;
    let status = h
        .run_check(name, |h| {
            let result = check(h);
            failed = h.failures().to_vec();
            if let Err(e) = &result {
                error = Some(e.to_string());
            }
            result
        })
        .unwrap();
    Judged {
        status,
        failed,
        error,
    }
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let role = args[1].to_str().unwrap();
    if role == "--version" {
        println!("fixture-reference-v1");
    } else if role == "sort"
        && args
            .iter()
            .any(|a| a.to_string_lossy().contains("boundary-fixture"))
    {
        boundary_tool(&args);
    } else if args[1..].iter().any(|a| {
        Path::new(a)
            .ancestors()
            .any(|p| p.file_name().is_some_and(|n| n == "diff-fixture"))
    }) && role != "driver"
    {
        std::process::exit(diff_fixture::tool(&args[1..]));
    } else if args
        .iter()
        .any(|a| a.to_string_lossy().contains("apply-fixture"))
        && role != "driver"
    {
        std::process::exit(apply_fixture::tool(&args[1..]));
    } else if args[1..].iter().any(|a| {
        Path::new(a)
            .ancestors()
            .any(|p| p.file_name().is_some_and(|n| n == "change-fixture"))
    }) && role != "driver"
    {
        std::process::exit(change_fixture::tool(&args[1..]));
    } else if role == "cache-copy" {
        let input = Path::new(&args[2]);
        let count = input.with_extension("count");
        let previous = std::fs::read_to_string(&count).unwrap_or_default();
        std::fs::write(count, format!("{previous}x")).unwrap();
        if args.get(4).is_none_or(|a| a != "omit") {
            std::fs::copy(input, &args[3]).unwrap();
        }
        if args.get(4).is_some_and(|a| a == "mutate") {
            std::fs::write(input, b"changed").unwrap();
        }
        if args.get(4).is_some_and(|a| a == "fail") {
            std::process::exit(1);
        }
    } else if args[1..].iter().any(|a| {
        Path::new(a)
            .ancestors()
            .any(|p| p.file_name().is_some_and(|n| n == "encoding-fixture"))
    }) {
        std::process::exit(encoding_fixture::tool(&args[1..]));
    } else if args[1..].iter().any(|a| {
        Path::new(a)
            .ancestors()
            .any(|p| p.file_name().is_some_and(|n| n == "extract-fixture"))
    }) {
        std::process::exit(extract_fixture::tool(&args[1..]));
    } else if args[1..].iter().any(|a| {
        Path::new(a)
            .ancestors()
            .any(|p| p.file_name().is_some_and(|n| n == "dedupe-fixture"))
    }) && role != "driver"
    {
        std::process::exit(dedupe_fixture::tool(&args[1..]));
    } else if args[1..].iter().any(|a| {
        Path::new(a)
            .ancestors()
            .any(|p| p.file_name().is_some_and(|n| n == "getid-fixture"))
    }) && role != "driver"
    {
        std::process::exit(getid_fixture::tool(&args[1..]));
    } else if args[1..].iter().any(|a| {
        Path::new(a)
            .ancestors()
            .any(|p| p.file_name().is_some_and(|n| n == "read-fixture"))
    }) {
        std::process::exit(read_fixture::tool(&args[1..]));
    } else if ["getid", "cat", "fileinfo"].contains(&role) {
        std::process::exit(refs_fixture::tool(&args[1..]));
    } else if role == "driver" {
        driver(args[2].to_str().unwrap(), Path::new(&args[3]));
    } else {
        worker(role);
    }
}

fn no_children() {
    let mut status = 0;
    // SAFETY: writable status; this worker owns every child it starts.
    assert_eq!(unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

fn driver(role: &str, dir: &Path) {
    let mut lifecycle = process::Lifecycle::install().unwrap();
    if role == "parent-death" {
        parent_death();
        return;
    }
    let exe = std::env::current_exe().unwrap();
    if role == "diff-family" {
        diff_fixture::driver(lifecycle, &exe, dir);
        no_children();
        return;
    }
    if let Some(fault) = role.strip_prefix("apply=") {
        apply_fixture::driver(lifecycle, &exe, dir, fault);
        no_children();
        return;
    } else if role == "encoding" {
        encoding_fixture::driver(lifecycle, &exe, dir);
        no_children();
        return;
    }
    if role == "extract" {
        extract_fixture::driver(lifecycle, &exe, dir);
        no_children();
        return;
    }
    if role == "change" {
        change_fixture::driver(lifecycle, &exe, dir);
        return;
    }
    if role == "read" {
        read_fixture::driver(lifecycle, &exe, dir);
        no_children();
        return;
    }
    if role == "dedupe" {
        dedupe_fixture::driver(lifecycle, &exe, dir);
        no_children();
        return;
    }
    if role == "getid" {
        getid_fixture::driver(lifecycle, &exe, dir);
        no_children();
        return;
    }
    if role == "refs" {
        refs_fixture::driver(lifecycle, &exe, dir);
        no_children();
        return;
    }
    if role == "cache" {
        cache_driver(&mut lifecycle, &exe, dir);
        no_children();
        return;
    }
    let sink = dir.join("stdout");
    let result = lifecycle.run(&process::Launch {
        program: &exe,
        args: vec![role.into()],
        env: vec![],
        cwd: dir,
        sink: &sink,
    });
    match role {
        "orphan" | "chain" => assert!(
            matches!(result, Err(error::Error::Environment(_))),
            "{result:?}"
        ),
        "interrupt" => {
            assert!(
                matches!(result, Err(error::Error::Interrupted)),
                "{result:?}"
            );
            assert!(matches!(
                lifecycle.checkpoint(),
                Err(error::Error::Interrupted)
            ));
            // Cancellation is sticky and prevents any later launch.
            assert!(matches!(
                lifecycle.run(&process::Launch {
                    program: &exe,
                    args: vec!["armed".into()],
                    env: vec![],
                    cwd: dir,
                    sink: &sink,
                }),
                Err(error::Error::Interrupted)
            ));
        }
        "overflow" => assert!(
            matches!(result, Err(error::Error::Tool(ref why)) if why.contains("stderr exceeded")),
            "{result:?}"
        ),
        "armed" => assert!(result.unwrap().status.success()),
        "wrong-parent" => assert_eq!(result.unwrap().code(), Some(127)),
        _ => panic!("unknown driver role"),
    }
    no_children();
}

fn parent_death() {
    use std::os::unix::process::ExitStatusExt;
    // This process is already a subreaper. After the driver dies its child
    // is adopted here, so we can prove the backstop killed it and reap it.
    let status = descendant("dying-parent").wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    let mut status = 0;
    // SAFETY: writable status; the only remaining child is the adopted worker.
    // wait-deliberate: PDEATHSIG must kill it; the outer test's HangBound
    // aborts the test if that contract breaks.
    assert!(unsafe { libc::waitpid(-1, &mut status, 0) } > 0);
    assert!(libc::WIFSIGNALED(status));
    assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    no_children();
}

fn descendant(role: &str) -> std::process::Child {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = std::process::Command::new(exe);
    cmd.env_clear()
        .arg(role)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().unwrap()
}

#[expect(
    clippy::zombie_processes,
    reason = "the lifecycle must kill and reap deliberately abandoned descendants"
)]
fn worker(role: &str) {
    match role {
        "parked" => std::thread::park(),
        "dying-parent" => {
            let mut lifecycle = process::Lifecycle::install().unwrap();
            let exe = std::env::current_exe().unwrap();
            let _result = lifecycle.run(&process::Launch {
                program: &exe,
                args: vec!["kill-parent".into()],
                env: vec![],
                cwd: Path::new("."),
                sink: Path::new("death.stdout"),
            });
            panic!("driver survived its child's SIGKILL");
        }
        "kill-parent" => {
            // SAFETY: our parent is the sacrificial lifecycle driver.
            assert_eq!(unsafe { libc::kill(libc::getppid(), libc::SIGKILL) }, 0);
            std::thread::park();
        }
        "orphan" => {
            let _child = descendant("parked");
        }
        "chain" => {
            assert!(descendant("orphan").wait().unwrap().success());
        }
        "interrupt" => {
            let _child = descendant("parked");
            // SAFETY: the parent is the live lifecycle driver waiting for us.
            assert_eq!(unsafe { libc::kill(libc::getppid(), libc::SIGTERM) }, 0);
            std::thread::park();
        }
        "overflow" => {
            let bytes = [b'x'; 8192];
            // wait-deliberate: the lifecycle must reject overflow and kill us;
            // the parent test's fail-stop bound catches a broken implementation.
            loop {
                std::io::stderr().write_all(&bytes).unwrap();
            }
        }
        "armed" => {
            let mut signal = 0_i32;
            // SAFETY: prctl writes one integer to the live local.
            assert_eq!(
                unsafe { libc::prctl(libc::PR_GET_PDEATHSIG, &mut signal) },
                0
            );
            assert_eq!(signal, libc::SIGKILL);
            // SAFETY: sigprocmask writes the current mask to a live local.
            let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::sigprocmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask) },
                0
            );
            for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                // SAFETY: mask was filled above.
                assert_eq!(unsafe { libc::sigismember(&mask, signal) }, 0);
            }
        }
        "wrong-parent" => {
            // SAFETY: this worker is single-threaded; zero cannot be its parent.
            unsafe { process::prepare_child(0) };
            panic!("parent race check returned");
        }
        _ => panic!("unknown worker role"),
    }
}

/// Run the fixture twice in `mode`, a run that must not complete: each runs
/// afresh (an incomplete run is never published) and places nothing.
fn incomplete_runs(
    life: &mut process::Lifecycle,
    watched: &mut run::Run,
    tools: &reference::Tools,
    cache: &mut reference_cache::ReferenceCache,
    input: &Path,
    dir: &Path,
    mode: &str,
) -> Vec<reference_cache::Settled> {
    let mut settled = Vec::new();
    for n in 0..2 {
        let target = dir.join(format!("{mode}{n}"));
        std::fs::create_dir(&target).unwrap();
        let declared = reference::ToolRun::osmium(format!("{mode}.1"))
            .arg("cache-copy")
            .input(input)
            .output(&target.join("out.pbf"))
            .arg(mode);
        settled.push(
            cache
                .execute(life, watched, tools, &declared, &target.join("stdout"))
                .unwrap(),
        );
        assert!(!target.join("out.pbf").exists());
        assert!(!target.join("stdout").exists());
    }
    settled
}

fn cache_driver(life: &mut process::Lifecycle, exe: &Path, dir: &Path) {
    if !["/usr/bin/ldd", "/bin/ldd"]
        .iter()
        .any(|p| Path::new(p).is_file())
    {
        eprintln!("cache fixture skipped: ldd unavailable");
        return;
    }
    let mut watched = run::Run::create(dir, exe, true).unwrap();
    let input = dir.join("fixture.pbf");
    std::fs::write(&input, b"input").unwrap();
    let tools = reference::Tools {
        osmium: exe.to_path_buf(),
    };
    let mut cache = reference_cache::ReferenceCache::new(dir, "fixture-run".into(), false).unwrap();
    for (n, fresh, hit) in [(0, false, false), (1, false, true), (2, true, false)] {
        cache.fresh = fresh;
        let target = dir.join(format!("check{n}"));
        std::fs::create_dir(&target).unwrap();
        let output = target.join("out.pbf");
        let declared = reference::ToolRun::osmium("copy.1")
            .arg("cache-copy")
            .input(&input)
            .output(&output);
        let settled = cache
            .execute(
                life,
                &mut watched,
                &tools,
                &declared,
                &target.join("stdout"),
            )
            .unwrap();
        let reference_cache::Settled::Complete { ended, cached } = settled else {
            panic!("copy run did not complete: {settled:?}");
        };
        assert!(ended.status.success());
        assert_eq!(cached, hit);
        assert_eq!(std::fs::read(output).unwrap(), b"input");
    }
    assert_eq!(std::fs::read(input.with_extension("count")).unwrap(), b"xx");
    cache.fresh = false;
    for settled in incomplete_runs(life, &mut watched, &tools, &mut cache, &input, dir, "fail") {
        let reference_cache::Settled::Unaccepted { ended } = settled else {
            panic!("failing run was not unaccepted: {settled:?}");
        };
        assert_eq!(ended.code(), Some(1));
    }
    // An accepted exit that leaves its declared output missing is settled as
    // invalid, never as complete.
    for settled in incomplete_runs(life, &mut watched, &tools, &mut cache, &input, dir, "omit") {
        let reference_cache::Settled::InvalidOutputs { ended, invalid } = settled else {
            panic!("omitting run was not invalid: {settled:?}");
        };
        assert!(ended.status.success());
        assert_eq!(invalid, vec![("out.pbf".to_owned(), "missing")]);
    }
    assert_eq!(
        std::fs::read(input.with_extension("count")).unwrap(),
        b"xxxxxx"
    );
    let declared = reference::ToolRun::osmium("mutate.1")
        .arg("cache-copy")
        .input(&input)
        .output(&dir.join("moved.pbf"))
        .arg("mutate");
    assert!(matches!(
        cache.execute(
            life,
            &mut watched,
            &tools,
            &declared,
            &dir.join("mutated.stdout")
        ),
        Err(error::Error::Environment(_))
    ));
    // Only the completed unmutated run was published.
    let entries = std::fs::read_dir(&cache.dir)
        .unwrap()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .count();
    assert_eq!(entries, 1);
    // A later run that rewrote the same input path between runs misses:
    // the key holds the input's content, not its path.
    std::fs::write(&input, b"rewritten").unwrap();
    let mut later = run::Run::create(dir, exe, true).unwrap();
    let target = dir.join("rewritten");
    std::fs::create_dir(&target).unwrap();
    let output = target.join("out.pbf");
    let declared = reference::ToolRun::osmium("copy.1")
        .arg("cache-copy")
        .input(&input)
        .output(&output);
    let settled = cache
        .execute(life, &mut later, &tools, &declared, &target.join("stdout"))
        .unwrap();
    let reference_cache::Settled::Complete { ended, cached } = settled else {
        panic!("rewritten run did not complete: {settled:?}");
    };
    assert!(ended.status.success());
    assert!(!cached);
    assert_eq!(std::fs::read(output).unwrap(), b"rewritten");
}

#[path = "../src/counts.rs"]
pub(crate) mod counts;
#[path = "../src/extract_config.rs"]
pub(crate) mod extract_config;
#[path = "../src/verify_element_diff.rs"]
pub(crate) mod verify_element_diff;
#[path = "../src/verify_extract_simple.rs"]
pub(crate) mod verify_extract_simple;
#[path = "../src/verify_polygon.rs"]
pub(crate) mod verify_polygon;

#[path = "../src/diff_summary.rs"]
pub(crate) mod diff_summary;
#[path = "../src/verify_reencode.rs"]
pub(crate) mod verify_reencode;

#[path = "../src/verify_change_listing.rs"]
pub(crate) mod verify_change_listing;
#[path = "../src/verify_extsort.rs"]
pub(crate) mod verify_extsort;

#[path = "change_fixture.rs"]
mod change_fixture;

#[path = "../src/verify_diff_detail.rs"]
pub(crate) mod verify_diff_detail;
#[path = "../src/verify_diff_opl.rs"]
pub(crate) mod verify_diff_opl;

// A generated input selects only this test protocol, never a real dataset.
fn boundary_tool(args: &[std::ffi::OsString]) {
    let input = args
        .iter()
        .map(Path::new)
        .find(|p| p.file_name().is_some_and(|n| n == "input.pbf"))
        .unwrap();
    let mode = std::fs::read_to_string(input).unwrap();
    if mode == "interrupt" {
        std::fs::write(input.with_extension("ready"), "ready").unwrap();
        // SAFETY: pause has no arguments. The verifier's stop must kill this child.
        // wait-deliberate: the parent test sends SIGTERM after readiness; HangBound contains a regression.
        unsafe {
            libc::pause();
        }
    }
    std::process::exit(1);
}
