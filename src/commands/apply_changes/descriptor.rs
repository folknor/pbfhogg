//! Types shared between the scanner, worker pool, and drain actor of the
//! descriptor-first streaming pipeline.
//!
//! The scanner emits one [`BlobDescriptor`] per OsmData blob via a
//! [`ScannedBlob`] message, tagging each as either a fast-path passthrough
//! (bypasses the worker pool entirely - drain receives it directly) or an
//! overlap candidate (routes to the worker pool for decompress + precise
//! check + maybe rewrite).
//!
//! Workers emit [`DrainItem`]s after processing candidates: false positives
//! become `CopyRange` items, and true overlaps become rewritten blocks.
//! Under `--direct-io` output, passthrough descriptors also route through
//! workers so they can pread the full frame bytes and emit `OwnedBytes`.
//!
//! The drain actor consumes a unified [`DrainItem`] stream (scanner's
//! passthrough stream + worker outputs merged into an unbounded reorder
//! buffer keyed by global seq) and produces the output PBF in file order.

use crate::blob_meta::{BlobIndex, ElemKind};

/// Per-blob metadata produced by the scanner from a single `HeaderWalker`
/// probe. Workers pread the compressed body from
/// `(frame_start + blob_offset, data_size)` only for `Candidate`
/// descriptors; `Passthrough` descriptors never have their body read in
/// userspace (the kernel copies them via `copy_file_range`).
///
/// Under `--direct-io` output, passthrough still needs body bytes because
/// `copy_file_range` is incompatible with the backend; workers pread the
/// **full framed bytes** from `(frame_start, frame_len)` and emit an
/// [`DrainItem::OwnedBytes`] carrying the complete frame, so the
/// drain can forward via `write_raw_owned` without re-framing.
pub(super) struct BlobDescriptor {
    /// Monotonic sequence number assigned by the scanner in file order.
    /// The drain's reorder buffer keys on this.
    pub seq: u64,
    /// Byte offset of the 4-byte length prefix at the start of the frame.
    pub frame_start: u64,
    /// Total framed bytes: `4 + header_len + data_size`.
    pub frame_len: usize,
    /// Byte offset within the frame where the Blob protobuf starts
    /// (equivalently, `4 + header_len`). Used by workers to compute the
    /// pread target for the compressed body.
    pub blob_offset: usize,
    /// Size of the Blob protobuf payload in bytes.
    pub data_size: usize,
    /// Element kind from indexdata (or inferred post-decompress under
    /// `--force`).
    pub kind: ElemKind,
    /// Inclusive `(min_id, max_id)` from indexdata. `None` under
    /// `--force` / unindexed inputs; workers fill it in after scan.
    pub id_range: Option<(i64, i64)>,
    /// Parsed indexdata, if the blob carried any. `None` under
    /// `--force` / unindexed inputs.
    pub index: Option<BlobIndex>,
    /// Raw tagdata bytes from BlobHeader field 4, if present.
    /// Forwarded byte-for-byte to the output writer on passthrough.
    pub tagdata: Option<Box<[u8]>>,
}

/// Scanner routing decision: based on `id_range` overlap against the OSC
/// `DiffRanges`, either send to the worker pool for precise check or to
/// the drain directly as a passthrough.
pub(super) enum ScannedBlob {
    /// Overlap candidate. Dispatched to the worker pool.
    Candidate(BlobDescriptor),
    /// No overlap, indexed. Fast-path - bypasses the worker pool.
    ///
    /// Under splice-capable output backends (buffered / io_uring), the
    /// drain forwards this directly as `DrainItem::CopyRange`.
    /// Under `--direct-io`, the scanner routes this through the worker
    /// pool instead (workers pread the full frame and emit
    /// [`DrainItem::OwnedBytes`]).
    Passthrough(BlobDescriptor),
}

/// Unified drain input. Arrives from two sources:
/// - Scanner's passthrough stream (splice-capable backends) emits
///   [`DrainItem::CopyRange`].
/// - Workers emit [`DrainItem::CopyRange`] for false positives,
///   [`DrainItem::OwnedBytes`] for `--direct-io` passthrough, and
///   [`DrainItem::Rewritten`] for actual rewrites.
///
/// The drain's reorder buffer keys on `seq` and processes items in file
/// order. Handles type transitions, gap creates, passthrough coalescing,
/// and writer submission from this unified stream.
pub(super) enum DrainItem {
    /// Raw frame bytes at `(frame_start, frame_len)` in the input file.
    /// Drain extends a contiguous-range coalescer; flushes as a single
    /// `copy_file_range` call when the next item breaks the run (type
    /// transition, gap create, rewrite, or buffer cap).
    CopyRange {
        seq: u64,
        frame_start: u64,
        frame_len: usize,
        kind: ElemKind,
        id_range: (i64, i64),
        /// Indexdata for the output blob. Preserved byte-for-byte from
        /// the input BlobHeader indexdata field.
        index: BlobIndex,
        tagdata: Option<Box<[u8]>>,
    },
    /// Owned frame bytes from a passthrough pread. Drain writes via
    /// `write_raw_owned`, no re-framing needed. Emitted by the scanner
    /// passthrough path under `--direct-io` and (since 2026-04-23) by
    /// the consumer-build false-positive path where `use_copy_range`
    /// is false. `count` is the element count the blob contributes to
    /// the output - the drain credits it to `base_<kind>` so merge
    /// stats match all-features parity regardless of which
    /// passthrough path the blob took.
    OwnedBytes {
        seq: u64,
        frame_bytes: Vec<u8>,
        kind: ElemKind,
        id_range: (i64, i64),
        count: u64,
    },
    /// Rewritten blob, with each output block already framed by the
    /// worker (compression + protobuf framing applied in-line via
    /// `frame_blob_pipelined`). Drain calls `write_raw_owned` per
    /// chunk - this bypasses the writer's per-block `rayon::spawn`
    /// dispatch and the associated `writer_pipeline_send_wait_ns`
    /// spike (P1.5 in the plan). Stats merge as before.
    Rewritten {
        seq: u64,
        framed_chunks: Vec<Vec<u8>>,
        kind: ElemKind,
        id_range: (i64, i64),
        stats: super::stats::MergeStats,
    },
}

