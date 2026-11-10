//! Sealing an invocation (ADR-0020): the seal's refusals, its
//! finalization's two regions, and the call stream it ends, read back from a
//! real FIFO.
//!
//! Finalization is observed through the `test-hooks` seal observer
//! (`FaultPlan::on_seal_step`), which parks or unwinds the finalizing thread
//! at a named step. Deliberate unwinds use `resume_unwind`, which does not
//! run the panic hook: this module shares the `all` binary and cannot
//! install a quiet process-wide hook.

#![allow(clippy::unwrap_used)]

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, mpsc};

use pbfhogg::debug::{CallId, InstrumentSink, MarkerFifo, StreamAccount, StreamState};
use pbfhogg::host::{Invocation, SealOutcome, SealRefusal};
use pbfhogg::test_hooks::SealStep;

/// A FIFO in the test's scratch, drained on a thread until every write end
/// has dropped; the drain returns everything it read.
struct Capture {
    _dir: tempfile::TempDir,
    fifo: MarkerFifo,
    drain: std::thread::JoinHandle<String>,
}

impl Capture {
    fn new() -> Self {
        let dir = crate::common::test_scratch();
        let path = dir.path().join("sidecar.fifo");
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path in the test's own scratch.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
        let mut reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        // Room for everything a test writes, where the host allows it, so
        // the drain rarely matters. A test still never assumes nothing was
        // dropped: see `assert_ended`.
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: an fcntl on a descriptor this function owns. A refusal
            // (a host capping pipe sizes) leaves the default, which is fine.
            let _size = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETPIPE_SZ, 1 << 20) };
        }
        let fifo = MarkerFifo::open(&path).unwrap();
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
            String::from_utf8(bytes).unwrap()
        });
        Self {
            _dir: dir,
            fifo,
            drain,
        }
    }

    /// An invocation owning a fresh call sink tagged `id`.
    fn invocation(&self, id: &str, tap: Option<Arc<dyn InstrumentSink>>) -> Invocation {
        let sink = self.fifo.call(CallId::new(id).unwrap()).unwrap();
        Invocation::with_call_sink(sink, tap)
    }

    /// The bodies call `id` wrote, in arrival order. Every invocation (and
    /// so every call sink) must be dropped first.
    fn finish(self, id: &str) -> Vec<String> {
        drop(self.fifo);
        let text = self.drain.join().unwrap();
        let tag = format!(" %{id} ");
        text.lines()
            .filter_map(|l| l.split_once(&tag).map(|(_, body)| body.to_owned()))
            .collect()
    }
}

/// Cat a small generated input under `invocation`, then the end-of-run
/// emitters, as a harness call does.
fn run_cat(invocation: &Invocation) -> pbfhogg::Result<()> {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("base.osm.pbf");
    crate::common::write_base_pbf(&input);
    let host = pbfhogg::host::Host {
        invocation: invocation.clone(),
        ..crate::common::test_host()
    };
    let result = pbfhogg::cat::cat(
        &[&input],
        crate::common::out(dir.path().join("out.osm.pbf")),
        None,
        &Default::default(),
        &Default::default(),
        &Default::default(),
        &host,
    )
    .map(drop);
    pbfhogg::write::emit_writer_counters(invocation);
    pbfhogg::read::emit_read_counters(invocation);
    result
}

fn account(invocation: &Invocation) -> StreamAccount {
    invocation.stream_account().unwrap()
}

/// The stream's sealing part: every body from the first sealing emission.
fn sealing_tail(bodies: &[String]) -> &[String] {
    let first = bodies
        .iter()
        .position(|b| b.starts_with("@@") || b.starts_with('!'))
        .unwrap_or(bodies.len());
    &bodies[first..]
}

