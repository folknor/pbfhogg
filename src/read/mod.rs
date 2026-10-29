pub mod blob;
pub(crate) mod blob_wire;
pub mod block;
pub(crate) mod claims;
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
pub mod reader;
pub(crate) mod region_grid;
pub(crate) mod wire;
pub(crate) mod wire_schema;

/// Emit `invocation`'s `pipeline_decompress_pool_*` sidecar counters, at
/// most once per invocation.
///
/// The read-side twin of [`crate::write::emit_writer_counters`]: call it once,
/// on the way out, after every thread of the invocation has joined - the CLI
/// does so on its single exit path, success or failure. The totals cover
/// every decompression pool of the invocation (classify workers,
/// command-specific readers, altw passthrough), not only the pipelined
/// reader, which is why no per-pipeline path emits them. Counter names and
/// FIFO framing are unchanged. Inert without a sink on the invocation, and
/// under a sealed one.
pub fn emit_read_counters(invocation: &crate::host::Invocation) {
    use std::sync::atomic::Ordering::Relaxed;
    let Ok(call) = invocation.call() else {
        return;
    };
    let metrics = call.metrics();
    if metrics.read_emitted.swap(true, Relaxed) {
        return;
    }
    pipeline_metrics::emit_decompress_pool_counters(&metrics.decompress, &call);
}
