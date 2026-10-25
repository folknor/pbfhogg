//! Extract or remove elements by ID. Equivalent to `osmium getid` / `osmium removeid`.

use std::path::Path;

use crate::idset::IdSet;

use super::output_header::{Declared, OutputFacts, Ways};
use super::{
    HeaderOverrides, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local, require_indexdata, writer_from_header,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::error::Error;
use crate::file_writer::FileWriter;
use crate::owned::{dense_node_metadata, element_metadata, relation_members_into};
use crate::read::header_walker::ScanArm;
use crate::tunables::FULL_SCAN_ARM_MIN_BLOBS;
use crate::write::WriteOptions;
use crate::writer::PbfWriter;
use crate::{BlobFilter, Element, ElementKind, ElementReader, PrimitiveBlock};

use super::Result;

// ---------------------------------------------------------------------------
// ID parsing
// ---------------------------------------------------------------------------

/// Parsed element IDs grouped by type.
/// Uses `IdSet` for O(1) membership testing at all scales.
pub struct ElementIds {
    pub node_ids: IdSet,
    pub way_ids: IdSet,
    pub relation_ids: IdSet,
}

impl ElementIds {
    /// An empty set.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            node_ids: IdSet::new(),
            way_ids: IdSet::new(),
            relation_ids: IdSet::new(),
        }
    }

    /// True when no ID of any type is present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.node_ids.has_any() && !self.way_ids.has_any() && !self.relation_ids.has_any()
    }

    fn insert(&mut self, kind: ElementKind, id: i64) {
        match kind {
            ElementKind::Node => self.node_ids.set(id),
            ElementKind::Way => self.way_ids.set(id),
            ElementKind::Relation => self.relation_ids.set(id),
        }
    }
}

/// What one `process_block` call emitted.
///
/// `process_block` runs on decode-worker threads, so it cannot write
/// into a shared `ElementIds`; it hands back a small owned list instead
/// and the sequential consumer merges. The list stays empty unless the
/// caller asked for found-ID reporting, so the parallel path pays
/// nothing when `--verbose-ids` is off.
struct BlockOutcome {
    nodes: u64,
    ways: u64,
    relations: u64,
    /// Requested IDs that were actually written. Empty unless the caller
    /// set `GetidOptions::report_found`.
    found: Vec<(ElementKind, i64)>,
}

/// A typed element reference accepted by `getid`, `getparents` and
/// `inspect --show`.
///
/// The parser accepts both the compact form (`n123`) and the named form
/// (`node/123`). It keeps signed IDs intact; `getid` applies its separate
/// non-negative query rule, while `inspect --show` can inspect signed IDs in
/// an input PBF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElementRef {
    /// The referenced OSM element kind.
    pub kind: ElementKind,
    /// The signed OSM element ID.
    pub id: i64,
}

impl std::str::FromStr for ElementRef {
    type Err = crate::Error;

    fn from_str(spec: &str) -> std::result::Result<Self, Self::Err> {
        let (kind, id_str) = if let Some(id) = spec.strip_prefix("node/") {
            (ElementKind::Node, id)
        } else if let Some(id) = spec.strip_prefix("way/") {
            (ElementKind::Way, id)
        } else if let Some(id) = spec.strip_prefix("relation/") {
            (ElementKind::Relation, id)
        } else if let Some(id) = spec.strip_prefix('n') {
            (ElementKind::Node, id)
        } else if let Some(id) = spec.strip_prefix('w') {
            (ElementKind::Way, id)
        } else if let Some(id) = spec.strip_prefix('r') {
            (ElementKind::Relation, id)
        } else {
            return Err(Error::invalid_argument(format!(
                "invalid element spec '{spec}': expected n<id>, w<id>, r<id>, \
                 node/<id>, way/<id>, or relation/<id>"
            )));
        };

        let id = id_str.parse().map_err(|_| {
            Error::invalid_argument(format!(
                "invalid ID in '{spec}': '{id_str}' is not an integer"
            ))
        })?;
        Ok(Self { kind, id })
    }
}

/// Parse an ID spec like "n123", "w456", "r789".
// The messages show the bad input value, which is more helpful for CLI users
// than the underlying ParseIntError.
fn parse_id_spec(spec: &str, default_type: Option<ElementKind>) -> Result<(ElementKind, i64)> {
    let (kind, id) = parse_id_spec_inner(spec, default_type)?;
    if id < 0 {
        let kind = kind.as_str();
        return Err(Error::invalid_argument(format!(
            "getid requires non-negative input ids. \
             Input contains {kind} id {id}. \
             Negative ids are JOSM editor-local staging identifiers \
             that should be resolved before processing."
        )));
    }
    Ok((kind, id))
}

fn parse_id_spec_inner(
    spec: &str,
    default_type: Option<ElementKind>,
) -> Result<(ElementKind, i64)> {
    // The optional default is deliberate: unlike osmium's implicit node
    // default, pbfhogg requires an explicit --default-type for bare numbers.
    if spec.len() < 2 {
        if let Some(default) = default_type {
            let id: i64 = spec.parse().map_err(|_| {
                Error::invalid_argument(format!("invalid ID spec: {spec:?} (bad number)"))
            })?;
            return Ok((default, id));
        }
        return Err(Error::invalid_argument(format!(
            "invalid ID spec: {spec:?} (expected n/w/r prefix + number)"
        )));
    }
    let prefix = spec.as_bytes()[0];
    if matches!(prefix, b'n' | b'w' | b'r') {
        let element_ref: ElementRef = spec
            .parse()
            .map_err(|error: Error| error.context(format!("invalid ID spec: {spec:?}")))?;
        return Ok((element_ref.kind, element_ref.id));
    }
    if let Some(default) = default_type {
        let id: i64 = spec.parse().map_err(|_| {
            Error::invalid_argument(format!("invalid ID spec: {spec:?} (bad number)"))
        })?;
        return Ok((default, id));
    }
    Err(Error::invalid_argument(format!(
        "invalid ID spec: {spec:?} (expected prefix 'n', 'w', or 'r')"
    )))
}

/// Parse ID specs from command-line arguments.
// Retain this public wrapper for library compatibility.
pub fn parse_ids(specs: &[String]) -> Result<ElementIds> {
    parse_ids_with_default_type(specs, None)
}

/// Parse ID specs from command-line arguments with optional default element type for bare IDs.
pub fn parse_ids_with_default_type(
    specs: &[String],
    default_type: Option<ElementKind>,
) -> Result<ElementIds> {
    let mut set = ElementIds::empty();
    for spec in specs {
        let (kind, id) = parse_id_spec(spec, default_type)?;
        set.insert(kind, id);
    }
    Ok(set)
}

/// Parse ID specs from a file, one per line. See
/// [`parse_ids_from_file_with_default_type`] for the line rules.
// Retain this public wrapper for library compatibility.
pub fn parse_ids_from_file(path: &Path) -> Result<ElementIds> {
    parse_ids_from_file_with_default_type(path, None)
}

