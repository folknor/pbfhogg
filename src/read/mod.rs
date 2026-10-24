pub mod blob;
pub(crate) mod blob_wire;
pub mod block;
pub(crate) mod columnar;
pub(crate) mod decompress;
pub mod dense;
#[cfg(feature = "linux-direct-io")]
pub mod direct_reader;
pub mod elements;
pub mod file_reader;
pub(crate) mod frame;
pub(crate) mod header_walker;
pub mod input;
pub(crate) mod pipeline;
pub(crate) mod pipeline_metrics;
pub(crate) mod raw_frame;
pub mod reader;
pub(crate) mod region_grid;
pub(crate) mod wire;
pub(crate) mod wire_schema;

/// Emit the process-wide `pipeline_decompress_pool_*` sidecar counters, at
/// most once per process.
///
/// The read-side twin of [`crate::write::emit_writer_counters`]: call it once,
/// on the way out, after every command thread has joined - the CLI does so on
/// its single exit path, success or failure. The totals cover every
/// decompression pool in the process (classify workers, command-specific
/// readers, altw passthrough), not only the pipelined reader, which is why no
/// per-pipeline path emits them. Counter names and FIFO framing are unchanged.
/// Inert without an armed marker FIFO.
pub fn emit_read_counters() {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    static EMITTED: AtomicBool = AtomicBool::new(false);
    if EMITTED.swap(true, Relaxed) {
        return;
    }
    pipeline_metrics::emit_decompress_pool_counters();
}
