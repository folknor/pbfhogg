//! In-process host isolation has no CLI expression, so these tests call the
//! hidden command entries as well as the public host API.
use pbfhogg::host::{ProcessActions, Resources};
use pbfhogg::process::FdReservation;
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
    fn fd_allowance(&self, _wanted: u64) -> FdReservation {
        self.fd.fetch_add(1, Relaxed);
        FdReservation::unreserved(pbfhogg::process::current_nofile())
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
/// The value `invocation` holds for counter `name`.
fn counter(invocation: &pbfhogg::host::Invocation, name: &str) -> u64 {
    let mut found = None;
    invocation.for_each_counter(|n, value| {
        if n == name {
            found = Some(value);
        }
    });
    found.unwrap_or_else(|| panic!("no counter named {name}"))
}

/// Each run's counters are its own invocation's: two runs at once, on inputs
/// of different sizes, each count exactly their own output, and a second run
/// under the same token adds into the same totals. When these were
/// process-global statics, the concurrent pair saw each other's bytes.
#[test]
fn counters_are_per_invocation() {
    let sizes: Vec<(pbfhogg::host::Invocation, u64)> = std::thread::scope(|scope| {
        let runs: Vec<_> = [3_i64, 40]
            .into_iter()
            .map(|count| {
                scope.spawn(move || {
                    let dir = crate::common::test_scratch();
                    let raw = dir.path().join("raw.pbf");
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
                    let host = crate::common::test_host();
                    let out = dir.path().join("indexed.pbf");
                    pbfhogg::cat::cat(
                        &[&raw],
                        crate::common::out(&out),
                        None,
                        &Default::default(),
                        &Default::default(),
                        &Default::default(),
                        &host,
                    )
                    .expect("cat");
                    let size = std::fs::metadata(&out).expect("output").len();
                    (host.invocation, size)
                })
            })
            .collect();
        runs.into_iter()
            .map(|run| run.join().expect("run"))
            .collect()
    });
    assert_ne!(
        sizes[0].1, sizes[1].1,
        "the two outputs must differ to tell them apart"
    );
    for (invocation, size) in &sizes {
        assert_eq!(counter(invocation, "writer_bytes_written"), *size);
        assert!(counter(invocation, "writer_sync_all_calls") > 0);
    }

    // A second run under the same token is the same invocation.
    let dir = crate::common::test_scratch();
    let base = input(dir.path());
    let mut host = crate::common::test_host();
    let first = crate::common::out(dir.path().join("first.pbf"));
    let second = crate::common::out(dir.path().join("second.pbf"));
    host.invocation = sizes[0].0.clone();
    let before = counter(&host.invocation, "writer_bytes_written");
    for output in [first, second] {
        pbfhogg::cat::cat(
            &[&base],
            output,
            None,
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &host,
        )
        .expect("cat");
    }
    let total = std::fs::metadata(dir.path().join("first.pbf"))
        .expect("first")
        .len()
        + std::fs::metadata(dir.path().join("second.pbf"))
            .expect("second")
            .len();
    assert_eq!(
        counter(&host.invocation, "writer_bytes_written") - before,
        total
    );
}

/// A sink that records what its invocation emitted.
#[derive(Default)]
struct RecordingSink {
    markers: std::sync::Mutex<Vec<String>>,
    counters: std::sync::Mutex<Vec<(String, i64)>>,
}
impl pbfhogg::debug::InstrumentSink for RecordingSink {
    fn marker(&self, name: &str) {
        self.markers.lock().expect("sink").push(name.to_owned());
    }
    fn counter(&self, name: &str, value: i64) {
        self.counters
            .lock()
            .expect("sink")
            .push((name.to_owned(), value));
    }
}

/// Run cat over a fresh input of `count` nodes under `invocation`, then the
/// end-of-invocation emitters, as the CLI does; returns the output size.
fn instrumented_cat(count: i64, invocation: pbfhogg::host::Invocation) -> u64 {
    let dir = crate::common::test_scratch();
    let raw = dir.path().join("raw.pbf");
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
    host.invocation = invocation;
    let out = dir.path().join("indexed.pbf");
    pbfhogg::cat::cat(
        &[&raw],
        crate::common::out(&out),
        None,
        &Default::default(),
        &Default::default(),
        &Default::default(),
        &host,
    )
    .expect("cat");
    pbfhogg::write::emit_writer_counters(&host.invocation);
    pbfhogg::read::emit_read_counters(&host.invocation);
    std::fs::metadata(&out).expect("output").len()
}

/// Each invocation's markers and counters reach its own sink and no other:
/// two concurrent runs, each sink sees its run's markers and its run's
/// `writer_bytes_written`.
#[test]
fn instrumentation_reaches_only_the_invocations_own_sink() {
    let results: Vec<(Arc<RecordingSink>, u64)> = std::thread::scope(|scope| {
        let runs: Vec<_> = [3_i64, 40]
            .into_iter()
            .map(|count| {
                scope.spawn(move || {
                    let sink = Arc::new(RecordingSink::default());
                    let invocation = pbfhogg::host::Invocation::with_sink(
                        Arc::clone(&sink) as Arc<dyn pbfhogg::debug::InstrumentSink>
                    );
                    let size = instrumented_cat(count, invocation);
                    (sink, size)
                })
            })
            .collect();
        runs.into_iter()
            .map(|run| run.join().expect("run"))
            .collect()
    });
    for (sink, size) in &results {
        let markers = sink.markers.lock().expect("sink");
        assert_eq!(
            markers.iter().filter(|m| *m == "CAT_SCAN_START").count(),
            1,
            "{markers:?}"
        );
        let counters = sink.counters.lock().expect("sink");
        let written: Vec<i64> = counters
            .iter()
            .filter(|(name, _)| name == "writer_bytes_written")
            .map(|(_, value)| *value)
            .collect();
        assert_eq!(written, [i64::try_from(*size).expect("size")]);
    }
}

/// Two concurrent calls write one real FIFO through their own `CallSink`s
/// beside the untagged process stream: every line is whole, at most 512
/// bytes, carries its own call's tag, and each call's stream holds its own
/// markers and its own `writer_bytes_written`. A 512-byte tagged line goes
/// out; a 513-byte one does not.
#[cfg(unix)]
#[test]
fn call_sinks_share_one_fifo_and_keep_their_lines_apart() {
    use pbfhogg::debug::{CallId, InstrumentSink, MarkerFifo};
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = crate::common::test_scratch();
    let path = dir.path().join("sidecar.fifo");
    let c_path = std::ffi::CString::new(path.to_str().expect("utf-8 path")).expect("c path");
    // SAFETY: a valid NUL-terminated path in the test's own scratch.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
    // The read end first: a non-blocking write open needs a reader.
    let mut reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .expect("read end");
    enlarge_pipe(&reader);

    let fifo = MarkerFifo::open(&path).expect("write end");
    // Drain while the calls write, so a small pipe capacity cannot drop
    // lines; EOF arrives once every write handle has dropped.
    let drain = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => panic!("drain FIFO: {e}"),
            }
        }
        String::from_utf8(bytes).expect("utf-8 stream")
    });
    let start = fifo.now_us();
    fifo.marker("PROCESS_START");
    let sizes: Vec<(String, u64, (usize, u64))> = std::thread::scope(|scope| {
        let runs: Vec<_> = [("small", 3_i64), ("large", 40)]
            .into_iter()
            .map(|(id, count)| {
                let sink = fifo
                    .call(CallId::new(id).expect("call id"))
                    .expect("a fresh id");
                scope.spawn(move || {
                    let invocation = pbfhogg::host::Invocation::with_call_sink(sink, None);
                    let size = instrumented_cat(count, invocation.clone());
                    let pbfhogg::host::SealOutcome::Sealed(snapshot) = invocation.seal() else {
                        panic!("{id}: nothing of the run is left to hold it open");
                    };
                    let account = invocation.stream_account().expect("a call sink");
                    assert_eq!(account.state, pbfhogg::debug::StreamState::Sealed, "{id}");
                    (id.to_owned(), size, (snapshot.len(), account.drops))
                })
            })
            .collect();
        runs.into_iter()
            .map(|run| run.join().expect("run"))
            .collect()
    });
    assert!(
        fifo.call(CallId::new("small").expect("call id")).is_err(),
        "an id is issued once"
    );
    let edge_whole = write_edge_lines(&fifo);
    assert!(fifo.now_us() >= start, "the clock is monotonic");
    drop(fifo);

    let text = drain.join().expect("drain");
    let lines: Vec<&str> = text.lines().collect();
    assert!(text.ends_with('\n'), "no fragment");
    for line in &lines {
        assert!(line.len() < 512, "line over budget: {}", line.len());
    }
    assert!(
        lines
            .iter()
            .any(|l| l.ends_with(" PROCESS_START") && !l.contains('%'))
    );
    assert!(
        !edge_whole
            || lines
                .iter()
                .any(|l| l.contains(" %eeee") && l.ends_with('n') && l.len() == 511),
        "the 512-byte tagged line, newline included, goes out whole"
    );
    assert!(
        !lines.iter().any(|l| l.ends_with('o')),
        "513 bytes must drop"
    );
    for (id, size, sealed) in &sizes {
        assert_own_sealed_stream(&lines, id, *size, *sealed);
    }
}

