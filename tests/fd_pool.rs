//! `host::FdPool`, the reserving descriptor provider: each command holds its
//! grant exactly while the descriptors it asked for are open, and hands it
//! back on success, refusal and error alike, whether or not the caller keeps
//! the `Host`. Observed from the commands' own markers, so nothing here
//! depends on timing or on the host's descriptor limit.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pbfhogg::cat::CleanAttrs;
use pbfhogg::debug::InstrumentSink;
use pbfhogg::host::{FdPool, Host, Invocation, NoProcessActions, ProcessActions};
use pbfhogg::write::WriteOptions;

/// Records the pool's free count at each named marker, and every counter
/// sample.
struct Watch {
    pool: Arc<FdPool>,
    at: &'static [&'static str],
    seen: Mutex<Vec<(String, u64)>>,
    counters: Mutex<Vec<(String, i64)>>,
}

impl InstrumentSink for Watch {
    fn marker(&self, name: &str) {
        if self.at.contains(&name) {
            let free = self.pool.available();
            self.seen
                .lock()
                .expect("seen")
                .push((name.to_owned(), free));
        }
    }
    fn counter(&self, name: &str, value: i64) {
        self.counters
            .lock()
            .expect("counters")
            .push((name.to_owned(), value));
    }
}

fn watched(pool: &Arc<FdPool>, at: &'static [&'static str]) -> (Host, Arc<Watch>) {
    let watch = Arc::new(Watch {
        pool: Arc::clone(pool),
        at,
        seen: Mutex::new(Vec::new()),
        counters: Mutex::new(Vec::new()),
    });
    let mut host = crate::common::test_host();
    host.process = Arc::clone(pool) as Arc<dyn ProcessActions>;
    host.invocation = Invocation::with_sink(Arc::clone(&watch) as Arc<dyn InstrumentSink>);
    (host, watch)
}

fn pool(budget: u64) -> Arc<FdPool> {
    Arc::new(FdPool::new(budget, Arc::new(NoProcessActions)))
}

fn inputs(dir: &Path) -> [PathBuf; 2] {
    let a = dir.join("a.osm.pbf");
    let b = dir.join("b.osm.pbf");
    crate::common::write_base_pbf(&a);
    crate::common::write_base_pbf(&b);
    [a, b]
}

