//! In-process host isolation has no CLI expression, so these tests call the
//! hidden command entries as well as the public host API.
use pbfhogg::host::{ProcessActions, Resources};
use pbfhogg::process::FdAllowance;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering::Relaxed},
};

#[derive(Default)]
struct Actions {
    alloc: AtomicUsize,
    fd: AtomicUsize,
}
impl ProcessActions for Actions {
    fn cross_thread_alloc_pool(&self) {
        self.alloc.fetch_add(1, Relaxed);
    }
    fn fd_allowance(&self, _wanted: u64) -> FdAllowance {
        self.fd.fetch_add(1, Relaxed);
        pbfhogg::process::current_nofile()
    }
}
fn input(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("base.osm.pbf");
    crate::common::write_base_pbf(&path);
    path
}
#[test]
fn process_actions_fire_once_at_each_hook_site() {
    let dir = crate::common::test_scratch();
    let base = input(dir.path());
    let actions = Arc::new(Actions::default());
    let mut host = crate::common::test_host();
    host.process = Arc::clone(&actions) as Arc<dyn ProcessActions>;
    let overrides = pbfhogg::HeaderOverrides::default();
    let clean = pbfhogg::cat::CleanAttrs {
        version: true,
        ..Default::default()
    };
    pbfhogg::cat::cat(
        &[&base],
        crate::common::out(dir.path().join("clean.pbf")),
        None,
        &clean,
        &Default::default(),
        &overrides,
        &host,
    )
    .expect("cat clean");
    // cat asks for one descriptor per pinned input before pinning them.
    assert_eq!(
        (actions.alloc.load(Relaxed), actions.fd.load(Relaxed)),
        (1, 1)
    );
    actions.alloc.store(0, Relaxed);
    actions.fd.store(0, Relaxed);
    pbfhogg::altw::external::external_join(
        &base,
        crate::common::out(dir.path().join("external.pbf")),
        true,
        &Default::default(),
        &overrides,
        false,
        &host,
        crate::common::scratch_in(dir.path()),
    )
    .expect("external");
    assert_eq!(
        (actions.alloc.load(Relaxed), actions.fd.load(Relaxed)),
        (0, 1)
    );
    actions.fd.store(0, Relaxed);
    pbfhogg::cat::cat(
        &[&base],
        crate::common::out(dir.path().join("cat.pbf")),
        None,
        &Default::default(),
        &Default::default(),
        &overrides,
        &host,
    )
    .expect("cat");
    assert_eq!(
        (actions.alloc.load(Relaxed), actions.fd.load(Relaxed)),
        (0, 1)
    );
}
struct CountingResources(AtomicUsize);
impl Resources for CountingResources {
    fn parallelism(&self) -> Option<std::num::NonZeroUsize> {
        self.0.fetch_add(1, Relaxed);
        crate::common::fixed_resources().parallelism()
    }
    fn memory_available(&self) -> Option<u64> {
        crate::common::fixed_resources().memory_available()
    }
}
#[test]
fn resources_provider_is_consulted() {
    let dir = crate::common::test_scratch();
    let base = input(dir.path());
    let resources = Arc::new(CountingResources(AtomicUsize::new(0)));
    let mut host = crate::common::test_host();
    host.resources = Arc::clone(&resources) as Arc<dyn Resources>;
    pbfhogg::repack::repack(
        &base,
        crate::common::out(dir.path().join("repack.pbf")),
        pbfhogg::block_builder::DEFAULT_ELEMENT_CAP,
        &Default::default(),
        &Default::default(),
        &host,
    )
    .expect("repack");
    assert!(resources.0.load(Relaxed) > 0);
}

