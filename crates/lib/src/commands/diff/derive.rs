//! Generate an OSC diff from two PBF snapshots. Equivalent to `osmium derive-changes`.
//!
//! Merges both files, appending each work unit's changes to one temp file
//! per action in unit order, then assembles the final `.osc.gz` in a single
//! pass. Memory is bounded by the units and decoded blocks in flight, not
//! total change count.
//! Requires both inputs to declare `Sort.Type_then_ID`.

use std::io::{self, Write};

use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

use crate::ElementKind;
use crate::commands::input_front::sorted_and_indexed;
use crate::error::{Error, Result};
use crate::osc::merge_join::{
    BlockMergeAction, MergeSink, StreamingBlocks, element_id, stream_merge_phase,
};
use crate::osc::write::{OwnedMetadata, write_element_xml};
use crate::read::input::{InputSource, PinnedInput};

use super::units::UnitOut;

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
/// Pass `None` to omit delete timestamps. A value past
/// `9999-12-31T23:59:59Z` (253402300799) is refused with `InvalidArgument`
/// before either input is opened: OSC cannot spell it.
///
/// Every value written is one pbfhogg's own OSC reader accepts, so the
/// output always applies: a created or modified node whose coordinate does
/// not fit `i32` decimicrodegrees (`WireFormat`) or lies outside +-90 /
/// +-180 degrees (`InvalidData`), an element timestamp outside years 0000
/// to 9999 (`InvalidData`) or one whose scaling overflows (`WireFormat`)
/// fails the call, and nothing is published.
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
    let gzip = crate::osc::output_is_gzip(output.destination())?;
    // The delete override must be one OSC can spell, judged once here, before
    // either input is opened and whether or not any delete is ever written.
    check_delete_timestamp(timestamp)?;
    let jobs = super::resolve_workers(parallelism, host);
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
    // pbfhogg-verify's derive-changes check matches this note's rendered
    // line exactly to judge each run's route (at a fixed `--jobs 2`); a
    // reword fails that check closed, so change its matcher with the text.
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

    // The unit workers deflate the fragments themselves for a gzip output;
    // the element stream writes plain XML on this thread.
    let encoding = if both_indexed && gzip {
        Fragments::GzipMembers
    } else {
        Fragments::Raw
    };
    let mut sink = ChangeSink::new(scratch, encoding, increment_version, timestamp)?;

    let result = (|| -> Result<DeriveChangesStats> {
        if both_indexed {
            derive_changes_block_pair(
                &old,
                &new,
                jobs,
                direct_io,
                increment_version,
                timestamp,
                &mut sink,
                scratch,
                host,
            )?;
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

/// The run's change sink: the three action fragments, as scratch files the
/// OSC assembly frames.
struct ChangeSink {
    w: ChangeWriters<io::BufWriter<crate::scratch::ScratchFile>>,
    encoding: Fragments,
    creates_path: crate::scratch::ScratchPath,
    modifies_path: crate::scratch::ScratchPath,
    deletes_path: crate::scratch::ScratchPath,
}

impl ChangeSink {
    /// A sink on three fresh scratch files, `derive-<action>.xml.tmp`.
    fn new(
        scratch: &crate::scratch::RunScratch,
        encoding: Fragments,
        increment_version: bool,
        timestamp: Option<u64>,
    ) -> Result<Self> {
        let cp = scratch.file("derive-creates.xml.tmp")?;
        let mp = scratch.file("derive-modifies.xml.tmp")?;
        let dp = scratch.file("derive-deletes.xml.tmp")?;
        let creates = io::BufWriter::new(crate::scratch::create_file(&cp)?);
        let modifies = io::BufWriter::new(crate::scratch::create_file(&mp)?);
        let deletes = io::BufWriter::new(crate::scratch::create_file(&dp)?);
        Ok(Self {
            w: ChangeWriters::new(creates, modifies, deletes, increment_version, timestamp),
            encoding,
            creates_path: cp,
            modifies_path: mp,
            deletes_path: dp,
        })
    }

    fn flush(&mut self) -> Result<()> {
        self.w.creates.get_mut().flush()?;
        self.w.modifies.get_mut().flush()?;
        self.w.deletes.get_mut().flush()?;
        Ok(())
    }

    fn stats(&self) -> DeriveChangesStats {
        self.w.stats()
    }
}

/// Which action fragment a piece of XML belongs to.
#[derive(Clone, Copy)]
enum Section {
    Creates = 0,
    Modifies = 1,
    Deletes = 2,
}

impl Section {
    fn name(self) -> &'static str {
        match self {
            Self::Creates => "creates",
            Self::Modifies => "modifies",
            Self::Deletes => "deletes",
        }
    }
}

/// What the three action fragments hold. Chosen once per run and never
/// read off a file or an output name after that.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fragments {
    /// Plain element XML: the unindexed element stream, and every plain
    /// `.osc` output. Assembly frames it (deflating it for `.osc.gz`).
    Raw,
    /// Concatenated gzip members of element XML, deflated by the unit
    /// workers: the work-unit engine writing `.osc.gz`. Assembly deflates
    /// only the document framing and copies the fragments verbatim.
    GzipMembers,
}

/// Copy a scratch file to the end of `dst`, checking the cancellation token
/// between buffers: neither file observes it.
fn copy_checked(
    src: &crate::scratch::ScratchPath,
    dst: &mut impl Write,
    cancel: &crate::host::Recorder,
) -> Result<()> {
    let mut src = crate::scratch::open_file(src)?;
    let mut buf = vec![0_u8; crate::tunables::DIFF_SPILL_COPY_BUFFER];
    loop {
        cancel.check()?;
        let n = match io::Read::read(&mut src, &mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        // `read` never reports more than the buffer holds.
        dst.write_all(buf.get(..n).unwrap_or_default())?;
    }
}

/// The refusal of a change OSC cannot carry: a create, modify or delete of
/// an ID that occurs more than once on either side.
///
/// Both merges pair equal IDs in the order they occur, so a repeated ID is
/// compared occurrence by occurrence. But an OSC holds one final statement
/// per kind and ID, and applying it replaces every base occurrence of the
/// ID with that one statement (or deletes them all), so no OSC turns old
/// `[n1 a, n1 b]` into new `[n1 a, n1 c]`, or `[n1, n1]` into `[n1]`.
/// Repeats that are unchanged on both sides need no statement and pass: the
/// OSC is silent on them, and apply carries them through.
///
/// Actions reach the sink in OSM order within a kind (a merge emits nothing
/// before every smaller ID), so the guard needs only the current ID: how
/// often each side holds it, and whether any action on it is a change. It
/// is judged once the ID is over: a larger ID, another kind, or the end of
/// the unit or phase ([`Self::finish`]). A key going backwards means a block
/// whose elements are out of order behind its claims, which the element
/// stream would have refused at admission; it is refused here too.
///
/// A byte-identical pair is seen only at its first and last ID
/// (`BlobEqual`). That suffices: an ID strictly inside the pair's range
/// occurs only inside that pair, identically on both sides, so nothing
/// changes it, and a change on an edge ID elsewhere adds an occurrence to
/// the one the edge contributes.
#[derive(Default)]
struct RepeatGuard {
    current: Option<RepeatRun>,
}

struct RepeatRun {
    kind: ElementKind,
    id: i64,
    old: u32,
    new: u32,
    changed: bool,
}

impl RepeatGuard {
    /// Record occurrences of `id` on the old and new side, and whether this
    /// action changes it.
    fn observe(
        &mut self,
        kind: ElementKind,
        id: i64,
        old: u32,
        new: u32,
        changed: bool,
    ) -> Result<()> {
        if let Some(run) = self.current.as_mut()
            && run.kind == kind
        {
            match crate::osm_id::osm_id_cmp(id, run.id) {
                std::cmp::Ordering::Equal => {
                    run.old = run.old.saturating_add(old);
                    run.new = run.new.saturating_add(new);
                    run.changed |= changed;
                    return Ok(());
                }
                std::cmp::Ordering::Less => {
                    return Err(crate::error::new_unsorted(format!(
                        "derive: {} {id} follows {} {}. The input's blocks hold their \
                         elements out of canonical order.",
                        kind.as_str(),
                        kind.as_str(),
                        run.id,
                    )));
                }
                std::cmp::Ordering::Greater => {}
            }
        }
        self.finish()?;
        self.current = Some(RepeatRun {
            kind,
            id,
            old,
            new,
            changed,
        });
        Ok(())
    }

    /// Judge the current ID: it is over.
    fn finish(&mut self) -> Result<()> {
        match self.current.take() {
            Some(run) if run.changed && (run.old > 1 || run.new > 1) => {
                Err(Error::invalid_data(format!(
                    "derive: {} {} occurs at least {} times in the old input and {} in the \
                     new one, and changed. A derived change file states each object once \
                     and applying it rewrites every occurrence, so derive refuses a change \
                     to a repeated ID.",
                    run.kind.as_str(),
                    run.id,
                    run.old,
                    run.new,
                )))
            }
            _ => Ok(()),
        }
    }
}

/// Element XML for the three actions, written to `W` as merge actions
/// arrive, with their counts. The run's sink writes to its scratch files; a
/// unit's worker writes to memory and hands the bytes over in chunks.
struct ChangeWriters<W: Write> {
    creates: Writer<W>,
    modifies: Writer<W>,
    deletes: Writer<W>,
    create_count: u64,
    modify_count: u64,
    delete_count: u64,
    increment_version: bool,
    timestamp: Option<u64>,
    coord_buf: String,
    /// Every element an action names passes through it, run elements
    /// included.
    repeats: RepeatGuard,
}

impl<W: Write> ChangeWriters<W> {
    fn new(
        creates: W,
        modifies: W,
        deletes: W,
        increment_version: bool,
        timestamp: Option<u64>,
    ) -> Self {
        Self {
            creates: Writer::new(creates),
            modifies: Writer::new(modifies),
            deletes: Writer::new(deletes),
            create_count: 0,
            modify_count: 0,
            delete_count: 0,
            increment_version,
            timestamp,
            coord_buf: String::new(),
            repeats: RepeatGuard::default(),
        }
    }

    /// A create of `elem`, which only the new input holds.
    fn created(&mut self, kind: ElementKind, elem: &crate::Element<'_>) -> Result<()> {
        self.repeats.observe(kind, element_id(elem), 0, 1, true)?;
        self.write_create(elem)
    }

    /// A delete of `elem`, which only the old input holds.
    fn deleted(&mut self, kind: ElementKind, elem: &crate::Element<'_>) -> Result<()> {
        self.repeats.observe(kind, element_id(elem), 1, 0, true)?;
        self.write_delete(elem, kind)
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

    fn stats(&self) -> DeriveChangesStats {
        DeriveChangesStats {
            creates: self.create_count,
            modifies: self.modify_count,
            deletes: self.delete_count,
        }
    }
}

impl<W: Write> MergeSink for ChangeWriters<W> {
    /// Record one merge action, from any path - each work unit of the
    /// block-pair merge and the unindexed element stream. The one place
    /// derive turns an action into OSC, so the output depends neither on
    /// the worker count nor on whether the inputs carried indexdata.
    fn record(&mut self, kind: ElementKind, action: &BlockMergeAction<'_>) -> Result<()> {
        match action {
            BlockMergeAction::BlobEqual { first, last, .. } => {
                self.repeats.observe(kind, *first, 1, 1, false)?;
                if first != last {
                    self.repeats.observe(kind, *last, 1, 1, false)?;
                }
            }
            BlockMergeAction::ElementEqual(e) => {
                self.repeats.observe(kind, element_id(e), 1, 1, false)?;
            }
            BlockMergeAction::BlobOldOnly(run) => {
                for elem in run.elements() {
                    self.deleted(kind, &elem)?;
                }
            }
            BlockMergeAction::BlobNewOnly(run) => {
                for elem in run.elements() {
                    self.created(kind, &elem)?;
                }
            }
            BlockMergeAction::ElementModified { new, .. } => {
                self.repeats.observe(kind, element_id(new), 1, 1, true)?;
                self.write_modify(new)?;
            }
            BlockMergeAction::ElementOldOnly(o) => self.deleted(kind, o)?,
            BlockMergeAction::ElementNewOnly(n) => self.created(kind, n)?,
        }
        Ok(())
    }
}

impl<W: Write> ChangeWriters<W> {
    fn section(&mut self, section: Section) -> &mut Writer<W> {
        match section {
            Section::Creates => &mut self.creates,
            Section::Modifies => &mut self.modifies,
            Section::Deletes => &mut self.deletes,
        }
    }
}

const SECTIONS: [Section; 3] = [Section::Creates, Section::Modifies, Section::Deletes];

/// A unit's spill file for one section: the members its worker could not
/// queue once the unit's channel was full.
struct Spill {
    path: crate::scratch::ScratchPath,
    file: io::BufWriter<crate::scratch::ScratchFile>,
}

/// A derive unit's worker side: element XML formatted into memory, one
/// buffer per action, each handed over as a member once it reaches
/// [`DIFF_OUTPUT_CHUNK_BYTES`](crate::tunables::DIFF_OUTPUT_CHUNK_BYTES).
///
/// Single-sided blob runs are polled element by element, so a member is at
/// most the chunk size plus one element (a whole planet blob would be tens
/// of MB). Under [`Fragments::GzipMembers`] the worker deflates each member
/// itself, so the gzip work runs on every worker and what waits in memory is
/// compressed.
///
/// Members go to the unit's channel while it has room. Once it is full the
/// unit spills every later member, to the end of the unit, to a scratch
/// file per section, which the consumer appends to the fragment after the
/// unit's queued members. Without the spill a unit the consumer is not
/// reading yet could only run a few chunks ahead, and a stretch of units
/// with large output (the new nodes at a planet's tail) would format on one
/// core, the head unit's. The switch is one-way, so each section's members
/// reach the fragment in order; it chooses only where a finished member is
/// stored, so the bytes do not depend on scheduling.
struct UnitWriters<'s> {
    w: ChangeWriters<Vec<u8>>,
    encoding: Fragments,
    scratch: &'s crate::scratch::RunScratch,
    unit: usize,
    /// Section bytes at which a member is handed over.
    chunk_bytes: usize,
    /// Members queued before the unit spills regardless of the channel
    /// (`usize::MAX` outside the fault plan).
    spill_after: usize,
    queued: usize,
    spilling: bool,
    spills: [Option<Spill>; 3],
}

/// A finished derive unit: its counts and the spill files it wrote.
struct UnitDone {
    stats: DeriveChangesStats,
    spills: [Option<crate::scratch::ScratchPath>; 3],
}

impl UnitWriters<'_> {
    /// Hand over every section past the chunk size.
    fn poll(&mut self, out: &mut dyn UnitOut<(Section, Vec<u8>)>) -> Result<()> {
        for section in SECTIONS {
            if self.w.section(section).get_ref().len() >= self.chunk_bytes {
                self.emit(section, out)?;
            }
        }
        Ok(())
    }

    /// Hand over a section's buffer as one member.
    fn emit(&mut self, section: Section, out: &mut dyn UnitOut<(Section, Vec<u8>)>) -> Result<()> {
        if out.abandoned() {
            return Err(Error::internal(
                "derive unit: the consumer of the unit is gone",
            ));
        }
        let buf = self.w.section(section).get_mut();
        let member = match self.encoding {
            Fragments::GzipMembers => {
                let member = crate::write::parallel_gzip::compress_one(buf)?;
                buf.clear();
                member
            }
            Fragments::Raw => std::mem::take(buf),
        };
        if self.queued >= self.spill_after {
            self.spilling = true;
        }
        if !self.spilling {
            match out.try_send((section, member))? {
                None => {
                    self.queued += 1;
                    return Ok(());
                }
                Some((_, member)) => {
                    self.spilling = true;
                    return self.spill(section, &member);
                }
            }
        }
        self.spill(section, &member)
    }

    fn spill(&mut self, section: Section, member: &[u8]) -> Result<()> {
        let slot = &mut self.spills[section as usize];
        if slot.is_none() {
            let name = format!("derive-unit-{}-{}.tmp", self.unit, section.name());
            let path = self.scratch.file(&name)?;
            let file = io::BufWriter::new(crate::scratch::create_file(&path)?);
            *slot = Some(Spill { path, file });
        }
        if let Some(spill) = slot.as_mut() {
            spill.file.write_all(member)?;
        }
        Ok(())
    }
}

impl super::units::UnitSink for UnitWriters<'_> {
    type Item = (Section, Vec<u8>);
    type Stats = UnitDone;

    fn record_into(
        &mut self,
        kind: ElementKind,
        action: &BlockMergeAction<'_>,
        out: &mut dyn UnitOut<(Section, Vec<u8>)>,
    ) -> Result<()> {
        match action {
            BlockMergeAction::BlobOldOnly(run) => {
                for elem in run.elements() {
                    self.w.deleted(kind, &elem)?;
                    self.poll(out)?;
                }
                Ok(())
            }
            BlockMergeAction::BlobNewOnly(run) => {
                for elem in run.elements() {
                    self.w.created(kind, &elem)?;
                    self.poll(out)?;
                }
                Ok(())
            }
            _ => {
                self.w.record(kind, action)?;
                self.poll(out)
            }
        }
    }

    fn finish_into(mut self, out: &mut dyn UnitOut<(Section, Vec<u8>)>) -> Result<UnitDone> {
        // Units are key windows that never split an ID, so the unit's last
        // ID is complete here.
        self.w.repeats.finish()?;
        for section in SECTIONS {
            if !self.w.section(section).get_ref().is_empty() {
                self.emit(section, out)?;
            }
        }
        let mut spills: [Option<crate::scratch::ScratchPath>; 3] = [None, None, None];
        for (slot, spill) in spills.iter_mut().zip(std::mem::take(&mut self.spills)) {
            if let Some(Spill { path, file }) = spill {
                // A flush failure here would otherwise be lost in a drop and
                // truncate the fragment.
                file.into_inner().map_err(io::IntoInnerError::into_error)?;
                *slot = Some(path);
            }
        }
        Ok(UnitDone {
            stats: self.w.stats(),
            spills,
        })
    }
}

