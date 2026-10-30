//! Concatenate PBF files with optional type filtering. Equivalent to `osmium cat`.

pub mod dedupe;
pub mod reindex;

use super::{
    HeaderOverrides, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, require_indexdata,
};
use crate::blob::{BlobKind, decode_blob_to_headerblock, decompress_blob_data_into};
use crate::blob_meta::scan_block_ids_and_tags;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::file_reader::FileReader;
use crate::owned::{TypeFilter, dense_node_metadata, element_metadata, relation_members_into};
use crate::write::WriteOptions;
use crate::writer::{Compression, PbfWriter, reframe_raw_with_index};
use crate::{BlobFilter, Element, ElementKind, PrimitiveBlock};

use super::output_header::{Declared, OutputFacts, OutputHeader, Ways};
use super::{Result, flush_local};
use crate::error::Error;
use crate::read::frame::Frames;
use crate::read::input::{InputSource, PinnedInput};
use crate::writer::frame_blob_pipelined;

/// Which metadata attributes to strip via `--clean`.
#[derive(Clone, Copy, Default)]
pub struct CleanAttrs {
    pub version: bool,
    pub changeset: bool,
    pub timestamp: bool,
    pub uid: bool,
    pub user: bool,
}

impl CleanAttrs {
    /// True if any attribute is being cleaned.
    pub fn any(&self) -> bool {
        self.version || self.changeset || self.timestamp || self.uid || self.user
    }
}

/// Statistics from a cat operation.
pub struct CatStats {
    pub blobs_passthrough: u64,
    /// Input blobs decoded (sidecar `cat_blobs_decoded`, and the summary's
    /// "Decoded N blobs"), not output blobs written.
    pub blobs_decoded: u64,
    pub elements_written: u64,
}

impl CatStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        if self.blobs_decoded > 0 && self.blobs_passthrough > 0 {
            writeln!(
                w,
                "Decoded {} blobs, wrote {} elements; {} blobs passed through",
                self.blobs_decoded, self.elements_written, self.blobs_passthrough,
            )?;
        } else if self.blobs_decoded > 0 {
            writeln!(
                w,
                "Decoded {} blobs, wrote {} elements",
                self.blobs_decoded, self.elements_written,
            )?;
        } else {
            writeln!(w, "{} blobs passed through", self.blobs_passthrough)?;
        }

        Ok(())
    }
}

/// Concatenate one or more PBF files into a single output.
///
/// If `type_filter` is set, only elements of matching types are included
/// (requires full decode). Without a filter, blobs are passed through as
/// raw bytes (zero decode); an unindexed blob is scanned and reframed with
/// indexdata, and one whose block no single index can describe is
/// re-encoded into single-kind blocks (see `reencode_unclassifiable`).
///
/// Every input is pinned before any is read
/// ([`PinnedInput`](crate::read::input::PinnedInput)), a path given twice
/// sharing one pin, so each is read from the file the run started on. An
/// input handed over as a caller's pin is used as it is (see
/// [`InputSource::pin`](crate::read::input::InputSource::pin)) and never
/// shared with a path. The pins are held for the whole run: one new
/// descriptor per distinct path spelling (a caller's pin opens none), which
/// the run first asks the host for (`ProcessActions::fd_allowance`), with a
/// fixed allowance on top for the reader, writer and transient opens.
///
/// The library never raises the process's open-file limit (descriptor
/// limits are the caller's policy). An embedding program that cats more
/// files than its soft `RLIMIT_NOFILE` leaves room for raises the limit
/// itself (up to the hard limit, which the refusal names when it probed
/// it), has its `ProcessActions` grant more, or cats fewer files per call.
/// Holding the pins is the price of reading each input from one file:
/// re-opening a path between its header read and its blob read could read
/// a file replaced in between.
///
/// # Errors
/// Besides read, decode and write failures: an input that is not a regular
/// file, or a host that allows fewer descriptors than
/// the run holds, is refused before any input is read.
pub fn cat<'a, S: Into<InputSource<'a>> + Copy>(
    files: &[S],
    output: impl crate::output::BindOutput,
    type_filter: Option<TypeFilter>,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = cat_to(files, &output, type_filter, clean, write, overrides, host)?;
    output.publish()?;
    Ok(result)
}

