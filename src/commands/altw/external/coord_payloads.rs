//! Blob-ordered delta-varint coord payload format.
//!
//! Stage 3 produces `coord_payloads` inline (integrated path, the default).
//! Each way blob's coordinates are delta-varint encoded and stored contiguously;
//! stage 4 preads one payload per way blob instead of mmapping the flat
//! coord_slots array. Measured compression is ~1.81× vs coord_slots (37.5 GB
//! Europe → 20.8 GB; 99 GB planet projects to ~55 GB). The original 3-4×
//! estimate did not account for OSM's moderate varint widths after delta
//! encoding.
//! The integration is pursued primarily for its non-wall benefits
//! (scratch footprint, page faults, memory pressure), not raw compression.
//!
//! # File format
//!
//! ```text
//! u64            num_way_blobs         (LE)
//! u64            total_payload_bytes   (LE)
//! u64 * (N+1)    blob_offsets[0..=N]   (LE, byte offsets into payload
//!                                       section; blob i's payload spans
//!                                       offsets[i]..offsets[i+1];
//!                                       offsets[N] == total_payload_bytes)
//! bytes          payload section (concatenated per-blob delta-varint streams)
//! ```
//!
//! # Per-blob payload
//!
//! Walk the blob's ways in PBF order (order recorded in the per-way refcount
//! sidecar). For each way with N refs, read N coord slots sequentially from
//! the bucket's dense scatter buffer in stage 3. Emit `2*N` zigzag-varints:
//! (lat_delta_0, lon_delta_0, ..., lat_delta_N-1, lon_delta_N-1) where
//! delta_0 is the absolute value (delta from 0) and deltas reset per way.
//!
//! # Decoder contract (stage 4)
//!
//! Stage 4 knows ref_count per way from the per-way refcount sidecar.
//! For each way, consume `2*ref_count` varints, unzigzag, accumulate running
//! lat/lon. No per-way framing bytes.

use std::path::Path;

use super::super::Result;
use super::COORD_SLOT_SIZE;

// ---------------------------------------------------------------------------
// Integrated path types
// ---------------------------------------------------------------------------

/// Location of one way blob's encoded coord payload. Written by stage 3
/// into a `ConcurrentBlobLocationRouter` slot; read by stage 4 via
/// `pread_blob_payload`. `Worker` variants point at a pwrite-durable
/// offset in the per-worker tmp file; `Straddler` variants hold the
/// fully-encoded bytes in RAM (only ~hundreds at planet scale);
/// `Empty` is pre-populated at router construction for zero-ref blobs
/// so stage 4 never waits on a slot that will never be published.
pub(super) enum BlobLocation {
    Worker {
        worker_id: u32,
        byte_offset: u64,
        byte_length: u64,
    },
    Straddler {
        bytes: Vec<u8>,
    },
    Empty,
}

// ====================================================================
// ConcurrentBlobLocationRouter - streaming variant for item #2
// ====================================================================
//
// Stage 3 publishes per-blob entries as it produces them; stage 4 workers
// call `wait_ready(blob_idx)` before preading the input PBF way blob, so a
// stalled decode thread never holds a decompressed block + StringTable
// resident while waiting. The sequential `build_blob_location_router`
// phase is gone.
//
// Three terminal states: populated (serve), aborted (return recorded
// error), producer_done with empty slot (return deterministic missing-
// publication error - matches the current build_blob_location_router
// "non-zero refs but no entry" check at coord_payloads.rs:398).

/// Side of a straddler this worker is contributing.
#[derive(Clone, Copy, Debug)]
pub(super) enum StraddlerSide {
    Left,
    Right,
}

/// In-flight straddler state (only populated for the few hundred way blobs
/// that actually straddle a slot-bucket boundary).
enum StraddlerPartial {
    Left(Vec<u8>),
    Right(Vec<u8>),
}

