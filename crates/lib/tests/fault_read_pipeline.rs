//! Fault injection into the pipelined reader's decode admission, through
//! the decode gate in the reader's token's fault plan (`pbfhogg::test_hooks`).
//! The reader is handed that token explicitly (`ElementReader::invocation`); a
//! reader built without one would mint its own and never see the gate.

#![allow(clippy::unwrap_used)]

use std::sync::mpsc;
use std::thread::JoinHandle;

use pbfhogg::ElementReader;
use pbfhogg::host::Invocation;

use crate::common::write_many_block_pbf;

/// Owns the driver thread from the moment it is spawned. Releasing the
/// parked decode and joining happen on every exit path, a failed assertion
/// included, so a failure never leaves a worker parked at the gate.
struct Driver {
    cancel: Invocation,
    handle: Option<JoinHandle<()>>,
}

impl Driver {
    fn release_and_join(&mut self) {
        self.cancel.faults().release_blocked_decode();
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            eprintln!("pipelined-read driver thread panicked");
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.release_and_join();
    }
}

/// The invocation's own `pipeline_reorder_high_water` counter: the most
/// filled reorder slots any pipelined read under it has held.
fn reorder_high_water(invocation: &Invocation) -> u64 {
    let mut value = 0;
    invocation.for_each_counter(|name, v| {
        if name == "pipeline_reorder_high_water" {
            value = v;
        }
    });
    value
}

#[test]
fn admission_high_water_bounded_under_slow_first_decode() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("many.osm.pbf");
    write_many_block_pbf(&path, 16);

    let cancel = Invocation::new();
    let faults = cancel.faults();
    faults.arm_block_decode(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let reader_cancel = cancel.clone();
    let mut driver = Driver {
        cancel: cancel.clone(),
        handle: Some(std::thread::spawn(move || {
            let result = ElementReader::from_path(&path)
                .unwrap()
                .invocation(reader_cancel)
                .read_ahead(1)
                .decode_ahead(2)
                .for_each_block_pipelined(|_| Ok(()));
            done_tx.send(result).unwrap();
        })),
    };

    assert!(
        faults.wait_for(|| faults.blocked_decode_ready()),
        "decode hook was not reached"
    );
    // The counter does not ring the plan: the wait's tick re-reads it.
    assert!(
        faults.wait_for(|| reorder_high_water(&cancel) >= 1),
        "later decode did not reach the reorder buffer"
    );

    faults.release_blocked_decode();
    let result = done_rx
        .recv_timeout(pbfhogg::TEST_HANG_BOUND)
        .expect("pipeline did not finish after releasing decode hook");
    driver.release_and_join();
    result.unwrap();

    assert!(reorder_high_water(&cancel) <= 2);
}