/// The stream ended `state` and the wire agrees with the account. With no
/// drop counted: every snapshot counter's sealing emission, then the
/// terminal record, last, and nothing ordinary after sealing began. Drops
/// or not (the FIFO filled), what can never happen: a terminal record that
/// disagrees with the account, a second one, anything after one, or an
/// ordinary record after a sealing one.
fn assert_ended(bodies: &[String], account: StreamAccount, sealed: usize, state: &str) {
    let expected_state = if state == "sealed" {
        StreamState::Sealed
    } else {
        StreamState::Failed
    };
    assert_eq!(account.state, expected_state, "{bodies:?}");
    let tail = sealing_tail(bodies);
    let terminal = format!("!{state} drops={}", account.drops);
    let terminals: Vec<usize> = bodies
        .iter()
        .enumerate()
        .filter(|(_, b)| b.starts_with('!'))
        .map(|(i, _)| i)
        .collect();
    match terminals.as_slice() {
        [] => assert!(account.drops > 0, "a terminal record missing with no drop"),
        [at] => {
            assert_eq!(bodies[*at], terminal, "{bodies:?}");
            assert_eq!(*at + 1, bodies.len(), "nothing follows the terminal record");
        }
        _ => panic!("more than one terminal record: {bodies:?}"),
    }
    assert!(
        tail.iter()
            .all(|b| b.starts_with("@@") || b.starts_with('!')),
        "an ordinary record after sealing began: {tail:?}"
    );
    if account.drops == 0 {
        assert_eq!(tail.len(), sealed + 1, "{tail:?}");
    }
}

