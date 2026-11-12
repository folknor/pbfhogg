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
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        pbfhogg::TEST_HANG_BOUND,
        pbfhogg::TEST_HANG_REPORT_GRACE,
        "io_uring dispatch panic",
    );
    let _lease = pbfhogg_test_scratch::locked_memory_lease();
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
        crate::common::test_executor(),
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

    let dispatches = writer
        .test_faults()
        .uring_dispatch_count
        .load(Ordering::Relaxed);
    require_dispatch_panic_error(write_result, dispatches).expect("injected writer failure");
}

/// Both send-side recovery and flush join the writer and preserve its payload.
/// A channel failure alone, or a caller unwind, does not prove that recovery.
fn require_dispatch_panic_error(
    result: std::thread::Result<pbfhogg::Result<()>>,
    dispatches: u64,
) -> Result<(), String> {
    if dispatches != 3 {
        return Err(format!(
            "expected dispatch 3 to panic, observed {dispatches} dispatches"
        ));
    }
    match result {
        Ok(Err(error)) => {
            let message = error.report();
            if message
                .contains("writer thread panicked: test-hooks: uring_writer dispatch #3 panicking")
            {
                Ok(())
            } else {
                Err(format!(
                    "failure did not preserve the injected panic: {message}"
                ))
            }
        }
        Ok(Ok(())) => Err("injected writer panic returned success".to_owned()),
        Err(_) => Err("writer panic unwound into the caller instead of returning Err".to_owned()),
    }
}

#[test]
fn dispatch_panic_assertion_rejects_unrelated_error() {
    let unrelated = std::io::Error::other("unrelated write failure");
    assert!(require_dispatch_panic_error(Ok(Err(unrelated.into())), 3).is_err());
}

#[test]
fn dispatch_panic_assertion_rejects_caller_unwind() {
    let payload: Box<dyn std::any::Any + Send> =
        Box::new("test-hooks: uring_writer dispatch #3 panicking");
    assert!(require_dispatch_panic_error(Err(payload), 3).is_err());
}

#[test]
fn dispatch_panic_assertion_requires_reached_dispatch() {
    let injected = || {
        std::io::Error::other(
            "writer thread panicked: test-hooks: uring_writer dispatch #3 panicking",
        )
        .into()
    };
    assert!(require_dispatch_panic_error(Ok(Err(injected())), 2).is_err());
    assert!(require_dispatch_panic_error(Ok(Err(injected())), 3).is_ok());
}