/// Parse ID specs from file with optional default element type for bare IDs.
///
/// Each line is read the way osmium's `read_id_file` (`id_file.cpp`, shared
/// by `getid` and `removeid`) reads it: spaces - only spaces - are stripped
/// from both ends, then everything from the first space or `#` onward is
/// dropped, and an empty remainder is skipped. So `n123 # note` and
/// `n123 anything` both mean `n123`, and a tab is part of the token, which
/// makes `\tn123` an error in both tools.
///
/// This is deliberately not the `--expressions` reader's rule: osmium cuts
/// expressions at `#` alone and strips nothing at line level (see
/// `tag_expr::read_expressions_file`), because a space is meaningful inside
/// an expression and never inside an id.
///
/// One deliberate difference: a single trailing `\r` is removed first, so a
/// CRLF file reads like an LF one. osmium keeps it, which rejects `n123\r`
/// and `n123#c\r` as illegal ids while accepting `n123 #c\r` (the space cut
/// removes the `\r`); a line ending should not decide that.
pub fn parse_ids_from_file_with_default_type(
    path: &Path,
    default_type: Option<ElementKind>,
) -> Result<ElementIds> {
    let contents = crate::read::input::read_to_string(path)?;
    let specs: Vec<String> = contents
        .split('\n')
        .filter_map(id_file_token)
        .map(ToString::to_string)
        .collect();
    // A bad id in a file is malformed input content, not a bad request: the
    // spec parser's `InvalidArgument` is reclassified here, kept as the cause.
    parse_ids_with_default_type(&specs, default_type).map_err(|error| {
        Error::invalid_data_with(format!("invalid id file {}", path.display()), error)
    })
}

/// The id token on one raw line of an id file, or `None` to skip the line.
/// See [`parse_ids_from_file_with_default_type`] for the rules.
fn id_file_token(raw: &str) -> Option<&str> {
    let line = raw.strip_suffix('\r').unwrap_or(raw).trim_matches(' ');
    let token = match line.find([' ', '#']) {
        Some(pos) => &line[..pos],
        None => line,
    };
    (!token.is_empty()).then_some(token)
}

/// Collect all element IDs from a PBF file.
///
/// Reads every node, way, and relation in the file and adds its ID to the
/// returned `ElementIds`. No member or reference IDs are collected - only
/// top-level element IDs (matching osmium's `--id-osm-file` behavior).
///
/// Every data blob is decoded and its IDs read from the elements, so
/// indexdata decides nothing here and a partially indexed file gives the
/// same set as a wholly indexed one: the walk accepts it.
pub fn parse_ids_from_pbf(
    path: &Path,
    _direct_io: bool,
    host: &crate::host::Host,
) -> Result<ElementIds> {
    let mut set = ElementIds::empty();

    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        path,
        None,
        crate::scan::classify::PartialIndex::Accept,
        host,
    )?;

    struct IdBatch {
        node_ids: Vec<i64>,
        way_ids: Vec<i64>,
        relation_ids: Vec<i64>,
    }

    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &schedule,
        None,
        || (),
        |block, _s| {
            let mut batch = IdBatch {
                node_ids: Vec::new(),
                way_ids: Vec::new(),
                relation_ids: Vec::new(),
            };
            for element in block.elements_skip_metadata() {
                match &element {
                    Element::DenseNode(dn) => batch.node_ids.push(dn.id()),
                    Element::Node(n) => batch.node_ids.push(n.id()),
                    Element::Way(w) => batch.way_ids.push(w.id()),
                    Element::Relation(r) => batch.relation_ids.push(r.id()),
                }
            }
            Ok(batch)
        },
        |_seq, batch| {
            for id in batch.node_ids {
                set.node_ids.set(id);
            }
            for id in batch.way_ids {
                set.way_ids.set(id);
            }
            for id in batch.relation_ids {
                set.relation_ids.set(id);
            }
        },
        host,
    )?;

    Ok(set)
}

