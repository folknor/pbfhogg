//! Types shared between the scanner, worker pool, and drain actor of the
//! descriptor-first streaming pipeline.
//!
//! The scanner emits one [`BlobDescriptor`] per OsmData blob via a
//! [`ScannedBlob`] message, tagging each as either a fast-path passthrough
//! (bypasses the worker pool entirely - drain receives it directly) or an
//! overlap candidate (routes to the worker pool for decompress + precise
//! check + maybe rewrite).
//!
//! Workers emit [`DrainItem`]s after processing candidates: indexed false
//! positives become `CopyRange` items (`OwnedBytes` under `--direct-io`
//! output), unindexed ones reframed `OwnedBytes`, and true overlaps
//! rewritten blocks. Under `--direct-io` output,
//! passthrough descriptors also route through workers so they can pread the
//! full frame bytes and emit `OwnedBytes`.
//!
//! The drain actor consumes a unified [`DrainItem`] stream (scanner's
//! passthrough stream + worker outputs merged into an unbounded reorder
//! buffer keyed by global seq) and produces the output PBF in file order.

use crate::ElementKind;
use crate::blob_meta::BlobIndex;

/// How untouched (non-overlapping) blobs travel from the input to the output.
///
/// Decided once in `merge()` from the output backend's
/// `supports_copy_range()` and handed to the scanner, the worker pool and the
/// drain, so the three cannot disagree. `T` is whatever the stage needs on the
/// splice route: the scanner's `drain_tx` sender, the drain's input file, or
/// `()` for the workers, which only need to know which route they are on.
#[derive(Clone, Copy)]
pub(super) enum Route<T> {
    /// Kernel-space splice: the scanner sends `DrainItem::CopyRange` straight
    /// to the drain, and the drain coalesces adjacent ranges into
    /// `write_raw_copy` calls.
    Splice(T),
    /// Owned bytes: the backend that cannot splice (`--direct-io`).
    /// Passthroughs go through the worker pool,
    /// which preads the full frame and emits `DrainItem::OwnedBytes`.
    Owned,
}

impl<T> Route<T> {
    /// True on the splice route.
    pub(super) fn is_splice(&self) -> bool {
        matches!(self, Self::Splice(_))
    }
}

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
    /// The blob's file-wide ordinal as counted by the header walker (the
    /// OsmHeader blob is blob 0, so this is not `seq`). Error provenance.
    pub blob_index: u64,
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
    /// Element kind from indexdata. A placeholder `Node` on an unindexed
    /// blob (`--force`), which the worker replaces from the decompressed
    /// payload.
    pub kind: ElementKind,
    /// Inclusive `(min_id, max_id)` from indexdata. `None` on an unindexed
    /// blob until the worker derives it from the payload.
    pub id_range: Option<(i64, i64)>,
    /// The indexdata the input blob carries, if any. `None` on an unindexed
    /// blob (`--force`); only a descriptor with one may become a
    /// [`DrainItem::CopyRange`], since that copies the input header as-is.
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
/// - Workers emit [`DrainItem::CopyRange`] for indexed false positives on
///   the splice route, [`DrainItem::OwnedBytes`] for every indexed
///   passthrough on the owned route (`--direct-io`) and for reframed
///   unindexed blobs on either route, and [`DrainItem::Rewritten`] for
///   actual rewrites.
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
        kind: ElementKind,
        id_range: (i64, i64),
        /// Indexdata for the output blob. Preserved byte-for-byte from
        /// the input BlobHeader indexdata field.
        index: BlobIndex,
        tagdata: Option<Box<[u8]>>,
    },
    /// A complete frame held in memory, which the drain writes via
    /// `write_raw_owned` without re-framing. Two producers, both workers:
    ///
    /// - On the owned route (`--direct-io`), an indexed blob that leaves
    ///   unchanged - a scanner passthrough, a precise-check false positive,
    ///   or a way blob a refresh scan found clean - as the frame the worker
    ///   read (`verbatim`). This is the owned route's `CopyRange`.
    /// - On either route, an unindexed blob (`--force`) the diff leaves
    ///   alone, its original body under a rebuilt header carrying the
    ///   sidecars derived from its payload (not `verbatim`).
    ///
    /// `count` is the element count the blob contributes to the output; the
    /// drain credits it to `base_<kind>`, as it does a `CopyRange`'s, so the
    /// stats do not depend on the route.
    OwnedBytes {
        seq: u64,
        frame_bytes: Vec<u8>,
        kind: ElementKind,
        id_range: (i64, i64),
        count: u64,
        /// The bytes are the input blob's own indexed frame, unchanged: the
        /// drain counts the blob as an index hit, as it counts a
        /// `CopyRange`.
        verbatim: bool,
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
        kind: ElementKind,
        id_range: (i64, i64),
        stats: super::stats::MergeStats,
    },
    /// An unindexed input blob (`--force`) whose payload holds no element.
    /// No index can describe it, so it is left out of the output rather
    /// than copied unindexed, as plain `cat` does. It still holds its seq
    /// so the reorder buffer advances, but it has no kind: it must not
    /// move the drain's type transitions, gap creates or order watermark.
    Omitted { seq: u64 },
}

