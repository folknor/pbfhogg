//! Per-binary fault-injection test for `write::parallel_writer`
//! (apply-changes' default output backend).
//!
//! Split out of the former `apply_changes_invariants.rs` (2026-04-25).
//! This file is its own `[[test]]` target in `Cargo.toml`, so the
//! static hooks are per-process and race-free without `#[ignore]` or
//! `--test-threads=1`. The hook shape was chosen over a per-instance
//! `MergeOptions` field because pool workers are spawned deep inside
//! `parallel_writer_thread_inner` - threading a config through every
//! entry point would be invasive.
//!
//! Note: `parallel_writer` assigns absolute `pwrite` offsets to
//! every op up-front. If a pool worker panics holding queued ops
//! in its channel, those ops' offsets go unwritten - the kernel
//! leaves zero-filled holes at those byte ranges. The command
//! must still return an error and must not claim success; that's
//! one invariant this test locks. Whether the resulting file is
//! truncated or merely malformed-and-holey is a secondary concern
//! covered by CHANGELOG notes, not by the
//! absence-or-truncation assertion (which is too strong for this
//! backend).

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(feature = "static-test-hooks")]
mod parallel_writer {
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::sync::atomic::Ordering::SeqCst;
    use std::time::{Duration, Instant};

    use flate2::write::GzEncoder;
    use pbfhogg::apply_changes::{MergeOptions, merge};
    use pbfhogg::write::WriteOptions;
    use pbfhogg::write::parallel_writer_test_hooks as pw_hooks;
    use tempfile::TempDir;

    use crate::common::{self, generate_nodes, write_multi_block_test_pbf};

    fn write_osc(path: &Path, xml: &str) {
        let file = File::create(path).expect("create osc file");
        let mut enc = GzEncoder::new(file, flate2::Compression::fast());
        enc.write_all(xml.as_bytes()).expect("write xml");
        enc.finish().expect("finish gz");
    }

    /// A pool-worker panic inside `write::parallel_writer` must surface as
    /// a hard failure carrying the panic's message - and the writer must not
    /// return while any pool worker is still running.
    ///
    /// The second half is a fixed defect: the writer used to return on the first
    /// panicked join and detach the rest of the pool, which kept writing
    /// into the output after `merge()` had already reported failure. The
    /// hooks make that deterministic: worker 0 gets the first op and
    /// panics only once a sibling is parked mid-op, and the parked sibling
    /// is released only when the writer starts joining a non-panicked
    /// worker. A writer that walks away from its pool never reaches that
    /// join, so it returns with the sibling still parked and
    /// `LIVE_POOL_WORKERS` non-zero. A writer that joins everything
    /// releases the sibling, waits for it, and returns with zero live.
    #[test]
    fn fault_injection_parallel_writer_pool_panic_surfaces_error_and_joins_pool() {
        // Static hooks are process-global; reset before and after to
        // avoid leaking state to sibling tests in this binary (there
        // are none, but keep the discipline).
        pw_hooks::reset();

        let dir = TempDir::new().expect("tempdir");
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let output = dir.path().join("output.osm.pbf");

        // Multi-blob base so the parallel_writer pool dispatches several
        // ops: 40 nodes in blocks of 5 = 8+ data blobs, so worker 1 gets
        // the second op while worker 0 waits to panic.
        let nodes = generate_nodes(40, 1);
        write_multi_block_test_pbf(&base, &nodes, &[], &[], 5);
        write_osc(
            &osc,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="1" lat="0.0000001" lon="0.0000001" version="2"/>
    <node id="10" lat="0.0000010" lon="0.0000010" version="2"/>
    <node id="20" lat="0.0000020" lon="0.0000020" version="2"/>
    <node id="30" lat="0.0000030" lon="0.0000030" version="2"/>
  </modify>
</osmChange>"#,
        );

        let before = common::snapshot_dir(dir.path());

        pw_hooks::HOLD_OTHER_POOL_WORKERS.store(true, SeqCst);
        pw_hooks::PANIC_IN_POOL_WORKER.store(0, SeqCst);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            merge(
                &base,
                &osc,
                &output,
                &MergeOptions {
                    write: WriteOptions {
                        force: true,
                        ..WriteOptions::default()
                    },
                    locations_on_ways: false,
                    ignore_missing_nodes: false,
                    jobs: Some(2),
                    allow_version_regression: false,
                    #[cfg(feature = "test-hooks")]
                    panic_at_blob_seq: None,
                },
                &pbfhogg::HeaderOverrides::default(),
                &crate::common::test_host(),
            )
        }));

        // Observe before releasing anything.
        let live_at_return = pw_hooks::LIVE_POOL_WORKERS.load(SeqCst);
        let join_reached = pw_hooks::JOIN_REACHED_HELD_WORKER.load(SeqCst);
        let timed_out = pw_hooks::HOOK_TIMED_OUT.load(SeqCst);

        // Unconditional cleanup before any assertion, so a failing
        // assertion cannot leave parked workers behind: release them and
        // wait (bounded) for every pool thread to exit, then disarm.
        pw_hooks::RELEASE_POOL_WORKERS.store(true, SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while pw_hooks::LIVE_POOL_WORKERS.load(SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let live_after_release = pw_hooks::LIVE_POOL_WORKERS.load(SeqCst);
        pw_hooks::reset();
        assert_eq!(
            live_after_release, 0,
            "parallel_writer pool workers still running 10 s after release"
        );
        // The handshake is what makes the assertions below meaningful: a
        // sibling was parked when worker 0 panicked, and stayed parked until
        // the writer started joining it. A timed-out wait means one of those
        // did not happen and a detaching writer could have passed.
        assert!(
            !timed_out,
            "a test-hook handshake timed out; the detach check proved nothing"
        );

        let err = match result {
            Ok(Ok(_)) => {
                panic!("parallel_writer pool-worker panic must not produce a silent success")
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => panic!("merge() must report the pool-worker panic as an Err, not unwind"),
        };
        assert!(
            err.contains("test-hooks: parallel_writer pool worker 0 panicking"),
            "the surfaced error must carry the panic payload, got: {err}"
        );
        assert!(
            join_reached,
            "the writer returned without ever joining a parked pool worker - it detached its pool"
        );
        assert_eq!(
            live_at_return, 0,
            "merge() returned while {live_at_return} parallel_writer pool worker(s) were still \
             running - the writer detached its pool"
        );

        let mut after = common::snapshot_dir(dir.path());
        after.remove(std::path::Path::new("output.osm.pbf"));
        common::assert_scratch_unchanged(&before, &after);
    }
}