fn cat(files: &[&Path], output: &Path, host: &Host) -> pbfhogg::Result<()> {
    pbfhogg::cat::cat(
        files,
        crate::common::out(output),
        None,
        &CleanAttrs::default(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        host,
    )
    .map(drop)
}

/// Plain cat of two distinct inputs asks for 2 plus the fixed headroom.
fn cat_wants_for_two() -> u64 {
    2 + 8
}

/// One `Host` kept across two cats: each holds its grant through the run
/// and the budget is whole again as soon as each returns, so a retained host
/// never exhausts the pool.
#[test]
fn a_retained_host_gets_its_grant_back_after_each_cat() {
    let dir = crate::common::test_scratch();
    let [a, b] = inputs(dir.path());
    let budget = 100;
    let pool = pool(budget);
    let (host, watch) = watched(&pool, &["CAT_SCAN_START"]);
    for round in 0..2 {
        let output = dir.path().join(format!("out{round}.osm.pbf"));
        cat(&[&a, &b], &output, &host).expect("cat within budget");
        assert_eq!(
            pool.available(),
            budget,
            "round {round}: released on return"
        );
    }
    let seen = watch.seen.lock().expect("seen").clone();
    assert_eq!(
        seen,
        [
            ("CAT_SCAN_START".to_owned(), budget - cat_wants_for_two()),
            ("CAT_SCAN_START".to_owned(), budget - cat_wants_for_two()),
        ],
        "each run holds its grant while its pins are open"
    );
}

/// A grant short of what cat needs is refused and handed back at once; so
/// is a grant whose run fails after admission.
#[test]
fn a_refused_or_failed_cat_returns_its_grant() {
    let dir = crate::common::test_scratch();
    let [a, b] = inputs(dir.path());
    let output = dir.path().join("out.osm.pbf");

    let short = pool(cat_wants_for_two() - 1);
    let (host, _) = watched(&short, &[]);
    let err = cat(&[&a, &b], &output, &host).expect_err("one short refuses");
    assert!(err.to_string().contains("host grant"), "{err}");
    assert_eq!(short.available(), cat_wants_for_two() - 1);
    assert!(!output.exists());

    let budget = 100;
    let enough = pool(budget);
    let (host, _) = watched(&enough, &[]);
    let missing = dir.path().join("missing.osm.pbf");
    cat(&[&a, &missing], &output, &host).expect_err("a missing input fails after admission");
    assert_eq!(enough.available(), budget);
    assert!(!output.exists());
}

/// `cat --dedupe` asks once per argument and still holds the grant in pass
/// 2, where its sources are open; the budget is whole when it returns.
#[test]
fn dedupe_holds_its_grant_through_pass_two() {
    let dir = crate::common::test_scratch();
    let [a, b] = inputs(dir.path());
    let output = dir.path().join("out.osm.pbf");
    let budget = 100;
    let pool = pool(budget);
    let (host, watch) = watched(&pool, &["MERGEPBF_PASS1_START", "MERGEPBF_PASS2_START"]);
    pbfhogg::cat::dedupe::merge_pbf(
        &[a.as_path(), b.as_path()],
        crate::common::out(&output),
        &WriteOptions {
            force: true,
            ..WriteOptions::default()
        },
        &pbfhogg::HeaderOverrides::default(),
        &host,
    )
    .expect("dedupe within budget");
    let held = budget - cat_wants_for_two();
    let seen = watch.seen.lock().expect("seen").clone();
    assert_eq!(
        seen,
        [
            ("MERGEPBF_PASS1_START".to_owned(), held),
            ("MERGEPBF_PASS2_START".to_owned(), held),
        ]
    );
    assert_eq!(pool.available(), budget);
}

/// altw external stage 1 sizes its workers from the grant (two by the test
/// host's cores, one by a pool holding room for one), holds it while its
/// shard files are open, and hands it back before stage 2. Below one
/// worker's need it refuses and returns the grant.
#[test]
fn external_stage_one_is_capped_by_the_pool_and_releases_before_stage_two() {
    let dir = crate::common::test_scratch();
    let [input, _] = inputs(dir.path());
    let output = dir.path().join("out.osm.pbf");
    // Headroom 64 plus one worker's 256 shards, short of a second worker.
    let budget = 64 + 256 + 100;
    let pool = pool(budget);
    let (host, watch) = watched(&pool, &["EXTJOIN_STAGE1_END", "EXTJOIN_STAGE2_START"]);
    pbfhogg::altw::external::external_join(
        &input,
        crate::common::out(&output),
        true,
        &Default::default(),
        &Default::default(),
        false,
        &host,
        crate::common::scratch_in(dir.path()),
    )
    .expect("external join with one worker");
    let seen = watch.seen.lock().expect("seen").clone();
    assert_eq!(
        seen,
        [
            ("EXTJOIN_STAGE1_END".to_owned(), budget),
            ("EXTJOIN_STAGE2_START".to_owned(), budget),
        ],
        "released when stage 1 returns"
    );
    let counters = watch.counters.lock().expect("counters").clone();
    let counter = |name: &str| {
        counters
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, v)| v)
            .unwrap_or_else(|| panic!("{name} missing: {counters:?}"))
    };
    assert_eq!(
        counter("extjoin_cpu_cap_workers"),
        2,
        "the test host's cores"
    );
    assert_eq!(
        counter("extjoin_fd_available"),
        i64::try_from(budget).expect("small")
    );
    assert_eq!(counter("extjoin_fd_cap_workers"), 1, "capped by the pool");

    let small = self::pool(64 + 256 - 1);
    let (host, _) = watched(&small, &[]);
    let refused = dir.path().join("refused.osm.pbf");
    let err = pbfhogg::altw::external::external_join(
        &input,
        crate::common::out(&refused),
        true,
        &Default::default(),
        &Default::default(),
        false,
        &host,
        crate::common::scratch_in(dir.path()),
    )
    .err()
    .expect("below one worker refuses");
    assert!(err.to_string().contains("host granted"), "{err}");
    assert_eq!(small.available(), 64 + 256 - 1);
}
