//! Generate an OSC diff from two PBF snapshots. Equivalent to `osmium derive-changes`.
//!
//! Streams through both files writing changes directly to temp files,
//! then assembles the final `.osc.gz` in a single pass. Memory is bounded
//! by one element at a time, not total change count.
//! Requires both inputs to declare `Sort.Type_then_ID`.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

use crate::ElementKind;
use crate::commands::input_front::sorted_and_indexed;
use crate::error::{Error, Result};
use crate::osc::merge_join::{
    BlockMergeAction, BlockPairMergeState, StreamingBlocks, block_pair_merge_phase,
    stream_merge_phase,
};
use crate::osc::write::{OwnedMetadata, write_element_xml};
use crate::read::input::{InputSource, PinnedInput};

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics from a derive-changes operation.
#[derive(Debug)]
pub struct DeriveChangesStats {
    pub creates: u64,
    pub modifies: u64,
    pub deletes: u64,
}

impl DeriveChangesStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let total = self.creates + self.modifies + self.deletes;
        writeln!(
            w,
            "{total} changes: {} creates, {} modifies, {} deletes",
            self.creates, self.modifies, self.deletes,
        )?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generate an OSC diff from two sorted PBF snapshots.
///
/// Streams through both files using pipelined block iterators and performs
/// a merge-join by (type, id). Changes are buffered by action type and
/// written as OsmChange XML, gzipped when the output name ends `.osc.gz` and
/// plain when it ends `.osc` (any other name is refused before any work). Memory is bounded by the number of
/// changed elements, not total input size.
///
/// `timestamp` overrides every emitted delete timestamp with the same Unix second.
/// Pass `None` to omit delete timestamps.
///
/// Requires both inputs to declare `Sort.Type_then_ID` - returns an
/// actionable error if either is unsorted.
///
/// Every route opens each input more than once, so both must be regular
/// files or block devices (`InvalidArgument` otherwise) and are pinned for
/// the run once the output name is accepted, `old` first, before either is
/// read (see [`PinnedInput`] and [`InputSource::pin`]). A path spelled
/// exactly as the other shares its pin, so deriving a file against itself
/// yields no changes; a caller may pin either input first and pass the pin,
/// which is used as it is. Where a later open cannot reach the pinned file,
/// an input replaced mid-run fails with `InputReplaced` and nothing is
/// published.
#[allow(clippy::too_many_arguments)]
pub fn derive_changes<'a, 'b>(
    old: impl Into<InputSource<'a>>,
    new: impl Into<InputSource<'b>>,
    output: crate::output::Output,
    direct_io: bool,
    increment_version: bool,
    timestamp: Option<u64>,
    parallelism: crate::host::Parallelism,
    host: &crate::host::Host,
    scratch: crate::scratch::RunScratch,
) -> Result<DeriveChangesStats> {
    host.cancel.check()?;
    let result = derive_changes_to(
        old.into(),
        new.into(),
        &output,
        direct_io,
        increment_version,
        timestamp,
        parallelism,
        host,
        &scratch,
    )?;
    drop(scratch);
    output.publish(&host.cancel)?;
    Ok(result)
}