/// [`cat`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn cat_to<'a, S: Into<InputSource<'a>> + Copy>(
    files: &[S],
    output: &crate::output::BoundOutput,
    type_filter: Option<TypeFilter>,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<CatStats> {
    let (direct_io, force) = (write.direct_reads, write.force);
    if files.is_empty() {
        return Err(Error::invalid_argument("no input files specified"));
    }

    // Every input is read at least twice (its header, then its blobs, and
    // between them an indexdata probe on some routes), with every header
    // read before any blob, so every input is pinned up front and holds its
    // pin for the run: one new descriptor per distinct path spelling.
    let sources: Vec<InputSource<'a>> = files.iter().map(|&file| file.into()).collect();
    let new_pins = crate::read::input::new_pins(&sources);
    // Held to the end of the run, which is when the pins close.
    let _reservation = admit_descriptors(
        new_pins,
        &format!(
            "cat holds a file descriptor per distinct input for the whole run: {} input \
             files ({new_pins} distinct paths to pin)",
            files.len()
        ),
        host,
    )?;
    let pins = crate::read::input::pin_sources(&sources, host)?;
    let files = pins.as_slice();

    // Selecting every kind while cleaning skips nothing, so it is the clean
    // pass with no type filter: the same output, and no indexdata to want.
    // Without `--clean` an all-kinds filter keeps its own route, where
    // indexdata buys raw copies.
    let type_filter = type_filter.filter(|tf| !(clean.any() && tf.count() == 3));

    // The indexdata requirement belongs to the paths that use indexdata: a
    // type filter skips a blob of an unselected kind on its indexdata alone,
    // and the passthrough arm copies a selected one raw. The clean pass with
    // every kind selected decodes every blob exactly once whatever its
    // indexdata, so it has no slower fallback for `--force` to make explicit.
    if type_filter.is_some() {
        for file in files {
            require_indexdata(
                file,
                direct_io,
                force,
                "input PBF has no blob-level indexdata. Without indexdata, the type \
                 filter is a no-op - all blobs are decompressed (significantly slower).",
                host,
            )?;
        }
    }

    host.invocation.marker("CAT_SCAN_START");
    let result = match (type_filter, clean.any()) {
        (None, false) => cat_passthrough(files, output, write, overrides, host),
        (None, true) => cat_filtered(
            files,
            output,
            TypeFilter::all(),
            clean,
            write,
            overrides,
            host,
        ),
        (Some(filter), false) => {
            cat_type_passthrough(files, output, filter, write, overrides, host)
        }
        (Some(filter), true) => cat_filtered(files, output, filter, clean, write, overrides, host),
    }?;
    host.invocation.marker("CAT_SCAN_END");
    host.invocation
        .counter("cat_blobs_passthrough", result.blobs_passthrough);
    host.invocation
        .counter("cat_blobs_decoded", result.blobs_decoded);
    host.invocation
        .counter("cat_elements_written", result.elements_written);
    Ok(result)
}

/// Ask the host for the descriptors a cat run opens and holds: `held` (its
/// new pins, or dedupe's pass-2 sources) plus [`CAT_EXTRA_FDS`], and refuse
/// the run up front when the host allows fewer. `demand` names what is
/// held in the refusal. A conservative requirement (the fixed allowance has
/// slack). The caller holds the returned reservation until those
/// descriptors are closed; a refusal drops a short grant at once.
///
/// [`CAT_EXTRA_FDS`]: crate::tunables::CAT_EXTRA_FDS
pub(super) fn admit_descriptors(
    held: usize,
    demand: &str,
    host: &crate::host::Entered<'_>,
) -> Result<crate::process::FdReservation> {
    super::descriptors::admit(
        &super::descriptors::Demand {
            held,
            extra: crate::tunables::CAT_EXTRA_FDS,
            what: demand,
            fewer: "cat fewer files per call",
        },
        host,
    )
}

// ---------------------------------------------------------------------------
// Output header
// ---------------------------------------------------------------------------

/// Read one file's `OSMHeader` block. The header is the first frame; a file
/// that starts with anything else has none (a later one is refused by the
/// frame decoder).
fn read_header(
    input: &PinnedInput,
    direct_io: bool,
    host: &crate::host::Entered<'_>,
) -> Result<crate::HeaderBlock> {
    let file = input.path();
    let mut frames = Frames::of_input(
        FileReader::open_in(input, direct_io, &host.invocation)?,
        file,
        host,
    );
    match frames.next_raw()? {
        Some(frame) if frame.blob_type == BlobKind::OsmHeader => {
            decode_blob_to_headerblock(frame.blob_bytes()).map_err(|e| frames.locate(frame.id, e))
        }
        _ => Err(crate::error::new_missing_header(file)),
    }
}

/// Read every input's `OSMHeader`, one per input, in input order.
fn read_headers(
    files: &[PinnedInput],
    direct_io: bool,
    host: &crate::host::Entered<'_>,
) -> Result<Vec<crate::HeaderBlock>> {
    files
        .iter()
        .map(|f| read_header(f, direct_io, host))
        .collect()
}

/// What a raw-copying cat path does to ways, decided before the header is
/// written: every way blob is copied whole when every input's first data blob
/// carries indexdata, otherwise `unindexed` (the path's treatment of a blob
/// without indexdata). The probe reads the first data blob only; the paths
/// refuse a later unindexed way blob that would break what the header then
/// declared (see [`Declared::broken_by_unindexed_way_blob`]).
///
/// [`Declared::broken_by_unindexed_way_blob`]: super::output_header::Declared::broken_by_unindexed_way_blob
fn raw_copy_ways(
    files: &[PinnedInput],
    headers: &[crate::HeaderBlock],
    direct_io: bool,
    unindexed: Ways,
    host: &crate::host::Entered<'_>,
) -> Result<Ways> {
    let enriched = headers.iter().any(|h| {
        h.has_locations_on_ways() || h.has_way_members_v1() || h.has_shared_node_pins_v1()
    });
    // Without a declared enrichment the variants all declare the same thing,
    // so the probe is skipped.
    if !enriched {
        return Ok(Ways::BlobsCopied);
    }
    for file in files {
        if !super::has_indexdata_in(file, direct_io, host)? {
            return Ok(unindexed);
        }
    }
    Ok(Ways::BlobsCopied)
}

