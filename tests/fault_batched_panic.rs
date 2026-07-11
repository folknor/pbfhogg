//! Worker-panic fault coverage for the gated batch pipeline.

#![cfg(feature = "test-hooks")]
#![allow(clippy::unwrap_used)]

use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::time::Duration;

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
fn batched_worker_panic_reports_error() {
    hooks::reset();
    hooks::PANIC_BATCH_SEQ.store(0, Relaxed);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = ElementReader::new(std::io::Cursor::new(fixture(2)))
            .unwrap()
            .batched_pipeline(true)
            .decode_threads(1)
            .for_each_pipelined(|_| {});
        done_tx.send(result).unwrap();
    });
    let error = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("worker panic path hung")
        .expect_err("hook panic must surface as decode error");
    assert!(error.to_string().contains("decode task panicked"));
    hooks::reset();
}
