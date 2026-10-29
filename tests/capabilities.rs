//! The invocation's capabilities at rest: every public entry holds a call
//! guard for its extent, every value that can record outside a public call
//! holds a permit, every value of a census kind issued to the caller holds a
//! ticket, and nothing else does. At rest (no call in
//! progress) a live permit is therefore always accounted for by a live
//! ticket, so a seal can never find permits it cannot attribute.
//!
//! Read through the `test-hooks` accessor `Invocation::capabilities`.

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::sync::mpsc;

use pbfhogg::host::{Capabilities, Invocation};

/// At rest: no guard, and any permit is attributable to an issued value.
fn assert_quiescent(invocation: &Invocation) -> Capabilities {
    let c = invocation.capabilities();
    assert_eq!(c.guards, 0, "no call is in progress: {c:?}");
    assert!(!c.saturated, "{c:?}");
    assert!(
        c.permits == 0 || c.session + c.writer + c.output + c.buffer > 0,
        "a permit with nothing issued to account for it: {c:?}"
    );
    c
}

fn base_bytes() -> Vec<u8> {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("base.osm.pbf");
    crate::common::write_base_pbf(&path);
    std::fs::read(path).unwrap()
}

/// A caller's stream: small reads, and every read off the thread that built
/// it parks until released, so the block iterator's background pipeline
/// blocks in the caller's `read`.
struct ParkingRead {
    bytes: std::io::Cursor<Vec<u8>>,
    owner: std::thread::ThreadId,
    parked: mpsc::SyncSender<()>,
    release: std::sync::Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
    released: bool,
}

impl Read for ParkingRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.released && std::thread::current().id() != self.owner {
            self.parked.try_send(()).ok();
            let release = self
                .release
                .lock()
                .map_err(|_| std::io::Error::other("poisoned"))?;
            release.recv().ok();
            self.released = true;
        }
        let n = buf.len().min(64);
        self.bytes.read(&mut buf[..n])
    }
}

/// A block iterator held by the caller while its pipeline is blocked in the
/// caller's stream holds the pipeline's permit and its session ticket, and
/// no call guard. Exhausted, it holds the ticket and no permit; its blocks,
/// kept past the end of the pipeline, hold the pool's permit and one buffer
/// ticket each until they are dropped.
#[test]
fn a_block_iterator_blocked_on_the_callers_stream() {
    let invocation = Invocation::new();
    let (parked_tx, parked_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel();
    let stream = ParkingRead {
        bytes: std::io::Cursor::new(base_bytes()),
        owner: std::thread::current().id(),
        parked: parked_tx,
        release: std::sync::Arc::new(std::sync::Mutex::new(release_rx)),
        released: false,
    };
    let mut blocks = pbfhogg::ElementReader::new(stream)
        .unwrap()
        .invocation(invocation.clone())
        .into_blocks_pipelined();
    parked_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the pipeline reached the caller's stream");
    let held = assert_quiescent(&invocation);
    assert!(held.permits >= 1, "{held:?}");
    assert_eq!((held.session, held.buffer), (1, 0), "{held:?}");

    release_tx.send(()).unwrap();
    let kept: Vec<_> = blocks.by_ref().map(Result::unwrap).collect();
    assert!(!kept.is_empty());
    assert!(blocks.next().is_none());
    let finished = assert_quiescent(&invocation);
    assert_eq!(finished.session, 1);
    assert_eq!(finished.buffer, kept.len() as u64, "{finished:?}");
    assert_eq!(finished.permits, 1, "the pool's permit only: {finished:?}");

    drop(kept);
    let exhausted = assert_quiescent(&invocation);
    assert_eq!(
        (exhausted.permits, exhausted.session, exhausted.buffer),
        (0, 1, 0),
        "an exhausted iterator records nothing more: {exhausted:?}"
    );
    drop(blocks);
    assert_eq!(invocation.capabilities(), Capabilities::default());
}

/// A block handed to a block callback is issued: kept past the call, it
/// keeps the pool's permit and its buffer ticket. Elements lent by
/// `for_each_pipelined` issue nothing.
#[test]
fn blocks_kept_from_a_callback_are_issued() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("base.osm.pbf");
    crate::common::write_base_pbf(&path);
    let invocation = Invocation::new();
    let mut kept = Vec::new();
    pbfhogg::ElementReader::from_path(&path)
        .unwrap()
        .invocation(invocation.clone())
        .for_each_block_pipelined(|block| {
            kept.push(block);
            Ok(())
        })
        .unwrap();
    let c = assert_quiescent(&invocation);
    assert_eq!((c.buffer, c.permits), (kept.len() as u64, 1), "{c:?}");
    drop(kept);
    assert_eq!(invocation.capabilities(), Capabilities::default());

    let mut elements = 0_u64;
    pbfhogg::ElementReader::from_path(&path)
        .unwrap()
        .invocation(invocation.clone())
        .for_each_pipelined(|_| elements += 1)
        .unwrap();
    assert!(elements > 0);
    assert_eq!(invocation.capabilities(), Capabilities::default());
}