/// The output header of a non-dedupe cat run. Order is the inputs': one sorted
/// input stays sorted, a concatenation of several does not, however each is
/// ordered. Header fields are reconciled across every input by
/// [`output_header::build`](super::output_header::build).
fn cat_output_header(
    headers: &[crate::HeaderBlock],
    ways: Ways,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<OutputHeader> {
    super::output_header::build(headers, &OutputFacts::carry(ways), overrides, host)
}

// ---------------------------------------------------------------------------
// Passthrough path: no type filter, zero decode
// ---------------------------------------------------------------------------

fn cat_passthrough(
    files: &[PinnedInput],
    output: &crate::output::BoundOutput,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<CatStats> {
    let direct_io = write.direct_reads;
    // Every payload is copied byte for byte; an unindexed blob's header is
    // rebuilt to add indexdata, which drops its field 5. The exception is a
    // blob whose block the index scan cannot classify: it is re-encoded, and
    // `reencode_unclassifiable` keeps every declaration `PayloadsCopied`
    // makes true of its ways (inline locations carried, a declared pin or
    // way-membership loss refused).
    let headers = read_headers(files, direct_io, host)?;
    let ways = raw_copy_ways(files, &headers, direct_io, Ways::PayloadsCopied, host)?;
    let header = cat_output_header(&headers, ways, overrides, host)?;
    let declared = header.declared;

    let mut writer = super::writer_from_header_bytes(
        output,
        write.compression,
        &header.bytes,
        write.backend,
        host,
    )?;
    let mut blobs: u64 = 0;
    // Touched only by the unclassifiable-block arm.
    let mut blobs_reencoded: u64 = 0;
    // The subset of `blobs_reencoded` that yielded no elements.
    let mut blobs_omitted: u64 = 0;
    let mut elements_reencoded: u64 = 0;
    let mut decompress_buf: Vec<u8> = Vec::new();

    for input in files {
        let file = input.path();
        let mut frames = Frames::of_input(
            FileReader::open_in(input, direct_io, &host.invocation)?,
            file,
            host,
        );
        // cat is the remedy for a partially indexed file: it indexes every
        // blob that lacks indexdata, so it is exempt from the refusal.
        frames.accept_partial_index();

        while let Some(mut frame) = frames.next_raw()? {
            match &frame.blob_type {
                BlobKind::OsmHeader => {}
                BlobKind::OsmData => {
                    if frame.index.is_some() {
                        // Already has indexdata - pass through as-is.
                        writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))?;
                    } else {
                        // No indexdata - decompress to scan IDs/tags, reframe with index.
                        let blob_bytes = frame.blob_bytes();
                        // Name the input, the blob and its frame offset
                        // through the crate error's own location, which
                        // prints as "(in PATH, blob N, at byte offset M)".
                        // The walk owns the provenance.
                        let id = frame.id;
                        let locate = |e: Error| frames.locate(id, e);
                        decompress_blob_data_into(blob_bytes, &mut decompress_buf)
                            .map_err(locate)?;
                        let (index, tagdata) =
                            scan_block_ids_and_tags(&decompress_buf).map_err(locate)?;
                        let index = match index {
                            Some(idx) => idx,
                            None => {
                                // No single index describes the block (no
                                // elements, a multi-field group, kinds mixed
                                // across groups). Copying it would leave the
                                // output unindexed, and the indexing recipe
                                // this command is would loop: re-encode it
                                // into single-kind indexed blocks instead.
                                let (blocks, count) =
                                    reencode_unclassifiable(&decompress_buf, declared)
                                        .map_err(locate)?;
                                decompress_buf.clear();
                                if blocks.is_empty() {
                                    blobs_omitted += 1;
                                }
                                for block in blocks {
                                    writer.write_primitive_block_owned(block)?;
                                }
                                blobs_reencoded += 1;
                                elements_reencoded += count;
                                continue;
                            }
                        };
                        if declared.way_members && index.kind == crate::ElementKind::Way {
                            return Err(locate(
                                declared.broken_by_unindexed_way_blob("adding its indexdata"),
                            ));
                        }
                        let tagdata_bytes = tagdata
                            .as_ref()
                            .and_then(crate::blob_meta::TagIndex::serialize);
                        let reframed = reframe_raw_with_index(
                            blob_bytes,
                            &index.serialize(),
                            tagdata_bytes.as_deref(),
                            &writer.cancel_token().metrics().writer,
                        )?;
                        decompress_buf.clear();
                        writer.write_raw_owned(reframed)?;
                    }
                    blobs += 1;
                }
                _ => {}
            }
        }
    }

    writer.flush()?;
    note_reencoded(host, blobs_reencoded - blobs_omitted, blobs_omitted);
    Ok(CatStats {
        blobs_passthrough: blobs,
        blobs_decoded: blobs_reencoded,
        elements_written: elements_reencoded,
    })
}

