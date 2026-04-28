//! Produce a valid-but-adversarial PBF by stripping properties or
//! perturbing structure. A "make our lives difficult" tool for exercising
//! code paths that require less-optimised inputs (unsorted, missing
//! indexdata, scattered coords).
//!
//! v1 transformations: `--unsort`, `--strip-locations`, `--strip-indexdata`.
//! Flags compose. See [`notes/degrade.md`](../../../notes/degrade.md) for
//! the design rationale.
//!
//! Implementation paths:
//!
//! - Pure passthrough (only `--strip-indexdata`): raw blob frames are
//!   reframed with a cleared `BlobHeader.indexdata` field. Blob bytes
//!   are bit-identical; only the header changes. Mirrors `cat`'s
//!   passthrough but drops the index instead of adding one.
//! - Decode path (`--unsort` and/or `--strip-locations`): elements are
//!   pulled through the pipelined reader, transformed, and re-emitted
//!   via `BlockBuilder`. `--strip-indexdata` composes by suppressing
//!   the indexdata field at frame time.
//!
//! `--unsort` swaps two adjacent same-kind elements at the first
//! BlockBuilder cap boundary (per kind), so adjacent output blobs of
//! that kind have overlapping ID ranges. This is the minimum
//! perturbation that makes `sort`'s `detect_overlaps` flag the file -
//! enough to trigger the overlap-rewrite path without chaos-ifying
//! every blob.

use std::path::Path;

use super::{
    build_output_header, flush_block, writer_from_header_bytes, HeaderOverrides, Result,
};
use crate::block_builder::{BlockBuilder, MemberData};
use crate::blob::BlobKind;
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::owned::{
    dense_node_metadata, element_metadata, owned_to_metadata, read_dense_node, read_node,
    read_relation, read_way, OwnedElement,
};
use crate::read::raw_frame::read_raw_frame;
use crate::writer::{encode_blob_header_into, frame_blob_pipelined, Compression, PbfWriter};
use crate::{Element, ElementReader};

/// Default per-block element cap. Matches the `BlockBuilder` default and
/// the PBF interop convention. Tests pass a smaller cap via the hidden
/// `--block-cap` CLI flag so fixtures don't need 8000+ elements per kind
/// to exercise the `--unsort` swap.
pub const DEFAULT_BLOCK_CAP: usize = 8000;

/// Set of degradations to apply. At least one flag must be set.
#[derive(Clone, Copy, Debug, Default)]
pub struct DegradeFlags {
    pub unsort: bool,
    pub strip_locations: bool,
    pub strip_indexdata: bool,
}

impl DegradeFlags {
    pub fn any(self) -> bool {
        self.unsort || self.strip_locations || self.strip_indexdata
    }

    /// Returns `true` if elements must be decoded and re-encoded. Only
    /// `--strip-indexdata` alone can run as a pure blob-level passthrough.
    fn needs_decode(self) -> bool {
        self.unsort || self.strip_locations
    }
}

/// Per-run statistics from a degrade operation.
pub struct DegradeStats {
    pub blobs_written: u64,
    pub elements_written: u64,
    pub flags: DegradeFlags,
}

impl DegradeStats {
    pub fn print_summary(&self) {
        let mut applied: Vec<&str> = Vec::new();
        if self.flags.unsort {
            applied.push("--unsort");
        }
        if self.flags.strip_locations {
            applied.push("--strip-locations");
        }
        if self.flags.strip_indexdata {
            applied.push("--strip-indexdata");
        }
        eprintln!(
            "Degraded {} elements across {} blobs (applied: {})",
            self.elements_written,
            self.blobs_written,
            applied.join(" "),
        );
    }
}

