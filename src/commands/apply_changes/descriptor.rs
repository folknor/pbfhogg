//! Types shared between the scanner, worker pool, and drain actor of the
//! descriptor-first streaming pipeline.
//!
//! These types are consumed by `scanner.rs`, `streaming.rs`, and `drain.rs`
//! (pending implementation). `#[allow(dead_code)]` at the module level
//! keeps the scaffold quiet until those modules land; remove it once all
//! variants and methods are referenced.

#![allow(dead_code)]
//!
//! The scanner emits one [`BlobDescriptor`] per OsmData blob via a
//! [`ScannedBlob`] message, tagging each as either a fast-path passthrough
//! (bypasses the worker pool entirely - drain receives it directly) or an
//! overlap candidate (routes to the worker pool for decompress + precise
//! check + maybe rewrite).
//!
//! Workers emit [`WorkerOutput`] after processing a candidate: false
//! positives (no actual element overlap) come out as passthrough; true
//! overlaps come out as rewritten blocks. Under `--direct-io` output where
//! kernel-side `copy_file_range` is unavailable, passthrough descriptors
//! also route through workers so they can pread the full frame bytes and
//! emit `OwnedPassthrough`.
//!
//! The drain actor consumes a unified [`DrainItem`] stream (scanner's
//! passthrough stream + worker outputs merged into a byte-budget reorder
//! buffer keyed by global seq) and produces the output PBF in file order.

use crate::block_builder::OwnedBlock;
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
/// [`WorkerOutput::OwnedPassthrough`] carrying the complete frame, so the
/// drain can forward via `write_raw_owned` without re-framing.
#[derive(Clone)]
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
    /// [`WorkerOutput::OwnedPassthrough`]).
    Passthrough(BlobDescriptor),
}

/// Worker output, sent to the drain.
pub(super) enum WorkerOutput {
    /// The blob had a loose range overlap but no element actually matched
    /// the diff. Emit as passthrough (CopyRange under splice-capable
    /// backends, OwnedPassthrough under `--direct-io`).
    ///
    /// The worker has already decompressed the body to do the precise
    /// check; on splice-capable backends the decompressed bytes are
    /// discarded and the drain re-reads the raw frame via
    /// `copy_file_range`.
    FalsePositive(BlobDescriptor),
    /// Rewritten blob. The worker applied the OSC overlay element-by-
    /// element and re-encoded using its thread-local `BlockBuilder`.
    Rewritten {
        seq: u64,
        blocks: Vec<OwnedBlock>,
        kind: ElemKind,
        id_range: (i64, i64),
    },
    /// `--direct-io` fallback: worker pread the full frame for a
    /// passthrough descriptor because `copy_file_range` isn't available.
    /// Drain writes via `write_raw_owned(frame_bytes)`.
    OwnedPassthrough {
        seq: u64,
        frame_bytes: Vec<u8>,
        kind: ElemKind,
        id_range: (i64, i64),
    },
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
    /// Owned frame bytes from a `--direct-io` passthrough pread. Drain
    /// writes via `write_raw_owned`, no re-framing needed.
    OwnedBytes {
        seq: u64,
        frame_bytes: Vec<u8>,
        kind: ElemKind,
        id_range: (i64, i64),
    },
    /// Rewritten blocks. Drain writes via
    /// `writer.write_primitive_block_owned` per block.
    Rewritten {
        seq: u64,
        blocks: Vec<OwnedBlock>,
        kind: ElemKind,
        id_range: (i64, i64),
    },
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

    /// Approximate byte cost for the reorder buffer's byte-budget
    /// backpressure. CopyRange descriptors are small; Rewritten carries
    /// the full re-encoded payload; OwnedBytes carries the compressed
    /// frame.
    pub(super) fn byte_cost(&self) -> usize {
        const DESCRIPTOR_OVERHEAD: usize = 64;
        match self {
            DrainItem::CopyRange { tagdata, .. } => {
                DESCRIPTOR_OVERHEAD + tagdata.as_ref().map_or(0, |t| t.len())
            }
            DrainItem::OwnedBytes { frame_bytes, .. } => {
                DESCRIPTOR_OVERHEAD + frame_bytes.len()
            }
            DrainItem::Rewritten { blocks, .. } => {
                // OwnedBlock is (Vec<u8>, BlobIndex, Option<Vec<u8>>):
                // (block_bytes, index, tagdata).
                DESCRIPTOR_OVERHEAD
                    + blocks
                        .iter()
                        .map(|(block_bytes, _, tagdata)| {
                            block_bytes.len() + tagdata.as_ref().map_or(0, Vec::len)
                        })
                        .sum::<usize>()
            }
        }
    }
}