#[derive(Default)]
struct Recording(std::sync::Mutex<Vec<(String, String)>>);
impl pbfhogg::host::DiagnosticSink for Recording {
    fn event(&self, event: &pbfhogg::host::Event<'_>) {
        self.0
            .lock()
            .expect("sink")
            .push((event.key.to_owned(), event.args.to_string()));
    }
}
#[test]
fn diagnostics_are_per_run() {
    std::thread::scope(|scope| {
        for count in [3, 5] {
            scope.spawn(move || {
                let dir = crate::common::test_scratch();
                let raw = dir.path().join("raw.pbf");
                let indexed = dir.path().join("indexed.pbf");
                let nodes: Vec<_> = (1..=count)
                    .map(|id| crate::common::TestNode {
                        id,
                        lat: 0,
                        lon: 0,
                        tags: vec![],
                        meta: None,
                    })
                    .collect();
                crate::common::write_multi_block_test_pbf(&raw, &nodes, &[], &[], 1);
                let mut host = crate::common::test_host();
                pbfhogg::cat::cat(
                    &[&raw],
                    crate::common::out(&indexed),
                    None,
                    &Default::default(),
                    &Default::default(),
                    &Default::default(),
                    &host,
                )
                .expect("index");
                let sink = Arc::new(Recording::default());
                host.diagnostics = pbfhogg::host::Diagnostics::new(
                    Arc::clone(&sink) as Arc<dyn pbfhogg::host::DiagnosticSink>
                );
                pbfhogg::sort::sort(
                    &indexed,
                    crate::common::out(dir.path().join("sorted.pbf")),
                    &Default::default(),
                    &Default::default(),
                    &host,
                )
                .expect("sort");
                let lines: Vec<_> = sink
                    .0
                    .lock()
                    .expect("sink")
                    .iter()
                    .filter(|(key, _)| key == "sort.indexed")
                    .map(|(_, text)| text.clone())
                    .collect();
                assert_eq!(lines, [format!("  {count} OSMData blobs indexed")]);
            });
        }
    });
}
#[test]
fn unrecognised_indexdata_warning_is_per_handle() {
    let sink = Arc::new(Recording::default());
    let first = pbfhogg::host::Diagnostics::new(
        Arc::clone(&sink) as Arc<dyn pbfhogg::host::DiagnosticSink>
    );
    let second = pbfhogg::host::Diagnostics::new(
        Arc::clone(&sink) as Arc<dyn pbfhogg::host::DiagnosticSink>
    );
    let dir = crate::common::test_scratch();
    let bad = dir.path().join("bad-indexdata.osm.pbf");
    crate::common::write_bad_indexdata_pbf(&bad);
    for (diagnostics, expected) in [(first.clone(), 1), (second, 2), (first, 2)] {
        let reader = pbfhogg::BlobReader::from_path(&bad)
            .expect("reader")
            .diagnostics(diagnostics);
        for blob in reader {
            blob.expect("blob");
        }
        assert_eq!(sink.0.lock().expect("sink").len(), expected);
    }
}
fn one_thread_pool() -> Arc<rayon::ThreadPool> {
    Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("pool"),
    )
}
/// A minimal but real PrimitiveBlock: one empty PrimitiveGroup (field 2).
const EMPTY_BLOCK: [u8; 2] = [0x12, 0x00];
fn empty_header() -> Vec<u8> {
    pbfhogg::block_builder::HeaderBuilder::new()
        .build()
        .expect("header")
}
fn count_data_blobs(path: &std::path::Path) -> usize {
    pbfhogg::BlobReader::from_path(path)
        .expect("reader")
        .filter(|blob| blob.as_ref().expect("blob").get_type() == pbfhogg::BlobType::OsmData)
        .count()
}
/// More blocks than the writer has dispatch permits, so the producer must
/// wait for framing work queued on its own (only) thread.
const POOL_BLOCKS: usize = 200;
#[test]
fn writer_on_a_one_thread_pool_flushes() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("pool.pbf");
    let pool = one_thread_pool();
    pool.install(|| -> pbfhogg::Result<()> {
        let mut writer = pbfhogg::writer::PbfWriter::to_path(
            &path,
            Default::default(),
            &empty_header(),
            pbfhogg::host::Executor::Pool(Arc::clone(&pool)),
        )?;
        for _ in 0..POOL_BLOCKS {
            writer.write_primitive_block(&EMPTY_BLOCK)?;
        }
        writer.flush()
    })
    .expect("flush on a one-thread pool");
    assert_eq!(count_data_blobs(&path), POOL_BLOCKS);
}
#[test]
fn writer_on_a_one_thread_pool_drops_unflushed() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("dropped.pbf");
    let pool = one_thread_pool();
    // Completing at all is the assertion: a blocking join from the pool's
    // only thread would never let the queued framing work run. An unflushed
    // drop on a non-panicking path is the error-bail-out shape, which the
    // library allows (its unit-test-only flush assertion does not apply to
    // integration tests).
    pool.install(|| -> pbfhogg::Result<()> {
        let mut writer = pbfhogg::writer::PbfWriter::to_path(
            &path,
            Default::default(),
            &empty_header(),
            pbfhogg::host::Executor::Pool(Arc::clone(&pool)),
        )?;
        for _ in 0..POOL_BLOCKS {
            writer.write_primitive_block(&EMPTY_BLOCK)?;
        }
        drop(writer);
        Ok(())
    })
    .expect("writes on a one-thread pool");
    assert_eq!(count_data_blobs(&path), POOL_BLOCKS);
}
#[test]
fn ambient_writer_inside_a_foreign_pool_flushes() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("ambient.pbf");
    one_thread_pool()
        .install(|| -> pbfhogg::Result<()> {
            let mut writer = pbfhogg::writer::PbfWriter::to_path(
                &path,
                Default::default(),
                &empty_header(),
                pbfhogg::host::Executor::Ambient,
            )?;
            for _ in 0..POOL_BLOCKS {
                writer.write_primitive_block(&EMPTY_BLOCK)?;
            }
            writer.flush()
        })
        .expect("flush under Ambient inside a one-thread pool");
    assert_eq!(count_data_blobs(&path), POOL_BLOCKS);
}
#[cfg(feature = "test-hooks")]
#[test]
fn swallowed_framing_panic_surfaces_at_flush() {
    let dir = crate::common::test_scratch();
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .panic_handler(|_| {})
            .build()
            .expect("pool"),
    );
    let mut writer = pbfhogg::writer::PbfWriter::to_path(
        &dir.path().join("panic.pbf"),
        Default::default(),
        &empty_header(),
        pbfhogg::host::Executor::Pool(pool),
    )
    .expect("writer");
    writer.test_faults().framing_panic_at.store(3, Relaxed);
    for _ in 0..10 {
        writer.write_primitive_block(&EMPTY_BLOCK).expect("write");
    }
    let err = writer
        .flush()
        .expect_err("a swallowed framing panic must not flush as a complete file");
    assert!(
        err.to_string()
            .contains("an upstream framer dropped without sending an earlier seq"),
        "got: {err}"
    );
}
/// A swallowed panic must return its permit even when flush itself has to
/// run the framing job on the pool's only worker. Cover an interior gap, a
/// missing tail, and a stream whose only task panics.
#[cfg(feature = "test-hooks")]
#[test]
fn swallowed_framing_panics_on_one_worker_fail_flush() {
    let dir = crate::common::test_scratch();
    let panics = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&panics);
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .panic_handler(move |_| {
                seen.fetch_add(1, Relaxed);
            })
            .build()
            .expect("pool"),
    );
    for (count, panic_at, message) in [
        (10, 3, "earlier seq"),
        (10, 9, "final seq"),
        (1, 0, "final seq"),
    ] {
        pool.install(|| {
            let mut writer = pbfhogg::writer::PbfWriter::to_path(
                &dir.path().join(format!("panic-{count}-{panic_at}.pbf")),
                Default::default(),
                &empty_header(),
                pbfhogg::host::Executor::Pool(Arc::clone(&pool)),
            )
            .expect("writer");
            writer
                .test_faults()
                .framing_panic_at
                .store(panic_at, Relaxed);
            for _ in 0..count {
                writer.write_primitive_block(&EMPTY_BLOCK).expect("write");
            }
            let err = writer.flush().expect_err("the output is incomplete");
            assert!(err.to_string().contains(message), "got: {err}");
        });
    }
    assert_eq!(
        panics.load(Relaxed),
        3,
        "every panic reaches the caller's handler"
    );
}
fn run_external_join(
    input: &std::path::Path,
    output: &std::path::Path,
    host: &pbfhogg::host::Host,
    scratch: pbfhogg::scratch::RunScratch,
) -> Result<(), String> {
    pbfhogg::altw::external::external_join(
        input,
        crate::common::out(output),
        true,
        &Default::default(),
        &Default::default(),
        false,
        host,
        scratch,
    )
    .map(drop)
    .map_err(|e| e.to_string())
}
/// altw external builds its output writer in stage 4, on a scoped thread of
/// its own. A backend setup refusal there must reach the caller as the typed
/// `PlatformUnsupported` (the CLI's exit 3), not flattened at the stage join.
/// The CLI test of the same contract cannot force the refusal without a host
/// assumption, so the fault plan injects it at the one place a backend becomes
/// a writer. Buffered reads, so no genuine input-side refusal can stand in for
/// the injected one; its source message proves which refusal arrived.
#[cfg(feature = "test-hooks")]
#[test]
fn external_join_stage4_backend_refusal_stays_typed_across_the_join() {
    let dir = crate::common::test_scratch();
    let base = input(dir.path());
    let host = crate::common::test_host();
    let faults = host.cancel.faults();
    faults.refuse_output_backend.store(true, Relaxed);
    let output = dir.path().join("refused.pbf");
    let Err(err) = pbfhogg::altw::external::external_join(
        &base,
        crate::common::out(&output),
        true,
        &Default::default(),
        &Default::default(),
        false,
        &host,
        crate::common::scratch_in(dir.path()),
    ) else {
        panic!("an injected backend refusal must fail the run");
    };
    match err.kind() {
        pbfhogg::ErrorKind::PlatformUnsupported { source, .. } => assert_eq!(
            source.to_string(),
            pbfhogg::test_hooks::INJECTED_BACKEND_REFUSAL,
            "a different platform refusal arrived: {err}"
        ),
        other => panic!("the stage 4 refusal lost its type at the join: {other:?} ({err})"),
    }
    assert_eq!(
        faults.output_backend_refusals.load(Relaxed),
        1,
        "the refusal must come from stage 4's single writer construction"
    );
    assert!(!output.exists(), "a refused run published an output");
}
#[test]
fn concurrent_external_joins_use_distinct_scratch() {
    let dir = crate::common::test_scratch();
    let base = input(dir.path());
    let host = crate::common::test_host();
    let reference = dir.path().join("reference.pbf");
    run_external_join(
        &base,
        &reference,
        &host,
        crate::common::scratch_in(dir.path()),
    )
    .expect("sequential external join");
    // Both outputs share one parent, so both runs place their scratch
    // directory beside each other under the same pid.
    let outputs = [dir.path().join("a.pbf"), dir.path().join("b.pbf")];
    struct Overlap(std::sync::Barrier);
    impl ProcessActions for Overlap {
        fn cross_thread_alloc_pool(&self) {}
        fn fd_allowance(&self, _wanted: u64) -> FdAllowance {
            self.0.wait();
            pbfhogg::process::current_nofile()
        }
    }
    let mut host = host;
    host.process = Arc::new(Overlap(std::sync::Barrier::new(2)));
    std::thread::scope(|scope| {
        let handles: Vec<_> = outputs
            .iter()
            .map(|output| {
                scope.spawn(|| {
                    run_external_join(&base, output, &host, crate::common::scratch_in(dir.path()))
                })
            })
            .collect();
        for handle in handles {
            handle
                .join()
                .expect("external join thread")
                .expect("concurrent external join");
        }
    });
    let expected = crate::common::read_all_elements_with_coords(&reference);
    for output in &outputs {
        let got = crate::common::read_all_elements_with_coords(output);
        assert_eq!(got.nodes, expected.nodes, "{}", output.display());
        assert_eq!(got.ways, expected.ways, "{}", output.display());
        assert_eq!(got.relations, expected.relations, "{}", output.display());
    }
}
#[test]
fn a_missing_scratch_parent_is_refused_before_anything_is_created() {
    let dir = crate::common::test_scratch();
    let missing = dir.path().join("missing");
    let host = crate::common::test_host();
    let err = pbfhogg::scratch::RunScratch::create(&missing, &host)
        .expect_err("a missing scratch parent must fail");
    assert!(
        err.to_string().contains(&missing.display().to_string()),
        "the error must name the scratch parent, got: {err}"
    );
    assert!(!missing.exists(), "the scratch parent must not be created");
}
/// Auto routes a sorted, indexed input on the memory the host supplies:
/// plenty routes its small store to sparse, none leaves no page-cache
/// budget and routes external.
#[test]
fn fixed_memory_routes_auto() {
    let dir = crate::common::test_scratch();
    let base = input(dir.path());
    for (memory, backend) in [(8 << 30, "sparse"), (0, "external")] {
        let sink = Arc::new(Recording::default());
        let mut host = crate::common::test_host();
        host.diagnostics = pbfhogg::host::Diagnostics::new(
            Arc::clone(&sink) as Arc<dyn pbfhogg::host::DiagnosticSink>
        );
        host.resources = Arc::new(pbfhogg::host::Fixed {
            parallelism: std::num::NonZeroUsize::new(4),
            memory_available: Some(memory),
        });
        let opts = pbfhogg::altw::AltwOptions {
            keep_untagged_nodes: true,
            write: Default::default(),
            index_type: pbfhogg::altw::IndexType::Auto,
            inject_prepass: false,
            ignore_missing_nodes: false,
        };
        pbfhogg::altw::add_locations_to_ways(
            &base,
            crate::common::out(dir.path().join(format!("auto-{backend}.pbf"))),
            &opts,
            &Default::default(),
            &host,
            crate::common::scratch_in(dir.path()),
        )
        .expect("auto");
        let prefix = format!("auto-selected index type {backend} (store estimate");
        assert!(
            sink.0
                .lock()
                .expect("sink")
                .iter()
                .any(|(key, text)| key == "altw.auto_select" && text.starts_with(&prefix)),
            "{memory} bytes must route {backend}"
        );
    }
}