/// What the drain channel carries: a blob's [`DrainItem`], or the scanner's
/// one control message.
#[expect(
    clippy::large_enum_variant,
    reason = "every blob's item is the large variant; boxing it would allocate per blob \
              to shrink a message sent once per run"
)]
pub(super) enum DrainMessage {
    Item(DrainItem),
    /// Under `--locations-on-ways`, sent once by the scanner, at its first
    /// way or relation descriptor or at end of input: every node descriptor
    /// it dispatched before this has a seq below `node_end_seq` (zero when
    /// there were none). The drain acts on it on receipt, outside seq order:
    /// it is never buffered, counted or given a seq. Once the drain has
    /// processed every seq below it, the node phase is complete on sorted
    /// input and the coordinate map can be published. (On unsorted input a
    /// node descriptor after a way is dispatched past the boundary, misses
    /// publication, and is refused later by the drain's output-order guard.) It travels on the drain channel
    /// itself, so the drain learns it from the one receive it already
    /// blocks on, instead of polling a second channel.
    NodeBoundary {
        node_end_seq: u64,
    },
}

impl BlobDescriptor {
    /// Convert this descriptor into a [`DrainItem::CopyRange`] for the
    /// scanner's fast-path. Returns `None` when the descriptor lacks
    /// indexdata (only indexed blobs can take the splice fast-path).
    pub(super) fn into_drain_copy_range(mut self) -> Option<DrainItem> {
        self.take_copy_range()
    }

    /// Convert an indexed descriptor into a [`DrainItem::CopyRange`]: the
    /// drain splices the raw frame via `copy_file_range`, header included,
    /// and credits the index's `count` to the run's statistics.
    ///
    /// `None` when the descriptor carries no indexdata. Copying such a frame
    /// would put an unindexed blob in the output beside the indexed ones the
    /// rewrites produce, a partially indexed file every command but `cat`
    /// refuses; the worker reframes it with an index derived from the
    /// payload instead.
    ///
    /// Moves the tagdata into the item, retaining the descriptor for error
    /// diagnostics without cloning its allocation.
    pub(super) fn take_copy_range(&mut self) -> Option<DrainItem> {
        let index = self.index?;
        let id_range = self.id_range.unwrap_or((index.min_id, index.max_id));
        Some(DrainItem::CopyRange {
            seq: self.seq,
            frame_start: self.frame_start,
            frame_len: self.frame_len,
            kind: self.kind,
            id_range,
            index,
            tagdata: self.tagdata.take(),
        })
    }
}

impl DrainItem {
    /// The seq this item is keyed on in the reorder buffer.
    pub(super) fn seq(&self) -> u64 {
        match self {
            DrainItem::CopyRange { seq, .. }
            | DrainItem::OwnedBytes { seq, .. }
            | DrainItem::Rewritten { seq, .. }
            | DrainItem::Omitted { seq } => *seq,
        }
    }

    /// Where this blob sits in the output's type and ID order: its element
    /// kind (Node → Way → Relation boundaries drive flushes of pending
    /// upserts) and its OSM ID range (gap creates before it, the order
    /// watermark). `None` for [`DrainItem::Omitted`], which writes nothing.
    pub(super) fn placement(&self) -> Option<(ElementKind, (i64, i64))> {
        match self {
            DrainItem::CopyRange { kind, id_range, .. }
            | DrainItem::OwnedBytes { kind, id_range, .. }
            | DrainItem::Rewritten { kind, id_range, .. } => Some((*kind, *id_range)),
            DrainItem::Omitted { .. } => None,
        }
    }

    /// Approximate retained byte cost for reorder high-water accounting.
    /// This is telemetry, not an admission permit or a memory bound.
    /// CopyRange descriptors are small; Rewritten carries
    /// the full re-encoded payload; OwnedBytes carries the compressed
    /// frame.
    pub(super) fn byte_cost(&self) -> usize {
        use crate::tunables::DESCRIPTOR_OVERHEAD;
        match self {
            DrainItem::CopyRange { tagdata, .. } => {
                DESCRIPTOR_OVERHEAD + tagdata.as_ref().map_or(0, |t| t.len())
            }
            DrainItem::OwnedBytes { frame_bytes, .. } => DESCRIPTOR_OVERHEAD + frame_bytes.len(),
            DrainItem::Rewritten { framed_chunks, .. } => {
                DESCRIPTOR_OVERHEAD + framed_chunks.iter().map(Vec::len).sum::<usize>()
            }
            DrainItem::Omitted { .. } => DESCRIPTOR_OVERHEAD,
        }
    }
}
