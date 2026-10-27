//! Fault injection into `write::parallel_writer` (apply-changes' default
//! output backend), through the fault plan on the run's token
//! (`pbfhogg::test_hooks`).
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

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::Ordering::SeqCst;
use std::time::{Duration, Instant};

use flate2::write::GzEncoder;
use pbfhogg::apply_changes::{MergeOptions, merge};
use pbfhogg::write::WriteOptions;

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
/// `live_pool_workers` non-zero. A writer that joins everything
/// releases the sibling, waits for it, and returns with zero live.
///
/// The handshake counters are the run's token's, and apply-changes opens
/// exactly one output writer (one pool) per run, so no other pool can feed
/// them.
#[test]
fn fault_injection_parallel_writer_pool_panic_surfaces_error_and_joins_pool() {
    let dir = crate::common::test_scratch();
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

    let host = crate::common::test_host();
    let faults = host.invocation.faults();
    faults.hold_other_pool_workers.store(true, SeqCst);
    faults.pool_worker_panic.store(0, SeqCst);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        merge(
            &base,
            &osc,
            common::out(&output),
            &MergeOptions {
                write: WriteOptions {
                    force: true,
                    ..WriteOptions::default()
                },
                parallelism: pbfhogg::Parallelism::from_jobs(2),
                ..MergeOptions::default()
            },
            &pbfhogg::HeaderOverrides::default(),
            &host,
        )
    }));

    // Observe before releasing anything.
    let live_at_return = faults.live_pool_workers.load(SeqCst);
    let join_reached = faults.join_reached_held_worker.load(SeqCst);
    let timed_out = faults.pool_hook_timed_out.load(SeqCst);

    // Unconditional cleanup before any assertion, so a failing
    // assertion cannot leave parked workers behind: release them and
    // wait (bounded) for every pool thread to exit.
    faults.release_pool_workers.store(true, SeqCst);
    let deadline = Instant::now() + Duration::from_secs(10);
    while faults.live_pool_workers.load(SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let live_after_release = faults.live_pool_workers.load(SeqCst);
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

    // A writer that failed mid-stream publishes nothing: no output under
    // the destination name, no staging file, no scratch.
    let after = common::snapshot_dir(dir.path());
    common::assert_scratch_unchanged(&before, &after);
    assert!(
        !output.exists(),
        "a failed parallel write left an output file"
    );
}