/// Every scratch-producing backend works in the run directory it is handed
/// and leaves its parent exactly as it found it.
#[test]
fn scratch_backends_use_the_given_directory_and_leave_nothing() {
    let dir = crate::common::test_scratch();
    let scratch = dir.path().join("scratch");
    std::fs::create_dir(&scratch).expect("scratch directory");
    let base = input(dir.path());
    let changed = dir.path().join("changed.osm.pbf");
    crate::common::write_changed_pbf(&changed);
    for (command, jobs) in [("sparse", 1), ("text", 2), ("osc", 1), ("osc", 2)] {
        let output = dir.path().join(format!("{command}-{jobs}.osc"));
        let host = crate::common::test_host();
        run_scratch_backend(
            command,
            jobs,
            &base,
            &changed,
            &output,
            &host,
            crate::common::scratch_in(&scratch),
        )
        .expect("valid scratch");
        assert_eq!(
            std::fs::read_dir(&scratch).expect("scratch").count(),
            0,
            "{command} must remove its scratch directory"
        );
    }
}

/// A diff that runs one shard writes no scratch and takes `None`; one that may
/// run several refuses `None` before reading anything. `diff_stats` takes no
/// scratch at all, however many shards it runs.
#[test]
fn diff_without_scratch_needs_a_single_shard() {
    let dir = crate::common::test_scratch();
    let base = input(dir.path());
    let changed = dir.path().join("changed.osm.pbf");
    crate::common::write_changed_pbf(&changed);
    let run = |verbose: bool, jobs: usize| {
        let options = pbfhogg::diff::DiffOptions {
            suppress_common: false,
            verbose,
            type_filter: None,
            parallelism: pbfhogg::Parallelism::from_jobs(jobs),
        };
        pbfhogg::diff::diff(
            &base,
            &changed,
            &mut Vec::new(),
            &options,
            false,
            &crate::common::test_host(),
            None,
        )
    };
    assert!(run(false, 1).is_ok());
    assert!(run(true, 1).is_ok());
    // Verbose shards like any other diff.
    for verbose in [false, true] {
        let err = run(verbose, 2).expect_err("two shards need scratch");
        assert!(err.to_string().contains("scratch directory"), "{err}");
    }
    // Counting takes no scratch at any shard count and writes nothing.
    let before = crate::common::snapshot_dir(dir.path());
    for jobs in [0, 2, 4] {
        let stats = pbfhogg::diff::diff_stats(
            &base,
            &changed,
            None,
            pbfhogg::Parallelism::from_jobs(jobs),
            false,
            &crate::common::test_host(),
        )
        .expect("diff_stats");
        assert!(stats.has_differences(), "jobs {jobs}: {stats:?}");
    }
    assert_eq!(crate::common::snapshot_dir(dir.path()), before);
}

