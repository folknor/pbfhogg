//! Own process: this test deliberately lowers its soft descriptor limit.
//! The scenarios share that process-wide limit, so they run in sequence
//! inside one test function rather than as concurrent libtest tests.
mod common;

fn external_join(
    host: &pbfhogg::host::Host,
    input: &std::path::Path,
    output: &std::path::Path,
) -> Result<(), String> {
    pbfhogg::altw::external::external_join(
        input,
        output,
        true,
        &Default::default(),
        &Default::default(),
        false,
        host,
    )
    .map(drop)
    .map_err(|e| e.to_string())
}

#[test]
fn no_process_actions_leave_rlimit_nofile_unchanged() {
    use pbfhogg::host::{Host, NoProcessActions};
    use std::sync::Arc;
    let dir = tempfile::tempdir().expect("directory");
    let input = dir.path().join("base.osm.pbf");
    common::write_base_pbf(&input);
    // Not common::test_host(): that raises the descriptor limit this test
    // must see unraised.
    let host = Host {
        diagnostics: pbfhogg::host::Diagnostics::discard(),
        process: Arc::new(NoProcessActions),
        resources: Arc::new(common::fixed_resources()),
        page_cache: pbfhogg::host::PageCachePolicy::Keep,
        executor: pbfhogg::host::Executor::Ambient,
        scratch_parent: None,
    };
    let set_soft = |soft: u64| {
        // SAFETY: this standalone test owns its process and initializes the output.
        unsafe {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
            limit.rlim_cur = soft;
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
            limit.rlim_max
        }
    };
    let get = || {
        let mut after = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: the output structure is initialized and exclusively borrowed.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut after) },
            0
        );
        (after.rlim_cur, after.rlim_max)
    };
    // The scenarios set soft limits up to 1024.
    if get().1 < 1024 {
        eprintln!("Skipping: hard RLIMIT_NOFILE below 1024");
        return;
    }

    // Scenario 0: only descriptors numbered below the cutoff are counted.
    // Descriptors are allocated lowest-free-first and nothing else in this
    // process opens files concurrently, so every number below the highest
    // held one is occupied once these are open (inherited descriptors may
    // sit between held ones) and the count's own directory handle lands
    // above them.
    #[cfg(target_os = "linux")]
    {
        use pbfhogg::process::{FdCount, count_open_descriptors};
        use std::os::fd::AsRawFd as _;
        let held: Vec<std::fs::File> = (0..8)
            .map(|_| std::fs::File::open("/dev/null").expect("open /dev/null"))
            .collect();
        let fds: Vec<u64> = held
            .iter()
            .map(|f| u64::try_from(f.as_raw_fd()).expect("non-negative fd"))
            .collect();
        let lo = *fds.iter().min().expect("held");
        let hi = *fds.iter().max().expect("held") + 1;
        let count = |limit| match count_open_descriptors(limit) {
            FdCount::Counted(n) => n,
            other => panic!("procfs must be countable on Linux, got {other:?}"),
        };
        assert_eq!(count(0), 0);
        assert_eq!(count(hi) - count(lo), hi - lo, "held fds {fds:?}");
        assert!(count(hi) - count(lo) >= 8, "held fds {fds:?}");
        assert_eq!(count(hi), hi, "every number below {hi} is occupied");
        assert!(
            count(u64::MAX) > count(hi),
            "the count's own handle is above"
        );
        drop(held);
    }

    // Scenario 1: the run probes the limit and leaves it as it found it.
    let hard = set_soft(1024);
    external_join(&host, &input, &dir.path().join("external.pbf")).expect("external");
    assert_eq!(get(), (1024, hard));

    // Scenario 2: descriptors the process already holds come out of the
    // allowance. 600 would fit one 256-shard worker plus headroom if the
    // held files were ignored, and the run would then die with EMFILE
    // partway through stage 1; counted, it fails up front instead. Linux
    // only: elsewhere the open descriptors are not counted.
    #[cfg(target_os = "linux")]
    {
        set_soft(600);
        let held: Vec<std::fs::File> = (0..400)
            .map(|_| std::fs::File::open("/dev/null").expect("open /dev/null"))
            .collect();
        let err = external_join(&host, &input, &dir.path().join("held.pbf"))
            .expect_err("400 held descriptors leave no room for a shard worker");
        assert!(
            err.contains("file descriptor limit too low") && err.contains("already open"),
            "expected the up-front allowance error, got: {err}"
        );
        drop(held);
        assert_eq!(get(), (600, hard));
        set_soft(1024);
    }
}