/// Derive's side on the calling thread: each member appended to its action's
/// fragment of the run's [`ChangeSink`], then the unit's spills, counts
/// folded in unit order, so every fragment holds its elements in input
/// order.
struct OscConsumer<'a> {
    sink: &'a mut ChangeSink,
    scratch: &'a crate::scratch::RunScratch,
    cancel: &'a crate::host::Recorder,
}

impl<'a> super::units::UnitConsumer for OscConsumer<'a> {
    type Sink = UnitWriters<'a>;

    const WORKER: &'static str = "derive unit worker";

    /// A member is already a whole buffer: it goes straight to the file
    /// (the fragment's buffer holds nothing on this path), so the fragment
    /// on disk is current after every member.
    fn item(&mut self, (section, bytes): (Section, Vec<u8>)) -> Result<()> {
        let fragment = self.sink.w.section(section).get_mut();
        fragment.flush()?;
        fragment.get_mut().write_all(&bytes)?;
        Ok(())
    }

    fn finish_unit(&mut self, done: UnitDone) -> Result<()> {
        for (section, spill) in SECTIONS.into_iter().zip(&done.spills) {
            if let Some(path) = spill {
                let fragment = self.sink.w.section(section).get_mut();
                fragment.flush()?;
                copy_checked(path, fragment.get_mut(), self.cancel)?;
                self.scratch.remove(path);
            }
        }
        let stats = done.stats;
        self.sink.w.create_count += stats.creates;
        self.sink.w.modify_count += stats.modifies;
        self.sink.w.delete_count += stats.deletes;
        Ok(())
    }

