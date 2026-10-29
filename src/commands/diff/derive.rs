//! Generate an OSC diff from two PBF snapshots. Equivalent to `osmium derive-changes`.
//!
//! Merges both files writing changes directly to temp files (one set per
//! shard of a multi-shard phase, concatenated in shard order), then
//! assembles the final `.osc.gz` in a single pass. Memory is bounded by the
//! decoded blocks in flight, not total change count.
//! Requires both inputs to declare `Sort.Type_then_ID`.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

use crate::ElementKind;
use crate::commands::input_front::sorted_and_indexed;
use crate::error::{Error, Result};
use crate::osc::merge_join::{BlockMergeAction, MergeSink, StreamingBlocks, stream_merge_phase};
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
/// Performs a merge-join by (type, id), over ID-range shards for indexed
/// inputs (`parallelism`; the answer is the same at any shard count).
/// Changes are buffered by action type and
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
/// files (`InvalidArgument` otherwise) and are pinned for
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
    output: impl crate::output::BindOutput,
    direct_io: bool,
    increment_version: bool,
    timestamp: Option<u64>,
    parallelism: crate::host::Parallelism,
    host: &crate::host::Host,
    scratch: crate::scratch::RunScratch,
) -> Result<DeriveChangesStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
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
    output.publish()?;
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
    output: &crate::output::BoundOutput,
    direct_io: bool,
    increment_version: bool,
    timestamp: Option<u64>,
    parallelism: crate::host::Parallelism,
    host: &crate::host::Entered<'_>,
    scratch: &crate::scratch::RunScratch,
) -> Result<DeriveChangesStats> {
    host.invocation.check()?;
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
        "Streaming diff merge-walks both inputs in id order.",
    )?;
    crate::commands::require_sorted(
        new_sorted,
        new_path,
        "New PBF",
        "Streaming diff merge-walks both inputs in id order.",
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
        host.invocation
            .counter("derive_changes_sequential_fallback", 1);
    }

    // No compressed-input multiplier can predict changed XML bytes: tag
    // escaping, reference lists and the changed fraction determine expansion.
    // An exact preflight would repeat comparison and XML serialization on this
    // measured path. Keep write errors authoritative instead of refusing a
    // potentially empty diff against an invented scratch-space requirement.

    host.invocation.marker("DERIVECHANGES_SCAN_START");

    let mut sink = ChangeSink::new(scratch, "", increment_version, timestamp)?;

    let result = (|| -> Result<DeriveChangesStats> {
        if both_indexed {
            derive_changes_block_pair(&old, &new, jobs, direct_io, &mut sink, scratch, host)?;
        } else {
            derive_changes_element_stream(&old, &new, direct_io, &mut sink, host)?;
        }
        sink.flush()?;

        host.invocation.marker("DERIVECHANGES_SCAN_END");

        let stats = sink.stats();

        host.invocation.marker("DERIVECHANGES_WRITE_START");
        assemble_osc(output, &sink, host)?;
        host.invocation.marker("DERIVECHANGES_WRITE_END");
        Ok(stats)
    })();
    // Remove fragments before returning, on errors and during unwinding too.
    drop(sink);
    let stats = result?;

    host.invocation
        .counter("derivechanges_creates", stats.creates);
    host.invocation
        .counter("derivechanges_modifies", stats.modifies);
    host.invocation
        .counter("derivechanges_deletes", stats.deletes);

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
    /// A sink on three fresh scratch files, `derive-<action><suffix>.xml.tmp`:
    /// the run's final fragments with an empty suffix, a shard's own with
    /// one naming its kind and index. Creating a file truncates any of the
    /// same name, so no two live sinks may share a suffix.
    fn new(
        scratch: &crate::scratch::RunScratch,
        suffix: &str,
        increment_version: bool,
        timestamp: Option<u64>,
    ) -> Result<Self> {
        let cp = scratch.file(&format!("derive-creates{suffix}.xml.tmp"))?;
        let mp = scratch.file(&format!("derive-modifies{suffix}.xml.tmp"))?;
        let dp = scratch.file(&format!("derive-deletes{suffix}.xml.tmp"))?;
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

impl MergeSink for ChangeSink {
    /// Record one merge action, from any path - each shard of the block-pair
    /// merge and the unindexed element stream. The one place derive turns an
    /// action into OSC, so the output depends neither on the shard count nor
    /// on whether the inputs carried indexdata.
    fn record(&mut self, kind: ElementKind, action: &BlockMergeAction<'_>) -> Result<()> {
        match action {
            BlockMergeAction::BlobEqual(_) | BlockMergeAction::ElementEqual(_) => {}
            BlockMergeAction::BlobOldOnly(run) => {
                for elem in run.elements() {
                    self.write_delete(&elem, kind)?;
                }
            }
            BlockMergeAction::BlobNewOnly(run) => {
                for elem in run.elements() {
                    self.write_create(&elem)?;
                }
            }
            BlockMergeAction::ElementModified { new, .. } => self.write_modify(new)?,
            BlockMergeAction::ElementOldOnly(o) => self.write_delete(o, kind)?,
            BlockMergeAction::ElementNewOnly(n) => self.write_create(n)?,
        }
        Ok(())
    }
}

/// Derive's side of a sharded comparison: the run's [`ChangeSink`] as the
/// final sink, and one `ChangeSink` per shard of a multi-shard phase whose
/// three fragments are appended, action by action and in shard order, to
/// the final sink's three.
struct OscReport<'a> {
    sink: &'a mut ChangeSink,
    scratch: &'a crate::scratch::RunScratch,
}