/// The backends write their scratch into the run directory they are given
/// and nowhere else. A run directory that cannot be written to makes each
/// fail naming a path inside it, which a backend creating its files beside
/// the input or output (as before) would not.
#[test]
fn scratch_backends_write_inside_the_run_directory() {
    // Root ignores directory permissions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = crate::common::test_scratch();
    let base = input(dir.path());
    let changed = dir.path().join("changed.osm.pbf");
    crate::common::write_changed_pbf(&changed);
    for (command, jobs) in [("sparse", 1), ("text", 2), ("osc", 1), ("osc", 2)] {
        let parent = dir.path().join(format!("parent-{command}-{jobs}"));
        std::fs::create_dir(&parent).expect("parent");
        let scratch = crate::common::scratch_in(&parent);
        let run_dir = scratch.path().to_path_buf();
        std::fs::set_permissions(
            &run_dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o500),
        )
        .expect("lock the run directory");
        let output = dir.path().join(format!("locked-{command}-{jobs}.osc"));
        let host = crate::common::test_host();
        let result = run_scratch_backend(command, jobs, &base, &changed, &output, &host, scratch);
        std::fs::set_permissions(
            &run_dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("unlock");
        let err = result.expect_err("a run directory that cannot be written must fail the run");
        assert!(
            err.contains(&run_dir.display().to_string()),
            "{command} jobs {jobs}: {err}"
        );
    }
}

