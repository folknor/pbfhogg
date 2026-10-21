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