    fn on_kind_end(
        &mut self,
        kind: ElementKind,
        units: usize,
        max_blobs: usize,
        min_blobs: usize,
        invocation: &crate::host::Recorder,
    ) {
        let [units_name, max_name, min_name] = match kind {
            ElementKind::Node => [
                "derivepar_node_units",
                "derivepar_node_unit_max_blobs",
                "derivepar_node_unit_min_blobs",
            ],
            ElementKind::Way => [
                "derivepar_way_units",
                "derivepar_way_unit_max_blobs",
                "derivepar_way_unit_min_blobs",
            ],
            ElementKind::Relation => [
                "derivepar_rel_units",
                "derivepar_rel_unit_max_blobs",
                "derivepar_rel_unit_min_blobs",
            ],
        };
        invocation.counter(units_name, units);
        invocation.counter(max_name, max_blobs);
        invocation.counter(min_name, min_blobs);
    }

    #[cfg(pbfhogg_test_hooks)]
    fn panic_hook(faults: &crate::test_hooks::FaultPlan) -> &std::sync::atomic::AtomicUsize {
        &faults.derive_unit_panic
    }

    #[cfg(pbfhogg_test_hooks)]
    fn panic_message(idx: usize) -> String {
        format!("test-hooks: derive unit {idx} panicking")
    }
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
/// work units streamed from both inputs (see `units`). Only changed
/// elements are serialized; each unit's XML reaches the run's fragments in
/// unit order as the run goes.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn derive_changes_block_pair(
    old: &PinnedInput,
    new: &PinnedInput,
    jobs: usize,
    direct_io: bool,
    increment_version: bool,
    timestamp: Option<u64>,
    sink: &mut ChangeSink,
    scratch: &crate::scratch::RunScratch,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    // Scratch holds the three action fragments, which gzip assembly keeps
    // alive. Header ranges/counts cannot predict changed XML bytes, so an
    // input-size free-space refusal here would be a guess, not a
    // lifetime-aware budget; retain bounded-memory streaming and propagate
    // actual write failures.
    let phases = [
        super::units::Phase {
            kind: ElementKind::Node,
            start_marker: "DERIVECHANGES_PHASE_NODE_START",
            end_marker: "DERIVECHANGES_PHASE_NODE_END",
        },
        super::units::Phase {
            kind: ElementKind::Way,
            start_marker: "DERIVECHANGES_PHASE_WAY_START",
            end_marker: "DERIVECHANGES_PHASE_WAY_END",
        },
        super::units::Phase {
            kind: ElementKind::Relation,
            start_marker: "DERIVECHANGES_PHASE_REL_START",
            end_marker: "DERIVECHANGES_PHASE_REL_END",
        },
    ];
    let encoding = sink.encoding;
    let (chunk_bytes, spill_after) = unit_output_limits(&host.invocation);
    let make_sink = |unit: usize| UnitWriters {
        w: ChangeWriters::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            increment_version,
            timestamp,
        ),
        encoding,
        scratch,
        unit,
        chunk_bytes,
        spill_after,
        queued: 0,
        spilling: false,
        spills: [None, None, None],
    };
    let mut consumer = OscConsumer {
        sink,
        scratch,
        cancel: &host.invocation,
    };
    let merge_stats = super::units::run(
        old,
        new,
        &phases,
        jobs,
        direct_io,
        true,
        false,
        &make_sink,
        &mut consumer,
        host,
    )?;
    super::emit_merge_stats_counters(&merge_stats, &host.invocation);
    Ok(())
}