fn run_scratch_backend(
    command: &str,
    jobs: usize,
    base: &std::path::Path,
    changed: &std::path::Path,
    output: &std::path::Path,
    host: &pbfhogg::host::Host,
    scratch: pbfhogg::scratch::RunScratch,
) -> Result<(), String> {
    match command {
        "text" => {
            let options = pbfhogg::diff::DiffOptions {
                suppress_common: false,
                verbose: false,
                type_filter: None,
                parallelism: pbfhogg::Parallelism::from_jobs(jobs),
            };
            pbfhogg::diff::diff(
                base,
                changed,
                &mut Vec::new(),
                &options,
                false,
                host,
                Some(scratch),
            )
            .map(drop)
            .map_err(|e| e.to_string())
        }
        "osc" => pbfhogg::diff::derive::derive_changes(
            base,
            changed,
            crate::common::out(output),
            false,
            false,
            None,
            pbfhogg::Parallelism::from_jobs(jobs),
            host,
            scratch,
        )
        .map(drop)
        .map_err(|e| e.to_string()),
        "sparse" => {
            let options = pbfhogg::altw::AltwOptions {
                keep_untagged_nodes: true,
                write: Default::default(),
                index_type: pbfhogg::altw::IndexType::Sparse,
                inject_prepass: false,
                ignore_missing_nodes: false,
            };
            pbfhogg::altw::add_locations_to_ways(
                base,
                crate::common::out(output),
                &options,
                &Default::default(),
                host,
                scratch,
            )
            .map(drop)
            .map_err(|e| e.to_string())
        }
        _ => unreachable!("test backend"),
    }
}

