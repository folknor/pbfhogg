//! Extract or remove elements by ID. Equivalent to `osmium getid` / `osmium removeid`.

use std::path::Path;

use crate::idset::IdSet;

use super::output_header::{Declared, OutputFacts, Ways};
use super::relation_closure::{EdgeStore, RelationRefs};
use super::{
    HeaderOverrides, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local, require_indexdata, writer_from_header,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::error::Error;
use crate::file_writer::FileWriter;
use crate::owned::{dense_node_metadata, element_metadata};
use crate::read::block::{BlockBuffer, Selection, SelectionRefs};
use crate::read::header_walker::ScanArm;
use crate::read::input::{InputFile, InputSource, PinnedInput};
use crate::read::project_ids::{RelationSpans, WaySpans};
use crate::scan::classify::ScheduledBlob;
use crate::tunables::FULL_SCAN_ARM_MIN_BLOBS;
use crate::write::WriteOptions;
use crate::writer::PbfWriter;
use crate::{BlobFilter, Element, ElementKind, ElementReader, MemberId};

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
            ElementKind::Node => self.node_ids.insert(id),
            ElementKind::Way => self.way_ids.insert(id),
            ElementKind::Relation => self.relation_ids.insert(id),
        }
    }

    /// The set of one kind.
    pub(super) fn of(&self, kind: ElementKind) -> &IdSet {
        match kind {
            ElementKind::Node => &self.node_ids,
            ElementKind::Way => &self.way_ids,
            ElementKind::Relation => &self.relation_ids,
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
#[derive(Default)]
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
/// (`node/123`). It keeps signed IDs intact: a negative ID is an ID like any
/// other, and every `i64` is accepted.
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

/// Parse an ID spec like "n123", "w456", "r-789".
///
/// Any `i64` is accepted, negative ones and both extremes included: getid
/// and getparents select by the exact signed ID. osmium refuses a negative
/// text ID (and `LLONG_MIN`/`LLONG_MAX` as illegal) while matching every
/// other ID by its absolute value; pbfhogg is exact instead.
// The messages show the bad input value, which is more helpful for CLI users
// than the underlying ParseIntError.
fn parse_id_spec(spec: &str, default_type: Option<ElementKind>) -> Result<(ElementKind, i64)> {
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
/// indexdata decides nothing here; a partially indexed file is still
/// refused, as by every command input walk (`Frames::refuse_partial_index`).
///
/// The input is opened once, and that one description serves the whole
/// read, so a path reads one file without a pin. A caller that pins it
/// anyway (the CLI pins every input before reading any) passes the pin.
///
/// `_direct_io` is accepted and not yet read: the schedule walk this reads
/// through has no O_DIRECT route. The parameter is kept on purpose, as
/// plumbing for the planned redesign of how `--direct-io` reaches each
/// read; do not remove it as dead.
pub fn parse_ids_from_pbf<'a>(
    input: impl Into<InputSource<'a>>,
    _direct_io: bool,
    host: &crate::host::Host,
) -> Result<ElementIds> {
    let host = &host.enter()?;
    let mut set = ElementIds::empty();

    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        input,
        None,
        crate::scan::classify::KindClaims::Refuse,
        host,
    )?;

    struct IdBatch {
        node_ids: Vec<i64>,
        way_ids: Vec<i64>,
        relation_ids: Vec<i64>,
    }

    // Every element's ID, read through the shared ID projection on the
    // scoped block validator. The three sets are independent unions, so
    // the order between kinds does not matter.
    let mut all_kinds = crate::element_kind::ElementKindSet::default();
    for kind in [ElementKind::Node, ElementKind::Way, ElementKind::Relation] {
        all_kinds.add(kind);
    }
    crate::scan::classify::parallel_project_phase(
        &shared_file,
        &schedule,
        None,
        crate::read::project_ids::IdScratch::default,
        |_, buffer, scratch| {
            let (crate::read::project_ids::KindIds { .. }, accepted) = buffer
                .project_ids_and_refs(|_, _| {
                    crate::read::project_ids::KindIds::new(all_kinds, &mut *scratch)
                })?;
            let mut batch = IdBatch {
                node_ids: Vec::new(),
                way_ids: Vec::new(),
                relation_ids: Vec::new(),
            };
            scratch.each(&accepted, |kind, id| {
                match kind {
                    ElementKind::Node => batch.node_ids.push(id),
                    ElementKind::Way => batch.way_ids.push(id),
                    ElementKind::Relation => batch.relation_ids.push(id),
                }
                Ok(())
            })?;
            Ok(batch)
        },
        |_seq, batch| {
            insert_all(&mut set.node_ids, batch.node_ids);
            insert_all(&mut set.way_ids, batch.way_ids);
            insert_all(&mut set.relation_ids, batch.relation_ids);
        },
        host,
    )?;

    Ok(set)
}

/// Insert every ID of `ids` into `set`, whatever its sign.
fn insert_all(set: &mut IdSet, ids: impl IntoIterator<Item = i64>) {
    for id in ids {
        set.insert(id);
    }
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

/// The reference-following mode of [`getid`], as osmium's `getid -r`: the
/// requested objects plus everything they reference. The members of
/// requested relations, recursively through relations in relations
/// (cycle-safe), then the nodes of every way requested or pulled in. With
/// the setting that means something only there.
pub struct AddReferenced {
    /// Strip tags from every added object, of any kind, that was not itself
    /// requested. A requested object keeps its tags even where something
    /// else requested also references it.
    pub remove_tags: bool,
}

/// Options for getid.
pub struct GetidOptions {
    /// `Some` runs the mode that adds referenced objects ([`AddReferenced`]).
    /// `None` emits exact ID matches only; that mode has no setting of its
    /// own, so there is nothing to refuse.
    pub add_referenced: Option<AddReferenced>,
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
/// With `opts.add_referenced` set, the objects the requested ones reference
/// are also included ([`AddReferenced`]). Otherwise, only exact ID matches
/// are output.
///
/// Every route opens the input more than once, so the input must be a
/// regular file (`InvalidArgument` otherwise) and is
/// pinned for the run once the options are accepted (see [`PinnedInput`]
/// and [`InputSource::pin`]); a caller may pin it first and pass the pin.
/// Where a later open cannot reach the pinned file, an input replaced
/// mid-run fails with `InputReplaced`.
pub fn getid<'a>(
    input: impl Into<InputSource<'a>>,
    output: impl crate::output::BindOutput,
    ids: &ElementIds,
    opts: &GetidOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<GetidStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = getid_to(input, &output, ids, opts, write, overrides, host)?;
    output.publish()?;
    Ok(result)
}

/// [`getid`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn getid_to<'a>(
    input: impl Into<InputSource<'a>>,
    output: &crate::output::BoundOutput,
    ids: &ElementIds,
    opts: &GetidOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
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
fn getid_dispatched<'a>(
    input: impl Into<InputSource<'a>>,
    output: &crate::output::BoundOutput,
    ids: &ElementIds,
    opts: &GetidOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    min_blobs: u64,
    host: &crate::host::Entered<'_>,
) -> Result<(Option<ScanArm>, GetidStats)> {
    // Every route opens the input more than once, each open its own open
    // file description (the indexdata probe, the arm estimate, the walker or
    // the streaming reader; `add_referenced`'s one schedule walk, whose
    // descriptor serves every reference phase, and pass 2's reader), and each is
    // derived from this one pin, so a file renamed over the input mid-run
    // cannot be read partly from each: where `/proc/self/fd` reopens the pin
    // every open reaches the pinned file, otherwise on Unix a replaced input
    // fails the run (`PinnedInput` says what it does off Unix). Taken before
    // any other open of the input; a caller's pin is used as it is.
    let pin = input.into().pin(host)?;
    let input = &pin;
    // Every element is still matched against the requested IDs on a forced
    // run; what is lost is the blob skipping. Every path skips on kind and
    // ID range; `add_referenced`'s first relation read alone reads every
    // relation blob.
    // `--add-referenced` with no way or relation requested pulls nothing in
    // (only ways and relations reference anything), so it is plain include
    // and runs on the include arms.
    let add_referenced = opts
        .add_referenced
        .as_ref()
        .filter(|_| ids.way_ids.has_any() || ids.relation_ids.has_any());
    let reason = if add_referenced.is_some() {
        "input PBF has no blob-level indexdata. Without it, blobs cannot be skipped by \
         element type or ID range, so blobs a pass does not need are decompressed too."
    } else {
        "input PBF has no blob-level indexdata. Without it, blobs cannot be skipped by \
         element type or ID range, so blobs holding none of the requested IDs are \
         decompressed too."
    };
    let has_indexdata = require_indexdata(input, write.direct_reads, write.force, reason, host)?;

    // The indexdata result selects an arm only for the no-references path.
    // add_referenced still shares the refusal above: --force explicitly opts
    // into its slower unindexed scans even though it does not dispatch an arm.
    //
    // A partially indexed file (indexdata on some data blobs and not others,
    // in either order) is refused on every path: both include arms, the
    // invert walker, `--add-referenced`'s two passes and `-I`'s walk each
    // read through a command-input walk, which refuses it once both kinds of
    // blob have been seen (`Frames::refuse_partial_index`), whichever arm the
    // blob count picked. An unindexed first data blob is refused by the probe
    // above before that, unless `--force`. reference/indexdata-trust.md,
    // "Missing and partial indexdata".
    let (arm, result) = if let Some(add) = add_referenced {
        let stats = getid_with_refs(
            input,
            output,
            ids,
            add,
            opts.report_found,
            write,
            overrides,
            host,
        )?;
        (None, stats)
    } else {
        let arm = super::dispatch_scan_arm(input, has_indexdata, min_blobs, host)?;
        let stats = filter_by_id(
            input,
            output,
            ids,
            Filter::Include(arm),
            write,
            overrides,
            opts.report_found,
            host,
        )?;
        (Some(arm), stats)
    };
    host.invocation
        .counter("getid_nodes_written", result.nodes_written);
    host.invocation
        .counter("getid_ways_written", result.ways_written);
    host.invocation
        .counter("getid_relations_written", result.relations_written);
    Ok((arm, result))
}

/// Remove elements matching the given IDs (output everything else).
///
/// Requires blob-level indexdata so the invert-mode raw-passthrough fast
/// path at `filter_by_id` can skip re-encoding non-matching blobs. On a
/// non-indexed PBF the fast path is unreachable and every blob would
/// decode-and-re-encode silently; `--force` overrides.
///
/// The input is opened twice (the indexdata probe, then the walk), so it
/// is pinned as in [`getid`], with the same refusals.
pub fn removeid<'a>(
    input: impl Into<InputSource<'a>>,
    output: impl crate::output::BindOutput,
    ids: &ElementIds,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<GetidStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = removeid_to(input, &output, ids, write, overrides, host)?;
    output.publish()?;
    Ok(result)
}