/// Apply the requested degradations to `input` and emit `output`.
///
/// `block_cap` is the per-block element cap used by the decode path's
/// `BlockBuilder`. Production callers pass `DEFAULT_BLOCK_CAP`; tests pass
/// a smaller value so the `--unsort` swap can be exercised on fixtures of
/// modest size.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub fn degrade(
    input: &Path,
    output: &Path,
    flags: DegradeFlags,
    block_cap: usize,
    compression: Compression,
    direct_io: bool,
    io_uring: bool,
    overrides: &HeaderOverrides,
) -> Result<DegradeStats> {
    if !flags.any() {
        return Err("degrade requires at least one transformation flag \
                    (--unsort, --strip-locations, --strip-indexdata)"
            .into());
    }
    if block_cap == 0 {
        return Err("--block-cap must be > 0".into());
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("degrade_unsort", i64::from(flags.unsort));
        crate::debug::emit_counter(
            "degrade_strip_locations",
            i64::from(flags.strip_locations),
        );
        crate::debug::emit_counter(
            "degrade_strip_indexdata",
            i64::from(flags.strip_indexdata),
        );
        crate::debug::emit_counter("degrade_block_cap", block_cap as i64);
    }

    let stats = if flags.needs_decode() {
        degrade_decode_path(
            input, output, flags, block_cap, compression, direct_io, io_uring, overrides,
        )?
    } else {
        degrade_passthrough_strip_indexdata(input, output, compression, direct_io, io_uring, overrides)?
    };

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("degrade_blobs_written", stats.blobs_written as i64);
        crate::debug::emit_counter(
            "degrade_elements_written",
            stats.elements_written as i64,
        );
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Passthrough: --strip-indexdata only
// ---------------------------------------------------------------------------

/// Raw blob frame iteration with cleared `BlobHeader.indexdata`.
///
/// Blob payload bytes are forwarded verbatim - inline `LocationsOnWays`
/// coordinates, sortedness, and every element-level property pass through
/// unchanged. The output's `LocationsOnWays` and `Sort.Type_then_ID`
/// header features are preserved when the input declared them, since the
/// blob bytes still encode that data.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn degrade_passthrough_strip_indexdata(
    input: &Path,
    output: &Path,
    compression: Compression,
    direct_io: bool,
    io_uring: bool,
    overrides: &HeaderOverrides,
) -> Result<DegradeStats> {
    let header_bytes = {
        let reader = ElementReader::open(input, direct_io)?;
        let header = reader.header().clone();
        build_output_header(&header, header.is_sorted(), overrides, |hb| {
            let mut hb = hb;
            if header.has_locations_on_ways() {
                hb = hb.optional_feature("LocationsOnWays");
            }
            hb
        })?
    };

    let mut writer =
        writer_from_header_bytes(output, compression, &header_bytes, direct_io, io_uring)?;

    let mut reader = FileReader::open(input, direct_io)?;
    let mut file_offset: u64 = 0;
    let mut blobs_written: u64 = 0;

    crate::debug::emit_marker("DEGRADE_PASSTHROUGH_START");
    while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset)? {
        match &frame.blob_type {
            BlobKind::OsmHeader => {}
            BlobKind::OsmData => {
                let blob_bytes = frame.blob_bytes();
                let tagdata = frame.tagdata.as_deref();
                let reframed = reframe_raw_without_index(blob_bytes, tagdata)?;
                writer.write_raw_owned(reframed)?;
                blobs_written += 1;
            }
            _ => {}
        }
    }
    crate::debug::emit_marker("DEGRADE_PASSTHROUGH_END");

    crate::debug::emit_marker("DEGRADE_FLUSH_START");
    writer.flush()?;
    crate::debug::emit_marker("DEGRADE_FLUSH_END");

    Ok(DegradeStats {
        blobs_written,
        elements_written: 0,
        flags: DegradeFlags {
            unsort: false,
            strip_locations: false,
            strip_indexdata: true,
        },
    })
}

/// Reframe a raw OSMData blob with a `BlobHeader` that omits the
/// `indexdata` field. `tagdata` is preserved (a separate flag will strip
/// it; v1 only targets indexdata).
fn reframe_raw_without_index(
    blob_bytes: &[u8],
    tagdata: Option<&[u8]>,
) -> std::io::Result<Vec<u8>> {
    let datasize = i32::try_from(blob_bytes.len()).map_err(|_| {
        std::io::Error::other(format!("blob datasize overflow: {} bytes", blob_bytes.len()))
    })?;
    let mut header_buf = Vec::new();
    encode_blob_header_into("OSMData", datasize, None, tagdata, &mut header_buf);
    let header_len = u32::try_from(header_buf.len()).map_err(|_| {
        std::io::Error::other(format!("header too large: {} bytes", header_buf.len()))
    })?;
    let total_len = 4 + header_buf.len() + blob_bytes.len();
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(&header_buf);
    out.extend_from_slice(blob_bytes);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Decode path: --unsort and/or --strip-locations (with optional --strip-indexdata)
// ---------------------------------------------------------------------------

/// Element kind tag for per-kind state in the unsort buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Node = 0,
    Way = 1,
    Relation = 2,
}