/// Write a 512-byte and a 513-byte tagged line on a call of 64-byte id;
/// whether nothing but the 513-byte lines was dropped.
#[cfg(unix)]
fn write_edge_lines(fifo: &pbfhogg::debug::MarkerFifo) -> bool {
    use pbfhogg::debug::{CallId, InstrumentSink};
    let edge = fifo
        .call(CallId::new(&"e".repeat(64)).expect("call id"))
        .expect("a fresh id");
    // The width of the timestamp the line will carry: retry if the clock
    // crossed a power of ten between sizing and emitting (that attempt's
    // line is 513 bytes and dropped).
    let mut attempts = 0;
    for _ in 0..5 {
        attempts += 1;
        let before = fifo.now_us();
        let fixed = format!("{before} %{} \n", edge.id().as_str()).len();
        edge.marker(&"n".repeat(512 - fixed));
        edge.marker(&"o".repeat(513 - fixed));
        if fifo.now_us().to_string().len() == before.to_string().len() {
            break;
        }
    }
    // Each attempt drops its 513-byte line; any further drop was a full
    // FIFO, which can take the 512-byte line too.
    edge.account().drops == attempts
}

/// Room for every line a test writes into the pipe `end` belongs to, where
/// the host allows it, so a drain that falls behind does not drop lines. A
/// refusal leaves the default size.
#[cfg(unix)]
fn enlarge_pipe(end: &std::fs::File) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: an fcntl on a descriptor the caller owns.
        let _size = unsafe { libc::fcntl(end.as_raw_fd(), libc::F_SETPIPE_SZ, 1 << 20) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = end;
}