// `has_indexdata` is a `commands` export.
#[cfg(all(feature = "linux-direct-io", feature = "commands"))]
mod platform {
    use crate::common::{self, TestNode};

    fn direct_input(path: &std::path::Path) {
        common::write_test_pbf_sorted(
            path,
            &[TestNode {
                id: 1,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            }],
            &[],
            &[],
        );
    }

    #[test]
    fn direct_reader_reads_a_pbf() {
        let dir = common::test_scratch();
        let path = dir.path().join("direct-reader.osm.pbf");
        direct_input(&path);

        let reader = match pbfhogg::BlobReader::open(&path, true) {
            Ok(reader) => reader,
            Err(error) if common::is_einval(&error) => return,
            Err(error) => panic!("open direct PBF reader: {error}"),
        };
        let mut count = 0;
        for blob in reader {
            blob.expect("read direct PBF blob");
            count += 1;
        }
        assert_eq!(count, 2, "header and data blobs must be readable");
    }

    #[test]
    fn direct_skip_rejects_a_truncated_header_blob() {
        let dir = common::test_scratch();
        let path = dir.path().join("direct-skip-truncated.osm.pbf");
        direct_input(&path);
        let bytes = std::fs::read(&path).expect("read fixture");
        let header_blob = common::adversarial::locate_blobs(&bytes)
            .into_iter()
            .next()
            .expect("header blob");
        std::fs::write(&path, &bytes[..header_blob.blob_end - 1]).expect("truncate fixture");

        let error =
            match pbfhogg::has_indexdata(&path, true, &pbfhogg::host::Diagnostics::discard()) {
                Ok(_) => panic!("truncated header blob must fail while skipping"),
                Err(error) if common::is_einval(&error) => return,
                Err(error) => error,
            };
        assert_unexpected_eof(&error);
    }