/// Blob-ordered concurrent router. Stage 3 producers publish; stage 4
/// consumers wait.
pub(super) struct ConcurrentBlobLocationRouter {
    worker_files: Vec<std::sync::Arc<std::fs::File>>,
    slots: Vec<std::sync::Mutex<Option<BlobLocation>>>,
    // Partial-straddler staging. Only way-blob indices that actually
    // straddle a slot-bucket boundary appear in this map (a few hundred
    // at planet scale). A single global Mutex is fine: publish count is
    // O(straddlers) not O(way_blobs), so lock contention is negligible
    // compared to the earlier per-blob-Mutex Vec sized for all way blobs
    // (~3 MB committed at planet for <1% live entries).
    straddler_partials: std::sync::Mutex<rustc_hash::FxHashMap<usize, StraddlerPartial>>,
    // Single global Condvar. `notify_all` is cheap at our scale (<=6
    // stage-4 waiters at any moment) and avoids per-slot Condvar
    // allocation at planet (~57K blobs would be ~3 MB of Condvars).
    notify: std::sync::Condvar,
    // Dummy mutex paired with `notify` for Condvar::wait. Waiters lock
    // the slot mutex to check the predicate, release it, then lock this
    // dummy to wait. This is OK because Condvar::wait only requires
    // *some* mutex, not the one guarding the predicate state.
    notify_mu: std::sync::Mutex<()>,
    aborted: std::sync::atomic::AtomicBool,
    producer_done: std::sync::atomic::AtomicBool,
    abort_error: std::sync::Mutex<Option<String>>,
    // Depth-gated WAIT span for stage-4 threads blocked in `wait_ready`
    // (stage 3 behind stage 4); drives `brokkr sidecar --stalls`
    // category S4_ROUTER. Only the slow path below touches it, so the
    // already-published fast path stays lock-free.
    wait_gauge: super::StallGauge,
    // Counters emitted by mod.rs after the scope joins.
    pub(super) stats: std::sync::Mutex<ConcurrentRouterStats>,
}

#[derive(Default)]
pub(super) struct ConcurrentRouterStats {
    pub num_worker: u64,
    pub num_straddlers: u64,
    pub num_empty: u64,
    pub worker_bytes: u64,
    pub straddler_bytes: u64,
    pub straddler_encode_ns: u64,
}

impl ConcurrentBlobLocationRouter {
    /// Build the router, pre-populating `Empty` entries for every zero-ref
    /// way blob. After construction, call `publish_worker` /
    /// `publish_straddler_half` as stage 3 produces entries, and
    /// `mark_producer_done` after stage 3 joins.
    pub(super) fn new(
        per_way_rcs: &PerWayRcs,
        worker_files: Vec<std::sync::Arc<std::fs::File>>,
    ) -> Result<Self> {
        let num_way_blobs = per_way_rcs.num_blobs();
        let mut slots: Vec<std::sync::Mutex<Option<BlobLocation>>> =
            Vec::with_capacity(num_way_blobs);
        let mut num_empty: u64 = 0;
        for blob_idx in 0..num_way_blobs {
            if per_way_rcs.blob_has_nonzero_refs(blob_idx)? {
                slots.push(std::sync::Mutex::new(None));
            } else {
                slots.push(std::sync::Mutex::new(Some(BlobLocation::Empty)));
                num_empty += 1;
            }
        }
        let straddler_partials: std::sync::Mutex<rustc_hash::FxHashMap<usize, StraddlerPartial>> =
            std::sync::Mutex::new(rustc_hash::FxHashMap::default());
        let stats = ConcurrentRouterStats {
            num_empty,
            ..Default::default()
        };
        Ok(Self {
            worker_files,
            slots,
            straddler_partials,
            notify: std::sync::Condvar::new(),
            notify_mu: std::sync::Mutex::new(()),
            aborted: std::sync::atomic::AtomicBool::new(false),
            producer_done: std::sync::atomic::AtomicBool::new(false),
            abort_error: std::sync::Mutex::new(None),
            wait_gauge: super::StallGauge::new("WAIT_S4_ROUTER_START", "WAIT_S4_ROUTER_END"),
            stats: std::sync::Mutex::new(stats),
        })
    }

    /// Total way-blob count (for caller diagnostics).
    pub(super) fn num_blobs(&self) -> usize {
        self.slots.len()
    }

    /// Shared write/read handle for a stage-3 worker's tmp file. Stage 3
    /// workers call `write_all_at` on this; stage 4 workers' preads
    /// come through `pread_blob_payload` on the same `&File`.
    pub(super) fn worker_file(&self, worker_id: usize) -> &std::sync::Arc<std::fs::File> {
        &self.worker_files[worker_id]
    }

    /// Publish a fully-contained blob's payload location. Called by
    /// stage 3 workers right after the payload bytes have been `pwrite`d.
    pub(super) fn publish_worker(
        &self,
        blob_idx: usize,
        worker_id: u32,
        byte_offset: u64,
        byte_length: u64,
    ) -> Result<()> {
        let mut guard = self.slots[blob_idx]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_some() {
            return Err(format!(
                "router publish_worker: blob {blob_idx} already has a location \
                 (likely duplicate emission across workers)"
            )
            .into());
        }
        *guard = Some(BlobLocation::Worker {
            worker_id,
            byte_offset,
            byte_length,
        });
        drop(guard);
        {
            let mut s = self
                .stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.num_worker += 1;
            s.worker_bytes += byte_length;
        }
        self.notify.notify_all();
        Ok(())
    }

