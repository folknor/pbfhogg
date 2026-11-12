//! Own process: this test deliberately lowers its soft descriptor limit.
//! The scenarios share that process-wide limit and the process's
//! descriptor table, so they run in sequence under one libtest test rather
//! than as concurrent tests.

// As in `crates/lib/tests/all.rs`: `allow-expect-in-tests` does not reach the helpers of
// an integration-test crate.
#![allow(
    clippy::expect_used,
    reason = "an integration-test crate; a failed expect is a test failure"
)]

mod common;

/// The only libtest test in this process: every scenario below opens
/// descriptors, and the last ones change the process-wide limit, so none
/// may overlap another.
#[test]
fn descriptor_limit_scenarios() {
    injected_descriptor_counts_refuse_external_join_without_procfs();
    no_process_actions_leave_rlimit_nofile_unchanged();
    #[cfg(all(target_os = "linux", pbfhogg_test_hooks))]
    the_largest_heavy_run_fits_its_descriptor_charge();
}

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
            executor: crate::common::test_executor(),
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
    external_join_keeping(host, input, output, true)
}

fn external_join_keeping(
    host: &pbfhogg::host::Host,
    input: &std::path::Path,
    output: &std::path::Path,
    keep_untagged_nodes: bool,
) -> Result<(), String> {
    pbfhogg::altw::external::external_join(
        input,
        crate::common::out(output),
        keep_untagged_nodes,
        &Default::default(),
        &Default::default(),
        false,
        host,
        // Made from the test's own host, whose provider is the one under test.
        pbfhogg::scratch::RunScratch::create(
            output.parent().unwrap_or(std::path::Path::new(".")),
            host,
        )
        .map_err(|e| e.to_string())?,
    )
    .map(drop)
    .map_err(|e| e.to_string())
}