/// Merge two `ElementIds`s together (union).
pub fn merge_id_sets(a: &mut ElementIds, b: &ElementIds) {
    a.node_ids.merge_from(&b.node_ids);
    a.way_ids.merge_from(&b.way_ids);
    a.relation_ids.merge_from(&b.relation_ids);
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Options for getid.
pub struct GetidOptions {
    /// Include referenced nodes of matching ways (two-pass).
    pub add_referenced: bool,
    /// Strip tags from referenced objects not explicitly requested.
    /// Requires `add_referenced`; refused without it.
    pub remove_tags: bool,
    /// Collect the requested IDs that were actually written, into
    /// [`GetidStats::found`].
    ///
    /// This is what `--verbose-ids` reports on. It is gathered on the
    /// write path, where each emitted element is already in hand;
    /// the alternative - decoding the finished output a second time -
    /// roughly doubled the command's runtime for a diagnostic.
    pub report_found: bool,
}

/// Statistics from a getid/removeid operation.
pub struct GetidStats {
    pub nodes_written: u64,
    pub ways_written: u64,
    pub relations_written: u64,
    /// Elements in blobs that were copied through raw, as *claimed* by their
    /// `BlobHeader.indexdata`.
    ///
    /// Kept apart from the `*_written` counters on purpose. Those are counted
    /// from elements this command actually decoded; a raw-passthrough blob is
    /// never decoded, so the only number available for it is the one the file
    /// claims about itself - and nothing verifies that claim (`check --index`
    /// exists to). Folding it in produced one "Wrote N elements" line mixing
    /// observation with hearsay, and a lying indexdata moved N with no way for
    /// the reader to tell. Reporting them separately keeps the headline true.
    pub passthrough_claimed: u64,
    /// The requested IDs that were written, when
    /// [`GetidOptions::report_found`] was set. `None` otherwise -
    /// distinguishing "not asked for" from "asked for, found nothing".
    pub found: Option<ElementIds>,
}

impl GetidStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let total = self.nodes_written + self.ways_written + self.relations_written;
        writeln!(
            w,
            "Wrote {total} elements: {} nodes, {} ways, {} relations",
            self.nodes_written, self.ways_written, self.relations_written,
        )?;
        if self.passthrough_claimed > 0 {
            writeln!(
                w,
                "Plus {} elements in raw-passthrough blobs, per their indexdata \
                 (unverified)",
                self.passthrough_claimed,
            )?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Extract elements matching the given IDs.
///
/// If `opts.add_referenced` is true, referenced nodes of matching ways are also
/// included (two-pass). Otherwise, only exact ID matches are output.
pub fn getid(
    input: &Path,
    output: crate::output::Output,
    ids: &ElementIds,
    opts: &GetidOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<GetidStats> {
    host.cancel.check()?;
    let result = getid_to(input, &output, ids, opts, write, overrides, host)?;
    output.publish(&host.cancel)?;
    Ok(result)
}

/// [`getid`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn getid_to(
    input: &Path,
    output: &crate::output::Output,
    ids: &ElementIds,
    opts: &GetidOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<GetidStats> {
    let (_, stats) = getid_dispatched(
        input,
        output,
        ids,
        opts,
        write,
        overrides,
        FULL_SCAN_ARM_MIN_BLOBS,
        host,
    )?;
    Ok(stats)
}

/// Dispatch the single-pass include mode on the blob-count estimate, then
/// run. Returns the arm (`None` on the two-pass `--add-referenced` path,
/// which is not dispatched) alongside the stats so tests can inject
/// `min_blobs` and assert which arm auto-dispatch actually executed.
#[allow(clippy::too_many_arguments)]
fn getid_dispatched(
    input: &Path,
    output: &crate::output::Output,
    ids: &ElementIds,
    opts: &GetidOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    min_blobs: u64,
    host: &crate::host::Host,
) -> Result<(Option<ScanArm>, GetidStats)> {
    // Refused before the input is touched: without `add_referenced` no
    // referenced object is emitted, so there is nothing to strip tags from.
    if opts.remove_tags && !opts.add_referenced {
        return Err(crate::Error::invalid_argument(
            "getid: remove_tags has no effect without add_referenced, which is what \
             emits the referenced objects it strips",
        ));
    }
    let has_indexdata = require_indexdata(
        input,
        write.direct_reads,
        write.force,
        "input PBF has no blob-level indexdata. Without indexdata, the type filter \
         based on requested ID types is a no-op - all blobs are decompressed \
         (significantly slower).",
        host,
    )?;

    // The indexdata result selects an arm only for the no-references path.
    // add_referenced still shares the refusal above: --force explicitly opts
    // into its slower unindexed scans even though it does not dispatch an arm.
    //
    // This first-blob probe is the only indexing decision getid makes. A
    // partially indexed file (indexed first data blob, a later one without
    // indexdata) is accepted on every path, deliberately and without a
    // refusing walk: no getid path skips, copies or counts a blob on the
    // strength of indexdata it lacks. Both include arms, the invert walker,
    // `--add-referenced`'s two passes and `-I`'s walk decompress such a blob
    // and filter its actual elements, so the output equals the one from the
    // same file wholly indexed; only the decode costs more. The one refusal
    // that can follow is the invert walker's, for an unindexed way blob under
    // a carried way-enrichment declaration, which re-encoding would break.
    // reference/indexdata-trust.md, "Missing and partial indexdata".
    let (arm, result) = if opts.add_referenced {
        let stats = getid_with_refs(input, output, ids, opts, write, overrides, host)?;
        (None, stats)
    } else {
        let arm = super::dispatch_scan_arm(input, has_indexdata, min_blobs, host)?;
        let stats = filter_by_id(
            input,
            output,
            ids,
            true,
            Ways::Reencoded,
            write,
            overrides,
            arm,
            opts.report_found,
            host,
        )?;
        (Some(arm), stats)
    };
    crate::debug::emit_counter("getid_nodes_written", result.nodes_written);
    crate::debug::emit_counter("getid_ways_written", result.ways_written);
    crate::debug::emit_counter("getid_relations_written", result.relations_written);
    Ok((arm, result))
}

/// Remove elements matching the given IDs (output everything else).
///
/// Requires blob-level indexdata so the invert-mode raw-passthrough fast
/// path at `filter_by_id` can skip re-encoding non-matching blobs. On a
/// non-indexed PBF the fast path is unreachable and every blob would
/// decode-and-re-encode silently; `--force` overrides.
pub fn removeid(
    input: &Path,
    output: crate::output::Output,
    ids: &ElementIds,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<GetidStats> {
    host.cancel.check()?;
    let result = removeid_to(input, &output, ids, write, overrides, host)?;
    output.publish(&host.cancel)?;
    Ok(result)
}

/// [`removeid`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn removeid_to(
    input: &Path,
    output: &crate::output::Output,
    ids: &ElementIds,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<GetidStats> {
    let indexed = require_indexdata(
        input,
        write.direct_reads,
        write.force,
        "input PBF has no blob-level indexdata. Without indexdata, the \
         invert-mode raw-passthrough fast path is unreachable and every \
         blob is decompressed and re-encoded (significantly slower).",
        host,
    )?;
    // Every way blob is copied whole when no way ID is removed and the blobs
    // carry indexdata (a blob without it is decoded to find its IDs, and the
    // walker refuses an unindexed way blob behind an indexed first blob when
    // the header has declared a way enrichment). The output is a subset of
    // the input with nothing changed, so the copied declarations still hold.
    let ways = if indexed && !ids.way_ids.has_any() {
        Ways::BlobsCopied
    } else {
        Ways::Reencoded
    };
    // Invert mode is pinned to the walker arm. The streaming full-scan
    // arm implements include mode only; fusing invert's raw passthrough
    // into a sequential stream is possible but unmeasured (ADR-0006).
    filter_by_id(
        input,
        output,
        ids,
        false,
        ways,
        write,
        overrides,
        ScanArm::Walker,
        false,
        host,
    )
}

// ---------------------------------------------------------------------------
// Single-pass filter (shared by getid without refs and removeid).
// Include mode: skip non-matching blobs by type and ID range.
// Invert mode: raw passthrough for non-matching blobs.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn filter_by_id(
    input: &Path,
    output: &crate::output::Output,
    ids: &ElementIds,
    include: bool,
    ways: Ways,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    arm: ScanArm,
    report_found: bool,
    host: &crate::host::Host,
) -> Result<GetidStats> {
    match arm {
        ScanArm::Walker => filter_by_id_walker(
            input,
            output,
            ids,
            include,
            ways,
            write,
            overrides,
            report_found,
            host,
        ),
        ScanArm::FullScan => {
            debug_assert!(
                include,
                "the streaming full-scan arm implements include mode only; \
                 removeid is pinned to ScanArm::Walker"
            );
            filter_by_id_streaming(input, output, ids, write, overrides, report_found, host)
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn filter_by_id_walker(
    input: &Path,
    output: &crate::output::Output,
    ids: &ElementIds,
    include: bool,
    ways: Ways,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    report_found: bool,
    host: &crate::host::Host,
) -> Result<GetidStats> {
    use crate::blob::{BlobKind, decode_blob_to_headerblock};
    use crate::read::header_walker::HeaderWalker;

    crate::debug::emit_marker("GETID_SCAN_START");

    // Single pread-based pass over blob headers. Data bodies are only
    // read (via pread) when the blob matters: OsmHeader (once, to emit
    // the output header), OsmData-with-match in include mode, every
    // OsmData in invert mode (the raw-passthrough path still needs the
    // frame bytes). Include mode with a small ID set thus reads only
    // the header index (~140 MB at planet) plus a handful of matching
    // blob bodies.
    let mut walker = HeaderWalker::open(input, host)?;
    let mut data_buf: Vec<u8> = Vec::new();
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
    let mut bb = BlockBuilder::new();
    let mut output_blocks: Vec<crate::block_builder::OwnedBlock> = Vec::new();
    let mut stats = GetidStats {
        nodes_written: 0,
        passthrough_claimed: 0,
        ways_written: 0,
        relations_written: 0,
        found: report_found.then(ElementIds::empty),
    };
    let blob_filter = BlobFilter::new(
        ids.node_ids.has_any(),
        ids.way_ids.has_any(),
        ids.relation_ids.has_any(),
    );
    let mut blobs_skipped: u64 = 0;
    let mut blobs_passthrough: u64 = 0;
    let mut osmdata_blobs: u64 = 0;

    // Find and decode the leading OsmHeader blob to build the output header.
    let mut writer: Option<PbfWriter<FileWriter>> = None;
    let facts = OutputFacts::carry(ways);
    let mut declared = Declared::default();

    while let Some(meta) = walker.next_header()? {
        // The walker owns provenance: failures on this blob are located with
        // the blob index and frame start it handed out.
        let locate = |e: Error| walker.locate_meta(&meta, e);
        match meta.blob_type {
            // The walker refuses a second OSMHeader, so this is the only one.
            BlobKind::OsmHeader => {
                walker
                    .pread_data(meta.data_offset, meta.data_size, &mut data_buf)
                    .map_err(locate)?;
                let header = decode_blob_to_headerblock(&data_buf).map_err(locate)?;
                let built = super::output_header::build_one(&header, &facts, overrides, host)?;
                declared = built.declared;
                let header_bytes = built.bytes;
                writer = Some(super::writer_from_header_bytes(
                    output,
                    write.compression,
                    &header_bytes,
                    write.backend,
                    host,
                )?);
            }
            BlobKind::OsmData => {
                osmdata_blobs += 1;
                let w = writer
                    .as_mut()
                    .ok_or_else(|| crate::error::new_missing_header(input))?;

                if let Some(ref idx) = meta.index {
                    let has_match = match idx.kind {
                        crate::ElementKind::Node => {
                            ids.node_ids.any_in_range(idx.min_id, idx.max_id)
                        }
                        crate::ElementKind::Way => ids.way_ids.any_in_range(idx.min_id, idx.max_id),
                        crate::ElementKind::Relation => {
                            ids.relation_ids.any_in_range(idx.min_id, idx.max_id)
                        }
                    };
                    if include {
                        if !blob_filter.wants_index(idx) || !has_match {
                            blobs_skipped += 1;
                            continue;
                        }
                    } else if !has_match {
                        // Invert mode, no ID match: raw passthrough. We
                        // need the full frame bytes (length prefix +
                        // header + data) to write verbatim.
                        walker
                            .pread_data(meta.frame_start, meta.frame_size, &mut frame_buf)
                            .map_err(locate)?;
                        // The blob is copied verbatim and never decoded, so
                        // `idx.count` is what the file says about itself, not
                        // something this command observed. It goes in its own
                        // counter rather than into `*_written`.
                        stats.passthrough_claimed += idx.count;
                        w.write_raw_owned(std::mem::take(&mut frame_buf))?;
                        blobs_passthrough += 1;
                        continue;
                    }
                }
                // Blob might contain matching IDs, or the blob carries no
                // indexdata and we must decode to check. Pread the data
                // and run the per-element filter.
                walker
                    .pread_data(meta.data_offset, meta.data_size, &mut data_buf)
                    .map_err(locate)?;
                decompress_buf.clear();
                crate::blob::decompress_blob_data_into(&data_buf, &mut decompress_buf)
                    .map_err(locate)?;
                // Move the decompressed Vec into the block: no Bytes round-trip,
                // no second whole-buffer copy (the `new_with_scratch(Bytes)` route
                // paid a `to_vec`). `decompress_buf` is reallocated next blob.
                let block = PrimitiveBlock::from_vec_with_scratch(
                    std::mem::take(&mut decompress_buf),
                    &mut st_scratch,
                    &mut gr_scratch,
                )
                .map_err(locate)?;
                output_blocks.clear();
                let outcome = process_block(
                    &block,
                    &mut bb,
                    &mut output_blocks,
                    ids,
                    include,
                    None,
                    false,
                    report_found,
                )
                .map_err(locate)?;
                flush_local(&mut bb, &mut output_blocks)?;
                // Only an unindexed blob can be a way blob decoded under a
                // header that declared way enrichment (with one, no way ID is
                // removed, so an indexed way blob never matches).
                if meta.index.is_none()
                    && declared.way_enrichment()
                    && output_blocks
                        .iter()
                        .any(|b| b.index.kind == crate::ElementKind::Way)
                {
                    return Err(locate(
                        declared.broken_by_unindexed_way_blob("re-encoding it"),
                    ));
                }
                for OwnedBlock {
                    bytes: block_bytes,
                    index,
                    tagdata,
                    way_members,
                } in output_blocks.drain(..)
                {
                    w.write_primitive_block_owned(
                        block_bytes,
                        index,
                        tagdata.as_deref(),
                        way_members.as_deref(),
                    )?;
                }
                stats.nodes_written += outcome.nodes;
                stats.ways_written += outcome.ways;
                stats.relations_written += outcome.relations;
                if let Some(found) = stats.found.as_mut() {
                    for (kind, id) in outcome.found {
                        found.insert(kind, id);
                    }
                }
            }
            _ => {}
        }
    }

    let mut writer = writer.ok_or_else(|| crate::error::new_missing_header(input))?;
    crate::debug::emit_counter("walk_actual_osmdata_blobs", osmdata_blobs);
    if blobs_skipped > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "getid.blobs_skipped",
            "{blobs_skipped} blobs skipped by ID range filter"
        );
    }
    if blobs_passthrough > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "getid.invert.passthrough",
            "{blobs_passthrough} blobs passed through raw"
        );
    }
    writer.flush()?;
    crate::debug::emit_marker("GETID_SCAN_END");
    Ok(stats)
}

/// Sequential full-read include scan: stream every frame with
/// `read_raw_frame` (one buffered, readahead-friendly pass), skip
/// non-matching OSMData blobs via the indexdata kind + ID-range prescreen
/// before decompression, and decode + filter only intersecting blobs.
///
/// This is the measured high-blob-count arm for getid include mode
/// (ADR-0006). It deliberately does NOT use the pipelined reader: with a
/// sparse query nearly every blob is skipped, so the run is bounded by
/// moving bytes past the prescreen, and per-frame pipeline overhead
/// (permit accounting, rayon dispatch, channel hops) times millions of
/// small blobs loses to a plain sequential read loop.
#[allow(clippy::too_many_lines)]
fn filter_by_id_streaming(
    input: &Path,
    output: &crate::output::Output,
    ids: &ElementIds,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    report_found: bool,
    host: &crate::host::Host,
) -> Result<GetidStats> {
    use crate::blob::{BlobKind, decode_blob_to_headerblock};
    use crate::file_reader::FileReader;
    use crate::read::raw_frame::{frame_location, read_raw_frame};

    crate::debug::emit_marker("GETID_SCAN_START");

    let blob_filter = BlobFilter::new(
        ids.node_ids.has_any(),
        ids.way_ids.has_any(),
        ids.relation_ids.has_any(),
    );
    let mut writer: Option<PbfWriter<FileWriter>> = None;
    let mut stats = GetidStats {
        nodes_written: 0,
        passthrough_claimed: 0,
        ways_written: 0,
        relations_written: 0,
        found: report_found.then(ElementIds::empty),
    };
    let mut blobs_skipped: u64 = 0;
    let mut osmdata_blobs: u64 = 0;

    let mut reader = FileReader::open(input, write.direct_reads)?;
    let mut file_offset: u64 = 0;
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut bb = BlockBuilder::new();
    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset, host, input)? {
        // The frame loop owns the provenance of failures on this frame.
        let locate = |e: Error| e.locate(frame_location(input, frame.file_offset));
        match frame.blob_type {
            // `read_raw_frame` refuses a second OSMHeader, so this is the
            // only one.
            BlobKind::OsmHeader => {
                let header = decode_blob_to_headerblock(frame.blob_bytes()).map_err(locate)?;
                let header_bytes = super::output_header::build_one(
                    &header,
                    &super::output_header::OutputFacts::carry(
                        super::output_header::Ways::Reencoded,
                    ),
                    overrides,
                    host,
                )?
                .bytes;
                writer = Some(super::writer_from_header_bytes(
                    output,
                    write.compression,
                    &header_bytes,
                    write.backend,
                    host,
                )?);
            }
            BlobKind::OsmData => {
                osmdata_blobs += 1;
                let w = writer
                    .as_mut()
                    .ok_or_else(|| crate::error::new_missing_header(input))?;
                if let Some(ref idx) = frame.index {
                    let has_match = match idx.kind {
                        crate::ElementKind::Node => {
                            ids.node_ids.any_in_range(idx.min_id, idx.max_id)
                        }
                        crate::ElementKind::Way => ids.way_ids.any_in_range(idx.min_id, idx.max_id),
                        crate::ElementKind::Relation => {
                            ids.relation_ids.any_in_range(idx.min_id, idx.max_id)
                        }
                    };
                    if !blob_filter.wants_index(idx) || !has_match {
                        blobs_skipped += 1;
                        continue;
                    }
                }
                // Blob might contain matching IDs, or carries no indexdata
                // and must be decoded to check.
                decompress_buf.clear();
                crate::blob::decompress_blob_data_into(frame.blob_bytes(), &mut decompress_buf)
                    .map_err(locate)?;
                // Move the decompressed Vec into the block: no Bytes round-trip,
                // no second whole-buffer copy (the `new_with_scratch(Bytes)` route
                // paid a `to_vec`). `decompress_buf` is reallocated next blob.
                let block = PrimitiveBlock::from_vec_with_scratch(
                    std::mem::take(&mut decompress_buf),
                    &mut st_scratch,
                    &mut gr_scratch,
                )
                .map_err(locate)?;
                output_blocks.clear();
                let outcome = process_block(
                    &block,
                    &mut bb,
                    &mut output_blocks,
                    ids,
                    true,
                    None,
                    false,
                    report_found,
                )
                .map_err(locate)?;
                flush_local(&mut bb, &mut output_blocks)?;
                for OwnedBlock {
                    bytes: block_bytes,
                    index,
                    tagdata,
                    way_members,
                } in output_blocks.drain(..)
                {
                    w.write_primitive_block_owned(
                        block_bytes,
                        index,
                        tagdata.as_deref(),
                        way_members.as_deref(),
                    )?;
                }
                stats.nodes_written += outcome.nodes;
                stats.ways_written += outcome.ways;
                stats.relations_written += outcome.relations;
                if let Some(found) = stats.found.as_mut() {
                    for (kind, id) in outcome.found {
                        found.insert(kind, id);
                    }
                }
            }
            _ => {}
        }
    }

    let mut writer = writer.ok_or_else(|| crate::error::new_missing_header(input))?;
    crate::debug::emit_counter("walk_actual_osmdata_blobs", osmdata_blobs);
    if blobs_skipped > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "getid.blobs_skipped",
            "{blobs_skipped} blobs skipped by ID range filter"
        );
    }
    writer.flush()?;
    crate::debug::emit_marker("GETID_SCAN_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Two-pass getid with --add-referenced
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn getid_with_refs(
    input: &Path,
    output: &crate::output::Output,
    ids: &ElementIds,
    opts: &GetidOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<GetidStats> {
    let mut stats = GetidStats {
        nodes_written: 0,
        passthrough_claimed: 0,
        ways_written: 0,
        relations_written: 0,
        found: opts.report_found.then(ElementIds::empty),
    };

    // Pass 1: Collect ref node IDs from matching ways. Uses IdSet for O(1)
    // lookups in pass 2 instead of BTreeSet's O(log n).
    crate::debug::emit_marker("GETID_PASS1_START");
    let mut dep_node_ids = crate::idset::IdSet::new();
    let mut has_dep_nodes = false;

    if ids.way_ids.has_any() {
        // Parallel classification: pread workers scan way blobs for matching
        // way IDs and collect their node refs. A blob without indexdata is
        // scheduled and decoded, and the worker looks only at the ways it
        // actually holds, so a partially indexed file is accepted here as on
        // every other getid path (see `getid_dispatched`).
        let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
            input,
            Some(crate::ElementKind::Way),
            crate::scan::classify::PartialIndex::Accept,
            host,
        )?;

        crate::scan::classify::parallel_classify_accumulate(
            &shared_file,
            &schedule,
            None,
            crate::idset::IdSet::new,
            |block, node_ids| {
                for element in block.elements_skip_metadata() {
                    if let Element::Way(w) = &element
                        && ids.way_ids.get(w.id())
                    {
                        for r in w.refs() {
                            node_ids.set(r);
                        }
                    }
                }
                Ok(())
            },
            |worker_node_ids| {
                if worker_node_ids.has_any() {
                    has_dep_nodes = true;
                }
                dep_node_ids.merge(worker_node_ids);
            },
            host,
        )?;
    }
    // When --remove-tags is set, referenced-only nodes (not explicitly requested)
    // get their tags stripped. Check at query time: dep_node_ids.get(id) && !ids.node_ids.get(id).
    let strip_tags = opts.remove_tags && has_dep_nodes;
    crate::debug::emit_marker("GETID_PASS1_END");
    // Cheap one-time iteration; gives the dep-set size before pass 2
    // starts so the counter survives a SIGKILL during pass 2.
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("getid_dep_node_ids", dep_node_ids.iter().count());
    }

    // Pass 2: Write matching elements + dependent nodes (transform fused into
    // the decode workers).
    crate::debug::emit_marker("GETID_PASS2_START");
    let reader = ElementReader::open_in(input, write.direct_reads, host)?;
    // Skip blob types not needed: nodes if no node IDs and no dependent nodes,
    // ways always needed (add-referenced mode), relations if no relation IDs.
    let reader = reader.with_blob_filter(BlobFilter::new(
        ids.node_ids.has_any() || has_dep_nodes,
        true,
        ids.relation_ids.has_any(),
    ));
    let mut writer = writer_from_header(
        output,
        write.compression,
        reader.header(),
        &super::output_header::OutputFacts::carry(super::output_header::Ways::Reencoded),
        overrides,
        write.backend,
        host,
    )?;

    let dep_ref = if has_dep_nodes {
        Some(&dep_node_ids)
    } else {
        None
    };

    reader.for_each_fused_block(
        |block| {
            let mut bb = BlockBuilder::new();
            let mut output = Vec::new();
            let outcome = process_block(
                &block,
                &mut bb,
                &mut output,
                ids,
                true,
                dep_ref,
                strip_tags,
                opts.report_found,
            )?;
            flush_local(&mut bb, &mut output)?;
            Ok((output, outcome))
        },
        |(blocks, outcome)| {
            for OwnedBlock {
                bytes,
                index,
                tagdata,
                way_members,
            } in blocks
            {
                writer.write_primitive_block_owned(
                    bytes,
                    index,
                    tagdata.as_deref(),
                    way_members.as_deref(),
                )?;
            }
            stats.nodes_written += outcome.nodes;
            stats.ways_written += outcome.ways;
            stats.relations_written += outcome.relations;
            if let Some(found) = stats.found.as_mut() {
                for (kind, id) in outcome.found {
                    found.insert(kind, id);
                }
            }
            Ok(())
        },
    )?;

    writer.flush()?;
    crate::debug::emit_marker("GETID_PASS2_END");
    Ok(stats)
}