/// [`derive_changes`] writing to a borrowed output it does not publish. The
/// inputs are pinned here, after the output-name refusal and before either
/// is read; every open below is derived from a pin.
#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
pub(crate) fn derive_changes_to(
    old: InputSource<'_>,
    new: InputSource<'_>,
    output: &crate::output::Output,
    direct_io: bool,
    increment_version: bool,
    timestamp: Option<u64>,
    parallelism: crate::host::Parallelism,
    host: &crate::host::Host,
    scratch: &crate::scratch::RunScratch,
) -> Result<DeriveChangesStats> {
    host.cancel.check()?;
    // The name rule every OSC producer shares, applied before any work.
    crate::osc::output_is_gzip(output.destination())?;
    let jobs = super::resolve_shards(parallelism, host);
    let [old, new] = super::pin_pair(old, new, host)?;
    let (old_path, new_path) = (old.path(), new.path());
    // Sorted headers + indexdata from one front read of each input.
    let (old_sorted, old_indexed) = sorted_and_indexed(&old, direct_io, host)?;
    let (new_sorted, new_indexed) = sorted_and_indexed(&new, direct_io, host)?;
    crate::commands::require_sorted(
        old_sorted,
        old_path,
        "Old PBF",
        "Streaming diff requires sorted inputs to operate in constant memory.",
    )?;
    crate::commands::require_sorted(
        new_sorted,
        new_path,
        "New PBF",
        "Streaming diff requires sorted inputs to operate in constant memory.",
    )?;
    let both_indexed = old_indexed && new_indexed;
    if jobs > 1 && !both_indexed {
        let reason = "missing indexdata";
        crate::host::diag!(
            host.diagnostics,
            Note,
            "derive_changes.sequential_fallback",
            "Using sequential comparison: {reason}"
        );
        crate::debug::emit_counter("derive_changes_sequential_fallback", 1);
    }

    // No compressed-input multiplier can predict changed XML bytes: tag
    // escaping, reference lists and the changed fraction determine expansion.
    // An exact preflight would repeat comparison and XML serialization on this
    // measured path. Keep write errors authoritative instead of refusing a
    // potentially empty diff against an invented scratch-space requirement.

    crate::debug::emit_marker("DERIVECHANGES_SCAN_START");

    // Derivation has no verbose-detail mode: only indexdata and worker count
    // gate this attempt, unlike the text diff driver.
    // Parallel shard-based path. Only valid when both inputs are indexed
    // (same constraint as derive_changes_block_pair) and hold no negative
    // IDs (the driver declines with `None`). Falls through to the
    // sequential sink path otherwise.
    let parallel_stats = if both_indexed && jobs > 1 {
        super::derive_parallel::derive_changes_parallel(
            &old,
            &new,
            output,
            scratch,
            jobs,
            increment_version,
            timestamp,
            host,
        )?
    } else {
        None
    };
    if let Some(stats) = parallel_stats {
        crate::debug::emit_marker("DERIVECHANGES_SCAN_END");
        crate::debug::emit_counter("derivechanges_creates", stats.creates);
        crate::debug::emit_counter("derivechanges_modifies", stats.modifies);
        crate::debug::emit_counter("derivechanges_deletes", stats.deletes);
        return Ok(stats);
    }

    let mut sink = ChangeSink::new(scratch, increment_version, timestamp)?;

    let result = (|| -> Result<DeriveChangesStats> {
        if both_indexed {
            derive_changes_block_pair(&old, &new, direct_io, &mut sink, host)?;
        } else {
            derive_changes_element_stream(&old, &new, direct_io, &mut sink, host)?;
        }
        sink.flush()?;

        crate::debug::emit_marker("DERIVECHANGES_SCAN_END");

        let stats = sink.stats();

        crate::debug::emit_marker("DERIVECHANGES_WRITE_START");
        assemble_osc(output, &sink, host)?;
        crate::debug::emit_marker("DERIVECHANGES_WRITE_END");
        Ok(stats)
    })();
    // Remove fragments before returning, on errors and during unwinding too.
    drop(sink);
    let stats = result?;

    crate::debug::emit_counter("derivechanges_creates", stats.creates);
    crate::debug::emit_counter("derivechanges_modifies", stats.modifies);
    crate::debug::emit_counter("derivechanges_deletes", stats.deletes);

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Streaming change sink - writes element XML directly to temp files
// ---------------------------------------------------------------------------

struct ChangeSink {
    creates: Writer<io::BufWriter<File>>,
    modifies: Writer<io::BufWriter<File>>,
    deletes: Writer<io::BufWriter<File>>,
    creates_path: crate::scratch::ScratchPath,
    modifies_path: crate::scratch::ScratchPath,
    deletes_path: crate::scratch::ScratchPath,
    create_count: u64,
    modify_count: u64,
    delete_count: u64,
    increment_version: bool,
    timestamp: Option<u64>,
    coord_buf: String,
}

impl ChangeSink {
    fn new(
        scratch: &crate::scratch::RunScratch,
        increment_version: bool,
        timestamp: Option<u64>,
    ) -> Result<Self> {
        let cp = scratch.file("derive-creates.xml.tmp")?;
        let mp = scratch.file("derive-modifies.xml.tmp")?;
        let dp = scratch.file("derive-deletes.xml.tmp")?;
        let creates = Writer::new(io::BufWriter::new(crate::scratch::create_file(&cp)?));
        let modifies = Writer::new(io::BufWriter::new(crate::scratch::create_file(&mp)?));
        let deletes = Writer::new(io::BufWriter::new(crate::scratch::create_file(&dp)?));
        Ok(Self {
            creates,
            modifies,
            deletes,
            creates_path: cp,
            modifies_path: mp,
            deletes_path: dp,
            create_count: 0,
            modify_count: 0,
            delete_count: 0,
            increment_version,
            timestamp,
            coord_buf: String::new(),
        })
    }

    fn write_create(&mut self, elem: &crate::Element<'_>) -> Result<()> {
        write_element_xml(&mut self.creates, elem, &mut self.coord_buf)?;
        self.create_count += 1;
        Ok(())
    }

    fn write_modify(&mut self, elem: &crate::Element<'_>) -> Result<()> {
        write_element_xml(&mut self.modifies, elem, &mut self.coord_buf)?;
        self.modify_count += 1;
        Ok(())
    }

    fn write_delete(&mut self, elem: &crate::Element<'_>, kind: ElementKind) -> Result<()> {
        if let Some((tag, id, meta)) = extract_delete_info(elem, kind) {
            write_delete_element(
                &mut self.deletes,
                tag,
                id,
                meta.as_ref(),
                self.increment_version,
                self.timestamp,
            )?;
            self.delete_count += 1;
        }
        Ok(())
    }

    /// Record one merge action, from either sequential path. The one place
    /// a sequential derive turns an action into OSC, so the output does not
    /// depend on whether the inputs carried indexdata.
    fn record(&mut self, kind: ElementKind, action: &BlockMergeAction<'_>) -> Result<()> {
        match *action {
            BlockMergeAction::BlobEqual(_) | BlockMergeAction::ElementEqual { .. } => {}
            BlockMergeAction::BlobOldOnly { block, skip, .. } => {
                for elem in block.elements().skip(skip) {
                    self.write_delete(&elem, kind)?;
                }
            }
            BlockMergeAction::BlobNewOnly { block, skip, .. } => {
                for elem in block.elements().skip(skip) {
                    self.write_create(&elem)?;
                }
            }
            BlockMergeAction::ElementModified { new, .. } => self.write_modify(new)?,
            BlockMergeAction::ElementOldOnly(o) => self.write_delete(o, kind)?,
            BlockMergeAction::ElementNewOnly(n) => self.write_create(n)?,
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.creates.get_mut().flush()?;
        self.modifies.get_mut().flush()?;
        self.deletes.get_mut().flush()?;
        Ok(())
    }

    fn stats(&self) -> DeriveChangesStats {
        DeriveChangesStats {
            creates: self.create_count,
            modifies: self.modify_count,
            deletes: self.delete_count,
        }
    }
}

/// Extract just id + metadata from a borrowed element for delete output.
/// Avoids full owned conversion (no tag/ref cloning - deletes only need id + version).
/// Shared by the sequential sink and the parallel shard workers, so both
/// paths emit the same delete for the same element. The version is passed
/// raw; `write_delete_element` decides whether it is a claim.
pub(super) fn extract_delete_info(
    elem: &crate::Element<'_>,
    kind: ElementKind,
) -> Option<(&'static str, i64, Option<OwnedMetadata>)> {
    match (kind, elem) {
        (ElementKind::Node, crate::Element::DenseNode(dn)) => Some((
            "node",
            dn.id(),
            dn.info()
                .map(crate::dense::DenseNodeInfo::version)
                .map(OwnedMetadata::version_only),
        )),
        (ElementKind::Node, crate::Element::Node(n)) => Some((
            "node",
            n.id(),
            n.info().version().map(OwnedMetadata::version_only),
        )),
        (ElementKind::Way, crate::Element::Way(w)) => Some((
            "way",
            w.id(),
            w.info().version().map(OwnedMetadata::version_only),
        )),
        (ElementKind::Relation, crate::Element::Relation(r)) => Some((
            "relation",
            r.id(),
            r.info().version().map(OwnedMetadata::version_only),
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Change collection via shared merge-join
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Optimized block-pair path (borrowed elements, zero-alloc for Equal)
// ---------------------------------------------------------------------------

/// Stream changes using block-pair merge with borrowed elements.
/// Only changed elements (~1.2% of typical daily diff) are materialized as owned,
/// written to temp files immediately, then dropped.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn derive_changes_block_pair(
    old: &PinnedInput,
    new: &PinnedInput,
    direct_io: bool,
    sink: &mut ChangeSink,
    host: &crate::host::Host,
) -> Result<()> {
    let mut old_reader = crate::blob::BlobReader::open_in(old, direct_io, host)?;
    old_reader.set_parse_indexdata(true);
    let mut new_reader = crate::blob::BlobReader::open_in(new, direct_io, host)?;
    new_reader.set_parse_indexdata(true);

    let mut merge = BlockPairMergeState::new(old_reader, new_reader);

    crate::debug::emit_marker("DERIVECHANGES_PHASE_NODE_START");
    collect_phase_block_pair(&mut merge, ElementKind::Node, sink)?;
    crate::debug::emit_marker("DERIVECHANGES_PHASE_NODE_END");

    crate::debug::emit_marker("DERIVECHANGES_PHASE_WAY_START");
    collect_phase_block_pair(&mut merge, ElementKind::Way, sink)?;
    crate::debug::emit_marker("DERIVECHANGES_PHASE_WAY_END");

    crate::debug::emit_marker("DERIVECHANGES_PHASE_REL_START");
    collect_phase_block_pair(&mut merge, ElementKind::Relation, sink)?;
    crate::debug::emit_marker("DERIVECHANGES_PHASE_REL_END");

    super::emit_merge_stats_counters(&merge.stats);
    Ok(())
}

/// Run one type phase of block-pair merge, streaming changed elements to temp files.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn collect_phase_block_pair(
    merge: &mut BlockPairMergeState,
    kind: ElementKind,
    sink: &mut ChangeSink,
) -> Result<()> {
    block_pair_merge_phase(merge, kind, true, &mut |action| sink.record(kind, &action))
}

/// Fallback path for inputs without indexdata: the element-level streaming
/// merge, borrowed elements straight from each side's current block, fed
/// through the same [`ChangeSink::record`] as the block-pair path.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn derive_changes_element_stream(
    old: &PinnedInput,
    new: &PinnedInput,
    direct_io: bool,
    sink: &mut ChangeSink,
    host: &crate::host::Host,
) -> Result<()> {
    let pool = crate::osc::merge_join::merge_buffers();
    let mut old_src =
        StreamingBlocks::new_sequential(old, direct_io, std::sync::Arc::clone(&pool), host)?;
    let mut new_src = StreamingBlocks::new_sequential(new, direct_io, pool, host)?;

    let phases = [
        (
            ElementKind::Node,
            "DERIVECHANGES_PHASE_NODE_START",
            "DERIVECHANGES_PHASE_NODE_END",
        ),
        (
            ElementKind::Way,
            "DERIVECHANGES_PHASE_WAY_START",
            "DERIVECHANGES_PHASE_WAY_END",
        ),
        (
            ElementKind::Relation,
            "DERIVECHANGES_PHASE_REL_START",
            "DERIVECHANGES_PHASE_REL_END",
        ),
    ];
    for (kind, start, end) in phases {
        crate::debug::emit_marker(start);
        stream_merge_phase(&mut old_src, &mut new_src, kind, &mut |action| {
            sink.record(kind, &action)
        })?;
        crate::debug::emit_marker(end);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// OSC XML writer
// ---------------------------------------------------------------------------

/// Assemble the final `.osc.gz` from temp file fragments.
/// Writes XML structure via quick_xml Writer, copies raw fragment bytes
/// directly to the underlying parallel gzip writer (not through the XML
/// writer).
fn assemble_osc(
    output: &crate::output::Output,
    sink: &ChangeSink,
    host: &crate::host::Host,
) -> Result<()> {
    assemble_osc_from_paths(
        output,
        &sink.creates_path,
        &sink.modifies_path,
        &sink.deletes_path,
        sink.create_count,
        sink.modify_count,
        sink.delete_count,
        host,
    )
}

/// Assemble the final `.osc.gz` from three external temp file fragments.
/// Same logic as `assemble_osc` but takes the paths + counts directly,
/// so non-`ChangeSink` callers (e.g. the parallel derive path) can
/// produce fragments however they like and feed them in.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_osc_from_paths(
    output: &crate::output::Output,
    creates_path: &Path,
    modifies_path: &Path,
    deletes_path: &Path,
    create_count: u64,
    modify_count: u64,
    delete_count: u64,
    host: &crate::host::Host,
) -> Result<()> {
    use crate::write::parallel_gzip::{DEFAULT_CHUNK_SIZE, ParallelGzipWriter};

    // Stage boundary. The assembly itself stops through the output writer,
    // which observes the token at every buffer it writes (the gzip writer
    // thread records that as its failure and every later `write` returns it).
    host.cancel.check()?;

    // Framing follows the output name, the rule every OSC producer shares
    // (`crate::osc::output_is_gzip`): `.osc.gz` is written as ordered gzip
    // chunks, `.osc` as plain XML.
    let gzip = crate::osc::output_is_gzip(output.destination())?;
    // The three XML fragments remain live throughout gzip assembly. Their
    // exact lengths are now known, but are not the compressed output size:
    // refusing unless that much space remains would reject compressible XML.
    // Output can also be on a different filesystem from host-selected scratch.
    // A statvfs snapshot would not reserve capacity against concurrent writers;
    // propagate actual output write failures rather than promise a preflight.
    let buf = output.writer(&host.cancel)?;

    // Gzip member count = ceil(total_uncompressed_bytes / chunk_size).
    // At planet scale (~45 GB of XML across the three temp files) that's
    // ~23k 2 MB members; the 18 B per-member framing is ~400 KB of
    // overhead, and the per-chunk dictionary reset costs ~1-3% on the
    // ratio. See the CHANGELOG for measured wall wins.
    // The gzip worker count comes from host parallelism (every available
    // thread; 4 when the host reports none), not from `-j`: `-j` gates and
    // sizes the shard comparison, and this assembler runs after it has
    // finished. A `-j 1` run therefore still compresses on every thread.
    let worker_count = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DERIVE_GZIP_WORKERS_RESERVED,
        crate::tunables::DERIVE_GZIP_WORKERS_MIN,
        crate::tunables::DERIVE_GZIP_WORKERS_FALLBACK,
    );
    if gzip {
        let gz = ParallelGzipWriter::new(buf, DEFAULT_CHUNK_SIZE, worker_count, &host.cancel);
        let gz = write_document(
            gz,
            creates_path,
            modifies_path,
            deletes_path,
            create_count,
            modify_count,
            delete_count,
        )?;
        gz.finish()?.finish()
    } else {
        write_document(
            buf,
            creates_path,
            modifies_path,
            deletes_path,
            create_count,
            modify_count,
            delete_count,
        )?
        .finish()
    }
}

/// Write the OsmChange document around the three fragments into `sink` and
/// hand the sink back for the caller to finish.
fn write_document<W: Write>(
    sink: W,
    creates_path: &Path,
    modifies_path: &Path,
    deletes_path: &Path,
    create_count: u64,
    modify_count: u64,
    delete_count: u64,
) -> Result<W> {
    let mut writer = Writer::new_with_indent(sink, b' ', 2);

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    writer.write_event(Event::Text(BytesText::new("\n")))?;

    let mut root = BytesStart::new("osmChange");
    root.push_attribute(("version", "0.6"));
    writer.write_event(Event::Start(root))?;

    copy_section(&mut writer, "create", creates_path, create_count)?;
    copy_section(&mut writer, "modify", modifies_path, modify_count)?;
    copy_section(&mut writer, "delete", deletes_path, delete_count)?;

    writer.write_event(Event::End(BytesEnd::new("osmChange")))?;

    Ok(writer.into_inner())
}

/// Copy a temp file's raw XML bytes into an action section.
/// Flushes the XML writer before copying raw bytes to the underlying writer,
/// then resumes structured writing for the closing tag.
fn copy_section<W: Write>(
    writer: &mut Writer<W>,
    tag: &str,
    path: &Path,
    count: u64,
) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    writer.write_event(Event::Start(BytesStart::new(tag)))?;
    // Flush the XML writer's internal buffer before raw byte copy
    writer.get_mut().flush()?;
    // Copy raw XML fragment bytes directly to the underlying writer
    let mut tmp = io::BufReader::new(crate::read::input::open(path)?);
    io::copy(&mut tmp, writer.get_mut())
        .map_err(|e| Error::from(e).context(format!("failed to copy {}", path.display())))?;
    writer.write_event(Event::End(BytesEnd::new(tag)))?;
    Ok(())
}

/// Write one `<node|way|relation id=".." [version=".."] [timestamp=".."]/>`
/// delete entry. Shared by the sequential sink (both sequential paths) and
/// the parallel shard workers.
pub(super) fn write_delete_element<W: Write>(
    writer: &mut Writer<W>,
    tag_name: &str,
    id: i64,
    metadata: Option<&OwnedMetadata>,
    increment_version: bool,
    timestamp: Option<u64>,
) -> Result<()> {
    let mut elem = BytesStart::new(tag_name);
    let id_str = id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    // Emitted only when the source claimed a version. OSM versions start
    // at 1, and the PBF inputs spell "no version" as 0 (pbfhogg's
    // BlockBuilder and libosmium, including the padding slot of a node
    // without metadata in a dense block whose other nodes carry it) or -1
    // (Osmosis). `version="0"` - or `version="1"` under
    // `--increment-version` - would be a claim the input never made. The
    // filter sits here rather than at extraction because every derive path
    // funnels its deletes through this function.
    if let Some(version) = metadata.and_then(|m| m.version).filter(|&v| v >= 1) {
        let version = if increment_version {
            version.saturating_add(1)
        } else {
            version
        };
        let v_str = version.to_string();
        elem.push_attribute(("version", v_str.as_str()));
    }
    if let Some(timestamp) = timestamp {
        let ts = crate::commands::format_epoch_secs(i64::try_from(timestamp).unwrap_or(i64::MAX));
        elem.push_attribute(("timestamp", ts.as_str()));
    }
    writer.write_event(Event::Empty(elem))?;
    Ok(())
}

#[cfg(test)]
mod scratch_tests {
    use super::ChangeSink;
    use crate::scratch::RunScratch;

    #[test]
    fn delete_elements_use_the_supplied_timestamp() {
        let mut writer = quick_xml::Writer::new(Vec::new());
        for id in [1, 2] {
            super::write_delete_element(&mut writer, "node", id, None, false, Some(0))
                .expect("write delete");
        }
        let xml = String::from_utf8(writer.into_inner()).expect("XML");
        assert_eq!(xml.matches("timestamp=\"1970-01-01T00:00:00Z\"").count(), 2);
    }

    /// A delete carries a version only when the source claimed one: 0 and
    /// -1 are how PBF writers spell "no version", so neither prints, not
    /// even bumped by `--increment-version`.
    #[test]
    fn delete_versions_below_one_are_not_claims() {
        for increment in [false, true] {
            let mut writer = quick_xml::Writer::new(Vec::new());
            for (id, version) in [(1, 0), (2, -1), (3, 3)] {
                let meta = super::OwnedMetadata::version_only(version);
                super::write_delete_element(&mut writer, "node", id, Some(&meta), increment, None)
                    .expect("write delete");
            }
            let xml = String::from_utf8(writer.into_inner()).expect("XML");
            let v = if increment { 4 } else { 3 };
            assert_eq!(
                xml,
                format!("<node id=\"1\"/><node id=\"2\"/><node id=\"3\" version=\"{v}\"/>"),
            );
        }
    }

    #[test]
    fn dropping_the_run_scratch_removes_all_sink_fragments() {
        let dir = crate::path_guard::tests::scratch();
        let scratch =
            RunScratch::create(dir.path(), &crate::host::Host::for_tests()).expect("scratch");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _sink = ChangeSink::new(&scratch, false, None).expect("sink");
            panic!("simulate a failed derive run");
        }));
        assert!(result.is_err());
        drop(scratch);
        assert_eq!(std::fs::read_dir(dir.path()).expect("directory").count(), 0);
    }
}
