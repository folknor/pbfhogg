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
pub(crate) mod header_walker;
pub mod indexed;
pub(crate) mod pipeline;
pub(crate) mod pipeline_metrics;
pub(crate) mod raw_frame;
pub mod reader;
pub(crate) mod wire;

#[cfg(feature = "test-hooks")]
pub mod pipeline_test_hooks {
    pub use super::pipeline::test_hooks::{
        BLOCK_DECODE_SEQ, BLOCKED_DECODE_READY, RELEASE_BLOCKED_DECODE, REORDER_FILLED_HIGH_WATER,
        REORDER_WINDOW_HIGH_WATER, reset,
    };
}