/// Whether the element is covered by the query: requested by ID, or -
/// for nodes under `--add-referenced` - pulled in as a way's node ref.
///
/// Include mode emits dominated elements; invert mode emits the rest.
fn is_dominated(
    ids: &ElementIds,
    dep_node_ids: Option<&crate::idset::IdSet>,
    element: &Element<'_>,
) -> bool {
    match element {
        Element::DenseNode(dn) => {
            ids.node_ids.get(dn.id()) || dep_node_ids.is_some_and(|deps| deps.get(dn.id()))
        }
        Element::Node(n) => {
            ids.node_ids.get(n.id()) || dep_node_ids.is_some_and(|deps| deps.get(n.id()))
        }
        Element::Way(w) => ids.way_ids.get(w.id()),
        Element::Relation(r) => ids.relation_ids.get(r.id()),
    }
}

/// Note the element about to be written, if the user actually asked for
/// it by ID.
///
/// Only *requested* IDs count: in `--add-referenced` mode a node can be
/// emitted because a matched way refers to it, which is not the same as
/// the user having asked for it, and reporting those would make the
/// "did not find" list go silent on genuine misses. Ways and relations
/// reach the write path in include mode only when they matched
/// directly, so they need no such guard.
fn record_found(found: &mut Vec<(ElementKind, i64)>, ids: &ElementIds, element: &Element<'_>) {
    match element {
        Element::DenseNode(dn) if ids.node_ids.get(dn.id()) => {
            found.push((ElementKind::Node, dn.id()));
        }
        Element::Node(n) if ids.node_ids.get(n.id()) => {
            found.push((ElementKind::Node, n.id()));
        }
        Element::Way(w) => found.push((ElementKind::Way, w.id())),
        Element::Relation(r) => found.push((ElementKind::Relation, r.id())),
        _ => {}
    }
}

