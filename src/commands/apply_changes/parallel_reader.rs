//! Parallel base-PBF reader: a header-only schedule scan drives a pool of
//! pread workers, and a reorder pump on the manager thread re-establishes
//! file order before delivering `RawBlobFrame`s to `merge()`'s main loop.
//!
//! Replaces the old sequential reader-thread. At planet, the sequential
//! reader capped the pipeline at ~1400 frames/s, which in turn capped classify
//! batches at ~12 blobs/batch and left rayon cores idle. See
//! `notes/apply-changes-opportunities.md` plan item #3.

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;

use crate::blob::{parse_blob_header_with_index, BlobKind};
use crate::blob_meta::BlobIndex;
use crate::file_reader::FileReader;
use crate::read::raw_frame::RawBlobFrame;
use crate::reorder_buffer::ReorderBuffer;

use super::stats::StallAccumulator;

const READER_CHANNEL_SIZE: usize = 128;

/// Schedule entry for a single OsmData blob: where it lives in the input file
/// plus any header-derived metadata needed downstream (indexdata, tagdata).
/// Workers pread the frame bytes using `frame_offset` + `blob_offset +
/// data_size`, so the OsmHeader blob and any non-OsmData blobs are filtered
/// out at schedule-build time rather than re-checked per worker.
#[derive(Clone)]
struct ReaderScheduleEntry {
    frame_offset: u64,
    blob_offset: usize,
    data_size: usize,
    index: Option<BlobIndex>,
    tagdata: Option<Box<[u8]>>,
}

impl ReaderScheduleEntry {
    fn frame_len(&self) -> usize {
        self.blob_offset + self.data_size
    }
}

/// Sequential header-only walk that dispatches each OsmData blob's schedule
/// entry into `dispatch_tx` as it's parsed. Runs concurrently with the pread
/// workers - they pull entries from the dispatch channel and start processing
/// as soon as the scanner produces them. Returns the number of OsmData
/// entries dispatched.
fn scan_and_dispatch(
    base_pbf: &Path,
    direct_io: bool,
    dispatch_tx: &mpsc::SyncSender<(usize, ReaderScheduleEntry)>,
) -> std::result::Result<usize, String> {
    use std::io::Read as _;

    let mut reader = FileReader::open(base_pbf, direct_io).map_err(|e| e.to_string())?;
    let mut file_offset: u64 = 0;
    let mut past_header = false;
    let mut seq: usize = 0;

    loop {
        let frame_start = file_offset;

        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        }
        let header_len = u32::from_be_bytes(len_buf) as usize;

        let mut header_bytes = vec![0u8; header_len];
        reader.read_exact(&mut header_bytes).map_err(|e| e.to_string())?;

        let (blob_type, data_size, raw_index, tagdata) =
            parse_blob_header_with_index(&header_bytes).map_err(|e| e.to_string())?;
        let index = raw_index.and_then(|ref data| BlobIndex::deserialize(data));

        let blob_offset = 4 + header_len;
        file_offset = frame_start + (blob_offset + data_size) as u64;

        reader.skip(data_size as u64).map_err(|e| e.to_string())?;

        if blob_type == BlobKind::OsmHeader {
            past_header = true;
            continue;
        }
        if !past_header || blob_type != BlobKind::OsmData {
            continue;
        }

        let entry = ReaderScheduleEntry {
            frame_offset: frame_start,
            blob_offset,
            data_size,
            index,
            tagdata,
        };
        if dispatch_tx.send((seq, entry)).is_err() {
            // Workers have shut down (e.g. downstream error). Stop scanning.
            break;
        }
        seq += 1;
    }

    Ok(seq)
}