/// Call `id`'s stream holds its own markers and `writer_bytes_written`, and
/// the seal ended it: every snapshot counter as a sealing emission, then the
/// terminal record, last, with no ordinary record after sealing began.
/// `sealed` is the snapshot's size and the account's drops; with drops
/// counted (the FIFO filled), only what no drop excuses is checked.
fn assert_own_sealed_stream(lines: &[&str], id: &str, size: u64, sealed: (usize, u64)) {
    let (sealed, drops) = sealed;
    let tag = format!(" %{id} ");
    let mine: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.split_once(&tag).map(|(_, body)| body))
        .collect();
    let scans = mine.iter().filter(|b| **b == "CAT_SCAN_START").count();
    let written: Vec<&str> = mine
        .iter()
        .filter_map(|b| b.strip_prefix("@writer_bytes_written="))
        .collect();
    if drops == 0 {
        assert_eq!(scans, 1, "{id}: {mine:?}");
        assert_eq!(written, [size.to_string()], "{id}");
    } else {
        assert!(scans <= 1, "{id}: {mine:?}");
        assert!(written.iter().all(|w| *w == size.to_string()), "{id}");
    }
    // Whatever was dropped: one terminal record at most, last, carrying the
    // account, and no ordinary record after a sealing one.
    let terminals: Vec<&&str> = mine.iter().filter(|b| b.starts_with('!')).collect();
    assert!(terminals.len() <= 1, "{id}: {mine:?}");
    if let Some(terminal) = terminals.first() {
        assert_eq!(**terminal, format!("!sealed drops={drops}"), "{id}");
        assert_eq!(mine.last(), Some(*terminal), "{id}: {mine:?}");
    }
    if let Some(first) = mine.iter().position(|b| b.starts_with("@@")) {
        assert!(
            mine[first..]
                .iter()
                .all(|b| b.starts_with("@@") || b.starts_with('!')),
            "{id}: an ordinary record after sealing began"
        );
    }
    if drops == 0 {
        let emissions = mine.iter().filter(|b| b.starts_with("@@")).count();
        assert_eq!(emissions, sealed, "{id}");
        assert_eq!(terminals.len(), 1, "{id}: {mine:?}");
    }
}

