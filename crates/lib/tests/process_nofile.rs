//! Own process: this test deliberately lowers its soft descriptor limit.
//! The scenarios share that process-wide limit, so they run in sequence
//! inside one test function rather than as concurrent libtest tests.

// As in `crates/lib/tests/all.rs`: `allow-expect-in-tests` does not reach the helpers of
// an integration-test crate.
#![allow(
    clippy::expect_used,
    reason = "an integration-test crate; a failed expect is a test failure"
)]

mod common;

#[test]
fn injected_descriptor_counts_refuse_external_join_without_procfs() {
    use pbfhogg::host::{Host, ProcessActions};
    use pbfhogg::process::{FdAllowance, FdCount, FdProbe, FdReservation};
    use std::sync::Arc;

    struct InjectedCount(FdProbe);
    impl ProcessActions for InjectedCount {
        fn fd_allowance(&self, _wanted: u64) -> FdReservation {
            FdReservation::unreserved(FdAllowance::from_probe(self.0))
        }
    }

    let dir = common::test_scratch();
    let input = dir.path().join("base.osm.pbf");
    common::write_base_pbf(&input);
    // Inject at the existing provider boundary: neither procfs nor an actual
    // descriptor shortage is needed, and no process limit is changed.
    for (index, (soft_limit, open, available, message)) in [
        (
            600,
            FdCount::Counted(400),
            200,
            "400 already open, 200 available",
        ),
        (
            600,
            FdCount::Counted(601),
            0,
            "601 already open, 0 available",
        ),
        (
            128,
            FdCount::Uncounted,
            128,
            "file descriptor limit too low",
        ),
        (
            600,
            FdCount::Failed(libc::EIO),
            0,
            "counting the process's open file descriptors failed",
        ),
        (
            600,
            FdCount::Exhausted(libc::EMFILE),
            0,
            "the process already holds every descriptor",
        ),
        (
            600,
            FdCount::Exhausted(libc::ENFILE),
            0,
            "the system-wide file table is full",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let probe = FdProbe {
            soft_limit,
            adjustment: None,
            open,
            hard_limit: Some(soft_limit),
        };
        let allowance = FdAllowance::from_probe(probe);
        assert_eq!(allowance.descriptors, available, "{open:?}");
        assert_eq!(allowance.probe, Some(probe));
        let host = Host {
            diagnostics: pbfhogg::host::Diagnostics::discard(),
            process: Arc::new(InjectedCount(probe)),
            resources: Arc::new(common::fixed_resources()),
            page_cache: pbfhogg::host::PageCachePolicy::Keep,
            executor: pbfhogg::host::Executor::Ambient,
            invocation: pbfhogg::host::Invocation::new(),
        };
        let output = dir.path().join(format!("refused-{index}.pbf"));
        let error = external_join(&host, &input, &output)
            .expect_err("injected allowance cannot hold a shard worker");
        assert!(error.contains(message), "{open:?}: {error}");
    }
}

fn external_join(
    host: &pbfhogg::host::Host,
    input: &std::path::Path,
    output: &std::path::Path,
) -> Result<(), String> {
    pbfhogg::altw::external::external_join(
        input,
        crate::common::out(output),
        true,
        &Default::default(),
        &Default::default(),
        false,
        host,
        // Made from the test's own host: `common::test_host` raises the soft
        // limit this test is measuring.
        pbfhogg::scratch::RunScratch::create(
            output.parent().unwrap_or(std::path::Path::new(".")),
            host,
        )
        .map_err(|e| e.to_string())?,
    )
    .map(drop)
    .map_err(|e| e.to_string())
}

#[test]
fn no_process_actions_leave_rlimit_nofile_unchanged() {
    use pbfhogg::host::{Host, NoProcessActions};
    use std::sync::Arc;
    let dir = common::test_scratch();
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
        invocation: pbfhogg::host::Invocation::new(),
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
    // Counting is independent of the descriptor budget scenarios. Compare a
    // controlled delta rather than assuming inherited descriptor numbers have
    // no holes. The counter's own directory handle is present in both counts.
    #[cfg(target_os = "linux")]
    let countable = {
        use pbfhogg::process::{FdCount, count_open_descriptors};
        match count_open_descriptors(u64::MAX) {
            FdCount::Counted(before) if get().0.saturating_sub(before) >= 8 => {
                let held: Vec<_> = (0..8)
                    .map(|_| std::fs::File::open(&input).expect("open fixture"))
                    .collect();
                assert!(matches!(count_open_descriptors(0), FdCount::Counted(0)));
                match count_open_descriptors(u64::MAX) {
                    FdCount::Counted(after) => assert_eq!(after, before + 8),
                    other => panic!("count became unavailable with eight held files: {other:?}"),
                }
                drop(held);
                true
            }
            other => {
                common::skip(format!(
                    "descriptor counting unavailable or fewer than eight free slots: {other:?}"
                ));
                false
            }
        }
    };

    // Scenario 1: the run probes the limit and leaves it as it found it.
    let original = get();
    let hard = original.1;
    if hard >= 1024 {
        set_soft(1024);
        external_join(&host, &input, &dir.path().join("external.pbf")).expect("external");
        assert_eq!(get(), (1024, hard));
    } else {
        common::skip("unchanged-limit scenario needs hard RLIMIT_NOFILE >= 1024");
    }

    // Scenario 2: descriptors the process already holds come out of the
    // allowance. 600 would fit one 256-shard worker plus headroom if the
    // held files were ignored, and the run would then die with EMFILE
    // partway through stage 1; counted, it fails up front instead. Linux
    // only: elsewhere the open descriptors are not counted.
    #[cfg(target_os = "linux")]
    if countable && hard >= 600 {
        set_soft(600);
        let held = (0..400)
            .map(|_| std::fs::File::open(&input))
            .collect::<std::io::Result<Vec<_>>>();
        let held = match held {
            Ok(held) => held,
            Err(error) if error.raw_os_error() == Some(libc::EMFILE) => {
                common::skip("inherited descriptors leave fewer than 400 free slots");
                set_soft(original.0);
                return;
            }
            Err(error) => panic!("open held fixture: {error}"),
        };
        let err = external_join(&host, &input, &dir.path().join("held.pbf"))
            .expect_err("400 held descriptors leave no room for a shard worker");
        assert!(
            err.contains("file descriptor limit too low") && err.contains("already open"),
            "expected the up-front allowance error, got: {err}"
        );
        drop(held);
        assert_eq!(get(), (600, hard));
    } else {
        common::skip("held-descriptor scenario needs counting and hard RLIMIT_NOFILE >= 600");
    }
    set_soft(original.0);
}