/// While one seal parks in its finalization (after the sealing emissions,
/// before the terminal record), a second is refused as `Finalizing`, cancel
/// finds the invocation ended, and the account is not yet frozen. Released,
/// the first seals, and a later seal returns its snapshot.
#[test]
fn concurrent_sealers_are_refused_until_the_account_is_frozen() {
    let capture = Capture::new();
    let invocation = capture.invocation("c", None);
    run_cat(&invocation).unwrap();
    let (reached_tx, reached_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    invocation.faults().on_seal_step(move |step| {
        if step == SealStep::Stream {
            reached_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
    });
    let first = {
        let invocation = invocation.clone();
        std::thread::spawn(move || invocation.seal())
    };
    reached_rx.recv().unwrap();
    assert!(matches!(
        invocation.seal(),
        SealOutcome::Refused(SealRefusal::Finalizing)
    ));
    assert_eq!(invocation.cancel(), pbfhogg::CancelOutcome::Ended);
    assert_eq!(
        account(&invocation).state,
        StreamState::Open,
        "not frozen yet"
    );
    release_tx.send(()).unwrap();
    let SealOutcome::Sealed(snapshot) = first.join().unwrap() else {
        panic!("the first sealer seals");
    };
    let SealOutcome::Sealed(again) = invocation.seal() else {
        panic!("a later seal returns the stored result");
    };
    assert!(Arc::ptr_eq(&snapshot, &again));
    let ended = account(&invocation);
    drop(invocation);
    let bodies = capture.finish("c");
    assert_ended(&bodies, ended, snapshot.len(), "sealed");
}

/// A tap that sees the run's markers, and does something on the first one
/// named `on`.
struct Tap {
    on: &'static str,
    calls: AtomicUsize,
    action: Box<dyn Fn() + Send + Sync>,
}

impl InstrumentSink for Tap {
    fn marker(&self, name: &str) {
        self.calls.fetch_add(1, SeqCst);
        if name == self.on {
            (self.action)();
        }
    }
    fn counter(&self, _name: &str, _value: i64) {
        self.calls.fetch_add(1, SeqCst);
    }
}

/// A seal from inside a callback of a call is a call in progress. The tap
/// holds the invocation only through a slot the test clears afterwards.
#[test]
fn a_seal_from_a_callback_is_refused() {
    let capture = Capture::new();
    let slot: Arc<Mutex<Option<Invocation>>> = Arc::default();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let tap = Arc::new(Tap {
        on: "CAT_SCAN_START",
        calls: AtomicUsize::new(0),
        action: Box::new({
            let (slot, seen) = (Arc::clone(&slot), Arc::clone(&seen));
            move || {
                if let Some(invocation) = slot.lock().unwrap().as_ref() {
                    seen.lock()
                        .unwrap()
                        .push(format!("{:?}", invocation.seal()));
                }
            }
        }),
    });
    let invocation = capture.invocation("t", Some(tap as Arc<dyn InstrumentSink>));
    *slot.lock().unwrap() = Some(invocation.clone());
    run_cat(&invocation).unwrap();
    slot.lock().unwrap().take();
    assert_eq!(*seen.lock().unwrap(), ["Refused(InProgress)"]);
    let SealOutcome::Sealed(snapshot) = invocation.seal() else {
        panic!("the call returned");
    };
    let ended = account(&invocation);
    drop(invocation);
    let bodies = capture.finish("t");
    assert_ended(&bodies, ended, snapshot.len(), "sealed");
}

/// The in-flight barrier is the word: while a callback of the run is parked
/// inside an emission, a seal is refused as a call in progress; once the
/// run has returned, the seal ends the stream, and no ordinary record
/// follows its first sealing emission.
#[test]
fn an_emission_in_flight_holds_the_seal_off() {
    let capture = Capture::new();
    let (parked_tx, parked_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let tap = Arc::new(Tap {
        on: "CAT_SCAN_START",
        calls: AtomicUsize::new(0),
        action: Box::new(move || {
            parked_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }),
    });
    let invocation = capture.invocation("f", Some(tap as Arc<dyn InstrumentSink>));
    let run = {
        let invocation = invocation.clone();
        std::thread::spawn(move || run_cat(&invocation))
    };
    parked_rx.recv().unwrap();
    assert!(matches!(
        invocation.seal(),
        SealOutcome::Refused(SealRefusal::InProgress)
    ));
    assert_eq!(account(&invocation).state, StreamState::Open);
    release_tx.send(()).unwrap();
    run.join().unwrap().unwrap();
    let SealOutcome::Sealed(snapshot) = invocation.seal() else {
        panic!("the run returned");
    };
    let ended = account(&invocation);
    drop(invocation);
    let bodies = capture.finish("f");
    // Ordinary records after the parked emission reached the sink, all of
    // them ahead of the sealing records.
    let scan = bodies.iter().position(|b| b == "CAT_SCAN_END");
    let first_sealing = bodies.iter().position(|b| b.starts_with("@@"));
    if let (Some(scan), Some(first_sealing)) = (scan, first_sealing) {
        assert!(scan < first_sealing, "{bodies:?}");
    }
    assert_ended(&bodies, ended, snapshot.len(), "sealed");
}

/// A caller's stream whose destructor attempts a seal of the invocation
/// reading it, and records what it got.
struct SealingOnDrop {
    bytes: std::io::Cursor<Vec<u8>>,
    invocation: Invocation,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Read for SealingOnDrop {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.bytes.read(buf)
    }
}

impl Drop for SealingOnDrop {
    fn drop(&mut self) {
        let outcome = format!("{:?}", self.invocation.seal());
        self.seen.lock().unwrap().push(outcome);
    }
}

/// A public entry's admission covers the destruction of what it consumed:
/// the caller's stream, dropped inside `for_each` (returning or unwinding
/// out of the caller's callback), sees a seal refused as a call in
/// progress.
#[test]
fn a_consumed_stream_is_destroyed_inside_the_admission() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("base.osm.pbf");
    crate::common::write_base_pbf(&path);
    let bytes = std::fs::read(&path).unwrap();
    let invocation = Invocation::new();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let reader = |bytes: &Vec<u8>| {
        let stream = SealingOnDrop {
            bytes: std::io::Cursor::new(bytes.clone()),
            invocation: invocation.clone(),
            seen: Arc::clone(&seen),
        };
        pbfhogg::ElementReader::new(stream)
            .unwrap()
            .invocation(invocation.clone())
    };
    reader(&bytes).for_each(|_| {}).unwrap();
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reader(&bytes).for_each(|_| std::panic::resume_unwind(Box::new("callback")))
    }));
    assert!(unwound.is_err());
    assert_eq!(
        *seen.lock().unwrap(),
        ["Refused(InProgress)", "Refused(InProgress)"]
    );
    assert!(matches!(invocation.seal(), SealOutcome::Sealed(_)));
}

/// A tap that panics on `CAT_SCAN_START`, counting every callback.
struct PanickingTap {
    calls: AtomicUsize,
    /// The callback count when it panicked.
    at_panic: AtomicUsize,
}

