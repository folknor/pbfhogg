use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) struct WriterMetrics {
    pub permit_wait_ns: AtomicU64,
    pub frame_ns: AtomicU64,
    pub compress_ns: AtomicU64,
    pub pipeline_send_wait_ns: AtomicU64,
    pub recv_wait_ns: AtomicU64,
    pub reorder_high_water: AtomicU64,
    pub write_ns: AtomicU64,
    pub flush_ns: AtomicU64,
    pub bytes_framed: AtomicU64,
    pub bytes_written: AtomicU64,

    pub payload_framed_items: AtomicU64,
    pub payload_framed_bytes: AtomicU64,
    pub payload_raw_items: AtomicU64,
    pub payload_raw_bytes: AtomicU64,
    pub payload_raw_chunk_items: AtomicU64,
    pub payload_raw_chunk_bytes: AtomicU64,
    pub payload_copy_range_items: AtomicU64,
    pub payload_copy_range_bytes: AtomicU64,

    pub buffered_write_calls: AtomicU64,
    pub buffered_write_bytes: AtomicU64,
    pub direct_write_calls: AtomicU64,
    pub direct_write_bytes: AtomicU64,
    pub sync_all_ns: AtomicU64,

    /// `writev` syscalls issued by the batched-buffered sink.
    pub batched_writev_calls: AtomicU64,
    /// Total frames (`OutputChunk` items) across all batched `writev`
    /// calls — divide by `batched_writev_calls` for average batch size.
    pub batched_writev_frames: AtomicU64,

    pub uring_submit_calls: AtomicU64,
    pub uring_submit_ns: AtomicU64,
    pub uring_submit_and_wait_calls: AtomicU64,
    pub uring_submit_and_wait_ns: AtomicU64,
    pub uring_cq_wait_ns: AtomicU64,
}

impl WriterMetrics {
    const fn new() -> Self {
        Self {
            permit_wait_ns: AtomicU64::new(0),
            frame_ns: AtomicU64::new(0),
            compress_ns: AtomicU64::new(0),
            pipeline_send_wait_ns: AtomicU64::new(0),
            recv_wait_ns: AtomicU64::new(0),
            reorder_high_water: AtomicU64::new(0),
            write_ns: AtomicU64::new(0),
            flush_ns: AtomicU64::new(0),
            bytes_framed: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            payload_framed_items: AtomicU64::new(0),
            payload_framed_bytes: AtomicU64::new(0),
            payload_raw_items: AtomicU64::new(0),
            payload_raw_bytes: AtomicU64::new(0),
            payload_raw_chunk_items: AtomicU64::new(0),
            payload_raw_chunk_bytes: AtomicU64::new(0),
            payload_copy_range_items: AtomicU64::new(0),
            payload_copy_range_bytes: AtomicU64::new(0),
            buffered_write_calls: AtomicU64::new(0),
            buffered_write_bytes: AtomicU64::new(0),
            direct_write_calls: AtomicU64::new(0),
            direct_write_bytes: AtomicU64::new(0),
            sync_all_ns: AtomicU64::new(0),
            batched_writev_calls: AtomicU64::new(0),
            batched_writev_frames: AtomicU64::new(0),
            uring_submit_calls: AtomicU64::new(0),
            uring_submit_ns: AtomicU64::new(0),
            uring_submit_and_wait_calls: AtomicU64::new(0),
            uring_submit_and_wait_ns: AtomicU64::new(0),
            uring_cq_wait_ns: AtomicU64::new(0),
        }
    }

    pub fn record_reorder_high_water(&self, len: usize) {
        let len = len as u64;
        let mut current = self.reorder_high_water.load(Relaxed);
        while len > current {
            match self
                .reorder_high_water
                .compare_exchange_weak(current, len, Relaxed, Relaxed)
            {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    pub fn emit(&self) {
        macro_rules! emit {
            ($name:literal, $field:ident) => {
                crate::debug::emit_counter(
                    $name,
                    i64::try_from(self.$field.load(Relaxed)).unwrap_or(i64::MAX),
                );
            };
        }

        emit!("writer_permit_wait_ns", permit_wait_ns);
        emit!("writer_frame_ns", frame_ns);
        emit!("writer_compress_ns", compress_ns);
        emit!("writer_pipeline_send_wait_ns", pipeline_send_wait_ns);
        emit!("writer_recv_wait_ns", recv_wait_ns);
        emit!("writer_reorder_high_water", reorder_high_water);
        emit!("writer_write_ns", write_ns);
        emit!("writer_flush_ns", flush_ns);
        emit!("writer_bytes_framed", bytes_framed);
        emit!("writer_bytes_written", bytes_written);

        emit!("writer_payload_framed_items", payload_framed_items);
        emit!("writer_payload_framed_bytes", payload_framed_bytes);
        emit!("writer_payload_raw_items", payload_raw_items);
        emit!("writer_payload_raw_bytes", payload_raw_bytes);
        emit!("writer_payload_raw_chunk_items", payload_raw_chunk_items);
        emit!("writer_payload_raw_chunk_bytes", payload_raw_chunk_bytes);
        emit!("writer_payload_copy_range_items", payload_copy_range_items);
        emit!("writer_payload_copy_range_bytes", payload_copy_range_bytes);

        emit!("writer_buffered_write_calls", buffered_write_calls);
        emit!("writer_buffered_write_bytes", buffered_write_bytes);
        emit!("writer_direct_write_calls", direct_write_calls);
        emit!("writer_direct_write_bytes", direct_write_bytes);
        emit!("writer_sync_all_ns", sync_all_ns);

        emit!("writer_batched_writev_calls", batched_writev_calls);
        emit!("writer_batched_writev_frames", batched_writev_frames);

        emit!("writer_uring_submit_calls", uring_submit_calls);
        emit!("writer_uring_submit_ns", uring_submit_ns);
        emit!("writer_uring_submit_and_wait_calls", uring_submit_and_wait_calls);
        emit!("writer_uring_submit_and_wait_ns", uring_submit_and_wait_ns);
        emit!("writer_uring_cq_wait_ns", uring_cq_wait_ns);
    }
}

pub(crate) static WRITER_METRICS: WriterMetrics = WriterMetrics::new();