impl Kind {
    fn index(self) -> usize {
        self as usize
    }
}

/// Per-kind unsort state: hold the (cap-1)-th element of each kind so it
/// can be re-injected after the cap-th element. Triggers exactly one
/// adjacent-blob ID overlap per kind in the output.
struct UnsortState {
    /// `held[k]` is the (cap-1)-th element of kind `k`, captured pending
    /// re-injection at element index `cap+1`. `None` once injected (or
    /// if the kind never reached `cap-1` elements).
    held: [Option<OwnedElement>; 3],
    /// Number of elements seen for each kind so far in the input stream.
    seen: [u64; 3],
    /// Whether the swap has fired for each kind (for diagnostic).
    fired: [bool; 3],
    /// Element cap that drives the swap point. Matches BlockBuilder cap.
    cap: u64,
}

impl UnsortState {
    fn new(cap: usize) -> Self {
        Self {
            held: [None, None, None],
            seen: [0, 0, 0],
            fired: [false, false, false],
            cap: cap as u64,
        }
    }

    /// Returns true if this is the (cap-1)-th element of its kind, i.e.
    /// the one to hold for swap.
    fn should_hold(&self, kind: Kind) -> bool {
        let n = self.seen[kind.index()];
        self.cap >= 2 && n + 1 == self.cap
    }

    /// Returns true if this is the (cap+1)-th element of its kind, i.e.
    /// the one after which the held element is re-injected.
    fn should_inject_after(&self, kind: Kind) -> bool {
        let n = self.seen[kind.index()];
        self.cap >= 2 && n + 1 == self.cap + 1 && self.held[kind.index()].is_some()
    }
}

/// Streaming decode + re-encode. Handles `--unsort`, `--strip-locations`,
/// and the decode-side composition of `--strip-indexdata`.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn degrade_decode_path(
    input: &Path,
    output: &Path,
    flags: DegradeFlags,
    block_cap: usize,
    compression: Compression,
    direct_io: bool,
    io_uring: bool,
    overrides: &HeaderOverrides,
) -> Result<DegradeStats> {
    let reader = ElementReader::open(input, direct_io)?;
    let header = reader.header().clone();

    // Warn about LocationsOnWays loss on the decode path - BlockBuilder
    // does not preserve inline way-node coordinates. `--strip-locations`
    // makes the loss explicit, so suppress the warning in that case.
    if !flags.strip_locations {
        super::warn_locations_on_ways_loss(&header);
    }

    let preserve_sorted = !flags.unsort && header.is_sorted();
    let header_bytes = build_output_header(&header, preserve_sorted, overrides, |hb| hb)?;

    let mut writer =
        writer_from_header_bytes(output, compression, &header_bytes, direct_io, io_uring)?;
    let mut bb = BlockBuilder::with_element_cap(block_cap);
    let mut unsort = UnsortState::new(block_cap);
    let mut elements_written: u64 = 0;

    crate::debug::emit_marker("DEGRADE_DECODE_START");
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        for element in block.elements() {
            handle_element(&element, flags, &mut bb, &mut writer, &mut unsort, compression)?;
            elements_written += 1;
        }
    }

    // End-of-input: re-inject any elements still held (only happens for
    // kinds whose total count is between cap-1 and cap, where the swap
    // partially fired). The held element is the smallest-id member of
    // its kind, so emitting it at end preserves correctness even if the
    // swap target never arrived.
    for k in [Kind::Node, Kind::Way, Kind::Relation] {
        if let Some(elem) = unsort.held[k.index()].take() {
            write_owned_element(&elem, flags, &mut bb, &mut writer, compression)?;
        }
    }

    flush_terminal_blocks(&mut bb, &mut writer, flags.strip_indexdata, compression)?;
    crate::debug::emit_marker("DEGRADE_DECODE_END");

    crate::debug::emit_marker("DEGRADE_FLUSH_START");
    writer.flush()?;
    crate::debug::emit_marker("DEGRADE_FLUSH_END");

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(
            "degrade_unsort_fired_nodes",
            i64::from(unsort.fired[Kind::Node.index()]),
        );
        crate::debug::emit_counter(
            "degrade_unsort_fired_ways",
            i64::from(unsort.fired[Kind::Way.index()]),
        );
        crate::debug::emit_counter(
            "degrade_unsort_fired_relations",
            i64::from(unsort.fired[Kind::Relation.index()]),
        );
    }

    let blobs_written = blob_count(output, direct_io)?;

    Ok(DegradeStats {
        blobs_written,
        elements_written,
        flags,
    })
}