/// Announce the blobs plain cat did not copy byte for byte: the summary
/// counts them as decoded, which alone does not say their content was
/// normalised (rebuilt) or dropped (omitted). The note states the rules
/// that applied, not that each one changed a value in the run's input.
fn note_reencoded(host: &crate::host::Entered<'_>, rebuilt: u64, omitted: u64) {
    if rebuilt > 0 {
        let omitted_clause = if omitted == 0 {
            String::new()
        } else {
            format!(
                "; {omitted} more such blobs yielded no elements and were omitted, with \
                 any unknown fields they carried"
            )
        };
        crate::host::diag!(
            host.diagnostics,
            Note,
            "cat.reencoded_unclassifiable",
            "{rebuilt} blobs held blocks no single index describes and were rebuilt into \
             single-kind blocks by the full re-encode rules: nodes written dense, \
             coordinates truncated toward zero to decimicrodegrees, retained timestamps \
             truncated to whole seconds, metadata with an absent or sentinel version \
             discarded, missing fields of retained metadata written as defaults, unknown \
             fields discarded{omitted_clause}"
        );
    } else if omitted > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "cat.reencoded_unclassifiable",
            "{omitted} blobs held blocks no single index describes, yielded no elements \
             and were omitted, with any unknown fields they carried"
        );
    }
}

/// Re-encode a block the index scan cannot classify into single-kind blocks,
/// each carrying its own index and tagdata. Returns the blocks in output
/// order and the number of elements they hold.
///
/// Elements are re-emitted in decoder order (groups in order; within a group
/// dense nodes, nodes, ways, relations - protobuf gives the interleaving of
/// distinct fields in one message no meaning). A new block starts at every
/// change of kind and at the builder's element cap; a block with no elements
/// yields nothing. Adjacent groups of one kind share a block.
///
/// Content is what the builder normally writes, the same normalisation
/// `cat --type` and `--clean` apply. Kept: ids, the decoded decimicrodegree
/// coordinates of nodes and of inline way locations, tags, refs, members
/// and roles, element order as above, and the presence of retained metadata
/// (no metadata is backfilled onto a node that had none).
/// Normalised or lost (`cat_passthrough` announces both in one note):
///
/// - Every node is written dense, on the default granularity with zero
///   offsets. The decoder truncates each coordinate toward zero to
///   decimicrodegrees before the builder sees it, so a finer granularity or
///   a non-zero offset can move a value, even at the default granularity.
/// - Timestamps are truncated toward zero to whole seconds, whatever the
///   input's date granularity.
/// - Metadata is discarded when its version is absent or the `-1` sentinel
///   (plain `Info` and dense alike; version 0 keeps it). Retained metadata
///   gets explicit defaults for its absent fields, and a `-1` changeset, the
///   sentinel for "none", is written as 0.
/// - Unknown fields of the block, its groups and its elements are dropped,
///   and so are pins the header does not declare (below). A block that
///   yields no elements writes nothing at all, so whatever unknown fields it
///   carried are gone with it.
///
/// Nothing is invented beyond those defaults:
///
/// - A node run is also split where metadata presence flips. One builder
///   block holding nodes with and without metadata would backfill zeroed
///   metadata onto the ones that had none.
/// - Tags, relation roles and the user of re-encoded metadata are resolved
///   through the fallible accessors. The lossy ones would turn a bad string
///   index into a missing tag or an empty string.
/// - Inline way locations (`LocationsOnWays`) are carried whenever a way has
///   them; the decoder has already checked they match its refs one for one.
/// - The builder cannot write field 20, so a way carrying pins under a
///   declared `SharedNodePins-v1` is refused, as is any way under a declared
///   `WayMembers-v1` (the ordinary reindex arm refuses an unindexed way blob
///   there too). Pins the header does not declare are dropped.
///
/// The cold arm owns everything it allocates: a copy of the payload (so the
/// caller's decompression buffer keeps its capacity), the builder and the
/// scratch vectors all live here.
///
/// These blocks are not refused: an empty block loses nothing of OSM
/// content, kinds changing across groups is legal per the PBF spec and older
/// producers wrote it, libosmium reads even multi-field groups, and this
/// command is the indexing recipe every indexdata refusal points at, so a
/// refusal here loops the advice. Refusing only when the re-encode would
/// change a value was also rejected: plain `cat` would then disagree with
/// `--type` and `--clean`, which normalise the same way.
///
/// A split that keeps more is possible without the builder, and is worth
/// building only if a producer writing these shapes turns up and the
/// normalisation matters to someone. Copy the block's string table,
/// granularity, offsets and date granularity verbatim into each output
/// block, and regroup the raw element messages by kind, splitting at each
/// change of kind and at the element cap; each piece is then scanned and
/// indexed as the ordinary reindex arm does. String-table bytes and plain
/// node, way and relation messages would pass through byte for byte. Dense
/// nodes keep their recognised content but not their bytes: a dense group
/// split by a cap or a kind change must have its packed delta arrays
/// re-delta-encoded. It still would not be lossless: unknown fields of the
/// block and of its groups have no home once groups are regrouped, so it
/// needs a policy for them. Its costs: a protobuf-level regrouper the crate
/// does not have (the builder works from decoded elements), the dense
/// re-delta encoder, and the whole string table carried into every piece
/// (wasted bytes unless pruned, which is a re-encode of its own).
#[cold]
#[inline(never)]
#[allow(clippy::too_many_lines)]
fn reencode_unclassifiable(
    payload: &[u8],
    declared: Declared,
) -> crate::Result<(Vec<OwnedBlock>, u64)> {
    let block = PrimitiveBlock::from_vec(payload.to_vec())?;
    let mut bb = BlockBuilder::new();
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut locations_buf: Vec<(i32, i32)> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    // Whether the nodes in the builder carry metadata; read only while the
    // builder holds nodes.
    let mut node_run_has_meta = false;
    let mut count: u64 = 0;

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                check_tags(dn.tags_checked())?;
                let meta = dense_node_metadata(dn)?;
                start_node(&mut bb, &mut output, &mut node_run_has_meta, meta.is_some())?;
                bb.add_node(
                    dn.id(),
                    dn.decimicro_lat(),
                    dn.decimicro_lon(),
                    dn.tags(),
                    meta.as_ref(),
                );
            }
            Element::Node(n) => {
                check_tags(n.tags_checked())?;
                let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                start_node(&mut bb, &mut output, &mut node_run_has_meta, meta.is_some())?;
                bb.add_node(
                    n.id(),
                    n.decimicro_lat(),
                    n.decimicro_lon(),
                    n.tags(),
                    meta.as_ref(),
                );
            }
            Element::Way(w) => {
                if declared.way_members {
                    return Err(declared.broken_by_unindexed_way_blob("re-encoding it"));
                }
                if declared.shared_node_pins && w.shared_node_pins().is_some() {
                    return Err(Error::invalid_data(format!(
                        "way {} sits in a block no single index can describe, and \
                         re-encoding the block into single-kind blocks would drop its \
                         {} pins after the output header already declared them. \
                         Strip the enrichment from the input, and rerun",
                        w.id(),
                        crate::HeaderBlock::SHARED_NODE_PINS_V1,
                    )));
                }
                check_tags(w.tags_checked())?;
                let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
                ensure_way_capacity_local(&mut bb, &mut output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                locations_buf.clear();
                locations_buf.extend(
                    w.node_locations()
                        .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon())),
                );
                if locations_buf.is_empty() {
                    bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                } else {
                    bb.add_way_with_locations(
                        w.id(),
                        w.tags(),
                        &refs_buf,
                        &locations_buf,
                        meta.as_ref(),
                    );
                }
            }
            Element::Relation(r) => {
                check_tags(r.tags_checked())?;
                let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                ensure_relation_capacity_local(&mut bb, &mut output)?;
                relation_members_into(r, &mut members_buf)?;
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
            }
        }
        count += 1;
    }
    flush_local(&mut bb, &mut output)?;
    Ok((output, count))
}