/// [`removeid`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn removeid_to<'a>(
    input: impl Into<InputSource<'a>>,
    output: &crate::output::BoundOutput,
    ids: &ElementIds,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<GetidStats> {
    // The probe and the walk each open the input; see `getid_dispatched`.
    let pin = input.into().pin(host)?;
    let input = &pin;
    let indexed = require_indexdata(
        input,
        write.direct_reads,
        write.force,
        "input PBF has no blob-level indexdata. Without indexdata, the \
         invert-mode raw-passthrough fast path is unreachable and every \
         blob is decompressed and re-encoded (significantly slower).",
        host,
    )?;
    // Every way blob is copied whole when no way ID is requested and the blobs
    // carry indexdata (an unindexed blob behind an indexed first blob makes
    // the file partially indexed, which the walk refuses). The output is a
    // subset of the input with nothing changed, so the copied declarations
    // still hold. Otherwise way blobs are rewritten, and the rewritten ways
    // keep their inline coordinates exactly when the input declares them, so
    // they agree with the copied ones.
    let ways_copied = indexed && !ids.way_ids.has_any();
    // Invert mode is pinned to the walker arm. The streaming full-scan
    // arm implements include mode only; fusing invert's raw passthrough
    // into a sequential stream is possible but unmeasured (ADR-0006).
    filter_by_id(
        input,
        output,
        ids,
        Filter::Remove { ways_copied },
        write,
        overrides,
        false,
        host,
    )
}

/// What [`filter_by_id`] does with the requested IDs, and how it scans.
/// Removal runs only on the walker arm: the streaming full-scan arm
/// implements include mode alone, so a removal on it is not expressible.
#[derive(Clone, Copy)]
enum Filter {
    /// Write the requested elements, scanning on the given arm. Every way
    /// blob written is re-encoded.
    Include(ScanArm),
    /// Write everything but the requested elements, on the walker; with
    /// `ways_copied`, every way blob is copied whole.
    Remove { ways_copied: bool },
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
    input: &PinnedInput,
    output: &crate::output::BoundOutput,
    ids: &ElementIds,
    filter: Filter,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    report_found: bool,
    host: &crate::host::Entered<'_>,
) -> Result<GetidStats> {
    let (include_mode, ways_copied) = match filter {
        Filter::Include(ScanArm::FullScan) => {
            return filter_by_id_streaming(
                input,
                output,
                ids,
                write,
                overrides,
                report_found,
                host,
            );
        }
        Filter::Include(ScanArm::Walker) => (true, false),
        Filter::Remove { ways_copied } => (false, ways_copied),
    };
    filter_by_id_walker(
        input,
        output,
        ids,
        include_mode,
        ways_copied,
        write,
        overrides,
        report_found,
        host,
    )
}