fn element_kind(element: &Element<'_>) -> Option<Kind> {
    match element {
        Element::Node(_) | Element::DenseNode(_) => Some(Kind::Node),
        Element::Way(_) => Some(Kind::Way),
        Element::Relation(_) => Some(Kind::Relation),
    }
}

fn handle_element(
    element: &Element<'_>,
    flags: DegradeFlags,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    unsort: &mut UnsortState,
    compression: Compression,
) -> Result<()> {
    let Some(kind) = element_kind(element) else {
        return Ok(());
    };

    if flags.unsort {
        if unsort.should_hold(kind) {
            unsort.held[kind.index()] = Some(read_owned(element));
            unsort.seen[kind.index()] += 1;
            return Ok(());
        }
        if unsort.should_inject_after(kind) {
            // Add the kind's currently-arriving element first (it goes
            // into the previous block, displacing the held cap-1 slot).
            // Then add the held one (which becomes the first element of
            // the next block, sandwiched between the higher-id element
            // we just wrote and the rest of the new block).
            add_element_to_builder(element, flags, bb, writer, compression)?;
            let held = unsort.held[kind.index()].take().expect("held checked");
            write_owned_element(&held, flags, bb, writer, compression)?;
            unsort.fired[kind.index()] = true;
            unsort.seen[kind.index()] += 1;
            return Ok(());
        }
    }

    add_element_to_builder(element, flags, bb, writer, compression)?;
    unsort.seen[kind.index()] += 1;
    Ok(())
}

/// Read a borrowed element into an `OwnedElement` so it can be deferred.
fn read_owned(element: &Element<'_>) -> OwnedElement {
    match element {
        Element::Node(n) => OwnedElement::Node(read_node(n)),
        Element::DenseNode(dn) => OwnedElement::Node(read_dense_node(dn)),
        Element::Way(w) => OwnedElement::Way(read_way(w)),
        Element::Relation(r) => OwnedElement::Relation(read_relation(r)),
    }
}

/// Add a borrowed element to the BlockBuilder. Flushes via the
/// `--strip-indexdata`-aware path so output blobs match the requested
/// degradation when the flag is set.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn add_element_to_builder(
    element: &Element<'_>,
    flags: DegradeFlags,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    compression: Compression,
) -> Result<()> {
    match element {
        Element::Node(n) => {
            ensure_capacity(Kind::Node, bb, writer, flags.strip_indexdata, compression)?;
            let meta = element_metadata(&n.info());
            bb.add_node(
                n.id(),
                n.decimicro_lat(),
                n.decimicro_lon(),
                n.tags(),
                meta.as_ref(),
            );
        }
        Element::DenseNode(dn) => {
            ensure_capacity(Kind::Node, bb, writer, flags.strip_indexdata, compression)?;
            let meta = dense_node_metadata(dn);
            bb.add_node(
                dn.id(),
                dn.decimicro_lat(),
                dn.decimicro_lon(),
                dn.tags(),
                meta.as_ref(),
            );
        }
        Element::Way(w) => {
            ensure_capacity(Kind::Way, bb, writer, flags.strip_indexdata, compression)?;
            let refs: Vec<i64> = w.refs().collect();
            let meta = element_metadata(&w.info());
            bb.add_way(w.id(), w.tags(), &refs, meta.as_ref());
        }
        Element::Relation(r) => {
            ensure_capacity(Kind::Relation, bb, writer, flags.strip_indexdata, compression)?;
            let members: Vec<MemberData<'_>> = r
                .members()
                .map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                })
                .collect();
            let meta = element_metadata(&r.info());
            bb.add_relation(r.id(), r.tags(), &members, meta.as_ref());
        }
    }
    Ok(())
}

