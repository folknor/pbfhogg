//! Stalled-worker fault coverage for the gated batch pipeline.

#![cfg(feature = "test-hooks")]
#![allow(clippy::unwrap_used)]

use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use pbfhogg::ElementReader;
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::read::batched_pipeline_test_hooks as hooks;
use pbfhogg::writer::{Compression, PbfWriter};

fn fixture(blobs: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut bytes, Compression::Zlib(6));
        writer
            .write_header(&HeaderBuilder::new().build().unwrap())
            .unwrap();
        for id in 0..blobs {
            let mut block = BlockBuilder::new();
            block.add_node(
                i64::try_from(id + 1).unwrap(),
                500_000_000,
                100_000_000,
                std::iter::empty::<(&str, &str)>(),
                None,
            );
            writer
                .write_primitive_block(block.take().unwrap().unwrap())
                .unwrap();
        }
        writer.flush().unwrap();
    }
    bytes
}

#[test]
fn batched_ordering_bounded_under_stalled_worker() {
    hooks::reset();
    hooks::STALL_BATCH_SEQ.store(0, Relaxed);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut ids = Vec::new();
        let result = ElementReader::new(std::io::Cursor::new(fixture(130)))
            .unwrap()
            .batched_pipeline(true)
            .decode_threads(2)
            .for_each_pipelined(|element| match element {
                pbfhogg::Element::Node(node) => ids.push(node.id()),
                pbfhogg::Element::DenseNode(node) => ids.push(node.id()),
                _ => {}
            });
        done_tx.send((result, ids)).unwrap();
    });
    let start = Instant::now();
    while !hooks::STALLED_READY.load(Relaxed) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "stall hook not reached"
        );
        std::thread::yield_now();
    }
    hooks::RELEASE_STALLED.store(true, Relaxed);
    let (result, ids) = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("stalled pipeline did not finish");
    result.unwrap();
    assert_eq!(ids, (1_i64..=130).collect::<Vec<_>>());
    assert!(
        hooks::reorder_high_water() <= 2,
        "batch reorder grew beyond channel backstop"
    );
    hooks::reset();
}