/// A derive unit's chunk bytes and forced-spill point: the tunable and
/// "never", or the fault plan's overrides in a test-hooks build.
fn unit_output_limits(invocation: &crate::host::Recorder) -> (usize, usize) {
    #[cfg(pbfhogg_test_hooks)]
    {
        let faults = invocation.faults();
        let chunk = faults
            .derive_chunk_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let spill_after = faults
            .derive_spill_after
            .load(std::sync::atomic::Ordering::Relaxed);
        let chunk = if chunk > 0 {
            chunk
        } else {
            crate::tunables::DIFF_OUTPUT_CHUNK_BYTES
        };
        (chunk, spill_after)
    }
    #[cfg(not(pbfhogg_test_hooks))]
    {
        let _ = invocation;
        (crate::tunables::DIFF_OUTPUT_CHUNK_BYTES, usize::MAX)
    }
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
        stream_merge_phase(&mut old_src, &mut new_src, kind, &mut sink.w)?;
        sink.w.repeats.finish()?;
        host.invocation.marker(end);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// OSC XML writer
// ---------------------------------------------------------------------------

/// Assemble the output from the run's three fragments and their element
/// counts. Writes the XML structure via a quick_xml Writer and copies the
/// fragment bytes directly to the writer underneath it (not through the XML
/// writer): plain fragments through the parallel gzip writer or as they are,
/// gzip-member fragments verbatim between deflated framing members.
fn assemble_osc(
    output: &crate::output::BoundOutput,
    sink: &ChangeSink,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let creates_path = &sink.creates_path;
    let modifies_path = &sink.modifies_path;
    let deletes_path = &sink.deletes_path;
    let (create_count, modify_count, delete_count) = (
        sink.w.create_count,
        sink.w.modify_count,
        sink.w.delete_count,
    );
    let encoding = sink.encoding;
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

    // Raw fragments (the unindexed element stream) are deflated here in
    // `DEFAULT_CHUNK_SIZE` members; gzip-member fragments arrive deflated by
    // the unit workers in `DIFF_OUTPUT_CHUNK_BYTES` members (plus at most one
    // element). Each member's 18 B framing and dictionary reset cost a little
    // ratio either way.
    // The gzip worker count comes from host parallelism (every available
    // thread; 4 when the host reports none), not from `-j`: this assembler
    // runs after the comparison has finished. A `-j 1` run therefore still
    // compresses on every thread.
    let worker_count = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DERIVE_GZIP_WORKERS_RESERVED,
        crate::tunables::DERIVE_GZIP_WORKERS_MIN,
        crate::tunables::DERIVE_GZIP_WORKERS_FALLBACK,
    );
    let paths = [creates_path, modifies_path, deletes_path];
    let counts = [create_count, modify_count, delete_count];
    // Deliberately not one output stage with merge-changes. Both already
    // deflate on workers and copy finished members in order; what they would
    // share is that copy. The obligations that decide correctness differ:
    // this assembler joins three already ordered action fragments and
    // deflates only the framing, while merge-changes' coordinator orders
    // concurrent inputs' batches against a credit ledger and promotes spill
    // files. The raw arm keeps `ParallelGzipWriter` for the unindexed stream,
    // whose fragments are plain XML.
    match (gzip, encoding) {
        (true, Fragments::GzipMembers) => {
            // The fragments are gzip members already; only the framing
            // between them is deflated, each stretch as its own member.
            let glue = GlueMembers {
                out: buf,
                pending: Vec::new(),
            };
            let glue = write_document(glue, paths, counts, &mut |glue, path| {
                glue.emit_pending()?;
                let mut fragment = crate::scratch::open_file(path)?;
                io::copy(&mut fragment, &mut glue.out).map_err(|e| {
                    Error::from(e).context(format!("failed to copy {}", path.display()))
                })?;
                Ok(())
            })?;
            glue.finish()?.finish()
        }
        (true, Fragments::Raw) => {
            let gz =
                ParallelGzipWriter::new(buf, DEFAULT_CHUNK_SIZE, worker_count, &host.invocation);
            let gz = write_document(gz, paths, counts, &mut copy_raw)?;
            gz.finish()?.finish()
        }
        (false, _) => write_document(buf, paths, counts, &mut copy_raw)?.finish(),
    }
}