    /// The skip error reaches the caller as a bare `io::Error` or wrapped in
    /// the crate error; either way it must be `UnexpectedEof`.
    fn assert_unexpected_eof(error: &(dyn std::error::Error + 'static)) {
        let kind = match error.downcast_ref::<pbfhogg::Error>() {
            Some(error) => match error.kind() {
                pbfhogg::ErrorKind::Io(source) => source.kind(),
                other => panic!("expected an I/O error, got {other:?}"),
            },
            None => error
                .downcast_ref::<std::io::Error>()
                .unwrap_or_else(|| panic!("expected an I/O error, got {error}"))
                .kind(),
        };
        assert_eq!(kind, std::io::ErrorKind::UnexpectedEof, "{error}");
    }

    /// The direct reader's page-aligned skip branch: seeking succeeds past
    /// EOF, so the target must be checked against the file length. The
    /// fixture is one bare frame header whose declared payload ends exactly
    /// on a page boundary (4-byte prefix + 14-byte header + 8174 = 8192),
    /// with no payload bytes on disk.
    #[test]
    fn direct_skip_rejects_a_page_aligned_target_past_eof() {
        let dir = common::test_scratch();
        let path = dir.path().join("direct-skip-aligned.osm.pbf");
        // BlobHeader: type "OSMHeader" (field 1), datasize 8174 (field 3,
        // varint 0xEE 0x3F).
        let mut frame = vec![0, 0, 0, 14, 0x0A, 9];
        frame.extend_from_slice(b"OSMHeader");
        frame.extend_from_slice(&[0x18, 0xEE, 0x3F]);
        assert_eq!(frame.len(), 18);
        std::fs::write(&path, &frame).expect("write fixture");

        let error =
            match pbfhogg::has_indexdata(&path, true, &pbfhogg::host::Diagnostics::discard()) {
                Ok(_) => panic!("aligned skip beyond EOF must fail"),
                Err(error) if common::is_einval(&error) => return,
                Err(error) => error,
            };
        assert_unexpected_eof(&error);
    }
}