/// Parallel reader: header-only schedule scan + N pread workers + a reorder
/// pump that re-establishes file order before delivering frames to the main
/// loop.
///
/// The workers pread their assigned schedule entries (dispatched via a
/// work-stealing `AtomicUsize`) and send `(seq, RawBlobFrame)` into an
/// internal channel; a reorder pump on the manager thread re-orders via
/// `ReorderBuffer` and forwards to the external `frame_rx` that the main loop
/// already reads from. Main-loop semantics are unchanged.
///
/// The `stalls` accumulator captures wait time on the external `ordered_tx`
/// (i.e. when the main loop's consumption can't keep up), matching the prior
/// `merge_reader_send_wait_us` attribution.
#[allow(clippy::too_many_lines)]
pub(super) fn spawn_parallel_reader(
    base_pbf: &Path,
    direct_io: bool,
    stalls: Arc<StallAccumulator>,
) -> (
    std::thread::JoinHandle<std::result::Result<(), String>>,
    mpsc::Receiver<RawBlobFrame>,
) {
    let base_path = base_pbf.to_path_buf();
    let (ordered_tx, ordered_rx) = mpsc::sync_channel::<RawBlobFrame>(READER_CHANNEL_SIZE);

    let handle = std::thread::spawn(move || -> std::result::Result<(), String> {
        use std::os::unix::fs::FileExt as _;
        use std::sync::atomic::Ordering;
        use std::sync::Mutex;

        let shared_file = Arc::new(
            std::fs::File::open(&base_path)
                .map_err(|e| format!("failed to open {}: {e}", base_path.display()))?,
        );

        crate::debug::emit_marker("MERGE_READER_START");

        let decode_threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(1))
            .unwrap_or(4);

        let (dispatch_tx, dispatch_rx) =
            mpsc::sync_channel::<(usize, ReaderScheduleEntry)>(decode_threads * 4);
        let dispatch_rx = Arc::new(Mutex::new(dispatch_rx));

        let (worker_tx, worker_rx) =
            mpsc::sync_channel::<(usize, RawBlobFrame)>(decode_threads * 4);

        let first_err: Mutex<Option<String>> = Mutex::new(None);
        let scanned: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        let mut frames_sent: u64 = 0;
        let mut blocked_sends: u64 = 0;

        std::thread::scope(|scope| {
            // Scanner thread: walks headers sequentially, streams entries
            // into the dispatch channel as they're parsed. Workers start
            // pread'ing the first entry as soon as it's dispatched; the
            // scanner doesn't need to complete before the main loop sees
            // frames.
            {
                let dispatch_tx = dispatch_tx.clone();
                let first_err = &first_err;
                let scanned = &scanned;
                let base_path = base_path.clone();
                scope.spawn(move || {
                    crate::debug::emit_marker("MERGE_READER_SCAN_START");
                    match scan_and_dispatch(&base_path, direct_io, &dispatch_tx) {
                        Ok(count) => {
                            scanned.store(count, Ordering::Relaxed);
                        }
                        Err(e) => {
                            let mut slot = first_err
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if slot.is_none() {
                                *slot = Some(e);
                            }
                        }
                    }
                    crate::debug::emit_marker("MERGE_READER_SCAN_END");
                });
            }
            drop(dispatch_tx);

            // Pread workers: pull (seq, entry) from dispatch, pread the
            // frame, send (seq, frame) to the reorder pump.
            for _ in 0..decode_threads {
                let rx = Arc::clone(&dispatch_rx);
                let tx = worker_tx.clone();
                let file = Arc::clone(&shared_file);
                let first_err = &first_err;
                scope.spawn(move || {
                    let mut read_buf: Vec<u8> = Vec::new();
                    loop {
                        if first_err
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .is_some()
                        {
                            return;
                        }
                        let (seq, entry) = {
                            let guard =
                                rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            match guard.recv() {
                                Ok(d) => d,
                                Err(_) => return,
                            }
                        };
                        let frame_len = entry.frame_len();
                        read_buf.resize(frame_len, 0);
                        if let Err(e) = file
                            .read_exact_at(&mut read_buf, entry.frame_offset)
                            .map_err(|e| format!("pread at {}: {e}", entry.frame_offset))
                        {
                            let mut slot = first_err
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if slot.is_none() {
                                *slot = Some(e);
                            }
                            return;
                        }
                        let frame = RawBlobFrame {
                            frame_bytes: std::mem::take(&mut read_buf),
                            blob_type: BlobKind::OsmData,
                            blob_offset: entry.blob_offset,
                            index: entry.index,
                            tagdata: entry.tagdata,
                            file_offset: entry.frame_offset,
                        };
                        if tx.send((seq, frame)).is_err() {
                            return;
                        }
                    }
                });
            }
            drop(dispatch_rx);
            drop(worker_tx);

            // Reorder pump runs on the manager thread. Pending depth is
            // bounded by `decode_threads` since that's the max number of
            // in-flight preads.
            let mut reorder: ReorderBuffer<RawBlobFrame> =
                ReorderBuffer::with_capacity(decode_threads * 2);
            while let Ok((seq, frame)) = worker_rx.recv() {
                reorder.push(seq, frame);
                while let Some(frame) = reorder.pop_ready() {
                    match ordered_tx.try_send(frame) {
                        Ok(()) => {
                            frames_sent += 1;
                        }
                        Err(mpsc::TrySendError::Full(frame)) => {
                            crate::debug::emit_marker("WAIT_READER_SEND_START");
                            let t0 = std::time::Instant::now();
                            let res = ordered_tx.send(frame);
                            let elapsed_us =
                                u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX);
                            stalls
                                .reader_send_us
                                .fetch_add(elapsed_us, Ordering::Relaxed);
                            crate::debug::emit_marker("WAIT_READER_SEND_END");
                            blocked_sends += 1;
                            if res.is_err() {
                                return;
                            }
                            frames_sent += 1;
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => return,
                    }
                }
            }
        });

        if let Some(e) = first_err
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(e);
        }

        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter("merge_reader_frames_sent", frames_sent as i64);
            crate::debug::emit_counter("merge_reader_blocked_sends", blocked_sends as i64);
            crate::debug::emit_counter(
                "merge_reader_decode_threads",
                decode_threads as i64,
            );
            crate::debug::emit_counter(
                "merge_reader_schedule_len",
                scanned.load(Ordering::Relaxed) as i64,
            );
        }
        crate::debug::emit_marker("MERGE_READER_END");
        Ok(())
    });

    (handle, ordered_rx)
}