/// Make room for a node whose metadata presence is `has_meta`: flush at the
/// cap, on a change of kind, and where the run's metadata presence flips.
fn start_node(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    node_run_has_meta: &mut bool,
    has_meta: bool,
) -> crate::Result<()> {
    if !bb.can_add_node() || (!bb.is_empty() && *node_run_has_meta != has_meta) {
        flush_local(bb, output)?;
    }
    *node_run_has_meta = has_meta;
    Ok(())
}

/// Resolve every tag string, failing on the first bad string index.
fn check_tags<'t>(
    tags: impl Iterator<Item = crate::Result<(&'t str, &'t str)>>,
) -> crate::Result<()> {
    for tag in tags {
        tag?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Type-filtered passthrough: raw frame copy for matching blob types
// ---------------------------------------------------------------------------

/// Type-filtered cat via raw frame passthrough. Matching blobs (by indexdata
/// ElementKind) are written as-is - zero decompression, zero re-encoding.
/// Non-matching blobs are skipped. Blobs without indexdata fall back to full
/// decode + re-encode.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn cat_type_passthrough(
    files: &[PinnedInput],
    output: &crate::output::BoundOutput,
    tf: TypeFilter,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<CatStats> {
    let blob_filter = BlobFilter::new(tf.nodes, tf.ways, tf.relations);
    let direct_io = write.direct_reads;

    // Matching indexed blobs are copied whole; an unindexed blob is decoded
    // and re-encoded without way enrichment.
    let headers = read_headers(files, direct_io, host)?;
    let ways = raw_copy_ways(files, &headers, direct_io, Ways::Reencoded, host)?;
    let header = cat_output_header(&headers, ways, overrides, host)?;
    let declared = header.declared;

    let mut writer = super::writer_from_header_bytes(
        output,
        write.compression,
        &header.bytes,
        write.backend,
        host,
    )?;
    let mut blobs_passthrough: u64 = 0;
    let mut blobs_decoded: u64 = 0;
    let mut elements_written: u64 = 0;

    let clean = CleanAttrs::default(); // no-op clean

    // Hoisted outside per-blob loop: reused across iterations.
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut bb = BlockBuilder::new();
    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();

    for input in files {
        let file = input.path();
        let mut frames = Frames::of_input(
            FileReader::open_in(input, direct_io, &host.invocation)?,
            file,
            host,
        );
        // cat is the remedy for a partially indexed file, so `--type` is
        // exempt from the refusal: a blob without indexdata is decoded and
        // its selected elements re-encoded with fresh indexdata.
        frames.accept_partial_index();

        while let Some(mut frame) = frames.next_raw()? {
            match &frame.blob_type {
                BlobKind::OsmHeader => {}
                BlobKind::OsmData => {
                    if let Some(ref idx) = frame.index {
                        if !blob_filter.wants_index(idx) {
                            continue;
                        }
                        writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))?;
                        blobs_passthrough += 1;
                    } else {
                        let blob_bytes = frame.blob_bytes();
                        decompress_buf.clear();
                        let id = frame.id;
                        let locate = |e: Error| frames.locate(id, e);
                        decompress_blob_data_into(blob_bytes, &mut decompress_buf)
                            .map_err(locate)?;
                        // Take the decompressed Vec straight into the block: no
                        // Bytes round-trip, no second whole-buffer copy (the
                        // dropped `new_with_scratch(Bytes)` route paid a `to_vec`).
                        // `decompress_buf` is left empty and reallocated next blob.
                        let block = PrimitiveBlock::from_vec_with_scratch(
                            std::mem::take(&mut decompress_buf),
                            &mut st_scratch,
                            &mut gr_scratch,
                        )
                        .map_err(locate)?;
                        output_blocks.clear();
                        let count = process_block(
                            &block,
                            &mut bb,
                            &mut output_blocks,
                            &tf,
                            &clean,
                            &mut refs_buf,
                        )?;
                        flush_local(&mut bb, &mut output_blocks)?;
                        if declared.way_enrichment()
                            && output_blocks
                                .iter()
                                .any(|b| b.index.kind == crate::ElementKind::Way)
                        {
                            return Err(locate(
                                declared.broken_by_unindexed_way_blob("re-encoding it"),
                            ));
                        }
                        for block in output_blocks.drain(..) {
                            writer.write_primitive_block_owned(block)?;
                        }
                        blobs_decoded += 1;
                        elements_written += count;
                    }
                }
                _ => {}
            }
        }
    }

    writer.flush()?;
    Ok(CatStats {
        blobs_passthrough,
        blobs_decoded,
        elements_written,
    })
}

