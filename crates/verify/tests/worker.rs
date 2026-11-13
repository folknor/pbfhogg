//! Rust-only lifecycle adversary. No role depends on the host environment.
#![allow(dead_code, clippy::unwrap_used)]
#[path = "../src/error.rs"]
pub(crate) mod error;
#[path = "../src/process.rs"]
pub(crate) mod process;
#[path = "../src/provision.rs"]
pub(crate) mod provision;
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
#[path = "extract_fixture.rs"]
mod extract_fixture;
#[path = "../src/harness.rs"]
pub(crate) mod harness;
#[path = "../src/inspect_report.rs"]
pub(crate) mod inspect_report;
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
mod apply_fixture;
#[path = "encoding_fixture.rs"]
mod encoding_fixture;

#[path = "diff_fixture.rs"]
mod diff_fixture;

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
    } else if role == "-q" {
        let dest = args.iter().position(|a| a == "-o").unwrap() + 1;
        std::fs::write(&args[dest], "abc").unwrap();
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
        std::fs::copy(input, &args[3]).unwrap();
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
            .any(|p| p.file_name().is_some_and(|n| n == "read-fixture"))
    }) {
        std::process::exit(read_fixture::tool(&args[1..]));
    } else if ["getid", "cat", "inspect", "diff"].contains(&role) {
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
    if role == "provision" {
        provision_driver(&mut lifecycle, &exe, dir);
        no_children();
        return;
    }
    if role == "diff-family" {
        diff_fixture::driver(lifecycle, &exe, dir);
        no_children();
        return;
    }
    if role == "apply" {
        apply_fixture::driver(lifecycle, &exe, dir);
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
        osmosis: None,
        osmconvert: None,
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
        let (ended, cached) = cache
            .execute(
                life,
                &mut watched,
                &tools,
                &declared,
                &target.join("stdout"),
            )
            .unwrap();
        assert!(ended.status.success());
        assert_eq!(cached, hit);
        assert_eq!(std::fs::read(output).unwrap(), b"input");
    }
    assert_eq!(std::fs::read(input.with_extension("count")).unwrap(), b"xx");
    cache.fresh = false;
    for n in 0..2 {
        let target = dir.join(format!("failed{n}"));
        std::fs::create_dir(&target).unwrap();
        let declared = reference::ToolRun::osmium("failed.1")
            .arg("cache-copy")
            .input(&input)
            .output(&target.join("out.pbf"))
            .arg("fail");
        let (ended, cached) = cache
            .execute(
                life,
                &mut watched,
                &tools,
                &declared,
                &target.join("stdout"),
            )
            .unwrap();
        assert_eq!(ended.code(), Some(1));
        assert!(!cached);
    }
    assert_eq!(
        std::fs::read(input.with_extension("count")).unwrap(),
        b"xxxx"
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

fn provision_driver(life: &mut process::Lifecycle, exe: &Path, dir: &Path) {
    let downloader = provision::Downloader {
        curl: exe.into(),
        tar: exe.into(),
        env: Vec::new(),
    };
    let archive = dir.join("archive");
    let pin = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    downloader
        .fetch(life, dir, "fixture-only", &archive, pin)
        .unwrap();
    assert!(
        downloader
            .fetch(life, dir, "fixture-only", &archive, "wrong")
            .is_err()
    );
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("bin")).unwrap();
    std::fs::create_dir_all(tree.join("jdk/bin")).unwrap();
    std::fs::write(tree.join("bin/osmosis"), "launcher").unwrap();
    std::fs::write(tree.join("jdk/bin/java"), "java").unwrap();
    let root = dir.join("installed");
    provision::complete(life, &tree, &root, pin).unwrap();
    assert!(!tree.exists());
    assert!(root.join("complete").is_file());
    std::fs::rename(&root, &tree).unwrap();
    // A pending stop must prevent publishing even a fully prepared tree.
    // SAFETY: sends SIGTERM to this worker, where Lifecycle owns its mask.
    assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
    assert!(matches!(
        provision::complete(life, &tree, &root, pin),
        Err(error::Error::Interrupted)
    ));
    assert!(!root.exists());
    assert!(tree.join("complete").is_file());
}

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