/// A sink that panics is switched off, and the run it observes completes.
///
/// The sink unwinds with `resume_unwind`, which does not run the panic hook:
/// a `panic!` on a pool thread prints its message past libtest's capture,
/// and this test shares the `all` binary, so it cannot install a quiet
/// process-wide hook. The library's containment (`catch_unwind` in
/// `Invocation::deliver`) sees the same unwind either way. The payload
/// counts its own drops: the library must forget a caught payload, not drop
/// it, because the payload's destructor is caller code too.
#[test]
fn a_panicking_sink_does_not_fail_the_run() {
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Relaxed);
        }
    }
    struct Panicking {
        calls: AtomicUsize,
        payload_drops: Arc<AtomicUsize>,
    }
    impl Panicking {
        fn fail(&self) -> ! {
            self.calls.fetch_add(1, Relaxed);
            std::panic::resume_unwind(Box::new(Payload(Arc::clone(&self.payload_drops))))
        }
    }
    impl pbfhogg::debug::InstrumentSink for Panicking {
        fn marker(&self, _name: &str) {
            self.fail()
        }
        fn counter(&self, _name: &str, _value: i64) {
            self.fail()
        }
    }
    let sink = Arc::new(Panicking {
        calls: AtomicUsize::new(0),
        payload_drops: Arc::new(AtomicUsize::new(0)),
    });
    let invocation = pbfhogg::host::Invocation::with_sink(
        Arc::clone(&sink) as Arc<dyn pbfhogg::debug::InstrumentSink>
    );
    assert!(invocation.instrumented());
    assert!(instrumented_cat(5, invocation.clone()) > 0);
    assert!(!invocation.instrumented(), "the sink must be latched off");
    assert_eq!(sink.calls.load(Relaxed), 1, "no call after the first panic");
    assert_eq!(
        sink.payload_drops.load(Relaxed),
        0,
        "a caught sink panic's payload must be forgotten, not dropped"
    );
}

/// Records the publication events of its invocation, and holds every
/// `PUBLISH_START` until a second one arrives or `PUBLISH_OVERLAP_WAIT`
/// passes, so two publications that could overlap do.
struct PublishOrder {
    events: std::sync::Mutex<Vec<(String, Option<i64>)>>,
    starts: std::sync::Mutex<usize>,
    started: std::sync::Condvar,
}
/// Long enough for the second publication, already past its hooks, to reach
/// `PUBLISH_START` if nothing holds it back.
const PUBLISH_OVERLAP_WAIT: std::time::Duration = std::time::Duration::from_millis(500);
impl pbfhogg::debug::InstrumentSink for PublishOrder {
    fn marker(&self, name: &str) {
        if !name.starts_with("PUBLISH_") {
            return;
        }
        self.events
            .lock()
            .expect("events")
            .push((name.to_owned(), None));
        if name == "PUBLISH_START" {
            let mut starts = self.starts.lock().expect("starts");
            *starts += 1;
            self.started.notify_all();
            drop(
                self.started
                    .wait_timeout_while(starts, PUBLISH_OVERLAP_WAIT, |n| *n < 2)
                    .expect("starts"),
            );
        }
    }
    fn counter(&self, name: &str, value: i64) {
        if name.starts_with("publish_sync_") {
            self.events
                .lock()
                .expect("events")
                .push((name.to_owned(), Some(value)));
        }
    }
}