// ---------------------------------------------------------------------------
// Filtered path: parallel decode + rebuild
// ---------------------------------------------------------------------------

/// Process a single `PrimitiveBlock` through the type filter, writing matching
/// elements into the thread-local `BlockBuilder` and flushing complete blocks
/// into `output`. Returns the number of elements written.
///
/// Called from rayon worker threads via `map_init`.
fn process_block(
    block: &crate::PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    tf: &TypeFilter,
    clean: &CleanAttrs,
    refs_buf: &mut Vec<i64>,
) -> crate::Result<u64> {
    // Elements are kept by their actual kind against the selection, never by
    // the blob's indexdata kind. What indexdata decides is upstream: whether
    // the blob was scheduled at all (a blob labelled with an unselected kind
    // is skipped whole, see reference/indexdata-trust.md).
    let (filter_node, filter_way, filter_relation) = (tf.nodes, tf.ways, tf.relations);
    let mut count: u64 = 0;
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) if filter_node => {
                ensure_node_capacity_local(bb, output)?;
                let meta = clean_metadata(dense_meta(dn, clean)?, clean);
                bb.add_node(
                    dn.id(),
                    dn.decimicro_lat(),
                    dn.decimicro_lon(),
                    dn.tags(),
                    meta.as_ref(),
                );
                count += 1;
            }
            Element::Node(n) if filter_node => {
                ensure_node_capacity_local(bb, output)?;
                let meta = clean_metadata(
                    element_meta(clean, ElementKind::Node, n.id(), &n.info())?,
                    clean,
                );
                bb.add_node(
                    n.id(),
                    n.decimicro_lat(),
                    n.decimicro_lon(),
                    n.tags(),
                    meta.as_ref(),
                );
                count += 1;
            }
            Element::Way(w) if filter_way => {
                ensure_way_capacity_local(bb, output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                let meta = clean_metadata(
                    element_meta(clean, ElementKind::Way, w.id(), &w.info())?,
                    clean,
                );
                bb.add_way(w.id(), w.tags(), refs_buf, meta.as_ref());
                count += 1;
            }
            Element::Relation(r) if filter_relation => {
                ensure_relation_capacity_local(bb, output)?;
                relation_members_into(r, &mut members_buf)?;
                let meta = clean_metadata(
                    element_meta(clean, ElementKind::Relation, r.id(), &r.info())?,
                    clean,
                );
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                count += 1;
            }
            _ => {}
        }
    }

    Ok(count)
}

use super::clean_metadata;

/// Resolve metadata for re-encoding. Under `--clean user` the user is dropped
/// afterwards, so it is not resolved at all: `cat` is the recovery command and
/// a file with an out-of-range user string index must be rescuable with it.
/// Every other re-encoding path refuses such an index; roles have no
/// `--clean` and stay refused.
fn element_meta<'a>(
    clean: &CleanAttrs,
    kind: ElementKind,
    id: i64,
    info: &crate::Info<'a>,
) -> crate::Result<Option<crate::block_builder::Metadata<'a>>> {
    if clean.user {
        crate::owned::element_metadata_dropping_user(kind, id, info)
    } else {
        element_metadata(kind, id, info)
    }
}

