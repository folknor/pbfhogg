//! Fault injection into `write::uring_writer`, through the writer's own
//! token's fault plan (`uring_panic_at_dispatch`, see `pbfhogg::test_hooks`).
//!
//! The test skips on a typed setup refusal, on hosts where io_uring init
//! fails due to environment constraints (low `RLIMIT_MEMLOCK`, old kernel,
//! missing WriteFixed support).

use std::sync::atomic::Ordering;

use pbfhogg::block_builder::{self, BlockBuilder};
use pbfhogg::writer::{Compression, PbfWriter};

/// A panic in the uring writer thread mid-stream must surface
/// via `PbfWriter::flush()` (which joins the writer thread and
/// routes the join result through `?`) as an `Err`, never a
/// silent short file.
#[test]
fn fault_injection_uring_writer_dispatch_panic_surfaces_via_flush() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("uring_fault.osm.pbf");

    let header = block_builder::HeaderBuilder::new().build().expect("header");

    // Construct the uring writer. Init may fail on hosts with
    // insufficient MEMLOCK; treat that as "environment not
    // suitable for this test" and return early rather than
    // claiming a failure the environment caused.
    let mut writer = match PbfWriter::to_path_uring(
        &path,
        Compression::default(),
        &header,
        pbfhogg::host::Executor::Ambient,
    ) {
        Ok(w) => w,
        Err(e) if crate::common::is_uring_unavailable(&e) => {
            crate::common::skip(format!("io_uring not available: {e}"));
            return;
        }
        Err(e) => panic!("unexpected io_uring init error: {e}"),
    };

    // Arm: panic on dispatch #3. Low enough to land mid-stream
    // with more items still in the pipeline. The writer thread holds a
    // clone of the same token, so arming after construction reaches it.
    writer
        .test_faults()
        .uring_panic_at_dispatch
        .store(3, Ordering::Relaxed);

    // Dispatch 8+ blocks so multiple items flow through the
    // reorder loop. Each block = one PipelineItem = one
    // dispatch. The header was already written before the hook
    // was armed (it's synchronous).
    let write_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for batch in 0..8_i64 {
            let mut bb = BlockBuilder::new();
            for id in 0..5 {
                let node_id = batch * 100 + id + 1;
                bb.add_node(
                    node_id,
                    123_456_789,
                    -987_654_321,
                    std::iter::empty::<(&str, &str)>(),
                    None,
                );
            }
            let bytes = bb.take().expect("block_builder take").expect("block");
            writer.write_primitive_block(bytes)?;
        }
        writer.flush()
    }));

    match write_result {
        Ok(Ok(())) => panic!("uring_writer dispatch panic must not produce a silent success"),
        Ok(Err(_)) => {
            // Expected: flush() returned Err carrying the
            // joined-thread panic.
        }
        Err(_) => {
            // Also acceptable: panic propagates directly via
            // write_primitive_block's send path (channel drop).
        }
    }
}