/// A finished shard: its three flushed fragments and their counts.
struct OscShardDone {
    creates: crate::scratch::ScratchPath,
    modifies: crate::scratch::ScratchPath,
    deletes: crate::scratch::ScratchPath,
    stats: DeriveChangesStats,
}

impl super::sharded::ShardedReport for OscReport<'_> {
    type Shard = ChangeSink;
    type Done = OscShardDone;

    const WORKER: &'static str = "derive shard worker";

    fn inline(&mut self) -> &mut impl MergeSink {
        &mut *self.sink
    }

    fn open_shard(&mut self, kind: ElementKind, idx: usize) -> Result<Self::Shard> {
        ChangeSink::new(
            self.scratch,
            &format!("-{}-{idx}", kind.type_char()),
            self.sink.increment_version,
            self.sink.timestamp,
        )
    }

    fn finish_shard(mut shard: Self::Shard) -> Result<Self::Done> {
        shard.flush()?;
        let stats = shard.stats();
        // The writers drop here, closing the files before the append opens
        // them again.
        let ChangeSink {
            creates_path,
            modifies_path,
            deletes_path,
            ..
        } = shard;
        Ok(OscShardDone {
            creates: creates_path,
            modifies: modifies_path,
            deletes: deletes_path,
            stats,
        })
    }

    /// Append through the final sink's own `BufWriter`s, never another
    /// descriptor, so bytes an inline phase left buffered stay ahead of the
    /// appended ones, and fold the counts into the final sink's, which the
    /// OSC assembly frames from.
    fn absorb(&mut self, done: Vec<Self::Done>, cancel: &crate::host::Recorder) -> Result<()> {
        let mut first_err = None;
        for shard in done {
            // Once per shard, between appends (the appends themselves are
            // kernel copies with no checkpoint): after a cancellation the
            // remaining shard files are removed below, as after a failed
            // append.
            if first_err.is_none()
                && let Err(e) = cancel.check()
            {
                first_err = Some(e);
            }
            if first_err.is_some() {
                self.scratch.remove(&shard.creates);
                self.scratch.remove(&shard.modifies);
                self.scratch.remove(&shard.deletes);
                continue;
            }
            // `and` takes its argument eagerly, so all three calls run and
            // each removes its own source file even when an earlier copy
            // failed; the first error is the one kept.
            let appended =
                append_and_cleanup(self.sink.creates.get_mut(), &shard.creates, self.scratch)
                    .and(append_and_cleanup(
                        self.sink.modifies.get_mut(),
                        &shard.modifies,
                        self.scratch,
                    ))
                    .and(append_and_cleanup(
                        self.sink.deletes.get_mut(),
                        &shard.deletes,
                        self.scratch,
                    ));
            if let Err(e) = appended {
                first_err = Some(e);
                continue;
            }
            self.sink.create_count += shard.stats.creates;
            self.sink.modify_count += shard.stats.modifies;
            self.sink.delete_count += shard.stats.deletes;
        }
        first_err.map_or(Ok(()), Err)
    }

    fn on_plan(
        &mut self,
        kind: ElementKind,
        shards: &[super::shard::Shard],
        invocation: &crate::host::Recorder,
    ) {
        let [shards_name, max_name, min_name] = match kind {
            ElementKind::Node => [
                "derivepar_node_shards",
                "derivepar_node_shard_max_blobs",
                "derivepar_node_shard_min_blobs",
            ],
            ElementKind::Way => [
                "derivepar_way_shards",
                "derivepar_way_shard_max_blobs",
                "derivepar_way_shard_min_blobs",
            ],
            ElementKind::Relation => [
                "derivepar_rel_shards",
                "derivepar_rel_shard_max_blobs",
                "derivepar_rel_shard_min_blobs",
            ],
        };
        invocation.counter(shards_name, shards.len());
        let blobs = shards.iter().map(super::shard::Shard::blobs);
        invocation.counter(max_name, blobs.clone().max().unwrap_or(0));
        invocation.counter(min_name, blobs.min().unwrap_or(0));
    }

    #[cfg(feature = "test-hooks")]
    fn panic_hook(faults: &crate::test_hooks::FaultPlan) -> &std::sync::atomic::AtomicUsize {
        &faults.derive_shard_panic
    }

    #[cfg(feature = "test-hooks")]
    fn panic_message(idx: usize) -> String {
        format!("test-hooks: derive_parallel shard {idx} panicking")
    }
}

