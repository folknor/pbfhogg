//! Fault-injection test for the pipelined reader's decode admission.
//!
//! Arms `pbfhogg::read::pipeline_test_hooks`, process-global state that
//! every pipelined read in the process consults: `arm_block_decode` parks
//! whichever decode reaches that sequence number, and the reorder
//! high-water marks are a max over every concurrent pipeline. That is why
//! this test owns its process (its own `[[test]]` target in `Cargo.toml`):
//! it used to live in `read_paths.rs` beside some thirty pipelined-reader
//! tests, any of which could park on the armed hook or feed the high-water
//! assertion.

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(feature = "static-test-hooks")]
mod read_pipeline {
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use pbfhogg::ElementReader;
    use pbfhogg::read::pipeline_test_hooks::{
        REORDER_FILLED_HIGH_WATER, arm_block_decode, blocked_decode_ready, release_blocked_decode,
        reset,
    };

    use crate::common::write_many_block_pbf;

    /// Owns the driver thread from the moment it is spawned. Releasing the
    /// parked decode and joining happen on every exit path, a failed
    /// assertion included, so a failure never leaves a worker parked in
    /// `maybe_block_decode`; the hooks are reset only after the join.
    struct Driver {
        handle: Option<JoinHandle<()>>,
    }

    impl Driver {
        fn release_and_join(&mut self) {
            release_blocked_decode();
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
            reset();
        }
    }

    #[test]
    fn admission_high_water_bounded_under_slow_first_decode() {
        reset();
        let dir = crate::common::test_scratch();
        let path = dir.path().join("many.osm.pbf");
        write_many_block_pbf(&path, 16);

        arm_block_decode(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let mut driver = Driver {
            handle: Some(std::thread::spawn(move || {
                let result = ElementReader::from_path(&path)
                    .unwrap()
                    .read_ahead(1)
                    .decode_ahead(2)
                    .for_each_block_pipelined(|_| Ok(()));
                done_tx.send(result).unwrap();
            })),
        };

        let start = Instant::now();
        while !blocked_decode_ready() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "decode hook was not reached"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        let start = Instant::now();
        while REORDER_FILLED_HIGH_WATER.load(Relaxed) < 1 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "later decode did not reach the reorder buffer"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        release_blocked_decode();
        let result = done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("pipeline did not finish after releasing decode hook");
        driver.release_and_join();
        result.unwrap();

        assert!(REORDER_FILLED_HIGH_WATER.load(Relaxed) <= 2);
    }
}