    /// Publish one half of a straddler. When the second half arrives, the
    /// caller's `encode_scratch` is used to inline-encode the full payload
    /// via `encode_blob_payload_from_record`, and the slot transitions to
    /// `Straddler { bytes }`.
    pub(super) fn publish_straddler_half(
        &self,
        blob_idx: usize,
        side: StraddlerSide,
        raw_bytes: Vec<u8>,
        per_way_rcs: &PerWayRcs,
        inject_prepass: bool,
        encode_scratch: &mut Vec<u8>,
    ) -> Result<()> {
        let mut map = self
            .straddler_partials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (left_bytes, right_bytes) = match (map.remove(&blob_idx), side) {
            (None, StraddlerSide::Left) => {
                map.insert(blob_idx, StraddlerPartial::Left(raw_bytes));
                return Ok(());
            }
            (None, StraddlerSide::Right) => {
                map.insert(blob_idx, StraddlerPartial::Right(raw_bytes));
                return Ok(());
            }
            (Some(StraddlerPartial::Left(left)), StraddlerSide::Right) => (left, raw_bytes),
            (Some(StraddlerPartial::Right(right)), StraddlerSide::Left) => (raw_bytes, right),
            (Some(StraddlerPartial::Left(_)), StraddlerSide::Left) => {
                return Err(
                    format!("router straddler blob {blob_idx}: duplicate left half").into(),
                );
            }
            (Some(StraddlerPartial::Right(_)), StraddlerSide::Right) => {
                return Err(
                    format!("router straddler blob {blob_idx}: duplicate right half").into(),
                );
            }
        };
        // Both halves present - encode inline and publish.
        drop(map);
        let t_enc = std::time::Instant::now();
        let mut coord_bytes = left_bytes;
        coord_bytes.extend_from_slice(&right_bytes);
        encode_scratch.clear();
        encode_blob_payload_from_record(
            &coord_bytes,
            per_way_rcs.blob_record(blob_idx),
            blob_idx,
            inject_prepass,
            encode_scratch,
        )
        .map_err(|e| format!("router straddler encode blob {blob_idx}: {e}"))?;
        #[allow(clippy::cast_possible_truncation)]
        let encode_ns = t_enc.elapsed().as_nanos() as u64;
        let bytes = std::mem::take(encode_scratch);
        let byte_len = bytes.len() as u64;
        let mut slot_guard = self.slots[blob_idx]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot_guard.is_some() {
            return Err(format!(
                "router straddler blob {blob_idx}: slot already populated at encode time"
            )
            .into());
        }
        *slot_guard = Some(BlobLocation::Straddler { bytes });
        drop(slot_guard);
        {
            let mut s = self
                .stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.num_straddlers += 1;
            s.straddler_bytes += byte_len;
            s.straddler_encode_ns += encode_ns;
        }
        self.notify.notify_all();
        Ok(())
    }

    /// Signal that stage 3 has finished producing. Any `wait_ready` caller
    /// whose slot is still empty after this will return a deterministic
    /// missing-publication error.
    pub(super) fn mark_producer_done(&self) {
        self.producer_done
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_all();
    }

    /// Signal a terminal failure. Wakes all waiters; subsequent waits and
    /// preads observe the recorded error.
    pub(super) fn abort(&self, msg: String) {
        {
            let mut guard = self
                .abort_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.is_none() {
                *guard = Some(msg);
            }
        }
        self.aborted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_all();
    }