/// Copy a shard's scratch file onto `out` and then remove it - also when the
/// copy fails, so a failed write does not leave the temp file behind.
fn append_and_cleanup(
    out: &mut io::BufWriter<File>,
    src: &crate::scratch::ScratchPath,
    scratch: &crate::scratch::RunScratch,
) -> Result<()> {
    // Named: a bare "Permission denied" for a scratch file the user never
    // chose says nothing about where to look.
    let copied = crate::scratch::open(src)
        .and_then(|mut f| io::copy(&mut f, out))
        .map_err(|e| {
            crate::Error::from(e).context(format!("failed to copy scratch file {}", src.display()))
        });
    scratch.remove(src);
    copied.map(drop)
}

/// Extract just id + metadata from a borrowed element for delete output.
/// Avoids full owned conversion (no tag/ref cloning - deletes only need id + version).
/// The version is passed raw; `write_delete_element` decides whether it is
/// a claim. An element of another kind than the phase's yields `None` and
/// is neither written nor counted.
fn extract_delete_info(
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

/// Stream changes using the block-pair merge with borrowed elements, over
/// up to `jobs` ID-range shards per kind (see `shard` and `sharded`). Only
/// changed elements are serialized, written to temp files immediately.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn derive_changes_block_pair(
    old: &PinnedInput,
    new: &PinnedInput,
    jobs: usize,
    // Not honoured yet: the walk and the shard reads pread through the
    // walker's buffered descriptor at every shard count. Kept as the place
    // the flag reaches once those reads honour it.
    _direct_io: bool,
    sink: &mut ChangeSink,
    scratch: &crate::scratch::RunScratch,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let walked = super::shard::walk_pair(old, new, host)?;
    if walked.has_negative && jobs > 1 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "derive_changes.negative_id_fallback",
            "Using a single shard: negative IDs cannot use ID-range shards"
        );
        host.invocation
            .counter("derive_changes_negative_id_fallback", 1);
    }
    // Scratch peaks include the final action fragments plus the current
    // kind's shard fragments. Appending temporarily duplicates the source
    // shard, then removes it; gzip later keeps all three final fragments
    // alive. Header ranges/counts cannot predict changed XML bytes, so an
    // input-size free-space refusal here would be a guess, not a
    // lifetime-aware budget; retain bounded-memory streaming and propagate
    // actual write failures.
    let phases = [
        super::sharded::Phase {
            kind: ElementKind::Node,
            start_marker: "DERIVECHANGES_PHASE_NODE_START",
            end_marker: "DERIVECHANGES_PHASE_NODE_END",
        },
        super::sharded::Phase {
            kind: ElementKind::Way,
            start_marker: "DERIVECHANGES_PHASE_WAY_START",
            end_marker: "DERIVECHANGES_PHASE_WAY_END",
        },
        super::sharded::Phase {
            kind: ElementKind::Relation,
            start_marker: "DERIVECHANGES_PHASE_REL_START",
            end_marker: "DERIVECHANGES_PHASE_REL_END",
        },
    ];
    let mut report = OscReport { sink, scratch };
    let merge_stats = super::sharded::run(&walked, &phases, jobs, true, &mut report, host)?;
    super::emit_merge_stats_counters(&merge_stats, &host.invocation);
    Ok(())
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
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let pool = crate::osc::merge_join::merge_buffers(&host.invocation);
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
        host.invocation.marker(start);
        stream_merge_phase(&mut old_src, &mut new_src, kind, &mut *sink)?;
        host.invocation.marker(end);
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
    output: &crate::output::BoundOutput,
    sink: &ChangeSink,
    host: &crate::host::Entered<'_>,
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

/// Assemble the final `.osc.gz` from three temp file fragments and their
/// element counts; [`assemble_osc`] passes the run's sink's.
#[allow(clippy::too_many_arguments)]
fn assemble_osc_from_paths(
    output: &crate::output::BoundOutput,
    creates_path: &Path,
    modifies_path: &Path,
    deletes_path: &Path,
    create_count: u64,
    modify_count: u64,
    delete_count: u64,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    use crate::write::parallel_gzip::{DEFAULT_CHUNK_SIZE, ParallelGzipWriter};

    // Stage boundary. The assembly itself stops through the output writer,
    // which observes the token at every buffer it writes (the gzip writer
    // thread records that as its failure and every later `write` returns it).
    host.invocation.check()?;

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
    let buf = output.writer()?;

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
        let gz = ParallelGzipWriter::new(buf, DEFAULT_CHUNK_SIZE, worker_count, &host.invocation);
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
/// delete entry, for [`ChangeSink`].
fn write_delete_element<W: Write>(
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
        let scratch = RunScratch::create(dir.path(), &crate::host::Host::entered_for_tests())
            .expect("scratch");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _sink = ChangeSink::new(&scratch, "", false, None).expect("sink");
            panic!("simulate a failed derive run");
        }));
        assert!(result.is_err());
        drop(scratch);
        assert_eq!(std::fs::read_dir(dir.path()).expect("directory").count(), 0);
    }
}