/// Two publications of one invocation, at once: each `PUBLISH_START` is
/// followed by its own `publish_sync_calls`, `publish_sync_ns` and
/// `PUBLISH_END` before the other publication's span begins, so a stream
/// reader can pair every sample with its span. Both runs start together, and
/// the sink holds the first span open, so without the invocation's
/// publication execution lock the two spans interleave on the stream. The
/// lock is taken before the decision, so the `before_publish` hooks (which
/// run after it) never overlap either.
#[test]
fn concurrent_publications_of_one_invocation_do_not_interleave() {
    let sink = Arc::new(PublishOrder {
        events: std::sync::Mutex::new(Vec::new()),
        starts: std::sync::Mutex::new(0),
        started: std::sync::Condvar::new(),
    });
    let mut host = crate::common::test_host();
    host.invocation = pbfhogg::host::Invocation::with_sink(
        Arc::clone(&sink) as Arc<dyn pbfhogg::debug::InstrumentSink>
    );
    let dir = crate::common::test_scratch();
    let base = input(dir.path());
    let at_start = Arc::new(std::sync::Barrier::new(2));
    let in_hook = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let overlapped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::scope(|scope| {
        for name in ["a.pbf", "b.pbf"] {
            let at_start = Arc::clone(&at_start);
            let (in_hook, overlapped) = (Arc::clone(&in_hook), Arc::clone(&overlapped));
            let (host, base, output) = (&host, &base, dir.path().join(name));
            scope.spawn(move || {
                use std::sync::atomic::Ordering::SeqCst;
                let output = crate::common::out(output).before_publish(move || {
                    if in_hook.swap(true, SeqCst) {
                        overlapped.store(true, SeqCst);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    in_hook.store(false, SeqCst);
                });
                at_start.wait();
                pbfhogg::cat::cat(
                    &[base],
                    output,
                    None,
                    &Default::default(),
                    &Default::default(),
                    &Default::default(),
                    host,
                )
                .expect("cat");
            });
        }
    });
    assert!(
        !overlapped.load(std::sync::atomic::Ordering::SeqCst),
        "two before_publish hooks of one invocation ran at once"
    );
    let events = sink.events.lock().expect("events");
    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    let span = [
        "PUBLISH_START",
        "publish_sync_calls",
        "publish_sync_ns",
        "PUBLISH_END",
    ];
    assert_eq!(names, [span, span].concat(), "{events:?}");
    let calls: Vec<i64> = events
        .iter()
        .filter(|(name, _)| name == "publish_sync_calls")
        .filter_map(|(_, value)| *value)
        .collect();
    assert!(
        calls.iter().all(|&n| n > 0),
        "each publication syncs its own output: {calls:?}"
    );
    assert!(
        calls.iter().sum::<i64>()
            <= i64::try_from(counter(&host.invocation, "writer_sync_all_calls")).expect("calls"),
        "a publication's syncs are part of its invocation's writer_sync_all_calls"
    );
}

/// The once-only unrecognised-indexdata warning is once per invocation, not
/// per `Diagnostics` handle: readers under separate invocations each warn,
/// even through clones of one handle (two runs given clones of one
/// `Diagnostics` used to suppress each other's warning), and readers under
/// one invocation warn once between them, whichever handle they report to.
#[test]
fn unrecognised_indexdata_warning_is_per_invocation() {
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
    let read = |diagnostics: &pbfhogg::host::Diagnostics,
                invocation: &pbfhogg::host::Invocation| {
        let reader = pbfhogg::BlobReader::from_path(&bad)
            .expect("reader")
            .diagnostics(diagnostics.clone())
            .invocation(invocation.clone());
        for blob in reader {
            blob.expect("blob");
        }
        sink.0.lock().expect("sink").len()
    };
    // Separate invocations, one handle and its clone: each warns.
    assert_eq!(read(&first, &pbfhogg::host::Invocation::new()), 1);
    assert_eq!(read(&first.clone(), &pbfhogg::host::Invocation::new()), 2);
    // One invocation, either handle: one warning between them.
    let shared = pbfhogg::host::Invocation::new();
    assert_eq!(read(&first, &shared), 3);
    assert_eq!(read(&second, &shared), 3);
    assert_eq!(read(&first, &shared.clone()), 3);
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
/// The real-refusal test of the same contract
/// (`external_backend_output_refusal_stays_typed_across_stage_join`, below)
/// cannot force the refusal without a host assumption, so the fault plan
/// injects it at the one place a backend becomes a writer. Buffered reads, so no genuine input-side refusal can stand in for
/// the injected one; its source message proves which refusal arrived.
#[cfg(feature = "test-hooks")]
#[test]
fn external_join_stage4_backend_refusal_stays_typed_across_the_join() {
    let dir = crate::common::test_scratch();
    let base = input(dir.path());
    let host = crate::common::test_host();
    let faults = host.invocation.faults();
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

/// The same stage 4 contract on the real O_DIRECT refusal path.
///
/// This drives the library with a direct output and buffered reads
/// (`direct_reads: false`), a combination the CLI cannot express (its
/// `--direct-io` sets both, so an input-side refusal would fire first on the
/// main thread and never reach stage 4). With reads buffered, the only way
/// the run can fail with a refusal is stage 4's output open. A host whose
/// filesystem accepts O_DIRECT output completes the run instead, so the test
/// cannot force the refusal there; it then announces a skip without
/// exercising the join, and only a failure is held to the contract. The
/// injected twin above covers the join on every host with `test-hooks`.
#[cfg(feature = "linux-direct-io")]
#[test]
fn external_backend_output_refusal_stays_typed_across_stage_join() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let nodes = crate::common::generate_nodes(6, 1);
    let mut ways = crate::common::generate_ways(1, 1_000, 3, 1);
    ways[0].refs = vec![1, 2, 3];
    crate::common::write_multi_block_test_pbf(&input, &nodes, &ways, &[], 3);

    let result = pbfhogg::altw::add_locations_to_ways(
        &input,
        crate::common::out(&output),
        &pbfhogg::altw::AltwOptions {
            keep_untagged_nodes: true,
            write: pbfhogg::write::WriteOptions {
                backend: pbfhogg::write::OutputBackend::Direct,
                direct_reads: false,
                ..pbfhogg::write::WriteOptions::default()
            },
            index_type: pbfhogg::altw::IndexType::External,
            inject_prepass: false,
            ignore_missing_nodes: false,
        },
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
        crate::common::scratch_in(dir.path()),
    );
    match result {
        Ok(_) => crate::common::skip("O_DIRECT output accepted on this filesystem"),
        Err(error) => assert!(
            matches!(
                error.kind(),
                pbfhogg::ErrorKind::PlatformUnsupported {
                    feature: "direct-io",
                    ..
                }
            ),
            "an output-side O_DIRECT refusal must stay typed; got: {}",
            error.report(),
        ),
    }
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
        fn fd_allowance(&self, _wanted: u64) -> FdReservation {
            self.0.wait();
            FdReservation::unreserved(pbfhogg::process::current_nofile())
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

// `has_indexdata` is a `commands` export. The module groups the tests under
// their feature gate only: they are cheap and skip on a typed O_DIRECT
// refusal, so they run in tier 1 (reference/testing.md "Test placement").
#[cfg(all(feature = "linux-direct-io", feature = "commands"))]
mod direct_read {
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
            Err(error) if common::is_einval(&error) => {
                common::skip(format!("O_DIRECT not supported: {error}"));
                return;
            }
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
                Err(error) if common::is_einval(&error) => {
                    common::skip(format!("O_DIRECT not supported: {error}"));
                    return;
                }
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
                Err(error) if common::is_einval(&error) => {
                    common::skip(format!("O_DIRECT not supported: {error}"));
                    return;
                }
                Err(error) => error,
            };
        assert_unexpected_eof(&error);
    }
}