impl PanickingTap {
    fn seen(&self, name: &str) {
        let n = self.calls.fetch_add(1, SeqCst) + 1;
        if name == "CAT_SCAN_START" {
            self.at_panic.store(n, SeqCst);
            std::panic::resume_unwind(Box::new("tap"));
        }
    }
}

impl InstrumentSink for PanickingTap {
    fn marker(&self, name: &str) {
        self.seen(name);
    }
    fn counter(&self, name: &str, _value: i64) {
        self.seen(name);
    }
}

/// A tap that panics is latched off on its own: it is never called again,
/// while the sink keeps receiving the run's later ordinary records, and the
/// seal ends the stream as usual.
#[test]
fn a_panicking_tap_is_latched_off_alone() {
    let capture = Capture::new();
    let tap = Arc::new(PanickingTap {
        calls: AtomicUsize::new(0),
        at_panic: AtomicUsize::new(0),
    });
    let invocation = capture.invocation("p", Some(Arc::clone(&tap) as Arc<dyn InstrumentSink>));
    run_cat(&invocation).unwrap();
    let at_panic = tap.at_panic.load(SeqCst);
    assert!(at_panic > 0, "the tap panicked");
    assert_eq!(
        tap.calls.load(SeqCst),
        at_panic,
        "no callback after the panic"
    );
    assert!(invocation.instrumented(), "the sink is still working");
    let SealOutcome::Sealed(snapshot) = invocation.seal() else {
        panic!("the call returned");
    };
    assert_eq!(tap.calls.load(SeqCst), at_panic, "nothing at sealing");
    let ended = account(&invocation);
    drop(invocation);
    let bodies = capture.finish("p");
    if ended.drops == 0 {
        let scan = bodies.iter().position(|b| b == "CAT_SCAN_START").unwrap();
        assert!(
            bodies[scan + 1..]
                .iter()
                .any(|b| !b.starts_with("@@") && !b.starts_with('!')),
            "ordinary records after the tap's panic reached the sink: {bodies:?}"
        );
    }
    assert_ended(&bodies, ended, snapshot.len(), "sealed");
}

/// A panic before the snapshot fails the finalization: the seal is refused,
/// for good, the invocation admits nothing, and the stream stays open with
/// no sealing record written.
#[test]
fn a_panic_before_the_snapshot_fails_the_finalization() {
    let capture = Capture::new();
    let invocation = capture.invocation("b", None);
    run_cat(&invocation).unwrap();
    invocation.faults().on_seal_step(|step| {
        if step == SealStep::Snapshot {
            std::panic::resume_unwind(Box::new("before the snapshot"));
        }
    });
    for _ in 0..2 {
        assert!(matches!(
            invocation.seal(),
            SealOutcome::Refused(SealRefusal::FinalizationFailed)
        ));
    }
    assert_eq!(invocation.cancel(), pbfhogg::CancelOutcome::Ended);
    assert_eq!(account(&invocation).state, StreamState::Open);
    let refused = run_cat(&invocation).unwrap_err();
    assert!(
        matches!(refused.kind(), pbfhogg::ErrorKind::Sealed),
        "{refused}"
    );
    drop(invocation);
    let bodies = capture.finish("b");
    assert!(
        !bodies
            .iter()
            .any(|b| b.starts_with("@@") || b.starts_with('!')),
        "{bodies:?}"
    );
}

/// A panic after the snapshot is committed seals the invocation anyway; the
/// stream ends `failed`, and its terminal record agrees with the account.
#[test]
fn a_panic_after_the_snapshot_seals_with_a_failed_stream() {
    let capture = Capture::new();
    let invocation = capture.invocation("a", None);
    run_cat(&invocation).unwrap();
    invocation.faults().on_seal_step(|step| {
        if step == SealStep::Stream {
            std::panic::resume_unwind(Box::new("after the snapshot"));
        }
    });
    let SealOutcome::Sealed(snapshot) = invocation.seal() else {
        panic!("committed, so sealed");
    };
    let ended = account(&invocation);
    drop(invocation);
    let bodies = capture.finish("a");
    assert_ended(&bodies, ended, snapshot.len(), "failed");
}