/// Write an owned element to the BlockBuilder (used for re-injecting held
/// elements during `--unsort`). Mirrors `add_element_to_builder` but reads
/// from owned-element fields. Routes flushes through the
/// strip-indexdata-aware path so output blob framing matches the rest
/// of the run.
fn write_owned_element(
    element: &OwnedElement,
    flags: DegradeFlags,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    compression: Compression,
) -> Result<()> {
    match element {
        OwnedElement::Node(n) => {
            ensure_capacity(Kind::Node, bb, writer, flags.strip_indexdata, compression)?;
            let meta = owned_to_metadata(n.metadata.as_ref());
            bb.add_node(
                n.id,
                n.decimicro_lat,
                n.decimicro_lon,
                n.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                meta.as_ref(),
            );
        }
        OwnedElement::Way(w) => {
            ensure_capacity(Kind::Way, bb, writer, flags.strip_indexdata, compression)?;
            let meta = owned_to_metadata(w.metadata.as_ref());
            bb.add_way(
                w.id,
                w.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                &w.refs,
                meta.as_ref(),
            );
        }
        OwnedElement::Relation(r) => {
            ensure_capacity(Kind::Relation, bb, writer, flags.strip_indexdata, compression)?;
            let members: Vec<MemberData<'_>> = r
                .members
                .iter()
                .map(|m| MemberData {
                    id: m.id,
                    role: &m.role,
                })
                .collect();
            let meta = owned_to_metadata(r.metadata.as_ref());
            bb.add_relation(
                r.id,
                r.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                &members,
                meta.as_ref(),
            );
        }
    }
    Ok(())
}

/// Capacity check that flushes via the strip-indexdata-aware path.
fn ensure_capacity(
    kind: Kind,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    strip_indexdata: bool,
    compression: Compression,
) -> Result<()> {
    let can_add = match kind {
        Kind::Node => bb.can_add_node(),
        Kind::Way => bb.can_add_way(),
        Kind::Relation => bb.can_add_relation(),
    };
    if !can_add {
        flush_with_indexdata_choice(bb, writer, strip_indexdata, compression)?;
    }
    Ok(())
}

/// Take the current block from the BlockBuilder and write it. When
/// `strip_indexdata` is set, frame manually via `frame_blob_pipelined`
/// with `indexdata = None` and dispatch via `write_raw_owned`. Otherwise
/// use the standard `write_primitive_block_owned` path that embeds
/// indexdata.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn flush_with_indexdata_choice(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    strip_indexdata: bool,
    compression: Compression,
) -> Result<()> {
    if !strip_indexdata {
        return flush_block(bb, writer);
    }
    if let Some((bytes, _index, tagdata)) = bb.take_owned()? {
        let parts =
            frame_blob_pipelined(&bytes, &compression, None, tagdata.as_deref())?;
        writer.write_raw_owned(parts.into_vec())?;
    }
    Ok(())
}

fn flush_terminal_blocks(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    strip_indexdata: bool,
    compression: Compression,
) -> Result<()> {
    flush_with_indexdata_choice(bb, writer, strip_indexdata, compression)
}

/// After the writer has flushed, count the OsmData blobs in `output` so
/// the stats summary reflects reality regardless of which path was taken.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn blob_count(output: &Path, direct_io: bool) -> Result<u64> {
    let mut reader = FileReader::open(output, direct_io)?;
    let mut file_offset: u64 = 0;
    let mut count: u64 = 0;
    while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset)? {
        if frame.blob_type == BlobKind::OsmData {
            count += 1;
        }
    }
    Ok(count)
}
