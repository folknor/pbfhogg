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
pub mod indexed;
pub(crate) mod pipeline;
pub(crate) mod pipeline_metrics;
pub(crate) mod raw_frame;
pub mod reader;
pub(crate) mod region_grid;
pub(crate) mod wire;

#[cfg(feature = "test-hooks")]
pub mod pipeline_test_hooks {
    pub use super::pipeline::test_hooks::{
        REORDER_FILLED_HIGH_WATER, REORDER_WINDOW_HIGH_WATER, arm_block_decode,
        blocked_decode_ready, release_blocked_decode, reset,
    };
}

#[cfg(feature = "test-hooks")]
pub mod region_grid_test_hooks {
    pub use super::region_grid::test_hooks::{FORCE_LINEAR, reset};
}