    pub(super) fn is_aborted(&self) -> bool {
        self.aborted.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Block until `blob_idx`'s slot is populated, the router is aborted,
    /// or the producer is done with the slot still empty. Returns `Ok(())`
    /// when the slot is known to be populated; returns `Err` on abort or
    /// missing-publication.
    pub(super) fn wait_ready(&self, blob_idx: usize) -> Result<()> {
        // Fast path: slot already populated.
        {
            let guard = self.slots[blob_idx]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.is_some() {
                return Ok(());
            }
        }
        // Slow path: wait on the global Condvar, re-checking predicates.
        // Genuinely blocked from here on (the fast path above returned
        // for published slots), so the WAIT_S4_ROUTER stall span never
        // fires on the hot path.
        let _stall = self.wait_gauge.track();
        loop {
            let mu_guard = self
                .notify_mu
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Re-check predicates under the notify mutex to avoid a missed
            // wakeup. Publishers take slot-mu, set slot, drop slot-mu,
            // then notify_all. Waiters check slot-mu first, release it,
            // then lock notify-mu and re-check - so if the publisher
            // notified between our slot check and our notify-mu acquire,
            // we see the populated slot on the re-check. If it notified
            // after we acquire notify-mu, wait picks up the notification.
            if self.aborted.load(std::sync::atomic::Ordering::SeqCst) {
                drop(mu_guard);
                let err = self
                    .abort_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                    .unwrap_or_else(|| "router aborted (no message recorded)".to_string());
                return Err(err.into());
            }
            {
                let guard = self.slots[blob_idx]
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if guard.is_some() {
                    return Ok(());
                }
                if self.producer_done.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(format!(
                        "router: no publication for blob {blob_idx} after producer finished"
                    )
                    .into());
                }
            }
            drop(
                self.notify
                    .wait(mu_guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
    }

    /// Read blob `blob_idx`'s encoded payload. Callers that have already
    /// awaited via `wait_ready` take the fast path; callers that come in
    /// cold also work (fast-path hits if the slot is ready, else blocks
    /// via `wait_ready`).
    pub(super) fn pread_blob_payload(&self, blob_idx: usize, buf: &mut Vec<u8>) -> Result<()> {
        self.wait_ready(blob_idx)?;
        use std::os::unix::fs::FileExt as _;
        let loc = {
            let guard = self.slots[blob_idx]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Cloning here avoids holding the slot lock while we pread;
            // the clone is cheap for Worker (pod) and moderate for
            // Straddler (one Vec<u8> clone of the encoded bytes).
            match &*guard {
                Some(loc) => loc.clone(),
                None => {
                    return Err(format!(
                        "router pread_blob_payload: slot {blob_idx} empty after wait_ready"
                    )
                    .into());
                }
            }
        };
        match loc {
            BlobLocation::Worker {
                worker_id,
                byte_offset,
                byte_length,
            } => {
                #[allow(clippy::cast_possible_truncation)]
                let len = byte_length as usize;
                buf.resize(len, 0);
                if len > 0 {
                    self.worker_files[worker_id as usize]
                        .read_exact_at(buf, byte_offset)
                        .map_err(|e| {
                            format!("router pread worker {worker_id} blob {blob_idx}: {e}")
                        })?;
                }
            }
            BlobLocation::Straddler { bytes } => {
                buf.clear();
                buf.extend_from_slice(&bytes);
            }
            BlobLocation::Empty => {
                buf.clear();
            }
        }
        Ok(())
    }
}

impl Clone for BlobLocation {
    fn clone(&self) -> Self {
        match self {
            Self::Worker {
                worker_id,
                byte_offset,
                byte_length,
            } => Self::Worker {
                worker_id: *worker_id,
                byte_offset: *byte_offset,
                byte_length: *byte_length,
            },
            Self::Straddler { bytes } => Self::Straddler {
                bytes: bytes.clone(),
            },
            Self::Empty => Self::Empty,
        }
    }
}

/// Panic-safe bilateral cancellation guard. Stage 3 and stage 4 workers
/// build one at closure entry and call `disarm` on normal exit. If the
/// worker panics instead, the `Drop` impl fires `router.abort`, which
/// wakes every `wait_ready` caller on the other side via `notify_all`.
pub(super) struct AbortOnDrop<'a> {
    router: &'a ConcurrentBlobLocationRouter,
    label: &'static str,
    armed: std::cell::Cell<bool>,
}

impl<'a> AbortOnDrop<'a> {
    pub(super) fn new(router: &'a ConcurrentBlobLocationRouter, label: &'static str) -> Self {
        Self {
            router,
            label,
            armed: std::cell::Cell::new(true),
        }
    }

    pub(super) fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for AbortOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed.get() {
            self.router
                .abort(format!("{} panicked (AbortOnDrop guard fired)", self.label));
        }
    }
}

/// Indexed per-way ref-count sidecar.
///
/// Retains the original varint-encoded sidecar bytes and records only the
/// per-blob byte offsets. Callers decode one blob's refcounts on demand,
/// avoiding the planet-scale flat `Vec<u32>` residency that stage 3/finalize
/// used to keep alive simultaneously.
pub(super) struct PerWayRcs {
    data: Vec<u8>,
    offsets: Vec<usize>, // len == num_blobs + 1; offsets[num_blobs] == data.len()
}

impl PerWayRcs {
    pub(super) fn blob_record(&self, blob_idx: usize) -> &[u8] {
        &self.data[self.offsets[blob_idx]..self.offsets[blob_idx + 1]]
    }