/// Dense-node counterpart of [`element_meta`].
fn dense_meta<'a>(
    dn: &'a crate::DenseNode<'a>,
    clean: &CleanAttrs,
) -> crate::Result<Option<crate::block_builder::Metadata<'a>>> {
    if clean.user {
        crate::owned::dense_node_metadata_dropping_user(dn)
    } else {
        dense_node_metadata(dn)
    }
}

/// Cat with full decode + re-encode, the `--clean` path (with or without a
/// type filter).
///
/// One pass per input: a header walk schedules every data blob, minus those
/// whose indexdata claims an unselected kind, and
/// `parallel_classify_phase_ordered` decodes each scheduled blob exactly
/// once. The worker keeps the selected elements by their actual kind,
/// rebuilds them through a worker-local `BlockBuilder` (a new block at each
/// change of kind and at the element cap) and frames the output blobs; the
/// ordered merge writes them in schedule order.
///
/// A blob without indexdata needs nothing else: it is scheduled and decoded
/// like any other, so a partially indexed input gives the output its wholly
/// indexed form would (`reference/indexdata-trust.md`, "Missing and partial
/// indexdata"), and with every kind selected a wholly unindexed input costs
/// what an indexed one does.
///
/// **Output order** is input order: the emitted elements are a subsequence of
/// the decoded input sequence, so an honestly sorted input stays sorted, and an
/// unsorted one is not regrouped by kind. (An earlier shape ran one phase per
/// kind, which regrouped unsorted input by kind and had to decode every
/// unindexed blob once per phase, since nothing said which kinds it held.)
///
/// Replaces an earlier `into_blocks_pipelined` + `for_each_primitive_block_batch_budgeted`
/// shape that hit the cross-thread `PrimitiveBlock` retention pattern of the
/// pipelined reader at the time (~25 GB at planet scale; OOM at 28.9 GB peak
/// measured 2026-04-26 overnight): blocks were allocated on its decode pool
/// and dropped on the consumer thread, so glibc kept the freed pages on the
/// decode threads' arenas, and batching only narrowed that window without
/// closing it. The classify pool confines each blob's allocation and
/// drop to a single worker thread. The pipelined reader has since moved its
/// string-table entries inline into the pooled decompression buffer (see its
/// "Memory: decoded-block working set" doc in `src/read/pipeline.rs`), so
/// this history no longer describes a live hazard there.
fn cat_filtered(
    files: &[PinnedInput],
    output: &crate::output::BoundOutput,
    tf: TypeFilter,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<CatStats> {
    let (compression, direct_io) = (write.compression, write.direct_reads);
    // Per-blob worker pool; same precedent as check --refs / verify_ids.
    // The workers do BlockBuilder re-encode (allocation-heavy) but each
    // blob's alloc/free cycle is confined to a single worker thread, so
    // the cross-blob scratch reuse that arena capping penalises (measured
    // on the pre-migration time-filter snapshot path) doesn't apply.
    host.process.cross_thread_alloc_pool();

    // -----------------------------------------------------------------------
    // Output header: reconciled across every input; every way re-encoded.
    // -----------------------------------------------------------------------
    let headers = read_headers(files, direct_io, host)?;
    let header = cat_output_header(&headers, Ways::Reencoded, overrides, host)?;
    let mut writer =
        super::writer_from_header_bytes(output, compression, &header.bytes, write.backend, host)?;

    let mut blobs_decoded: u64 = 0;
    let mut elements: u64 = 0;

    // Indexdata prunes only when a kind is unselected; with every kind
    // selected there is nothing to filter and no note to give.
    let kind_filter = (tf.count() < 3).then(|| BlobFilter::new(tf.nodes, tf.ways, tf.relations));

    for file in files {
        // cat is the remedy for a partially indexed file, so this walk is
        // exempt from the command-input refusal: a blob without indexdata is
        // scheduled whatever the selection and decoded like the rest.
        let (schedule, shared_file, _) =
            crate::scan::classify::build_classify_schedule_blob_filtered_accepting_partial(
                file,
                None,
                kind_filter.as_ref(),
                host,
            )?;
        // The ordered pass of one input, kind phases nested inside (see
        // `KindPhase`). Closed before an error propagates, so the pair holds
        // on a failing run too.
        host.invocation.marker("CAT_FILTERED_START");
        let pass = run_filtered_pass(
            &shared_file,
            &schedule,
            tf,
            clean,
            compression,
            &mut writer,
            host,
        );
        host.invocation.marker("CAT_FILTERED_END");
        let (b, e) = pass?;
        blobs_decoded += b;
        elements += e;
    }

    writer.flush()?;

    Ok(CatStats {
        blobs_passthrough: 0,
        blobs_decoded,
        elements_written: elements,
    })
}

/// Decode, filter, clean and frame every scheduled blob of one input, and
/// write the framed blobs in schedule order. Returns the number of input
/// blobs decoded and of elements written.
///
/// The ordered pool is what bounds memory here. Worker output is large
/// (~1.6 MB framed per planet node blob, tens of thousands of node blobs), and
/// ordering alone would not cap it: one slow blob at the head lets workers
/// race ahead while their results wait for it. The pool admits schedule
/// position `k` only once `k` is within the window of the earliest unwritten
/// position, so at most a window of results is alive between decode and the
/// end of its write, whatever the input size. Each result holds one
/// input blob's rebuilt output; a block that switches kind often frames into
/// several smaller blobs, still bounded by that one block.
fn run_filtered_pass(
    input: &crate::read::input::InputFile,
    schedule: &[crate::scan::classify::ScheduledBlob],
    tf: TypeFilter,
    clean: &CleanAttrs,
    compression: Compression,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    host: &crate::host::Entered<'_>,
) -> Result<(u64, u64)> {
    type PassResult = crate::Result<(Vec<(ElementKind, Vec<u8>)>, u64)>;
    let mut blobs_decoded: u64 = 0;
    let mut total_elements: u64 = 0;
    let mut phase = KindPhase::default();

    // A fresh BlockBuilder per blob (BlockBuilder::new is just a few empty
    // Vec/HashMap initialisers; no heap reservation until elements are
    // added), as before the worker state could hold a non-Send one: keeping
    // one per worker would change what each worker retains between blobs. A
    // failed blob ends the pool, located with its input and ordinal.
    let pass = crate::scan::classify::parallel_classify_phase_ordered(
        input,
        schedule,
        None,
        std::num::NonZeroUsize::new(crate::tunables::POOL_REORDER_CAPACITY_WIDE),
        || (),
        |block, _state| -> PassResult {
            let mut bb = BlockBuilder::new();
            let mut refs_buf: Vec<i64> = Vec::new();
            let mut output: Vec<OwnedBlock> = Vec::new();
            let count = process_block(block, &mut bb, &mut output, &tf, clean, &mut refs_buf)?;
            flush_local(&mut bb, &mut output)?;

            let mut framed: Vec<(ElementKind, Vec<u8>)> = Vec::with_capacity(output.len());
            for OwnedBlock {
                bytes: block_bytes,
                index,
                tagdata,
                way_members,
            } in output
            {
                let indexdata = index.serialize();
                let blob = frame_blob_pipelined(
                    &block_bytes,
                    &compression,
                    Some(indexdata.as_slice()),
                    tagdata.as_deref(),
                    way_members.as_deref(),
                    &host.invocation.metrics().writer,
                )?;
                framed.push((index.kind, blob.into_vec()));
            }
            Ok((framed, count))
        },
        |_position, (framed, count)| {
            // Called in schedule order; a write error ends the pass at once.
            blobs_decoded += 1;
            for (kind, blob) in framed {
                phase.enter(kind, &host.invocation);
                writer.write_raw_owned(blob)?;
            }
            total_elements += count;
            Ok(())
        },
        host,
    );
    phase.close(&host.invocation);
    pass?;

    Ok((blobs_decoded, total_elements))
}

/// The sidecar kind phase of one `--clean` pass: `CAT_NODES_*`,
/// `CAT_WAYS_*` and `CAT_RELATIONS_*`, the names the earlier one-phase-per-kind
/// shape emitted (the recorded planet baseline's phases), now driven by the
/// output.
///
/// A phase opens when the first blob of its kind is about to be written, and
/// closes when a blob of a greater kind (node < way < relation) is, or when
/// the pass ends. They are write-side kind transitions, not execution
/// barriers: the pool decodes up to its admission window of tasks ahead of
/// the write, so work on the next kind overlaps the phase before it, and the
/// wait for the first next-kind result is charged to the phase before. A
/// per-phase comparison against the per-kind shape is therefore approximate,
/// with no time bound the code enforces; the wall and the enclosing
/// `CAT_FILTERED_*` pair are the exact figures.
///
/// A blob of an equal or lower kind changes nothing, so an unsorted input
/// cannot flood the marker stream: each phase is named by the greatest kind
/// written so far, at most three pairs per input. Work that writes nothing (an
/// unselected kind's unindexed blobs) opens no phase, so a pass that writes
/// nothing has the `CAT_FILTERED_*` pair alone.
#[derive(Default)]
struct KindPhase(Option<ElementKind>);

impl KindPhase {
    fn enter(&mut self, kind: ElementKind, invocation: &crate::host::Recorder) {
        if self.0.is_some_and(|open| kind <= open) {
            return;
        }
        self.close(invocation);
        invocation.marker(Self::marker(kind, true));
        self.0 = Some(kind);
    }

    fn close(&mut self, invocation: &crate::host::Recorder) {
        if let Some(open) = self.0.take() {
            invocation.marker(Self::marker(open, false));
        }
    }

    fn marker(kind: ElementKind, start: bool) -> &'static str {
        match (kind, start) {
            (ElementKind::Node, true) => "CAT_NODES_START",
            (ElementKind::Node, false) => "CAT_NODES_END",
            (ElementKind::Way, true) => "CAT_WAYS_START",
            (ElementKind::Way, false) => "CAT_WAYS_END",
            (ElementKind::Relation, true) => "CAT_RELATIONS_START",
            (ElementKind::Relation, false) => "CAT_RELATIONS_END",
        }
    }
}