/// A caller's `Write` wrapping an issued writer.
struct Adapter {
    _inner: pbfhogg::writer::PbfWriter<Vec<u8>>,
}

impl Write for Adapter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Two issued writers, one inside the other, are two values in the census.
/// Consuming the outer one retires its permit and ticket only; the inner
/// one, moved out through the caller's wrapper, keeps its own.
#[test]
fn a_writer_nested_in_a_callers_writer_is_counted_once_each() {
    let invocation = Invocation::new();
    let compression = pbfhogg::writer::Compression::None;
    let inner = pbfhogg::writer::PbfWriter::new_in(Vec::new(), compression, invocation.clone());
    let outer = pbfhogg::writer::PbfWriter::new_in(
        Adapter { _inner: inner },
        compression,
        invocation.clone(),
    );
    let both = assert_quiescent(&invocation);
    assert_eq!((both.writer, both.permits), (2, 2), "{both:?}");
    let adapter = outer.into_inner().unwrap();
    let one = assert_quiescent(&invocation);
    assert_eq!((one.writer, one.permits), (1, 1), "{one:?}");
    drop(adapter);
    assert_eq!(invocation.capabilities(), Capabilities::default());
}

/// A bound output is issued at `bind` and keeps one ticket however it is
/// passed around; a command handed it consumes it. Dropped after a cancel,
/// it was decided by the cancel before its permit and ticket retired.
#[test]
fn bound_outputs_are_issued_once() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("base.osm.pbf");
    crate::common::write_base_pbf(&input);
    let invocation = Invocation::new();
    let host = pbfhogg::host::Host {
        invocation: invocation.clone(),
        ..crate::common::test_host()
    };

    let bound = crate::common::out(dir.path().join("sorted.osm.pbf")).bind(&invocation);
    let c = assert_quiescent(&invocation);
    assert_eq!((c.output, c.permits), (1, 1), "{c:?}");
    pbfhogg::sort::sort(
        &input,
        bound,
        &Default::default(),
        &Default::default(),
        &host,
    )
    .unwrap();
    assert_eq!(invocation.capabilities(), Capabilities::default());

    let pending = crate::common::out(dir.path().join("never.osm.pbf")).bind(&invocation);
    let _outcome = invocation.cancel();
    let mut decided = 0;
    invocation.for_each_publication(|row| {
        if row.decision() == pbfhogg::Decision::RefusedCancelled
            && row
                .members()
                .iter()
                .any(|m| m.destination().ends_with("never.osm.pbf"))
        {
            decided += 1;
        }
    });
    assert_eq!(decided, 1, "decided while its owner is still held");
    let c = assert_quiescent(&invocation);
    assert_eq!((c.output, c.permits), (1, 1), "{c:?}");
    drop(pending);
    assert_eq!(invocation.capabilities(), Capabilities::default());
}