/// Copy a plain fragment through to the document's sink.
fn copy_raw<W: Write>(sink: &mut W, path: &crate::scratch::ScratchPath) -> Result<()> {
    let mut tmp = io::BufReader::new(crate::scratch::open_file(path)?);
    io::copy(&mut tmp, sink)
        .map_err(|e| Error::from(e).context(format!("failed to copy {}", path.display())))?;
    Ok(())
}

/// The document framing around gzip-member fragments: framing bytes gather
/// in `pending` and leave as one gzip member before each fragment and at
/// the end, so the decompressed document is byte for byte the one the plain
/// path writes.
struct GlueMembers<W: Write> {
    out: W,
    pending: Vec<u8>,
}

impl<W: Write> GlueMembers<W> {
    fn emit_pending(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            let member = crate::write::parallel_gzip::compress_one(&self.pending)?;
            self.out.write_all(&member)?;
            self.pending.clear();
        }
        Ok(())
    }

    fn finish(mut self) -> Result<W> {
        self.emit_pending()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for GlueMembers<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// Framing stays pending until a fragment or the end needs it out.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Write the OsmChange document around the three fragments into `sink` and
/// hand the sink back for the caller to finish.
fn write_document<W: Write>(
    sink: W,
    [creates_path, modifies_path, deletes_path]: [&crate::scratch::ScratchPath; 3],
    [create_count, modify_count, delete_count]: [u64; 3],
    copy: &mut dyn FnMut(&mut W, &crate::scratch::ScratchPath) -> Result<()>,
) -> Result<W> {
    let mut writer = Writer::new_with_indent(sink, b' ', 2);

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    writer.write_event(Event::Text(BytesText::new("\n")))?;

    let mut root = BytesStart::new("osmChange");
    root.push_attribute(("version", "0.6"));
    writer.write_event(Event::Start(root))?;

    copy_section(&mut writer, "create", creates_path, create_count, copy)?;
    copy_section(&mut writer, "modify", modifies_path, modify_count, copy)?;
    copy_section(&mut writer, "delete", deletes_path, delete_count, copy)?;

    writer.write_event(Event::End(BytesEnd::new("osmChange")))?;

    Ok(writer.into_inner())
}

/// Copy a temp file's fragment into an action section through `copy`.
/// Flushes the XML writer before the copy to the underlying writer, then
/// resumes structured writing for the closing tag.
fn copy_section<W: Write>(
    writer: &mut Writer<W>,
    tag: &str,
    path: &crate::scratch::ScratchPath,
    count: u64,
    copy: &mut dyn FnMut(&mut W, &crate::scratch::ScratchPath) -> Result<()>,
) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    writer.write_event(Event::Start(BytesStart::new(tag)))?;
    writer.get_mut().flush()?;
    copy(writer.get_mut(), path)?;
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
            version.checked_add(1).ok_or_else(|| {
                Error::invalid_data(format!(
                    "cannot increment delete version {version} for {tag_name} {id}"
                ))
            })?
        } else {
            version
        };
        let v_str = version.to_string();
        elem.push_attribute(("version", v_str.as_str()));
    }
    if let Some(timestamp) = timestamp {
        let ts = delete_timestamp_text(timestamp)
            .map_err(|_| crate::Error::internal("delete timestamp reached the writer unchecked"))?;
        elem.push_attribute(("timestamp", ts.as_str()));
    }
    writer.write_event(Event::Empty(elem))?;
    Ok(())
}

