//! In-process host isolation has no CLI expression, so these tests call the
//! hidden command entries as well as the public host API.
use pbfhogg::host::{MachineProbe, ProcessActions, Resources};
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
fn input() -> &'static std::path::Path {
    std::path::Path::new("tests/fixtures/host_identity/inputs/base.osm.pbf")
}
#[test]
fn process_actions_fire_once_at_each_hook_site() {
    let dir = tempfile::tempdir().expect("directory");
    let actions = Arc::new(Actions::default());
    let mut host = crate::common::test_host();
    host.process = Arc::clone(&actions) as Arc<dyn ProcessActions>;
    let overrides = pbfhogg::HeaderOverrides::default();
    let clean = pbfhogg::cat::CleanAttrs {
        version: true,
        ..Default::default()
    };
    pbfhogg::cat::cat(
        &[input()],
        &dir.path().join("clean.pbf"),
        None,
        &clean,
        Default::default(),
        false,
        false,
        &overrides,
        &host,
    )
    .expect("cat clean");
    assert_eq!(
        (actions.alloc.load(Relaxed), actions.fd.load(Relaxed)),
        (1, 0)
    );
    actions.alloc.store(0, Relaxed);
    pbfhogg::altw::external::external_join(
        input(),
        &dir.path().join("external.pbf"),
        true,
        Default::default(),
        false,
        false,
        &overrides,
        false,
        &host,
    )
    .expect("external");
    assert_eq!(
        (actions.alloc.load(Relaxed), actions.fd.load(Relaxed)),
        (0, 1)
    );
    actions.fd.store(0, Relaxed);
    pbfhogg::cat::cat(
        &[input()],
        &dir.path().join("cat.pbf"),
        None,
        &Default::default(),
        Default::default(),
        false,
        false,
        &overrides,
        &host,
    )
    .expect("cat");
    assert_eq!(
        (actions.alloc.load(Relaxed), actions.fd.load(Relaxed)),
        (0, 0)
    );
}
struct CountingResources(AtomicUsize);
impl Resources for CountingResources {
    fn parallelism(&self) -> Option<std::num::NonZeroUsize> {
        self.0.fetch_add(1, Relaxed);
        MachineProbe.parallelism()
    }
    fn memory_available(&self) -> Option<u64> {
        MachineProbe.memory_available()
    }
}
#[test]
fn resources_provider_is_consulted() {
    let dir = tempfile::tempdir().expect("directory");
    let resources = Arc::new(CountingResources(AtomicUsize::new(0)));
    let mut host = crate::common::test_host();
    host.resources = Arc::clone(&resources) as Arc<dyn Resources>;
    pbfhogg::repack::repack(
        input(),
        &dir.path().join("repack.pbf"),
        8000,
        Default::default(),
        false,
        false,
        false,
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
                let dir = tempfile::tempdir().expect("directory");
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
                    &indexed,
                    None,
                    &Default::default(),
                    Default::default(),
                    false,
                    false,
                    &Default::default(),
                    &host,
                )
                .expect("index");
                let sink = Arc::new(Recording::default());
                host.diagnostics = pbfhogg::host::Diagnostics::new(
                    Arc::clone(&sink) as Arc<dyn pbfhogg::host::DiagnosticSink>
                );
                let opts = pbfhogg::sort::SortOptions {
                    compression: Default::default(),
                    direct_io: false,
                    io_uring: false,
                    force: false,
                };
                pbfhogg::sort::sort(
                    &indexed,
                    &dir.path().join("sorted.pbf"),
                    &opts,
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
    for (diagnostics, expected) in [(first.clone(), 1), (second, 2), (first, 2)] {
        let reader = pbfhogg::BlobReader::from_path(
            "tests/fixtures/host_identity/inputs/bad-indexdata.osm.pbf",
        )
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
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("pool.pbf");
    let pool = one_thread_pool();
    pool.install(|| -> std::io::Result<()> {
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
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("dropped.pbf");
    let pool = one_thread_pool();
    // Completing at all is the assertion: a blocking join from the pool's
    // only thread would never let the queued framing work run.
    pool.install(|| -> std::io::Result<()> {
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
}
#[test]
fn ambient_writer_inside_a_foreign_pool_flushes() {
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("ambient.pbf");
    one_thread_pool()
        .install(|| -> std::io::Result<()> {
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
    let dir = tempfile::tempdir().expect("directory");
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
    writer.test_panic_in_framing_at(3);
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
    let dir = tempfile::tempdir().expect("directory");
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
            writer.test_panic_in_framing_at(panic_at);
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
fn run_external_join(output: &std::path::Path, host: &pbfhogg::host::Host) -> Result<(), String> {
    pbfhogg::altw::external::external_join(
        input(),
        output,
        true,
        Default::default(),
        false,
        false,
        &Default::default(),
        false,
        host,
    )
    .map(drop)
    .map_err(|e| e.to_string())
}
#[test]
fn concurrent_external_joins_use_distinct_scratch() {
    let dir = tempfile::tempdir().expect("directory");
    let host = crate::common::test_host();
    let reference = dir.path().join("reference.pbf");
    run_external_join(&reference, &host).expect("sequential external join");
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
            .map(|output| scope.spawn(|| run_external_join(output, &host)))
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
fn scratch_parent_override_is_honoured() {
    let dir = tempfile::tempdir().expect("directory");
    let missing = dir.path().join("missing");
    let mut host = crate::common::test_host();
    host.scratch_parent = Some(missing.clone());
    let err = run_external_join(&dir.path().join("out.pbf"), &host)
        .expect_err("a missing scratch parent must fail the run");
    assert!(
        err.contains(&missing.display().to_string()),
        "the error must name the scratch parent, got: {err}"
    );
    assert!(!missing.exists(), "the scratch parent must not be created");
}
#[test]
fn fixed_memory_routes_auto_to_external() {
    let dir = tempfile::tempdir().expect("directory");
    let sink = Arc::new(Recording::default());
    let mut host = crate::common::test_host();
    host.diagnostics = pbfhogg::host::Diagnostics::new(
        Arc::clone(&sink) as Arc<dyn pbfhogg::host::DiagnosticSink>
    );
    host.resources = Arc::new(pbfhogg::host::Fixed {
        parallelism: std::num::NonZeroUsize::new(4),
        memory_available: Some(0),
    });
    let opts = pbfhogg::altw::AltwOptions {
        keep_untagged_nodes: true,
        compression: Default::default(),
        direct_io: false,
        force: false,
        index_type: pbfhogg::altw::IndexType::Auto,
        inject_prepass: false,
        ignore_missing_nodes: false,
    };
    pbfhogg::altw::add_locations_to_ways(
        input(),
        &dir.path().join("auto.pbf"),
        &opts,
        &Default::default(),
        &host,
    )
    .expect("auto");
    assert!(
        sink.0
            .lock()
            .expect("sink")
            .iter()
            .any(|(key, text)| key == "altw.auto_select"
                && text.starts_with("auto-selected --index-type external"))
    );
}

/// The override must reach every scratch-producing backend, including both
/// derive paths and text diff's shards, rather than only external altw.
#[test]
fn scratch_parent_override_reaches_sparse_diff_and_derive() {
    let dir = tempfile::tempdir().expect("directory");
    let scratch = dir.path().join("scratch");
    let missing = dir.path().join("missing");
    std::fs::create_dir(&scratch).expect("scratch directory");
    for (command, jobs) in [("sparse", 1), ("text", 2), ("osc", 1), ("osc", 2)] {
        let output = dir.path().join(format!("{command}-{jobs}.out"));
        let mut host = crate::common::test_host();
        for parent in [&missing, &scratch] {
            host.scratch_parent = Some(parent.clone());
            let result = run_scratch_backend(command, jobs, &output, &host);
            if parent == &missing {
                let err = result.expect_err("the scratch parent must exist");
                assert!(
                    err.contains(&missing.display().to_string()),
                    "{command}: {err}"
                );
                assert!(!missing.exists());
            } else {
                result.expect("valid scratch override");
                assert_eq!(
                    std::fs::read_dir(&scratch).expect("scratch").count(),
                    0,
                    "{command} must remove its scratch fragments"
                );
            }
        }
    }
}

fn run_scratch_backend(
    command: &str,
    jobs: usize,
    output: &std::path::Path,
    host: &pbfhogg::host::Host,
) -> Result<(), String> {
    let changed = std::path::Path::new("tests/fixtures/host_identity/inputs/changed.osm.pbf");
    match command {
        "text" => {
            let options = pbfhogg::diff::DiffOptions {
                suppress_common: false,
                verbose: false,
                summary: false,
                type_filter: None,
                jobs,
            };
            pbfhogg::diff::diff(input(), changed, &mut Vec::new(), &options, false, host)
                .map(drop)
                .map_err(|e| e.to_string())
        }
        "osc" => pbfhogg::diff::derive::derive_changes(
            input(),
            changed,
            output,
            false,
            false,
            false,
            jobs,
            host,
        )
        .map(drop)
        .map_err(|e| e.to_string()),
        "sparse" => {
            let options = pbfhogg::altw::AltwOptions {
                keep_untagged_nodes: true,
                compression: Default::default(),
                direct_io: false,
                force: false,
                index_type: pbfhogg::altw::IndexType::Sparse,
                inject_prepass: false,
                ignore_missing_nodes: false,
            };
            pbfhogg::altw::add_locations_to_ways(
                input(),
                output,
                &options,
                &Default::default(),
                host,
            )
            .map(drop)
            .map_err(|e| e.to_string())
        }
        _ => unreachable!("test backend"),
    }
}