/// Process a single `PrimitiveBlock` through the ID filter, writing matching
/// elements into the thread-local `BlockBuilder` and flushing complete blocks
/// into `output`. Returns the per-block [`BlockOutcome`].
///
/// Called on the decode-worker threads via `for_each_fused_block`.
#[allow(clippy::too_many_arguments)]
fn process_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    ids: &ElementIds,
    include: bool,
    dep_node_ids: Option<&crate::idset::IdSet>,
    strip_tags: bool,
    report_found: bool,
) -> crate::Result<BlockOutcome> {
    let mut nodes: u64 = 0;
    let mut ways: u64 = 0;
    let mut relations: u64 = 0;
    let mut found: Vec<(ElementKind, i64)> = Vec::new();

    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        let dominated = is_dominated(ids, dep_node_ids, &element);
        let emit = if include { dominated } else { !dominated };
        if !emit {
            continue;
        }

        if report_found && include {
            record_found(&mut found, ids, &element);
        }

        match &element {
            Element::DenseNode(dn) => {
                ensure_node_capacity_local(bb, output)?;
                // Strip tags from referenced-only nodes (dep but not explicit)
                let strip = strip_tags
                    && dep_node_ids.is_some_and(|deps| deps.get(dn.id()))
                    && !ids.node_ids.get(dn.id());
                let meta = dense_node_metadata(dn)?;
                if strip {
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        std::iter::empty::<(&str, &str)>(),
                        meta.as_ref(),
                    );
                } else {
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        dn.tags(),
                        meta.as_ref(),
                    );
                }
                nodes += 1;
            }
            Element::Node(n) => {
                ensure_node_capacity_local(bb, output)?;
                let strip = strip_tags
                    && dep_node_ids.is_some_and(|deps| deps.get(n.id()))
                    && !ids.node_ids.get(n.id());
                let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                if strip {
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        std::iter::empty::<(&str, &str)>(),
                        meta.as_ref(),
                    );
                } else {
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        n.tags(),
                        meta.as_ref(),
                    );
                }
                nodes += 1;
            }
            Element::Way(w) => {
                ensure_way_capacity_local(bb, output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
                bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                ways += 1;
            }
            Element::Relation(r) => {
                ensure_relation_capacity_local(bb, output)?;
                relation_members_into(r, &mut members_buf)?;
                let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                relations += 1;
            }
        }
    }

    Ok(BlockOutcome {
        nodes,
        ways,
        relations,
        found,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Tests use `unwrap()` throughout because panicking is the correct failure mode
// for unit tests -- it immediately fails the test with a clear backtrace pointing
// to the exact call site. Propagating Results via `-> Result<()>` in tests would
// lose the backtrace and produce less actionable error messages. The crate-wide
// `unwrap_used = "deny"` lint is designed for production code where panics are
// unacceptable; test code is exempt via this module-level allow.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::writer::Compression;

    #[test]
    fn parse_node_id() {
        let (prefix, id) = parse_id_spec("n123", None).unwrap();
        assert_eq!(prefix, ElementKind::Node);
        assert_eq!(id, 123);
    }

    #[test]
    fn parse_way_id() {
        let (prefix, id) = parse_id_spec("w456", None).unwrap();
        assert_eq!(prefix, ElementKind::Way);
        assert_eq!(id, 456);
    }

    #[test]
    fn parse_relation_id() {
        let (prefix, id) = parse_id_spec("r789", None).unwrap();
        assert_eq!(prefix, ElementKind::Relation);
        assert_eq!(id, 789);
    }

    #[test]
    fn element_ref_parser_accepts_short_and_named_types() {
        for (spec, kind, id) in [
            ("n123", ElementKind::Node, 123),
            ("node/123", ElementKind::Node, 123),
            ("w456", ElementKind::Way, 456),
            ("way/456", ElementKind::Way, 456),
            ("r789", ElementKind::Relation, 789),
            ("relation/789", ElementKind::Relation, 789),
            ("node/-5", ElementKind::Node, -5),
        ] {
            let element_ref = spec.parse::<ElementRef>().unwrap();
            assert_eq!(element_ref.kind, kind);
            assert_eq!(element_ref.id, id);
        }
    }

    #[test]
    fn parse_large_id() {
        let (prefix, id) = parse_id_spec("n9876543210", None).unwrap();
        assert_eq!(prefix, ElementKind::Node);
        assert_eq!(id, 9_876_543_210);
    }

    #[test]
    fn parse_invalid_prefix() {
        assert!(parse_id_spec("x123", None).is_err());
    }

    #[test]
    fn parse_missing_number() {
        assert!(parse_id_spec("n", None).is_err());
    }

    #[test]
    fn parse_bad_number() {
        assert!(parse_id_spec("nabc", None).is_err());
    }

    #[test]
    fn parse_too_short() {
        assert!(parse_id_spec("n", None).is_err());
        assert!(parse_id_spec("", None).is_err());
    }

    /// osmium's `read_id_file`: strip spaces, cut at the first space or `#`,
    /// skip empty. The previous reader honoured `#` only at line start, so
    /// `n123 # note` was an error here and `n123` in osmium.
    #[test]
    fn id_file_token_follows_the_osmium_reader() {
        assert_eq!(id_file_token("n123"), Some("n123"));
        assert_eq!(id_file_token("  n123  "), Some("n123"));
        assert_eq!(id_file_token("n123 # note"), Some("n123"));
        assert_eq!(id_file_token("n123#note"), Some("n123"));
        assert_eq!(id_file_token("n123 w456"), Some("n123"));
        assert_eq!(id_file_token("# comment"), None);
        assert_eq!(id_file_token("   # indented comment"), None);
        assert_eq!(id_file_token(""), None);
        assert_eq!(id_file_token("   "), None);
        // CRLF line endings: one trailing CR is a line ending, not id text.
        assert_eq!(id_file_token("n123\r"), Some("n123"));
        assert_eq!(id_file_token("n123#c\r"), Some("n123"));
        assert_eq!(id_file_token("\r"), None);
        // Tabs are not whitespace here, as in osmium: the token keeps them
        // and fails to parse.
        assert_eq!(id_file_token("\tn123"), Some("\tn123"));
        assert!(parse_id_spec("\tn123", None).is_err());
        assert!(parse_id_spec("n123\t", None).is_err());
    }

    #[test]
    fn id_file_inline_comments_and_crlf() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("ids.txt");
        std::fs::write(
            &path,
            "# header\r\nn1 # first\r\nw10#way\r\n\r\n  r7  extra\r\n",
        )
        .unwrap();
        let ids = parse_ids_from_file(&path).unwrap();
        assert!(ids.node_ids.get(1));
        assert!(ids.way_ids.get(10));
        assert!(ids.relation_ids.get(7));
        assert!(!ids.node_ids.get(7));

        std::fs::write(&path, "n1\n\tn2\n").unwrap();
        assert!(
            parse_ids_from_file(&path).is_err(),
            "a tab-led id is an illegal id, as in osmium"
        );
    }

    #[test]
    fn parse_ids_mixed() {
        let specs: Vec<String> = vec!["n1", "n2", "w10", "r100"]
            .into_iter()
            .map(ToString::to_string)
            .collect();
        let set = parse_ids(&specs).unwrap();
        assert!(set.node_ids.get(1));
        assert!(set.node_ids.get(2));
        assert!(!set.node_ids.get(3));
        assert!(set.way_ids.get(10));
        assert!(set.relation_ids.get(100));
    }

    #[test]
    fn parse_ids_accept_named_type_forms() {
        let specs: Vec<String> = vec!["node/1", "way/10", "relation/100"]
            .into_iter()
            .map(ToString::to_string)
            .collect();
        let set = parse_ids(&specs).unwrap();
        assert!(set.node_ids.get(1));
        assert!(set.way_ids.get(10));
        assert!(set.relation_ids.get(100));
    }

    #[test]
    fn parse_bare_id_with_default_type_node() {
        let (prefix, id) = parse_id_spec("42", Some(ElementKind::Node)).unwrap();
        assert_eq!(prefix, ElementKind::Node);
        assert_eq!(id, 42);
    }

    #[test]
    fn parse_ids_bare_with_default_type_way() {
        let specs: Vec<String> = vec!["1", "2", "w10", "r100"]
            .into_iter()
            .map(ToString::to_string)
            .collect();
        let set = parse_ids_with_default_type(&specs, Some(ElementKind::Way)).unwrap();
        assert!(!set.node_ids.has_any());
        assert!(set.way_ids.get(1));
        assert!(set.way_ids.get(2));
        assert!(set.way_ids.get(10));
        assert!(set.relation_ids.get(100));
    }

    #[test]
    fn parse_ids_bare_without_default_type_errors() {
        let specs: Vec<String> = vec!["123".to_string()];
        assert!(parse_ids_with_default_type(&specs, None).is_err());
    }

    #[test]
    fn parse_negative_id_rejected_with_named_id_and_kind() {
        let err = parse_id_spec("n-1", None).unwrap_err().to_string();
        assert!(err.contains("non-negative"), "{err}");
        assert!(err.contains("node id -1"), "{err}");

        let err = parse_id_spec("w-42", None).unwrap_err().to_string();
        assert!(err.contains("way id -42"), "{err}");

        let err = parse_id_spec("r-7", None).unwrap_err().to_string();
        assert!(err.contains("relation id -7"), "{err}");

        // Bare negative number with default type must also reject.
        let err = parse_id_spec("-5", Some(ElementKind::Node))
            .unwrap_err()
            .to_string();
        assert!(err.contains("node id -5"), "{err}");
    }

    /// Write a node block, then optionally a way block, which can be written
    /// without indexdata to model a partially indexed input.
    fn write_blocks(
        path: &Path,
        node_ids: &[i64],
        way: Option<(i64, &[i64])>,
        index_way_blob: bool,
    ) {
        let file = std::fs::File::create(path).unwrap();
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::default(),
        );
        writer
            .write_header(
                &crate::block_builder::HeaderBuilder::new()
                    .sorted()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        let mut block = BlockBuilder::new();
        for &id in node_ids {
            block.add_node(id, 0, 0, std::iter::empty::<(&str, &str)>(), None);
        }
        writer
            .write_primitive_block(block.take().unwrap().unwrap())
            .unwrap();
        if let Some((way_id, refs)) = way {
            block.add_way(way_id, std::iter::empty::<(&str, &str)>(), refs, None);
            let bytes = block.take().unwrap().unwrap();
            if index_way_blob {
                writer.write_primitive_block(bytes).unwrap();
            } else {
                writer.write_primitive_block_no_indexdata(bytes).unwrap();
            }
        }
        writer.flush().unwrap();
    }

    fn read_ids(path: &Path, host: &crate::host::Host) -> Vec<i64> {
        let mut found = Vec::new();
        ElementReader::open_in(path, false, host)
            .unwrap()
            .for_each(|element| match element {
                Element::DenseNode(node) => found.push(node.id()),
                Element::Node(node) => found.push(node.id()),
                Element::Way(way) => found.push(way.id()),
                Element::Relation(relation) => found.push(relation.id()),
            })
            .unwrap();
        found.sort_unstable();
        found
    }

    #[test]
    fn with_refs_writes_referenced_elements() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2, 3], Some((10, &[1, 2])), true);
        let ids = parse_ids(&["w10".to_owned()]).unwrap();
        let opts = GetidOptions {
            add_referenced: true,
            remove_tags: false,
            report_found: false,
        };
        let output = dir.path().join("output.pbf");
        let staged = crate::output::Output::new(
            &output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .unwrap();
        getid_with_refs(
            &input,
            &staged,
            &ids,
            &opts,
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            &crate::host::Host::for_tests(),
        )
        .unwrap();
        staged.publish(&crate::host::Cancel::new()).unwrap();
        assert_eq!(
            read_ids(&output, &crate::host::Host::for_tests()),
            vec![1, 2, 10]
        );
    }

    /// A file indexed on its node blob and not on its way blob is accepted by
    /// `--add-referenced`'s classify walk and by the `-I` reader, and both
    /// read the unindexed blob's actual elements: the way and the nodes it
    /// references come out, and every ID is collected.
    #[test]
    fn partially_indexed_input_is_read_in_full_by_the_classify_walks() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("partial.pbf");
        write_blocks(&input, &[1, 2, 3], Some((10, &[1, 2])), false);
        let host = crate::host::Host::for_tests();
        let ids = parse_ids(&["w10".to_owned()]).unwrap();
        let opts = GetidOptions {
            add_referenced: true,
            remove_tags: false,
            report_found: false,
        };
        let output = dir.path().join("output.pbf");
        let staged = crate::output::Output::new(
            &output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .unwrap();
        let (arm, stats) = getid_dispatched(
            &input,
            &staged,
            &ids,
            &opts,
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            FULL_SCAN_ARM_MIN_BLOBS,
            &host,
        )
        .unwrap();
        staged.publish(&crate::host::Cancel::new()).unwrap();
        assert_eq!(arm, None);
        assert_eq!((stats.nodes_written, stats.ways_written), (2, 1));
        assert_eq!(read_ids(&output, &host), vec![1, 2, 10]);

        let collected = parse_ids_from_pbf(&input, false, &host).unwrap();
        assert!((1..=3).all(|id| collected.node_ids.get(id)));
        assert!(collected.way_ids.get(10));
        assert!(!collected.relation_ids.has_any());
    }

    /// Run the same include-mode query under both arms, assert they emit
    /// identical element sets, and return that common (sorted) ID list.
    fn ids_from_both_arms(input: &Path, query: &[&str], host: &crate::host::Host) -> Vec<i64> {
        let dir = crate::path_guard::tests::scratch();
        let query: Vec<String> = query.iter().map(|s| (*s).to_owned()).collect();
        let query = parse_ids(&query).unwrap();
        let walker = dir.path().join("walker.pbf");
        let full_scan = dir.path().join("full-scan.pbf");
        for (output, arm) in [(&walker, ScanArm::Walker), (&full_scan, ScanArm::FullScan)] {
            let staged = crate::output::Output::new(
                output,
                crate::output::Overwrite::Refuse,
                &crate::host::Diagnostics::discard(),
            )
            .unwrap();
            filter_by_id(
                input,
                &staged,
                &query,
                true,
                super::Ways::Reencoded,
                &WriteOptions::default(),
                &HeaderOverrides::default(),
                arm,
                false,
                host,
            )
            .unwrap();
            staged.publish(&crate::host::Cancel::new()).unwrap();
        }
        let walker_ids = read_ids(&walker, host);
        assert_eq!(walker_ids, read_ids(&full_scan, host));
        walker_ids
    }

    #[test]
    fn walker_and_full_scan_include_arms_emit_the_same_elements() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], Some((10, &[1, 2])), true);
        assert_eq!(
            ids_from_both_arms(&input, &["n1", "w10"], &crate::host::Host::for_tests()),
            vec![1, 10]
        );
    }

    #[test]
    fn include_arms_agree_on_empty_query_result() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], Some((10, &[1, 2])), true);
        assert!(ids_from_both_arms(&input, &["n999"], &crate::host::Host::for_tests()).is_empty());
    }

    #[test]
    fn include_arms_agree_on_single_data_blob_file() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], None, true);
        assert_eq!(
            ids_from_both_arms(&input, &["n2"], &crate::host::Host::for_tests()),
            vec![2]
        );
    }

    #[test]
    fn include_arms_agree_when_a_blob_lacks_indexdata() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], Some((10, &[1, 2])), false);
        assert_eq!(
            ids_from_both_arms(&input, &["n1", "w10"], &crate::host::Host::for_tests()),
            vec![1, 10]
        );
    }

    #[test]
    fn auto_dispatch_crosses_arms_at_the_injected_threshold() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], Some((10, &[1, 2])), true);
        let query = parse_ids(&["n1".to_owned(), "w10".to_owned()]).unwrap();
        let opts = GetidOptions {
            add_referenced: false,
            remove_tags: false,
            report_found: false,
        };
        for (min_blobs, expected_arm) in [
            (1, ScanArm::FullScan),
            (FULL_SCAN_ARM_MIN_BLOBS, ScanArm::Walker),
        ] {
            let output = dir.path().join(format!("out-{min_blobs}.pbf"));
            let staged = crate::output::Output::new(
                &output,
                crate::output::Overwrite::Refuse,
                &crate::host::Diagnostics::discard(),
            )
            .unwrap();
            let (arm, _) = getid_dispatched(
                &input,
                &staged,
                &query,
                &opts,
                &WriteOptions::default(),
                &HeaderOverrides::default(),
                min_blobs,
                &crate::host::Host::for_tests(),
            )
            .unwrap();
            staged.publish(&crate::host::Cancel::new()).unwrap();
            assert_eq!(arm, Some(expected_arm));
            assert_eq!(
                read_ids(&output, &crate::host::Host::for_tests()),
                vec![1, 10]
            );
        }
    }
}