/// The OSC spelling of the delete override, or why it has none: an instant
/// after `9999-12-31T23:59:59Z` has no four-digit year, and pbfhogg's own
/// OSC parser would refuse the file derive published.
fn delete_timestamp_text(
    timestamp: u64,
) -> std::result::Result<String, crate::commands::TimestampError> {
    // A u64 above i64::MAX is past year 9999 too; saturate into the refusal.
    crate::commands::format_rfc3339_utc(i64::try_from(timestamp).unwrap_or(i64::MAX))
}

/// Refuse a delete override [`delete_timestamp_text`] cannot spell, as an
/// invalid argument: it is the caller's value, not the inputs'.
fn check_delete_timestamp(timestamp: Option<u64>) -> Result<()> {
    if let Some(timestamp) = timestamp {
        delete_timestamp_text(timestamp).map_err(|err| {
            crate::Error::invalid_argument_from(err).context(format!(
                "delete timestamp override {timestamp} (Unix seconds)"
            ))
        })?;
    }
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

    /// The override is checked against what OSC can spell: the last second
    /// of 9999 is written, anything later (u64 values past i64::MAX
    /// included) is an invalid argument, never clamped into a date.
    #[test]
    fn a_delete_timestamp_past_year_9999_is_refused() {
        super::check_delete_timestamp(None).expect("no override");
        super::check_delete_timestamp(Some(253_402_300_799)).expect("year 9999");
        let mut writer = quick_xml::Writer::new(Vec::new());
        super::write_delete_element(&mut writer, "node", 1, None, false, Some(253_402_300_799))
            .expect("write delete");
        let xml = String::from_utf8(writer.into_inner()).expect("XML");
        assert!(xml.contains("timestamp=\"9999-12-31T23:59:59Z\""), "{xml}");
        for ts in [253_402_300_800, i64::MAX as u64, u64::MAX] {
            let err = super::check_delete_timestamp(Some(ts)).expect_err("past 9999");
            assert!(
                matches!(err.kind(), crate::ErrorKind::InvalidArgument { .. }),
                "{ts}: {err:?}"
            );
        }
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
        let scratch = RunScratch::create(dir.path(), &crate::host::Host::for_tests().entered())
            .expect("scratch");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _sink =
                ChangeSink::new(&scratch, super::Fragments::Raw, false, None).expect("sink");
            panic!("simulate a failed derive run");
        }));
        assert!(result.is_err());
        drop(scratch);
        assert_eq!(std::fs::read_dir(dir.path()).expect("directory").count(), 0);
    }
}