/// The walker arm: one header walk, decoding on this thread each blob the
/// claimed kind and range keep. Decoding serially is deliberate: an
/// include query decodes only the blobs whose range holds a requested ID,
/// a handful on a narrow query (planet include warm under 0.2 s), and a
/// file with enough blobs for the walk to dominate goes to the full scan
/// arm (ADR-0006). A decode pool here would only add a hop.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn filter_by_id_walker(
    input: &PinnedInput,
    output: &crate::output::BoundOutput,
    ids: &ElementIds,
    include_mode: bool,
    ways_copied: bool,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    report_found: bool,
    host: &crate::host::Entered<'_>,
) -> Result<GetidStats> {
    use crate::blob::{BlobKind, decode_blob_to_headerblock};
    use crate::read::header_walker::HeaderWalker;

    let limits = crate::commands::split::Limits::new(write.compression);
    host.invocation.marker("GETID_SCAN_START");

    // Single pread-based pass over blob headers. Data bodies are only
    // read (via pread) when the blob matters: OsmHeader (once, to emit
    // the output header), OsmData-with-match in include mode, every
    // OsmData in invert mode (the raw-passthrough path still needs the
    // frame bytes). Include mode with a small ID set thus reads only
    // the header index (~140 MB at planet) plus a handful of matching
    // blob bodies.
    let mut walker = HeaderWalker::open(input, host)?;
    let mut data_buf = crate::read::read_buf::ReadBuf::default();
    let mut frame_buf = crate::read::read_buf::ReadBuf::default();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
    let mut bb = None;
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
    let selection = IdSelection {
        ids,
        pulled: None,
        include_matches: include_mode,
        strip_tags: false,
        report_found,
    };
    let mut blobs_skipped: u64 = 0;
    let mut blobs_passthrough: u64 = 0;
    let mut osmdata_blobs: u64 = 0;

    // Find and decode the leading OsmHeader blob to build the output header.
    let mut writer: Option<PbfWriter<FileWriter>> = None;
    let mut coords = crate::owned::WayCoords::Drop;
    let mut declared = Declared::default();

    while let Some(meta) = walker.next_header()? {
        // The walker owns provenance: failures on this blob are located with
        // the blob index and frame start it handed out.
        let locate = |e: Error| walker.locate_meta(&meta, e);
        match meta.blob_type {
            // The walker refuses a second OSMHeader, so this is the only one.
            BlobKind::OsmHeader => {
                let data = walker
                    .pread_data(meta.data_offset, meta.data_size, &mut data_buf)
                    .map_err(locate)?;
                let header = decode_blob_to_headerblock(data).map_err(locate)?;
                data_buf.release_outlier(&host.invocation);
                coords = crate::owned::WayCoords::of_input(&header);
                let facts = OutputFacts::carry(if ways_copied {
                    Ways::BlobsCopied
                } else {
                    coords.rewritten()
                });
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
                    .ok_or_else(|| crate::error::new_missing_header(input.path()))?;

                if let Some(ref idx) = meta.index {
                    let has_match = match idx.kind {
                        crate::ElementKind::Node => {
                            ids.node_ids.any_in_numeric_range(idx.min_id, idx.max_id)
                        }
                        crate::ElementKind::Way => {
                            ids.way_ids.any_in_numeric_range(idx.min_id, idx.max_id)
                        }
                        crate::ElementKind::Relation => ids
                            .relation_ids
                            .any_in_numeric_range(idx.min_id, idx.max_id),
                    };
                    if include_mode {
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
                        // A direct walker lends the frame from this buffer's
                        // aligned scratch, which `take` copies and leaves
                        // behind; it follows the retention line like any
                        // other read buffer, released before the writer
                        // call so an oversized scratch and its copy are not
                        // both held while the writer waits.
                        let frame = frame_buf.take();
                        frame_buf.release_outlier(&host.invocation);
                        w.write_raw_owned(frame)?;
                        blobs_passthrough += 1;
                        continue;
                    }
                }
                // Blob might contain matching IDs, or the blob carries no
                // indexdata and we must decode to check. Pread the data
                // and run the per-element filter.
                let data = walker
                    .pread_data(meta.data_offset, meta.data_size, &mut data_buf)
                    .map_err(locate)?;
                decompress_buf.clear();
                let decoded = crate::blob::decompress_blob_raw(data, &mut decompress_buf);
                // Done with the compressed bytes; the read buffer follows its
                // retention limit (read/read_buf.rs).
                data_buf.release_outlier(&host.invocation);
                decoded.map_err(locate)?;
                // Move the decompressed Vec into the block: no Bytes round-trip,
                // no second whole-buffer copy (the `new_with_scratch(Bytes)` route
                // paid a `to_vec`). `decompress_buf` is reallocated next blob.
                // The blob was routed (or passed over) on its claimed kind; a
                // block holding another kind is a lying claim, refused before
                // anything is written for it. An unindexed blob claims nothing.
                let buffer = BlockBuffer::new(
                    std::mem::take(&mut decompress_buf),
                    None,
                    &mut st_scratch,
                    &mut gr_scratch,
                    meta.index.as_ref().map(|idx| idx.kind),
                );
                output_blocks.clear();
                let outcome =
                    process_block(buffer, &mut bb, &mut output_blocks, &selection, coords)
                        .map_err(locate)?;
                // Only an unindexed blob can be a way blob decoded under a
                // header that declared the copied way enrichments (with
                // them, no way ID is removed, so an indexed way blob never
                // matches). The walk refuses a partially indexed file at that
                // blob's frame, before this decode, so this is a backstop for
                // the declaration: it must hold whatever the indexing policy
                // becomes. A header for rewritten ways declares only the
                // coordinates every rewritten way keeps, so it needs none.
                if ways_copied
                    && meta.index.is_none()
                    && declared.way_enrichment()
                    && output_blocks
                        .iter()
                        .any(|b| b.index.kind == crate::ElementKind::Way)
                {
                    return Err(locate(
                        declared.broken_by_unindexed_way_blob("re-encoding it"),
                    ));
                }
                crate::commands::split::fit_blocks(&mut output_blocks, &limits).map_err(locate)?;
                for block in output_blocks.drain(..) {
                    w.write_primitive_block_owned(block)?;
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

    let mut writer = writer.ok_or_else(|| crate::error::new_missing_header(input.path()))?;
    host.invocation
        .counter("walk_actual_osmdata_blobs", osmdata_blobs);
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
    host.invocation.marker("GETID_SCAN_END");
    Ok(stats)
}

/// Sequential include scan: read frame headers, skip non-matching OSMData
/// payloads via the indexdata kind + ID-range prescreen, and read + decode
/// only intersecting blobs. Payload skips retain buffered bytes when possible.
///
/// This is the measured high-blob-count arm for getid include mode
/// (ADR-0006). It deliberately does NOT use the pipelined reader: with a
/// sparse query nearly every blob is skipped, so the run is bounded by
/// walking headers and skipping payloads, and per-frame pipeline overhead
/// (permit accounting, rayon dispatch, channel hops) times millions of
/// small blobs loses to a plain sequential read loop.
#[allow(clippy::too_many_lines)]
fn filter_by_id_streaming(
    input: &PinnedInput,
    output: &crate::output::BoundOutput,
    ids: &ElementIds,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    report_found: bool,
    host: &crate::host::Entered<'_>,
) -> Result<GetidStats> {
    use crate::blob::{BlobKind, decode_blob_to_headerblock};
    use crate::file_reader::FileReader;
    use crate::read::frame::Frames;

    let limits = crate::commands::split::Limits::new(write.compression);
    host.invocation.marker("GETID_SCAN_START");

    let blob_filter = BlobFilter::new(
        ids.node_ids.has_any(),
        ids.way_ids.has_any(),
        ids.relation_ids.has_any(),
    );
    let mut writer: Option<PbfWriter<FileWriter>> = None;
    let mut coords = crate::owned::WayCoords::Drop;
    let mut stats = GetidStats {
        nodes_written: 0,
        passthrough_claimed: 0,
        ways_written: 0,
        relations_written: 0,
        found: report_found.then(ElementIds::empty),
    };
    let selection = IdSelection {
        ids,
        pulled: None,
        include_matches: true,
        strip_tags: false,
        report_found,
    };
    let mut blobs_skipped: u64 = 0;
    let mut osmdata_blobs: u64 = 0;

    let mut frames = Frames::of_file_input(
        FileReader::open_in(input, write.direct_reads, &host.invocation)?,
        input.path(),
        host,
    );
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut bb = None;
    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    while let Some(frame) = frames.next_header()? {
        // The frame loop owns the provenance of failures on this frame.
        let id = frame.id;
        match frame.header.blob_type {
            // The frame walk refuses a second OSMHeader, so this is the only
            // one.
            BlobKind::OsmHeader => {
                let mut payload = Vec::with_capacity(frame.data_size);
                frames.read_payload(&mut payload)?;
                let header =
                    decode_blob_to_headerblock(&payload).map_err(|e| frames.locate(id, e))?;
                coords = crate::owned::WayCoords::of_input(&header);
                let header_bytes = super::output_header::build_one(
                    &header,
                    &OutputFacts::carry(coords.rewritten()),
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
                    .ok_or_else(|| crate::error::new_missing_header(input.path()))?;
                let index = frame
                    .header
                    .indexdata
                    .as_ref()
                    .and_then(|data| crate::blob_meta::BlobIndex::deserialize(data));
                if let Some(ref idx) = index {
                    let has_match = match idx.kind {
                        crate::ElementKind::Node => {
                            ids.node_ids.any_in_numeric_range(idx.min_id, idx.max_id)
                        }
                        crate::ElementKind::Way => {
                            ids.way_ids.any_in_numeric_range(idx.min_id, idx.max_id)
                        }
                        crate::ElementKind::Relation => ids
                            .relation_ids
                            .any_in_numeric_range(idx.min_id, idx.max_id),
                    };
                    if !blob_filter.wants_index(idx) || !has_match {
                        frames.skip_payload()?;
                        blobs_skipped += 1;
                        continue;
                    }
                }
                // Blob might contain matching IDs, or carries no indexdata
                // and must be decoded to check.
                let mut payload = Vec::with_capacity(frame.data_size);
                frames.read_payload(&mut payload)?;
                let locate = |e: Error| frames.locate(id, e);
                decompress_buf.clear();
                crate::blob::decompress_blob_raw(&payload, &mut decompress_buf).map_err(locate)?;
                // Move the decompressed Vec into the block: no Bytes round-trip,
                // no second whole-buffer copy (the `new_with_scratch(Bytes)` route
                // paid a `to_vec`). `decompress_buf` is reallocated next blob.
                // As on the walker, the selection validates the kind claim.
                let buffer = BlockBuffer::new(
                    std::mem::take(&mut decompress_buf),
                    None,
                    &mut st_scratch,
                    &mut gr_scratch,
                    index.as_ref().map(|idx| idx.kind),
                );
                output_blocks.clear();
                let outcome =
                    process_block(buffer, &mut bb, &mut output_blocks, &selection, coords)
                        .map_err(locate)?;
                crate::commands::split::fit_blocks(&mut output_blocks, &limits).map_err(locate)?;
                for block in output_blocks.drain(..) {
                    w.write_primitive_block_owned(block)?;
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
            _ => frames.skip_payload()?,
        }
    }

    let mut writer = writer.ok_or_else(|| crate::error::new_missing_header(input.path()))?;
    host.invocation
        .counter("walk_actual_osmdata_blobs", osmdata_blobs);
    if blobs_skipped > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "getid.blobs_skipped",
            "{blobs_skipped} blobs skipped by ID range filter"
        );
    }
    writer.flush()?;
    host.invocation.marker("GETID_SCAN_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Two-pass getid with --add-referenced
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn getid_with_refs(
    input: &PinnedInput,
    output: &crate::output::BoundOutput,
    ids: &ElementIds,
    add: &AddReferenced,
    report_found: bool,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<GetidStats> {
    let mut stats = GetidStats {
        nodes_written: 0,
        passthrough_claimed: 0,
        ways_written: 0,
        relations_written: 0,
        found: report_found.then(ElementIds::empty),
    };

    // Pass 1: follow the references, as osmium's `getid -r` does
    // (`find_referenced_objects`): the members of requested relations,
    // relations in relations closed transitively, then the nodes of every
    // way requested or pulled in. Everything pulled in lands in `pulled`,
    // which pass 2 selects from beside `ids`.
    //
    // One header walk sorts the blobs by kind, keeps each one's claim and
    // serves every phase: the reference phases and pass 2 each read only
    // the blobs whose claimed ID range holds an ID they need, as the include
    // arms do (the claim is believed, reference/indexdata-trust.md). The
    // exception is the first relation read, which needs every relation's
    // relation members and so reads every relation blob, holding each to
    // its exact claim, because the later reads select relation blobs by it.
    // A blob is selected by its own frame's claim, so every frame holding a
    // repeated ID is read. Unindexed input (only on a forced run) is read
    // whole by every phase, and pass 2 reads its one file-order schedule
    // once. A partially indexed file is refused by the walk as on every
    // getid path (see `getid_dispatched`); a blob whose claim its contents
    // contradict is refused when a phase decodes it. Only a request naming
    // a way or a relation comes here: nodes alone pull nothing in, and that
    // request is plain include.
    host.invocation.marker("GETID_PASS1_START");
    let mut pulled = ElementIds::empty();
    let (schedules, indexes, shared_file) =
        crate::scan::classify::build_classify_schedules_split_with_claims(input, host)?;
    let indexed = matches!(
        schedules,
        crate::scan::classify::KindSchedules::Indexed { .. }
    );
    let (node_schedule, way_schedule, relation_schedule) = schedules.into_replicated();
    fn claims(
        indexed: bool,
        of: &[crate::blob_meta::BlobIndex],
    ) -> Option<&[crate::blob_meta::BlobIndex]> {
        indexed.then_some(of)
    }

    if ids.relation_ids.has_any() {
        host.invocation.marker("GETID_RELATIONS_START");
        close_relations(
            &shared_file,
            &relation_schedule,
            claims(indexed, &indexes.relations),
            ids,
            &mut pulled,
            host,
        )?;
        host.invocation.marker("GETID_RELATIONS_END");
        crate::debug::emit_mallinfo2(&host.invocation, "getid_after_relations");
    }

    // The way phase reads the requested ways and the pulled-in ones
    // alike, so the requested ones join the pulled set: selection and
    // tag stripping test `ids` first, so the overlap changes neither.
    pulled.way_ids.merge_from(&ids.way_ids);
    if pulled.way_ids.has_any() {
        host.invocation.marker("GETID_WAYS_START");
        let selected = in_claimed_range(
            &way_schedule,
            claims(indexed, &indexes.ways),
            &pulled.way_ids,
            None,
        )?;
        host.invocation
            .counter("getid_way_phase_blobs", selected.len());
        collect_way_nodes(&shared_file, &selected, &mut pulled, host)?;
        host.invocation.marker("GETID_WAYS_END");
        crate::debug::emit_mallinfo2(&host.invocation, "getid_after_ways");
    }
    host.invocation.marker("GETID_PASS1_END");
    // Cheap one-time counts; give the pulled-set sizes before pass 2
    // starts so the counters survive a SIGKILL during pass 2. The way and
    // relation counts include requested IDs where the reference walk
    // reached them.
    host.invocation
        .counter("getid_dep_node_ids", pulled.node_ids.len());
    host.invocation
        .counter("getid_dep_way_ids", pulled.way_ids.len());
    host.invocation
        .counter("getid_dep_relation_ids", pulled.relation_ids.len());

    // Pass 2: write the requested and pulled-in elements, in file order,
    // from the blobs whose claimed range holds one of them. An unindexed
    // file's blobs are read once each.
    host.invocation.marker("GETID_PASS2_START");
    let schedule = if indexed {
        let mut schedule = in_claimed_range(
            &node_schedule,
            Some(&indexes.nodes),
            &ids.node_ids,
            Some(&pulled.node_ids),
        )?;
        schedule.extend(in_claimed_range(
            &way_schedule,
            Some(&indexes.ways),
            &ids.way_ids,
            Some(&pulled.way_ids),
        )?);
        schedule.extend(in_claimed_range(
            &relation_schedule,
            Some(&indexes.relations),
            &ids.relation_ids,
            Some(&pulled.relation_ids),
        )?);
        schedule.sort_unstable_by_key(ScheduledBlob::frame_start);
        schedule
    } else {
        // Replicated, so each kind's schedule is the whole file in order.
        node_schedule
    };
    drop((way_schedule, relation_schedule));
    host.invocation.counter("getid_pass2_blobs", schedule.len());
    // The header, from a front read of the pinned input.
    let header = ElementReader::open_in(input, write.direct_reads, host)?
        .header()
        .clone();
    let coords = crate::owned::WayCoords::of_input(&header);
    let mut writer = writer_from_header(
        output,
        write.compression,
        &header,
        &OutputFacts::carry(coords.rewritten()),
        overrides,
        write.backend,
        host,
    )?;

    let selection = IdSelection {
        ids,
        pulled: (!pulled.is_empty()).then_some(&pulled),
        include_matches: true,
        strip_tags: add.remove_tags,
        report_found,
    };
    let limits = crate::commands::split::Limits::new(writer.compression());
    crate::scan::classify::parallel_project_phase_ordered(
        &shared_file,
        &schedule,
        None,
        None,
        || (),
        |buffer, _| {
            let mut bb = None;
            let mut output = Vec::new();
            let outcome = process_block(buffer, &mut bb, &mut output, &selection, coords)?;
            crate::commands::split::fit_blocks(&mut output, &limits)?;
            Ok((output, outcome))
        },
        |_position, (blocks, outcome)| {
            for block in blocks {
                writer.write_primitive_block_owned(block)?;
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
        host,
    )?;

    writer.flush()?;
    host.invocation.marker("GETID_PASS2_END");
    Ok(stats)
}

/// Close the requested relations over their members into `pulled`: node
/// and way members of every relation reached, and the reached relations
/// themselves (any not requested), recursively through relations in
/// relations.
///
/// At most two reads of the relation blobs, whatever the nesting depth, as
/// osmium's `find_relations_in_relations` and
/// `find_nodes_and_ways_in_relations`. The first collects the requested
/// relations' members and every relation-to-relation edge of the other
/// relations; the closure is then walked in memory from the requested
/// relations' relation members (an explicit worklist, so a deep chain
/// cannot exhaust the stack, and a relation is expanded once, so a cycle
/// ends). The second runs only when the walk reached a relation that was
/// not requested, and collects those relations' node and way members. A
/// requested relation reached by the walk is not expanded again: the first
/// read already took all its members. The parent test reads only `ids`,
/// which nobody mutates, so which edges are kept does not depend on the
/// order blobs complete in. A reached relation joins `pulled` whether or
/// not the file holds it; pass 2 writes what exists.
///
/// The edge store holds one pair per relation member of a relation, file
/// wide, which is the price of the two-read bound (a fixed-point loop over
/// the relation blobs stores nothing but reads them once per nesting
/// level). It lives in its own anonymous mapping ([`EdgeStore`], which says
/// why), released as soon as the closure is walked. The per-blob edge
/// vectors the workers hand over are still `Vec`s, each bounded by one
/// blob.
///
/// Every member is followed by its exact signed ID, negative ones
/// included; osmium abs-folds a negative member (`positive_ref`), so for
/// a reference to `-N` it adds object `N`.
///
/// With `claims` (an indexed file, one claim per schedule entry), the first
/// read holds every relation blob to its exact claim and the second reads
/// only the blobs whose claimed range holds a reached relation.
fn close_relations(
    shared_file: &InputFile,
    schedule: &[ScheduledBlob],
    claims: Option<&[crate::blob_meta::BlobIndex]>,
    ids: &ElementIds,
    pulled: &mut ElementIds,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let mut seeds: Vec<i64> = Vec::new();
    let mut edges = EdgeStore::default();
    let mut store_failed: Option<Error> = None;
    collect_relation_members(
        shared_file,
        schedule,
        claims,
        &ids.relation_ids,
        true,
        |refs| {
            insert_all(&mut pulled.node_ids, refs.nodes);
            insert_all(&mut pulled.way_ids, refs.ways);
            seeds.extend(refs.relations);
            if store_failed.is_none()
                && let Err(e) = edges.extend(&refs.edges)
            {
                store_failed = Some(e);
            }
        },
        host,
    )?;
    if let Some(e) = store_failed {
        return Err(e);
    }

    host.invocation.counter("getid_relation_edges", edges.len());
    host.invocation
        .counter("getid_relation_edge_bytes", edges.mapped_bytes());
    let edges = edges.sorted();
    let mut added = false;
    let mut worklist = seeds;
    while let Some(id) = worklist.pop() {
        if ids.relation_ids.get(id) || !pulled.relation_ids.insert_if_new(id) {
            continue;
        }
        added = true;
        worklist.extend(edges.children(id));
    }
    drop(edges);

    // Two relation reads, never a frontier traversal that rereads only the
    // blobs each nesting level reaches: the relation reads take under
    // 0.3 s at planet, a small share of the command, and the edge store
    // already bounds them at two whatever the depth, so a per-level
    // traversal was refused.
    if added {
        let reached = in_claimed_range(schedule, claims, &pulled.relation_ids, None)?;
        host.invocation
            .counter("getid_reached_relation_blobs", reached.len());
        collect_relation_members(
            shared_file,
            &reached,
            None,
            &pulled.relation_ids,
            false,
            |refs| {
                insert_all(&mut pulled.node_ids, refs.nodes);
                insert_all(&mut pulled.way_ids, refs.ways);
            },
            host,
        )?;
    }
    Ok(())
}

/// Collect relation members and closure edges through the scoped validator.
fn collect_relation_members(
    shared_file: &InputFile,
    schedule: &[ScheduledBlob],
    claims: Option<&[crate::blob_meta::BlobIndex]>,
    collect: &IdSet,
    seed_edges: bool,
    mut merge: impl FnMut(RelationRefs),
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    if let Some(claims) = claims
        && claims.len() != schedule.len()
    {
        return Err(Error::internal(
            "relation phase: claims do not match the schedule entry for entry",
        ));
    }
    crate::scan::classify::parallel_project_phase(
        shared_file,
        schedule,
        None,
        Vec::new,
        |position, buffer, relations| -> Result<RelationRefs> {
            let (RelationSpans { .. }, accepted) =
                buffer.project_ids_and_refs(|_, _| RelationSpans::new(&mut *relations))?;
            if let Some(claim) = claims.and_then(|c| c.get(position)) {
                let mut walk =
                    crate::blob_meta::ClaimWalk::new(Some(claim), crate::blob_meta::Order::Free);
                for relation in relations.iter() {
                    walk.observe(relation.id)?;
                }
                walk.finish()?;
            }
            let mut refs = RelationRefs::default();
            for relation in relations.iter() {
                let collected = collect.get(relation.id);
                if !collected && !seed_edges {
                    continue;
                }
                for member in relation.members(&accepted)? {
                    match member {
                        MemberId::Node(id) if collected => refs.nodes.push(id),
                        MemberId::Way(id) if collected => refs.ways.push(id),
                        MemberId::Relation(id) if seed_edges => {
                            if collected {
                                refs.relations.push(id);
                            } else {
                                refs.edges.push((relation.id, id));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(refs)
        },
        |_seq, refs| merge(refs),
        host,
    )
}

/// The entries of `schedule` whose claimed ID range holds an ID of `set`
/// or of `also`, in schedule order; every entry when `claims` is `None` (an
/// unindexed file, whose blobs claim nothing). `claims` holds one claim per
/// entry. Each entry is tested on its own claim, so overlapping ranges and
/// an ID repeated across blobs select every blob that may hold it.
fn in_claimed_range(
    schedule: &[ScheduledBlob],
    claims: Option<&[crate::blob_meta::BlobIndex]>,
    set: &IdSet,
    also: Option<&IdSet>,
) -> Result<Vec<ScheduledBlob>> {
    let Some(claims) = claims else {
        return Ok(schedule.to_vec());
    };
    if claims.len() != schedule.len() {
        return Err(Error::internal(
            "getid: blob claims do not match the schedule entry for entry",
        ));
    }
    Ok(schedule
        .iter()
        .zip(claims)
        .filter(|(_, claim)| {
            set.any_in_numeric_range(claim.min_id, claim.max_id)
                || also.is_some_and(|a| a.any_in_numeric_range(claim.min_id, claim.max_id))
        })
        .map(|(entry, _)| *entry)
        .collect())
}

/// Add the node refs of every way in `pulled.way_ids` to `pulled.node_ids`.
///
/// Each blob projects `WaySpans` for those ways. After acceptance, their
/// refs are decoded, sorted and deduplicated into a
/// per-blob vector, which the consumer inserts into the one node set. A
/// per-worker set would hold up to one node-range bitmap per worker on a
/// broad query (`-I` naming every way); the per-blob vector is bounded by
/// one blob's refs and empty for almost every blob of a narrow one.
fn collect_way_nodes(
    shared_file: &InputFile,
    schedule: &[ScheduledBlob],
    pulled: &mut ElementIds,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let ElementIds {
        node_ids, way_ids, ..
    } = pulled;
    let way_ids = &*way_ids;
    crate::scan::classify::parallel_project_phase(
        shared_file,
        schedule,
        None,
        Vec::new,
        |_, buffer, ways| -> Result<Vec<i64>> {
            let (WaySpans { .. }, accepted) =
                buffer.project_ids_and_refs(|_, _| WaySpans::new(Some(way_ids), &mut *ways))?;
            let mut refs: Vec<i64> = Vec::new();
            for way in ways.iter() {
                refs.extend(way.refs(&accepted)?);
            }
            refs.sort_unstable();
            refs.dedup();
            Ok(refs)
        },
        |_seq, refs| {
            insert_all(node_ids, refs);
        },
        host,
    )
}

struct IdSelection<'a> {
    ids: &'a ElementIds,
    pulled: Option<&'a ElementIds>,
    include_matches: bool,
    strip_tags: bool,
    report_found: bool,
}

impl IdSelection<'_> {
    fn select(&self, kind: ElementKind, id: i64) -> Selection {
        let requested = self.ids.of(kind).get(id);
        let selected = requested || self.pulled.is_some_and(|p| p.of(kind).get(id));
        if selected == self.include_matches {
            Selection::retain(!self.strip_tags || requested)
        } else {
            Selection::Discard
        }
    }

    fn may_select_nodes(&self) -> bool {
        !self.include_matches
            || self.ids.node_ids.has_any()
            || self.pulled.is_some_and(|p| p.node_ids.has_any())
    }
}

/// An element's kind and ID.
pub(super) fn element_key(element: &Element<'_>) -> (ElementKind, i64) {
    match element {
        Element::DenseNode(dn) => (ElementKind::Node, dn.id()),
        Element::Node(n) => (ElementKind::Node, n.id()),
        Element::Way(w) => (ElementKind::Way, w.id()),
        Element::Relation(r) => (ElementKind::Relation, r.id()),
    }
}

/// Re-encode the selection in source order. The builder is reused on the
/// serial arms and allocated lazily on workers, where most blobs have no hit.
fn process_block(
    buffer: BlockBuffer<'_>,
    bb: &mut Option<BlockBuilder>,
    output: &mut Vec<OwnedBlock>,
    selection: &IdSelection<'_>,
    coords: crate::owned::WayCoords,
) -> Result<BlockOutcome> {
    let mut outcome = BlockOutcome::default();
    let block = buffer.for_selection()?;
    let mut encoder = SelectedEncoder::new(coords);
    block.visit_matching(
        selection.may_select_nodes(),
        |kind, id| selection.select(kind, id),
        None::<&fn(SelectionRefs<'_>) -> bool>,
        |element, keep_tags| {
            let (kind, id) = element_key(&element);
            if selection.report_found && selection.include_matches && selection.ids.of(kind).get(id)
            {
                outcome.found.push((kind, id));
            }
            encoder.write(
                bb.get_or_insert_with(BlockBuilder::new),
                output,
                &element,
                keep_tags,
            )?;
            match kind {
                ElementKind::Node => outcome.nodes += 1,
                ElementKind::Way => outcome.ways += 1,
                ElementKind::Relation => outcome.relations += 1,
            }
            Ok(())
        },
    )?;
    if let Some(bb) = bb {
        flush_local(bb, output)?;
    }
    Ok(outcome)
}

/// Reusable re-encoding scratch for the ID and parent selectors.
pub(super) struct SelectedEncoder<'a> {
    coords: crate::owned::WayCoords,
    way: crate::owned::WayScratch,
    members: Vec<MemberData<'a>>,
}

impl<'a> SelectedEncoder<'a> {
    pub(super) fn new(coords: crate::owned::WayCoords) -> Self {
        Self {
            coords,
            way: crate::owned::WayScratch::default(),
            members: Vec::new(),
        }
    }

    pub(super) fn write(
        &mut self,
        bb: &mut BlockBuilder,
        output: &mut Vec<OwnedBlock>,
        element: &Element<'a>,
        keep_tags: bool,
    ) -> Result<()> {
        match element {
            Element::DenseNode(dn) => {
                ensure_node_capacity_local(bb, output)?;
                let meta = dense_node_metadata(dn)?;
                bb.add_node_dense(
                    dn.id(),
                    dn.decimicro_lat()?,
                    dn.decimicro_lon()?,
                    keep_tags.then(|| dn.tags()).into_iter().flatten(),
                    meta.as_ref(),
                );
            }
            Element::Node(n) => {
                ensure_node_capacity_local(bb, output)?;
                let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                bb.add_node_partial(
                    n.id(),
                    n.decimicro_lat()?,
                    n.decimicro_lon()?,
                    keep_tags.then(|| n.tags()).into_iter().flatten(),
                    meta.as_ref(),
                );
            }
            Element::Way(w) => {
                ensure_way_capacity_local(bb, output)?;
                self.way.prepare(w, self.coords)?;
                let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
                self.way.write(
                    bb,
                    keep_tags.then(|| w.tags()).into_iter().flatten(),
                    meta.as_ref(),
                );
            }
            Element::Relation(r) => {
                ensure_relation_capacity_local(bb, output)?;
                r.members_into(&mut self.members)?;
                let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                bb.add_relation_partial(
                    r.id(),
                    keep_tags.then(|| r.tags()).into_iter().flatten(),
                    &self.members,
                    meta.as_ref(),
                );
            }
        }
        Ok(())
    }
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
    use crate::MemberId;
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
        crate::test_support::assert_err_kind!(
            parse_id_spec("x123", None),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("expected prefix"),
        );
    }

    #[test]
    fn parse_missing_number() {
        crate::test_support::assert_err_kind!(
            parse_id_spec("n", None),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("prefix + number"),
        );
    }

    #[test]
    fn parse_bad_number() {
        crate::test_support::assert_err_kind!(
            parse_id_spec("nabc", None),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("is not an integer"),
        );
    }

    #[test]
    fn parse_too_short() {
        crate::test_support::assert_err_kind!(
            parse_id_spec("n", None),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("prefix + number"),
        );
        crate::test_support::assert_err_kind!(
            parse_id_spec("", None),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("prefix + number"),
        );
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
        crate::test_support::assert_err_kind!(
            parse_id_spec("\tn123", None),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("expected prefix"),
        );
        crate::test_support::assert_err_kind!(
            parse_id_spec("n123\t", None),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("is not an integer"),
        );
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
        crate::test_support::assert_err_kind!(
            parse_ids_from_file(&path),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("invalid id file"),
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
        crate::test_support::assert_err_kind!(
            parse_ids_with_default_type(&specs, None),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("expected prefix"),
        );
    }

    #[test]
    fn parse_accepts_every_signed_id() {
        assert_eq!(parse_id_spec("n-1", None).unwrap(), (ElementKind::Node, -1));
        assert_eq!(
            parse_id_spec("w-42", None).unwrap(),
            (ElementKind::Way, -42)
        );
        assert_eq!(
            parse_id_spec("relation/-7", None).unwrap(),
            (ElementKind::Relation, -7)
        );
        assert_eq!(
            parse_id_spec("-5", Some(ElementKind::Node)).unwrap(),
            (ElementKind::Node, -5)
        );
        assert_eq!(
            parse_id_spec("n-9223372036854775808", None).unwrap(),
            (ElementKind::Node, i64::MIN)
        );
        assert_eq!(
            parse_id_spec("w9223372036854775807", None).unwrap(),
            (ElementKind::Way, i64::MAX)
        );
        // One past either extreme is still a bad number.
        crate::test_support::assert_err_kind!(
            parse_id_spec("n-9223372036854775809", None),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("is not an integer"),
        );
        crate::test_support::assert_err_kind!(
            parse_id_spec("-", Some(ElementKind::Node)),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("bad number"),
        );
        // A source holding only negative IDs is not an empty request.
        let ids = parse_ids(&["n-3".to_owned()]).unwrap();
        assert!(!ids.is_empty());
        assert!(ids.node_ids.get(-3) && !ids.node_ids.get(3));
    }

    /// Write one sorted node blob per slice, every blob indexed.
    fn write_node_blobs(path: &Path, blobs: &[&[i64]]) {
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
        for ids in blobs {
            for &id in *ids {
                block.add_node(id, 0, 0, std::iter::empty::<(&str, &str)>(), None);
            }
            writer
                .write_primitive_block(block.take().unwrap().unwrap())
                .unwrap();
        }
        writer.flush().unwrap();
    }

    /// The IDs of `path` in file order, unsorted.
    fn ids_in_file_order(path: &Path) -> Vec<i64> {
        let mut found = Vec::new();
        ElementReader::open_in(path, false, &crate::host::Host::for_tests().entered())
            .unwrap()
            .for_each(|element| found.push(element_key(&element).1))
            .unwrap();
        found
    }

    /// Both include arms select negative IDs exactly, through the signed
    /// numeric range prefilter: in a zero-and-negatives blob, a
    /// negative-only blob, a blob straddling zero and a blob holding both
    /// `i64` extremes (one dense run, so the output's delta wraps). A query
    /// inside a negative-only blob's range that names no element of it, and
    /// one outside every range, select nothing; no magnitude twin leaks in.
    #[test]
    fn include_arms_select_negative_ids_exactly() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_node_blobs(
            &input,
            &[
                &[0, -1, -2],
                &[-5, -7, -9],
                &[-11, i64::MIN, 1, 2],
                &[7, i64::MAX],
            ],
        );
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        assert_eq!(
            ids_from_both_arms(&input, &["n-7", "n2", "n-1"], &host),
            vec![-7, -1, 2]
        );
        assert_eq!(
            ids_from_both_arms(&input, &["n-6"], &host),
            Vec::<i64>::new()
        );
        assert_eq!(
            ids_from_both_arms(&input, &["n-100"], &host),
            Vec::<i64>::new()
        );
        assert_eq!(
            ids_from_both_arms(
                &input,
                &["n-9223372036854775808", "n9223372036854775807"],
                &host
            ),
            vec![i64::MIN, i64::MAX]
        );
        // The emitted order is the input's canonical order.
        let query = parse_ids(&[
            "n9223372036854775807".to_owned(),
            "n-9223372036854775808".to_owned(),
            "n-11".to_owned(),
            "n0".to_owned(),
        ])
        .unwrap();
        let output = dir.path().join("ordered.pbf");
        let staged = crate::output::Output::new(
            &output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .unwrap()
        .bind(&host.invocation)
        .unwrap();
        filter_by_id(
            &PinnedInput::pin(&input, &host).unwrap(),
            &staged,
            &query,
            super::Filter::Include(ScanArm::Walker),
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            false,
            &host,
        )
        .unwrap();
        staged.publish().unwrap();
        assert_eq!(ids_in_file_order(&output), vec![0, -11, i64::MIN, i64::MAX]);
    }

    /// `--add-referenced` follows a closure through negative relations:
    /// `r9 -> r-1 -> r-2 -> r9` (a cycle), where `r-1` and `r-2` are not
    /// requested, so the walk reaches `r-2` only through an edge whose
    /// parent is negative, and `n-4` and `w-3` (with its node `n-6`) are
    /// reachable only through `r-2`; `n4` comes in as `w-3`'s second ref.
    /// The magnitude twins `w3`, `n6` and `r2` are present and must stay
    /// out.
    #[test]
    fn add_referenced_follows_negative_relations_and_refs() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("closure.pbf");
        let file = std::fs::File::create(&input).unwrap();
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
        for id in [-4, -6, 4, 6] {
            block.add_node(id, 0, 0, std::iter::empty::<(&str, &str)>(), None);
        }
        writer
            .write_primitive_block(block.take().unwrap().unwrap())
            .unwrap();
        block.add_way(-3, std::iter::empty::<(&str, &str)>(), &[-6, 4], None);
        block.add_way(3, std::iter::empty::<(&str, &str)>(), &[6], None);
        writer
            .write_primitive_block(block.take().unwrap().unwrap())
            .unwrap();
        let member = |id| MemberData { id, role: "" };
        let empty = std::iter::empty::<(&str, &str)>;
        block.add_relation(-1, empty(), &[member(MemberId::Relation(-2))], None);
        block.add_relation(
            -2,
            empty(),
            &[
                member(MemberId::Node(-4)),
                member(MemberId::Way(-3)),
                member(MemberId::Relation(9)),
            ],
            None,
        );
        block.add_relation(2, empty(), &[member(MemberId::Node(6))], None);
        block.add_relation(9, empty(), &[member(MemberId::Relation(-1))], None);
        writer
            .write_primitive_block(block.take().unwrap().unwrap())
            .unwrap();
        writer.flush().unwrap();

        let ids = parse_ids(&["r9".to_owned()]).unwrap();
        let output = dir.path().join("output.pbf");
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let staged = crate::output::Output::new(
            &output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .unwrap()
        .bind(&host.invocation)
        .unwrap();
        getid_with_refs(
            &PinnedInput::pin(&input, &host).unwrap(),
            &staged,
            &ids,
            &AddReferenced { remove_tags: false },
            false,
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            &host,
        )
        .unwrap();
        staged.publish().unwrap();
        // Nodes -4, -6, 4 (w-3's second ref), way -3, relations -1, -2, 9.
        assert_eq!(ids_in_file_order(&output), vec![-4, -6, 4, -3, -1, -2, 9]);
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

    fn read_ids(path: &Path, host: &crate::host::Entered<'_>) -> Vec<i64> {
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
        let add = AddReferenced { remove_tags: false };
        let output = dir.path().join("output.pbf");
        let staged = crate::output::Output::new(
            &output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .unwrap();
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let staged = staged.bind(&host.invocation).unwrap();
        getid_with_refs(
            &PinnedInput::pin(&input, &host).unwrap(),
            &staged,
            &ids,
            &add,
            false,
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            &host,
        )
        .unwrap();
        staged.publish().unwrap();
        assert_eq!(
            read_ids(&output, &crate::host::Host::for_tests().entered()),
            vec![1, 2, 10]
        );
    }

    /// A file indexed on its node blob and not on its way blob is refused by
    /// the two walks that run before any output is written:
    /// `--add-referenced`'s pass 1 schedule walk (a way ID requested) and the `-I`
    /// reader, each with the typed partial missing-indexdata error located
    /// at the way blob (ordinal 2: the header is 0, the node blob 1). The
    /// paths that refuse mid-write (both include arms, the invert walker,
    /// pass 2) are pinned by the CLI input-contract table, in both orders:
    /// this crate's test builds assert on a writer dropped unflushed, which
    /// is what an error return mid-write does.
    #[test]
    fn partially_indexed_input_is_refused_before_any_output() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("partial.pbf");
        write_blocks(&input, &[1, 2, 3], Some((10, &[1, 2])), false);
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let pin = PinnedInput::pin(&input, &host).unwrap();
        let refused = |label: &str, result: Result<()>| {
            let err = result.expect_err(label);
            assert!(
                matches!(
                    err.kind(),
                    crate::error::ErrorKind::MissingIndexdata { partial: true, .. }
                ),
                "{label}: {err:?}"
            );
            assert_eq!(err.blob_index(), Some(2), "{label}: {err:?}");
        };
        let staged = || {
            crate::output::Output::new(
                dir.path().join("output.pbf"),
                crate::output::Overwrite::Replace,
                &crate::host::Diagnostics::discard(),
            )
            .unwrap()
            .bind(&host.invocation)
            .unwrap()
        };
        let ids = parse_ids(&["w10".to_owned()]).unwrap();
        let add = AddReferenced { remove_tags: false };
        refused(
            "add-referenced pass 1",
            getid_with_refs(
                &pin,
                &staged(),
                &ids,
                &add,
                false,
                &WriteOptions::default(),
                &HeaderOverrides::default(),
                &host,
            )
            .map(drop),
        );
        refused("-I", parse_ids_from_pbf(&input, false, &host).map(drop));
    }

    /// Run the same include-mode query under both arms, assert they emit
    /// identical element sets, and return that common (sorted) ID list.
    fn ids_from_both_arms(
        input: &Path,
        query: &[&str],
        host: &crate::host::Entered<'_>,
    ) -> Vec<i64> {
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
            let staged = staged.bind(&host.invocation).unwrap();
            filter_by_id(
                &PinnedInput::pin(input, host).unwrap(),
                &staged,
                &query,
                super::Filter::Include(arm),
                &WriteOptions::default(),
                &HeaderOverrides::default(),
                false,
                host,
            )
            .unwrap();
            staged.publish().unwrap();
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
            ids_from_both_arms(
                &input,
                &["n1", "w10"],
                &crate::host::Host::for_tests().entered()
            ),
            vec![1, 10]
        );
    }

    #[test]
    fn include_arms_skip_rejected_payloads_before_a_match() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], Some((10, &[1, 2])), true);
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        // Reject the node blob by kind, then by ID range while still asking
        // for nodes. In both cases the following way payload must be read.
        for query in [&["w10"][..], &["n999", "w10"][..]] {
            assert_eq!(ids_from_both_arms(&input, query, &host), vec![10]);
        }
        // Also skip the final payload after reading a matching node blob.
        assert_eq!(ids_from_both_arms(&input, &["n1"], &host), vec![1]);
    }

    #[test]
    fn include_arms_agree_on_empty_query_result() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], Some((10, &[1, 2])), true);
        assert!(
            ids_from_both_arms(&input, &["n999"], &crate::host::Host::for_tests().entered())
                .is_empty()
        );
    }

    #[test]
    fn include_arms_agree_on_single_data_blob_file() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], None, true);
        assert_eq!(
            ids_from_both_arms(&input, &["n2"], &crate::host::Host::for_tests().entered()),
            vec![2]
        );
    }

    /// The streaming full-scan arm reads only the pinned input. It needs an
    /// estimate past `FULL_SCAN_ARM_MIN_BLOBS` to dispatch, which no fixture
    /// reaches through the public `getid`, so this drives `getid_dispatched`
    /// with the threshold at one; `crates/lib/tests/fault_getid_input.rs` covers every
    /// other route. Three opens: the probe, the estimate walk and the
    /// streaming reader. The replacement is armed after each in turn, in
    /// both reopen modes, buffered and `O_DIRECT`.
    #[cfg(pbfhogg_test_hooks)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn full_scan_arm_reads_only_the_pinned_input() {
        use std::sync::atomic::Ordering;
        const OPENS: u64 = 3;
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("in.pbf");
        let replacement = dir.path().join("replacement.pbf");
        let query = parse_ids(&["n1".to_owned(), "w10".to_owned()]).unwrap();
        let opts = GetidOptions {
            add_referenced: None,
            report_found: false,
        };
        let run = |output: &Path, host: &crate::host::Host, direct: bool| {
            let host = &host.enter()?;
            let staged = crate::output::Output::new(
                output,
                crate::output::Overwrite::Replace,
                &crate::host::Diagnostics::discard(),
            )
            .unwrap();
            let write = WriteOptions {
                direct_reads: direct,
                ..WriteOptions::default()
            };
            let staged = staged.bind(&host.invocation).unwrap();
            let result = getid_dispatched(
                &input,
                &staged,
                &query,
                &opts,
                &write,
                &HeaderOverrides::default(),
                1,
                host,
            );
            if result.is_ok() {
                staged.publish().unwrap();
            }
            result
        };
        // The getid reach row makes the probe and streaming fallback
        // direct, with the intervening estimate walk buffered.
        let witness = |report: &crate::test_hooks::InputReplacement,
                       host: &crate::host::Host,
                       direct: bool,
                       cell: &str| {
            use crate::test_hooks::Transport::{Buffered, Direct};
            let expected = if direct {
                [Direct, Buffered, Direct]
            } else {
                [Buffered; 3]
            };
            assert_eq!(report.transports, expected, "{cell}: transports");
            let mut direct_opens = None;
            host.invocation.for_each_counter(|name, value| {
                if name == "direct_io_read_opens" {
                    direct_opens = Some(value);
                }
            });
            assert_eq!(
                direct_opens,
                Some(if direct { 2 } else { 0 }),
                "{cell}: O_DIRECT read opens"
            );
        };
        let mut directs = vec![false];
        if cfg!(feature = "linux-direct-io") {
            directs.push(true);
        }
        for direct in directs {
            write_blocks(&input, &[1, 2], Some((10, &[1, 2])), true);
            let reference = dir.path().join("reference.pbf");
            let host_owner = crate::host::Host::for_tests();
            let host = host_owner.entered();
            host.invocation
                .faults()
                .arm_input_replacement(&input, &replacement, OPENS + 1);
            match run(&reference, &host, direct) {
                Ok((arm, _)) => assert_eq!(arm, Some(ScanArm::FullScan), "direct {direct}"),
                #[cfg(feature = "linux-direct-io")]
                Err(e) if direct && crate::test_support::is_direct_io_refusal(&e) => {
                    crate::test_support::skip(format!(
                        "the direct transport: O_DIRECT not supported: {e}"
                    ));
                    continue;
                }
                Err(e) => panic!("direct {direct}: unreplaced run: {e}"),
            }
            let counted = host.invocation.faults().input_replacement().unwrap();
            assert_eq!(
                (counted.attempts, counted.verified, counted.fired),
                (OPENS, OPENS, false),
                "direct {direct}: {counted:?}"
            );
            witness(&counted, &host, direct, "unreplaced run");
            let expected = std::fs::read(&reference).unwrap();
            assert_eq!(read_ids(&reference, &host), vec![1, 10]);

            for after in 1..=OPENS {
                for force_path in [true, false] {
                    write_blocks(&input, &[1, 2], Some((10, &[1, 2])), true);
                    // Other IDs entirely: a run that read any of it would
                    // write different bytes.
                    write_blocks(&replacement, &[1_001, 1_002], Some((1_010, &[1_001])), true);
                    let host_owner = crate::host::Host::for_tests();
                    let host = host_owner.entered();
                    host.invocation
                        .faults()
                        .force_pin_path_reopen
                        .store(force_path, Ordering::Relaxed);
                    let procfs = PinnedInput::pin(&input, &host)
                        .unwrap()
                        .reopens_through_procfs();
                    host.invocation
                        .faults()
                        .arm_input_replacement(&input, &replacement, after);
                    let output = dir.path().join("out.pbf");
                    let result = run(&output, &host, direct);
                    let cell = format!("direct {direct}, after open {after}, procfs {procfs}");
                    #[cfg(feature = "linux-direct-io")]
                    if let Err(e) = &result
                        && direct
                        && crate::test_support::is_direct_io_refusal(e)
                    {
                        crate::test_support::skip(format!("{cell}: O_DIRECT not supported: {e}"));
                        continue;
                    }
                    let report = host.invocation.faults().input_replacement().unwrap();
                    assert!(report.fired, "{cell}: {report:?}");
                    if procfs || after == OPENS {
                        let (arm, _) = result.unwrap_or_else(|e| panic!("{cell}: {e}"));
                        assert_eq!(arm, Some(ScanArm::FullScan), "{cell}");
                        assert_eq!(report.attempts, OPENS, "{cell}: {report:?}");
                        assert_eq!(report.verified, OPENS, "{cell}: {report:?}");
                        witness(&report, &host, direct, &cell);
                        assert_eq!(report.refused, 0, "{cell}: {report:?}");
                        assert_eq!(std::fs::read(&output).unwrap(), expected, "{cell}");
                        std::fs::remove_file(&output).unwrap();
                    } else if cfg!(unix) {
                        let err = result
                            .err()
                            .unwrap_or_else(|| panic!("{cell}: must refuse"));
                        assert!(
                            matches!(err.kind(), crate::ErrorKind::InputReplaced),
                            "{cell}: {err:?}"
                        );
                        assert_eq!(
                            (report.attempts, report.refused),
                            (after + 1, 1),
                            "{cell}: {report:?}"
                        );
                        assert!(!output.exists(), "{cell}");
                    }
                }
            }
        }
    }

    #[test]
    fn auto_dispatch_crosses_arms_at_the_injected_threshold() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], Some((10, &[1, 2])), true);
        let query = parse_ids(&["n1".to_owned(), "w10".to_owned()]).unwrap();
        let opts = GetidOptions {
            add_referenced: None,
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
            let host_owner = crate::host::Host::for_tests();
            let host = host_owner.entered();
            let staged = staged.bind(&host.invocation).unwrap();
            let (arm, _) = getid_dispatched(
                &input,
                &staged,
                &query,
                &opts,
                &WriteOptions::default(),
                &HeaderOverrides::default(),
                min_blobs,
                &host,
            )
            .unwrap();
            staged.publish().unwrap();
            assert_eq!(arm, Some(expected_arm));
            assert_eq!(
                read_ids(&output, &crate::host::Host::for_tests().entered()),
                vec![1, 10]
            );
        }
    }

    fn run_block(
        bytes: Vec<u8>,
        selection: &IdSelection<'_>,
        coords: crate::owned::WayCoords,
    ) -> Result<(Vec<Vec<u8>>, BlockOutcome)> {
        let (mut strings, mut groups) = (Vec::new(), Vec::new());
        let buffer = BlockBuffer::new(bytes, None, &mut strings, &mut groups, None);
        let mut output = Vec::new();
        let outcome = process_block(buffer, &mut None, &mut output, selection, coords)?;
        Ok((output.into_iter().map(|b| b.bytes).collect(), outcome))
    }

    fn id_selection(ids: &ElementIds) -> IdSelection<'_> {
        IdSelection {
            ids,
            pulled: None,
            include_matches: true,
            strip_tags: false,
            report_found: true,
        }
    }

    #[test]
    fn scoped_getid_matches_the_full_element_walk() {
        use crate::read::wire_fixtures::{mutations, projection_fixtures};

        let queries = [
            &["n1"][..],
            &["n10", "n99", "w20", "r30"],
            &["w21"],
            &["r51"],
            &["n12345"],
        ]
        .map(|query| {
            parse_ids(&query.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>()).unwrap()
        });
        let deps = parse_ids(&["n3", "n9", "n11", "w20", "r31"].map(str::to_owned)).unwrap();
        let coords = crate::owned::WayCoords::Drop;
        for (what, bytes) in projection_fixtures() {
            for input in
                std::iter::once(bytes.clone()).chain(mutations(&bytes).into_iter().map(|(_, b)| b))
            {
                let Ok(block) = crate::PrimitiveBlock::from_vec(input.clone()) else {
                    continue;
                };
                for ids in &queries {
                    for (include_matches, pulled, strip_tags) in [
                        (true, None, false),
                        (false, None, false),
                        (true, Some(&deps), false),
                        (true, Some(&deps), true),
                    ] {
                        let selection = IdSelection {
                            include_matches,
                            pulled,
                            strip_tags,
                            ..id_selection(ids)
                        };
                        let mut bb = BlockBuilder::new();
                        let mut encoder = SelectedEncoder::new(coords);
                        let mut expected = Vec::new();
                        let mut counts = [0; 3];
                        let mut found = Vec::new();
                        let oracle = (|| -> Result<()> {
                            for element in block.elements() {
                                let (kind, id) = element_key(&element);
                                let requested = ids.of(kind).get(id);
                                let dependent = pulled.is_some_and(|deps| deps.of(kind).get(id));
                                if include_matches == (requested || dependent) {
                                    if include_matches && requested {
                                        found.push((kind, id));
                                    }
                                    counts[usize::from(kind.ordinal())] += 1;
                                    encoder.write(
                                        &mut bb,
                                        &mut expected,
                                        &element,
                                        !strip_tags || requested,
                                    )?;
                                }
                            }
                            flush_local(&mut bb, &mut expected)
                        })();
                        match (oracle, run_block(input.clone(), &selection, coords)) {
                            (Ok(()), Ok((output, stats))) => {
                                assert_eq!(
                                    output,
                                    expected.into_iter().map(|b| b.bytes).collect::<Vec<_>>(),
                                    "{what}"
                                );
                                assert_eq!(
                                    [stats.nodes, stats.ways, stats.relations],
                                    counts,
                                    "{what}"
                                );
                                assert_eq!(stats.found, found, "{what}");
                            }
                            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{what}"),
                            (a, b) => panic!(
                                "{what}: full accepted {}, scoped accepted {}",
                                a.is_ok(),
                                b.is_ok()
                            ),
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn getid_checks_retained_output_fields() {
        use crate::owned::WayCoords;
        use crate::read::wire_fixtures::{
            Group, block, dense, field_bytes, plain_node, relation, way,
        };

        let ids = parse_ids(&["n1", "w2", "r3"].map(str::to_owned)).unwrap();
        let selection = id_selection(&ids);
        let node = |record| Group {
            nodes: vec![record],
            ..Group::default()
        };
        let dense_node = |record| Group {
            dense: Some(record),
            ..Group::default()
        };
        let plain_info = |info: &[u8]| {
            let mut record = plain_node(1, 0, 0, &[], None);
            field_bytes(&mut record, 4, info);
            node(record)
        };
        let dense_info = |info: &[u8]| {
            let mut record = dense(&[(1, 0, 0), (2, 0, 0)], &[]);
            field_bytes(&mut record, 5, info);
            dense_node(record)
        };
        let mut locations = way(2, &[1], (1, 2));
        field_bytes(
            &mut locations,
            9,
            &crate::read::wire_fixtures::deltas(&[i64::MAX]),
        );
        field_bytes(&mut locations, 10, &[0]);
        for group in [
            node(plain_node(1, 0, 0, &[(1, 127)], None)),
            node(plain_node(1, i64::MAX, 0, &[], None)),
            plain_info(&[0x08]),
            plain_info(&[0x28, 127]),
            dense_node(dense(&[(1, 0, 0)], &[1, 127, 0])),
            dense_node(dense(&[(1, i64::MAX, 0)], &[])),
            dense_info(&[0x0a, 1, 1]), // version count shorter than the ID column
            dense_info(&[0x2a, 3, 254, 1]), // truncated inner framing
            dense_info(&[0x2a, 3, 254, 1, 0]), // user_sid delta of 127
            Group {
                ways: vec![locations],
                ..Group::default()
            },
            Group {
                relations: vec![relation(3, &[(127, 1, 0)])],
                ..Group::default()
            },
        ] {
            let input = block(&["", "k", "v"], &[group]);
            let full = crate::PrimitiveBlock::from_vec(input.clone())
                .and_then(|block| {
                    let mut encoder = SelectedEncoder::new(WayCoords::Keep);
                    for element in block.elements().filter(|el| {
                        let (kind, id) = element_key(el);
                        ids.of(kind).get(id)
                    }) {
                        encoder.write(&mut BlockBuilder::new(), &mut Vec::new(), &element, true)?;
                    }
                    Ok(())
                })
                .expect_err("full writer must refuse invalid retained data");
            let scoped = run_block(input, &selection, WayCoords::Keep)
                .map(drop)
                .expect_err("scoped writer must refuse invalid retained data");
            assert_eq!(scoped.to_string(), full.to_string());
        }
    }

    #[test]
    fn getid_discards_unread_fields_and_strips_only_dependency_tags() {
        use crate::owned::WayCoords;
        use crate::read::wire_fixtures::{Group, block, dense, field_bytes, plain_node, way};

        let input = |stripped_tag, discarded_tag, discarded_info: &[u8]| {
            let mut discarded = way(10, &[1], (1, discarded_tag));
            field_bytes(&mut discarded, 4, discarded_info);
            block(
                &["", "k", "v"],
                &[Group {
                    nodes: vec![plain_node(4, 0, 0, &[(1, discarded_tag)], None)],
                    dense: Some(dense(
                        &[(1, 0, 0), (2, 1, 1), (3, 2, 2)],
                        &[1, 2, 0, 1, stripped_tag, 0, 1, discarded_tag, 0],
                    )),
                    ways: vec![discarded],
                    ..Group::default()
                }],
            )
        };
        let good = input(2, 2, &[]);
        let bad = input(127, 127, &[0x08]);
        let full = crate::PrimitiveBlock::from_vec(bad.clone())
            .expect_err("full validation rejects the fixture's bad tag index");
        assert!(
            matches!(
                full.kind(),
                crate::ErrorKind::StringtableIndexOutOfBounds { index: 127 }
            ),
            "{full}"
        );
        let ids = parse_ids(&["n1".to_owned()]).unwrap();
        let deps = parse_ids(&["n2".to_owned()]).unwrap();
        let selection = IdSelection {
            pulled: Some(&deps),
            strip_tags: true,
            ..id_selection(&ids)
        };
        let (expected, _) = run_block(good, &selection, WayCoords::Drop).unwrap();
        let (blocks, stats) = run_block(bad.clone(), &selection, WayCoords::Drop).unwrap();
        assert_eq!(blocks, expected);
        assert_eq!(stats.nodes, 2);
        assert_eq!(stats.found, [(ElementKind::Node, 1)]);
        let keep_dependency_tags = IdSelection {
            strip_tags: false,
            ..selection
        };
        let scoped = run_block(bad, &keep_dependency_tags, WayCoords::Drop)
            .map(drop)
            .expect_err("retaining dependency tags must reject the bad tag index");
        assert!(
            matches!(
                scoped.kind(),
                crate::ErrorKind::StringtableIndexOutOfBounds { index: 127 }
            ),
            "{scoped}"
        );
    }

    #[test]
    fn getid_invert_does_not_validate_removed_records() {
        use crate::read::wire_fixtures::{Group, block, field_bytes, way};

        let input = |tag, info: &[u8]| {
            let mut removed = way(10, &[1], (1, tag));
            field_bytes(&mut removed, 4, info);
            block(
                &["", "k", "v"],
                &[Group {
                    ways: vec![removed, way(20, &[2], (1, 2))],
                    ..Group::default()
                }],
            )
        };
        let ids = parse_ids(&["w10".to_owned()]).unwrap();
        let selection = IdSelection {
            include_matches: false,
            ..id_selection(&ids)
        };
        let coords = crate::owned::WayCoords::Drop;
        let (good, _) = run_block(input(2, &[]), &selection, coords).unwrap();
        let (bad, outcome) = run_block(input(127, &[0x08]), &selection, coords).unwrap();
        assert_eq!(bad, good);
        assert_eq!(outcome.ways, 1);
        assert!(outcome.found.is_empty());
    }

    #[test]
    fn getid_dependency_scans_leave_stripped_tags_for_the_write_pass() {
        use crate::block_builder::HeaderBuilder;
        use crate::read::wire_fixtures::{Group, block, dense, field_bytes, relation, way};

        let dir = crate::path_guard::tests::scratch();
        let ids = parse_ids(&["r40".to_owned()]).unwrap();
        let host = crate::host::Host::for_tests();
        let entered = host.entered();
        let run = |name: &str, tag| {
            let mut pulled = relation(30, &[(0, 20, 1)]);
            field_bytes(&mut pulled, 2, &[1]);
            field_bytes(&mut pulled, 3, &[tag]);
            let dense_tags: &[u64] = if tag == 127 { &[1] } else { &[1, 2, 0] };
            let bytes = block(
                &["", "k", "v"],
                &[Group {
                    dense: Some(dense(&[(1, 0, 0)], dense_tags)),
                    ways: vec![way(20, &[1], (1, u64::from(tag)))],
                    relations: vec![pulled, relation(40, &[(0, 30, 2)])],
                    ..Group::default()
                }],
            );
            let input = dir.path().join(format!("{name}.pbf"));
            let mut writer =
                PbfWriter::new(std::fs::File::create(&input).unwrap(), Compression::None);
            writer
                .write_header(&HeaderBuilder::new().build().unwrap())
                .unwrap();
            writer.write_primitive_block_no_indexdata(&bytes).unwrap();
            writer.flush().unwrap();
            let output = dir.path().join(format!("{name}-out.pbf"));
            let staged = crate::output::Output::new(
                &output,
                crate::output::Overwrite::Refuse,
                &crate::host::Diagnostics::discard(),
            )
            .unwrap()
            .bind(&entered.invocation)
            .unwrap();
            getid_with_refs(
                &PinnedInput::pin(&input, &entered).unwrap(),
                &staged,
                &ids,
                &AddReferenced { remove_tags: true },
                false,
                &WriteOptions::default(),
                &HeaderOverrides::default(),
                &entered,
            )
            .unwrap();
            staged.publish().unwrap();
            assert_eq!(ids_in_file_order(&output), [1, 20, 30, 40]);
            std::fs::read(output).unwrap()
        };
        assert_eq!(run("malformed", 127), run("well-formed", 2));
    }
}