fn no_process_actions_leave_rlimit_nofile_unchanged() {
    use pbfhogg::host::{Host, NoProcessActions};
    use std::sync::Arc;
    let dir = common::test_scratch();
    let input = dir.path().join("base.osm.pbf");
    common::write_base_pbf(&input);
    // Not common::test_host(): its fixed grants never probe the limit this
    // test measures.
    let host = Host {
        diagnostics: pbfhogg::host::Diagnostics::discard(),
        process: Arc::new(NoProcessActions),
        resources: Arc::new(common::fixed_resources()),
        page_cache: pbfhogg::host::PageCachePolicy::Keep,
        executor: crate::common::test_executor(),
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

/// The process's open descriptor numbers, read from `/proc/self/fd`, less
/// the listing's own handle: that one is closed once the listing drops, so
/// only the numbers still open afterwards are kept.
#[cfg(all(target_os = "linux", pbfhogg_test_hooks))]
fn open_descriptors() -> Option<Vec<i32>> {
    let numbers = std::fs::read_dir("/proc/self/fd")
        .ok()?
        .map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .collect::<Option<Vec<i32>>>()?;
    Some(
        numbers
            .into_iter()
            // SAFETY: F_GETFD only queries the descriptor number.
            .filter(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1)
            .collect(),
    )
}

/// Restores the soft descriptor limit it was made with, on every exit.
#[cfg(all(target_os = "linux", pbfhogg_test_hooks))]
struct SoftLimit(libc::rlimit);

#[cfg(all(target_os = "linux", pbfhogg_test_hooks))]
impl SoftLimit {
    fn current() -> Self {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: the output structure is initialized and exclusively borrowed.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        Self(limit)
    }

    fn set(&self, soft: u64) -> bool {
        let limit = libc::rlimit {
            rlim_cur: soft,
            rlim_max: self.0.rlim_max,
        };
        // SAFETY: this standalone test owns its process; the structure is initialized.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 }
    }
}

#[cfg(all(target_os = "linux", pbfhogg_test_hooks))]
impl Drop for SoftLimit {
    fn drop(&mut self) {
        // SAFETY: as in `set`; restoring the limit this guard found.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) };
    }
}

/// Counters a run emits, for the shape assertions.
#[cfg(all(target_os = "linux", pbfhogg_test_hooks))]
#[derive(Default)]
struct Counters(std::sync::Mutex<Vec<(String, i64)>>);

#[cfg(all(target_os = "linux", pbfhogg_test_hooks))]
impl pbfhogg::debug::InstrumentSink for Counters {
    fn marker(&self, _name: &str) {}
    fn counter(&self, name: &str, value: i64) {
        self.0
            .lock()
            .expect("counters")
            .push((name.to_owned(), value));
    }
}

/// `TEST_HEAVY_RUN_FDS`, the descriptor gate's charge for one heavy run,
/// observed for the largest external join a heavy host's grant admits:
/// the kernel limit leaves exactly the charge free above the descriptors
/// already open, and the run must complete under it. The shape is forced,
/// not hoped for: eight fixed cores and the heavy grant give the most
/// stage-1 workers the grant fits (three) and saturate the later stages'
/// worker caps; a rendezvous holds every worker with its id-shard fleet
/// open, and the relation scan with its input open, until all have opened
/// theirs; 256 equal way blobs give the most slot buckets. The run's
/// scratch and output count against the charge; the fixture written
/// beforehand does not. Not covered: Auto routing, fault paths and the
/// direct-I/O and io_uring writers.
#[cfg(all(target_os = "linux", pbfhogg_test_hooks))]
fn the_largest_heavy_run_fits_its_descriptor_charge() {
    use pbfhogg::host::{Host, ProcessActions};
    use pbfhogg::test_hooks::FleetOutcome;
    use std::sync::Arc;

    struct HeavyGrant;
    impl ProcessActions for HeavyGrant {
        fn fd_allowance(&self, wanted: u64) -> pbfhogg::process::FdReservation {
            common::fd_gate::heavy_grant(wanted)
        }
    }

    let dir = common::test_scratch();
    let input = dir.path().join("fleet.osm.pbf");
    common::write_multi_block_test_pbf(
        &input,
        &common::generate_nodes(256, 1),
        &common::generate_ways(256, 1, 2, 1),
        &[],
        1,
    );
    let counters = Arc::new(Counters::default());
    let host = Host {
        diagnostics: pbfhogg::host::Diagnostics::discard(),
        process: Arc::new(HeavyGrant),
        resources: Arc::new(pbfhogg::host::Fixed {
            parallelism: std::num::NonZeroUsize::new(8),
            memory_available: Some(8 << 30),
        }),
        page_cache: pbfhogg::host::PageCachePolicy::Keep,
        executor: crate::common::test_executor(),
        invocation: pbfhogg::host::Invocation::with_sink(
            Arc::clone(&counters) as Arc<dyn pbfhogg::debug::InstrumentSink>
        ),
    };
    let workers = 3;
    host.invocation
        .faults()
        .arm_altw_stage1_fleet(workers, true);
    let output = dir.path().join("fleet-out.osm.pbf");

    let Some(baseline) = open_descriptors() else {
        common::skip("charge witness needs /proc/self/fd");
        return;
    };
    let open = u64::try_from(baseline.len()).expect("small");
    let cap = open + pbfhogg::TEST_HEAVY_RUN_FDS;
    let highest = baseline
        .iter()
        .copied()
        .max()
        .map_or(0, |fd| u64::try_from(fd).expect("fd"));
    let limit = SoftLimit::current();
    // Every baseline descriptor below the cap, so exactly the charge's
    // numbers are free beneath it: holes are filled first, never extra.
    if highest >= cap || limit.0.rlim_max < cap || !limit.set(cap) {
        common::skip(format!(
            "charge witness needs a soft limit of {cap} above descriptor {highest} \
             (hard {})",
            limit.0.rlim_max
        ));
        return;
    }
    let result = external_join_keeping(&host, &input, &output, false);
    drop(limit);

    let charge = pbfhogg::TEST_HEAVY_RUN_FDS;
    match result {
        Ok(()) => {}
        // The system-wide file table, not this process's cap: environmental.
        Err(e) if e.contains("os error 23") => {
            common::skip(format!("charge witness hit a full system file table: {e}"));
            return;
        }
        // EMFILE under the controlled cap means the run outgrew its charge.
        Err(e) if e.contains("os error 24") => {
            panic!("the largest heavy run needs more than its charge of {charge} descriptors: {e}")
        }
        Err(e) => panic!("charge witness run failed: {e}"),
    }
    assert_eq!(
        host.invocation.faults().altw_stage1_fleet_outcome(),
        Some(FleetOutcome::Complete),
        "every stage-1 fleet and the relation input were open at once"
    );
    let seen = counters.0.lock().expect("counters").clone();
    let counter = |name: &str| {
        seen.iter()
            .find(|(n, _)| n == name)
            .map(|&(_, v)| v)
            .unwrap_or_else(|| panic!("{name} missing: {seen:?}"))
    };
    assert_eq!(
        counter("extjoin_num_shard_workers"),
        i64::try_from(workers).expect("small"),
        "the grant's most workers"
    );
    assert_eq!(
        counter("extjoin_slot_bucket_count"),
        256,
        "the most slot buckets"
    );
}