    pub(super) fn num_blobs(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    #[cfg(test)]
    pub(super) fn decode_blob_into<'a>(
        &self,
        blob_idx: usize,
        scratch: &'a mut Vec<u32>,
    ) -> Result<&'a [u32]> {
        decode_blob_record_into(self.blob_record(blob_idx), blob_idx, scratch)?;
        Ok(scratch.as_slice())
    }

    pub(super) fn blob_has_nonzero_refs(&self, blob_idx: usize) -> Result<bool> {
        blob_record_has_nonzero_refs(self.blob_record(blob_idx), blob_idx)
    }
}

fn scan_blob_record(cursor: &mut protohoggr::Cursor<'_>, blob_idx: usize) -> Result<()> {
    let num_ways = cursor
        .read_varint()
        .map_err(|e| format!("per-way sidecar blob {blob_idx} num_ways: {e}"))?;
    #[allow(clippy::cast_possible_truncation)]
    let num_ways_usize = num_ways as usize;
    for way_idx in 0..num_ways_usize {
        cursor
            .read_varint()
            .map_err(|e| format!("per-way sidecar blob {blob_idx} way {way_idx}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
fn decode_blob_record_into(record: &[u8], blob_idx: usize, scratch: &mut Vec<u32>) -> Result<()> {
    let mut cursor = protohoggr::Cursor::new(record);
    let num_ways = cursor
        .read_varint()
        .map_err(|e| format!("per-way sidecar blob {blob_idx} num_ways: {e}"))?;
    #[allow(clippy::cast_possible_truncation)]
    let num_ways_usize = num_ways as usize;
    scratch.clear();
    scratch.reserve(num_ways_usize);
    for way_idx in 0..num_ways_usize {
        let rc = cursor
            .read_varint()
            .map_err(|e| format!("per-way sidecar blob {blob_idx} way {way_idx}: {e}"))?;
        #[allow(clippy::cast_possible_truncation)]
        scratch.push(rc as u32);
    }
    if cursor.remaining() != 0 {
        return Err(format!(
            "per-way sidecar blob {blob_idx} has {} trailing bytes",
            cursor.remaining()
        )
        .into());
    }
    Ok(())
}

fn blob_record_has_nonzero_refs(record: &[u8], blob_idx: usize) -> Result<bool> {
    let mut cursor = protohoggr::Cursor::new(record);
    let num_ways = cursor
        .read_varint()
        .map_err(|e| format!("per-way sidecar blob {blob_idx} num_ways: {e}"))?;
    #[allow(clippy::cast_possible_truncation)]
    let num_ways_usize = num_ways as usize;
    for way_idx in 0..num_ways_usize {
        let rc = cursor
            .read_varint()
            .map_err(|e| format!("per-way sidecar blob {blob_idx} way {way_idx}: {e}"))?;
        if rc != 0 {
            return Ok(true);
        }
    }
    if cursor.remaining() != 0 {
        return Err(format!(
            "per-way sidecar blob {blob_idx} has {} trailing bytes",
            cursor.remaining()
        )
        .into());
    }
    Ok(false)
}

fn build_per_way_refcount_index(data: Vec<u8>, num_way_blobs: usize) -> Result<PerWayRcs> {
    let mut cursor = protohoggr::Cursor::new(&data);
    let mut offsets: Vec<usize> = Vec::with_capacity(num_way_blobs + 1);
    offsets.push(0);
    let data_len = data.len();

    for blob_idx in 0..num_way_blobs {
        scan_blob_record(&mut cursor, blob_idx)?;
        offsets.push(data_len - cursor.remaining());
    }

    if cursor.remaining() != 0 {
        return Err(format!(
            "per-way refcount sidecar has {} trailing bytes",
            cursor.remaining()
        )
        .into());
    }

    Ok(PerWayRcs { data, offsets })
}

/// Parse the per-way refcount sidecar bytes into an indexed `PerWayRcs`.
#[cfg(test)]
pub(super) fn parse_per_way_refcount_sidecar_bytes(
    data: &[u8],
    num_way_blobs: usize,
) -> Result<PerWayRcs> {
    build_per_way_refcount_index(data.to_vec(), num_way_blobs)
}

/// Load the per-way refcount sidecar into an indexed `PerWayRcs`.
pub(super) fn load_per_way_refcount_sidecar_indexed(
    path: &Path,
    num_way_blobs: usize,
) -> Result<PerWayRcs> {
    let data = std::fs::read(path).map_err(|e| format!("read per-way refcount sidecar: {e}"))?;
    build_per_way_refcount_index(data, num_way_blobs)
}

const _: () = {
    assert!(COORD_SLOT_SIZE == 8);
};

/// Delta-encode one blob's coord slice into `output` from a pre-decoded
/// `per_way_rcs` slice. Reference implementation that tests assert against
/// the production streaming `encode_blob_payload_from_record` path.
#[cfg(test)]
pub(super) fn encode_blob_payload(
    coord_bytes: &[u8],
    per_way_rcs: &[u32],
    output: &mut Vec<u8>,
) -> std::result::Result<(), String> {
    let expected_bytes: u64 = per_way_rcs.iter().map(|&r| u64::from(r)).sum::<u64>() * 8;
    if coord_bytes.len() as u64 != expected_bytes {
        return Err(format!(
            "coord_bytes length mismatch: got {} bytes, expected {} (8 * sum(per_way_rcs))",
            coord_bytes.len(),
            expected_bytes
        ));
    }
    let mut cursor: usize = 0;
    for &rc in per_way_rcs {
        let mut last_lat: i32 = 0;
        let mut last_lon: i32 = 0;
        for _ in 0..rc {
            let off = cursor;
            cursor += COORD_SLOT_SIZE;
            let lat = i32::from_le_bytes([
                coord_bytes[off],
                coord_bytes[off + 1],
                coord_bytes[off + 2],
                coord_bytes[off + 3],
            ]);
            let lon = i32::from_le_bytes([
                coord_bytes[off + 4],
                coord_bytes[off + 5],
                coord_bytes[off + 6],
                coord_bytes[off + 7],
            ]);
            let dlat = i64::from(lat) - i64::from(last_lat);
            let dlon = i64::from(lon) - i64::from(last_lon);
            protohoggr::encode_varint(output, protohoggr::zigzag_encode_64(dlat));
            protohoggr::encode_varint(output, protohoggr::zigzag_encode_64(dlon));
            last_lat = lat;
            last_lon = lon;
        }
    }
    Ok(())
}

/// Delta-encode one blob's coord slice using the raw sidecar record for that
/// blob instead of a pre-decoded `&[u32]`.
#[hotpath::measure]
pub(super) fn encode_blob_payload_from_record(
    coord_bytes: &[u8],
    record: &[u8],
    blob_idx: usize,
    inject_prepass: bool,
    output: &mut Vec<u8>,
) -> std::result::Result<(), String> {
    let mut cursor = protohoggr::Cursor::new(record);
    let num_ways = cursor
        .read_varint()
        .map_err(|e| format!("per-way sidecar blob {blob_idx} num_ways: {e}"))?;
    #[allow(clippy::cast_possible_truncation)]
    let num_ways_usize = num_ways as usize;

    let mut coord_cursor: usize = 0;
    for way_idx in 0..num_ways_usize {
        let rc = cursor
            .read_varint()
            .map_err(|e| format!("per-way sidecar blob {blob_idx} way {way_idx}: {e}"))?;
        #[allow(clippy::cast_possible_truncation)]
        let rc_usize = rc as usize;
        let way_bytes = rc_usize
            .checked_mul(COORD_SLOT_SIZE)
            .ok_or_else(|| format!("blob {blob_idx} way {way_idx}: refcount byte size overflow"))?;
        if coord_cursor + way_bytes > coord_bytes.len() {
            return Err(format!(
                "coord_bytes length mismatch for blob {blob_idx}: got {} bytes, \
                 need at least {} bytes by way {way_idx}",
                coord_bytes.len(),
                coord_cursor + way_bytes,
            ));
        }

        let mut last_lat: i32 = 0;
        let mut last_lon: i32 = 0;
        // When injecting, lat carries the pin bit in position 0; unpack it and
        // accumulate the per-way pin bitmap emitted after the coordinate
        // varints (v2 framing: 2*N varints then ceil(N/8) bitmap bytes).
        let mut pins = if inject_prepass {
            vec![0_u8; rc_usize.div_ceil(8)]
        } else {
            Vec::new()
        };
        for i in 0..rc_usize {
            let off = coord_cursor;
            coord_cursor += COORD_SLOT_SIZE;
            let packed_lat = i32::from_le_bytes([
                coord_bytes[off],
                coord_bytes[off + 1],
                coord_bytes[off + 2],
                coord_bytes[off + 3],
            ]);
            let lon = i32::from_le_bytes([
                coord_bytes[off + 4],
                coord_bytes[off + 5],
                coord_bytes[off + 6],
                coord_bytes[off + 7],
            ]);
            let lat = if inject_prepass {
                if packed_lat & 1 != 0 {
                    pins[i / 8] |= 1 << (i % 8);
                }
                packed_lat >> 1
            } else {
                packed_lat
            };
            let dlat = i64::from(lat) - i64::from(last_lat);
            let dlon = i64::from(lon) - i64::from(last_lon);
            protohoggr::encode_varint(output, protohoggr::zigzag_encode_64(dlat));
            protohoggr::encode_varint(output, protohoggr::zigzag_encode_64(dlon));
            last_lat = lat;
            last_lon = lon;
        }
        if inject_prepass {
            output.extend_from_slice(&pins);
        }
    }
    if cursor.remaining() != 0 {
        return Err(format!(
            "per-way sidecar blob {blob_idx} has {} trailing bytes",
            cursor.remaining()
        ));
    }
    if coord_cursor != coord_bytes.len() {
        return Err(format!(
            "coord_bytes length mismatch for blob {blob_idx}: got {} bytes, expected {}",
            coord_bytes.len(),
            coord_cursor,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // In tests we use .expect() liberally; clippy::unwrap_used covers .unwrap()
    // but .expect() is always fine.

    fn make_coord_bytes(coords: &[(i32, i32)]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(coords.len() * 8);
        for &(lat, lon) in coords {
            buf.extend_from_slice(&lat.to_le_bytes());
            buf.extend_from_slice(&lon.to_le_bytes());
        }
        buf
    }

    fn decode_zigzag_varints(data: &[u8], count: usize) -> Vec<i64> {
        let mut cursor = protohoggr::Cursor::new(data);
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let v = cursor.read_varint().expect("read varint");
            out.push(protohoggr::zigzag_decode_64(v));
        }
        out
    }

    fn reconstruct_coords(output: &[u8], per_way_rcs: &[u32]) -> Vec<(i32, i32)> {
        let total_refs: usize = per_way_rcs.iter().map(|&r| r as usize).sum();
        let deltas = decode_zigzag_varints(output, total_refs * 2);
        let mut result = Vec::with_capacity(total_refs);
        let mut delta_idx = 0;
        for &rc in per_way_rcs {
            let mut last_lat: i64 = 0;
            let mut last_lon: i64 = 0;
            for _ in 0..rc {
                let lat = last_lat + deltas[delta_idx];
                let lon = last_lon + deltas[delta_idx + 1];
                delta_idx += 2;
                #[allow(clippy::cast_possible_truncation)]
                result.push((lat as i32, lon as i32));
                last_lat = lat;
                last_lon = lon;
            }
        }
        result
    }

    #[test]
    fn encode_blob_payload_single_way_single_ref() {
        let coords = [(12345_i32, 67890_i32)];
        let cb = make_coord_bytes(&coords);
        let rcs = [1u32];
        let mut out = Vec::new();
        encode_blob_payload(&cb, &rcs, &mut out).expect("encode");
        let reconstructed = reconstruct_coords(&out, &rcs);
        assert_eq!(reconstructed, coords);
    }

    #[test]
    fn encode_blob_payload_single_way_multi_ref() {
        let coords = [(100_i32, 200_i32), (150_i32, 250_i32), (90_i32, 180_i32)];
        let cb = make_coord_bytes(&coords);
        let rcs = [3u32];
        let mut out = Vec::new();
        encode_blob_payload(&cb, &rcs, &mut out).expect("encode");
        let reconstructed = reconstruct_coords(&out, &rcs);
        assert_eq!(reconstructed, coords);
    }

    #[test]
    fn encode_blob_payload_multiple_ways() {
        // Three ways of 2/3/1 refs; deltas reset at way boundaries.
        let coords = [
            (10_i32, 20_i32),
            (30_i32, 40_i32), // way 0
            (500_i32, 600_i32),
            (510_i32, 610_i32),
            (490_i32, 590_i32),    // way 1
            (9999_i32, -9999_i32), // way 2
        ];
        let cb = make_coord_bytes(&coords);
        let rcs = [2u32, 3u32, 1u32];
        let mut out = Vec::new();
        encode_blob_payload(&cb, &rcs, &mut out).expect("encode");
        let reconstructed = reconstruct_coords(&out, &rcs);
        assert_eq!(reconstructed, coords);
    }

    #[test]
    fn encode_blob_payload_empty_blob() {
        let cb: Vec<u8> = Vec::new();
        let rcs: &[u32] = &[];
        let mut out = vec![0xAAu8, 0xBBu8];
        encode_blob_payload(&cb, rcs, &mut out).expect("encode");
        // Output unchanged - only the sentinel bytes we put in.
        assert_eq!(out, [0xAAu8, 0xBBu8]);
    }

    #[test]
    fn encode_blob_payload_zero_coords() {
        let coords = [(0_i32, 0_i32), (0_i32, 0_i32)];
        let cb = make_coord_bytes(&coords);
        let rcs = [2u32];
        let mut out = Vec::new();
        encode_blob_payload(&cb, &rcs, &mut out).expect("encode");
        let reconstructed = reconstruct_coords(&out, &rcs);
        assert_eq!(reconstructed, coords);
    }

    #[test]
    fn encode_blob_payload_negative_coords() {
        let coords = [(-1_000_000_i32, -1_000_000_i32), (0_i32, 0_i32)];
        let cb = make_coord_bytes(&coords);
        let rcs = [2u32];
        let mut out = Vec::new();
        encode_blob_payload(&cb, &rcs, &mut out).expect("encode");
        let reconstructed = reconstruct_coords(&out, &rcs);
        assert_eq!(reconstructed, coords);
    }

    #[test]
    fn encode_blob_payload_length_mismatch() {
        // per_way_rcs = [2] expects 16 bytes; we pass 15.
        let cb = vec![0u8; 15];
        let rcs = [2u32];
        let result = encode_blob_payload(&cb, &rcs, &mut Vec::new());
        assert!(result.is_err(), "expected Err for length mismatch");
    }

    /// Build a minimal per-way refcount sidecar in memory for `num_way_blobs`
    /// blobs where each blob has the per-way ref counts given by `blobs`.
    fn make_sidecar_bytes(blobs: &[&[u32]]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for blob_rcs in blobs {
            protohoggr::encode_varint(&mut out, blob_rcs.len() as u64);
            for &rc in *blob_rcs {
                protohoggr::encode_varint(&mut out, rc as u64);
            }
        }
        out
    }

    #[test]
    fn parse_per_way_refcount_sidecar_bytes_basic() {
        // blob 0: 2 ways with ref counts [3, 1]
        // blob 1: 1 way with ref count [2]
        let sidecar = make_sidecar_bytes(&[&[3, 1], &[2]]);
        let pwr = parse_per_way_refcount_sidecar_bytes(&sidecar, 2).expect("parse");
        let mut scratch: Vec<u32> = Vec::new();
        assert_eq!(pwr.num_blobs(), 2);
        assert_eq!(
            pwr.decode_blob_into(0, &mut scratch).expect("decode 0"),
            &[3u32, 1u32]
        );
        assert_eq!(
            pwr.decode_blob_into(1, &mut scratch).expect("decode 1"),
            &[2u32]
        );
    }

    #[test]
    fn parse_per_way_refcount_sidecar_bytes_empty_blob() {
        // blob 0: 0 ways; blob 1: 1 way with 1 ref
        let sidecar = make_sidecar_bytes(&[&[], &[1]]);
        let pwr = parse_per_way_refcount_sidecar_bytes(&sidecar, 2).expect("parse");
        let mut scratch: Vec<u32> = Vec::new();
        assert_eq!(
            pwr.decode_blob_into(0, &mut scratch).expect("decode 0"),
            &[] as &[u32]
        );
        assert_eq!(
            pwr.decode_blob_into(1, &mut scratch).expect("decode 1"),
            &[1u32]
        );
    }

    #[test]
    fn encode_blob_payload_from_record_matches_decoded_path() {
        let coords = [
            (10_i32, 20_i32),
            (30_i32, 40_i32),
            (500_i32, 600_i32),
            (510_i32, 610_i32),
            (490_i32, 590_i32),
            (9999_i32, -9999_i32),
        ];
        let cb = make_coord_bytes(&coords);
        let pwr = make_per_way_rcs(&[&[2u32, 3u32, 1u32]]);
        let mut from_record = Vec::new();
        let mut from_decoded = Vec::new();
        let mut scratch: Vec<u32> = Vec::new();

        encode_blob_payload_from_record(&cb, pwr.blob_record(0), 0, false, &mut from_record)
            .expect("encode from record");
        let decoded = pwr
            .decode_blob_into(0, &mut scratch)
            .expect("decode 0")
            .to_vec();
        encode_blob_payload(&cb, &decoded, &mut from_decoded).expect("encode decoded");

        assert_eq!(from_record, from_decoded);
    }

    // Helper: build a PerWayRcs directly from slice-of-slices.
    fn make_per_way_rcs(blobs: &[&[u32]]) -> PerWayRcs {
        let sidecar = make_sidecar_bytes(blobs);
        parse_per_way_refcount_sidecar_bytes(&sidecar, blobs.len()).expect("make_per_way_rcs")
    }
}