impl BlobDescriptor {
    /// Convert this descriptor into a [`DrainItem::CopyRange`] for the
    /// scanner's fast-path. Returns `None` when the descriptor lacks
    /// indexdata (only indexed blobs can take the splice fast-path).
    pub(super) fn into_drain_copy_range(self) -> Option<DrainItem> {
        let index = self.index?;
        let id_range = self.id_range.unwrap_or((index.min_id, index.max_id));
        Some(DrainItem::CopyRange {
            seq: self.seq,
            frame_start: self.frame_start,
            frame_len: self.frame_len,
            kind: self.kind,
            id_range,
            index,
            tagdata: self.tagdata,
        })
    }

    /// Convert a false-positive descriptor into a [`DrainItem::CopyRange`]
    /// for the unified drain stream. The drain splices the raw frame
    /// via `copy_file_range`; the descriptor's index/tagdata carry the
    /// frame boundaries forward.
    ///
    /// `fallback_count` is used only when the descriptor has no
    /// indexdata (non-indexed blob under `--force`) - the worker
    /// supplies the element count walked from the already-parsed
    /// block so the drain can credit `base_<kind>` correctly (merge
    /// stats parity). For indexed blobs this value is ignored in
    /// favour of `index.count`.
    ///
    /// Moves the tagdata into the item, retaining the descriptor for error
    /// diagnostics without cloning its allocation.
    pub(super) fn take_drain_item(&mut self, fallback_count: u64) -> DrainItem {
        let id_range = self.id_range.unwrap_or((0, 0));
        let index = self.index.unwrap_or(BlobIndex {
            kind: self.kind,
            min_id: id_range.0,
            max_id: id_range.1,
            count: fallback_count,
            bbox: None,
        });
        DrainItem::CopyRange {
            seq: self.seq,
            frame_start: self.frame_start,
            frame_len: self.frame_len,
            kind: self.kind,
            id_range,
            index,
            tagdata: self.tagdata.take(),
        }
    }
}

impl DrainItem {
    /// The seq this item is keyed on in the reorder buffer.
    pub(super) fn seq(&self) -> u64 {
        match self {
            DrainItem::CopyRange { seq, .. }
            | DrainItem::OwnedBytes { seq, .. }
            | DrainItem::Rewritten { seq, .. } => *seq,
        }
    }

    /// The element kind of this blob - drain uses this for type-transition
    /// detection (Node → Way → Relation boundaries drive flushes of
    /// pending upserts).
    pub(super) fn kind(&self) -> ElemKind {
        match self {
            DrainItem::CopyRange { kind, .. }
            | DrainItem::OwnedBytes { kind, .. }
            | DrainItem::Rewritten { kind, .. } => *kind,
        }
    }

    /// The blob's OSM ID range - drain uses this for gap-create decisions
    /// (emit upserts with `id < min_id` before this blob).
    pub(super) fn id_range(&self) -> (i64, i64) {
        match self {
            DrainItem::CopyRange { id_range, .. }
            | DrainItem::OwnedBytes { id_range, .. }
            | DrainItem::Rewritten { id_range, .. } => *id_range,
        }
    }

    /// Approximate retained byte cost for reorder high-water accounting.
    /// This is telemetry, not an admission permit or a memory bound.
    /// CopyRange descriptors are small; Rewritten carries
    /// the full re-encoded payload; OwnedBytes carries the compressed
    /// frame.
    pub(super) fn byte_cost(&self) -> usize {
        const DESCRIPTOR_OVERHEAD: usize = 64;
        match self {
            DrainItem::CopyRange { tagdata, .. } => {
                DESCRIPTOR_OVERHEAD + tagdata.as_ref().map_or(0, |t| t.len())
            }
            DrainItem::OwnedBytes { frame_bytes, .. } => DESCRIPTOR_OVERHEAD + frame_bytes.len(),
            DrainItem::Rewritten { framed_chunks, .. } => {
                DESCRIPTOR_OVERHEAD + framed_chunks.iter().map(Vec::len).sum::<usize>()
            }
        }
    }
}
